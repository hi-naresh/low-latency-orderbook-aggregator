//! Measures wake-up-to-message latency of `WaitMode::Spin` vs `WaitMode::Block`.
//!
//! A local TLS WebSocket server (same process, loopback) sends ~1 KB binary messages, each
//! carrying the time it was sent; the client records `decoded − sent`. That covers: the
//! kernel delivering the packet, the client thread noticing (spinning vs being woken),
//! TLS decryption and WebSocket decoding. No real network is involved: it isolates what
//! the *code and the wait strategy* cost.
//!
//! Ignored by default (≈7 s, prints a table): `cargo test -p ob-net --release --test
//! latency -- --ignored --nocapture`
#![allow(
    clippy::unwrap_used,
    clippy::cast_possible_truncation,
    clippy::print_stdout
)]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use ob_net::ws::{Message, Opcode, encode_server_frame, handshake};
use ob_net::{ConnConfig, WaitMode, WsConnection, tls};
use rustls::pki_types::PrivatePkcs8KeyDer;
use rustls::{RootCertStore, ServerConfig, ServerConnection, StreamOwned};

static BASE: OnceLock<Instant> = OnceLock::new();

fn now_ns() -> u64 {
    BASE.get_or_init(Instant::now).elapsed().as_nanos() as u64
}

/// Runs one scenario: `count` messages, one every `interval`, and returns the latencies.
fn scenario(mode: WaitMode, count: usize, interval: Duration) -> Vec<u64> {
    let ck = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    let cert = ck.cert.der().clone();
    let server_cfg = Arc::new(
        ServerConfig::builder_with_provider(tls::crypto_provider())
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![cert.clone()],
                PrivatePkcs8KeyDer::from(ck.signing_key.serialize_der()).into(),
            )
            .unwrap(),
    );
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = thread::spawn(move || {
        let (sock, _) = listener.accept().unwrap();
        sock.set_nodelay(true).unwrap();
        let mut s = StreamOwned::new(ServerConnection::new(server_cfg).unwrap(), sock);
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
        thread::sleep(Duration::from_millis(50)); // let the client settle into its wait mode
        let mut payload = vec![b'x'; 1000];
        let mut frame = Vec::new();
        let mut next = Instant::now();
        for _ in 0..count {
            next += interval;
            while Instant::now() < next {
                std::hint::spin_loop(); // precise schedule
            }
            payload[..8].copy_from_slice(&now_ns().to_le_bytes());
            frame.clear();
            encode_server_frame(&mut frame, true, Opcode::Binary, &payload);
            s.write_all(&frame).unwrap();
            s.flush().unwrap();
        }
        thread::sleep(Duration::from_millis(200));
    });

    let mut roots = RootCertStore::empty();
    roots.add(cert).unwrap();
    let mut cfg = ConnConfig::new("localhost", "/ws").unwrap();
    cfg.port = port;
    cfg.tls = tls::client_config_with_roots(roots).unwrap();
    let mut conn = WsConnection::connect(&cfg).unwrap();
    conn.set_wait(mode).unwrap();
    let mut latencies = Vec::with_capacity(count);
    while latencies.len() < count {
        if let Some(Message::Binary(p)) = conn.poll().unwrap() {
            let sent = u64::from_le_bytes(p[..8].try_into().unwrap());
            latencies.push(now_ns() - sent);
        }
    }
    server.join().unwrap();
    latencies
}

fn report(label: &str, mut v: Vec<u64>) {
    v.sort_unstable();
    // Percentile by rank, in per-mille (500 = p50), integer index: no float rounding.
    let p = |permille: usize| v[(v.len() - 1) * permille / 1000] as f64 / 1000.0;
    println!(
        "{label:<34} n={:<5} p50 {:>6.1} µs  p90 {:>6.1} µs  p99 {:>6.1} µs  max {:>7.1} µs",
        v.len(),
        p(500),
        p(900),
        p(990),
        *v.last().unwrap() as f64 / 1000.0
    );
}

/// Runs `scenario` on a fresh thread, optionally with macOS's user-interactive QoS
/// class (or Linux SCHED_FIFO, if permitted) set first.
fn on_thread(qos: bool, mode: WaitMode, count: usize, interval: Duration) -> Vec<u64> {
    thread::spawn(move || {
        if qos {
            ob_os::set_realtime_priority(50).expect("priority");
        }
        scenario(mode, count, interval)
    })
    .join()
    .unwrap()
}

#[test]
#[ignore = "measurement, ~30 s; run with --ignored --nocapture"]
fn qos_vs_default_wakeup_latency() {
    now_ns();
    let block = WaitMode::Block {
        timeout: Duration::from_millis(100),
    };
    // Interleaved rounds: the machine's state drifts, so alternate the variants.
    for round in 1..=2 {
        for (mode_name, mode) in [("spin", WaitMode::Spin), ("block", block)] {
            for qos in [false, true] {
                let label = format!(
                    "r{round} {mode_name:<5} {}",
                    if qos {
                        "QoS user-interactive"
                    } else {
                        "default"
                    }
                );
                report(&label, on_thread(qos, mode, 150, Duration::from_millis(20)));
            }
        }
    }
}

#[test]
#[ignore = "measurement, ~7 s; run with --ignored --nocapture"]
fn spin_vs_block_wakeup_latency() {
    now_ns();
    let block = WaitMode::Block {
        timeout: Duration::from_millis(100),
    };
    for (name, count, interval) in [
        ("busy (every 200 µs)", 2_000, Duration::from_micros(200)),
        ("realistic (every 20 ms)", 150, Duration::from_millis(20)),
    ] {
        report(
            &format!("spin,  {name}"),
            scenario(WaitMode::Spin, count, interval),
        );
        report(&format!("block, {name}"), scenario(block, count, interval));
    }
}
