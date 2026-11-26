//! The feed state machine, driven with the real recorded messages and a fake clock.
//! Every rule (subscribe, live, gaps, resync, heartbeats, timeouts, backoff) is checked
//! deterministically, including what the aggregator sees at each moment.
#![allow(clippy::unwrap_used)]

use std::time::Duration;

use ob_agg::{Aggregator, Config, Exclusion, VenueConfig, VenueState};
use ob_core::InstrumentSpec;
use ob_feed::{
    Binance, Bybit, Command, Commands, Feed, FeedConfig, FeedEvent, ReconnectReason, Request,
    ResyncReason, State, Venue,
};
use ob_sync::{BookReader, VenueStatus, book_channel};

const SEC: u64 = 1_000_000_000;
const MS: u64 = 1_000_000;
const BINANCE: &str = include_str!("../../ob-parse/tests/fixtures/binance_depth20.jsonl");
const BYBIT: &str = include_str!("../../ob-parse/tests/fixtures/bybit_orderbook50.jsonl");

fn spec() -> InstrumentSpec {
    InstrumentSpec::from_steps("0.1", "0.001").unwrap()
}

struct Harness<V: Venue> {
    feed: Feed<V>,
    reader: BookReader,
    cmds: Commands,
}

impl<V: Venue> Harness<V> {
    fn new(venue: V) -> Self {
        let (publisher, reader) = book_channel();
        Harness {
            feed: Feed::new(venue, publisher, FeedConfig::default(), 7),
            reader,
            cmds: Commands::default(),
        }
    }
    fn connect(&mut self, now: u64) -> Vec<Command> {
        assert!(self.feed.ready_to_connect(now));
        self.feed.on_connected(now, &mut self.cmds);
        self.cmds.drain()
    }
    fn text(&mut self, now: u64, text: &str) -> Vec<Command> {
        self.feed.on_text(now, text.as_bytes(), &mut self.cmds);
        self.cmds.drain()
    }
    fn tick(&mut self, now: u64) -> Vec<Command> {
        self.feed.on_tick(now, &mut self.cmds);
        self.cmds.drain()
    }
    fn disconnected(&mut self, now: u64) -> Vec<Command> {
        self.feed.on_disconnected(now, &mut self.cmds);
        self.cmds.drain()
    }
    fn status(&self) -> VenueStatus {
        self.reader.top().status
    }
}

fn sends(cmds: &[Command]) -> Vec<Request> {
    cmds.iter()
        .filter_map(|c| {
            if let Command::Send(r) = c {
                Some(*r)
            } else {
                None
            }
        })
        .collect()
}
fn reconnect_reason(cmds: &[Command]) -> Option<ReconnectReason> {
    assert_eq!(
        cmds.iter().filter(|c| **c == Command::Reconnect).count(),
        cmds.iter()
            .filter(|c| matches!(c, Command::Log(FeedEvent::Reconnecting(_))))
            .count()
    );
    cmds.iter().find_map(|c| {
        if let Command::Log(FeedEvent::Reconnecting(r)) = c {
            Some(*r)
        } else {
            None
        }
    })
}
fn logged(cmds: &[Command], event: FeedEvent) -> bool {
    cmds.contains(&Command::Log(event))
}

// ---------------------------------------------------------------------------
// Binance
// ---------------------------------------------------------------------------

#[test]
fn binance_connects_subscribes_and_goes_live() {
    let mut h = Harness::new(Binance::new(spec(), "BTCUSDT"));
    assert_eq!(h.status(), VenueStatus::Down, "nothing published yet");
    let cmds = h.connect(0);
    assert_eq!(sends(&cmds), [Request::Subscribe]);
    assert!(logged(&cmds, FeedEvent::Connected));
    assert_eq!(h.status(), VenueStatus::Syncing, "connected, no book yet");
    assert_eq!(
        h.feed.venue().request(Request::Subscribe).unwrap(),
        br#"{"method":"SUBSCRIBE","params":["btcusdt@depth20@100ms"],"id":1}"#
    );

    let frames: Vec<&str> = BINANCE.lines().collect();
    assert!(
        h.text(MS, frames[0]).is_empty(),
        "the subscribe reply changes nothing"
    );
    let cmds = h.text(2 * MS, frames[1]);
    assert!(logged(&cmds, FeedEvent::Live));
    assert_eq!(h.status(), VenueStatus::Live);
    for (i, f) in frames[2..].iter().enumerate() {
        assert!(h.text((3 + i as u64) * MS, f).is_empty());
    }
    let s = h.feed.stats();
    assert_eq!((s.messages, s.applied, s.gaps, s.resyncs), (60, 59, 0, 0));
    // The published top of book is the last snapshot's.
    let top = h.reader.top();
    assert_eq!(top.bid.unwrap().px, spec().parse_px("58647.4").unwrap());
    assert_eq!(
        top.recv_ns,
        60 * MS,
        "stamped with our receive time (last frame at 60 ms)"
    );
}

