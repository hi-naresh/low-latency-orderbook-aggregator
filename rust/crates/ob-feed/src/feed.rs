//! The per-venue state machine, independent of I/O and of the real clock.
//!
//! ```text
//! Disconnected ──connected──▶ Subscribing ──first book──▶ Live
//!      ▲                           │                        │ gap / crossed / bad data
//!      │ backoff + jitter          │ subscribe timeout      ▼
//!      └────── Reconnect ◀─────────┴──── too many ──── Syncing (unsubscribe + resubscribe)
//!              (stale feed, pong timeout, refused,         │ fresh snapshot
//!               too many resyncs, sync timeout)             └──────────────▶ Live
//! ```
//!
//! **Sans-I/O:** every method takes the current time (`now`, monotonic ns) and appends
//! [`Command`]s (send a request, reconnect, log an event) for the driver to carry out.
//! So every timing rule (heartbeats, timeouts, backoff) is testable deterministically
//! with a fake clock and recorded messages.
//!
//! **Health travels with the book:** every state change publishes the book with the
//! matching status (`Down`, `Syncing`, `Live`), so the aggregator (step 9) stops using a
//! venue at the moment its feed stops trusting the book, and starts again only after a
//! fresh snapshot.

use std::time::Duration;

use ob_core::ErrorClass;
use ob_sync::{BookMeta, BookPublisher, VenueStatus};

use crate::backoff::{Backoff, BackoffConfig};
use crate::probe::{NoProbe, Probe};
use crate::venue::{Outcome, Request, ResyncReason, Venue};

/// Timing and escalation rules.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FeedConfig {
    /// After connecting (or resubscribing), a book must arrive within this long.
    pub subscribe_timeout: Duration,
    /// A connected feed with no message for this long is dead (both venues publish every
    /// 20–120 ms on BTCUSDT).
    pub stale_after: Duration,
    /// A heartbeat must be answered within this long.
    pub pong_timeout: Duration,
    /// After this long `Live`, a connection counts as healthy: backoff starts again from
    /// its first step. Prevents a flapping connection from retrying at full speed forever.
    pub healthy_after: Duration,
    /// More resyncs than this within `resync_window` → reconnect instead (resubscribing
    /// isn't fixing whatever is wrong).
    pub max_resyncs: u32,
    /// See `max_resyncs`.
    pub resync_window: Duration,
    /// Reconnect delays.
    pub backoff: BackoffConfig,
}

impl Default for FeedConfig {
    fn default() -> Self {
        FeedConfig {
            subscribe_timeout: Duration::from_secs(10),
            stale_after: Duration::from_secs(5),
            pong_timeout: Duration::from_secs(10),
            healthy_after: Duration::from_secs(30),
            max_resyncs: 5,
            resync_window: Duration::from_secs(60),
            backoff: BackoffConfig::default(),
        }
    }
}

/// Where the feed is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// No connection; the next attempt is allowed at `retry_at_ns`.
    Disconnected {
        /// Monotonic ns.
        retry_at_ns: u64,
    },
    /// Connected and subscribed, waiting for the first book.
    Subscribing {
        /// Since when (ns).
        since_ns: u64,
    },
    /// The book was found wrong; waiting for a fresh snapshot.
    Syncing {
        /// Since when (ns).
        since_ns: u64,
    },
    /// The book is believed correct.
    Live {
        /// Since when (ns).
        since_ns: u64,
    },
}

/// Why the feed asked for a reconnect.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReconnectReason {
    /// No message for `stale_after`.
    Stale,
    /// No book within `subscribe_timeout` of connecting.
    SubscribeTimeout,
    /// No fresh snapshot within `subscribe_timeout` of a resync.
    SyncTimeout,
    /// A heartbeat went unanswered.
    PongTimeout,
    /// The venue refused our subscription.
    SubscribeRefused,
    /// Resyncing kept failing (`max_resyncs` within `resync_window`).
    TooManyResyncs,
    /// A REST snapshot couldn't be fetched (full depth, step 19).
    SnapshotFailed,
}

