//! Is the fee-aware greedy plan really the cheapest? Checked two ways:
//!
//! 1. **Brute force** on small random books: enumerate *every* way of splitting the order
//!    across every level of every venue, and check none is cheaper (after fees) than the
//!    plan. This tests the optimality argument itself, not just the code.
//! 2. **Reference implementation** on larger random books: flatten all levels, sort by
//!    effective price, take greedily; the planner must produce exactly the same per-venue
//!    child orders.
#![allow(clippy::unwrap_used)]

use ob_agg::{Action, Aggregator, Config, ExecutionPlan, PPM, VenueConfig};
use ob_core::{Level, OrderBook, Px, Qty};
use ob_sync::{BookMeta, VenueStatus, book_channel};
use proptest::prelude::*;

#[derive(Clone, Debug)]
struct VenueCase {
    fee_ppm: u32,
    levels: Vec<(i64, i64)>, // asks, best first (for sells we mirror them into bids)
}

fn build(venues: &[VenueCase], action: Action) -> Aggregator {
    let mut agg = Aggregator::new(Config {
        max_age_ns: u64::MAX,
        ..Config::default()
    });
    for v in venues {
        let (mut p, r) = book_channel(); // dropping `p` later is fine: the reader keeps the book alive
        let levels: Vec<Level> = v
            .levels
            .iter()
            .map(|&(px, q)| Level::new(Px::from_raw(px), Qty::from_raw(q).unwrap()))
            .collect();
        let mut b: OrderBook = OrderBook::new();
        match action {
            Action::Buy => b.replace(&[], &levels).unwrap(),
            Action::Sell => {
                let mut bids = levels;
                bids.reverse(); // ascending prices reversed = best (highest) first
                b.replace(&bids, &[]).unwrap();
            }
        }
        p.publish(
            &b,
            BookMeta {
                update_id: 1,
                exchange_ts: 0,
                recv_ns: 0,
                status: VenueStatus::Live,
            },
        );
        agg.add_venue(
            VenueConfig {
                name: "v",
                depth: 64,
                taker_fee_ppm: v.fee_ppm,
            },
            r,
        )
        .unwrap();
    }
    agg.refresh();
    agg
}

/// Total cost of a plan × 10^6, exactly: Σ gross × (10^6 ± fee). For a sell this is the
/// negated proceeds, so "smaller is better" in both cases.
fn plan_cost(plan: &ExecutionPlan, venues: &[VenueCase], action: Action) -> i128 {
    plan.children()
        .iter()
        .map(|c| {
            let fee = i128::from(venues[c.venue.index()].fee_ppm);
            match action {
                Action::Buy => c.gross.raw() * (PPM + fee),
                Action::Sell => -(c.gross.raw() * (PPM - fee)),
            }
        })
        .sum()
}

/// Cost of one allocation (x_l units from each level), same convention.
fn allocation_cost(alloc: &[(i64, i64, i128)], action: Action) -> i128 {
    alloc
        .iter()
        .map(|&(px, x, fee)| {
            let g = i128::from(px) * i128::from(x);
            match action {
                Action::Buy => g * (PPM + fee),
                Action::Sell => -(g * (PPM - fee)),
            }
        })
        .sum()
}

/// The cheapest cost over every allocation summing to `target` (None if impossible).
fn brute_force(levels: &[(i64, i64, i128)], target: i64, action: Action) -> Option<i128> {
    fn go(
        levels: &[(i64, i64, i128)],
        i: usize,
        left: i64,
        chosen: &mut Vec<(i64, i64, i128)>,
        best: &mut Option<i128>,
        action: Action,
    ) {
        if i == levels.len() {
            if left == 0 {
                let c = allocation_cost(chosen, action);
                if best.is_none_or(|b| c < b) {
                    *best = Some(c);
                }
            }
            return;
        }
        let (px, q, fee) = levels[i];
        for x in 0..=q.min(left) {
            chosen.push((px, x, fee));
            go(levels, i + 1, left - x, chosen, best, action);
            chosen.pop();
        }
    }
    let mut best = None;
    go(levels, 0, target, &mut Vec::new(), &mut best, action);
    best
}

