//! Fixed-point prices, quantities and notionals.
//!
//! Each value is a plain integer. What one unit of that integer means (0.1 USDT? 10^-8
//! BTC?) is defined by the instrument's [`InstrumentSpec`](crate::InstrumentSpec), not
//! by these types. For BTCUSDT (tick 0.1, lot 0.001), `50000.1` is stored as `500001`
//! and `1.234` BTC as `1234`.
//!
//! Comparing two prices is one integer compare, a price is 8 bytes, and nothing
//! allocates. Converting to and from text needs the scale, so it lives on
//! [`InstrumentSpec`](crate::InstrumentSpec); `Debug` shows the raw integer.
//!
//! Values of different instruments must not be mixed. Every book and aggregator handles
//! exactly one instrument, so they never meet in practice.

use core::ops::{Add, AddAssign, Sub, SubAssign};

/// A price, in raw units of the instrument's price scale.
///
/// Signed so that differences between prices (spreads, cross-venue edges) can be
/// negative without wrapping; book parsers decide whether a negative price is valid.
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Debug)]
pub struct Px(i64);

/// A quantity, in raw units of the instrument's quantity scale. Never negative.
///
/// Stored as `i64`, not `u64`: the "never negative" rule is enforced by this type's
/// constructors and checked arithmetic, and a signed representation means a bug shows
/// up as an impossible negative value instead of a plausible huge one.
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Debug)]
pub struct Qty(i64);

/// Price × quantity, in raw units whose decimals are the sum of the price and quantity
/// decimals.
///
/// `i128`, because the product of any two `i64` values fits in it exactly.
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Debug)]
pub struct Notional(i128);

// The whole point of these types is that they cost exactly as much as the integer.
const _: () = assert!(size_of::<Px>() == 8 && size_of::<Option<Px>>() == 16);
const _: () = assert!(size_of::<Qty>() == 8 && size_of::<Notional>() == 16);

impl Px {
    /// The price 0.
    pub const ZERO: Px = Px(0);

    /// Wraps a raw value already in the instrument's price units.
    #[inline]
    pub const fn from_raw(raw: i64) -> Px {
        Px(raw)
    }

    /// The raw value in the instrument's price units.
    #[inline]
    pub const fn raw(self) -> i64 {
        self.0
    }

    /// Notional value of `qty` at this price. Exact: no rounding, cannot overflow.
    #[inline]
    pub const fn notional(self, qty: Qty) -> Notional {
        Notional(self.0 as i128 * qty.0 as i128)
    }
}

impl Qty {
    /// The quantity 0. In an update, a zero quantity means "remove this price level".
    pub const ZERO: Qty = Qty(0);

    /// The largest representable quantity.
    pub const MAX: Qty = Qty(i64::MAX);

    /// Wraps a raw value already in the instrument's quantity units.
    ///
    /// Returns `None` for a negative value: a negative quantity is always a bug or bad data.
    #[inline]
    pub const fn from_raw(raw: i64) -> Option<Qty> {
        if raw < 0 { None } else { Some(Qty(raw)) }
    }

    /// The raw value in the instrument's quantity units.
    #[inline]
    pub const fn raw(self) -> i64 {
        self.0
    }

    /// `true` for the zero quantity.
    #[inline]
    pub const fn is_zero(self) -> bool {
        self.0 == 0
    }

    /// `self + rhs`, or `None` on overflow.
    #[inline]
    pub const fn checked_add(self, rhs: Qty) -> Option<Qty> {
        match self.0.checked_add(rhs.0) {
            Some(v) => Some(Qty(v)),
            None => None,
        }
    }

    /// `self - rhs`, or `None` if the result would be negative.
    #[inline]
    pub const fn checked_sub(self, rhs: Qty) -> Option<Qty> {
        // Both are non-negative, so the subtraction itself cannot overflow.
        Qty::from_raw(self.0 - rhs.0)
    }
}

/// `+` that is checked in **every** build. Real volumes are far below the limit, so
/// overflow here means a bug; it panics, and the release build's `panic = "abort"` stops
/// the process rather than let a corrupted quantity reach routing.
impl Add for Qty {
    type Output = Qty;
    #[inline]
    fn add(self, rhs: Qty) -> Qty {
        match self.checked_add(rhs) {
            Some(q) => q,
            None => arithmetic_bug("Qty overflow"),
        }
    }
}

