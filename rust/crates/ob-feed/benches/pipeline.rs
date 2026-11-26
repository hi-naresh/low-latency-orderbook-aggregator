//! The whole per-message path, on recorded live data: parse → sequence check → apply →
//! publish, through the real `Feed` (no network). The yardstick for tuning: every change is
//! measured against these numbers, before and after.
//!
//! Each batch starts from a fresh, connected feed (set-up excluded from the timing) and
//! feeds every recorded message once, in order, so delta streams stay valid.
//!
//! Run: `cargo bench -p ob-feed --bench pipeline`
#![allow(clippy::unwrap_used, missing_docs)]

use std::time::Duration;

use criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main};
use ob_core::InstrumentSpec;
use ob_feed::{Binance, BinanceFullDepth, Bybit, Commands, Feed, FeedConfig, Venue};
use ob_sync::book_channel;

const FIX: &str = "../ob-parse/tests/fixtures/";
macro_rules! fixture {
    ($name:literal) => {
        include_str!(concat!("../../ob-parse/tests/fixtures/", $name))
    };
}
const BINANCE: &str = fixture!("binance_depth20.jsonl");
const BYBIT: &str = fixture!("bybit_orderbook50.jsonl");
const BINANCE_TOP: &str = fixture!("binance_depth_and_bookticker.jsonl");
const BYBIT_TOP: &str = fixture!("bybit_orderbook50_and_1.jsonl");
const FULL_SNAPSHOT: &str = fixture!("binance_full_snapshot.json");
const FULL_DIFFS: &str = fixture!("binance_full_diffs.jsonl");

fn spec() -> InstrumentSpec {
    InstrumentSpec::from_steps("0.1", "0.001").unwrap()
}

fn bench_venue<V: Venue>(
    c: &mut Criterion,
    name: &str,
    make: impl Fn() -> V,
    snapshot: Option<&str>,
    lines: &[&str],
) {
    let _ = FIX;
    let mut g = c.benchmark_group("pipeline");
    g.throughput(Throughput::Elements(lines.len() as u64));
    g.bench_function(name, |b| {
        b.iter_batched_ref(
            || {
                let (publisher, reader) = book_channel();
                let mut feed = Feed::new(make(), publisher, FeedConfig::default(), 1);
                let mut cmds = Commands::default();
                feed.on_connected(0, &mut cmds);
                if let Some(s) = snapshot {
                    // Full depth: the triggering event, then the snapshot (set-up).
                    feed.on_text(0, lines[0].as_bytes(), &mut cmds);
                    feed.on_snapshot(0, s.as_bytes(), &mut cmds);
                }
                (feed, cmds, reader)
            },
            |(feed, cmds, _reader)| {
                for l in lines {
                    feed.on_text(1, l.as_bytes(), cmds);
                }
                while cmds.pop().is_some() {}
            },
            BatchSize::LargeInput,
        );
    });
    g.finish();
}

fn bench(c: &mut Criterion) {
    let lines = |s: &'static str| s.lines().collect::<Vec<_>>();
    bench_venue(
        c,
        "binance_depth20",
        || Binance::new(spec(), "BTCUSDT"),
        None,
        &lines(BINANCE),
    );
    bench_venue(
        c,
        "bybit_orderbook50",
        || Bybit::new(spec(), "BTCUSDT"),
        None,
        &lines(BYBIT),
    );
    bench_venue(
        c,
        "binance_depth20+bookTicker",
        || Binance::new(spec(), "BTCUSDT").with_top_of_book(),
        None,
        &lines(BINANCE_TOP),
    );
    bench_venue(
        c,
        "bybit_orderbook50+1",
        || Bybit::new(spec(), "BTCUSDT").with_top_of_book(),
        None,
        &lines(BYBIT_TOP),
    );
    bench_venue(
        c,
        "binance_full_depth",
        || BinanceFullDepth::new(spec(), "BTCUSDT"),
        Some(FULL_SNAPSHOT),
        &lines(FULL_DIFFS),
    );
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
