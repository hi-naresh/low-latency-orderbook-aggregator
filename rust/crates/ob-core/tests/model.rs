//! Property tests: the order book must behave exactly like a simple, obviously-correct
//! model built on `BTreeMap`, for any sequence of operations.
//!
//! proptest generates thousands of random operation sequences. After **every** operation
//! the book and the model must agree on: what the operation did (`LevelChange` or
//! snapshot result), every level on both sides in order, and whether the book is
//! crossed. When a case fails, proptest shrinks it to a minimal failing sequence.
#![allow(clippy::unwrap_used, clippy::cast_possible_wrap)]

use std::collections::BTreeMap;

use ob_core::{Level, LevelChange, OrderBook, Px, Qty, Side, SnapshotError};
use proptest::prelude::*;

// ---------------------------------------------------------------------------
// The model: as simple as possible, no performance concerns
// ---------------------------------------------------------------------------

struct ModelSide {
    levels: BTreeMap<i64, i64>, // price → quantity
    bid: bool,
    cap: usize,
}

impl ModelSide {
    fn new(bid: bool, cap: usize) -> Self {
        ModelSide {
            levels: BTreeMap::new(),
            bid,
            cap,
        }
    }

    /// The worst price currently held.
    fn worst(&self) -> Option<i64> {
        let mut keys = self.levels.keys().copied();
        if self.bid {
            keys.next()
        } else {
            keys.next_back()
        }
    }

    fn is_worse(&self, a: i64, b: i64) -> bool {
        if self.bid { a < b } else { a > b }
    }

    fn set(&mut self, px: i64, qty: i64) -> LevelChange {
        if qty == 0 {
            return match self.levels.remove(&px) {
                Some(_) => LevelChange::Removed,
                None => LevelChange::RemovedAbsent,
            };
        }
        if let Some(q) = self.levels.get_mut(&px) {
            *q = qty;
            return LevelChange::Updated;
        }
        if self.levels.len() == self.cap {
            let worst = self.worst().unwrap();
            if self.is_worse(px, worst) {
                return LevelChange::OutOfDepth;
            }
            self.levels.remove(&worst);
        }
        self.levels.insert(px, qty);
        LevelChange::Inserted
    }

    /// Levels best → worst.
    fn best_first(&self) -> Vec<(i64, i64)> {
        let v: Vec<_> = self.levels.iter().map(|(&p, &q)| (p, q)).collect();
        if self.bid {
            v.into_iter().rev().collect()
        } else {
            v
        }
    }

    /// Mirrors `OrderBook::replace` validation: strictly best-first, no zero quantity.
    fn validate(&self, side: Side, levels: &[(i64, i64)]) -> Result<(), SnapshotError> {
        for (i, &(px, qty)) in levels.iter().enumerate() {
            if qty == 0 {
                return Err(SnapshotError::ZeroQty(side));
            }
            if i > 0 && !self.is_worse(px, levels[i - 1].0) {
                return Err(SnapshotError::Unsorted(side));
            }
        }
        Ok(())
    }

    fn load(&mut self, levels: &[(i64, i64)]) {
        self.levels = levels.iter().take(self.cap).copied().collect();
    }
}

struct Model {
    bids: ModelSide,
    asks: ModelSide,
}

impl Model {
    fn new(cap: usize) -> Self {
        Model {
            bids: ModelSide::new(true, cap),
            asks: ModelSide::new(false, cap),
        }
    }

    fn crossed(&self) -> bool {
        match (
            self.bids.best_first().first(),
            self.asks.best_first().first(),
        ) {
            (Some(b), Some(a)) => b.0 >= a.0,
            _ => false,
        }
    }
}

// ---------------------------------------------------------------------------
// Random operations
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
enum Op {
    Apply(Side, i64, i64),
    Replace(Vec<(i64, i64)>, Vec<(i64, i64)>),
    Clear,
}

fn side() -> impl Strategy<Value = Side> {
    prop_oneof![Just(Side::Bid), Just(Side::Ask)]
}

/// Prices from a narrow band so the same prices recur constantly (updates, removals,
/// collisions); quantities 0..=4, so about one update in five is a removal.
fn level() -> impl Strategy<Value = (i64, i64)> {
    (1000_i64..1040, 0_i64..=4)
}

