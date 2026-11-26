//! The two venue adapters.
//!
//! | | Binance (`depth20@100ms`) | Bybit (`orderbook.50`) |
//! |---|---|---|
//! | Every message | a full top-20 **snapshot** | a snapshot, then **deltas** |
//! | Sequence gap | the next snapshot fixes the book: count it, carry on | the book is wrong: **resync** |
//! | Unparseable message | drop it; the next snapshot fixes the book | **resync** |
//! | Heartbeat | server pings, answered by the transport | client must send `{"op":"ping"}` every 20 s |
//! | Book capacity | 64 (≥ 20) | **128** (≥ 2 × 50: deltas can list 100 levels) |
//! | Top of book (optional) | `bookTicker`, every change (~800/s) | `orderbook.1`, every 10 ms |
//! | Reconciled by | `u` (shared with depth) | `seq` (Bybit's cross sequence) |
//!
//! **Top-of-book overlay** (`with_top_of_book()`): the real-time best bid/ask stream is
//! subscribed on the same connection. Whichever of the depth book and the overlay has the
//! higher sequence number is the truth: a newer overlay is published over the book
//! ([`BookPublisher::publish_with_top`]); a depth message that catches up (sequence ≥
//! the overlay's) clears it. The overlay never modifies the depth book.

use std::time::Duration;

use ob_core::{InstrumentSpec, OrderBook};
use ob_parse::binance::{self, BinanceDepthParser, BookTicker, DepthUpdate};
use ob_parse::bybit::{self, BookMsg, BybitBookParser, Channel, Sequence};
use ob_sync::{BookMeta, BookPublisher, TopOverlay};

use crate::venue::{Outcome, Request, ResyncReason, Venue};

