//! Publishing a venue's order book to other threads through a [`seqlock`](crate::seqlock).
//!
//! The feed thread owns its `OrderBook` and updates it privately (fast, plain memory).
//! After each message it calls [`BookPublisher::publish`], which copies the top
//! [`DEPTH`] levels into the seqlock in one write. Readers on other threads get a
//! consistent copy, either the top of book ([`BookReader::top`], one cache line) or the
//! full depth ([`BookReader::read_into`]), and never block the writer.
//!
//! This is also what makes updates **atomic per message** for readers: a reader sees the
//! book before a message or after it, never half way through (which the Java skeleton's
//! one-level-at-a-time `handleOrder` couldn't guarantee).
//!
//! ## Word layout (chosen for the hot read)
//! ```text
//! cache line 0: [seq][head][best bid px][best bid qty][best ask px][best ask qty][exchange ts][recv ns]
//! then:         [update id], bids 1..DEPTH (px, qty pairs), asks 1..DEPTH (px, qty pairs)
//! ```
//! `head` packs the two side lengths and the venue's [`VenueStatus`]. `seq` plus the first
//! 7 words are exactly 64 bytes, so reading the top of book together with the venue's
//! health and freshness (and checking it's consistent) touches **one cache line**.

use ob_core::{BookLevels, Level, Px, Qty};

use crate::seqlock::{Reader, Writer, seqlock};

/// Levels per side that can be published.
///
/// Was 64 (`OrderBook`'s default capacity) until step 19: Binance full-depth mode keeps
/// ~1,000 levels a side, and the aggregator needs more than 64 of them to price large
/// orders (64 ticks of 0.1 is ~$6 of book, which sometimes held under 11 BTC; 25 BTC
/// orders came out partial). Raising it costs memory only: the seqlock holds 8 KiB per
/// venue, but writers and readers touch just the levels a book actually has, so the
/// 20- and 50-level venues pay nothing.
pub const DEPTH: usize = 256;

const HEAD: usize = 0; // n_bids | n_asks << 16 | status << 32
const BID0: usize = 1; // best bid: px, qty
const ASK0: usize = 3; // best ask: px, qty
const EXCHANGE_TS: usize = 5;
const RECV_NS: usize = 6;
const UPDATE_ID: usize = 7; // second cache line: not needed for the top of book
const BIDS: usize = 8; // bid levels 1..DEPTH
const ASKS: usize = BIDS + 2 * (DEPTH - 1); // ask levels 1..DEPTH
/// Number of words in the published book (8,224 bytes).
pub const WORDS: usize = ASKS + 2 * (DEPTH - 1);

/// The feed's view of its venue, published with every book so readers get levels and
/// health in one consistent read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum VenueStatus {
    /// Not connected, or nothing published yet. A fresh (all-zero) book reads as `Down`.
    #[default]
    Down = 0,
    /// Connected, building or rebuilding the book (e.g. waiting for a snapshot after a
    /// gap). The levels must not be used.
    Syncing = 1,
    /// The book is believed correct.
    Live = 2,
}

impl VenueStatus {
    const fn from_bits(bits: u64) -> VenueStatus {
        match bits {
            1 => VenueStatus::Syncing,
            2 => VenueStatus::Live,
            _ => VenueStatus::Down, // includes torn garbage: safe default
        }
    }
}

/// Word index of the price of level `i` (0 = best) on one side. Quantity follows it.
#[inline(always)]
const fn bid_at(i: usize) -> usize {
    if i == 0 { BID0 } else { BIDS + 2 * (i - 1) }
}
#[inline(always)]
const fn ask_at(i: usize) -> usize {
    if i == 0 { ASK0 } else { ASKS + 2 * (i - 1) }
}

/// What a publisher knows about the update besides the levels.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BookMeta {
    /// The venue's update ID for the last applied message (Binance `u`, Bybit `u`).
    pub update_id: u64,
    /// The venue's timestamp for it, ms since the epoch (Binance `E`, Bybit `ts`).
    pub exchange_ts: u64,
    /// When *we* received it, in ns on the local monotonic clock. Used for staleness:
    /// comparing local times avoids trusting the exchange's clock.
    pub recv_ns: u64,
    /// The feed's state for this venue.
    pub status: VenueStatus,
}

