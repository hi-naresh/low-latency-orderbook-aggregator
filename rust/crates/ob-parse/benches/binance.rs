//! Binance depth message benchmarks: the hand-written parser against serde_json, on the
//! 59 real recorded depth frames (each 20 + 20 levels, ~1 KB).
//!
//! Every variant produces the same thing, validated `Px`/`Qty` levels for both sides,
//! so the comparison is like for like:
//! * `serde_value`: parse to a generic `serde_json::Value` tree, then extract. The usual
//!   quick approach (and closest to the Java skeleton's `JsonNode`).
//! * `serde_typed`: `#[derive(Deserialize)]` struct borrowing `&str` from the input, the
//!   strongest idiomatic serde approach; levels converted with our fast decimal parser.
//! * `ob_parse`: this crate's parser (fused scan + decimal, into a reused buffer).
//! * `ob_parse+book`: parse and replace the order book, the whole hot path for Binance.
//!
//! Run: `cargo bench -p ob-parse --bench binance`
#![allow(
    clippy::unwrap_used,
    clippy::cast_possible_wrap,
    missing_docs,
    non_snake_case
)]

use std::hint::black_box;
use std::time::Duration;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use ob_core::{InstrumentSpec, Level, OrderBook};
use ob_parse::binance::{BinanceDepthParser, DepthUpdate, Message};
use serde::Deserialize;

const FIXTURE: &str = include_str!("../tests/fixtures/binance_depth20.jsonl");

#[derive(Deserialize)]
struct Typed<'a> {
    #[serde(borrow)]
    e: &'a str,
    E: u64,
    T: u64,
    #[serde(borrow)]
    s: &'a str,
    U: u64,
    u: u64,
    pu: u64,
    #[serde(borrow)]
    b: Vec<[&'a str; 2]>,
    #[serde(borrow)]
    a: Vec<[&'a str; 2]>,
}

fn convert(spec: &InstrumentSpec, raw: &[[&str; 2]]) -> Vec<Level> {
    raw.iter()
        .map(|[p, q]| {
            Level::new(
                ob_parse::parse_px(spec, p.as_bytes()).unwrap(),
                ob_parse::parse_qty(spec, q.as_bytes()).unwrap(),
            )
        })
        .collect()
}

fn bench(c: &mut Criterion) {
    let spec = InstrumentSpec::from_steps("0.1", "0.001").unwrap();
    let frames: Vec<&[u8]> = FIXTURE.lines().skip(1).map(str::as_bytes).collect();
    let bytes: usize = frames.iter().map(|f| f.len()).sum();
    let parser = BinanceDepthParser::new(spec, b"BTCUSDT");

    let mut group = c.benchmark_group("binance_depth20");
    group.throughput(Throughput::Elements(frames.len() as u64));
    eprintln!(
        "{} frames, {} bytes, {:.0} bytes/frame",
        frames.len(),
        bytes,
        bytes as f64 / frames.len() as f64
    );

    group.bench_function("serde_value", |b| {
        b.iter(|| {
            let mut acc = 0_i64;
            for f in black_box(&frames) {
                let v: serde_json::Value = serde_json::from_slice(f).unwrap();
                let side = |k: &str| -> Vec<Level> {
                    v[k].as_array()
                        .unwrap()
                        .iter()
                        .map(|l| {
                            Level::new(
                                spec.parse_px(l[0].as_str().unwrap()).unwrap(),
                                spec.parse_qty(l[1].as_str().unwrap()).unwrap(),
                            )
                        })
                        .collect()
                };
                let (bids, asks) = (side("b"), side("a"));
                acc += bids[0].px.raw() + asks[0].px.raw() + v["u"].as_u64().unwrap() as i64;
            }
            acc
        });
    });

    group.bench_function("serde_typed", |b| {
        b.iter(|| {
            let mut acc = 0_i64;
            for f in black_box(&frames) {
                let t: Typed<'_> = serde_json::from_slice(f).unwrap();
                assert!(t.e == "depthUpdate" && t.s == "BTCUSDT");
                let (bids, asks) = (convert(&spec, &t.b), convert(&spec, &t.a));
                acc += bids[0].px.raw() + asks[0].px.raw() + (t.u + t.U + t.pu + t.E + t.T) as i64;
            }
            acc
        });
    });

    let mut out = DepthUpdate::default();
    group.bench_function("ob_parse", |b| {
        b.iter(|| {
            let mut acc = 0_i64;
            for f in black_box(&frames) {
                assert_eq!(parser.parse(f, &mut out).unwrap(), Message::Depth);
                acc += out.bids[0].px.raw() + out.asks[0].px.raw() + out.final_update_id as i64;
            }
            acc
        });
    });

    let mut book: OrderBook = OrderBook::new();
    group.bench_function("ob_parse+book", |b| {
        b.iter(|| {
            for f in black_box(&frames) {
                parser.parse(f, &mut out).unwrap();
                out.apply_to(&mut book).unwrap();
            }
            book.best_bid().unwrap().px.raw()
        });
    });
    group.finish();
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
