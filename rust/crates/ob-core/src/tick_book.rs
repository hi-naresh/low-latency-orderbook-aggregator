//! A price-indexed order book for deep books (step 26).
//!
//! [`OrderBook`](crate::OrderBook) keeps each side as a sorted array. At 20–64 levels
//! that is the fastest structure (step 4). At 1,000 levels (Binance full depth, step 19) it
//! isn't: every update first binary-searches ~10 steps (unpredictable branches), and every
//! insert or removal shifts the levels better than it. Step 26 measured the 25 KB message
//! (651 updates, 133 inserts, 116 removals; inserts a median 216 levels deep) spending
//! ~30 µs applying.
//!
//! Here each side stores quantities **indexed by price in ticks**:
//!
//! ```text
//! qty:  [ q | 0 | q | q | 0 | ... ]   one slot per tick, in a circular window of WINDOW ticks
//! bits: [ 1   0   1   1   0   ... ]   which ticks hold a level (64 per word)
//! best, worst: the occupied ticks at the two ends
//! ```
//!
//! An update, insert or removal is an array write and a bit flip: O(1), no search, no
//! shifting. Only removing the best or the worst level scans the bitmap (64 ticks per
//! word) to find the next one. Reading best-first walks the set bits.
//!
//! **Capacity rules** (what keeps it exact; the full-depth adapter turns every refused or
//! dropped level into its "known region" boundary):
//! * at most `CAP` levels per side: as in `OrderBook`, a level worse than all of a full
//!   side is refused ([`LevelChange::OutOfDepth`]); a better one drops the worst;
//! * every level less than [`WINDOW`] ticks from the best (so the circular index never
//!   aliases): a level that would stretch the side further is refused if it would be the
//!   worst, or else worst levels are dropped until it fits.
//!
//! With the 8,192-tick window, BTCUSDT (tick 0.1) holds a side spanning up to 819 USD;
//! Binance's 1,000-level snapshot spans ~140 USD.

use crate::book::{BookLevels, Level, LevelChange, Side, SnapshotError};
use crate::divisor::Divisor;
use crate::{Px, Qty};

/// Ticks in each side's window (a multiple of 64).
pub const WINDOW: usize = 8192;
const WORDS: usize = WINDOW / 64;
const MASK: u64 = WINDOW as u64 - 1;
/// `WINDOW` as an `i64` (far below `i64::MAX`).
#[allow(clippy::cast_possible_wrap)] // 8192
const WINDOW_I: i64 = WINDOW as i64;

/// A count below `WINDOW` as an `i64` (never wraps: WINDOW is 8192).
#[inline(always)]
#[allow(clippy::cast_possible_wrap)]
const fn ticks(n: usize) -> i64 {
    n as i64
}

#[inline(always)]
const fn slot(t: i64) -> usize {
    // `t` mod WINDOW: the mask keeps the value below WINDOW, so it fits a usize.
    (t.cast_unsigned() & MASK) as usize
}

/// One side of a [`TickBook`].
#[derive(Clone, Debug)]
#[repr(C, align(64))]
pub struct TickSide<const CAP: usize, const BID: bool> {
    /// Quantity per tick slot (0 where `bits` says there is no level).
    qty: [i64; WINDOW],
    bits: [u64; WORDS],
    len: usize,
    /// The occupied ticks at the two ends (meaningful when `len > 0`).
    best: i64,
    worst: i64,
    /// Price of one tick, raw: a level's price is its tick index × `tick_raw`.
    tick_raw: i64,
}

impl<const CAP: usize, const BID: bool> TickSide<CAP, BID> {
    const SIDE: Side = if BID { Side::Bid } else { Side::Ask };

    const fn new(tick_raw: i64) -> Self {
        assert!(CAP > 0, "book capacity must be at least 1");
        TickSide {
            qty: [0; WINDOW],
            bits: [0; WORDS],
            len: 0,
            best: 0,
            worst: 0,
            tick_raw,
        }
    }

