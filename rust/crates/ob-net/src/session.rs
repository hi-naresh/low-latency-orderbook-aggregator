//! A TLS client session whose records are protected by our own record layer.
//!
//! ```text
//! handshake():  rustls's unbuffered client, driven over the socket until it may send
//!               data → rustls::kernel: hands over the traffic keys, keeps the secrets
//! receive():    every complete record in the buffer → decrypt in place → plaintext
//!               appended directly after the previous plaintext (no allocation, no copy)
//! seal_app():   payload → protected record(s) appended to the output buffer
//! ```
//!
//! **What stays with rustls** (through [`KernelConnection`], its API for exactly this):
//! certificate verification and the whole handshake, deriving new keys on a key update,
//! and storing session tickets for resumption. **What this module does:** framing,
//! nonces, sequence numbers, the post-handshake messages (session tickets, key updates),
//! and alerts. The cryptography is `ring`'s, via [`crate::record`].

use std::io::{self, Read, Write};
use std::sync::Arc;

use rustls::client::{ClientConnectionData, UnbufferedClientConnection};
use rustls::kernel::KernelConnection;
use rustls::pki_types::ServerName;
use rustls::unbuffered::{ConnectionState, EncodeError, InsufficientSizeError, UnbufferedStatus};
use rustls::{ClientConfig, Error, ProtocolVersion, SupportedCipherSuite};

use crate::conn::NetError;
use crate::record::{ContentType, HEADER, Keys, MAX_BODY, MAX_PLAINTEXT, Version, alert_error};

/// Largest post-handshake message accepted (session tickets are a few hundred bytes).
const MAX_HANDSHAKE_MESSAGE: usize = 1 << 16;
/// RFC 8446 §4.6.3 KeyUpdate: message type 24.
const KEY_UPDATE: u8 = 24;
/// RFC 8446 §4.6.1 NewSessionTicket: message type 4.
const NEW_SESSION_TICKET: u8 = 4;
/// TLS 1.2 HelloRequest (a renegotiation request): message type 0.
const HELLO_REQUEST: u8 = 0;

/// The receive buffer, shared by TLS and WebSocket decoding:
///
/// ```text
/// [ consumed | plaintext: start..end | (gap) | ciphertext not yet decrypted: raw_start..raw_end | free ]
/// ```
///
/// Records are decrypted from `raw_start` to `end`: the plaintext stays contiguous, which
/// is what lets the WebSocket decoder read messages spanning records in place.
pub(crate) struct RecvBuf {
    pub(crate) buf: Box<[u8]>,
    pub(crate) start: usize,
    pub(crate) end: usize,
    pub(crate) raw_start: usize,
    pub(crate) raw_end: usize,
}

impl RecvBuf {
    pub(crate) fn new(capacity: usize) -> RecvBuf {
        RecvBuf {
            buf: vec![0; capacity].into_boxed_slice(),
            start: 0,
            end: 0,
            raw_start: 0,
            raw_end: 0,
        }
    }

    /// Plaintext not yet consumed.
    pub(crate) fn unread(&self) -> &[u8] {
        &self.buf[self.start..self.end]
    }

    /// Where the next socket read goes, after making sure there's room for at least
    /// `need` bytes (moving the unread data to the front if necessary; rare).
    pub(crate) fn space(&mut self, need: usize) -> io::Result<&mut [u8]> {
        if self.buf.len() - self.raw_end < need {
            let plain = self.end - self.start;
            self.buf.copy_within(self.start..self.end, 0);
            let raw = self.raw_end - self.raw_start;
            self.buf.copy_within(self.raw_start..self.raw_end, plain);
            (self.start, self.end) = (0, plain);
            (self.raw_start, self.raw_end) = (plain, plain + raw);
            if self.buf.len() - self.raw_end < need {
                return Err(io::Error::other("message larger than the receive buffer"));
            }
        }
        Ok(&mut self.buf[self.raw_end..])
    }
}

/// One TLS session after its handshake.
pub(crate) struct Session {
    kernel: KernelConnection<ClientConnectionData>,
    version: Version,
    rx: Keys,
    tx: Keys,
    /// Post-handshake handshake messages, reassembled across records.
    handshake: Vec<u8>,
    /// The peer sent close_notify: nothing more will come.
    peer_closed: bool,
    /// Records we may send under one key (the AEAD's confidentiality limit).
    tx_limit: u64,
}

