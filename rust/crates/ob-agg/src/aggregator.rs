//! The cross-venue view.

use ob_core::{Level, Px, Qty};
use ob_sync::{BookReader, Snapshot, VenueStatus};

use crate::quotes::{MAX_VENUES, Quote, Quotes, VenueId, VenueVolume, Volume, sum_qty};

/// Static facts about a venue, given when it's added.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VenueConfig {
    /// Display name, e.g. `"binance"`.
    pub name: &'static str,
    /// How many levels per side the venue's stream carries (Binance `depth20` → 20,
    /// Bybit `orderbook.50` → 50). Used to tell whether visible volume is complete.
    pub depth: usize,
    /// Taker fee in parts per million of notional (Binance USDⓈ-M VIP 0: 0.0500% = 500;
    /// Bybit derivatives non-VIP: 0.0550% = 550). Set from your account's actual tier.
    /// Used by fee-aware routing and arbitrage; 0 means "prices only".
    pub taker_fee_ppm: u32,
}

/// Aggregator settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Config {
    /// A venue whose last message was received longer ago than this is excluded. Uses
    /// the local receive clock, not the exchange's.
    pub max_age_ns: u64,
    /// Two venues' books are only compared for arbitrage if their exchange timestamps
    /// are at most this far apart (books 15 s apart can show a "fake" cross).
    pub max_skew_ms: u64,
}

impl Default for Config {
    /// Max age 2 s: both venues publish every 20–120 ms on BTCUSDT, so 2 s of silence
    /// means something is wrong (a dead connection that hasn't been noticed yet).
    /// Max skew 150 ms: Binance conflates to 100 ms; allow that plus jitter.
    fn default() -> Self {
        Config {
            max_age_ns: 2_000_000_000,
            // Binance conflates to 100 ms; allow that plus network jitter.
            max_skew_ms: 150,
        }
    }
}

/// Why a venue isn't used right now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exclusion {
    /// The feed reports the venue down, or it never published.
    Down,
    /// The feed is rebuilding the book (e.g. after a sequence gap).
    Syncing,
    /// No message for longer than [`Config::max_age_ns`].
    Stale {
        /// How long since the last message, ns.
        age_ns: u64,
    },
    /// Best bid ≥ best ask on this single venue: our copy is wrong.
    Crossed,
}

/// Whether a venue is used, and if not, why.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VenueState {
    /// Its levels are used in every query.
    Included,
    /// Ignored, for this reason.
    Excluded(Exclusion),
}

struct Slot {
    config: VenueConfig,
    reader: BookReader,
    /// The latest consistent copy of the venue's book (refreshed by `refresh`).
    snap: Snapshot,
}

/// A read-only, cross-venue view of several venues' books.
///
/// **One per reading thread.** It holds a reader and a cached copy of each venue's book.
/// [`Aggregator::refresh`] copies only the venues that published since the last refresh;
/// queries then work on those copies without touching shared memory, so a series of
/// queries sees one stable picture.
///
/// **Time is an input.** Queries take `now_ns` (the local monotonic clock) to judge
/// staleness, so behaviour is deterministic and testable; the caller owns the clock.
///
/// Nothing allocates after the venues are added.
pub struct Aggregator {
    slots: Vec<Slot>,
    config: Config,
}

impl Aggregator {
    /// An aggregator with no venues.
    pub fn new(config: Config) -> Self {
        Aggregator {
            slots: Vec::with_capacity(MAX_VENUES),
            config,
        }
    }

    /// Adds a venue (at start-up). Returns its ID, or `None` if [`MAX_VENUES`] are already
    /// registered.
    pub fn add_venue(&mut self, config: VenueConfig, reader: BookReader) -> Option<VenueId> {
        let id = VenueId(
            u8::try_from(self.slots.len())
                .ok()
                .filter(|&i| usize::from(i) < MAX_VENUES)?,
        );
        self.slots.push(Slot {
            config,
            reader,
            snap: Snapshot::default(),
        });
        Some(id)
    }

