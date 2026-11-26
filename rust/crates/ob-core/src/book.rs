//! A single-venue, single-instrument order book: allocation-free, cache-friendly.
//!
//! Each side is a [`Ladder`]: two fixed-size arrays (prices and quantities, a
//! "structure of arrays") kept sorted from worst to best, so **the best level is the last
//! element**. The most frequent updates are at or near the top of the book, and at the
//! end of an array an insert or delete moves few or no elements.
//!
//! The book never allocates: its capacity `CAP` is fixed at compile time. When a side is
//! full, the worst level is dropped. That's correct because venues only promise their
//! top N levels anyway (Binance 20, Bybit 50), so `CAP` is chosen above that.

use core::cmp::Ordering;

use crate::{Px, Qty};

/// Which side of the book.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Side {
    /// Buy orders. Best = highest price.
    Bid,
    /// Sell orders. Best = lowest price.
    Ask,
}

impl Side {
    /// The other side.
    pub const fn opposite(self) -> Side {
        match self {
            Side::Bid => Side::Ask,
            Side::Ask => Side::Bid,
        }
    }
}

/// One price level: a price and the total quantity resting at it.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Level {
    /// The price.
    pub px: Px,
    /// Total quantity at that price. In an update, zero means "remove the level".
    pub qty: Qty,
}

impl Level {
    /// Shorthand constructor.
    pub const fn new(px: Px, qty: Qty) -> Level {
        Level { px, qty }
    }
}

/// What an update did to the book. Useful for metrics, tests and spotting bad feeds.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LevelChange {
    /// A new price level was added.
    Inserted,
    /// An existing level's quantity changed.
    Updated,
    /// A level was removed (update with zero quantity).
    Removed,
    /// Zero-quantity update for a price that isn't in the book. Harmless, but frequent
    /// ones can mean we're out of sync.
    RemovedAbsent,
    /// The side was full and the new level was worse than all of them, so it was not
    /// kept (it's outside the depth we track).
    OutOfDepth,
}

/// Why a snapshot was refused. The book is left exactly as it was.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SnapshotError {
    /// Levels were not strictly best-first (out of order, or a duplicate price).
    Unsorted(Side),
    /// A level with zero quantity; a snapshot lists only levels that exist.
    ZeroQty(Side),
}

impl core::fmt::Display for SnapshotError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            SnapshotError::Unsorted(s) => write!(f, "{s:?} levels not strictly best-first"),
            SnapshotError::ZeroQty(s) => write!(f, "{s:?} level with zero quantity in snapshot"),
        }
    }
}

impl core::error::Error for SnapshotError {}

/// Read access to a book's levels, best first: what publishing needs. Implemented by
/// [`OrderBook`] (sorted arrays) and [`crate::TickBook`] (price-indexed, step 26), so one
/// publisher serves both.
pub trait BookLevels {
    /// Bids from best (highest) to worst.
    fn bid_levels(&self) -> impl Iterator<Item = Level> + '_;
    /// Asks from best (lowest) to worst.
    fn ask_levels(&self) -> impl Iterator<Item = Level> + '_;
}

/// One side of the book: sorted worst → best, best at index `len - 1`.
///
/// `BID` is a compile-time constant, so the compiler generates two specialised copies of
/// this code (one per side) with the comparison direction baked in: no run-time branch
/// on the side.
#[derive(Clone, Debug)]
#[repr(C, align(64))]
pub struct Ladder<const CAP: usize, const BID: bool> {
    /// Prices, raw. Only `px[..len]` is meaningful.
    px: [i64; CAP],
    /// Quantities, raw, parallel to `px`.
    qty: [i64; CAP],
    len: usize,
}

impl<const CAP: usize, const BID: bool> Ladder<CAP, BID> {
    const SIDE: Side = if BID { Side::Bid } else { Side::Ask };

    const fn new() -> Self {
        assert!(CAP > 0, "book capacity must be at least 1");
        Ladder {
            px: [0; CAP],
            qty: [0; CAP],
            len: 0,
        }
    }

    /// `true` if price `a` is better than price `b` on this side.
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

