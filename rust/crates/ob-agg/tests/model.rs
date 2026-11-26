//! Property test: for random venues (books, health, ages) and random queries, the
//! aggregator must agree with a naive reference that recomputes everything from scratch
//! with plain loops and a full sort.
#![allow(clippy::unwrap_used)]

use ob_agg::{Aggregator, Config, VenueConfig, VenueId};
use ob_core::{Level, OrderBook, Px, Qty};
use ob_sync::{BookMeta, VenueStatus, book_channel};
use proptest::prelude::*;

const MAX_AGE: u64 = 1_000;

#[derive(Clone, Debug)]
struct VenueCase {
    status: VenueStatus,
    recv_ns: u64,
    depth: usize,
    bids: Vec<(i64, i64)>, // best first
    asks: Vec<(i64, i64)>,
}

fn venue() -> impl Strategy<Value = VenueCase> {
    let status = prop_oneof![6 => Just(VenueStatus::Live), 1 => Just(VenueStatus::Syncing), 1 => Just(VenueStatus::Down)];
    // Prices from a narrow band so venues tie and books sometimes cross.
    let side = prop::collection::btree_map(90_i64..110, 1_i64..5, 0..8);
    (status, 0_u64..3_000, 1_usize..8, side.clone(), side).prop_map(
        |(status, recv_ns, depth, b, a)| VenueCase {
            status,
            recv_ns,
            depth,
            bids: b.into_iter().rev().collect(),
            asks: a.into_iter().collect(),
        },
    )
}

fn levels(v: &[(i64, i64)]) -> Vec<Level> {
    v.iter()
        .map(|&(p, q)| Level::new(Px::from_raw(p), Qty::from_raw(q).unwrap()))
        .collect()
}

/// The reference: is the venue used at `now`?
fn included(v: &VenueCase, now: u64) -> bool {
    let crossed = matches!((v.bids.first(), v.asks.first()), (Some(b), Some(a)) if b.0 >= a.0);
    v.status == VenueStatus::Live && now.saturating_sub(v.recv_ns) <= MAX_AGE && !crossed
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 5_000, ..ProptestConfig::default() })]

    #[test]
    fn aggregator_matches_a_naive_reference(
        venues in prop::collection::vec(venue(), 0..5),
        now in 0_u64..3_000,
        limit in 85_i64..115,
    ) {
        let mut agg = Aggregator::new(Config { max_age_ns: MAX_AGE, ..Config::default() });
        let mut ids: Vec<VenueId> = Vec::new();
        let mut publishers = Vec::new();
        for v in &venues {
            let (mut p, r) = book_channel();
            let mut book: OrderBook = OrderBook::new();
            book.replace(&levels(&v.bids), &levels(&v.asks)).unwrap();
            p.publish(&book, BookMeta { update_id: 1, exchange_ts: 1, recv_ns: v.recv_ns, status: v.status });
            ids.push(agg.add_venue(VenueConfig { name: "v", depth: v.depth, taker_fee_ppm: 0 }, r).unwrap());
            publishers.push(p);
        }
        agg.refresh();

        // Best bids: included venues' best levels, sorted by (price desc, qty desc, venue asc).
        let mut want: Vec<(i64, i64, usize)> = venues.iter().enumerate()
            .filter(|(_, v)| included(v, now))
            .filter_map(|(i, v)| v.bids.first().map(|&(p, q)| (p, q, i)))
            .collect();
        want.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)).then(a.2.cmp(&b.2)));
        let got: Vec<_> = agg.best_bids(now).iter().map(|q| (q.px.raw(), q.qty.raw(), q.venue.index())).collect();
        prop_assert_eq!(got, want);

        // Best asks: (price asc, qty desc, venue asc).
        let mut want: Vec<(i64, i64, usize)> = venues.iter().enumerate()
            .filter(|(_, v)| included(v, now))
            .filter_map(|(i, v)| v.asks.first().map(|&(p, q)| (p, q, i)))
            .collect();
        want.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)).then(a.2.cmp(&b.2)));
        let got: Vec<_> = agg.best_asks(now).iter().map(|q| (q.px.raw(), q.qty.raw(), q.venue.index())).collect();
        prop_assert_eq!(got, want);

        // Volume at or above / at or below the limit, with completeness.
        for bid_side in [true, false] {
            let vol = if bid_side {
                agg.bid_volume_at_or_above(Px::from_raw(limit), now)
            } else {
                agg.ask_volume_at_or_below(Px::from_raw(limit), now)
            };
            let mut want = Vec::new();
            for (i, v) in venues.iter().enumerate().filter(|(_, v)| included(v, now)) {
                let side = if bid_side { &v.bids } else { &v.asks };
                let within: Vec<_> = side.iter().filter(|(p, _)| if bid_side { *p >= limit } else { *p <= limit }).collect();
                let complete = within.len() < side.len() || side.len() < v.depth;
                want.push((i, within.iter().map(|(_, q)| q).sum::<i64>(), within.len(), complete));
            }
            let got: Vec<_> = vol.iter().map(|v| (v.venue.index(), v.qty.raw(), v.levels, v.complete)).collect();
            prop_assert_eq!(&got, &want);
            prop_assert_eq!(vol.total.unwrap().raw(), want.iter().map(|w| w.1).sum::<i64>());
            prop_assert_eq!(vol.complete, want.iter().all(|w| w.3));
        }
        let _ = ids;
    }
}
