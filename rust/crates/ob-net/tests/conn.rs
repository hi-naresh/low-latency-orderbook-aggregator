//! End-to-end tests of `WsConnection` against a local TLS WebSocket server running in
//! the same process, with a throwaway certificate: real sockets and real TLS, so every
//! behaviour (record boundaries, pings, closes, refusals, drops, timeouts) is tested
//! deterministically, without the internet.
#![allow(clippy::unwrap_used, clippy::cast_possible_truncation)]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use ob_net::ws::{Message, Opcode, encode_server_frame, frame::apply_mask, handshake};
use ob_net::{ConnConfig, NetError, WaitMode, WsConnection, tls};
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};
use rustls::{RootCertStore, ServerConfig, ServerConnection, StreamOwned};

const FIXTURES: [&str; 2] = [
    include_str!("../../ob-parse/tests/fixtures/binance_depth20.jsonl"),
    include_str!("../../ob-parse/tests/fixtures/bybit_orderbook50.jsonl"),
];

/// A throwaway CA-less setup: a self-signed certificate for "localhost", trusted by the
/// client config and served by the server config.
fn tls_pair() -> (Arc<ServerConfig>, Arc<rustls::ClientConfig>) {
    let ck = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    let cert: CertificateDer<'static> = ck.cert.der().clone();
    let key = PrivatePkcs8KeyDer::from(ck.signing_key.serialize_der());
    let server = ServerConfig::builder_with_provider(tls::crypto_provider())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![cert.clone()], key.into())
        .unwrap();
    let mut roots = RootCertStore::empty();
    roots.add(cert).unwrap();
    (
        Arc::new(server),
        tls::client_config_with_roots(roots).unwrap(),
    )
}

/// The server side of one connection.
struct Server {
    stream: StreamOwned<ServerConnection, TcpStream>,
}

impl Server {
    /// Reads the HTTP upgrade request and answers 101, followed by `extra` in the same
    /// write (to test data arriving together with the handshake response).
    fn handshake(&mut self, extra: &[u8]) {
        let request = self.read_until_blank_line();
        let key = request
            .lines()
            .find_map(|l| l.strip_prefix("Sec-WebSocket-Key: "))
            .expect("client sent a key")
            .trim()
            .to_string();
        let mut response = format!(
            "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {}\r\n\r\n",
            handshake::expected_accept(&key)
        )
        .into_bytes();
        response.extend_from_slice(extra);
        self.send(&response);
    }

    fn read_until_blank_line(&mut self) -> String {
        let mut buf = Vec::new();
        let mut byte = [0_u8];
        while !buf.ends_with(b"\r\n\r\n") {
            self.stream.read_exact(&mut byte).unwrap();
            buf.push(byte[0]);
        }
        String::from_utf8(buf).unwrap()
    }

    /// One `write_all` + flush: one or more TLS records.
    fn send(&mut self, bytes: &[u8]) {
        self.stream.write_all(bytes).unwrap();
        self.stream.flush().unwrap();
    }

    /// Reads one frame from the client; returns (opcode, unmasked payload, was masked).
    fn read_client_frame(&mut self) -> (u8, Vec<u8>, bool) {
        let mut h = [0_u8; 2];
        self.stream.read_exact(&mut h).unwrap();
        let masked = h[1] & 0x80 != 0;
        let len = usize::from(h[1] & 0x7F); // our client only sends short frames
        let mut key = [0_u8; 4];
        if masked {
            self.stream.read_exact(&mut key).unwrap();
        }
        let mut payload = vec![0; len];
        self.stream.read_exact(&mut payload).unwrap();
        if masked {
            apply_mask(&mut payload, key);
        }
        (h[0] & 0x0F, payload, masked)
    }
}