/// Something worth logging.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FeedEvent {
    /// Connected and subscribed.
    Connected,
    /// The first good book after connecting or resyncing.
    Live,
    /// A message was missed, but the next snapshot repaired the book (Binance).
    Gap,
    /// A message was dropped; the book is still valid.
    Rejected(ErrorClass),
    /// The book was found wrong; resubscribing.
    Resync(ResyncReason),
    /// Giving up on this connection.
    Reconnecting(ReconnectReason),
    /// The connection is gone; the next attempt is in `retry_in`.
    Disconnected {
        /// Backoff delay.
        retry_in: Duration,
    },
    /// Fetching a REST snapshot to (re)build a full-depth book (step 19).
    FetchingSnapshot,
}

/// An instruction for the driver.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    /// Send this request on the connection.
    Send(Request),
    /// Close the connection and report `on_disconnected`.
    Reconnect,
    /// Fetch the venue's REST snapshot and report `on_snapshot` / `on_snapshot_failed`.
    FetchSnapshot,
    /// Log this.
    Log(FeedEvent),
}

/// A small fixed-capacity queue of commands (no allocation).
#[derive(Clone, Debug, Default)]
pub struct Commands {
    items: [Option<Command>; 16],
    head: usize,
    len: usize,
}

impl Commands {
    fn push(&mut self, c: Command) {
        if self.len < self.items.len() {
            self.items[(self.head + self.len) % self.items.len()] = Some(c);
            self.len += 1;
        }
    }

    /// Takes the oldest command.
    pub fn pop(&mut self) -> Option<Command> {
        if self.len == 0 {
            return None;
        }
        let c = self.items[self.head].take();
        self.head = (self.head + 1) % self.items.len();
        self.len -= 1;
        c
    }

    /// Takes everything, oldest first (for tests and simple drivers).
    pub fn drain(&mut self) -> Vec<Command> {
        std::iter::from_fn(|| self.pop()).collect()
    }
}

/// Counters for monitoring.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FeedStats {
    /// Text messages received.
    pub messages: u64,
    /// Book updates applied and published (including top-of-book updates).
    pub applied: u64,
    /// Of those, updates from the real-time top-of-book stream (step 18).
    pub top_updates: u64,
    /// Gaps repaired by the next snapshot (Binance).
    pub gaps: u64,
    /// Messages dropped with the book still valid.
    pub rejected: u64,
    /// Resyncs (resubscribe for a fresh snapshot).
    pub resyncs: u64,
    /// Reconnects the feed asked for.
    pub reconnects: u64,
    /// Connections lost or failed.
    pub disconnects: u64,
}

/// One venue's feed: its adapter, its publisher and the state machine.
pub struct Feed<V: Venue, P: Probe = NoProbe> {
    venue: V,
    probe: P,
    publisher: BookPublisher,
    cfg: FeedConfig,
    state: State,
    backoff: Backoff,
    last_msg_ns: u64,
    last_ping_ns: u64,
    pong_due_ns: Option<u64>,
    resync_window_start_ns: u64,
    resyncs_in_window: u32,
    last_update: (u64, u64),
    /// A snapshot fetch has been requested and not answered yet.
    snapshot_pending: bool,
    stats: FeedStats,
}