impl Session {
    /// Runs the TLS handshake over `sock` (blocking reads and writes; `before_io` is
    /// called before each, to enforce a deadline). Handshake bytes are read into `rb`'s
    /// raw region; whatever follows the handshake stays there, for [`Session::receive`].
    pub(crate) fn handshake<S: Read + Write>(
        config: Arc<ClientConfig>,
        name: ServerName<'static>,
        sock: &mut S,
        rb: &mut RecvBuf,
        mut before_io: impl FnMut(&mut S) -> Result<(), NetError>,
    ) -> Result<Session, NetError> {
        let mut conn = UnbufferedClientConnection::new(config, name)?;
        let mut out = vec![0_u8; 4096];
        let mut out_len = 0;
        loop {
            let UnbufferedStatus { discard, state } =
                conn.process_tls_records(&mut rb.buf[rb.raw_start..rb.raw_end]);
            let mut need_read = false;
            let mut done = false;
            match state? {
                ConnectionState::EncodeTlsData(mut e) => loop {
                    match e.encode(&mut out[out_len..]) {
                        Ok(n) => {
                            out_len += n;
                            break;
                        }
                        Err(EncodeError::InsufficientSize(InsufficientSizeError {
                            required_size,
                        })) => out.resize(out_len + required_size, 0),
                        Err(e) => return Err(NetError::Tls(Error::General(e.to_string()))),
                    }
                },
                ConnectionState::TransmitTlsData(t) => {
                    before_io(sock)?;
                    sock.write_all(&out[..out_len])?;
                    sock.flush()?;
                    out_len = 0;
                    t.done();
                }
                ConnectionState::BlockedHandshake => need_read = true,
                ConnectionState::WriteTraffic(_) => done = true,
                ConnectionState::PeerClosed | ConnectionState::Closed => {
                    return Err(NetError::Closed(None));
                }
                // No application data can come before we've sent a request; anything
                // else is a state this client doesn't use.
                _ => {
                    return Err(NetError::Tls(Error::General(
                        "unexpected state during the handshake".into(),
                    )));
                }
            }
            rb.raw_start += discard;
            if done {
                break;
            }
            if need_read {
                before_io(sock)?;
                let space = rb.space(HEADER + MAX_BODY)?;
                let n = sock.read(space)?;
                if n == 0 {
                    return Err(NetError::Io(io::ErrorKind::UnexpectedEof.into()));
                }
                rb.raw_end += n;
            }
        }
        let (secrets, kernel) = conn.dangerous_into_kernel_connection()?;
        let version = match kernel.protocol_version() {
            ProtocolVersion::TLSv1_3 => Version::Tls13,
            ProtocolVersion::TLSv1_2 => Version::Tls12,
            _ => {
                return Err(NetError::Tls(Error::General(
                    "unsupported TLS version".into(),
                )));
            }
        };
        let tx_limit = match kernel.negotiated_cipher_suite() {
            SupportedCipherSuite::Tls13(s) => s.common.confidentiality_limit,
            SupportedCipherSuite::Tls12(s) => s.common.confidentiality_limit,
        };
        let (tx_seq, tx) = &secrets.tx;
        let (rx_seq, rx) = &secrets.rx;
        // Plaintext starts where the undecrypted bytes start.
        (rb.start, rb.end) = (rb.raw_start, rb.raw_start);
        Ok(Session {
            version,
            rx: Keys::new(version, rx, *rx_seq)?,
            tx: Keys::new(version, tx, *tx_seq)?,
            kernel,
            handshake: Vec::with_capacity(4096),
            peer_closed: false,
            tx_limit,
        })
    }

    /// Decrypts every complete record in `rb`'s raw region in place, appending application
    /// data to its plaintext. Anything we must send in reply (a key update) is appended to
    /// `out`. Partial records stay for the next read.
    pub(crate) fn receive(&mut self, rb: &mut RecvBuf, out: &mut Vec<u8>) -> Result<(), Error> {
        while rb.raw_end - rb.raw_start >= HEADER {
            let h = &rb.buf[rb.raw_start..rb.raw_start + HEADER];
            let body = usize::from(u16::from_be_bytes([h[3], h[4]]));
            if body > MAX_BODY {
                return Err(Error::PeerSentOversizedRecord);
            }
            if rb.raw_end - rb.raw_start < HEADER + body {
                break; // the rest of this record hasn't arrived
            }
            if self.peer_closed {
                return Err(Error::General("record after close_notify".into()));
            }
            let rec = rb.raw_start;
            rb.raw_start += HEADER + body;
            let (typ, len) = self.rx.open(&mut rb.buf, rb.end, rec)?;
            let plain = rb.end..rb.end + len;
            match typ {
                ContentType::ApplicationData => rb.end += len,
                ContentType::Handshake => {
                    self.handshake.extend_from_slice(&rb.buf[plain]);
                    self.post_handshake(out)?;
                }
                ContentType::Alert => self.alert(&rb.buf[plain])?,
                ContentType::ChangeCipherSpec => {
                    return Err(Error::General(
                        "change_cipher_spec after the handshake".into(),
                    ));
                }
            }
        }
        if rb.raw_start == rb.raw_end {
            // Nothing left undecrypted: the next read lands right after the plaintext.
            (rb.raw_start, rb.raw_end) = (rb.end, rb.end);
        }
        Ok(())
    }

