//! Execution planning: routing across venues, fees, partial fills, rounding, and arbitrage.
#![allow(clippy::unwrap_used)]

use ob_agg::{Action, Aggregator, Config, VenueConfig, VenueId};
use ob_core::{Level, Notional, OrderBook, Px, Qty};
use ob_sync::{BookMeta, BookPublisher, VenueStatus, book_channel};

fn lvl(px: i64, qty: i64) -> Level {
    Level::new(Px::from_raw(px), Qty::from_raw(qty).unwrap())
}
fn qty(q: i64) -> Qty {
    Qty::from_raw(q).unwrap()
}

struct Venue {
    bids: Vec<Level>,
    asks: Vec<Level>,
    fee_ppm: u32,
    depth: usize,
    status: VenueStatus,
    exchange_ts: u64,
}

impl Venue {
    fn new(bids: Vec<Level>, asks: Vec<Level>, fee_ppm: u32) -> Self {
        Venue {
            bids,
            asks,
            fee_ppm,
            depth: 50,
            status: VenueStatus::Live,
            exchange_ts: 1_000,
        }
    }
}

fn build(venues: &[Venue]) -> (Aggregator, Vec<VenueId>, Vec<BookPublisher>) {
    let mut agg = Aggregator::new(Config {
        max_age_ns: u64::MAX,
        ..Config::default()
    });
    let (mut ids, mut pubs) = (Vec::new(), Vec::new());
    for v in venues {
        let (mut p, r) = book_channel();
        let mut b: OrderBook = OrderBook::new();
        b.replace(&v.bids, &v.asks).unwrap();
        p.publish(
            &b,
            BookMeta {
                update_id: 1,
                exchange_ts: v.exchange_ts,
                recv_ns: 0,
                status: v.status,
            },
        );
        ids.push(
            agg.add_venue(
                VenueConfig {
                    name: "v",
                    depth: v.depth,
                    taker_fee_ppm: v.fee_ppm,
                },
                r,
            )
            .unwrap(),
        );
        pubs.push(p);
    }
    agg.refresh();
    (agg, ids, pubs)
}

/// (venue index, qty, limit px, levels) for each child order.
fn children(plan: &ob_agg::ExecutionPlan) -> Vec<(usize, i64, i64, usize)> {
    plan.children()
        .iter()
        .map(|c| (c.venue.index(), c.qty.raw(), c.limit_px.raw(), c.levels))
        .collect()
}

// ---------------------------------------------------------------------------
// Price for volume (no fees)
// ---------------------------------------------------------------------------

#[test]
fn buying_takes_the_cheapest_levels_across_venues() {
    let (agg, _, _) = build(&[
        Venue::new(vec![], vec![lvl(100, 2), lvl(103, 10)], 0),
        Venue::new(vec![], vec![lvl(101, 3), lvl(102, 4)], 0),
    ]);
    // 2 @100 (v0) + 3 @101 (v1) + 4 @102 (v1) + 1 @103 (v0) = 10
    let plan = agg.ask_price_for_volume(qty(10), 0);
    assert_eq!(children(&plan), [(0, 3, 103, 2), (1, 7, 102, 2)]);
    assert_eq!((plan.filled, plan.partial), (qty(10), false));
    assert_eq!(plan.worst_px(), Some(Px::from_raw(103)));
    // gross = 200 + 303 + 408 + 103 = 1014 → VWAP 101.4, rounded up for a buy → 102.
    assert_eq!(plan.gross, Notional::from_raw(1014));
    assert_eq!(plan.vwap(), Some(Px::from_raw(102)));
}

#[test]
fn selling_takes_the_highest_bids_and_rounds_down() {
    let (agg, _, _) = build(&[
        Venue::new(vec![lvl(100, 1), lvl(97, 5)], vec![], 0),
        Venue::new(vec![lvl(99, 2), lvl(98, 2)], vec![], 0),
    ]);
    let plan = agg.bid_price_for_volume(qty(4), 0);
    // 1 @100 (v0) + 2 @99 (v1) + 1 @98 (v1): gross 396 → VWAP 99.0
    assert_eq!(children(&plan), [(0, 1, 100, 1), (1, 3, 98, 2)]);
    assert_eq!(plan.worst_px(), Some(Px::from_raw(98)));
    assert_eq!(plan.vwap(), Some(Px::from_raw(99)));
    // 5 units: gross 495 → 99.0; 6 units adds 97 → 592/6 = 98.67 → rounded down to 98.
    assert_eq!(
        agg.bid_price_for_volume(qty(6), 0).vwap(),
        Some(Px::from_raw(98))
    );
}

