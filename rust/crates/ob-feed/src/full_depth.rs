//! Binance full-depth mode (step 19): the diff stream (`<symbol>@depth@100ms`) applied
//! to a REST snapshot (`GET /fapi/v1/depth?limit=1000`), instead of the 20-level
//! partial-depth stream.
//!
//! **Why.** Depth 20 sees ~$10 of the book on each side of BTCUSDT. Large orders, and the
//! aggregator's volume and execution queries, need more. Full depth keeps up to 1,024
//! levels per side, exactly, with every update sequence-checked.
//!
//! **Sync rules** (Binance's "How to manage a local order book correctly", checked
//! against the current documentation):
//! 1. Subscribe to the diff stream; buffer its events.
//! 2. Fetch a snapshot (`lastUpdateId` = L).
//! 3. Drop any event with `u` < L.
//! 4. The first applied event must have `U` ≤ L ≤ `u`.
//! 5. After that, each event's `pu` must equal the previous event's `u`; otherwise
//!    start again from step 2.
//! 6. Quantities are absolute; 0 removes the level. Removing a level we don't have "can
//!    happen and is normal" (so, unlike Bybit, it is **not** a resync).
//!
//! **The book (step 26)** is price-indexed ([`TickBook`]): an update, insert or removal is
//! O(1), where the sorted array spent its time binary-searching ~1,000 prices and
//! shifting deep inserts. Besides at most [`FULL_CAP`] levels, it holds a side within
//! 8,192 ticks of the best (819 USD at BTC's 0.1 tick); whatever that rule refuses or
//! drops moves the known-region boundary, exactly as capacity did before.
//!
//! **What we know exactly: a region, not the whole book.** The diff stream reports
//! changes anywhere in the book (a live sample had levels at 6,020 with BTC at 60,000),
//! but the snapshot only gave us the best 1,000 levels. A level beyond the snapshot's
//! worst price would sit next to gaps we know nothing about. So each side has a
//! **boundary**: bids at or above it (asks at or below) are known exactly; updates beyond
//! it are ignored. The boundary starts at the snapshot's worst level (unless the snapshot
//! held the entire side), and moves inwards whenever the book is full and has to drop its
//! worst level, or refuses one worse than all it holds. If a side ends up knowing fewer
//! than [`MIN_KNOWN`] levels (the market moved far), the venue asks for a new snapshot.
//!
//! **Allocation.** Synced (the steady state): none, proven by `tests/zero_alloc.rs`.
//! While waiting for a snapshot, events are buffered as owned bytes: a cold path that
//! happens once per sync.

use std::mem;
use std::time::Duration;

use ob_core::{InstrumentSpec, Level, LevelChange, Px, Side, TickBook};
use ob_parse::binance::{self, BinanceDepthParser, BookTicker, DepthUpdate};
use ob_sync::{BookMeta, BookPublisher, DEPTH, TopOverlay};

use crate::venue::{Outcome, Request, ResyncReason, Venue};

/// Levels kept per side.
pub const FULL_CAP: usize = 1024;
/// The snapshot size requested (Binance's maximum for futures; request weight 20).
pub const SNAPSHOT_LIMIT: usize = 1000;
/// Fewer exactly known levels than this on a side → fetch a new snapshot. Equal to what
/// is published, so readers always get a full, exact top `DEPTH`.
pub const MIN_KNOWN: usize = DEPTH;
/// Events buffered while waiting for a snapshot (~60 s at 10 per second) before giving
/// up and starting the sync again.
const MAX_BUFFERED: usize = 600;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Sync {
    /// No snapshot requested yet.
    Idle,
    /// Snapshot requested; events are buffered.
    Awaiting,
    /// Book built from a snapshot with `lastUpdateId` = `snapshot_id`.
    Synced,
}