    /// The best level, if any.
    #[inline]
    pub fn best(&self) -> Option<Level> {
        self.level(0)
    }

    /// The level `depth` places from the top (0 = best).
    #[inline]
    pub fn level(&self, depth: usize) -> Option<Level> {
        let i = self.len.checked_sub(depth + 1)?;
        Some(self.at(i))
    }

    /// Levels from best to worst.
    pub fn iter(&self) -> impl DoubleEndedIterator<Item = Level> + ExactSizeIterator + '_ {
        (0..self.len).rev().map(|i| self.at(i))
    }

    #[inline]
    fn at(&self, i: usize) -> Level {
        // Only non-negative quantities are ever stored (they come from `Qty`).
        let qty = Qty::from_raw(self.qty[i]).unwrap_or(Qty::ZERO);
        Level::new(Px::from_raw(self.px[i]), qty)
    }

    /// Where `px` is (`Ok(index)`), or where it would go (`Err(index)`). A binary search:
    /// on whole updates it beat a linear scan from the best end, a branchless binary
    /// search and a hybrid of the two (see `docs/DESIGN.md`).
    #[inline]
    fn find(&self, px: i64) -> Result<usize, usize> {
        self.px[..self.len].binary_search_by(|&p| {
            if p == px {
                Ordering::Equal
            } else if Self::better(px, p) {
                Ordering::Less // worse than px: sorts before it
            } else {
                Ordering::Greater
            }
        })
    }

    /// Sets the quantity at `px`; zero removes the level.
    fn set(&mut self, px: i64, qty: i64) -> LevelChange {
        match (self.find(px), qty == 0) {
            (Ok(i), false) => {
                self.qty[i] = qty;
                LevelChange::Updated
            }
            (Ok(i), true) => {
                self.remove_at(i);
                LevelChange::Removed
            }
            (Err(_), true) => LevelChange::RemovedAbsent,
            (Err(pos), false) => self.insert_at(pos, px, qty),
        }
    }

    fn remove_at(&mut self, i: usize) {
        // Close the gap by moving the better levels (those above i) down one place.
        self.px.copy_within(i + 1..self.len, i);
        self.qty.copy_within(i + 1..self.len, i);
        self.len -= 1;
    }

    fn insert_at(&mut self, pos: usize, px: i64, qty: i64) -> LevelChange {
        if self.len < CAP {
            // Open a gap at `pos` by moving the better levels up one place.
            self.px.copy_within(pos..self.len, pos + 1);
            self.qty.copy_within(pos..self.len, pos + 1);
            self.px[pos] = px;
            self.qty[pos] = qty;
            self.len += 1;
            LevelChange::Inserted
        } else if pos == 0 {
            LevelChange::OutOfDepth
        } else {
            // Full: drop the worst level (index 0) by moving the levels below `pos` down,
            // which frees slot `pos - 1` for the new level. Only the worse part moves.
            self.px.copy_within(1..pos, 0);
            self.qty.copy_within(1..pos, 0);
            self.px[pos - 1] = px;
            self.qty[pos - 1] = qty;
            LevelChange::Inserted
        }
    }

    /// Checks a best-first snapshot without changing anything.
    fn validate_snapshot(levels: &[Level]) -> Result<(), SnapshotError> {
        for (i, level) in levels.iter().enumerate() {
            if level.qty.is_zero() {
                return Err(SnapshotError::ZeroQty(Self::SIDE));
            }
            if i > 0 && !Self::better(levels[i - 1].px.raw(), level.px.raw()) {
                return Err(SnapshotError::Unsorted(Self::SIDE));
            }
        }
        Ok(())
    }

    /// Replaces the side with an already-validated best-first snapshot: one linear copy.
    /// Beyond `CAP` levels, the worst ones are dropped.
    fn load_snapshot(&mut self, levels: &[Level]) {
        let n = levels.len().min(CAP);
        for (k, level) in levels[..n].iter().enumerate() {
            // Best-first input → best goes to index n - 1.
            self.px[n - 1 - k] = level.px.raw();
            self.qty[n - 1 - k] = level.qty.raw();
        }
        self.len = n;
    }
}

