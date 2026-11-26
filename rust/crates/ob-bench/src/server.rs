//! A local exchange: a TLS WebSocket server that sends a stream **open loop**.
//!
//! Send times follow a Poisson process (exponential gaps around a chosen mean) planned
//! in advance, independent of the client. If the client falls behind, messages queue in
//! the socket and their measured latency includes the wait. A closed-loop sender (send,
//! wait for the reply, send the next) would pause during exactly the slow moments and
//! hide them ("coordinated omission").
//!
//! Each message is one TLS record. The server encrypts it, then reads the clock and
//! stores that send time in `stamps[i]`, then writes the ciphertext to the socket. The
//! client's end-to-end latency for message *i* is `published − stamps[i]`. Both threads
//! read the same clock: rdtsc is invariant and synchronised across cores on the x86
//! Macs; elsewhere quanta reads the OS monotonic clock.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::{self, JoinHandle};

use ob_net::ConnConfig;
use ob_net::tls;
use ob_net::ws::{Opcode, encode_server_frame, handshake};

use crate::stream::Stream;

/// What the server did.
#[derive(Debug, Default, Clone, Copy)]
pub struct Sent {
    /// Messages sent.
    pub messages: u64,
    /// Sends that started more than 20 µs after their planned time (the sender thread
    /// was descheduled): they spread the planned arrivals but don't distort latency,
    /// which is measured from the actual send.
    pub late: u64,
}

/// A TLS server configuration with a fresh self-signed certificate for `localhost`, and
/// a client configuration that trusts only that certificate.
#[allow(clippy::unwrap_used)] // set-up: a failure here is a broken environment
pub(crate) fn localhost_tls() -> (Arc<rustls::ServerConfig>, Arc<rustls::ClientConfig>) {
    let ck = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    let cert = ck.cert.der().clone();
    let key = rustls::pki_types::PrivatePkcs8KeyDer::from(ck.signing_key.serialize_der());
    let server = rustls::ServerConfig::builder_with_provider(tls::crypto_provider())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![cert.clone()], key.into())
        .unwrap();
    let mut roots = rustls::RootCertStore::empty();
    roots.add(cert).unwrap();
    (
        Arc::new(server),
        tls::client_config_with_roots(roots).unwrap(),
    )
}

/// Starts the server for `n` messages of `stream`, at mean gap `mean_gap_ns`. Returns
/// the client's connection settings and the server thread.
pub fn spawn(
    mut stream: Stream,
    n: usize,
    mean_gap_ns: u64,
    clock: quanta::Clock,
    stamps: Arc<[AtomicU64]>,
    seed: u64,
) -> (ConnConfig, JoinHandle<Sent>) {
    #[allow(clippy::unwrap_used)] // set-up: a failure here is a broken environment
    let (cfg, listener, server_cfg) = {
        let (server_cfg, client_tls) = localhost_tls();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut cfg = ConnConfig::new("localhost", "/ws").unwrap();
        cfg.port = listener.local_addr().unwrap().port();
        cfg.tls = client_tls;
        (cfg, listener, server_cfg)
    };
    let handle = thread::spawn(move || {
        #[allow(clippy::unwrap_used)]
        let mut s = {
            let (sock, _) = listener.accept().unwrap();
            sock.set_nodelay(true).unwrap();
            let conn = rustls::ServerConnection::new(server_cfg).unwrap();
            let mut s = rustls::StreamOwned::new(conn, sock);
            upgrade(&mut s);
            // The subscription (its content doesn't matter here).
            let mut buf = [0_u8; 4096];
            let _ = s.read(&mut buf).unwrap();
            s
        };
        let mut rng = SplitMix64(seed);
        let mut frame = Vec::with_capacity(64 * 1024);
        let mut sent = Sent::default();
        let mut planned = clock.raw();
        let ns_per_tick = clock.delta_as_nanos(0, 1_000_000_000) as f64 / 1e9;
        for stamp in stamps.iter().take(n) {
            // Next planned send: exponential gap (Poisson arrivals).
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let gap_ticks = (rng.exponential(mean_gap_ns as f64) / ns_per_tick) as u64;
            planned += gap_ticks;
            while clock.raw() < planned {
                std::hint::spin_loop();
            }
            frame.clear();
            encode_server_frame(&mut frame, true, Opcode::Text, stream.next_message());
            if s.conn.writer().write_all(&frame).is_err() {
                break;
            }
            let now = clock.raw();
            if clock.delta_as_nanos(planned, now) > 20_000 {
                sent.late += 1;
            }
            stamp.store(now, Ordering::Release);
            let mut ok = true;
            while s.conn.wants_write() {
                if s.conn.write_tls(&mut s.sock).is_err() {
                    ok = false;
                    break;
                }
            }
            if !ok {
                break;
            }
            sent.messages += 1;
        }
        // Until the client closes.
        let mut buf = [0_u8; 4096];
        while s.read(&mut buf).is_ok_and(|n| n > 0) {}
        sent
    });
    (cfg, handle)
}

/// The server side of the WebSocket upgrade.
#[allow(clippy::unwrap_used)]
fn upgrade(s: &mut rustls::StreamOwned<rustls::ServerConnection, std::net::TcpStream>) {
    let mut req = Vec::new();
    let mut b = [0_u8];
    while !req.ends_with(b"\r\n\r\n") {
        s.read_exact(&mut b).unwrap();
        req.push(b[0]);
    }
    let req = String::from_utf8(req).unwrap();
    let key = req
        .lines()
        .find_map(|l| l.strip_prefix("Sec-WebSocket-Key: "))
        .unwrap()
        .trim();
    let resp = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {}\r\n\r\n",
        handshake::expected_accept(key)
    );
    s.write_all(resp.as_bytes()).unwrap();
    s.flush().unwrap();
}

/// Small, fast, seedable generator for the arrival schedule.
pub struct SplitMix64(pub u64);

impl SplitMix64 {
    /// Next 64 random bits.
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// An exponentially distributed value with the given mean.
    pub fn exponential(&mut self, mean: f64) -> f64 {
        // Uniform in (0, 1]: never 0, so ln is finite.
        #[allow(clippy::cast_precision_loss)]
        let u = ((self.next_u64() >> 11) + 1) as f64 / (1_u64 << 53) as f64;
        -mean * u.ln()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exponential_gaps_have_the_requested_mean() {
        let mut r = SplitMix64(7);
        let n = 200_000;
        let mean = (0..n).map(|_| r.exponential(1000.0)).sum::<f64>() / f64::from(n);
        assert!((mean - 1000.0).abs() < 10.0, "{mean}");
    }
}