#[test]
fn a_level_can_be_partly_used() {
    let (agg, _, _) = build(&[Venue::new(vec![], vec![lvl(100, 10)], 0)]);
    let plan = agg.ask_price_for_volume(qty(3), 0);
    assert_eq!(children(&plan), [(0, 3, 100, 1)]);
}

#[test]
fn not_enough_liquidity_is_a_partial_plan() {
    let (agg, _, _) = build(&[
        Venue::new(vec![], vec![lvl(100, 1)], 0),
        Venue::new(vec![], vec![lvl(101, 2)], 0),
    ]);
    let plan = agg.ask_price_for_volume(qty(5), 0);
    assert_eq!(
        (plan.filled, plan.partial, plan.requested),
        (qty(3), true, qty(5))
    );
    // Both books are shorter than their published depth: what we see is all there is.
    assert!(!plan.may_have_more_depth);
}

#[test]
fn partial_plans_flag_venues_that_may_hold_more_than_they_show() {
    let mut v = Venue::new(vec![], vec![lvl(100, 1), lvl(101, 1)], 0);
    v.depth = 2; // the venue only ever shows 2 levels, and we've used both
    let (agg, _, _) = build(&[v]);
    let plan = agg.ask_price_for_volume(qty(5), 0);
    assert!(plan.partial && plan.may_have_more_depth);
}

#[test]
fn empty_cases() {
    let (agg, _, _) = build(&[Venue::new(vec![lvl(99, 1)], vec![lvl(100, 1)], 0)]);
    let plan = agg.ask_price_for_volume(Qty::ZERO, 0);
    assert!(plan.children().is_empty() && !plan.partial);
    assert_eq!((plan.vwap(), plan.worst_px()), (None, None));
    let (none, _, _) = build(&[]);
    let plan = none.optimal_execution(Action::Buy, qty(1), 0);
    assert!(plan.partial && plan.filled == Qty::ZERO);
}

#[test]
fn unhealthy_venues_are_not_routed_to() {
    let mut down = Venue::new(vec![], vec![lvl(90, 100)], 0);
    down.status = VenueStatus::Syncing;
    let (agg, _, _) = build(&[down, Venue::new(vec![], vec![lvl(100, 5)], 0)]);
    assert_eq!(
        children(&agg.ask_price_for_volume(qty(2), 0)),
        [(1, 2, 100, 1)]
    );
}

#[test]
fn equal_prices_go_to_the_lower_venue_first() {
    let (agg, _, _) = build(&[
        Venue::new(vec![], vec![lvl(100, 1)], 0),
        Venue::new(vec![], vec![lvl(100, 1)], 0),
    ]);
    assert_eq!(
        children(&agg.ask_price_for_volume(qty(1), 0)),
        [(0, 1, 100, 1)]
    );
}

// ---------------------------------------------------------------------------
// Fees
// ---------------------------------------------------------------------------

#[test]
fn fees_can_change_where_to_trade() {
    // Venue 0 is cheaper on price (100.00) but charges 0.06%; venue 1 asks 100.05 with no
    // fee. Effective: 100.06 vs 100.05, so the fee-aware plan buys from venue 1.
    let (agg, _, _) = build(&[
        Venue::new(vec![], vec![lvl(10_000, 1)], 600),
        Venue::new(vec![], vec![lvl(10_005, 1)], 0),
    ]);
    assert_eq!(
        children(&agg.ask_price_for_volume(qty(1), 0)),
        [(0, 1, 10_000, 1)]
    );
    let plan = agg.optimal_execution(Action::Buy, qty(1), 0);
    assert_eq!(children(&plan), [(1, 1, 10_005, 1)]);
    assert_eq!(plan.fees, Notional::from_raw(0));
}

