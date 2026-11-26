//! Aggregator behaviour: ordering, ties, venue health, caching, and volume semantics.
#![allow(clippy::unwrap_used)]

use ob_agg::{Aggregator, Config, Exclusion, VenueConfig, VenueId, VenueState};
use ob_core::{Level, OrderBook, Px, Qty};
use ob_sync::{BookMeta, BookPublisher, VenueStatus, book_channel};

const SEC: u64 = 1_000_000_000;

fn lvl(px: i64, qty: i64) -> Level {
    Level::new(Px::from_raw(px), Qty::from_raw(qty).unwrap())
}

fn px(p: i64) -> Px {
    Px::from_raw(p)
}

fn qty(q: i64) -> Qty {
    Qty::from_raw(q).unwrap()
}

/// Publishes a book with the given levels and health.
fn publish(
    p: &mut BookPublisher,
    bids: &[Level],
    asks: &[Level],
    status: VenueStatus,
    recv_ns: u64,
) {
    let mut b: OrderBook = OrderBook::new();
    b.replace(bids, asks).unwrap();
    p.publish(
        &b,
        BookMeta {
            update_id: 1,
            exchange_ts: 1,
            recv_ns,
            status,
        },
    );
}

/// An aggregator with `n` venues (depth 20 each), their publishers and their IDs.
fn setup(n: usize) -> (Aggregator, Vec<BookPublisher>, Vec<VenueId>) {
    let mut agg = Aggregator::new(Config {
        max_age_ns: 2 * SEC,
        ..Config::default()
    });
    let names = ["binance", "bybit", "okx", "deribit"];
    let (pubs, ids) = (0..n)
        .map(|i| {
            let (p, r) = book_channel();
            (
                p,
                agg.add_venue(
                    VenueConfig {
                        name: names[i],
                        depth: 20,
                        taker_fee_ppm: 0,
                    },
                    r,
                )
                .unwrap(),
            )
        })
        .unzip();
    (agg, pubs, ids)
}

const LIVE: VenueStatus = VenueStatus::Live;

#[test]
fn empty_aggregator_has_no_quotes() {
    let mut agg = Aggregator::new(Config::default());
    assert_eq!(agg.refresh(), 0);
    assert!(agg.best_bids(0).is_empty() && agg.best_asks(0).is_empty());
    assert_eq!(agg.bid_volume_at_or_above(px(1), 0).total, Some(Qty::ZERO));
}

#[test]
fn one_venue_one_quote() {
    // The Java skeleton's only test: one exchange, one bid → one best bid.
    let (mut agg, mut p, ids) = setup(1);
    publish(&mut p[0], &[lvl(500_000, 15)], &[], LIVE, 0);
    agg.refresh();
    let bids = agg.best_bids(0);
    assert_eq!(bids.len(), 1);
    assert_eq!(
        (bids[0].px, bids[0].qty, bids[0].venue),
        (px(500_000), qty(15), ids[0])
    );
    assert_eq!(agg.venue(bids[0].venue).unwrap().name, "binance");
    assert!(agg.best_asks(0).is_empty());
}

#[test]
fn best_prices_are_ordered_across_venues() {
    let (mut agg, mut p, _ids) = setup(3);
    publish(
        &mut p[0],
        &[lvl(100, 1), lvl(99, 5)],
        &[lvl(103, 1)],
        LIVE,
        0,
    );
    publish(
        &mut p[1],
        &[lvl(101, 2)],
        &[lvl(102, 2), lvl(104, 9)],
        LIVE,
        0,
    );
    publish(&mut p[2], &[lvl(99, 3)], &[lvl(105, 3)], LIVE, 0);
    agg.refresh();
    let bids: Vec<_> = agg
        .best_bids(0)
        .iter()
        .map(|q| (q.px.raw(), q.venue.index()))
        .collect();
    assert_eq!(bids, [(101, 1), (100, 0), (99, 2)]);
    let asks: Vec<_> = agg
        .best_asks(0)
        .iter()
        .map(|q| (q.px.raw(), q.venue.index()))
        .collect();
    assert_eq!(asks, [(102, 1), (103, 0), (105, 2)]);
    let total: i64 = agg.best_bids(0).iter().map(|q| q.qty.raw()).sum();
    assert_eq!(total, qty(6).raw());
}

#[test]
fn ties_prefer_size_then_venue_order() {
    let (mut agg, mut p, _ids) = setup(3);
    publish(&mut p[0], &[lvl(100, 1)], &[lvl(101, 5)], LIVE, 0);
    publish(&mut p[1], &[lvl(100, 7)], &[lvl(101, 5)], LIVE, 0);
    publish(&mut p[2], &[lvl(100, 1)], &[lvl(101, 9)], LIVE, 0);
    agg.refresh();
    let bids: Vec<_> = agg.best_bids(0).iter().map(|q| q.venue.index()).collect();
    assert_eq!(bids, [1, 0, 2]); // larger size first; equal size keeps venue order
    let asks: Vec<_> = agg.best_asks(0).iter().map(|q| q.venue.index()).collect();
    assert_eq!(asks, [2, 0, 1]);
}