/// Leaks a string built once at start-up so parsers can hold `&'static` names.
pub(crate) fn leak(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

/// Binance USDⓈ-M futures, partial depth stream.
pub struct Binance {
    parser: BinanceDepthParser<'static>,
    msg: DepthUpdate,
    book: OrderBook<64>,
    last_u: Option<u64>,
    /// The newest bookTicker that is newer than the book: (its `u`, the levels).
    top: Option<(u64, TopOverlay)>,
    top_enabled: bool,
    stream: String,
    subscribe: Vec<u8>,
    unsubscribe: Vec<u8>,
}

/// SUBSCRIBE/UNSUBSCRIBE requests for Binance `streams`.
pub(crate) fn binance_requests(streams: &[String]) -> (Vec<u8>, Vec<u8>) {
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

impl Binance {
    /// `symbol` as Binance writes it (`"BTCUSDT"`).
    pub fn new(spec: InstrumentSpec, symbol: &str) -> Self {
        let stream = symbol.to_ascii_lowercase();
        let (subscribe, unsubscribe) = binance_requests(&[format!("{stream}@depth20@100ms")]);
        Binance {
            parser: BinanceDepthParser::new(spec, leak(symbol.to_string()).as_bytes()),
            msg: DepthUpdate::default(),
            book: OrderBook::new(),
            last_u: None,
            top: None,
            top_enabled: false,
            stream,
            subscribe,
            unsubscribe,
        }
    }

    /// Also subscribes to `<symbol>@bookTicker` (real-time best bid/ask, ~800 updates/s
    /// on BTCUSDT against 10/s for depth) and publishes it over the depth book.
    #[must_use]
    pub fn with_top_of_book(mut self) -> Self {
        let s = &self.stream;
        (self.subscribe, self.unsubscribe) =
            binance_requests(&[format!("{s}@depth20@100ms"), format!("{s}@bookTicker")]);
        self.top_enabled = true;
        self
    }

    fn on_ticker(&mut self, t: BookTicker) -> Outcome {
        // Nothing to lay it over before the first depth snapshot; and a ticker no newer
        // than the book or than the current overlay is old news.
        let newest = self.top.map_or(self.last_u, |(u, _)| Some(u));
        if !self.top_enabled || newest.is_none_or(|u| t.update_id <= u) {
            return Outcome::Ignored;
        }
        if t.bid.px >= t.ask.px {
            return Outcome::Rejected(ob_core::ErrorClass::Invalid); // a crossed top: never seen, never trusted
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

impl Venue for Binance {
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
            Request::Ping => None, // Binance pings us; the transport answers
        }
    }
    fn heartbeat_interval(&self) -> Option<Duration> {
        None
    }
    fn depth(&self) -> usize {
        20
    }

    fn on_text(&mut self, text: &[u8]) -> Outcome {
        match self.parser.parse(text, &mut self.msg) {
            // A dropped snapshot costs nothing: the next one (≤ 100 ms) is complete.
            Err(e) => Outcome::Rejected(e.class()),
            Ok(binance::Message::Response { id: 1, ok }) => Outcome::Subscribed { ok },
            Ok(binance::Message::Response { .. }) => Outcome::Ignored,
            Ok(binance::Message::BookTicker(t)) => self.on_ticker(t),
            Ok(binance::Message::Depth) => {
                let gap = self
                    .last_u
                    .is_some_and(|u| self.msg.prev_final_update_id != u);
                if self.msg.apply_to(&mut self.book).is_err() {
                    return Outcome::Rejected(ob_core::ErrorClass::Invalid); // book unchanged
                }
                self.last_u = Some(self.msg.final_update_id);
                // The book has caught up with (or passed) the overlay: drop it.
                if self.top.is_some_and(|(u, _)| u <= self.msg.final_update_id) {
                    self.top = None;
                }
                Outcome::Applied {
                    update_id: self.msg.final_update_id,
                    exchange_ts: self.msg.event_time,
                    gap,
                    top: false,
                }
            }
        }
    }

    fn reset(&mut self) {
        self.book.clear();
        self.last_u = None;
        self.top = None;
    }

    fn publish(&self, publisher: &mut BookPublisher, meta: BookMeta) {
        publisher.publish_with_top(&self.book, self.top.map(|(_, t)| t), meta);
    }
}

/// Bybit v5 linear, order book stream.
pub struct Bybit {
    parser: BybitBookParser<'static>,
    msg: BookMsg,
    /// Capacity ≥ 2 × depth: a delta can list 50 deletions and 50 insertions.
    book: OrderBook<128>,
    last_u: Option<u64>,
    /// `seq` of the last applied depth message (comparable with the top topic's `seq`).
    last_seq: u64,
    /// The newest top-of-book snapshot that is newer than the book: (its `seq`, levels).
    top: Option<(u64, TopOverlay)>,
    top_enabled: bool,
    topic: &'static str,
    symbol: String,
    subscribe: Vec<u8>,
    unsubscribe: Vec<u8>,
    heartbeat: Duration,
}

/// subscribe/unsubscribe requests for Bybit `topics`.
fn bybit_requests(topics: &[&str]) -> (Vec<u8>, Vec<u8>) {
    let list = topics
        .iter()
        .map(|t| format!("\"{t}\""))
        .collect::<Vec<_>>()
        .join(",");
    (
        format!(r#"{{"req_id":"sub","op":"subscribe","args":[{list}]}}"#).into_bytes(),
        format!(r#"{{"req_id":"unsub","op":"unsubscribe","args":[{list}]}}"#).into_bytes(),
    )
}

impl Bybit {
    /// `symbol` as Bybit writes it (`"BTCUSDT"`); subscribes to `orderbook.50.<symbol>`.
    pub fn new(spec: InstrumentSpec, symbol: &str) -> Self {
        let topic = leak(format!("orderbook.50.{symbol}"));
        let (subscribe, unsubscribe) = bybit_requests(&[topic]);
        Bybit {
            parser: BybitBookParser::new(
                spec,
                topic.as_bytes(),
                leak(symbol.to_string()).as_bytes(),
            ),
            msg: BookMsg::default(),
            book: OrderBook::new(),
            last_u: None,
            last_seq: 0,
            top: None,
            top_enabled: false,
            topic,
            symbol: symbol.to_string(),
            subscribe,
            unsubscribe,
            heartbeat: Duration::from_secs(20), // Bybit's documented requirement
        }
    }

    /// Also subscribes to `orderbook.1.<symbol>` (best bid/ask snapshots every 10 ms,
    /// against 20 ms for the depth stream) and publishes it over the depth book.
    #[must_use]
    pub fn with_top_of_book(mut self) -> Self {
        let top_topic = leak(format!("orderbook.1.{}", self.symbol));
        self.parser = self.parser.with_top_topic(top_topic.as_bytes());
        (self.subscribe, self.unsubscribe) = bybit_requests(&[self.topic, top_topic]);
        self.top_enabled = true;
        self
    }

    fn on_top(&mut self) -> Outcome {
        let m = &self.msg;
        let newest = self.top.map_or(self.last_seq, |(seq, _)| seq);
        if !self.top_enabled || self.last_u.is_none() || m.seq <= newest {
            return Outcome::Ignored; // no book yet, or not newer than what we show
        }
        let (Some(bid), Some(ask)) = (m.bids.first(), m.asks.first()) else {
            return Outcome::Ignored; // a one-sided top: nothing reliable to overlay
        };
        if bid.px >= ask.px {
            return Outcome::Rejected(ob_core::ErrorClass::Invalid);
        }
        self.top = Some((
            m.seq,
            TopOverlay {
                bid: *bid,
                ask: *ask,
            },
        ));
        Outcome::Applied {
            update_id: m.update_id,
            exchange_ts: m.ts,
            gap: false,
            top: true,
        }
    }

    /// Pings every `interval` instead of every 20 s (tests use milliseconds).
    #[must_use]
    pub fn with_heartbeat(mut self, interval: Duration) -> Self {
        self.heartbeat = interval;
        self
    }
}

impl Venue for Bybit {
    fn name(&self) -> &'static str {
        "bybit"
    }
    fn endpoint(&self) -> (&str, &str) {
        ("stream.bybit.com", "/v5/public/linear")
    }
    fn request(&self, request: Request) -> Option<&[u8]> {
        match request {
            Request::Subscribe => Some(&self.subscribe),
            Request::Unsubscribe => Some(&self.unsubscribe),
            Request::Ping => Some(br#"{"req_id":"hb","op":"ping"}"#),
        }
    }
    fn heartbeat_interval(&self) -> Option<Duration> {
        Some(self.heartbeat)
    }
    fn depth(&self) -> usize {
        50
    }

    fn on_text(&mut self, text: &[u8]) -> Outcome {
        match self.parser.parse(text, &mut self.msg) {
            // A lost delta can't be recovered: rebuild from a snapshot.
            Err(e) => Outcome::Resync(ResyncReason::Unparseable(e.class())),
            Ok(bybit::Message::Response {
                op: bybit::Op::Subscribe,
                success,
            }) => Outcome::Subscribed { ok: success },
            Ok(bybit::Message::Response {
                op: bybit::Op::Pong,
                success: true,
            }) => Outcome::Pong,
            Ok(bybit::Message::Response { .. }) => Outcome::Ignored,
            Ok(bybit::Message::Book) if self.msg.channel == Channel::Top => self.on_top(),
            Ok(bybit::Message::Book) => match self.msg.sequence(self.last_u) {
                // A duplicate, or a delta before the first snapshot (e.g. right after a
                // resubscribe, from the old subscription): nothing to apply it to.
                Sequence::Stale | Sequence::NoBaseline => Outcome::Ignored,
                Sequence::Gap { expected } => Outcome::Resync(ResyncReason::Gap {
                    expected,
                    got: self.msg.update_id,
                }),
                Sequence::Reset | Sequence::Next => match self.msg.apply_to(&mut self.book) {
                    Err(_) => Outcome::Resync(ResyncReason::BadSnapshot),
                    Ok(applied) if applied.removed_absent > 0 => {
                        Outcome::Resync(ResyncReason::RemovedAbsent)
                    }
                    Ok(_) if self.book.is_crossed() => Outcome::Resync(ResyncReason::Crossed),
                    Ok(_) => {
                        self.last_u = Some(self.msg.update_id);
                        self.last_seq = self.msg.seq;
                        // The book has caught up with (or passed) the overlay: drop it.
                        if self.top.is_some_and(|(seq, _)| seq <= self.msg.seq) {
                            self.top = None;
                        }
                        Outcome::Applied {
                            update_id: self.msg.update_id,
                            exchange_ts: self.msg.ts,
                            gap: false,
                            top: false,
                        }
                    }
                },
            },
        }
    }

    fn reset(&mut self) {
        self.book.clear();
        self.last_u = None;
        self.last_seq = 0;
        self.top = None;
    }

    fn publish(&self, publisher: &mut BookPublisher, meta: BookMeta) {
        publisher.publish_with_top(&self.book, self.top.map(|(_, t)| t), meta);
    }
}
