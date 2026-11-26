//! Result types: fixed capacity, so queries never allocate.

use core::ops::Deref;

use ob_core::{Level, Px, Qty};

/// The most venues one aggregator handles. Results are fixed arrays of this size.
pub const MAX_VENUES: usize = 8;

/// Identifies a venue within one [`crate::Aggregator`] (its index).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct VenueId(pub(crate) u8);

impl VenueId {
    /// Slots never exceed [`MAX_VENUES`] (8), so the index always fits a `u8`.
    pub(crate) fn from_index(i: usize) -> VenueId {
        debug_assert!(i < MAX_VENUES);
        VenueId(u8::try_from(i).unwrap_or(u8::MAX))
    }

    /// The index, 0-based, in the order venues were added.
    pub const fn index(self) -> usize {
        self.0 as usize
    }
}

/// A price and quantity offered on one venue (the Java skeleton's `Quote`, with a compact
/// venue ID instead of a `String`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Quote {
    /// Where.
    pub venue: VenueId,
    /// At what price.
    pub px: Px,
    /// How much.
    pub qty: Qty,
}

impl Quote {
    pub(crate) const fn new(venue: VenueId, level: Level) -> Quote {
        Quote {
            venue,
            px: level.px,
            qty: level.qty,
        }
    }
}

const NO_QUOTE: Quote = Quote {
    venue: VenueId(0),
    px: Px::ZERO,
    qty: Qty::ZERO,
};

/// Up to [`MAX_VENUES`] quotes, best first. Dereferences to `&[Quote]`.
#[derive(Clone, Copy, Debug)]
pub struct Quotes {
    items: [Quote; MAX_VENUES],
    len: usize,
}

impl Default for Quotes {
    fn default() -> Self {
        Quotes {
            items: [NO_QUOTE; MAX_VENUES],
            len: 0,
        }
    }
}

impl Quotes {
    /// Inserts keeping the order given by `better` (stable for equal keys). At most
    /// [`MAX_VENUES`] items, so insertion sort is the right tool: no allocation, a few
    /// compares.
    pub(crate) fn insert_sorted(&mut self, q: Quote, better: impl Fn(&Quote, &Quote) -> bool) {
        let Some(slot) = self.items.get_mut(self.len) else {
            return;
        };
        *slot = q;
        let mut i = self.len;
        while i > 0 && better(&self.items[i], &self.items[i - 1]) {
            self.items.swap(i, i - 1);
            i -= 1;
        }
        self.len += 1;
    }
}

impl Deref for Quotes {
    type Target = [Quote];
    fn deref(&self) -> &[Quote] {
        &self.items[..self.len]
    }
}

/// Sums quantities exactly: checked additions that stop at the first overflow. `None` if
/// the total doesn't fit a `Qty` (reported, never wrapped; impossible for real market
/// data).
#[inline]
pub(crate) fn sum_qty(qtys: impl IntoIterator<Item = Qty>) -> Option<Qty> {
    qtys.into_iter().try_fold(Qty::ZERO, Qty::checked_add)
}

/// One venue's volume available at a limit price or better.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct VenueVolume {
    /// Where.
    pub venue: VenueId,
    /// Total quantity at the limit or better, across the visible levels.
    pub qty: Qty,
    /// How many levels contributed.
    pub levels: usize,
    /// `true` if the visible book shows where the volume ends (a level beyond the limit,
    /// or the side has no more levels than the venue publishes). `false` if *every*
    /// visible level was within the limit, so the venue may have more volume deeper than
    /// it sends us (Binance publishes only 20 levels).
    pub complete: bool,
}

const NO_VOLUME: VenueVolume = VenueVolume {
    venue: VenueId(0),
    qty: Qty::ZERO,
    levels: 0,
    complete: true,
};

/// Volume at a limit price or better, per venue and in total.
#[derive(Clone, Copy, Debug)]
pub struct Volume {
    per_venue: [VenueVolume; MAX_VENUES],
    len: usize,
    /// Sum over venues. `None` only if it overflows a `Qty` (impossible for real
    /// quantities; reported rather than wrapped).
    pub total: Option<Qty>,
    /// `true` only if every venue's volume is [`VenueVolume::complete`].
    pub complete: bool,
}

impl Default for Volume {
    fn default() -> Self {
        Volume {
            per_venue: [NO_VOLUME; MAX_VENUES],
            len: 0,
            total: Some(Qty::ZERO),
            complete: true,
        }
    }
}

impl Volume {
    pub(crate) fn push(&mut self, v: VenueVolume) {
        if let Some(slot) = self.per_venue.get_mut(self.len) {
            *slot = v;
            self.len += 1;
        }
    }

    pub(crate) fn finish(&mut self) {
        self.total = sum_qty(self.iter().map(|v| v.qty));
        self.complete = self.iter().all(|v| v.complete);
    }
}

impl Deref for Volume {
    type Target = [VenueVolume];
    fn deref(&self) -> &[VenueVolume] {
        &self.per_venue[..self.len]
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn qty(raw: i64) -> Qty {
        Qty::from_raw(raw).unwrap()
    }

    #[test]
    fn quantity_sums_are_exact_and_overflow_is_reported() {
        assert_eq!(sum_qty([qty(1), qty(2), qty(3)]), Some(qty(6)));
        assert_eq!(sum_qty([Qty::MAX, Qty::ZERO]), Some(Qty::MAX));
        assert_eq!(sum_qty([Qty::MAX, qty(1)]), None, "reported, never wrapped");
        // MAX + MAX + 2 = 2^64 wraps to exactly 0 in i64, a valid-looking quantity: a
        // wrapping sum would return Some(0) here.
        assert_eq!(sum_qty([Qty::MAX, Qty::MAX, qty(2)]), None);
        assert_eq!(sum_qty([Qty::MAX; 64].iter().copied()), None);
    }
}