/// A fresher best bid and ask than the book's own, from a venue's real-time top-of-book
/// stream (Binance `bookTicker`, Bybit `orderbook.1`; step 18).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TopOverlay {
    /// Best bid.
    pub bid: Level,
    /// Best ask.
    pub ask: Level,
}

/// Creates a publisher (for the feed thread) and a reader (clone it for each reader).
pub fn book_channel() -> (BookPublisher, BookReader) {
    let (w, r) = seqlock::<WORDS>();
    (BookPublisher { w }, BookReader { r })
}

/// The writing side, owned by one feed thread.
pub struct BookPublisher {
    w: Writer<WORDS>,
}

impl BookPublisher {
    /// Publishes the top [`DEPTH`] levels of `book`, atomically for readers.
    #[inline]
    pub fn publish<B: BookLevels>(&mut self, book: &B, meta: BookMeta) {
        self.publish_with_top(book, None, meta);
    }

    /// Publishes `book` with a fresher top of book laid over it (step 18).
    ///
    /// Per side, readers see the overlay's level first, then only the book's levels that
    /// are strictly *worse* (bids below the overlay bid, asks above the overlay ask). The
    /// book's levels at or better than the overlay's price come from an older update:
    /// they have since been traded through or cancelled (or, at the same price, their
    /// quantity changed), so they are dropped. Deeper levels are kept: the overlay says
    /// nothing about them, and they are the best information there is.
    ///
    /// `book` itself is not modified, so a delta-based book (Bybit) keeps applying
    /// deltas to the exact state the venue described; the overlay exists only in what
    /// readers see. No copy is made: the merge happens while writing into the seqlock.
    #[inline]
    pub fn publish_with_top<B: BookLevels>(
        &mut self,
        book: &B,
        top: Option<TopOverlay>,
        meta: BookMeta,
    ) {
        let (top_bid, top_ask) = (top.map(|t| t.bid), top.map(|t| t.ask));
        // Book levels to keep: those strictly worse than the overlay (all, without one).
        let bids_kept = book
            .bid_levels()
            .skip_while(move |l| top_bid.is_some_and(|t| l.px >= t.px));
        let asks_kept = book
            .ask_levels()
            .skip_while(move |l| top_ask.is_some_and(|t| l.px <= t.px));
        let bids = top_bid.into_iter().chain(bids_kept).take(DEPTH);
        let asks = top_ask.into_iter().chain(asks_kept).take(DEPTH);
        self.w.write(|g| {
            let mut nb = 0_u64;
            for (i, l) in bids.enumerate() {
                g.set(bid_at(i), l.px.raw().cast_unsigned());
                g.set(bid_at(i) + 1, l.qty.raw().cast_unsigned());
                nb += 1;
            }
            let mut na = 0_u64;
            for (i, l) in asks.enumerate() {
                g.set(ask_at(i), l.px.raw().cast_unsigned());
                g.set(ask_at(i) + 1, l.qty.raw().cast_unsigned());
                na += 1;
            }
            g.set(HEAD, nb | na << 16 | (meta.status as u64) << 32);
            g.set(UPDATE_ID, meta.update_id);
            g.set(EXCHANGE_TS, meta.exchange_ts);
            g.set(RECV_NS, meta.recv_ns);
        });
    }

    /// The current version (grows by 2 per publish).
    pub fn version(&self) -> u64 {
        self.w.version()
    }
}

/// Best bid and ask with the venue's health, as one consistent read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Top {
    /// Best bid, if any.
    pub bid: Option<Level>,
    /// Best ask, if any.
    pub ask: Option<Level>,
    /// The feed's status when it published this.
    pub status: VenueStatus,
    /// Exchange timestamp of the last message (ms).
    pub exchange_ts: u64,
    /// Local receive time of the last message (ns, monotonic).
    pub recv_ns: u64,
    /// Seqlock version of this publish (0 = nothing published yet).
    pub version: u64,
}

/// A full consistent copy of a published book. Reuse one per reader: no allocation.
#[derive(Clone, Debug)]
pub struct Snapshot {
    bids: [Level; DEPTH],
    asks: [Level; DEPTH],
    n_bids: usize,
    n_asks: usize,
    /// Everything published with the levels: update ID, times, status.
    pub meta: BookMeta,
    /// Seqlock version of this publish.
    pub version: u64,
}

