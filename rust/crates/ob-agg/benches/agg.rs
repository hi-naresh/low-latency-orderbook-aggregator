//! Aggregator benchmarks on the real recordings (Binance 20+20, Bybit 50+50), plus the
//! quantity-sum strategies deferred from step 2.
//!
//! Run: `cargo bench -p ob-agg --bench agg`
#![allow(clippy::unwrap_used, clippy::cast_possible_wrap, missing_docs)]

use std::hint::black_box;
use std::time::Duration;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use ob_agg::{Action, Aggregator, Config, VenueConfig, sum_qty};
use ob_core::{InstrumentSpec, OrderBook, Px, Qty};
use ob_parse::binance::{BinanceDepthParser, DepthUpdate};
use ob_parse::bybit::{BookMsg, BybitBookParser, Message};
use ob_sync::{BookMeta, BookPublisher, VenueStatus, book_channel};

const BINANCE: &str = include_str!("../../ob-parse/tests/fixtures/binance_depth20.jsonl");
const BYBIT: &str = include_str!("../../ob-parse/tests/fixtures/bybit_orderbook50.jsonl");

/// The final book of each recording.
fn books() -> (OrderBook, OrderBook) {
    let spec = InstrumentSpec::from_steps("0.1", "0.001").unwrap();
    let p = BinanceDepthParser::new(spec, b"BTCUSDT");
    let (mut bn, mut m) = (OrderBook::new(), DepthUpdate::default());
    for f in BINANCE.lines().skip(1) {
        p.parse(f.as_bytes(), &mut m).unwrap();
        m.apply_to(&mut bn).unwrap();
    }
    let p = BybitBookParser::new(spec, b"orderbook.50.BTCUSDT", b"BTCUSDT");
    let (mut bb, mut m) = (OrderBook::new(), BookMsg::default());
    for f in BYBIT.lines() {
        if p.parse(f.as_bytes(), &mut m).unwrap() == Message::Book {
            m.apply_to(&mut bb).unwrap();
        }
    }
    (bn, bb)
}

fn live(k: u64) -> BookMeta {
    BookMeta {
        update_id: k,
        exchange_ts: k,
        recv_ns: 0,
        status: VenueStatus::Live,
    }
}

fn setup() -> (
    Aggregator,
    BookPublisher,
    BookPublisher,
    OrderBook,
    OrderBook,
) {
    let (bn, bb) = books();
    let (mut p1, r1) = book_channel();
    let (mut p2, r2) = book_channel();
    p1.publish(&bn, live(1));
    p2.publish(&bb, live(1));
    let mut agg = Aggregator::new(Config {
        max_age_ns: u64::MAX,
        ..Config::default()
    });
    agg.add_venue(
        VenueConfig {
            name: "binance",
            depth: 20,
            taker_fee_ppm: 0,
        },
        r1,
    )
    .unwrap();
    agg.add_venue(
        VenueConfig {
            name: "bybit",
            depth: 50,
            taker_fee_ppm: 0,
        },
        r2,
    )
    .unwrap();
    agg.refresh();
    (agg, p1, p2, bn, bb)
}

fn aggregator(c: &mut Criterion) {
    let (mut agg, mut p1, mut p2, bn, bb) = setup();
    let mut g = c.benchmark_group("aggregator");
    g.bench_function("refresh/nothing_changed", |b| {
        b.iter(|| black_box(&mut agg).refresh())
    });
    let mut k = 1;
    g.bench_function("refresh/both_changed (publish+copy 20x20 & 50x50)", |b| {
        b.iter(|| {
            k += 1;
            p1.publish(&bn, live(k));
            p2.publish(&bb, live(k));
            black_box(&mut agg).refresh()
        });
    });
    g.bench_function("best_bids+best_asks", |b| {
        b.iter(|| (black_box(&agg).best_bids(0), agg.best_asks(0)))
    });
    let limit = agg.best_bids(0)[0].px;
    let deep = Px::from_raw(limit.raw() - 100);
    g.bench_function("bid_volume (top level only)", |b| {
        b.iter(|| black_box(&agg).bid_volume_at_or_above(limit, 0))
    });
    g.bench_function("bid_volume ($10 deep)", |b| {
        b.iter(|| black_box(&agg).bid_volume_at_or_above(deep, 0))
    });
    g.finish();
}

