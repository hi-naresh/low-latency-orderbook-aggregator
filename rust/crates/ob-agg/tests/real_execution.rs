//! Execution planning and the arbitrage scan on the real recordings, with each venue's
//! real taker fee (Binance USDⓈ-M 0.05%, Bybit 0.055%) and real exchange timestamps.
#![allow(clippy::unwrap_used)]

use ob_agg::{Action, Aggregator, Config, VenueConfig, VenueId};
use ob_core::{InstrumentSpec, OrderBook};
use ob_parse::binance::{BinanceDepthParser, DepthUpdate};
use ob_parse::bybit::{BookMsg, BybitBookParser, Message};
use ob_sync::{BookMeta, BookPublisher, VenueStatus, book_channel};

const BINANCE: &str = include_str!("../../ob-parse/tests/fixtures/binance_depth20.jsonl");
const BYBIT: &str = include_str!("../../ob-parse/tests/fixtures/bybit_orderbook50.jsonl");

fn spec() -> InstrumentSpec {
    InstrumentSpec::from_steps("0.1", "0.001").unwrap()
}

/// Replays both recordings and returns an aggregator over the final books.
fn aggregator(config: Config) -> (Aggregator, VenueId, VenueId, Vec<BookPublisher>) {
    let (mut bn_pub, bn_r) = book_channel();
    let (mut bb_pub, bb_r) = book_channel();
    let mut agg = Aggregator::new(config);
    let bn = agg
        .add_venue(
            VenueConfig {
                name: "binance",
                depth: 20,
                taker_fee_ppm: 500,
            },
            bn_r,
        )
        .unwrap();
    let bb = agg
        .add_venue(
            VenueConfig {
                name: "bybit",
                depth: 50,
                taker_fee_ppm: 550,
            },
            bb_r,
        )
        .unwrap();

    let p = BinanceDepthParser::new(spec(), b"BTCUSDT");
    let (mut book, mut m) = (OrderBook::<64>::new(), DepthUpdate::default());
    for f in BINANCE.lines().skip(1) {
        p.parse(f.as_bytes(), &mut m).unwrap();
        m.apply_to(&mut book).unwrap();
        bn_pub.publish(
            &book,
            BookMeta {
                update_id: m.final_update_id,
                exchange_ts: m.event_time,
                recv_ns: 0,
                status: VenueStatus::Live,
            },
        );
    }
    let p = BybitBookParser::new(spec(), b"orderbook.50.BTCUSDT", b"BTCUSDT");
    let (mut book, mut m) = (OrderBook::<64>::new(), BookMsg::default());
    for f in BYBIT.lines() {
        if p.parse(f.as_bytes(), &mut m).unwrap() == Message::Book {
            m.apply_to(&mut book).unwrap();
            bb_pub.publish(
                &book,
                BookMeta {
                    update_id: m.update_id,
                    exchange_ts: m.ts,
                    recv_ns: 0,
                    status: VenueStatus::Live,
                },
            );
        }
    }
    agg.refresh();
    (agg, bn, bb, vec![bn_pub, bb_pub])
}

fn show(agg: &Aggregator, label: &str, plan: &ob_agg::ExecutionPlan) {
    let s = spec();
    let children: Vec<String> = plan
        .children()
        .iter()
        .map(|c| {
            format!(
                "{} {} up to {} ({} levels)",
                agg.venue(c.venue).unwrap().name,
                s.fmt_qty(c.qty),
                s.fmt_px(c.limit_px),
                c.levels
            )
        })
        .collect();
    eprintln!(
        "{label}: filled {} of {}; vwap {}, with fees {}; fees {} USDT; [{}]{}",
        s.fmt_qty(plan.filled),
        s.fmt_qty(plan.requested),
        plan.vwap().map_or("-".into(), |p| s.fmt_px(p).to_string()),
        plan.vwap_including_fees()
            .map_or("-".into(), |p| s.fmt_px(p).to_string()),
        s.fmt_notional(plan.fees),
        children.join("; "),
        if plan.partial {
            format!(" PARTIAL (more may exist: {})", plan.may_have_more_depth)
        } else {
            String::new()
        },
    );
}

#[test]
fn routing_a_buy_across_the_real_books() {
    let (agg, _bn, _bb, _p) = aggregator(Config {
        max_age_ns: u64::MAX,
        ..Config::default()
    });
    let s = spec();
    let best_ask = agg.best_asks(0)[0].px;
    for btc in ["1", "10"] {
        let q = s.parse_qty(btc).unwrap();
        let plan = agg.optimal_execution(Action::Buy, q, 0);
        show(&agg, &format!("buy {btc} BTC"), &plan);
        if !plan.partial {
            assert_eq!(plan.filled, q);
        }
        assert!(plan.vwap().unwrap() >= best_ask);
        assert!(plan.vwap_including_fees().unwrap() > plan.vwap().unwrap());
        assert!(plan.fees.raw() > 0);
        // The fee-aware plan's all-in cost is never worse than routing by price alone.
        let by_price = agg.ask_price_for_volume(q, 0);
        let all_in = |p: &ob_agg::ExecutionPlan| {
            p.gross.raw() * 1_000_000
                + p.children()
                    .iter()
                    .map(|c| c.gross.raw() * i128::from(agg.venue(c.venue).unwrap().taker_fee_ppm))
                    .sum::<i128>()
        };
        assert!(all_in(&plan) <= all_in(&by_price));
    }
}

#[test]
fn the_recorded_cross_is_neither_profitable_nor_simultaneous() {
    // Layer 1: real taker fees. Bybit's bid is $8.70 above Binance's ask, but buying on
    // Binance (0.05%) and selling on Bybit (0.055%) costs ~$61.6 per BTC in fees: no
    // opportunity at all, aligned or not.
    let (real, ..) = aggregator(Config {
        max_age_ns: u64::MAX,
        max_skew_ms: u64::MAX,
    });
    assert_eq!(real.scan_arbitrage(0), ob_agg::ArbScan::default());

    // Layer 2: pretend trading were free. Now the cross is "profitable", but the books
    // were recorded ~15 s apart, so the timing guard refuses it.
    let free = |max_skew_ms| {
        let (mut agg, bn, bb, p) = aggregator(Config {
            max_age_ns: u64::MAX,
            max_skew_ms,
        });
        agg.set_taker_fee(bn, 0);
        agg.set_taker_fee(bb, 0);
        (agg, bn, bb, p)
    };
    let (agg, bn, bb, _p) = free(Config::default().max_skew_ms);
    let scan = agg.scan_arbitrage(0);
    assert_eq!(
        scan.best, None,
        "must not act on books from different moments"
    );
    let fake = scan.rejected_for_skew.unwrap();
    assert_eq!((fake.sell_venue, fake.buy_venue), (bb, bn));
    assert!(fake.skew_ms > 5_000);
    let s = spec();
    eprintln!(
        "rejected (fees off): sell {} on bybit / buy {} on binance, {} BTC, {} USDT, but books {} ms apart",
        s.fmt_px(fake.sell_px),
        s.fmt_px(fake.buy_px),
        s.fmt_qty(fake.qty),
        s.fmt_notional(fake.net_profit),
        fake.skew_ms
    );

    // Layer 3: switch the guard off too, and the same books *are* reported: exactly the
    // mistake both layers exist to prevent.
    let (unguarded, ..) = free(u64::MAX);
    assert_eq!(unguarded.scan_arbitrage(0).best, Some(fake));
}