    /// `true` if tick `a` is a better price than tick `b` on this side.
    #[inline(always)]
    const fn better(a: i64, b: i64) -> bool {
        if BID { a > b } else { a < b }
    }

    /// Number of levels.
    #[inline]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// `true` if the side has no levels.
    #[inline]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[inline]
    fn level_at(&self, t: i64) -> Level {
        // Only positive quantities are stored (they come from `Qty`).
        let qty = Qty::from_raw(self.qty[slot(t)]).unwrap_or(Qty::ZERO);
        Level::new(Px::from_raw(t * self.tick_raw), qty)
    }

    /// The best level, if any.
    #[inline]
    pub fn best(&self) -> Option<Level> {
        (self.len > 0).then(|| self.level_at(self.best))
    }

    /// The worst level held, if any.
    #[inline]
    pub fn worst(&self) -> Option<Level> {
        (self.len > 0).then(|| self.level_at(self.worst))
    }

    /// The level `depth` places from the top (0 = best). Walks the levels: O(depth).
    pub fn level(&self, depth: usize) -> Option<Level> {
        self.iter().nth(depth)
    }

    /// Levels from best to worst.
    pub fn iter(&self) -> impl Iterator<Item = Level> + '_ {
        let mut next = (self.len > 0).then_some(self.best);
        let mut left = self.len;
        core::iter::from_fn(move || {
            let t = next?;
            left -= 1;
            next = if left == 0 {
                None
            } else {
                self.toward_worse(t)
            };
            Some(self.level_at(t))
        })
    }

    #[inline(always)]
    fn occupied(&self, t: i64) -> bool {
        let s = slot(t);
        self.bits[s >> 6] >> (s & 63) & 1 == 1
    }

    #[inline(always)]
    fn flip(&mut self, t: i64) {
        let s = slot(t);
        self.bits[s >> 6] ^= 1 << (s & 63);
    }

    /// The nearest occupied tick below `t`, looking at most `WINDOW - 1` ticks down.
    fn below(&self, t: i64) -> Option<i64> {
        let mut pos = t - 1;
        let mut left = WINDOW - 1;
        while left > 0 {
            let s = slot(pos);
            let bit = s & 63;
            // Bits `bit + 1 - take ..= bit` of this word are ticks `pos + 1 - take ..= pos`.
            let take = (bit + 1).min(left);
            let mut m = self.bits[s >> 6] & (u64::MAX >> (63 - bit));
            if take < bit + 1 {
                m &= !((1_u64 << (bit + 1 - take)) - 1);
            }
            if m != 0 {
                let hi = 63 - m.leading_zeros() as usize;
                return Some(pos - ticks(bit - hi));
            }
            pos -= ticks(take);
            left -= take;
        }
        None
    }

    /// The nearest occupied tick above `t`, looking at most `WINDOW - 1` ticks up.
    fn above(&self, t: i64) -> Option<i64> {
        let mut pos = t + 1;
        let mut left = WINDOW - 1;
        while left > 0 {
            let s = slot(pos);
            let bit = s & 63;
            // Bits `bit ..= bit + take - 1` are ticks `pos ..= pos + take - 1`.
            let take = (64 - bit).min(left);
            let mut m = self.bits[s >> 6] >> bit;
            if take < 64 - bit {
                m &= (1_u64 << take) - 1;
            }
            if m != 0 {
                return Some(pos + i64::from(m.trailing_zeros()));
            }
            pos += ticks(take);
            left -= take;
        }
        None
    }

    #[inline]
    fn toward_worse(&self, t: i64) -> Option<i64> {
        if BID { self.below(t) } else { self.above(t) }
    }

    #[inline]
    fn toward_better(&self, t: i64) -> Option<i64> {
        if BID { self.above(t) } else { self.below(t) }
    }

    fn remove(&mut self, t: i64) {
        self.flip(t);
        self.qty[slot(t)] = 0;
        self.len -= 1;
        if self.len > 0 {
            // All levels lie within the window, so a neighbour is always found.
            if t == self.best {
                self.best = self.toward_worse(t).unwrap_or(self.worst);
            }
            if t == self.worst {
                self.worst = self.toward_better(t).unwrap_or(self.best);
            }
        }
    }

    fn insert(&mut self, t: i64, qty: i64) {
        self.flip(t);
        self.qty[slot(t)] = qty;
        if self.len == 0 {
            (self.best, self.worst) = (t, t);
        } else if Self::better(t, self.best) {
            self.best = t;
        } else if Self::better(self.worst, t) {
            self.worst = t;
        }
        self.len += 1;
    }

    /// Sets the quantity at tick `t`; zero removes the level. Also returns the tick of
    /// the innermost level dropped to make room, if any were.
    fn set(&mut self, t: i64, qty: i64) -> (LevelChange, Option<i64>) {
        if self.len > 0 {
            let inside = !Self::better(t, self.best) && !Self::better(self.worst, t);
            // Only a tick between worst and best can be held, and only there is its slot
            // unambiguous (the side spans fewer than WINDOW ticks).
            if inside && self.occupied(t) {
                if qty == 0 {
                    self.remove(t);
                    return (LevelChange::Removed, None);
                }
                self.qty[slot(t)] = qty;
                return (LevelChange::Updated, None);
            }
        }
        if qty == 0 {
            return (LevelChange::RemovedAbsent, None);
        }
        if self.len == 0 {
            self.insert(t, qty);
            return (LevelChange::Inserted, None);
        }
        if Self::better(self.worst, t) {
            // It would be the new worst: refuse if full, or if too far from the best.
            if self.len == CAP || (t - self.best).abs() >= WINDOW_I {
                return (LevelChange::OutOfDepth, None);
            }
            self.insert(t, qty);
            return (LevelChange::Inserted, None);
        }
        // Better than the worst: make room by dropping worst levels if needed.
        let best = if Self::better(t, self.best) {
            t
        } else {
            self.best
        };
        let mut dropped = None;
        while self.len > 0 && ((best - self.worst).abs() >= WINDOW_I || self.len == CAP) {
            let w = self.worst;
            self.remove(w);
            dropped = Some(w);
        }
        self.insert(t, qty);
        (LevelChange::Inserted, dropped)
    }

    fn validate_snapshot(levels: &[Level]) -> Result<(), SnapshotError> {
        for (i, level) in levels.iter().enumerate() {
            if level.qty.is_zero() {
                return Err(SnapshotError::ZeroQty(Self::SIDE));
            }
            let in_order = match Self::SIDE {
                Side::Bid => level.px < levels[i.saturating_sub(1)].px,
                Side::Ask => level.px > levels[i.saturating_sub(1)].px,
            };
            if i > 0 && !in_order {
                return Err(SnapshotError::Unsorted(Self::SIDE));
            }
        }
        Ok(())
    }

    fn clear(&mut self) {
        // Only the occupied slots are non-zero: clear those, not the whole window.
        let mut next = (self.len > 0).then_some(self.best);
        while let Some(t) = next {
            next = if t == self.worst {
                None
            } else {
                self.toward_worse(t)
            };
            self.qty[slot(t)] = 0;
        }
        self.bits = [0; WORDS];
        self.len = 0;
    }
}