#[test]
fn fees_are_exact_and_rounded_against_us() {
    // Buy 3 @ 1000 on a 550 ppm (0.055%) venue: gross 3000, fee 1.65 → rounded up to 2.
    let (agg, _, _) = build(&[Venue::new(vec![lvl(990, 3)], vec![lvl(1000, 3)], 550)]);
    let buy = agg.optimal_execution(Action::Buy, qty(3), 0);
    assert_eq!(
        (buy.gross, buy.fees),
        (Notional::from_raw(3000), Notional::from_raw(2))
    );
    assert_eq!(buy.children()[0].fee, Notional::from_raw(2));
    // Average including fees: 3002 / 3 = 1000.67 → rounded up to 1001.
    assert_eq!(buy.vwap_including_fees(), Some(Px::from_raw(1001)));
    // Sell 3 @ 990: gross 2970, fee 1.6335 → 2; proceeds 2968 / 3 = 989.33 → rounded down.
    let sell = agg.optimal_execution(Action::Sell, qty(3), 0);
    assert_eq!(sell.fees, Notional::from_raw(2));
    assert_eq!(sell.vwap_including_fees(), Some(Px::from_raw(989)));
}

// ---------------------------------------------------------------------------
// Arbitrage
// ---------------------------------------------------------------------------

#[test]
fn a_cross_between_venues_is_measured_level_by_level() {
    // Sell venue 0's bids (101 ×2, 100.5 ×1), buy venue 1's asks (100 ×1, 100.2 ×3).
    // Prices ×10 to stay integral: 1010, 1005 / 1000, 1002.
    let (agg, ids, _) = build(&[
        Venue::new(vec![lvl(1010, 2), lvl(1005, 1)], vec![lvl(1020, 1)], 0),
        Venue::new(vec![lvl(990, 1)], vec![lvl(1000, 1), lvl(1002, 3)], 0),
    ]);
    let arb = agg.scan_arbitrage(0).best.unwrap();
    assert_eq!((arb.sell_venue, arb.buy_venue), (ids[0], ids[1]));
    // (1010−1000)×1 + (1010−1002)×1 + (1005−1002)×1 = 21 over 3 units, then bids run out.
    assert_eq!(
        (arb.qty, arb.gross_profit, arb.net_profit),
        (qty(3), Notional::from_raw(21), Notional::from_raw(21))
    );
    assert_eq!(
        (arb.sell_px, arb.buy_px),
        (Px::from_raw(1010), Px::from_raw(1000))
    );
    assert_eq!(
        (arb.sell_limit_px, arb.buy_limit_px),
        (Px::from_raw(1005), Px::from_raw(1002))
    );
}

#[test]
fn fees_shrink_or_remove_the_opportunity() {
    let books = |fee| {
        build(&[
            Venue::new(vec![lvl(10_010, 5)], vec![lvl(10_020, 1)], fee),
            Venue::new(vec![lvl(9_990, 1)], vec![lvl(10_000, 5)], fee),
        ])
    };
    // A 0.1% gross edge (10,010 vs 10,000). With 0.05% fees on each side, the edge is
    // 10,010×(1−0.0005) − 10,000×(1+0.0005) = −0.005 per unit: gone.
    assert!(books(0).0.scan_arbitrage(0).best.is_some());
    assert!(books(400).0.scan_arbitrage(0).best.is_some()); // 0.04% each: still +2.0 per unit
    assert!(books(500).0.scan_arbitrage(0).best.is_none());
    let arb = books(400).0.scan_arbitrage(0).best.unwrap();
    // net per unit ×10^6 = 10,010×999,600 − 10,000×1,000,400 = 1,996,000 → 1.996; ×5 = 9.98 → 9 (rounded down)
    assert_eq!(
        (arb.qty, arb.gross_profit, arb.net_profit),
        (qty(5), Notional::from_raw(50), Notional::from_raw(9))
    );
}

#[test]
fn books_too_far_apart_in_time_are_rejected_not_reported() {
    let mut late = Venue::new(vec![lvl(990, 1)], vec![lvl(1000, 5)], 0);
    late.exchange_ts = 1_000 + 151; // just past the 150 ms default
    let (agg, _, _) = build(&[Venue::new(vec![lvl(1010, 5)], vec![lvl(1020, 1)], 0), late]);
    let scan = agg.scan_arbitrage(0);
    assert_eq!(scan.best, None);
    assert_eq!(scan.rejected_for_skew.unwrap().skew_ms, 151);
}

#[test]
fn no_cross_no_signal_and_never_within_one_venue() {
    let (agg, _, _) = build(&[
        Venue::new(vec![lvl(999, 5)], vec![lvl(1000, 5)], 0),
        Venue::new(vec![lvl(998, 5)], vec![lvl(1001, 5)], 0),
    ]);
    assert_eq!(agg.scan_arbitrage(0), ob_agg::ArbScan::default());
}