fn small_venue() -> impl Strategy<Value = VenueCase> {
    // Prices close together (so fees matter), a few levels with small quantities.
    (
        prop::sample::select(vec![0_u32, 200, 500, 550, 1000, 5000]),
        prop::collection::btree_map(1_000_i64..1_012, 1_i64..4, 1..3),
    )
        .prop_map(|(fee_ppm, m)| VenueCase {
            fee_ppm,
            levels: m.into_iter().collect(),
        })
}

fn venue() -> impl Strategy<Value = VenueCase> {
    (
        0_u32..2_000,
        prop::collection::btree_map(9_000_i64..11_000, 1_i64..1_000, 0..40),
    )
        .prop_map(|(fee_ppm, m)| VenueCase {
            fee_ppm,
            levels: m.into_iter().collect(),
        })
}

fn action() -> impl Strategy<Value = Action> {
    prop_oneof![Just(Action::Buy), Just(Action::Sell)]
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 3_000, ..ProptestConfig::default() })]

    #[test]
    fn no_split_of_the_order_is_cheaper_than_the_plan(
        venues in prop::collection::vec(small_venue(), 1..4),
        action in action(),
        pick in any::<prop::sample::Index>(),
    ) {
        let flat: Vec<(i64, i64, i128)> = venues.iter()
            .flat_map(|v| v.levels.iter().map(move |&(p, q)| (p, q, i128::from(v.fee_ppm))))
            .collect();
        let total: i64 = flat.iter().map(|l| l.1).sum();
        let target = i64::try_from(pick.index(usize::try_from(total).unwrap() + 1)).unwrap(); // 0..=total
        let agg = build(&venues, action);
        let plan = agg.optimal_execution(action, Qty::from_raw(target).unwrap(), 0);
        prop_assert_eq!(plan.filled.raw(), target);
        let best = brute_force(&flat, target, action).unwrap();
        prop_assert_eq!(plan_cost(&plan, &venues, action), best, "greedy is not optimal");
    }

    #[test]
    fn planner_matches_a_sort_based_reference(
        venues in prop::collection::vec(venue(), 0..5),
        action in action(),
        target in 0_i64..60_000,
        with_fees in any::<bool>(),
    ) {
        let agg = build(&venues, action);
        let q = Qty::from_raw(target).unwrap();
        let plan = match (with_fees, action) {
            (true, _) => agg.optimal_execution(action, q, 0),
            (false, Action::Buy) => agg.ask_price_for_volume(q, 0),
            (false, Action::Sell) => agg.bid_price_for_volume(q, 0),
        };
        // Reference: every level of every venue, sorted by (effective price, venue, level).
        let mut all: Vec<(i128, usize, usize, i64, i64)> = Vec::new();
        for (vi, v) in venues.iter().enumerate() {
            let fee = if with_fees { i128::from(v.fee_ppm) } else { 0 };
            // Levels in the order the venue's book holds them (best first).
            let ordered: Vec<(i64, i64)> = match action {
                Action::Buy => v.levels.clone(),
                Action::Sell => v.levels.iter().rev().copied().collect(),
            };
            for (li, &(px, qty)) in ordered.iter().enumerate() {
                let key = match action {
                    Action::Buy => i128::from(px) * (PPM + fee),
                    Action::Sell => -(i128::from(px) * (PPM - fee)),
                };
                all.push((key, vi, li, px, qty));
            }
        }
        all.sort();
        let mut per = vec![(0_i64, 0_i64, 0_usize); venues.len()]; // qty, limit, levels
        let mut left = target;
        for &(_, vi, _, px, qty) in &all {
            if left == 0 { break; }
            let take = left.min(qty);
            left -= take;
            per[vi] = (per[vi].0 + take, px, per[vi].2 + 1);
        }
        let want: Vec<(usize, i64, i64, usize)> = per.iter().enumerate()
            .filter(|(_, p)| p.0 > 0).map(|(i, p)| (i, p.0, p.1, p.2)).collect();
        let got: Vec<(usize, i64, i64, usize)> = plan.children().iter()
            .map(|c| (c.venue.index(), c.qty.raw(), c.limit_px.raw(), c.levels)).collect();
        prop_assert_eq!(got, want);
        prop_assert_eq!(plan.partial, left > 0);
    }
}