#[test]
fn unhealthy_venues_are_excluded_with_a_reason() {
    let (mut agg, mut p, ids) = setup(4);
    publish(&mut p[0], &[lvl(100, 1)], &[lvl(101, 1)], LIVE, 9 * SEC); // healthy
    publish(
        &mut p[1],
        &[lvl(200, 1)],
        &[lvl(201, 1)],
        VenueStatus::Syncing,
        9 * SEC,
    );
    publish(&mut p[2], &[lvl(300, 1)], &[lvl(301, 1)], LIVE, 7 * SEC - 1); // too old at 9 s
    publish(&mut p[3], &[lvl(102, 1)], &[lvl(102, 1)], LIVE, 9 * SEC); // locked = crossed
    agg.refresh();
    let now = 9 * SEC;
    let state = |i: usize| agg.state(ids[i], now).unwrap();
    assert_eq!(state(0), VenueState::Included);
    assert_eq!(state(1), VenueState::Excluded(Exclusion::Syncing));
    assert_eq!(
        state(2),
        VenueState::Excluded(Exclusion::Stale {
            age_ns: 2 * SEC + 1
        })
    );
    assert_eq!(state(3), VenueState::Excluded(Exclusion::Crossed));
    // Only venue 0 contributes, even though the others show better prices.
    let bids = agg.best_bids(now);
    assert_eq!((bids.len(), bids[0].venue.index()), (1, 0));
}

#[test]
fn staleness_boundary_is_inclusive() {
    let (mut agg, mut p, ids) = setup(1);
    publish(&mut p[0], &[lvl(100, 1)], &[lvl(101, 1)], LIVE, 5 * SEC);
    agg.refresh();
    let id = ids[0];
    assert_eq!(agg.state(id, 7 * SEC), Some(VenueState::Included)); // exactly max age
    assert!(matches!(
        agg.state(id, 7 * SEC + 1),
        Some(VenueState::Excluded(Exclusion::Stale { .. }))
    ));
    // A receive time slightly ahead of `now` (clock read on another core) is not "stale".
    assert_eq!(agg.state(id, 4 * SEC), Some(VenueState::Included));
}

#[test]
fn never_published_venues_are_down() {
    let (mut agg, _p, ids) = setup(2);
    agg.refresh();
    assert_eq!(
        agg.state(ids[1], 0),
        Some(VenueState::Excluded(Exclusion::Down))
    );
}

#[test]
fn refresh_copies_only_changed_venues() {
    let (mut agg, mut p, _ids) = setup(3);
    for pubr in &mut p {
        publish(pubr, &[lvl(100, 1)], &[lvl(101, 1)], LIVE, 0);
    }
    assert_eq!(agg.refresh(), 3);
    assert_eq!(
        agg.refresh(),
        0,
        "nothing changed: one version load per venue, no copies"
    );
    publish(&mut p[1], &[lvl(100, 2)], &[lvl(101, 1)], LIVE, 0);
    assert_eq!(agg.refresh(), 1);
    assert_eq!(
        agg.best_bids(0)
            .iter()
            .find(|q| q.venue.index() == 1)
            .unwrap()
            .qty,
        qty(2)
    );
}

#[test]
fn queries_see_the_view_as_of_the_last_refresh() {
    let (mut agg, mut p, _ids) = setup(1);
    publish(&mut p[0], &[lvl(100, 1)], &[lvl(101, 1)], LIVE, 0);
    agg.refresh();
    publish(&mut p[0], &[lvl(150, 1)], &[lvl(151, 1)], LIVE, 0);
    // Without a refresh the view is stable: consecutive queries agree with each other.
    assert_eq!(agg.best_bids(0)[0].px, px(100));
    agg.refresh();
    assert_eq!(agg.best_bids(0)[0].px, px(150));
}

#[test]
fn bid_volume_counts_levels_at_or_above_the_limit() {
    let (mut agg, mut p, _ids) = setup(2);
    publish(
        &mut p[0],
        &[lvl(105, 1), lvl(104, 2), lvl(103, 4), lvl(100, 8)],
        &[],
        LIVE,
        0,
    );
    publish(&mut p[1], &[lvl(104, 16), lvl(102, 32)], &[], LIVE, 0);
    agg.refresh();
    let v = agg.bid_volume_at_or_above(px(104), 0);
    let per: Vec<_> = v
        .iter()
        .map(|v| (v.venue.index(), v.qty.raw(), v.levels))
        .collect();
    assert_eq!(per, [(0, 3, 2), (1, 16, 1)]); // 104 is included (inclusive limit)
    assert_eq!(v.total, Some(qty(19)));
    assert!(v.complete); // both venues show a level below the limit
    // A limit above every bid: nothing.
    assert_eq!(
        agg.bid_volume_at_or_above(px(106), 0).total,
        Some(Qty::ZERO)
    );
    // A limit below every bid: everything visible.
    assert_eq!(agg.bid_volume_at_or_above(px(1), 0).total, Some(qty(63)));
}