/// Binance USDⓈ-M futures, full depth (diff stream + REST snapshot).
pub struct BinanceFullDepth {
    parser: BinanceDepthParser<'static>,
    msg: DepthUpdate,
    /// Price-indexed (step 26): at 1,000 levels a sorted array spends its time searching
    /// and shifting. About 130 KB, so boxed.
    book: Box<TickBook<FULL_CAP>>,
    sync: Sync,
    snapshot_id: u64,
    /// `u` of the last applied event (`None` until the first one after the snapshot).
    last_u: Option<u64>,
    /// Exactly known region: bids with px ≥ `bid_floor`, asks with px ≤ `ask_ceiling`
    /// (`None`: the whole side is known).
    bid_floor: Option<Px>,
    ask_ceiling: Option<Px>,
    buffer: Vec<Vec<u8>>,
    /// Optional bookTicker overlay (step 18): (its `u`, the levels).
    top: Option<(u64, TopOverlay)>,
    top_enabled: bool,
    stream: String,
    snapshot_path: String,
    subscribe: Vec<u8>,
    unsubscribe: Vec<u8>,
}

fn requests(streams: &[String]) -> (Vec<u8>, Vec<u8>) {
    let list = streams
        .iter()
        .map(|s| format!("\"{s}\""))
        .collect::<Vec<_>>()
        .join(",");
    (
        format!(r#"{{"method":"SUBSCRIBE","params":[{list}],"id":1}}"#).into_bytes(),
        format!(r#"{{"method":"UNSUBSCRIBE","params":[{list}],"id":2}}"#).into_bytes(),
    )
}

impl BinanceFullDepth {
    /// `symbol` as Binance writes it (`"BTCUSDT"`).
    pub fn new(spec: InstrumentSpec, symbol: &str) -> Self {
        let stream = symbol.to_ascii_lowercase();
        let (subscribe, unsubscribe) = requests(&[format!("{stream}@depth@100ms")]);
        BinanceFullDepth {
            parser: BinanceDepthParser::new(
                spec,
                Box::leak(symbol.to_string().into_boxed_str()).as_bytes(),
            ),
            msg: DepthUpdate::default(),
            book: Box::new(TickBook::new(spec.tick())),
            sync: Sync::Idle,
            snapshot_id: 0,
            last_u: None,
            bid_floor: None,
            ask_ceiling: None,
            buffer: Vec::new(),
            top: None,
            top_enabled: false,
            snapshot_path: format!("/fapi/v1/depth?symbol={symbol}&limit={SNAPSHOT_LIMIT}"),
            stream,
            subscribe,
            unsubscribe,
        }
    }

    /// Also subscribes to `bookTicker` and lays it over the book (step 18). The diff
    /// stream's `u` is the same order-book update ID, so the same rule applies.
    #[must_use]
    pub fn with_top_of_book(mut self) -> Self {
        let s = &self.stream;
        (self.subscribe, self.unsubscribe) =
            requests(&[format!("{s}@depth@100ms"), format!("{s}@bookTicker")]);
        self.top_enabled = true;
        self
    }

    /// The exactly known region's boundaries (tests and diagnostics).
    pub fn known_region(&self) -> (Option<Px>, Option<Px>) {
        (self.bid_floor, self.ask_ceiling)
    }

    /// The book (tests and diagnostics).
    pub fn book(&self) -> &TickBook<FULL_CAP> {
        &self.book
    }

    /// Applies the diff event in `self.msg` under the sync rules.
    fn apply_event(&mut self) -> Outcome {
        let m = &self.msg;
        let (first, last, prev) = (m.first_update_id, m.final_update_id, m.prev_final_update_id);
        if last < self.snapshot_id {
            return Outcome::Ignored; // rule 3: older than the snapshot
        }
        match self.last_u {
            // Rule 4: the first event must straddle the snapshot.
            None if first > self.snapshot_id => {
                return Outcome::Resync(ResyncReason::Gap {
                    expected: self.snapshot_id,
                    got: first,
                });
            }
            None => {}
            // Rule 5: continuity.
            Some(u) if prev != u => {
                return Outcome::Resync(ResyncReason::Gap {
                    expected: u,
                    got: prev,
                });
            }
            Some(_) => {}
        }
        for i in 0..self.msg.bids.as_slice().len() {
            let l = self.msg.bids.as_slice()[i];
            apply_known(&mut self.book, Side::Bid, l, &mut self.bid_floor);
        }
        for i in 0..self.msg.asks.as_slice().len() {
            let l = self.msg.asks.as_slice()[i];
            apply_known(&mut self.book, Side::Ask, l, &mut self.ask_ceiling);
        }
        self.last_u = Some(last);
        if self.top.is_some_and(|(u, _)| u <= last) {
            self.top = None;
        }
        if self.book.is_crossed() {
            return Outcome::Resync(ResyncReason::Crossed);
        }
        if self.depleted() {
            return Outcome::Resync(ResyncReason::Depleted);
        }
        Outcome::Applied {
            update_id: last,
            exchange_ts: self.msg.event_time,
            gap: false,
            top: false,
        }
    }

    /// A side with a boundary knows fewer than `MIN_KNOWN` levels exactly.
    fn depleted(&self) -> bool {
        (self.bid_floor.is_some() && self.book.bids().len() < MIN_KNOWN)
            || (self.ask_ceiling.is_some() && self.book.asks().len() < MIN_KNOWN)
    }

    fn on_ticker(&mut self, t: BookTicker) -> Outcome {
        if self.sync != Sync::Synced {
            return Outcome::Ignored;
        }
        let newest = self
            .top
            .map_or(self.last_u.unwrap_or(self.snapshot_id), |(u, _)| u);
        if !self.top_enabled || t.update_id <= newest {
            return Outcome::Ignored;
        }
        if t.bid.px >= t.ask.px {
            return Outcome::Rejected(ob_core::ErrorClass::Invalid);
        }
        self.top = Some((
            t.update_id,
            TopOverlay {
                bid: t.bid,
                ask: t.ask,
            },
        ));
        Outcome::Applied {
            update_id: t.update_id,
            exchange_ts: t.event_time,
            gap: false,
            top: true,
        }
    }
}

/// Applies one level inside the known region, moving the boundary inwards when the
/// book's capacity rules force levels out (or refuse one).
fn apply_known(book: &mut TickBook<FULL_CAP>, side: Side, l: Level, bound: &mut Option<Px>) {
    // Outside the known region: we can't tell what's next to it. Ignore.
    let outside = |px: Px, b: Px| match side {
        Side::Bid => px < b,
        Side::Ask => px > b,
    };
    if bound.is_some_and(|b| outside(l.px, b)) {
        return;
    }
    // Just inside a dropped/refused price: the new boundary.
    let inside = |px: Px| match side {
        Side::Bid => Px::from_raw(px.raw() + 1),
        Side::Ask => Px::from_raw(px.raw() - 1),
    };
    match book.apply(side, l) {
        // A better level went in and the worst level(s) had to go: the region now ends
        // just inside the innermost one dropped.
        (LevelChange::Inserted, Some(dropped)) => *bound = Some(inside(dropped)),
        // Worse than everything held (full, or too far from the best): it exists, we
        // don't keep it, so the region ends above it.
        (LevelChange::OutOfDepth, _) => *bound = Some(inside(l.px)),
        _ => {}
    }
}

impl Venue for BinanceFullDepth {
    fn name(&self) -> &'static str {
        "binance"
    }
    fn endpoint(&self) -> (&str, &str) {
        ("fstream.binance.com", "/ws")
    }
    fn request(&self, request: Request) -> Option<&[u8]> {
        match request {
            Request::Subscribe => Some(&self.subscribe),
            Request::Unsubscribe => Some(&self.unsubscribe),
            Request::Ping => None,
        }
    }
    fn heartbeat_interval(&self) -> Option<Duration> {
        None
    }
    fn depth(&self) -> usize {
        DEPTH // what is published (the book itself holds up to FULL_CAP)
    }

    fn on_text(&mut self, text: &[u8]) -> Outcome {
        match self.parser.parse(text, &mut self.msg) {
            // A lost diff can't be recovered: rebuild.
            Err(e) => Outcome::Resync(ResyncReason::Unparseable(e.class())),
            Ok(binance::Message::Response { id: 1, ok }) => Outcome::Subscribed { ok },
            Ok(binance::Message::Response { .. }) => Outcome::Ignored,
            Ok(binance::Message::BookTicker(t)) => self.on_ticker(t),
            Ok(binance::Message::Depth) => match self.sync {
                Sync::Idle => {
                    self.buffer.push(text.to_vec());
                    self.sync = Sync::Awaiting;
                    Outcome::NeedSnapshot
                }
                Sync::Awaiting if self.buffer.len() >= MAX_BUFFERED => {
                    Outcome::Resync(ResyncReason::Gap {
                        expected: 0,
                        got: self.msg.first_update_id,
                    })
                }
                Sync::Awaiting => {
                    self.buffer.push(text.to_vec());
                    Outcome::Ignored
                }
                Sync::Synced => self.apply_event(),
            },
        }
    }

    fn snapshot_endpoint(&self) -> Option<(&str, &str)> {
        Some(("fapi.binance.com", &self.snapshot_path))
    }

    fn on_snapshot(&mut self, body: &[u8]) -> Outcome {
        if self.sync != Sync::Awaiting {
            return Outcome::Ignored;
        }
        // Cold path: collect the levels (≤ 1000 a side), then load them in one go.
        let (mut bids, mut asks) = (Vec::with_capacity(1024), Vec::with_capacity(1024));
        let header = match self.parser.parse_rest_snapshot(body, |side, l| {
            match side {
                Side::Bid => bids.push(l),
                Side::Ask => asks.push(l),
            }
            true
        }) {
            Ok(h) => h,
            Err(_) => return Outcome::Resync(ResyncReason::BadSnapshot),
        };
        if self.book.replace(&bids, &asks).is_err() || self.book.is_crossed() {
            return Outcome::Resync(ResyncReason::BadSnapshot);
        }
        // A side as long as the limit (or longer than we keep) is truncated by Binance
        // (or by us): the region ends at its last kept level. A shorter one is the whole
        // side.
        let bound = |levels: &[Level], held: usize| {
            (levels.len() >= SNAPSHOT_LIMIT || levels.len() > held)
                .then(|| levels[held.min(levels.len()) - 1].px)
        };
        self.bid_floor = bound(&bids, self.book.bids().len());
        self.ask_ceiling = bound(&asks, self.book.asks().len());
        self.snapshot_id = header.last_update_id;
        self.last_u = None;
        self.top = None;
        self.sync = Sync::Synced;
        // Replay what arrived while we waited (rules 3–5 apply).
        let mut outcome = Outcome::Applied {
            update_id: header.last_update_id,
            exchange_ts: header.event_time,
            gap: false,
            top: false,
        };
        for text in mem::take(&mut self.buffer) {
            if self.parser.parse(&text, &mut self.msg).is_err() {
                return Outcome::Resync(ResyncReason::BadSnapshot);
            }
            match self.apply_event() {
                Outcome::Ignored => {}
                applied @ Outcome::Applied { .. } => outcome = applied,
                other => return other,
            }
        }
        outcome
    }

    fn reset(&mut self) {
        self.book.clear();
        self.sync = Sync::Idle;
        self.snapshot_id = 0;
        self.last_u = None;
        self.bid_floor = None;
        self.ask_ceiling = None;
        self.buffer.clear();
        self.top = None;
    }

    fn publish(&self, publisher: &mut BookPublisher, meta: BookMeta) {
        publisher.publish_with_top(&*self.book, self.top.map(|(_, t)| t), meta);
    }
}
