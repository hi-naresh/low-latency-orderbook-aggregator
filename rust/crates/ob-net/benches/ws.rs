//! WebSocket decoding benchmarks on the real recorded messages, framed as the server
//! sends them: 59 Binance depth messages (~1 KB each) and 59 Bybit book messages (one
//! 2.3 KB snapshot, then ~250-byte deltas).
//!
//! * `ob_net` with and without UTF-8 validation (the RFC requires it; it costs a pass).
//! * `tungstenite`: the same bytes read through tungstenite's `WebSocket` over an
//!   in-memory stream (it copies each message into its own buffer).
//!
//! Run: `cargo bench -p ob-net --bench ws`
#![allow(clippy::unwrap_used, missing_docs)]

use std::hint::black_box;
use std::io::Cursor;
use std::time::Duration;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use ob_net::ws::{Message, MessageReader, Opcode, ReaderConfig, encode_server_frame};

const BINANCE: &str = include_str!("../../ob-parse/tests/fixtures/binance_depth20.jsonl");
const BYBIT: &str = include_str!("../../ob-parse/tests/fixtures/bybit_orderbook50.jsonl");

fn framed(fixture: &str) -> (Vec<u8>, usize) {
    let mut stream = Vec::new();
    let mut n = 0;
    for line in fixture.lines().filter(|l| l.contains("\"b\"")) {
        encode_server_frame(&mut stream, true, Opcode::Text, line.as_bytes());
        n += 1;
    }
    (stream, n)
}

fn decode_all(reader: &mut MessageReader, mut buf: &[u8]) -> usize {
    let mut total = 0;
    while let Some(d) = reader.decode(buf).unwrap() {
        if let Some(Message::Text(t)) = d.message {
            total += t.len();
        }
        buf = &buf[d.consumed..];
    }
    total
}

fn bench(c: &mut Criterion) {
    for (name, fixture) in [("binance", BINANCE), ("bybit", BYBIT)] {
        let (stream, n) = framed(fixture);
        let mut g = c.benchmark_group(format!("ws_decode/{name}"));
        g.throughput(Throughput::Elements(n as u64));
        let mut fast = MessageReader::new(ReaderConfig {
            validate_utf8: false,
            ..ReaderConfig::default()
        });
        g.bench_function("ob_net (no utf8 check)", |b| {
            b.iter(|| decode_all(&mut fast, black_box(&stream)))
        });
        let mut strict = MessageReader::new(ReaderConfig::default());
        g.bench_function("ob_net (utf8 checked)", |b| {
            b.iter(|| decode_all(&mut strict, black_box(&stream)))
        });
        // The tungstenite harness copies the stream each iteration (it reads from an owned
        // in-memory socket); measured alone so it can be subtracted for a fair comparison.
        g.bench_function("(harness: stream clone only)", |b| {
            b.iter(|| black_box(&stream).clone())
        });
        g.bench_function("tungstenite", |b| {
            b.iter(|| {
                let mut ws = tungstenite::WebSocket::from_raw_socket(
                    Cursor::new(black_box(&stream).clone()),
                    tungstenite::protocol::Role::Client,
                    None,
                );
                let mut total = 0;
                for _ in 0..n {
                    if let tungstenite::Message::Text(t) = ws.read().unwrap() {
                        total += t.len();
                    }
                }
                total
            });
        });
        g.finish();
    }
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .warm_up_time(Duration::from_millis(500))
        .measurement_time(Duration::from_secs(3))
        .sample_size(50);
    targets = bench
}
criterion_main!(benches);