#[test]
fn ask_volume_counts_levels_at_or_below_the_limit() {
    let (mut agg, mut p, _ids) = setup(2);
    publish(
        &mut p[0],
        &[],
        &[lvl(101, 1), lvl(102, 2), lvl(110, 4)],
        LIVE,
        0,
    );
    publish(&mut p[1], &[], &[lvl(102, 8)], LIVE, 0);
    agg.refresh();
    let v = agg.ask_volume_at_or_below(px(102), 0);
    assert_eq!(v.total, Some(qty(11)));
    let per: Vec<_> = v.iter().map(|v| (v.venue.index(), v.qty.raw())).collect();
    assert_eq!(per, [(0, 3), (1, 8)]);
}

#[test]
fn volume_flags_when_more_may_exist_beyond_the_visible_depth() {
    // Venue depth 20; it sent exactly 20 bid levels, all at or above the limit: the venue
    // may have more volume deeper than it publishes.
    let (mut agg, mut p, _ids) = setup(1);
    let full: Vec<Level> = (0..20).map(|i| lvl(1000 - i, 1)).collect();
    publish(&mut p[0], &full, &[], LIVE, 0);
    agg.refresh();
    let v = agg.bid_volume_at_or_above(px(900), 0);
    assert_eq!((v[0].levels, v[0].complete, v.complete), (20, false, false));
    // With a limit inside the book, the end of the volume is visible: complete.
    assert!(agg.bid_volume_at_or_above(px(990), 0).complete);
    // A book with fewer levels than the venue's depth is the whole book: complete.
    publish(&mut p[0], &full[..5], &[], LIVE, 0);
    agg.refresh();
    assert!(agg.bid_volume_at_or_above(px(900), 0).complete);
}

#[test]
fn at_most_eight_venues() {
    let mut agg = Aggregator::new(Config::default());
    for i in 0..8 {
        assert!(
            agg.add_venue(
                VenueConfig {
                    name: "v",
                    depth: 1,
                    taker_fee_ppm: 0
                },
                book_channel().1
            )
            .is_some(),
            "{i}"
        );
    }
    assert!(
        agg.add_venue(
            VenueConfig {
                name: "v",
                depth: 1,
                taker_fee_ppm: 0
            },
            book_channel().1
        )
        .is_none()
    );
}

/// "Best N levels across all venues": merged best first, venue-tagged, healthy
/// venues only, against a direct model (pool every healthy level, sort, take N).
#[test]
fn best_levels_merge_venues_best_first() {
    use ob_agg::Quote;
    use ob_core::Side;
    let (mut agg, mut pubs, ids) = setup(3);
    publish(
        &mut pubs[0],
        &[lvl(100, 1), lvl(98, 1), lvl(95, 1)],
        &[lvl(101, 1), lvl(104, 1)],
        LIVE,
        1,
    );
    publish(
        &mut pubs[1],
        &[lvl(99, 2), lvl(98, 2)],
        &[lvl(102, 2), lvl(103, 2)],
        LIVE,
        1,
    );
    // A venue that's down must not contribute, however good its prices.
    publish(
        &mut pubs[2],
        &[lvl(150, 9)],
        &[lvl(50, 9)],
        VenueStatus::Down,
        1,
    );
    agg.refresh();
    let mut out = [Quote {
        venue: ids[0],
        px: px(0),
        qty: qty(0),
    }; 10];
    let n = agg.best_levels(Side::Bid, 1, &mut out[..4]);
    let got: Vec<(VenueId, i64)> = out[..n].iter().map(|q| (q.venue, q.px.raw())).collect();
    // 100 (A), 99 (B), 98 (A, venue order on ties), 98 (B): the 4 best, not 95.
    assert_eq!(
        got,
        vec![(ids[0], 100), (ids[1], 99), (ids[0], 98), (ids[1], 98)]
    );
    let n = agg.best_levels(Side::Ask, 1, &mut out);
    let got: Vec<i64> = out[..n].iter().map(|q| q.px.raw()).collect();
    assert_eq!(
        got,
        vec![101, 102, 103, 104],
        "all 4 healthy asks; the down venue's 50 is out"
    );
    assert_eq!(out[1].qty, qty(2));
    // Asking for none, or with no healthy venue.
    assert_eq!(agg.best_levels(Side::Bid, 1, &mut []), 0);
    let (agg_empty, _, _) = setup(0);
    assert_eq!(agg_empty.best_levels(Side::Bid, 1, &mut out), 0);
}
