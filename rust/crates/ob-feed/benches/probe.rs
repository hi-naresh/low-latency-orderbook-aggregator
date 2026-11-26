//! What measuring costs: the same Binance messages through a feed with `NoProbe` and
//! with `StageProbe` (3 TSC stamps, 4 histogram records, 1 wall-clock read per message).
//!
//! Binance is used because every message is a full snapshot, so replaying the 59
//! recorded messages in a loop applies every one (Bybit deltas would be ignored as stale
//! on the second pass).
//!
//! Run: `cargo bench -p ob-feed --bench probe`
#![allow(clippy::unwrap_used, missing_docs)]

use std::time::Duration;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use ob_core::InstrumentSpec;
use ob_feed::probe::{Probe, StageProbe, WallClock};
use ob_feed::{Binance, Commands, Feed, FeedConfig};
use ob_sync::book_channel;

const BINANCE: &str = include_str!("../../ob-parse/tests/fixtures/binance_depth20.jsonl");

fn run<P: Probe>(c: &mut Criterion, name: &str, probe: P) {
    let spec = InstrumentSpec::from_steps("0.1", "0.001").unwrap();
    let lines: Vec<&[u8]> = BINANCE.lines().skip(1).map(str::as_bytes).collect();
    let (publisher, _reader) = book_channel();
    let mut feed = Feed::with_probe(
        Binance::new(spec, "BTCUSDT"),
        publisher,
        FeedConfig::default(),
        0,
        probe,
    );
    let mut cmds = Commands::default();
    feed.on_connected(0, &mut cmds);
    let mut g = c.benchmark_group("feed_on_text_binance");
    g.throughput(Throughput::Elements(lines.len() as u64));
    g.bench_function(name, |b| {
        b.iter(|| {
            for l in &lines {
                feed.on_text(1, l, &mut cmds);
            }
            while cmds.pop().is_some() {}
        })
    });
    g.finish();
}

fn bench(c: &mut Criterion) {
    run(c, "no_probe", ob_feed::probe::NoProbe);
    run(
        c,
        "stage_probe_fixed_wall_clock",
        StageProbe::new(WallClock::Fixed(1)),
    );
    run(
        c,
        "stage_probe_system_wall_clock",
        StageProbe::new(WallClock::System),
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
