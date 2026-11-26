//! Bybit order book benchmarks on the 59 real recorded book frames: one 50+50-level
//! snapshot and 58 small deltas.
//!
//! * `serde_typed`: `#[derive(Deserialize)]` borrowing `&str`, levels converted with our
//!   fast decimal parser (the strongest idiomatic serde baseline).
//! * `ob_parse`: this crate's parser into a reused buffer.
//! * `ob_parse+book`: parse, check the sequence, apply to the book, check for a cross:
//!   everything the Bybit hot path does per message.
//!
//! Run: `cargo bench -p ob-parse --bench bybit`
#![allow(clippy::unwrap_used, clippy::cast_possible_wrap, missing_docs)]

use std::hint::black_box;
use std::time::Duration;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use ob_core::{InstrumentSpec, Level, OrderBook};
use ob_parse::bybit::{BookMsg, BybitBookParser, Message, Sequence};
use serde::Deserialize;

const FIXTURE: &str = include_str!("../tests/fixtures/bybit_orderbook50.jsonl");

#[derive(Deserialize)]
struct Data<'a> {
    #[serde(borrow)]
    s: &'a str,
    #[serde(borrow)]
    b: Vec<[&'a str; 2]>,
    #[serde(borrow)]
    a: Vec<[&'a str; 2]>,
    u: u64,
    seq: u64,
}

#[derive(Deserialize)]
struct Typed<'a> {
    #[serde(borrow)]
    topic: &'a str,
    #[serde(borrow, rename = "type")]
    kind: &'a str,
    ts: u64,
    #[serde(borrow)]
    data: Data<'a>,
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
    let parser = BybitBookParser::new(spec, b"orderbook.50.BTCUSDT", b"BTCUSDT");
    let book_frames: Vec<&[u8]> = FIXTURE
        .lines()
        .map(str::as_bytes)
        .filter(|f| f.starts_with(b"{\"topic\""))
        .collect();
    let (snapshot, deltas) = (book_frames[0], &book_frames[1..]);
    eprintln!(
        "snapshot {} bytes; {} deltas, {:.0} bytes each on average",
        snapshot.len(),
        deltas.len(),
        deltas.iter().map(|d| d.len()).sum::<usize>() as f64 / deltas.len() as f64
    );

    let typed = |f: &[u8]| -> i64 {
        let t: Typed<'_> = serde_json::from_slice(f).unwrap();
        assert!(t.topic.len() > 1 && t.data.s == "BTCUSDT" && !t.kind.is_empty());
        let (b, a) = (convert(&spec, &t.data.b), convert(&spec, &t.data.a));
        (b.len() + a.len()) as i64 + (t.ts + t.data.u + t.data.seq) as i64
    };
    let mut out = BookMsg::default();

    let mut group = c.benchmark_group("bybit/snapshot_50x50");
    group.throughput(Throughput::Elements(1));
    group.bench_function("serde_typed", |b| b.iter(|| typed(black_box(snapshot))));
    group.bench_function("ob_parse", |b| {
        b.iter(|| {
            assert_eq!(
                parser.parse(black_box(snapshot), &mut out).unwrap(),
                Message::Book
            );
            out.bids.len()
        });
    });
    group.finish();

    let mut group = c.benchmark_group("bybit/deltas");
    group.throughput(Throughput::Elements(deltas.len() as u64));
    group.bench_function("serde_typed", |b| {
        b.iter(|| black_box(deltas).iter().map(|d| typed(d)).sum::<i64>())
    });
    group.bench_function("ob_parse", |b| {
        b.iter(|| {
            let mut acc = 0;
            for d in black_box(deltas) {
                parser.parse(d, &mut out).unwrap();
                acc += out.bids.len() + out.asks.len();
            }
            acc
        });
    });
    group.finish();

    // The whole hot path over the recording: snapshot, then each delta in sequence.
    let mut group = c.benchmark_group("bybit/full_path");
    group.throughput(Throughput::Elements(book_frames.len() as u64));
    let mut book: OrderBook = OrderBook::new();
    group.bench_function("ob_parse+sequence+book", |b| {
        b.iter(|| {
            let mut last = None;
            for f in black_box(&book_frames) {
                parser.parse(f, &mut out).unwrap();
                match out.sequence(last) {
                    Sequence::Reset | Sequence::Next => {
                        out.apply_to(&mut book).unwrap();
                        assert!(!book.is_crossed());
                        last = Some(out.update_id);
                    }
                    other => panic!("{other:?}"),
                }
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