/// Starts a server that runs `script` on the first connection. Returns the client
/// config pointing at it.
fn serve(script: impl FnOnce(&mut Server) + Send + 'static) -> (ConnConfig, JoinHandle<()>) {
    let (server_cfg, client_tls) = tls_pair();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = thread::spawn(move || {
        let (sock, _) = listener.accept().unwrap();
        sock.set_nodelay(true).unwrap();
        let conn = ServerConnection::new(server_cfg).unwrap();
        let mut server = Server {
            stream: StreamOwned::new(conn, sock),
        };
        script(&mut server);
    });
    let mut cfg = ConnConfig::new("localhost", "/ws").unwrap();
    cfg.port = port;
    cfg.tls = client_tls;
    cfg.connect_timeout = Duration::from_secs(5);
    (cfg, handle)
}

/// Polls until a message arrives (or panics after a few seconds), returning it owned.
fn next(conn: &mut WsConnection) -> Result<OwnedMessage, NetError> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(m) = conn.poll()? {
            return Ok(OwnedMessage::from(m));
        }
        assert!(Instant::now() < deadline, "no message within 5 s");
        std::hint::spin_loop();
    }
}

#[derive(Debug, PartialEq, Eq)]
enum OwnedMessage {
    Text(Vec<u8>),
    Ping(Vec<u8>),
    Close(Option<u16>),
    Other,
}

impl From<Message<'_>> for OwnedMessage {
    fn from(m: Message<'_>) -> Self {
        match m {
            Message::Text(t) => OwnedMessage::Text(t.to_vec()),
            Message::Ping(p) => OwnedMessage::Ping(p.to_vec()),
            Message::Close(c) => OwnedMessage::Close(c.map(|c| c.code)),
            _ => OwnedMessage::Other,
        }
    }
}

// ---------------------------------------------------------------------------

fn stream_all_fixtures(mode: WaitMode) {
    let lines: Vec<&'static str> = FIXTURES.iter().flat_map(|f| f.lines()).collect();
    let mut frames = Vec::new();
    for l in &lines {
        encode_server_frame(&mut frames, true, Opcode::Text, l.as_bytes());
    }
    let (cfg, server) = serve(move |s| {
        s.handshake(b"");
        // Random write sizes: TLS records end mid-frame, frames span records, several
        // frames share a record.
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut rest = &frames[..];
        while !rest.is_empty() {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let n = (1 + (x % 3000) as usize).min(rest.len());
            s.send(&rest[..n]);
            rest = &rest[n..];
        }
        let _ = s.read_client_frame(); // wait for the client's close before hanging up
    });
    let mut conn = WsConnection::connect(&cfg).unwrap();
    assert_eq!(
        conn.cipher_suite(),
        Some(rustls::CipherSuite::TLS13_AES_128_GCM_SHA256)
    );
    conn.set_wait(mode).unwrap();
    for (i, want) in lines.iter().enumerate() {
        let got = loop {
            if let Some(m) = conn.poll().unwrap() {
                break OwnedMessage::from(m);
            }
        };
        assert_eq!(
            got,
            OwnedMessage::Text(want.as_bytes().to_vec()),
            "message {i}"
        );
    }
    conn.close(1000).unwrap();
    server.join().unwrap();
}

#[test]
fn all_recorded_messages_arrive_intact_across_record_boundaries_spinning() {
    stream_all_fixtures(WaitMode::Spin);
}

#[test]
fn all_recorded_messages_arrive_intact_across_record_boundaries_blocking() {
    stream_all_fixtures(WaitMode::Block {
        timeout: Duration::from_millis(50),
    });
}

#[test]
fn pings_are_answered_and_closes_echoed() {
    let (cfg, server) = serve(|s| {
        s.handshake(b"");
        let mut out = Vec::new();
        encode_server_frame(&mut out, true, Opcode::Ping, b"hb-1");
        s.send(&out);
        assert_eq!(
            s.read_client_frame(),
            (0xA, b"hb-1".to_vec(), true),
            "pong with the same payload, masked"
        );
        let mut out = Vec::new();
        encode_server_frame(
            &mut out,
            true,
            Opcode::Close,
            &[0x03, 0xE8, b'b', b'y', b'e'],
        );
        s.send(&out);
        assert_eq!(
            s.read_client_frame(),
            (0x8, vec![0x03, 0xE8], true),
            "close echoed with the code"
        );
    });
    let mut conn = WsConnection::connect(&cfg).unwrap();
    assert_eq!(
        next(&mut conn).unwrap(),
        OwnedMessage::Ping(b"hb-1".to_vec())
    );
    assert_eq!(next(&mut conn).unwrap(), OwnedMessage::Close(Some(1000)));
    assert!(matches!(conn.poll(), Err(NetError::Closed(Some(1000)))));
    server.join().unwrap();
}