/// A price-indexed order book for one instrument on one venue: up to `CAP` levels per
/// side, each side spanning fewer than [`WINDOW`] ticks.
///
/// About 2 × 65 KiB, so callers usually keep it in a `Box`.
#[derive(Clone, Debug)]
pub struct TickBook<const CAP: usize> {
    tick: Divisor,
    bids: TickSide<CAP, true>,
    asks: TickSide<CAP, false>,
}

impl<const CAP: usize> TickBook<CAP> {
    /// An empty book for prices that are multiples of `tick` (the instrument's tick, as
    /// [`crate::InstrumentSpec::tick`] gives it). `tick` must be positive.
    pub fn new(tick: Px) -> Self {
        let raw = tick.raw().max(1);
        TickBook {
            tick: Divisor::new(raw.unsigned_abs()),
            bids: TickSide::new(raw),
            asks: TickSide::new(raw),
        }
    }

    #[inline]
    fn index(&self, px: Px) -> i64 {
        // Prices come from `InstrumentSpec`, which only accepts multiples of the tick.
        debug_assert!(self.tick.divides(px.raw()), "price off the tick");
        self.tick.exact_quotient(px.raw())
    }

    /// Applies one level (zero quantity removes it). Returns what happened, and the price
    /// of the innermost level dropped to make room, if any were: everything at and beyond
    /// it is no longer held.
    #[inline]
    pub fn apply(&mut self, side: Side, level: Level) -> (LevelChange, Option<Px>) {
        let t = self.index(level.px);
        let tick = self.bids.tick_raw;
        let (change, dropped) = match side {
            Side::Bid => self.bids.set(t, level.qty.raw()),
            Side::Ask => self.asks.set(t, level.qty.raw()),
        };
        (change, dropped.map(|d| Px::from_raw(d * tick)))
    }