    /// Complete post-handshake messages in the reassembly buffer.
    fn post_handshake(&mut self, out: &mut Vec<u8>) -> Result<(), Error> {
        while self.handshake.len() >= 4 {
            let h = &self.handshake;
            let len = usize::from(h[1]) << 16 | usize::from(h[2]) << 8 | usize::from(h[3]);
            if len > MAX_HANDSHAKE_MESSAGE {
                return Err(Error::General("post-handshake message too large".into()));
            }
            if h.len() < 4 + len {
                break; // continues in the next record
            }
            match (self.version, h[0]) {
                (Version::Tls13, NEW_SESSION_TICKET) => {
                    // Stored by rustls for resuming the next connection.
                    self.kernel
                        .handle_new_session_ticket(&self.handshake[4..4 + len])?;
                }
                (Version::Tls13, KEY_UPDATE) => {
                    // RFC 8446 §4.6.3: one byte, and it must end its record (the next
                    // record is under the new key).
                    if len != 1 || h.len() != 5 {
                        return Err(Error::General("malformed key update".into()));
                    }
                    let requested = match h[4] {
                        0 => false,
                        1 => true,
                        _ => return Err(Error::General("malformed key update".into())),
                    };
                    let (seq, secrets) = self.kernel.update_rx_secret()?;
                    self.rx = Keys::new(self.version, &secrets, seq)?;
                    if requested {
                        self.update_tx(out)?;
                    }
                }
                // Renegotiation isn't supported; RFC 5246 §7.4.1.1 lets a client
                // ignore the request.
                (Version::Tls12, HELLO_REQUEST) if len == 0 => {}
                _ => return Err(Error::General("unexpected post-handshake message".into())),
            }
            self.handshake.drain(..4 + len);
        }
        Ok(())
    }

    /// Sends a KeyUpdate (under the current key), then switches to the next key.
    fn update_tx(&mut self, out: &mut Vec<u8>) -> Result<(), Error> {
        self.tx
            .seal(ContentType::Handshake, &[KEY_UPDATE, 0, 0, 1, 0], out)?;
        let (seq, secrets) = self.kernel.update_tx_secret()?;
        self.tx = Keys::new(self.version, &secrets, seq)?;
        Ok(())
    }

    fn alert(&mut self, payload: &[u8]) -> Result<(), Error> {
        match payload {
            [_, 0] => {
                self.peer_closed = true; // close_notify
                Ok(())
            }
            [_, 90] => Ok(()), // user_canceled: a warning, followed by close_notify
            [_, description] => Err(alert_error(*description)),
            _ => Err(Error::General("malformed alert".into())),
        }
    }

    /// Appends `payload` as application data (split into records as needed) to `out`.
    pub(crate) fn seal_app(&mut self, payload: &[u8], out: &mut Vec<u8>) -> Result<(), Error> {
        for chunk in payload
            .chunks(MAX_PLAINTEXT)
            .chain(payload.is_empty().then_some(&[][..]))
        {
            if self.tx.seq() >= self.tx_limit {
                if self.version == Version::Tls13 {
                    self.update_tx(out)?;
                } else {
                    return Err(Error::General("TLS 1.2 key exhausted".into()));
                }
            }
            self.tx.seal(ContentType::ApplicationData, chunk, out)?;
        }
        Ok(())
    }

    /// Appends a close_notify alert to `out`.
    pub(crate) fn seal_close_notify(&mut self, out: &mut Vec<u8>) -> Result<(), Error> {
        self.tx.seal(ContentType::Alert, &[1, 0], out)
    }

