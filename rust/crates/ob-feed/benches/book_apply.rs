//! Step 26: the full-depth book's data structure, A/B on recorded data.
//!
//! The recorded Binance REST snapshot (1,000 levels a side), then its 100 diff events,
//! applied to (a) the sorted-array `OrderBook<1024>` used until step 26 and (b) the
//! price-indexed `TickBook<1024>`. Parsing is done beforehand: this measures applying
//! only, with the full-depth adapter's rule of ignoring levels beyond the snapshot's
//! known region.
//!
//! Run: `cargo bench -p ob-feed --bench book_apply`
#![allow(clippy::unwrap_used, missing_docs)]

use std::hint::black_box;
use std::time::Duration;

use criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main};
use ob_core::{InstrumentSpec, Level, OrderBook, Px, Side, TickBook};
use ob_feed::full_depth::FULL_CAP;
use ob_parse::binance::{BinanceDepthParser, DepthUpdate};

const SNAPSHOT: &str = include_str!("../../ob-parse/tests/fixtures/binance_full_snapshot.json");
const DIFFS: &str = include_str!("../../ob-parse/tests/fixtures/binance_full_diffs.jsonl");

struct Data {
    bids: Vec<Level>,
    asks: Vec<Level>,
    /// Per diff event: (side, level), in message order.
    events: Vec<Vec<(Side, Level)>>,
    floor: Px,
    ceiling: Px,
}

fn data() -> Data {
    let spec = InstrumentSpec::from_steps("0.1", "0.001").unwrap();
    let p = BinanceDepthParser::new(spec, b"BTCUSDT");
    let (mut bids, mut asks) = (Vec::new(), Vec::new());
    p.parse_rest_snapshot(SNAPSHOT.as_bytes(), |side, l| {
        match side {
            Side::Bid => bids.push(l),
            Side::Ask => asks.push(l),
        }
        true
    })
    .unwrap();
    let mut m = DepthUpdate::default();
    let events = DIFFS
        .lines()
        .map(|l| {
            p.parse(l.as_bytes(), &mut m).unwrap();
            let b = m.bids.as_slice().iter().map(|&l| (Side::Bid, l));
            let a = m.asks.as_slice().iter().map(|&l| (Side::Ask, l));
            b.chain(a).collect()
        })
        .collect();
    let floor = bids.last().unwrap().px;
    let ceiling = asks.last().unwrap().px;
    Data {
        bids,
        asks,
        events,
        floor,
        ceiling,
    }
}

fn known(d: &Data, side: Side, l: Level) -> bool {
    match side {
        Side::Bid => l.px >= d.floor,
        Side::Ask => l.px <= d.ceiling,
    }
}

fn bench(c: &mut Criterion) {
    let d = data();
    let n: usize = d.events.iter().map(Vec::len).sum();
    let mut g = c.benchmark_group("full_depth_apply");
    g.throughput(Throughput::Elements(n as u64));
    g.bench_function("sorted arrays (OrderBook<1024>)", |b| {
        b.iter_batched_ref(
            || {
                let mut book: Box<OrderBook<FULL_CAP>> = Box::new(OrderBook::new());
                book.replace(&d.bids, &d.asks).unwrap();
                book
            },
            |book| {
                for ev in &d.events {
                    for &(side, l) in ev {
                        if known(&d, side, l) {
                            black_box(book.apply(side, l));
                        }
                    }
                }
            },
            BatchSize::LargeInput,
        );
    });
    let tick = InstrumentSpec::from_steps("0.1", "0.001").unwrap().tick();
    g.bench_function("price-indexed (TickBook<1024>)", |b| {
        b.iter_batched_ref(
            || {
                let mut book: Box<TickBook<FULL_CAP>> = Box::new(TickBook::new(tick));
                book.replace(&d.bids, &d.asks).unwrap();
                book
            },
            |book| {
                for ev in &d.events {
                    for &(side, l) in ev {
                        if known(&d, side, l) {
                            black_box(book.apply(side, l));
                        }
                    }
                }
            },
            BatchSize::LargeInput,
        );
    });
    g.finish();
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