#[test]
fn binance_gap_is_counted_but_the_next_snapshot_repairs_it() {
    let mut h = Harness::new(Binance::new(spec(), "BTCUSDT"));
    h.connect(0);
    let frames: Vec<&str> = BINANCE.lines().collect();
    for (i, f) in frames.iter().enumerate().filter(|(i, _)| *i != 10) {
        let cmds = h.text(i as u64 * MS, f);
        assert_eq!(logged(&cmds, FeedEvent::Gap), i == 11, "frame {i}");
        assert!(sends(&cmds).is_empty(), "no resync for a snapshot stream");
    }
    assert_eq!((h.feed.stats().gaps, h.feed.stats().resyncs), (1, 0));
    assert_eq!(h.status(), VenueStatus::Live);
}

#[test]
fn binance_bad_message_is_dropped_and_the_book_kept() {
    let mut h = Harness::new(Binance::new(spec(), "BTCUSDT"));
    h.connect(0);
    let frames: Vec<&str> = BINANCE.lines().collect();
    h.text(MS, frames[1]);
    let before = h.reader.top();
    let cmds = h.text(2 * MS, &frames[2].replacen("58", "5x", 1));
    assert!(matches!(cmds[..], [Command::Log(FeedEvent::Rejected(_))]));
    assert_eq!(h.status(), VenueStatus::Live);
    assert_eq!(
        h.reader.top().bid,
        before.bid,
        "previous snapshot still published"
    );
    assert_eq!(h.feed.stats().rejected, 1);
}

#[test]
fn binance_needs_no_heartbeat_but_goes_stale_without_data() {
    let mut h = Harness::new(Binance::new(spec(), "BTCUSDT"));
    h.connect(0);
    h.text(MS, BINANCE.lines().nth(1).unwrap());
    assert!(
        sends(&h.tick(4 * SEC)).is_empty(),
        "Binance pings us, not the other way round"
    );
    assert_eq!(
        reconnect_reason(&h.tick(5 * SEC)),
        None,
        "5 s exactly: still fine"
    );
    assert_eq!(
        reconnect_reason(&h.tick(5 * SEC + 2 * MS)),
        Some(ReconnectReason::Stale)
    );
}

// ---------------------------------------------------------------------------
// Bybit
// ---------------------------------------------------------------------------

#[test]
fn bybit_snapshot_then_deltas() {
    let mut h = Harness::new(Bybit::new(spec(), "BTCUSDT"));
    h.connect(0);
    for (i, f) in BYBIT.lines().enumerate() {
        h.text(i as u64 * MS, f);
    }
    let s = h.feed.stats();
    assert_eq!((s.messages, s.applied, s.resyncs), (60, 59, 0));
    assert_eq!(h.status(), VenueStatus::Live);
    assert_eq!(
        h.reader.top().bid.unwrap().px,
        spec().parse_px("58656.2").unwrap()
    );
}

#[test]
fn bybit_gap_resyncs_and_the_aggregator_stops_using_it_until_a_fresh_snapshot() {
    let mut h = Harness::new(Bybit::new(spec(), "BTCUSDT"));
    let mut agg = Aggregator::new(Config {
        max_age_ns: u64::MAX,
        ..Config::default()
    });
    let id = agg
        .add_venue(
            VenueConfig {
                name: "bybit",
                depth: 50,
                taker_fee_ppm: 550,
            },
            h.reader.clone(),
        )
        .unwrap();
    h.connect(0);
    let frames: Vec<&str> = BYBIT.lines().collect();
    for f in &frames[..5] {
        h.text(MS, f);
    }
    agg.refresh();
    assert_eq!(agg.state(id, 0), Some(VenueState::Included));

    // Frame 5 is lost: frame 6 arrives out of sequence.
    let cmds = h.text(2 * MS, frames[6]);
    let u = |f: &str| {
        f.split("\"u\":")
            .nth(1)
            .unwrap()
            .split(',')
            .next()
            .unwrap()
            .parse::<u64>()
            .unwrap()
    };
    assert!(logged(
        &cmds,
        FeedEvent::Resync(ResyncReason::Gap {
            expected: u(frames[5]),
            got: u(frames[6])
        })
    ));
    assert_eq!(sends(&cmds), [Request::Unsubscribe, Request::Subscribe]);
    assert!(matches!(h.feed.state(), State::Syncing { .. }));
    agg.refresh();
    assert_eq!(
        agg.state(id, 0),
        Some(VenueState::Excluded(Exclusion::Syncing))
    );

    // Deltas from the old subscription keep arriving: ignored, no new resync.
    for f in &frames[7..10] {
        assert!(h.text(3 * MS, f).is_empty());
    }
    // The fresh snapshot after resubscribing restores the book and the venue.
    let cmds = h.text(4 * MS, frames[0]);
    assert!(logged(&cmds, FeedEvent::Live));
    h.text(5 * MS, frames[1]); // and deltas apply on top of it again
    agg.refresh();
    assert_eq!(agg.state(id, 0), Some(VenueState::Included));
    assert_eq!(h.feed.stats().resyncs, 1);
}