impl AddAssign for Qty {
    #[inline]
    fn add_assign(&mut self, rhs: Qty) {
        *self = *self + rhs;
    }
}

/// `-` that is checked in every build: going below zero is a logic error and panics.
/// Callers that cannot rule it out use [`Qty::checked_sub`] and handle `None`.
impl Sub for Qty {
    type Output = Qty;
    #[inline]
    fn sub(self, rhs: Qty) -> Qty {
        match self.checked_sub(rhs) {
            Some(q) => q,
            None => arithmetic_bug("Qty underflow"),
        }
    }
}

impl SubAssign for Qty {
    #[inline]
    fn sub_assign(&mut self, rhs: Qty) {
        *self = *self - rhs;
    }
}

impl Notional {
    /// Wraps a raw value already in notional units.
    #[inline]
    pub const fn from_raw(raw: i128) -> Notional {
        Notional(raw)
    }

    /// The raw value in notional units.
    #[inline]
    pub const fn raw(self) -> i128 {
        self.0
    }

    /// `self + rhs`, or `None` on overflow.
    #[inline]
    pub const fn checked_add(self, rhs: Notional) -> Option<Notional> {
        match self.0.checked_add(rhs.0) {
            Some(v) => Some(Notional(v)),
            None => None,
        }
    }
}

/// `+` checked in every build, like [`Qty`]'s.
impl Add for Notional {
    type Output = Notional;
    #[inline]
    fn add(self, rhs: Notional) -> Notional {
        match self.checked_add(rhs) {
            Some(n) => n,
            None => arithmetic_bug("Notional overflow"),
        }
    }
}

impl AddAssign for Notional {
    #[inline]
    fn add_assign(&mut self, rhs: Notional) {
        *self = *self + rhs;
    }
}

/// Panic for a broken arithmetic invariant (a bug, never bad input data).
///
/// `#[cold]` tells the compiler this path is rare, so it moves it out of line and lays
/// out the normal path as straight-line code; `#[inline(never)]` keeps the panic
/// machinery out of every caller. The check left behind on the hot path is a single
/// compare-and-branch the CPU predicts correctly every time.
#[cold]
#[inline(never)]
#[track_caller]
fn arithmetic_bug(what: &'static str) -> ! {
    panic!("{what}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(raw: i64) -> Qty {
        Qty::from_raw(raw).unwrap_or_else(|| panic!("negative test quantity {raw}"))
    }

    #[test]
    fn quantities_cannot_be_negative() {
        assert_eq!(Qty::from_raw(-1), None);
        assert_eq!(q(1).checked_sub(q(2)), None);
        assert_eq!(q(2000).checked_sub(q(500)), Some(q(1500)));
        assert_eq!(q(1).checked_add(q(i64::MAX)), None);
    }

    // These run in `cargo test --release` too: the checks are not debug-only.
    #[test]
    #[should_panic(expected = "Qty overflow")]
    fn qty_add_overflow_panics_in_every_build() {
        let _ = q(i64::MAX) + q(1);
    }

    #[test]
    #[should_panic(expected = "Qty underflow")]
    fn qty_sub_below_zero_panics_in_every_build() {
        let _ = q(1) - q(2);
    }

    #[test]
    #[should_panic(expected = "Notional overflow")]
    fn notional_add_overflow_panics_in_every_build() {
        let _ = Notional::from_raw(i128::MAX) + Notional::from_raw(1);
    }

    #[test]
    fn ordering_matches_numeric_order() {
        assert!(Px::from_raw(500_001) > Px::from_raw(500_000));
        assert!(Px::from_raw(-1) < Px::ZERO);
        assert!(q(1) < q(10));
    }

    #[test]
    fn notional_is_exact_at_the_extremes() {
        let max = Px::from_raw(i64::MAX).notional(q(i64::MAX));
        assert_eq!(max.raw(), i128::from(i64::MAX) * i128::from(i64::MAX));
        let min = Px::from_raw(i64::MIN).notional(q(i64::MAX));
        assert_eq!(min.raw(), i128::from(i64::MIN) * i128::from(i64::MAX));
    }
}