    /// The venue's configuration (its name, depth).
    pub fn venue(&self, id: VenueId) -> Option<&VenueConfig> {
        self.slots.get(id.index()).map(|s| &s.config)
    }

    /// The settings this aggregator was created with.
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Changes a venue's taker fee (fee tiers change with volume; also used in tests).
    pub fn set_taker_fee(&mut self, id: VenueId, taker_fee_ppm: u32) {
        if let Some(slot) = self.slots.get_mut(id.index()) {
            slot.config.taker_fee_ppm = taker_fee_ppm;
        }
    }

    /// The IDs of all venues, in the order they were added.
    pub fn venue_ids(&self) -> impl Iterator<Item = VenueId> + use<> {
        (0..self.slots.len()).map(VenueId::from_index)
    }

    /// Number of venues.
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    /// `true` if no venues were added.
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// Brings the cached copies up to date: one atomic load per venue to compare versions,
    /// and a full copy only for venues that published since. Returns how many were copied.
    pub fn refresh(&mut self) -> usize {
        let mut copied = 0;
        for slot in &mut self.slots {
            if slot.reader.version() != slot.snap.version {
                slot.reader.read_into(&mut slot.snap);
                copied += 1;
            }
        }
        copied
    }

    /// The cached copy of a venue's book (as of the last [`Aggregator::refresh`]).
    pub fn snapshot(&self, id: VenueId) -> Option<&Snapshot> {
        self.slots.get(id.index()).map(|s| &s.snap)
    }

    /// Whether the venue is used at `now_ns`, and if not why. Checked in order: status,
    /// freshness, crossed.
    pub fn state(&self, id: VenueId, now_ns: u64) -> Option<VenueState> {
        self.slots
            .get(id.index())
            .map(|s| self.judge(&s.snap, now_ns))
    }

    fn judge(&self, snap: &Snapshot, now_ns: u64) -> VenueState {
        let excluded = match snap.meta.status {
            VenueStatus::Down => Some(Exclusion::Down),
            VenueStatus::Syncing => Some(Exclusion::Syncing),
            VenueStatus::Live => {
                // A receive time "in the future" (another thread's clock read) is age 0.
                let age_ns = now_ns.saturating_sub(snap.meta.recv_ns);
                if age_ns > self.config.max_age_ns {
                    Some(Exclusion::Stale { age_ns })
                } else if crossed(snap) {
                    Some(Exclusion::Crossed)
                } else {
                    None
                }
            }
        };
        excluded.map_or(VenueState::Included, VenueState::Excluded)
    }

    /// Venues used at `now_ns`, with their books.
    fn included(&self, now_ns: u64) -> impl Iterator<Item = (VenueId, &Snapshot)> {
        self.slots.iter().enumerate().filter_map(move |(i, s)| {
            (self.judge(&s.snap, now_ns) == VenueState::Included)
                .then_some((VenueId::from_index(i), &s.snap))
        })
    }

    /// Venues used at `now_ns`, with their books and configuration (for the execution
    /// planner and the arbitrage scan).
    pub(crate) fn included_with_config(
        &self,
        now_ns: u64,
    ) -> impl Iterator<Item = (VenueId, &Snapshot, &VenueConfig)> {
        self.slots.iter().enumerate().filter_map(move |(i, s)| {
            (self.judge(&s.snap, now_ns) == VenueState::Included).then_some((
                VenueId::from_index(i),
                &s.snap,
                &s.config,
            ))
        })
    }