    /// The peer has closed the TLS session (close_notify).
    pub(crate) fn peer_closed(&self) -> bool {
        self.peer_closed
    }

    /// The negotiated cipher suite.
    pub(crate) fn cipher_suite(&self) -> rustls::CipherSuite {
        self.kernel.negotiated_cipher_suite().suite()
    }

    /// The negotiated protocol version.
    pub(crate) fn protocol_version(&self) -> ProtocolVersion {
        self.kernel.protocol_version()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::cast_possible_truncation)] // test data
mod tests {
    //! Against a real rustls server over an in-memory transport: the same encrypted byte
    //! stream fed to our receiver split at many different points must give exactly the
    //! plaintext the server wrote, for every cipher suite, on TLS 1.2 and 1.3, across key
    //! updates and session tickets.

    use std::io::{Read, Write};
    use std::sync::Arc;

    use rustls::crypto::ring as provider;
    use rustls::server::ServerConnection;
    use rustls::{RootCertStore, ServerConfig, SupportedCipherSuite, SupportedProtocolVersion};

    use super::*;

    /// The server end of an in-memory connection.
    struct Pump {
        server: ServerConnection,
    }

    impl Read for Pump {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if !self.server.wants_write() {
                return Err(io::ErrorKind::WouldBlock.into());
            }
            self.server.write_tls(&mut &mut buf[..])
        }
    }

    impl Write for Pump {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            // rustls takes in a bounded amount at a time: process as we go.
            let mut src = buf;
            while !src.is_empty() {
                self.server.read_tls(&mut src)?;
                self.server
                    .process_new_packets()
                    .map_err(io::Error::other)?;
            }
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    struct Setup {
        server_cfg: Arc<ServerConfig>,
        client_cfg: Arc<ClientConfig>,
    }

    fn setup(version: &'static SupportedProtocolVersion, suite: SupportedCipherSuite) -> Setup {
        let ck = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        let cert = ck.cert.der().clone();
        let key = rustls::pki_types::PrivatePkcs8KeyDer::from(ck.signing_key.serialize_der());
        let server_provider = Arc::new(rustls::crypto::CryptoProvider {
            cipher_suites: vec![suite],
            ..provider::default_provider()
        });
        let mut server_cfg = ServerConfig::builder_with_provider(server_provider)
            .with_protocol_versions(&[version])
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![cert.clone()], key.into())
            .unwrap();
        // Lets a test take the server's keys and forge what a real server never sends.
        server_cfg.enable_secret_extraction = true;
        let server_cfg = Arc::new(server_cfg);
        let mut roots = RootCertStore::empty();
        roots.add(cert).unwrap();
        let client_cfg = crate::tls::client_config_with_roots(roots).unwrap();
        Setup {
            server_cfg,
            client_cfg,
        }
    }

    /// Handshake in memory; returns our session, the server, and our receive buffer.
    fn connect(s: &Setup) -> (Session, Pump, RecvBuf) {
        let mut server = ServerConnection::new(Arc::clone(&s.server_cfg)).unwrap();
        server.set_buffer_limit(None); // the tests queue more than rustls's default 64 KiB
        let mut pump = Pump { server };
        let mut rb = RecvBuf::new(1 << 20);
        let name = ServerName::try_from("localhost").unwrap();
        let session =
            Session::handshake(Arc::clone(&s.client_cfg), name, &mut pump, &mut rb, |_| {
                Ok(())
            })
            .unwrap();
        (session, pump, rb)
    }

    /// Everything the server has to send, as bytes.
    fn drain(server: &mut ServerConnection) -> Vec<u8> {
        let mut out = Vec::new();
        while server.wants_write() {
            server.write_tls(&mut out).unwrap();
        }
        out
    }

    /// Feeds `wire` to our session in pieces of the given sizes (cycled); returns the
    /// plaintext and whatever the session wanted to send back.
    fn receive_in_pieces(
        session: &mut Session,
        rb: &mut RecvBuf,
        wire: &[u8],
        pieces: &[usize],
    ) -> Result<(Vec<u8>, Vec<u8>), Error> {
        let mut plain = Vec::new();
        let mut reply = Vec::new();
        let mut at = 0;
        let mut k = 0;
        while at < wire.len() {
            let n = pieces[k % pieces.len()].max(1).min(wire.len() - at);
            k += 1;
            let space = rb.space(HEADER + MAX_BODY).unwrap();
            space[..n].copy_from_slice(&wire[at..at + n]);
            rb.raw_end += n;
            at += n;
            session.receive(rb, &mut reply)?;
            plain.extend_from_slice(rb.unread());
            rb.start = rb.end; // consumed
        }
        Ok((plain, reply))
    }

    fn suites() -> Vec<(&'static SupportedProtocolVersion, SupportedCipherSuite)> {
        use provider::cipher_suite::*;
        vec![
            (&rustls::version::TLS13, TLS13_AES_128_GCM_SHA256),
            (&rustls::version::TLS13, TLS13_AES_256_GCM_SHA384),
            (&rustls::version::TLS13, TLS13_CHACHA20_POLY1305_SHA256),
            (
                &rustls::version::TLS12,
                TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256,
            ),
            (
                &rustls::version::TLS12,
                TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384,
            ),
            (
                &rustls::version::TLS12,
                TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256,
            ),
        ]
    }

    /// Messages of awkward sizes, including ones spanning several records.
    fn messages() -> Vec<Vec<u8>> {
        [0_usize, 1, 100, 16_383, 16_384, 16_385, 40_000, 7]
            .iter()
            .enumerate()
            .map(|(i, &n)| (0..n).map(|j| ((i * 31 + j) % 251) as u8).collect())
            .collect()
    }

    #[test]
    fn every_suite_and_split_gives_exactly_the_servers_plaintext() {
        for (version, suite) in suites() {
            let s = setup(version, suite);
            for pieces in [
                &[1][..],
                &[2, 3],
                &[5],
                &[4096],
                &[100, 1, 16_389],
                &[1 << 20],
            ] {
                let (mut session, mut pump, mut rb) = connect(&s);
                assert_eq!(session.cipher_suite(), suite.suite());
                let mut expected = Vec::new();
                for m in messages() {
                    pump.server.writer().write_all(&m).unwrap();
                    expected.extend_from_slice(&m);
                }
                // Session tickets (TLS 1.3) come first, then the data.
                let wire = drain(&mut pump.server);
                let (plain, reply) =
                    receive_in_pieces(&mut session, &mut rb, &wire, pieces).unwrap();
                assert_eq!(plain, expected, "{suite:?} pieces {pieces:?}");
                assert!(reply.is_empty());
            }
        }
    }

    /// The server asks for a key update in the middle of the stream: we decrypt the rest
    /// under its new key, answer with our own key update, and the server can read what
    /// we send under our new key.
    #[test]
    fn key_updates_in_both_directions() {
        use provider::cipher_suite::*;
        for suite in [TLS13_AES_128_GCM_SHA256, TLS13_CHACHA20_POLY1305_SHA256] {
            let s = setup(&rustls::version::TLS13, suite);
            for pieces in [&[1][..], &[7], &[1 << 20]] {
                let (mut session, mut pump, mut rb) = connect(&s);
                pump.server.writer().write_all(b"before").unwrap();
                pump.server.refresh_traffic_keys().unwrap();
                pump.server.writer().write_all(b"after").unwrap();
                let wire = drain(&mut pump.server);
                let (plain, reply) =
                    receive_in_pieces(&mut session, &mut rb, &wire, pieces).unwrap();
                assert_eq!(plain, b"beforeafter");
                assert!(!reply.is_empty(), "our key update answers the request");
                // Our key update, then data under the new key: rustls must accept both.
                let mut out = reply;
                session.seal_app(b"hello server", &mut out).unwrap();
                pump.write_all(&out).unwrap();
                let mut got = [0_u8; 64];
                let n = pump.server.reader().read(&mut got).unwrap();
                assert_eq!(&got[..n], b"hello server");
            }
        }
    }

    /// Our records, both versions and every suite, are accepted by rustls.
    #[test]
    fn the_server_reads_what_we_send() {
        for (version, suite) in suites() {
            let s = setup(version, suite);
            let (mut session, mut pump, _rb) = connect(&s);
            // Sizes around the record limit; read back between them (rustls buffers at
            // most 16 KiB of received plaintext).
            for len in [1_usize, 16_384, 16_385, 3] {
                let msg: Vec<u8> = (0..len).map(|i| (i % 253) as u8).collect();
                let mut out = Vec::new();
                session.seal_app(&msg, &mut out).unwrap();
                let mut got = vec![0_u8; len];
                let mut have = 0;
                for record_bytes in out.chunks(4096) {
                    pump.write_all(record_bytes).unwrap();
                    while let Ok(n) = pump.server.reader().read(&mut got[have..]) {
                        if n == 0 {
                            break;
                        }
                        have += n;
                    }
                }
                assert_eq!(have, len, "{suite:?}");
                assert_eq!(got, msg, "{suite:?}");
            }
            let mut got = [0_u8; 8];
            // And a clean close.
            let mut out = Vec::new();
            session.seal_close_notify(&mut out).unwrap();
            pump.write_all(&out).unwrap();
            assert_eq!(pump.server.reader().read(&mut got).unwrap(), 0, "{suite:?}");
        }
    }

    /// Session tickets reach rustls: the next connection with the same configuration
    /// resumes instead of doing a full handshake.
    #[test]
    fn session_tickets_enable_resumption() {
        use provider::cipher_suite::*;
        let s = setup(&rustls::version::TLS13, TLS13_AES_128_GCM_SHA256);
        let (mut session, mut pump, mut rb) = connect(&s);
        let wire = drain(&mut pump.server); // the tickets
        assert!(!wire.is_empty());
        receive_in_pieces(&mut session, &mut rb, &wire, &[1 << 20]).unwrap();
        let (_, pump2, _) = connect(&s);
        assert_eq!(
            pump2.server.handshake_kind(),
            Some(rustls::HandshakeKind::Resumed)
        );
    }

    #[test]
    fn close_notify_then_nothing_more() {
        for (version, suite) in suites() {
            let s = setup(version, suite);
            let (mut session, mut pump, mut rb) = connect(&s);
            pump.server.writer().write_all(b"bye").unwrap();
            pump.server.send_close_notify();
            let wire = drain(&mut pump.server);
            let (plain, _) = receive_in_pieces(&mut session, &mut rb, &wire, &[3]).unwrap();
            assert_eq!(plain, b"bye");
            assert!(session.peer_closed(), "{suite:?}");
        }
    }

    /// Nothing may follow close_notify (RFC 8446 §6.1): a record after it is an error.
    #[test]
    fn a_record_after_close_notify_is_an_error() {
        for (version, suite) in suites() {
            let s = setup(version, suite);
            let (mut session, mut pump, mut rb) = connect(&s);
            let tickets = drain(&mut pump.server);
            receive_in_pieces(&mut session, &mut rb, &tickets, &[1 << 20]).unwrap();
            // The server's sending keys, to forge "close_notify, then data".
            let secrets = pump.server.dangerous_extract_secrets().unwrap();
            let v = if version.version == rustls::ProtocolVersion::TLSv1_3 {
                Version::Tls13
            } else {
                Version::Tls12
            };
            let mut forged = Keys::new(v, &secrets.tx.1, secrets.tx.0).unwrap();
            let mut wire = Vec::new();
            forged.seal(ContentType::Alert, &[1, 0], &mut wire).unwrap();
            forged
                .seal(ContentType::ApplicationData, b"late", &mut wire)
                .unwrap();
            let r = receive_in_pieces(&mut session, &mut rb, &wire, &[1 << 20]);
            assert!(r.is_err(), "{suite:?}: {r:?}");
        }
    }

    /// One changed bit anywhere in a data record ends the session with an error (never
    /// a panic, never wrong plaintext).
    #[test]
    fn a_changed_bit_is_an_error() {
        for (version, suite) in suites() {
            let s = setup(version, suite);
            let (mut session, mut pump, mut rb) = connect(&s);
            let tickets = drain(&mut pump.server);
            receive_in_pieces(&mut session, &mut rb, &tickets, &[1 << 20]).unwrap();
            pump.server.writer().write_all(b"market data").unwrap();
            let wire = drain(&mut pump.server);
            for i in 0..wire.len() {
                let (mut session, mut pump, mut rb) = connect(&s);
                let tickets = drain(&mut pump.server);
                receive_in_pieces(&mut session, &mut rb, &tickets, &[1 << 20]).unwrap();
                pump.server.writer().write_all(b"market data").unwrap();
                let mut bad = drain(&mut pump.server);
                bad[i] ^= 0x80;
                let r = receive_in_pieces(&mut session, &mut rb, &bad, &[1 << 20]);
                match r {
                    Err(_) => {}
                    // A changed length can make the record look incomplete: then nothing
                    // is delivered, which is also safe.
                    Ok((plain, _)) => assert!(plain.is_empty(), "{suite:?} byte {i}: {plain:?}"),
                }
            }
            let _ = (session, rb);
        }
    }
}