/// An order book for one instrument on one venue.
///
/// `CAP` is the number of levels kept per side (default 64, above Bybit's 50 and
/// Binance's 20).
///
/// ```
/// use ob_core::{InstrumentSpec, Level, OrderBook, Side};
/// let s = InstrumentSpec::from_steps("0.1", "0.001").unwrap();
/// let lvl = |p: &str, q: &str| Level::new(s.parse_px(p).unwrap(), s.parse_qty(q).unwrap());
///
/// let mut book: OrderBook = OrderBook::new();
/// book.apply(Side::Bid, lvl("50000.0", "1.5"));
/// book.apply(Side::Ask, lvl("50000.5", "2"));
/// assert_eq!(book.best_bid(), Some(lvl("50000.0", "1.5")));
/// assert!(!book.is_crossed());
/// book.apply(Side::Bid, lvl("50000.0", "0")); // zero quantity removes the level
/// assert_eq!(book.best_bid(), None);
/// ```
#[derive(Clone, Debug)]
pub struct OrderBook<const CAP: usize = 64> {
    bids: Ladder<CAP, true>,
    asks: Ladder<CAP, false>,
}

impl<const CAP: usize> BookLevels for OrderBook<CAP> {
    #[inline]
    fn bid_levels(&self) -> impl Iterator<Item = Level> + '_ {
        self.bids.iter()
    }
    #[inline]
    fn ask_levels(&self) -> impl Iterator<Item = Level> + '_ {
        self.asks.iter()
    }
}

impl<const CAP: usize> Default for OrderBook<CAP> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const CAP: usize> OrderBook<CAP> {
    /// An empty book.
    pub const fn new() -> Self {
        OrderBook {
            bids: Ladder::new(),
            asks: Ladder::new(),
        }
    }

    /// Applies one level update: sets the quantity at that price, or removes the level
    /// if the quantity is zero.
    #[inline]
    pub fn apply(&mut self, side: Side, level: Level) -> LevelChange {
        let (px, qty) = (level.px.raw(), level.qty.raw());
        match side {
            Side::Bid => self.bids.set(px, qty),
            Side::Ask => self.asks.set(px, qty),
        }
    }

    /// Replaces the whole book with a snapshot. Each side must be strictly best-first
    /// (bids high → low, asks low → high) with no zero quantities, which is how venues
    /// send them.
    ///
    /// Both sides are validated **before** anything changes, so a bad snapshot leaves
    /// the book exactly as it was.
    pub fn replace(&mut self, bids: &[Level], asks: &[Level]) -> Result<(), SnapshotError> {
        Ladder::<CAP, true>::validate_snapshot(bids)?;
        Ladder::<CAP, false>::validate_snapshot(asks)?;
        self.bids.load_snapshot(bids);
        self.asks.load_snapshot(asks);
        Ok(())
    }

    /// Removes every level. O(1): only the lengths are reset.
    #[inline]
    pub fn clear(&mut self) {
        self.bids.len = 0;
        self.asks.len = 0;
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

    /// The bid side, for depth queries.
    #[inline]
    pub fn bids(&self) -> &Ladder<CAP, true> {
        &self.bids
    }

    /// The ask side, for depth queries.
    #[inline]
    pub fn asks(&self) -> &Ladder<CAP, false> {
        &self.asks
    }

    /// `true` if the best bid is at or above the best ask.
    ///
    /// On a single venue that can't persist (the orders would trade), so it means our
    /// copy of the book is wrong: a missed update, or stale data. Equal prices ("locked")
    /// count as crossed for the same reason. A book with an empty side is not crossed.
    #[inline]
    pub fn is_crossed(&self) -> bool {
        match (self.bids.best(), self.asks.best()) {
            (Some(bid), Some(ask)) => bid.px >= ask.px,
            _ => false,
        }
    }
}

/// What sweeping one side of a book would cost: the "market impact" of an order
/// ([`OrderBook::market_impact`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Impact {
    /// Quantity the visible book can fill (≤ the requested quantity).
    pub filled: Qty,
    /// Σ price × quantity over the fill, in price units × quantity units.
    pub notional: crate::Notional,
    /// Best price on that side before the order (`None`: empty side).
    pub best_px: Option<Px>,
    /// The last (worst) price the order reaches.
    pub worst_px: Option<Px>,
    /// Levels consumed, fully or partly.
    pub levels: usize,
    /// `true` if the visible book filled the whole quantity.
    pub complete: bool,
    /// Which side was consumed (asks for a buy, bids for a sell).
    pub took: Side,
}