impl Default for Snapshot {
    fn default() -> Self {
        let empty = Level::new(Px::ZERO, Qty::ZERO);
        Snapshot {
            bids: [empty; DEPTH],
            asks: [empty; DEPTH],
            n_bids: 0,
            n_asks: 0,
            meta: BookMeta::default(),
            version: 0,
        }
    }
}

impl Snapshot {
    /// Bids, best first.
    pub fn bids(&self) -> &[Level] {
        &self.bids[..self.n_bids]
    }
    /// Asks, best first.
    pub fn asks(&self) -> &[Level] {
        &self.asks[..self.n_asks]
    }
}

/// The reading side. Clone one per thread.
#[derive(Clone)]
pub struct BookReader {
    r: Reader<WORDS>,
}

/// Rebuilds a level from two raw words. In a discarded (torn) attempt the quantity could
/// be anything, so a negative value becomes zero instead of panicking; in an accepted
/// read it is always the non-negative quantity that was published.
#[inline(always)]
fn level(px: u64, qty: u64) -> Level {
    Level::new(
        Px::from_raw(px.cast_signed()),
        Qty::from_raw(qty.cast_signed()).unwrap_or(Qty::ZERO),
    )
}

/// Splits the head word into the two lengths and the status, clamping torn values so
/// they can't index out of range.
#[inline(always)]
fn head(word: u64) -> (usize, usize, VenueStatus) {
    let clamp = |n: u64| usize::try_from(n & 0xFFFF).unwrap_or(DEPTH).min(DEPTH);
    (
        clamp(word),
        clamp(word >> 16),
        VenueStatus::from_bits(word >> 32 & 0xFF),
    )
}

impl BookReader {
    /// Best bid and ask. Reads one cache line.
    #[inline]
    pub fn top(&self) -> Top {
        let read = self.r.read_versioned(|g| {
            (
                g.get(HEAD),
                g.get(BID0),
                g.get(BID0 + 1),
                g.get(ASK0),
                g.get(ASK0 + 1),
                g.get(EXCHANGE_TS),
                g.get(RECV_NS),
            )
        });
        let (h, bp, bq, ap, aq, exchange_ts, recv_ns) = read.value;
        let (nb, na, status) = head(h);
        Top {
            bid: (nb > 0).then(|| level(bp, bq)),
            ask: (na > 0).then(|| level(ap, aq)),
            status,
            exchange_ts,
            recv_ns,
            version: read.version, // the version this data belongs to
        }
    }

    /// Copies the whole published book into `out` (reused; nothing is allocated).
    /// Returns how many attempts were discarded because the writer was mid-publish.
    #[inline]
    pub fn read_into(&self, out: &mut Snapshot) -> u32 {
        let read = self.r.read_versioned(|g| {
            // `out` is a scratch buffer here: every attempt overwrites what it uses, and
            // only the final, consistent attempt is kept.
            let (nb, na, status) = head(g.get(HEAD));
            for (i, slot) in out.bids[..nb].iter_mut().enumerate() {
                *slot = level(g.get(bid_at(i)), g.get(bid_at(i) + 1));
            }
            for (i, slot) in out.asks[..na].iter_mut().enumerate() {
                *slot = level(g.get(ask_at(i)), g.get(ask_at(i) + 1));
            }
            out.n_bids = nb;
            out.n_asks = na;
            out.meta = BookMeta {
                update_id: g.get(UPDATE_ID),
                exchange_ts: g.get(EXCHANGE_TS),
                recv_ns: g.get(RECV_NS),
                status,
            };
        });
        out.version = read.version; // the version this copy belongs to
        read.retries
    }

    /// The current version: compare with a previous [`Top::version`] or
    /// [`Snapshot::version`] to skip work when nothing changed.
    #[inline]
    pub fn version(&self) -> u64 {
        self.r.version()
    }
}

