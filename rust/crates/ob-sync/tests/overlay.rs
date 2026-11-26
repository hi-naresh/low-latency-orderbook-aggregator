//! `publish_with_top`: a fresher best bid/ask laid over an older book.
#![allow(clippy::unwrap_used)]

use ob_core::{Level, OrderBook, Px, Qty};
use ob_sync::{BookMeta, DEPTH, Snapshot, TopOverlay, VenueStatus, book_channel};
use proptest::prelude::*;

fn l(px: i64, qty: i64) -> Level {
    Level::new(Px::from_raw(px), Qty::from_raw(qty).unwrap())
}

/// The test book's capacity (published depth is `min(DEPTH, CAP)`).
const CAP: usize = 128;

fn publish(bids: &[Level], asks: &[Level], top: Option<TopOverlay>) -> Snapshot {
    let mut book: OrderBook<CAP> = OrderBook::new();
    book.replace(bids, asks).unwrap();
    let (mut p, r) = book_channel();
    p.publish_with_top(
        &book,
        top,
        BookMeta {
            status: VenueStatus::Live,
            ..BookMeta::default()
        },
    );
    let mut s = Snapshot::default();
    r.read_into(&mut s);
    s
}

#[test]
fn the_overlay_replaces_stale_better_levels_and_keeps_deeper_ones() {
    // Book (older): bids 100, 99, 98; asks 101, 102, 103.
    let bids = [l(100, 5), l(99, 5), l(98, 5)];
    let asks = [l(101, 5), l(102, 5), l(103, 5)];
    // Newer top: the bid at 100 was traded through (best bid now 99, with a new
    // quantity), and a new ask appeared at 100, below the book's best ask.
    let s = publish(
        &bids,
        &asks,
        Some(TopOverlay {
            bid: l(99, 7),
            ask: l(100, 2),
        }),
    );
    assert_eq!(s.bids(), [l(99, 7), l(98, 5)]); // 100 dropped, 99's qty from the overlay
    assert_eq!(s.asks(), [l(100, 2), l(101, 5), l(102, 5), l(103, 5)]); // new best inserted
}

#[test]
fn without_an_overlay_it_is_the_plain_book() {
    let bids = [l(100, 5), l(99, 5)];
    let asks = [l(101, 5)];
    let s = publish(&bids, &asks, None);
    assert_eq!((s.bids(), s.asks()), (&bids[..], &asks[..]));
}

#[test]
fn an_overlay_on_an_empty_book_is_the_whole_book() {
    let s = publish(
        &[],
        &[],
        Some(TopOverlay {
            bid: l(100, 1),
            ask: l(101, 1),
        }),
    );
    assert_eq!((s.bids(), s.asks()), (&[l(100, 1)][..], &[l(101, 1)][..]));
}

/// Strictly decreasing (bids) or increasing (asks) prices, positive quantities.
fn side(ascending: bool) -> impl Strategy<Value = Vec<Level>> {
    prop::collection::btree_map(0_i64..200, 1_i64..50, 0..(CAP + 10)).prop_map(move |m| {
        let mut v: Vec<Level> = m.into_iter().map(|(p, q)| l(1000 + p, q)).collect();
        if !ascending {
            v.reverse();
        }
        v
    })
}

proptest! {
    /// Against a direct model: overlay level first, then the book's strictly worse
    /// levels, at most DEPTH per side.
    #[test]
    fn matches_the_model(
        bids in side(false),
        asks in side(true),
        top in prop::option::of((0_i64..200, 1_i64..50, 0_i64..200, 1_i64..50)),
    ) {
        let top = top.map(|(bp, bq, ap, aq)| TopOverlay { bid: l(1000 + bp, bq), ask: l(1000 + ap, aq) });
        // Keep the book itself uncrossed (bids below asks), as `replace` requires.
        let best_ask = asks.first().map_or(i64::MAX, |a| a.px.raw());
        let bids: Vec<Level> = bids.into_iter().filter(|b| b.px.raw() < best_ask).collect();
        let s = publish(&bids, &asks, top);
        let model = |levels: &[Level], first: Option<Level>, worse: &dyn Fn(Level, Level) -> bool| {
            let mut v: Vec<Level> = first.into_iter().collect();
            v.extend(levels.iter().copied().filter(|x| first.is_none_or(|f| worse(*x, f))));
            v.truncate(DEPTH.min(CAP));
            v
        };
        prop_assert_eq!(s.bids(), &model(&bids, top.map(|t| t.bid), &|x, f| x.px < f.px)[..]);
        prop_assert_eq!(s.asks(), &model(&asks, top.map(|t| t.ask), &|x, f| x.px > f.px)[..]);
    }
}
