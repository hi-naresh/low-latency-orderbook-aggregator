//! `ob-feed`: one feed per venue, keeping that venue's book correct and published.
//!
//! * [`venue`]: what differs between exchanges, behind the [`Venue`] trait.
//! * [`venues`]: the Binance and Bybit adapters (parse → sequence check → apply).
//! * [`full_depth`]: Binance full depth: diff stream + REST snapshot (step 19).
//! * [`feed`]: the state machine (subscribe, heartbeats, staleness, resync, reconnect),
//!   with no I/O and no real clock, so every timing rule is tested deterministically.
//! * [`driver`]: the I/O loop that runs a feed over a [`ob_net::WsConnection`].
//! * [`backoff`]: reconnect delays with jitter, and the shared monotonic clock.
//! * [`log`]: binary log records over an SPSC queue, so the feed thread never formats.
//! * [`probe`]: per-message stage timings (rdtsc) and latency histograms; free when off.
//! * [`record`]: a recording format, and replay through the same feed code.
#![forbid(unsafe_code)]

pub mod backoff;
pub mod driver;
pub mod feed;
pub mod full_depth;
pub mod log;
pub mod probe;
pub mod record;
pub mod venue;
pub mod venues;

pub use backoff::{Backoff, BackoffConfig, mono_ns};
pub use feed::{Command, Commands, Feed, FeedConfig, FeedEvent, FeedStats, ReconnectReason, State};
pub use full_depth::BinanceFullDepth;
pub use venue::{Outcome, Request, ResyncReason, Venue};
pub use venues::{Binance, Bybit};
