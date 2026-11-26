//! What differs between exchanges, behind one trait.
//!
//! A venue adapter knows its endpoint, how to phrase requests, and how to turn one text
//! message into an [`Outcome`] (parse → check sequence → apply to its own book). The
//! generic [`crate::Feed`] owns everything else: state, timers, heartbeats, resyncs,
//! reconnects, publishing.

use std::time::Duration;

use ob_core::ErrorClass;
use ob_sync::{BookMeta, BookPublisher};

/// A request the feed asks the venue adapter to phrase.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Request {
    /// Start receiving the book stream.
    Subscribe,
    /// Stop it (before re-subscribing to get a fresh snapshot).
    Unsubscribe,
    /// Application-level heartbeat (Bybit's `{"op":"ping"}`).
    Ping,
}

/// Why the book can't be trusted and must be rebuilt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResyncReason {
    /// A sequence gap: at least one message was missed.
    Gap {
        /// The update ID we expected next.
        expected: u64,
        /// The one that arrived.
        got: u64,
    },
    /// Our copy of this venue's book is crossed after applying an update.
    Crossed,
    /// A delete arrived for a level we don't have: our copy has drifted.
    RemovedAbsent,
    /// A snapshot was refused (unsorted levels, zero quantity).
    BadSnapshot,
    /// A message couldn't be parsed (with its error class).
    Unparseable(ErrorClass),
    /// A full-depth book has too few levels it knows exactly on one side (the market
    /// moved past the snapshot's range): fetch a new snapshot.
    Depleted,
}

/// What one message did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The book changed: publish it.
    Applied {
        /// The venue's update ID.
        update_id: u64,
        /// The venue's timestamp (ms).
        exchange_ts: u64,
        /// At least one message was missed before this one, but this one is a complete
        /// snapshot, so the book is correct again (Binance). Counted, not resynced.
        gap: bool,
        /// Only the top of book changed (a newer best bid/ask from the real-time stream),
        /// not the depth book.
        top: bool,
    },
    /// Nothing to do (a duplicate, a delta before the first snapshot, an unknown reply).
    Ignored,
    /// The message was dropped, but the book is still valid (Binance: the next snapshot
    /// replaces it anyway). Counted by class.
    Rejected(ErrorClass),
    /// The book is wrong: rebuild it.
    Resync(ResyncReason),
    /// Answer to our heartbeat.
    Pong,
    /// Answer to our subscribe request.
    Subscribed {
        /// `false` if the venue refused.
        ok: bool,
    },
    /// The venue needs a REST snapshot before it can build a book (full depth).
    /// The feed asks the driver to fetch [`Venue::snapshot_endpoint`] and calls
    /// [`Venue::on_snapshot`] with the body.
    NeedSnapshot,
}

/// One exchange's protocol.
pub trait Venue {
    /// Short name for logs and the aggregator (`"binance"`).
    fn name(&self) -> &'static str;
    /// Host and WebSocket path.
    fn endpoint(&self) -> (&str, &str);
    /// The bytes for `request`, or `None` if this venue doesn't use it.
    fn request(&self, request: Request) -> Option<&[u8]>;
    /// How often to send [`Request::Ping`], if the venue needs application heartbeats.
    fn heartbeat_interval(&self) -> Option<Duration>;
    /// Levels per side the stream carries (the aggregator's `VenueConfig::depth`).
    fn depth(&self) -> usize;
    /// Handles one text message: parse, check the sequence, apply to the book.
    fn on_text(&mut self, text: &[u8]) -> Outcome;
    /// Forgets the book and sequence state (after a disconnect or before a resync).
    fn reset(&mut self);
    /// Publishes the current book with `meta`.
    fn publish(&self, publisher: &mut BookPublisher, meta: BookMeta);
    /// Where to fetch a REST snapshot (host, path with query), for venues that need one.
    fn snapshot_endpoint(&self) -> Option<(&str, &str)> {
        None
    }
    /// Handles a fetched REST snapshot body (after [`Outcome::NeedSnapshot`]).
    fn on_snapshot(&mut self, _body: &[u8]) -> Outcome {
        Outcome::Ignored
    }
}
