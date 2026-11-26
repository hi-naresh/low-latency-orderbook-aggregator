//! The REST GET against a local TLS server playing scripted responses, plus a
//! live check (ignored by default).
#![allow(clippy::unwrap_used, clippy::print_stdout)]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use ob_net::{ConnConfig, NetError, http, tls};
use rustls::pki_types::PrivatePkcs8KeyDer;
use rustls::{RootCertStore, ServerConfig, ServerConnection, StreamOwned};

/// Serves one connection: reads the request, then writes `parts` with a pause between
/// them (so bodies arrive in pieces), then closes. Returns the request it received.
fn serve(parts: Vec<Vec<u8>>, pause: Duration) -> (ConnConfig, thread::JoinHandle<String>) {
    let ck = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    let cert = ck.cert.der().clone();
    let key = PrivatePkcs8KeyDer::from(ck.signing_key.serialize_der());
    let server_cfg = Arc::new(
        ServerConfig::builder_with_provider(tls::crypto_provider())
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![cert.clone()], key.into())
            .unwrap(),
    );
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let mut cfg = ConnConfig::new("localhost", "/").unwrap();
    cfg.port = listener.local_addr().unwrap().port();
    let mut roots = RootCertStore::empty();
    roots.add(cert).unwrap();
    cfg.tls = tls::client_config_with_roots(roots).unwrap();
    cfg.connect_timeout = Duration::from_secs(2);
    let h = thread::spawn(move || {
        let (sock, _) = listener.accept().unwrap();
        let mut s = StreamOwned::new(ServerConnection::new(server_cfg).unwrap(), sock);
        let mut req = Vec::new();
        let mut b = [0_u8];
        while !req.ends_with(b"\r\n\r\n") {
            if s.read_exact(&mut b).is_err() {
                break;
            }
            req.push(b[0]);
        }
        for p in parts {
            if s.write_all(&p).and_then(|()| s.flush()).is_err() {
                break;
            }
            thread::sleep(pause);
        }
        s.conn.send_close_notify();
        let _ = s.flush();
        String::from_utf8(req).unwrap()
    });
    (cfg, h)
}

fn ok_with_length(body: &[u8]) -> Vec<u8> {
    let mut r = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
        body.len()
    )
    .into_bytes();
    r.extend_from_slice(body);
    r
}

#[test]
fn a_content_length_body_arriving_in_pieces() {
    let body = br#"{"lastUpdateId":1,"bids":[],"asks":[]}"#.repeat(500);
    let resp = ok_with_length(&body);
    let parts: Vec<Vec<u8>> = resp.chunks(7000).map(<[u8]>::to_vec).collect();
    let (cfg, server) = serve(parts, Duration::from_millis(5));
    let got = http::get(&cfg, "/fapi/v1/depth?symbol=BTCUSDT&limit=1000", 1 << 20).unwrap();
    assert_eq!(got, body);
    let req = server.join().unwrap();
    assert!(
        req.starts_with("GET /fapi/v1/depth?symbol=BTCUSDT&limit=1000 HTTP/1.1\r\n"),
        "{req}"
    );
    assert!(req.contains("\r\nHost: localhost\r\n") && req.contains("Connection: close"));
    assert!(
        !req.to_ascii_lowercase().contains("accept-encoding"),
        "we never ask for compression"
    );
}

#[test]
fn a_chunked_body() {
    let resp = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\n{\"a\"\r\n3\r\n:1}\r\n0\r\n\r\n".to_vec();
    let parts: Vec<Vec<u8>> = resp.chunks(9).map(<[u8]>::to_vec).collect();
    let (cfg, server) = serve(parts, Duration::from_millis(2));
    assert_eq!(http::get(&cfg, "/x", 1024).unwrap(), b"{\"a\":1}");
    server.join().unwrap();
}

#[test]
fn errors_are_errors() {
    // A rate-limit status.
    let (cfg, s) = serve(
        vec![b"HTTP/1.1 429 Too Many Requests\r\nContent-Length: 0\r\n\r\n".to_vec()],
        Duration::ZERO,
    );
    assert!(matches!(
        http::get(&cfg, "/x", 1024),
        Err(NetError::HttpStatus(429))
    ));
    s.join().unwrap();
    // A body cut short (the server promised 100 bytes, sent 10, closed).
    let (cfg, s) = serve(
        vec![b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n0123456789".to_vec()],
        Duration::ZERO,
    );
    assert!(http::get(&cfg, "/x", 1024).is_err());
    s.join().unwrap();
    // Too big: refused from the header, before reading it.
    let (cfg, s) = serve(vec![ok_with_length(&[b'x'; 5000])], Duration::ZERO);
    assert!(http::get(&cfg, "/x", 1000).is_err());
    s.join().unwrap();
    // Compressed (never requested).
    let (cfg, s) = serve(
        vec![b"HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nContent-Length: 1\r\n\r\nx".to_vec()],
        Duration::ZERO,
    );
    assert!(http::get(&cfg, "/x", 1000).is_err());
    s.join().unwrap();
}

#[test]
fn a_server_that_never_answers_times_out() {
    let (mut cfg, s) = serve(
        vec![b"HTTP/1.1 200 OK\r\n".to_vec()],
        Duration::from_millis(800),
    );
    cfg.connect_timeout = Duration::from_millis(300);
    let t = Instant::now();
    assert!(matches!(
        http::get(&cfg, "/x", 1024),
        Err(NetError::Timeout)
    ));
    assert!(t.elapsed() < Duration::from_secs(1), "{:?}", t.elapsed());
    s.join().unwrap();
}

#[test]
#[ignore = "needs the internet"]
fn binance_rest_snapshot_live() {
    let cfg = ConnConfig::new("fapi.binance.com", "/").unwrap();
    let t = Instant::now();
    let body = http::get(&cfg, "/fapi/v1/depth?symbol=BTCUSDT&limit=1000", 4 << 20).unwrap();
    println!(
        "{} bytes in {:?}: {}…",
        body.len(),
        t.elapsed(),
        String::from_utf8_lossy(&body[..80])
    );
    assert!(body.starts_with(br#"{"lastUpdateId":"#));
}