    /// Replaces both sides with best-first snapshots, all-or-nothing. Levels beyond the
    /// capacity rules (more than `CAP`, or too far from the best) are dropped from the
    /// worst end; the side's `len()` says how many were kept.
    pub fn replace(&mut self, bids: &[Level], asks: &[Level]) -> Result<(), SnapshotError> {
        TickSide::<CAP, true>::validate_snapshot(bids)?;
        TickSide::<CAP, false>::validate_snapshot(asks)?;
        self.clear();
        for l in bids {
            if self.apply(Side::Bid, *l).0 == LevelChange::OutOfDepth {
                break;
            }
        }
        for l in asks {
            if self.apply(Side::Ask, *l).0 == LevelChange::OutOfDepth {
                break;
            }
        }
        Ok(())
    }

    /// Removes every level.
    pub fn clear(&mut self) {
        self.bids.clear();
        self.asks.clear();
    }

    /// Highest bid.
    #[inline]
    pub fn best_bid(&self) -> Option<Level> {
        self.bids.best()
    }

    /// Lowest ask.
    #[inline]
    pub fn best_ask(&self) -> Option<Level> {
        self.asks.best()
    }

    /// The bid side.
    #[inline]
    pub fn bids(&self) -> &TickSide<CAP, true> {
        &self.bids
    }

    /// The ask side.
    #[inline]
    pub fn asks(&self) -> &TickSide<CAP, false> {
        &self.asks
    }

    /// Best bid ≥ best ask: impossible on a correct single-venue book (step 3).
    #[inline]
    pub fn is_crossed(&self) -> bool {
        match (self.best_bid(), self.best_ask()) {
            (Some(b), Some(a)) => b.px >= a.px,
            _ => false,
        }
    }
}