impl Impact {
    /// Volume-weighted average price, rounded against the taker (up for a buy, down for
    /// a sell). `None` if nothing fills.
    pub fn vwap(&self) -> Option<Px> {
        let q = i128::from(self.filled.raw());
        if q == 0 {
            return None;
        }
        let n = self.notional.raw();
        let avg = match self.took {
            Side::Ask => (n + q - 1).div_euclid(q),
            Side::Bid => n.div_euclid(q),
        };
        i64::try_from(avg).ok().map(Px::from_raw)
    }

    /// How far the average price is from the best price, in price units (≥ 0): the cost
    /// of the order's own size.
    pub fn slippage(&self) -> Option<i64> {
        let (vwap, best) = (self.vwap()?.raw(), self.best_px?.raw());
        Some(match self.took {
            Side::Ask => vwap - best,
            Side::Bid => best - vwap,
        })
    }
}

impl<const CAP: usize> OrderBook<CAP> {
    /// The market impact of taking `qty` from side `take` (asks to buy, bids to sell):
    /// walks the levels best first until `qty` is filled or the visible book runs out.
    /// No allocation; integer arithmetic only. The Java skeleton's "calculate market
    /// impact" for a single venue (the aggregator does it across venues, with fees).
    pub fn market_impact(&self, take: Side, qty: Qty) -> Impact {
        match take {
            Side::Ask => sweep(self.asks().iter(), take, qty),
            Side::Bid => sweep(self.bids().iter(), take, qty),
        }
    }
}