#[test]
fn bybit_unparseable_or_crossed_data_resyncs() {
    let mut h = Harness::new(Bybit::new(spec(), "BTCUSDT"));
    h.connect(0);
    let frames: Vec<&str> = BYBIT.lines().collect();
    h.text(MS, frames[0]);
    let cmds = h.text(2 * MS, "{not json");
    assert!(matches!(
        cmds[0],
        Command::Log(FeedEvent::Resync(ResyncReason::Unparseable(_)))
    ));

    // A delta that puts a bid above the best ask: our copy can't be right.
    h.text(3 * MS, frames[0]);
    let crossing = frames[1].replacen(r#""b":[]"#, r#""b":[["99999.0","1.000"]]"#, 1);
    let cmds = h.text(4 * MS, &crossing);
    assert!(logged(&cmds, FeedEvent::Resync(ResyncReason::Crossed)));
}

#[test]
fn bybit_heartbeat_every_20s_and_reconnect_if_unanswered() {
    let mut h = Harness::new(Bybit::new(spec(), "BTCUSDT"));
    h.connect(0);
    let pong = r#"{"success":true,"ret_msg":"pong","conn_id":"x","req_id":"hb","op":"ping"}"#;
    let snap = BYBIT.lines().next().unwrap();
    // Keep data flowing so staleness doesn't interfere.
    let mut t = 0;
    let mut pings = Vec::new();
    while t <= 41 * SEC {
        h.text(t, snap);
        if !sends(&h.tick(t)).is_empty() {
            pings.push(t / SEC);
        }
        if t == 21 * SEC {
            h.text(t, pong); // the first ping is answered
        }
        t += SEC / 2;
    }
    assert_eq!(pings, [20, 40], "a ping every 20 s");
    // The second ping (at 40 s) is never answered: after the 10 s pong timeout, reconnect.
    for t in (41..=50).map(|s| s * SEC) {
        h.text(t, snap);
        assert_eq!(reconnect_reason(&h.tick(t)), None, "{t}");
    }
    h.text(51 * SEC, snap);
    assert_eq!(
        reconnect_reason(&h.tick(51 * SEC)),
        Some(ReconnectReason::PongTimeout)
    );
    assert_eq!(
        h.feed.venue().request(Request::Ping).unwrap(),
        br#"{"req_id":"hb","op":"ping"}"#
    );
}

#[test]
fn a_refused_subscription_reconnects() {
    let mut h = Harness::new(Bybit::new(spec(), "BTCUSDT"));
    h.connect(0);
    let refused = r#"{"success":false,"ret_msg":"error:handler not found","conn_id":"x","req_id":"sub","op":"subscribe"}"#;
    assert_eq!(
        reconnect_reason(&h.text(MS, refused)),
        Some(ReconnectReason::SubscribeRefused)
    );
}

#[test]
fn too_many_resyncs_escalate_to_a_reconnect() {
    let mut h = Harness::new(Bybit::new(spec(), "BTCUSDT"));
    h.connect(0);
    for i in 0..5 {
        assert_eq!(
            reconnect_reason(&h.text(i * SEC, "garbage")),
            None,
            "resync {i}"
        );
    }
    assert_eq!(
        reconnect_reason(&h.text(6 * SEC, "garbage")),
        Some(ReconnectReason::TooManyResyncs)
    );
    // Outside the 60 s window the count starts again.
    let mut h = Harness::new(Bybit::new(spec(), "BTCUSDT"));
    h.connect(0);
    for i in 0..8 {
        h.text(i * 61 * SEC, BYBIT.lines().next().unwrap()); // keep it alive
        assert_eq!(
            reconnect_reason(&h.text(i * 61 * SEC, "garbage")),
            None,
            "{i}"
        );
    }
}

// ---------------------------------------------------------------------------
// Timeouts and backoff
// ---------------------------------------------------------------------------

#[test]
fn no_book_after_subscribing_times_out() {
    let reply = r#"{"success":true,"ret_msg":"","conn_id":"x","req_id":"sub","op":"subscribe"}"#;
    // Messages keep arriving (here, replies), but never a book: the subscribe timeout.
    let mut h = Harness::new(Bybit::new(spec(), "BTCUSDT"));
    h.connect(0);
    for t in (1..=10).map(|s| s * SEC) {
        h.text(t, reply);
        assert_eq!(reconnect_reason(&h.tick(t)), None, "{t}");
    }
    h.text(10 * SEC + 1, reply);
    assert_eq!(
        reconnect_reason(&h.tick(10 * SEC + 1)),
        Some(ReconnectReason::SubscribeTimeout)
    );
    // Total silence after subscribing is a dead connection, whatever the state: the
    // (shorter) staleness rule fires first.
    let mut h = Harness::new(Bybit::new(spec(), "BTCUSDT"));
    h.connect(0);
    h.text(SEC, reply);
    assert_eq!(reconnect_reason(&h.tick(6 * SEC)), None);
    assert_eq!(
        reconnect_reason(&h.tick(6 * SEC + 1)),
        Some(ReconnectReason::Stale)
    );
}

#[test]
fn a_resync_that_never_gets_a_snapshot_times_out() {
    let mut h = Harness::new(Bybit::new(spec(), "BTCUSDT"));
    h.connect(0);
    let frames: Vec<&str> = BYBIT.lines().collect();
    h.text(0, frames[0]);
    h.text(MS, frames[3]); // gap → syncing at 1 ms
    // Old deltas keep the connection "alive", but no snapshot comes back.
    let mut reason = None;
    for t in (1..=12).map(|s| s * SEC) {
        h.text(t, frames[5]);
        reason = reason.or(reconnect_reason(&h.tick(t)));
    }
    assert_eq!(reason, Some(ReconnectReason::SyncTimeout));
}

#[test]
fn disconnects_back_off_exponentially_and_reset_once_healthy() {
    let mut h = Harness::new(Binance::new(spec(), "BTCUSDT"));
    let snap = BINANCE.lines().nth(1).unwrap();
    let mut now = 0;
    let mut delays = Vec::new();
    for _ in 0..6 {
        h.connect(now);
        h.text(now, snap);
        let cmds = h.disconnected(now + SEC);
        assert_eq!(h.status(), VenueStatus::Down);
        let Some(Command::Log(FeedEvent::Disconnected { retry_in })) = cmds.last().copied() else {
            panic!()
        };
        delays.push(retry_in);
        let State::Disconnected { retry_at_ns } = h.feed.state() else {
            panic!()
        };
        assert!(!h.feed.ready_to_connect(retry_at_ns - 1));
        now = retry_at_ns;
    }
    for (d, cap_ms) in delays.iter().zip([250_u64, 500, 1000, 2000, 4000, 8000]) {
        assert!(
            *d >= Duration::from_millis(cap_ms / 2) && *d <= Duration::from_millis(cap_ms),
            "{d:?} vs {cap_ms} ms"
        );
    }
    // A connection that stays live for 30 s resets the backoff.
    h.connect(now);
    for t in 0..=31 {
        h.text(now + t * SEC, snap);
        h.tick(now + t * SEC);
    }
    let cmds = h.disconnected(now + 32 * SEC);
    let Some(Command::Log(FeedEvent::Disconnected { retry_in })) = cmds.last().copied() else {
        panic!()
    };
    assert!(
        retry_in <= Duration::from_millis(250),
        "back to the first step: {retry_in:?}"
    );
}

#[test]
fn a_loss_reported_twice_counts_once_but_failed_connects_keep_backing_off() {
    let mut h = Harness::new(Binance::new(spec(), "BTCUSDT"));
    h.connect(0);
    assert_eq!(h.disconnected(SEC).len(), 1);
    assert!(h.disconnected(SEC).is_empty(), "already down: nothing more");
    assert_eq!(h.feed.stats().disconnects, 1);
    // Repeated connect failures while disconnected must each advance the backoff.
    let mut cmds = Commands::default();
    let mut last = 0;
    for i in 0..5 {
        h.feed.on_connect_failed(10 * SEC, &mut cmds);
        let State::Disconnected { retry_at_ns } = h.feed.state() else {
            panic!()
        };
        assert!(
            retry_at_ns > last || i == 0,
            "backoff advances on every failure"
        );
        last = retry_at_ns;
    }
    assert_eq!(h.feed.stats().disconnects, 6);
}