impl<const CAP: usize> BookLevels for TickBook<CAP> {
    #[inline]
    fn bid_levels(&self) -> impl Iterator<Item = Level> + '_ {
        self.bids.iter()
    }
    #[inline]
    fn ask_levels(&self) -> impl Iterator<Item = Level> + '_ {
        self.asks.iter()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::cast_possible_wrap)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::OrderBook;

    fn lv(px: i64, qty: i64) -> Level {
        Level::new(Px::from_raw(px), Qty::from_raw(qty).unwrap())
    }

    #[test]
    fn basic_operations_and_neighbours() {
        let mut b = TickBook::<8>::new(Px::from_raw(5));
        assert_eq!(b.apply(Side::Bid, lv(100, 1)).0, LevelChange::Inserted);
        assert_eq!(b.apply(Side::Bid, lv(120, 2)).0, LevelChange::Inserted);
        assert_eq!(b.apply(Side::Bid, lv(105, 3)).0, LevelChange::Inserted);
        assert_eq!(b.best_bid(), Some(lv(120, 2)));
        assert_eq!(b.bids().worst(), Some(lv(100, 1)));
        let got: Vec<i64> = b.bids().iter().map(|l| l.px.raw()).collect();
        assert_eq!(got, [120, 105, 100]);
        assert_eq!(b.apply(Side::Bid, lv(120, 0)).0, LevelChange::Removed);
        assert_eq!(b.best_bid(), Some(lv(105, 3)));
        assert_eq!(b.apply(Side::Bid, lv(110, 0)).0, LevelChange::RemovedAbsent);
        assert_eq!(b.apply(Side::Bid, lv(105, 9)).0, LevelChange::Updated);
        assert_eq!(b.apply(Side::Ask, lv(130, 1)).0, LevelChange::Inserted);
        assert!(!b.is_crossed());
        assert_eq!(b.apply(Side::Ask, lv(100, 1)).0, LevelChange::Inserted);
        assert!(b.is_crossed());
    }

    /// Levels around the window's wrap-around and far apart in absolute price.
    #[test]
    fn neighbours_across_the_circular_wrap() {
        let mut b = TickBook::<1024>::new(Px::from_raw(1));
        let base = 3 * WINDOW_I - 3; // slots WINDOW-3 .. and then wrap to 0, 1, ...
        for t in [base, base + 2, base + 5, base + 70, base + 200] {
            b.apply(Side::Ask, lv(t, 1));
        }
        let got: Vec<i64> = b.asks().iter().map(|l| l.px.raw()).collect();
        assert_eq!(got, [base, base + 2, base + 5, base + 70, base + 200]);
        b.apply(Side::Ask, lv(base, 0));
        b.apply(Side::Ask, lv(base + 200, 0));
        assert_eq!(b.best_ask().unwrap().px.raw(), base + 2);
        assert_eq!(b.asks().worst().unwrap().px.raw(), base + 70);
    }

    /// The neighbour scans look at most `WINDOW - 1` ticks away, so they never wrap round
    /// to the level they start from (the slot index is the tick modulo the window).
    #[test]
    fn scans_stop_before_wrapping_onto_themselves() {
        let mut b = TickBook::<4>::new(Px::from_raw(1));
        for t in [64_i64, 100, 8_191, 8_192, 8_193] {
            b.clear();
            b.apply(Side::Bid, lv(t, 1));
            assert_eq!(b.bids.below(t), None, "below {t}");
            assert_eq!(b.bids.above(t), None, "above {t}");
        }
        // One tick less than a window away is still found, in both directions.
        b.clear();
        b.apply(Side::Bid, lv(1000, 1));
        assert_eq!(b.bids.below(1000 + WINDOW_I - 1), Some(1000));
        assert_eq!(b.bids.above(1001 - WINDOW_I), Some(1000));
    }

    /// The window: a level too far from the best is refused if it would be the worst;
    /// a far *better* level drops worst levels until it fits, and says which.
    #[test]
    fn the_window_refuses_or_drops_and_reports() {
        let mut b = TickBook::<1024>::new(Px::from_raw(1));
        for t in 1000..1010 {
            b.apply(Side::Bid, lv(t, 1));
        }
        assert_eq!(
            b.apply(Side::Bid, lv(1009 - WINDOW_I, 1)).0,
            LevelChange::OutOfDepth
        );
        let (c, dropped) = b.apply(Side::Bid, lv(1004 + WINDOW_I, 1));
        assert_eq!(c, LevelChange::Inserted);
        // Ticks 1000..=1004 are now ≥ WINDOW below the new best: dropped; 1004 innermost.
        assert_eq!(dropped, Some(Px::from_raw(1004)));
        assert_eq!(b.bids().len(), 6);
        assert_eq!(b.bids().worst().unwrap().px.raw(), 1005);
    }

    #[derive(Clone, Debug)]
    enum Op {
        Apply(bool, i64, i64),
        Snapshot(Vec<i64>, Vec<i64>),
        Clear,
    }

    fn op() -> impl Strategy<Value = Op> {
        prop_oneof![
            20 => (any::<bool>(), 900_i64..1100, prop_oneof![1 => Just(0_i64), 3 => 1_i64..50])
                .prop_map(|(b, px, q)| Op::Apply(b, px, q)),
            1 => (proptest::collection::btree_set(900_i64..1000, 0..30),
                  proptest::collection::btree_set(1000_i64..1100, 0..30))
                .prop_map(|(b, a)| Op::Snapshot(b.into_iter().rev().collect(), a.into_iter().collect())),
            1 => Just(Op::Clear),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(2_000))]
        /// While every price fits in the window, the tick book behaves **exactly** like
        /// the sorted-array book: the same result for every operation, the same levels,
        /// at a capacity small enough that dropping and refusing happen often.
        #[test]
        fn matches_the_sorted_book(ops in proptest::collection::vec(op(), 1..200)) {
            let mut sorted: OrderBook<16> = OrderBook::new();
            let mut tick: TickBook<16> = TickBook::new(Px::from_raw(1));
            for op in ops {
                match op {
                    Op::Apply(bid, px, q) => {
                        let side = if bid { Side::Bid } else { Side::Ask };
                        let worst_before = match side {
                            Side::Bid => sorted.bids().level(15),
                            Side::Ask => sorted.asks().level(15),
                        };
                        let want = sorted.apply(side, lv(px, q));
                        let (got, dropped) = tick.apply(side, lv(px, q));
                        prop_assert_eq!(got, want);
                        // A drop happens exactly when a full side takes a better level.
                        let expect_drop = (want == LevelChange::Inserted)
                            .then_some(worst_before).flatten().map(|l| l.px);
                        prop_assert_eq!(dropped, expect_drop);
                    }
                    Op::Snapshot(b, a) => {
                        let b: Vec<Level> = b.into_iter().map(|p| lv(p, 1)).collect();
                        let a: Vec<Level> = a.into_iter().map(|p| lv(p, 2)).collect();
                        prop_assert_eq!(tick.replace(&b, &a), sorted.replace(&b, &a));
                    }
                    Op::Clear => {
                        sorted.clear();
                        tick.clear();
                    }
                }
                let s: Vec<Level> = sorted.bids().iter().collect();
                let t: Vec<Level> = tick.bids().iter().collect();
                prop_assert_eq!(s, t);
                let s: Vec<Level> = sorted.asks().iter().collect();
                let t: Vec<Level> = tick.asks().iter().collect();
                prop_assert_eq!(s, t);
                prop_assert_eq!(tick.bids().len(), sorted.bids().len());
                prop_assert_eq!(tick.is_crossed(), sorted.is_crossed());
            }
        }

        /// With prices spread over more than the window, the side always holds the best
        /// levels that fit: sorted, within the window, at most CAP, and whatever was
        /// dropped or refused was worse than everything kept.
        #[test]
        fn wide_prices_keep_the_best_that_fit(
            ops in proptest::collection::vec((0_i64..4 * WINDOW_I, 0_i64..5), 1..400)
        ) {
            let mut tick: TickBook<64> = TickBook::new(Px::from_raw(1));
            for (px, q) in ops {
                let (c, dropped) = tick.apply(Side::Ask, lv(px + 1, q));
                let held: Vec<i64> = tick.asks().iter().map(|l| l.px.raw()).collect();
                prop_assert!(held.windows(2).all(|w| w[0] < w[1]), "sorted best-first");
                prop_assert!(held.len() <= 64);
                if let (Some(&b), Some(&w)) = (held.first(), held.last()) {
                    prop_assert!(w - b < WINDOW_I, "within the window");
                }
                if let (Some(d), Some(&w)) = (dropped, held.last()) {
                    prop_assert!(d.raw() > w, "dropped levels were worse than all kept");
                }
                if c == LevelChange::OutOfDepth {
                    prop_assert!(!held.contains(&(px + 1)));
                }
            }
        }
    }
}
