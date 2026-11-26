//! Order book benchmarks (criterion), against a `BTreeMap` book:
//! 1. `apply`: an update plus reading the best level, on top-heavy and uniform streams;
//! 2. `snapshot`: replacing a whole 20-level book.
//!
//! Run: `cargo bench -p ob-core` (or `scripts/bench.sh -p ob-core` for CPU-specific
//! instructions). Results go to `target/criterion/`.
#![allow(
    clippy::unwrap_used,
    clippy::cast_possible_wrap,
    clippy::cast_possible_truncation,
    missing_docs
)]

use std::collections::BTreeMap;
use std::hint::black_box;
use std::time::Duration;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use ob_core::{Level, OrderBook, Px, Qty, Side};

// Far more updates than the CPU's branch-prediction history holds, so the predictor
// can't memorise the sequence across iterations.
const UPDATES: usize = 262_144;

/// Deterministic pseudo-random numbers, so every run benchmarks identical data.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    /// Distance from the top of the book. `p` = chance of stopping at each level, so
    /// small `p` spreads updates deeper. Capped at `max`.
    fn geometric(&mut self, p_percent: u64, max: u64) -> u64 {
        let mut d = 0;
        while d < max && self.next() % 100 >= p_percent {
            d += 1;
        }
        d
    }
}

// ---------------------------------------------------------------------------
// 1. Apply (update + read best), against a BTreeMap book
// ---------------------------------------------------------------------------

/// The straightforward alternative: one ordered map per side.
#[derive(Default)]
struct BTreeBook {
    bids: BTreeMap<i64, i64>,
    asks: BTreeMap<i64, i64>,
}

impl BTreeBook {
    fn apply(&mut self, side: Side, px: i64, qty: i64) {
        let map = match side {
            Side::Bid => &mut self.bids,
            Side::Ask => &mut self.asks,
        };
        if qty == 0 {
            map.remove(&px);
        } else {
            map.insert(px, qty);
        }
    }
    fn best(&self, side: Side) -> Option<i64> {
        match side {
            Side::Bid => self.bids.last_key_value().map(|(_, &q)| q),
            Side::Ask => self.asks.first_key_value().map(|(_, &q)| q),
        }
    }
}

/// Updates on both sides around a mid price; ~20% removals (quantity 0). Top-heavy:
/// distance from the top is geometric; otherwise uniform over 40 levels.
fn update_stream(top_heavy: bool) -> Vec<(Side, i64, i64)> {
    let mut rng = Rng(0x2545_F491_4F6C_DD1D);
    (0..UPDATES)
        .map(|_| {
            let side = if rng.next().is_multiple_of(2) {
                Side::Bid
            } else {
                Side::Ask
            };
            let d = if top_heavy {
                rng.geometric(40, 39)
            } else {
                rng.next() % 40
            } as i64;
            let px = match side {
                Side::Bid => 600_000 - d,
                Side::Ask => 600_001 + d,
            };
            let qty = (rng.next() % 5) as i64;
            (side, px, qty)
        })
        .collect()
}

fn run_book(book: &mut OrderBook<64>, stream: &[(Side, i64, i64)]) -> i64 {
    let mut acc = 0;
    for &(side, px, qty) in stream {
        book.apply(
            side,
            Level::new(Px::from_raw(px), Qty::from_raw(qty).unwrap()),
        );
        let best = match side {
            Side::Bid => book.best_bid(),
            Side::Ask => book.best_ask(),
        };
        acc += best.map_or(0, |l| l.qty.raw());
    }
    acc
}

fn bench_apply(c: &mut Criterion) {
    let mut group = c.benchmark_group("apply");
    group.throughput(Throughput::Elements(UPDATES as u64));
    for (workload, top_heavy) in [("top-heavy", true), ("uniform-40", false)] {
        let stream = update_stream(top_heavy);
        let mut book: OrderBook<64> = OrderBook::new();
        run_book(&mut book, &stream); // warm up to a steady-state book
        group.bench_function(BenchmarkId::new("sorted_arrays", workload), |b| {
            b.iter(|| run_book(&mut book, black_box(&stream)));
        });

        let mut tree = BTreeBook::default();
        let run_tree = |tree: &mut BTreeBook, stream: &[(Side, i64, i64)]| {
            let mut acc = 0;
            for &(side, px, qty) in stream {
                tree.apply(side, px, qty);
                acc += tree.best(side).unwrap_or(0);
            }
            acc
        };
        run_tree(&mut tree, &stream);
        group.bench_function(BenchmarkId::new("btreemap", workload), |b| {
            b.iter(|| run_tree(&mut tree, black_box(&stream)));
        });
    }
    group.finish();
}

// ---------------------------------------------------------------------------
// 2. Snapshot replace (Binance sends a 20-level snapshot every message)
// ---------------------------------------------------------------------------

fn bench_snapshot(c: &mut Criterion) {
    let lvl = |px: i64| Level::new(Px::from_raw(px), Qty::from_raw(px % 7 + 1).unwrap());
    let bids: Vec<Level> = (0..20).map(|i| lvl(600_000 - i)).collect(); // best-first
    let asks: Vec<Level> = (0..20).map(|i| lvl(600_001 + i)).collect();

    let mut group = c.benchmark_group("snapshot/20x20");
    group.throughput(Throughput::Elements(1));
    let mut book: OrderBook = OrderBook::new();
    group.bench_function("replace", |b| {
        b.iter(|| book.replace(black_box(&bids), black_box(&asks)).unwrap());
    });
    group.bench_function("clear+apply_each", |b| {
        b.iter(|| {
            book.clear();
            for l in black_box(&bids) {
                book.apply(Side::Bid, *l);
            }
            for l in black_box(&asks) {
                book.apply(Side::Ask, *l);
            }
        });
    });
    let mut tree = BTreeBook::default();
    group.bench_function("btreemap_rebuild", |b| {
        b.iter(|| {
            tree.bids.clear();
            tree.asks.clear();
            for l in black_box(&bids) {
                tree.bids.insert(l.px.raw(), l.qty.raw());
            }
            for l in black_box(&asks) {
                tree.asks.insert(l.px.raw(), l.qty.raw());
            }
        });
    });
    group.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .warm_up_time(Duration::from_millis(500))
        .measurement_time(Duration::from_secs(2))
        .sample_size(50);
    targets = bench_apply, bench_snapshot
}
criterion_main!(benches);