/// Walks `levels` (best first) until `qty` is filled. Generic, so each side gets its own
/// inlined loop (no dynamic dispatch per level).
fn sweep(levels: impl Iterator<Item = Level>, took: Side, qty: Qty) -> Impact {
    let mut impact = Impact {
        filled: Qty::ZERO,
        notional: crate::Notional::from_raw(0),
        best_px: None,
        worst_px: None,
        levels: 0,
        complete: qty.is_zero(),
        took,
    };
    let mut left = qty.raw();
    for l in levels {
        if left == 0 {
            break;
        }
        impact.best_px.get_or_insert(l.px);
        let take_qty = left.min(l.qty.raw());
        left -= take_qty;
        impact.notional = crate::Notional::from_raw(
            impact.notional.raw() + i128::from(l.px.raw()) * i128::from(take_qty),
        );
        impact.worst_px = Some(l.px);
        impact.levels += 1;
    }
    impact.filled = Qty::from_raw(qty.raw() - left).unwrap_or(Qty::ZERO);
    impact.complete = left == 0;
    impact
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::InstrumentSpec;

    fn spec() -> InstrumentSpec {
        InstrumentSpec::from_steps("0.1", "0.001").unwrap()
    }
    fn lvl(px: &str, qty: &str) -> Level {
        let s = spec();
        Level::new(s.parse_px(px).unwrap(), s.parse_qty(qty).unwrap())
    }
    fn bids<const C: usize>(b: &OrderBook<C>) -> Vec<Level> {
        b.bids().iter().collect()
    }
    fn asks<const C: usize>(b: &OrderBook<C>) -> Vec<Level> {
        b.asks().iter().collect()
    }

    #[test]
    fn empty_book_has_no_best_and_is_not_crossed() {
        let b: OrderBook = OrderBook::new();
        assert_eq!((b.best_bid(), b.best_ask()), (None, None));
        assert!(!b.is_crossed());
        assert!(b.bids().is_empty() && b.asks().is_empty());
    }

    #[test]
    fn bids_sort_high_to_low_and_asks_low_to_high() {
        let mut b: OrderBook = OrderBook::new();
        for p in ["100.0", "102.0", "101.0", "99.5"] {
            assert_eq!(b.apply(Side::Bid, lvl(p, "1")), LevelChange::Inserted);
        }
        for p in ["105.0", "103.0", "104.0", "103.5"] {
            assert_eq!(b.apply(Side::Ask, lvl(p, "1")), LevelChange::Inserted);
        }
        let bid_px: Vec<_> = bids(&b)
            .iter()
            .map(|l| spec().fmt_px(l.px).to_string())
            .collect();
        let ask_px: Vec<_> = asks(&b)
            .iter()
            .map(|l| spec().fmt_px(l.px).to_string())
            .collect();
        assert_eq!(bid_px, ["102", "101", "100", "99.5"]);
        assert_eq!(ask_px, ["103", "103.5", "104", "105"]);
        assert_eq!(b.best_bid(), Some(lvl("102.0", "1")));
        assert_eq!(b.best_ask(), Some(lvl("103.0", "1")));
        assert_eq!(b.bids().level(2), Some(lvl("100.0", "1")));
        assert_eq!(b.bids().level(4), None);
    }

    #[test]
    fn zero_quantity_removes_the_level() {
        let mut b: OrderBook = OrderBook::new();
        b.apply(Side::Bid, lvl("100.0", "1"));
        b.apply(Side::Bid, lvl("101.0", "2"));
        b.apply(Side::Bid, lvl("99.0", "3"));
        assert_eq!(b.apply(Side::Bid, lvl("100.0", "0")), LevelChange::Removed);
        assert_eq!(bids(&b), [lvl("101", "2"), lvl("99", "3")]);
        // Removing the best promotes the next one.
        assert_eq!(b.apply(Side::Bid, lvl("101.0", "0")), LevelChange::Removed);
        assert_eq!(b.best_bid(), Some(lvl("99", "3")));
        // Removing a price that isn't there changes nothing.
        assert_eq!(
            b.apply(Side::Bid, lvl("123.4", "0")),
            LevelChange::RemovedAbsent
        );
        assert_eq!(bids(&b), [lvl("99", "3")]);
        assert_eq!(b.apply(Side::Bid, lvl("99.0", "0")), LevelChange::Removed);
        assert!(b.bids().is_empty());
    }

    #[test]
    fn repeated_updates_at_one_price_keep_the_last_quantity() {
        let mut b: OrderBook = OrderBook::new();
        assert_eq!(b.apply(Side::Ask, lvl("100.0", "1")), LevelChange::Inserted);
        assert_eq!(
            b.apply(Side::Ask, lvl("100.0", "2.5")),
            LevelChange::Updated
        );
        assert_eq!(
            b.apply(Side::Ask, lvl("100.0", "0.001")),
            LevelChange::Updated
        );
        assert_eq!(asks(&b), [lvl("100.0", "0.001")]);
        assert_eq!(b.asks().len(), 1);
        // Same price written in a different form is the same level.
        assert_eq!(b.apply(Side::Ask, lvl("100", "7")), LevelChange::Updated);
        assert_eq!(asks(&b), [lvl("100.0", "7")]);
    }

    #[test]
    fn crossed_and_locked_books_are_detected() {
        let mut b: OrderBook = OrderBook::new();
        b.apply(Side::Bid, lvl("100.0", "1"));
        assert!(!b.is_crossed(), "one side empty");
        b.apply(Side::Ask, lvl("100.5", "1"));
        assert!(!b.is_crossed(), "normal spread");
        b.apply(Side::Ask, lvl("100.0", "1"));
        assert!(b.is_crossed(), "locked: bid == ask");
        b.apply(Side::Ask, lvl("100.0", "0"));
        b.apply(Side::Bid, lvl("101.0", "1"));
        assert!(b.is_crossed(), "bid above ask");
        b.apply(Side::Bid, lvl("101.0", "0"));
        assert!(!b.is_crossed(), "fixed by removing the bad bid");
    }

    #[test]
    fn a_full_side_keeps_the_best_levels() {
        let mut b: OrderBook<3> = OrderBook::new();
        for p in ["100.0", "101.0", "102.0"] {
            b.apply(Side::Bid, lvl(p, "1"));
        }
        // Worse than everything held: not kept.
        assert_eq!(
            b.apply(Side::Bid, lvl("99.0", "1")),
            LevelChange::OutOfDepth
        );
        // Better than the worst: the worst (100) is dropped.
        assert_eq!(b.apply(Side::Bid, lvl("100.5", "1")), LevelChange::Inserted);
        assert_eq!(
            bids(&b),
            [lvl("102", "1"), lvl("101", "1"), lvl("100.5", "1")]
        );
        // New best when full.
        assert_eq!(b.apply(Side::Bid, lvl("103.0", "1")), LevelChange::Inserted);
        assert_eq!(
            bids(&b),
            [lvl("103", "1"), lvl("102", "1"), lvl("101", "1")]
        );
        // Updating and removing still work when full.
        assert_eq!(b.apply(Side::Bid, lvl("101.0", "9")), LevelChange::Updated);
        assert_eq!(b.apply(Side::Bid, lvl("102.0", "0")), LevelChange::Removed);
        assert_eq!(bids(&b), [lvl("103", "1"), lvl("101", "9")]);
        // Same on the ask side, where worse means higher.
        for p in ["10.0", "11.0", "12.0"] {
            b.apply(Side::Ask, lvl(p, "1"));
        }
        assert_eq!(
            b.apply(Side::Ask, lvl("13.0", "1")),
            LevelChange::OutOfDepth
        );
        assert_eq!(b.apply(Side::Ask, lvl("9.0", "1")), LevelChange::Inserted);
        assert_eq!(asks(&b), [lvl("9", "1"), lvl("10", "1"), lvl("11", "1")]);
    }

    #[test]
    fn snapshot_replaces_everything() {
        let mut b: OrderBook = OrderBook::new();
        b.apply(Side::Bid, lvl("1.0", "1"));
        b.apply(Side::Ask, lvl("999.0", "1"));
        let new_bids = [lvl("100.0", "1"), lvl("99.9", "2"), lvl("99.5", "3")];
        let new_asks = [lvl("100.1", "4"), lvl("100.2", "5")];
        b.replace(&new_bids, &new_asks).unwrap();
        assert_eq!(bids(&b), new_bids);
        assert_eq!(asks(&b), new_asks);
        // An empty snapshot empties the book.
        b.replace(&[], &[]).unwrap();
        assert!(b.bids().is_empty() && b.asks().is_empty());
    }

    #[test]
    fn a_bad_snapshot_is_refused_and_changes_nothing() {
        let mut b: OrderBook = OrderBook::new();
        let good_bids = [lvl("100.0", "1"), lvl("99.0", "1")];
        let good_asks = [lvl("101.0", "1")];
        b.replace(&good_bids, &good_asks).unwrap();

        let unsorted = [lvl("99.0", "1"), lvl("100.0", "1")];
        let duplicate = [lvl("100.0", "1"), lvl("100.0", "2")];
        let zero = [lvl("100.0", "0")];
        assert_eq!(
            b.replace(&unsorted, &good_asks),
            Err(SnapshotError::Unsorted(Side::Bid))
        );
        assert_eq!(
            b.replace(&duplicate, &good_asks),
            Err(SnapshotError::Unsorted(Side::Bid))
        );
        // Different, valid bids paired with invalid asks: if the bids were written before
        // the asks were checked, the book would now hold `other_bids`.
        let other_bids = [lvl("50.0", "5")];
        assert_eq!(
            b.replace(&other_bids, &zero),
            Err(SnapshotError::ZeroQty(Side::Ask))
        );
        // Asks must go low → high.
        let asks_high_to_low = [lvl("102.0", "1"), lvl("101.0", "1")];
        assert_eq!(
            b.replace(&other_bids, &asks_high_to_low),
            Err(SnapshotError::Unsorted(Side::Ask))
        );
        // Nothing from any refused snapshot was applied.
        assert_eq!(bids(&b), good_bids);
        assert_eq!(asks(&b), good_asks);
    }

    #[test]
    fn snapshot_deeper_than_capacity_keeps_the_best() {
        let mut b: OrderBook<2> = OrderBook::new();
        b.replace(&[lvl("100", "1"), lvl("99", "1"), lvl("98", "1")], &[])
            .unwrap();
        assert_eq!(bids(&b), [lvl("100", "1"), lvl("99", "1")]);
    }

    #[test]
    fn clear_empties_both_sides() {
        let mut b: OrderBook = OrderBook::new();
        b.apply(Side::Bid, lvl("100", "1"));
        b.apply(Side::Ask, lvl("101", "1"));
        b.clear();
        assert_eq!((b.best_bid(), b.best_ask()), (None, None));
        // Still fully usable afterwards.
        b.apply(Side::Ask, lvl("101", "2"));
        assert_eq!(b.best_ask(), Some(lvl("101", "2")));
    }

    #[test]
    fn any_insert_order_gives_the_same_book() {
        // A light check against a sorted reference (tests/model.rs has the full one).
        let prices: Vec<i64> = (0..40).map(|i| (i * 37) % 101 + 1000).collect();
        let mut b: OrderBook = OrderBook::new();
        for &p in &prices {
            b.apply(
                Side::Bid,
                Level::new(Px::from_raw(p), Qty::from_raw(p).unwrap()),
            );
            b.apply(
                Side::Ask,
                Level::new(Px::from_raw(p), Qty::from_raw(p).unwrap()),
            );
        }
        let mut sorted = prices.clone();
        sorted.sort_unstable();
        sorted.dedup();
        let bid_px: Vec<i64> = b.bids().iter().map(|l| l.px.raw()).collect();
        let ask_px: Vec<i64> = b.asks().iter().map(|l| l.px.raw()).collect();
        assert_eq!(ask_px, sorted);
        sorted.reverse();
        assert_eq!(bid_px, sorted);
    }

    #[test]
    fn layout_is_compact_and_cache_aligned() {
        // 64 prices (512 B) + 64 quantities (512 B) + length, padded to a cache line.
        assert_eq!(size_of::<Ladder<64, true>>(), 1088);
        assert_eq!(align_of::<Ladder<64, true>>(), 64);
        assert_eq!(size_of::<OrderBook<64>>(), 2176);
        assert_eq!(size_of::<Level>(), 16);
    }

    #[test]
    fn market_impact_walks_levels_best_first() {
        let mut b: OrderBook = OrderBook::new();
        b.replace(
            &[lvl("99.0", "1"), lvl("98.0", "2")],
            &[lvl("100.0", "1"), lvl("101.0", "2"), lvl("102.0", "5")],
        )
        .unwrap();
        let q = |s: &str| spec().parse_qty(s).unwrap();
        // Buy 2: 1 @ 100 + 1 @ 101 → average 100.5, slippage 0.5, two levels.
        let i = b.market_impact(Side::Ask, q("2"));
        assert!(i.complete);
        assert_eq!((i.filled, i.levels), (q("2"), 2));
        assert_eq!(i.worst_px, Some(spec().parse_px("101.0").unwrap()));
        assert_eq!(i.vwap(), Some(spec().parse_px("100.5").unwrap()));
        assert_eq!(i.slippage(), Some(spec().parse_px("0.5").unwrap().raw()));
        // Sell 10: the visible bids hold only 3 → partial, average rounded down.
        let i = b.market_impact(Side::Bid, q("10"));
        assert!(!i.complete);
        assert_eq!((i.filled, i.levels), (q("3"), 2));
        assert_eq!(i.vwap(), Some(spec().parse_px("98.3").unwrap())); // 98.333… down
        // Nothing to do, and an empty side.
        assert!(b.market_impact(Side::Ask, Qty::ZERO).complete);
        let empty: OrderBook = OrderBook::new();
        let i = empty.market_impact(Side::Ask, q("1"));
        assert_eq!((i.filled, i.vwap(), i.complete), (Qty::ZERO, None, false));
    }
}