fn ns(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

impl<V: Venue> Feed<V> {
    /// A disconnected feed that may connect immediately. `seed` drives backoff jitter.
    pub fn new(venue: V, publisher: BookPublisher, cfg: FeedConfig, seed: u64) -> Self {
        Feed::with_probe(venue, publisher, cfg, seed, NoProbe)
    }
}

impl<V: Venue, P: Probe> Feed<V, P> {
    /// Like [`Feed::new`], measuring each message's stages with `probe`.
    pub fn with_probe(
        venue: V,
        publisher: BookPublisher,
        cfg: FeedConfig,
        seed: u64,
        probe: P,
    ) -> Self {
        Feed {
            venue,
            probe,
            publisher,
            cfg,
            state: State::Disconnected { retry_at_ns: 0 },
            backoff: Backoff::new(cfg.backoff, seed),
            last_msg_ns: 0,
            last_ping_ns: 0,
            pong_due_ns: None,
            resync_window_start_ns: 0,
            resyncs_in_window: 0,
            last_update: (0, 0),
            snapshot_pending: false,
            stats: FeedStats::default(),
        }
    }

    /// The venue adapter (its name, endpoint, request bytes).
    pub fn venue(&self) -> &V {
        &self.venue
    }

    /// The probe (stage timings).
    pub fn probe(&self) -> &P {
        &self.probe
    }

    /// The probe, mutably (the driver records the read stage; replay sets its clock).
    pub fn probe_mut(&mut self) -> &mut P {
        &mut self.probe
    }

    /// The current state.
    pub fn state(&self) -> State {
        self.state
    }

    /// Counters.
    pub fn stats(&self) -> FeedStats {
        self.stats
    }

    /// `true` if disconnected and the backoff delay has passed.
    pub fn ready_to_connect(&self, now: u64) -> bool {
        matches!(self.state, State::Disconnected { retry_at_ns } if now >= retry_at_ns)
    }

    /// The driver connected (TLS + WebSocket handshake done).
    pub fn on_connected(&mut self, now: u64, cmds: &mut Commands) {
        self.state = State::Subscribing { since_ns: now };
        self.last_msg_ns = now;
        self.last_ping_ns = now;
        self.pong_due_ns = None;
        self.snapshot_pending = false;
        self.venue.reset();
        self.publish(now, VenueStatus::Syncing);
        cmds.push(Command::Log(FeedEvent::Connected));
        cmds.push(Command::Send(Request::Subscribe));
    }

    /// A text message arrived at `now`.
    pub fn on_text(&mut self, now: u64, text: &[u8], cmds: &mut Commands) {
        self.last_msg_ns = now;
        self.stats.messages += 1;
        let handle_start = self.probe.stamp();
        let outcome = self.venue.on_text(text);
        self.handle(now, handle_start, outcome, cmds);
    }

    /// The REST snapshot the feed asked for (via [`Command::FetchSnapshot`]) arrived.
    pub fn on_snapshot(&mut self, now: u64, body: &[u8], cmds: &mut Commands) {
        self.snapshot_pending = false;
        self.last_msg_ns = now; // fetching took time in which no message was read
        let handle_start = self.probe.stamp();
        let outcome = self.venue.on_snapshot(body);
        self.handle(now, handle_start, outcome, cmds);
    }

    /// The snapshot couldn't be fetched: start over on a new connection (with backoff,
    /// which also protects the venue's REST rate limit).
    pub fn on_snapshot_failed(&mut self, _now: u64, cmds: &mut Commands) {
        self.snapshot_pending = false;
        self.reconnect(ReconnectReason::SnapshotFailed, cmds);
    }

    fn handle(&mut self, now: u64, handle_start: u64, outcome: Outcome, cmds: &mut Commands) {
        match outcome {
            Outcome::Applied {
                update_id,
                exchange_ts,
                gap,
                top,
            } => {
                self.stats.top_updates += u64::from(top);
                if gap {
                    self.stats.gaps += 1;
                    cmds.push(Command::Log(FeedEvent::Gap));
                }
                self.stats.applied += 1;
                self.last_update = (update_id, exchange_ts);
                if !matches!(self.state, State::Live { .. }) {
                    self.state = State::Live { since_ns: now };
                    cmds.push(Command::Log(FeedEvent::Live));
                }
                let handle_end = self.probe.stamp();
                self.publish(now, VenueStatus::Live);
                let publish_end = self.probe.stamp();
                self.probe
                    .applied(handle_start, handle_end, publish_end, exchange_ts);
            }
            Outcome::Ignored | Outcome::Subscribed { ok: true } => {}
            Outcome::Rejected(class) => {
                self.stats.rejected += 1;
                cmds.push(Command::Log(FeedEvent::Rejected(class)));
            }
            Outcome::Resync(reason) => self.resync(now, reason, cmds),
            Outcome::Pong => self.pong_due_ns = None,
            Outcome::Subscribed { ok: false } => {
                self.reconnect(ReconnectReason::SubscribeRefused, cmds)
            }
            Outcome::NeedSnapshot if !self.snapshot_pending => {
                self.snapshot_pending = true;
                cmds.push(Command::Log(FeedEvent::FetchingSnapshot));
                cmds.push(Command::FetchSnapshot);
            }
            Outcome::NeedSnapshot => {} // already asked
        }
    }

    /// Called whenever there's nothing to read: timeouts and heartbeats.
    pub fn on_tick(&mut self, now: u64, cmds: &mut Commands) {
        let since_msg = now.saturating_sub(self.last_msg_ns);
        match self.state {
            State::Disconnected { .. } => return,
            State::Subscribing { since_ns }
                if now.saturating_sub(since_ns) > ns(self.cfg.subscribe_timeout) =>
            {
                return self.reconnect(ReconnectReason::SubscribeTimeout, cmds);
            }
            // Deltas from the old subscription can keep arriving (and be ignored) while a
            // fresh snapshot never does: that's a timeout too, not "alive".
            State::Syncing { since_ns }
                if now.saturating_sub(since_ns) > ns(self.cfg.subscribe_timeout) =>
            {
                return self.reconnect(ReconnectReason::SyncTimeout, cmds);
            }
            _ if since_msg > ns(self.cfg.stale_after) => {
                return self.reconnect(ReconnectReason::Stale, cmds);
            }
            State::Live { since_ns }
                if now.saturating_sub(since_ns) >= ns(self.cfg.healthy_after) =>
            {
                self.backoff.reset()
            }
            _ => {}
        }
        if let Some(interval) = self.venue.heartbeat_interval() {
            if self.pong_due_ns.is_some_and(|due| now > due) {
                return self.reconnect(ReconnectReason::PongTimeout, cmds);
            }
            if now.saturating_sub(self.last_ping_ns) >= ns(interval) {
                self.last_ping_ns = now;
                if self.pong_due_ns.is_none() {
                    self.pong_due_ns = Some(now + ns(self.cfg.pong_timeout));
                }
                cmds.push(Command::Send(Request::Ping));
            }
        }
    }

    /// A connection attempt failed: wait longer before the next one.
    pub fn on_connect_failed(&mut self, now: u64, cmds: &mut Commands) {
        self.go_down(now, cmds);
    }

    /// The connection is gone (lost, or closed on our request). Idempotent: if the feed
    /// is already disconnected (e.g. a reconnect it asked for *and* a socket error both
    /// report the same loss), nothing happens, so the backoff isn't advanced twice.
    pub fn on_disconnected(&mut self, now: u64, cmds: &mut Commands) {
        if !matches!(self.state, State::Disconnected { .. }) {
            self.go_down(now, cmds);
        }
    }

    fn go_down(&mut self, now: u64, cmds: &mut Commands) {
        let delay = self.backoff.next_delay();
        self.state = State::Disconnected {
            retry_at_ns: now.saturating_add(ns(delay)),
        };
        self.stats.disconnects += 1;
        self.snapshot_pending = false;
        self.venue.reset();
        self.publish(now, VenueStatus::Down);
        cmds.push(Command::Log(FeedEvent::Disconnected { retry_in: delay }));
    }

    fn resync(&mut self, now: u64, reason: ResyncReason, cmds: &mut Commands) {
        self.stats.resyncs += 1;
        cmds.push(Command::Log(FeedEvent::Resync(reason)));
        if now.saturating_sub(self.resync_window_start_ns) > ns(self.cfg.resync_window) {
            self.resync_window_start_ns = now;
            self.resyncs_in_window = 0;
        }
        self.resyncs_in_window += 1;
        if self.resyncs_in_window > self.cfg.max_resyncs {
            return self.reconnect(ReconnectReason::TooManyResyncs, cmds);
        }
        self.snapshot_pending = false;
        self.venue.reset();
        self.state = State::Syncing { since_ns: now };
        self.publish(now, VenueStatus::Syncing);
        cmds.push(Command::Send(Request::Unsubscribe));
        cmds.push(Command::Send(Request::Subscribe));
    }

    fn reconnect(&mut self, reason: ReconnectReason, cmds: &mut Commands) {
        self.stats.reconnects += 1;
        cmds.push(Command::Log(FeedEvent::Reconnecting(reason)));
        cmds.push(Command::Reconnect);
    }

    fn publish(&mut self, now: u64, status: VenueStatus) {
        let (update_id, exchange_ts) = self.last_update;
        self.venue.publish(
            &mut self.publisher,
            BookMeta {
                update_id,
                exchange_ts,
                recv_ns: now,
                status,
            },
        );
    }
}