/// A snapshot side: half the time a valid one (sorted best-first, unique, non-zero),
/// half the time arbitrary levels that are usually invalid.
fn snapshot_side(bid: bool) -> impl Strategy<Value = Vec<(i64, i64)>> {
    let valid = prop::collection::btree_map(1000_i64..1040, 1_i64..=4, 0..12).prop_map(move |m| {
        let v: Vec<_> = m.into_iter().collect();
        if bid {
            v.into_iter().rev().collect()
        } else {
            v
        }
    });
    let arbitrary = prop::collection::vec(level(), 0..12);
    prop_oneof![valid, arbitrary]
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        85 => (side(), level()).prop_map(|(s, (p, q))| Op::Apply(s, p, q)),
        10 => (snapshot_side(true), snapshot_side(false)).prop_map(|(b, a)| Op::Replace(b, a)),
        5 => Just(Op::Clear),
    ]
}

fn to_levels(v: &[(i64, i64)]) -> Vec<Level> {
    v.iter()
        .map(|&(p, q)| Level::new(Px::from_raw(p), Qty::from_raw(q).unwrap()))
        .collect()
}

fn book_side<const CAP: usize>(b: &OrderBook<CAP>, side: Side) -> Vec<(i64, i64)> {
    let iter: Box<dyn Iterator<Item = Level>> = match side {
        Side::Bid => Box::new(b.bids().iter()),
        Side::Ask => Box::new(b.asks().iter()),
    };
    iter.map(|l| (l.px.raw(), l.qty.raw())).collect()
}

/// Runs `ops` on a book with capacity `CAP`, checking it against the model after every
/// operation.
fn check<const CAP: usize>(ops: &[Op]) -> Result<(), TestCaseError> {
    let mut book: OrderBook<CAP> = OrderBook::new();
    let mut model = Model::new(CAP);
    for (step, op) in ops.iter().enumerate() {
        match op {
            Op::Apply(side, px, qty) => {
                let got = book.apply(*side, to_levels(&[(*px, *qty)])[0]);
                let want = match side {
                    Side::Bid => model.bids.set(*px, *qty),
                    Side::Ask => model.asks.set(*px, *qty),
                };
                prop_assert_eq!(got, want, "step {}: {:?}", step, op);
            }
            Op::Replace(bids, asks) => {
                let got = book.replace(&to_levels(bids), &to_levels(asks));
                let want = model
                    .bids
                    .validate(Side::Bid, bids)
                    .and_then(|()| model.asks.validate(Side::Ask, asks));
                if want.is_ok() {
                    model.bids.load(bids);
                    model.asks.load(asks);
                }
                prop_assert_eq!(got, want, "step {}: {:?}", step, op);
            }
            Op::Clear => {
                book.clear();
                model = Model::new(CAP);
            }
        }
        prop_assert_eq!(
            book_side(&book, Side::Bid),
            model.bids.best_first(),
            "bids, step {}",
            step
        );
        prop_assert_eq!(
            book_side(&book, Side::Ask),
            model.asks.best_first(),
            "asks, step {}",
            step
        );
        prop_assert_eq!(book.is_crossed(), model.crossed(), "crossed, step {}", step);
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 2_000, ..ProptestConfig::default() })]

    // Capacity 4: the band holds 40 prices, so sides are full most of the time and the
    // drop-the-worst / out-of-depth paths run constantly.
    #[test]
    fn small_book_matches_model(ops in prop::collection::vec(op(), 1..300)) {
        check::<4>(&ops)?;
    }

    // Capacity 1: every insert of a better price replaces the only level.
    #[test]
    fn single_level_book_matches_model(ops in prop::collection::vec(op(), 1..100)) {
        check::<1>(&ops)?;
    }

    // Production capacity: sides never fill from a 40-price band.
    #[test]
    fn full_size_book_matches_model(ops in prop::collection::vec(op(), 1..300)) {
        check::<64>(&ops)?;
    }

    // Arbitrary prices across the whole i64 range (search at the extremes).
    #[test]
    fn arbitrary_prices_match_model(
        ops in prop::collection::vec(
            (side(), any::<i64>(), 0_i64..=4).prop_map(|(s, p, q)| Op::Apply(s, p, q)),
            1..200,
        )
    ) {
        check::<4>(&ops)?;
        check::<64>(&ops)?;
    }
}
