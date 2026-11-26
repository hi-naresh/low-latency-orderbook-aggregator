//! Order book benchmarks (criterion).
//!
//! Three questions, each answered with numbers:
//! 1. `search`: which way of finding a price is fastest, for each access pattern?
//! 2. `apply`: how fast is a full update (+ reading the best level) for each search
//!    strategy, compared with a `BTreeMap` book?
//! 3. `snapshot`: how fast is replacing the whole book?
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
use ob_core::search::{BranchlessBinary, Hybrid, LinearFromBest, Search, StdBinary};
use ob_core::{Level, OrderBook, Px, Qty, Side};

// Inputs are far larger than the CPU's branch-prediction history, so the predictor can't
// memorise the sequence across iterations. (A first version used 1,024 queries and
// 4,096 updates, and the predictor partly learned them, flattering branchy searches.)
const QUERIES: usize = 65_536;
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
// 1. Search
// ---------------------------------------------------------------------------

/// How far from the top of the book the searched prices are.
#[derive(Clone, Copy)]
enum Pattern {
    Top,      // always the best price
    TopHeavy, // mostly the top few levels: realistic market data
    Uniform,  // any level equally likely
    Bottom,   // always the worst price
}

impl Pattern {
    fn name(self) -> &'static str {
        match self {
            Pattern::Top => "top",
            Pattern::TopHeavy => "top-heavy",
            Pattern::Uniform => "uniform",
            Pattern::Bottom => "bottom",
        }
    }
}

/// Bid prices worst → best (ascending), 10 raw units apart, and query prices.
/// Half the queries are present prices, half fall between two levels (absent).
fn search_data(depth: usize, pattern: Pattern) -> (Vec<i64>, Vec<i64>) {
    let prices: Vec<i64> = (0..depth as i64).map(|i| 600_000 + 10 * i).collect();
    let top = depth as u64 - 1;
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let queries = (0..QUERIES)
        .map(|_| {
            let d = match pattern {
                Pattern::Top => 0,
                Pattern::TopHeavy => rng.geometric(50, top),
                Pattern::Uniform => rng.next() % depth as u64,
                Pattern::Bottom => top,
            };
            let present = prices[(top - d) as usize];
            if rng.next().is_multiple_of(2) {
                present
            } else {
                present - 5
            }
        })
        .collect();
    (prices, queries)
}

/// Benchmarks one strategy. Generic over `S`, so the search is inlined into the loop
/// exactly as it is inside the order book. (An earlier version called each search
/// through a function pointer, which blocks inlining and added ~5 ns per query.)
fn bench_one<S: Search>(
    group: &mut criterion::BenchmarkGroup<'_, criterion::measurement::WallTime>,
    name: &str,
    pattern: Pattern,
    prices: &[i64],
    queries: &[i64],
) {
    group.bench_function(BenchmarkId::new(name, pattern.name()), |b| {
        b.iter(|| {
            // Hide the data from the optimiser once per batch, not per query.
            let (prices, queries) = black_box((prices, queries));
            let mut acc = 0;
            for &q in queries {
                match S::find::<true>(prices, q) {
                    Ok(i) | Err(i) => acc += i,
                }
            }
            acc
        });
    });
}

fn bench_search(c: &mut Criterion) {
    for depth in [20, 50] {
        let mut group = c.benchmark_group(format!("search/depth{depth}"));
        group.throughput(Throughput::Elements(QUERIES as u64));
        for pattern in [
            Pattern::Top,
            Pattern::TopHeavy,
            Pattern::Uniform,
            Pattern::Bottom,
        ] {
            let (prices, queries) = search_data(depth, pattern);
            bench_one::<LinearFromBest>(&mut group, "linear_from_best", pattern, &prices, &queries);
            bench_one::<BranchlessBinary>(
                &mut group,
                "branchless_binary",
                pattern,
                &prices,
                &queries,
            );
            bench_one::<StdBinary>(&mut group, "std_binary", pattern, &prices, &queries);
            bench_one::<Hybrid>(&mut group, "hybrid", pattern, &prices, &queries);
        }
        group.finish();
    }
}

// ---------------------------------------------------------------------------
// 2. Apply (update + read best), against a BTreeMap book
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

/// Updates on both sides around a mid price; ~20% removals (quantity 0).
/// `spread_percent` controls depth: 40 = top-heavy, 0 = uniform over 40 levels.
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

fn run_book<S: Search>(book: &mut OrderBook<64, S>, stream: &[(Side, i64, i64)]) -> i64 {
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

fn bench_book<S: Search>(
    group: &mut criterion::BenchmarkGroup<'_, criterion::measurement::WallTime>,
    name: &str,
    workload: &str,
    stream: &[(Side, i64, i64)],
) {
    let mut book: OrderBook<64, S> = OrderBook::new();
    run_book(&mut book, stream); // warm up to a steady-state book
    group.bench_function(BenchmarkId::new(name, workload), |b| {
        b.iter(|| run_book(&mut book, black_box(stream)));
    });
}

fn bench_apply(c: &mut Criterion) {
    let mut group = c.benchmark_group("apply");
    group.throughput(Throughput::Elements(UPDATES as u64));
    for (workload, top_heavy) in [("top-heavy", true), ("uniform-40", false)] {
        let stream = update_stream(top_heavy);
        bench_book::<LinearFromBest>(&mut group, "array+linear", workload, &stream);
        bench_book::<BranchlessBinary>(&mut group, "array+branchless", workload, &stream);
        bench_book::<StdBinary>(&mut group, "array+std_binary", workload, &stream);
        bench_book::<Hybrid>(&mut group, "array+hybrid", workload, &stream);

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
// 3. Snapshot replace (Binance sends a 20-level snapshot every message)
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
    targets = bench_search, bench_apply, bench_snapshot
}
criterion_main!(benches);