#[test]
fn frames_sent_with_the_handshake_response_are_not_lost() {
    let mut first = Vec::new();
    encode_server_frame(&mut first, true, Opcode::Text, br#"{"result":null,"id":1}"#);
    let (cfg, server) = serve(move |s| {
        s.handshake(&first);
        let _ = s.read_client_frame();
    });
    let mut conn = WsConnection::connect(&cfg).unwrap();
    assert_eq!(
        next(&mut conn).unwrap(),
        OwnedMessage::Text(br#"{"result":null,"id":1}"#.to_vec())
    );
    conn.close(1000).unwrap();
    server.join().unwrap();
}

#[test]
fn client_messages_are_masked_text_frames() {
    let (cfg, server) = serve(|s| {
        s.handshake(b"");
        assert_eq!(
            s.read_client_frame(),
            (0x1, br#"{"op":"ping"}"#.to_vec(), true)
        );
    });
    let mut conn = WsConnection::connect(&cfg).unwrap();
    conn.send_text(br#"{"op":"ping"}"#).unwrap();
    server.join().unwrap();
}

#[test]
fn a_refused_upgrade_is_reported_with_its_status() {
    let (cfg, server) = serve(|s| {
        let _ = s.read_until_blank_line();
        s.send(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n");
    });
    let err = WsConnection::connect(&cfg).err().unwrap();
    assert!(
        matches!(
            err,
            NetError::Handshake(handshake::HandshakeError::Status(403))
        ),
        "{err:?}"
    );
    server.join().unwrap();
}

#[test]
fn an_untrusted_certificate_is_refused() {
    let (mut cfg, server) = serve(|s| {
        let _ = s.stream.conn.complete_io(&mut s.stream.sock); // client aborts the handshake
    });
    cfg.tls = tls::client_config().unwrap(); // public roots: the self-signed cert isn't trusted
    let err = WsConnection::connect(&cfg).err().unwrap();
    assert!(matches!(err, NetError::Io(_) | NetError::Tls(_)), "{err:?}");
    assert!(format!("{err:?}").contains("UnknownIssuer"), "{err:?}");
    server.join().unwrap();
}

#[test]
fn a_dropped_connection_is_an_error_not_a_hang() {
    let (cfg, server) = serve(|s| {
        s.handshake(b"");
        // Return without a close frame: the socket is dropped.
    });
    let mut conn = WsConnection::connect(&cfg).unwrap();
    server.join().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let err = loop {
        match conn.poll() {
            Err(e) => break e,
            Ok(_) => assert!(Instant::now() < deadline, "no error within 5 s"),
        }
    };
    assert!(matches!(err, NetError::Io(_)), "{err:?}");
}

#[test]
fn a_silent_server_hits_the_connect_deadline() {
    // Accepts TCP but never speaks TLS.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let holder = thread::spawn(move || {
        let (_sock, _) = listener.accept().unwrap();
        thread::sleep(Duration::from_secs(2));
    });
    let mut cfg = ConnConfig::new("localhost", "/ws").unwrap();
    cfg.port = port;
    cfg.connect_timeout = Duration::from_millis(300);
    let start = Instant::now();
    let err = WsConnection::connect(&cfg).err().unwrap();
    assert!(matches!(err, NetError::Timeout), "{err:?}");
    assert!(
        start.elapsed() < Duration::from_secs(1),
        "took {:?}",
        start.elapsed()
    );
    holder.join().unwrap();
}