/// Execution planning and the arbitrage scan on the real books (fees: 500 / 550 ppm).
fn execution(c: &mut Criterion) {
    let (mut agg, ..) = setup();
    let ids: Vec<_> = agg.venue_ids().collect();
    agg.set_taker_fee(ids[0], 500);
    agg.set_taker_fee(ids[1], 550);
    let spec = InstrumentSpec::from_steps("0.1", "0.001").unwrap();
    let mut g = c.benchmark_group("execution");
    for btc in ["1", "10", "60"] {
        let q = spec.parse_qty(btc).unwrap();
        g.bench_function(format!("optimal_execution buy {btc} BTC"), |b| {
            b.iter(|| black_box(&agg).optimal_execution(Action::Buy, q, 0));
        });
    }
    let one = spec.parse_qty("1").unwrap();
    g.bench_function("ask_price_for_volume 1 BTC (no fees)", |b| {
        b.iter(|| black_box(&agg).ask_price_for_volume(one, 0))
    });
    g.bench_function("scan_arbitrage (2 venues)", |b| {
        b.iter(|| black_box(&agg).scan_arbitrage(0))
    });
    g.finish();
}

/// The step 2 question: checked `+` on every element vs `i128` accumulation checked once
/// vs a branch-free `u64` overflow flag vs short-circuiting checked fold (what `sum_qty`
/// now uses), against unchecked wrapping (incorrect on overflow; the speed ceiling).
fn sums(c: &mut Criterion) {
    let mut g = c.benchmark_group("qty_sum");
    for n in [50_usize, 1_000_000] {
        let qtys: Vec<Qty> = (0..n)
            .map(|i| Qty::from_raw((i % 5000) as i64).unwrap())
            .collect();
        g.throughput(Throughput::Elements(n as u64));
        g.bench_with_input(BenchmarkId::new("checked_add_each", n), &qtys, |b, q| {
            b.iter(|| black_box(q).iter().fold(Qty::ZERO, |a, &x| a + x));
        });
        g.bench_with_input(BenchmarkId::new("i128_check_once", n), &qtys, |b, q| {
            b.iter(|| {
                let t: i128 = black_box(q).iter().map(|x| i128::from(x.raw())).sum();
                i64::try_from(t).ok().and_then(Qty::from_raw)
            });
        });
        g.bench_with_input(
            BenchmarkId::new("checked_fold_short_circuit (sum_qty)", n),
            &qtys,
            |b, q| b.iter(|| sum_qty(black_box(q).iter().copied())),
        );
        g.bench_with_input(BenchmarkId::new("u64_overflow_flag", n), &qtys, |b, q| {
            b.iter(|| {
                let (mut acc, mut overflow) = (0_u64, false);
                for x in black_box(q) {
                    let (s, o) = acc.overflowing_add(x.raw().cast_unsigned());
                    acc = s;
                    overflow |= o;
                }
                if overflow {
                    None
                } else {
                    Qty::from_raw(acc.cast_signed())
                }
            });
        });
        g.bench_with_input(
            BenchmarkId::new("wrapping_i64 (no check)", n),
            &qtys,
            |b, q| {
                b.iter(|| {
                    black_box(q)
                        .iter()
                        .fold(0_i64, |a, x| a.wrapping_add(x.raw()))
                });
            },
        );
    }
    g.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .warm_up_time(Duration::from_millis(500))
        .measurement_time(Duration::from_secs(3))
        .sample_size(50);
    targets = aggregator, execution, sums
}
criterion_main!(benches);