#[cfg(all(test, not(loom)))]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use ob_core::OrderBook;
    use ob_core::Side;

    fn lvl(px: i64, qty: i64) -> Level {
        Level::new(Px::from_raw(px), Qty::from_raw(qty).unwrap())
    }

    #[test]
    fn layout_puts_the_top_of_book_in_one_cache_line() {
        // seq (word -1) + words 0..=6 = 8 words = 64 bytes. Checked at compile time.
        const _: () = assert!(HEAD < 7 && ASK0 + 1 < 7 && EXCHANGE_TS < 7 && RECV_NS < 7);
        assert_eq!(WORDS, 1028); // 8 + 2 sides × 255 levels × 2 words (DEPTH = 256, step 19)
        // Every level of both sides has its own pair of words.
        let mut used = std::collections::BTreeSet::new();
        for i in 0..DEPTH {
            for w in [bid_at(i), bid_at(i) + 1, ask_at(i), ask_at(i) + 1] {
                assert!(w < WORDS && ![HEAD, UPDATE_ID, EXCHANGE_TS, RECV_NS].contains(&w));
                assert!(used.insert(w), "word {w} used twice");
            }
        }
    }

    #[test]
    fn publish_then_read_round_trips() {
        let (mut p, r) = book_channel();
        assert_eq!(r.top(), Top::default());
        let mut book: OrderBook = OrderBook::new();
        book.replace(
            &[lvl(100, 1), lvl(99, 2), lvl(98, 3)],
            &[lvl(101, 4), lvl(102, 5)],
        )
        .unwrap();
        let meta = BookMeta {
            update_id: 42,
            exchange_ts: 7,
            recv_ns: 9,
            status: VenueStatus::Live,
        };
        p.publish(&book, meta);
        let top = r.top();
        assert_eq!((top.bid, top.ask), (Some(lvl(100, 1)), Some(lvl(101, 4))));
        assert_eq!(
            (top.status, top.exchange_ts, top.recv_ns, top.version),
            (VenueStatus::Live, 7, 9, 2)
        );
        let mut snap = Snapshot::default();
        assert_eq!(r.read_into(&mut snap), 0);
        assert_eq!(snap.meta, meta);
        assert_eq!(snap.bids(), [lvl(100, 1), lvl(99, 2), lvl(98, 3)]);
        assert_eq!(snap.asks(), [lvl(101, 4), lvl(102, 5)]);
        // A smaller book later: no stale levels survive in the copy.
        book.apply(Side::Bid, lvl(99, 0));
        book.apply(Side::Bid, lvl(98, 0));
        book.apply(Side::Ask, lvl(101, 0));
        p.publish(
            &book,
            BookMeta {
                update_id: 43,
                status: VenueStatus::Syncing,
                ..meta
            },
        );
        r.read_into(&mut snap);
        assert_eq!(
            (snap.bids(), snap.asks()),
            (&[lvl(100, 1)][..], &[lvl(102, 5)][..])
        );
        assert_eq!((snap.version, snap.meta.status), (4, VenueStatus::Syncing));
    }

    #[test]
    fn an_unpublished_book_reads_as_down() {
        let (_p, r) = book_channel();
        assert_eq!(r.top().status, VenueStatus::Down);
        assert_eq!(VenueStatus::default(), VenueStatus::Down);
    }

    #[test]
    fn empty_sides_read_as_none() {
        let (mut p, r) = book_channel();
        let mut book: OrderBook = OrderBook::new();
        book.apply(Side::Ask, lvl(101, 4));
        p.publish(&book, BookMeta::default());
        let top = r.top();
        assert_eq!((top.bid, top.ask), (None, Some(lvl(101, 4))));
    }

    #[test]
    fn a_full_book_publishes_every_level() {
        let (mut p, r) = book_channel();
        let bids: Vec<Level> = (0..64).map(|i| lvl(1000 - i, i + 1)).collect();
        let asks: Vec<Level> = (0..64).map(|i| lvl(1001 + i, i + 1)).collect();
        let mut book: OrderBook = OrderBook::new();
        book.replace(&bids, &asks).unwrap();
        p.publish(&book, BookMeta::default());
        let mut snap = Snapshot::default();
        r.read_into(&mut snap);
        assert_eq!((snap.bids(), snap.asks()), (&bids[..], &asks[..]));
    }

    #[test]
    fn torn_values_are_clamped_not_trusted() {
        // What a discarded attempt might see: absurd lengths and negative quantities.
        assert_eq!(head(u64::MAX), (DEPTH, DEPTH, VenueStatus::Down));
        assert_eq!(head(3 | 4 << 16 | 2 << 32), (3, 4, VenueStatus::Live));
        assert_eq!(level(5, (-3_i64).cast_unsigned()).qty, Qty::ZERO);
    }
}