    /// The best `out.len()` price levels across all healthy venues, merged best first
    /// (highest bids / lowest asks; at equal prices, in venue order), each tagged with
    /// its venue. Writes into `out` and returns how many were written: no allocation.
    /// The assignment's "best N levels across all venues".
    pub fn best_levels(&self, side: ob_core::Side, now_ns: u64, out: &mut [Quote]) -> usize {
        // One cursor per included venue into its levels on `side` (best first).
        let mut heads: [(VenueId, &[ob_core::Level]); MAX_VENUES] = [(VenueId(0), &[]); MAX_VENUES];
        let mut k = 0;
        for (id, snap) in self.included(now_ns) {
            if let Some(h) = heads.get_mut(k) {
                *h = (
                    id,
                    match side {
                        ob_core::Side::Bid => snap.bids(),
                        ob_core::Side::Ask => snap.asks(),
                    },
                );
                k += 1;
            }
        }
        let heads = &mut heads[..k];
        let better = |a: ob_core::Px, b: ob_core::Px| match side {
            ob_core::Side::Bid => a > b,
            ob_core::Side::Ask => a < b,
        };
        let mut n = 0;
        for slot in out.iter_mut() {
            // A k-way merge with k ≤ MAX_VENUES: a linear scan of the heads beats a heap.
            let mut pick: Option<usize> = None;
            for (i, (_, levels)) in heads.iter().enumerate() {
                if let Some(l) = levels.first()
                    && pick.is_none_or(|p| better(l.px, heads[p].1[0].px))
                {
                    pick = Some(i);
                }
            }
            let Some(p) = pick else {
                break;
            };
            let (id, levels) = &mut heads[p];
            *slot = Quote::new(*id, levels[0]);
            *levels = &levels[1..];
            n += 1;
        }
        n
    }

    /// The best bid of every included venue, best first (highest price; ties: larger
    /// quantity, then lower venue ID). The Java skeleton's `getBestBid`.
    pub fn best_bids(&self, now_ns: u64) -> Quotes {
        let mut out = Quotes::default();
        for (id, snap) in self.included(now_ns) {
            if let Some(&l) = snap.bids().first() {
                out.insert_sorted(Quote::new(id, l), |a, b| (a.px, a.qty) > (b.px, b.qty));
            }
        }
        out
    }

    /// The best ask of every included venue, best first (lowest price; ties: larger
    /// quantity, then lower venue ID). The Java skeleton's `getBestAsk`.
    pub fn best_asks(&self, now_ns: u64) -> Quotes {
        let mut out = Quotes::default();
        for (id, snap) in self.included(now_ns) {
            if let Some(&l) = snap.asks().first() {
                out.insert_sorted(Quote::new(id, l), |a, b| {
                    a.px < b.px || a.px == b.px && a.qty > b.qty
                });
            }
        }
        out
    }

    /// Bid volume at `limit` **or higher**, per venue and in total: how much could be
    /// sold right now without going below `limit`. The Java skeleton's
    /// `getBidVolumeForPrice`.
    pub fn bid_volume_at_or_above(&self, limit: Px, now_ns: u64) -> Volume {
        self.volume(now_ns, |s| s.bids(), |px| px >= limit)
    }

    /// Ask volume at `limit` **or lower**, per venue and in total: how much could be
    /// bought right now without paying more than `limit`. The Java skeleton's
    /// `getAskVolumeForPrice`.
    pub fn ask_volume_at_or_below(&self, limit: Px, now_ns: u64) -> Volume {
        self.volume(now_ns, |s| s.asks(), |px| px <= limit)
    }

    fn volume(
        &self,
        now_ns: u64,
        side: impl Fn(&Snapshot) -> &[Level],
        within: impl Fn(Px) -> bool,
    ) -> Volume {
        let mut out = Volume::default();
        for (id, snap) in self.included(now_ns) {
            let levels = side(snap);
            // Levels are best-first, so the matching ones are a prefix.
            let n = levels.iter().take_while(|l| within(l.px)).count();
            let depth = self.slots[id.index()].config.depth;
            out.push(VenueVolume {
                venue: id,
                // Saturates only for absurd data (the total then reports None).
                qty: sum_qty(levels[..n].iter().map(|l| l.qty)).unwrap_or(Qty::MAX),
                levels: n,
                complete: n < levels.len() || levels.len() < depth,
            });
        }
        out.finish();
        out
    }
}

fn crossed(snap: &Snapshot) -> bool {
    match (snap.bids().first(), snap.asks().first()) {
        (Some(b), Some(a)) => b.px >= a.px,
        _ => false,
    }
}
