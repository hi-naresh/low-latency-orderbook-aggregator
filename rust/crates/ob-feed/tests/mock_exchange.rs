//! Integration tests against a **mock exchange**: a local TLS WebSocket server that plays
//! scripted sessions (recorded real messages, gaps, silence, garbage, abrupt hang-ups,
//! refused connections) to the *real* driver, feed, socket, TLS and WebSocket code.
//!
//! The state-machine tests (`feed.rs`) prove the decisions with a fake clock; these prove
//! that the decisions are carried out over a real connection: requests reach the server,
//! closes are clean, a dead socket is noticed, retries really wait, and the aggregator
//! drops and re-admits the venue at the right moments.
//!
//! Timings are scaled down (milliseconds instead of seconds) through `FeedConfig`.
#![allow(
    clippy::unwrap_used,
    clippy::cast_possible_truncation,
    clippy::print_stdout
)]

use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use ob_agg::{Aggregator, Config, Exclusion, VenueConfig, VenueId, VenueState};
use ob_core::InstrumentSpec;
use ob_feed::driver::{self, DriverEvent};
use ob_feed::{
    BackoffConfig, Binance, Bybit, Commands, Feed, FeedConfig, FeedEvent, FeedStats,
    ReconnectReason, ResyncReason, Venue, mono_ns,
};
use ob_net::ws::{Opcode, encode_server_frame, frame::apply_mask, handshake};
use ob_net::{ConnConfig, tls};
use ob_sync::{BookReader, Snapshot, book_channel};
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};
use rustls::{RootCertStore, ServerConfig, ServerConnection, StreamOwned};

const BINANCE: &str = include_str!("../../ob-parse/tests/fixtures/binance_depth20.jsonl");
const BYBIT: &str = include_str!("../../ob-parse/tests/fixtures/bybit_orderbook50.jsonl");
const MS: Duration = Duration::from_millis(1);

fn spec() -> InstrumentSpec {
    InstrumentSpec::from_steps("0.1", "0.001").unwrap()
}
fn binance() -> Binance {
    Binance::new(spec(), "BTCUSDT")
}
fn bybit() -> Bybit {
    Bybit::new(spec(), "BTCUSDT")
}
fn frames(fixture: &'static str) -> Vec<&'static str> {
    fixture.lines().collect()
}

/// Every timing rule scaled down so a test takes well under a second per scenario.
fn fast() -> FeedConfig {
    FeedConfig {
        subscribe_timeout: 1000 * MS,
        stale_after: 500 * MS,
        pong_timeout: 300 * MS,
        healthy_after: 2000 * MS,
        max_resyncs: 5,
        resync_window: Duration::from_secs(60),
        backoff: BackoffConfig {
            base: 50 * MS,
            max: 400 * MS,
        },
    }
}

// ---------------------------------------------------------------------------
// The mock exchange.

/// The server side of one connection.
struct Session {
    stream: StreamOwned<ServerConnection, TcpStream>,
}

impl Session {
    /// Completes TLS and the WebSocket upgrade.
    fn handshake(&mut self) {
        let mut request = Vec::new();
        let mut byte = [0_u8];
        while !request.ends_with(b"\r\n\r\n") {
            self.stream.read_exact(&mut byte).unwrap();
            request.push(byte[0]);
        }
        let request = String::from_utf8(request).unwrap();
        let key = request
            .lines()
            .find_map(|l| l.strip_prefix("Sec-WebSocket-Key: "))
            .unwrap()
            .trim();
        let response = format!(
            "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {}\r\n\r\n",
            handshake::expected_accept(key)
        );
        self.raw(response.as_bytes());
    }

    /// Handshake, then the client's first message must be its subscription.
    fn accept_subscription(&mut self) -> String {
        self.handshake();
        let sub = self.read_text();
        assert!(sub.to_ascii_lowercase().contains("subscribe"), "{sub}");
        sub
    }

    fn raw(&mut self, bytes: &[u8]) {
        self.stream.write_all(bytes).unwrap();
        self.stream.flush().unwrap();
    }

    fn frame(&mut self, opcode: Opcode, payload: &[u8]) {
        let mut out = Vec::new();
        encode_server_frame(&mut out, true, opcode, payload);
        self.raw(&out);
    }

    fn text(&mut self, text: &str) {
        self.frame(Opcode::Text, text.as_bytes());
    }

    fn texts(&mut self, texts: &[&str]) {
        for t in texts {
            self.text(t);
        }
    }

    /// One frame from the client: (opcode, unmasked payload). `None` if the connection
    /// ended (EOF, error or timeout).
    fn read_frame(&mut self) -> Option<(u8, Vec<u8>)> {
        let mut h = [0_u8; 2];
        self.stream.read_exact(&mut h).ok()?;
        assert!(h[1] & 0x80 != 0, "client frames must be masked");
        let len = match h[1] & 0x7F {
            126 => {
                let mut l = [0_u8; 2];
                self.stream.read_exact(&mut l).ok()?;
                usize::from(u16::from_be_bytes(l))
            }
            127 => panic!("our client never sends 64 KiB frames"),
            n => usize::from(n),
        };
        let mut key = [0_u8; 4];
        self.stream.read_exact(&mut key).ok()?;
        let mut payload = vec![0; len];
        self.stream.read_exact(&mut payload).ok()?;
        apply_mask(&mut payload, key);
        Some((h[0] & 0x0F, payload))
    }

    fn read_text(&mut self) -> String {
        let (op, payload) = self.read_frame().expect("connection ended");
        assert_eq!(op, 0x1, "expected a text frame");
        String::from_utf8(payload).unwrap()
    }

    /// Reads until the client's close frame; returns its status code, or `None` if the
    /// connection ended without one. Text frames (e.g. heartbeats) are collected.
    fn until_close(&mut self) -> (Option<u16>, Vec<String>) {
        let mut texts = Vec::new();
        while let Some((op, payload)) = self.read_frame() {
            match op {
                0x1 => texts.push(String::from_utf8(payload).unwrap()),
                0x8 => return (Some(u16::from_be_bytes([payload[0], payload[1]])), texts),
                _ => {}
            }
        }
        (None, texts)
    }

    /// Drops the TCP connection with no WebSocket close and no TLS close_notify: what a
    /// crashed server, a NAT timeout or a pulled cable looks like to the client.
    fn hang_up(self) {
        let _ = self.stream.sock.shutdown(Shutdown::Both);
    }
}

type Script = Box<dyn FnOnce(Session) + Send>;

struct MockExchange {
    cfg: ConnConfig,
    /// When each connection was accepted.
    accepted: Arc<Mutex<Vec<Instant>>>,
    server: JoinHandle<()>,
}

impl MockExchange {
    /// Serves one script per incoming connection, in order. After the last one the
    /// listener closes, so further attempts are refused (the venue is "down").
    fn start(scripts: Vec<Script>) -> Self {
        let ck = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        let cert: CertificateDer<'static> = ck.cert.der().clone();
        let key = PrivatePkcs8KeyDer::from(ck.signing_key.serialize_der());
        let server_cfg = Arc::new(
            ServerConfig::builder_with_provider(tls::crypto_provider())
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_no_client_auth()
                .with_single_cert(vec![cert.clone()], key.into())
                .unwrap(),
        );
        let mut roots = RootCertStore::empty();
        roots.add(cert).unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut cfg = ConnConfig::new("localhost", "/ws").unwrap();
        cfg.port = listener.local_addr().unwrap().port();
        cfg.tls = tls::client_config_with_roots(roots).unwrap();
        cfg.connect_timeout = Duration::from_secs(2);

        let accepted = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&accepted);
        let server = thread::spawn(move || {
            for script in scripts {
                let (sock, _) = listener.accept().unwrap();
                log.lock().unwrap().push(Instant::now());
                sock.set_nodelay(true).unwrap();
                // A broken test must fail, not hang.
                sock.set_read_timeout(Some(Duration::from_secs(10)))
                    .unwrap();
                let conn = ServerConnection::new(Arc::clone(&server_cfg)).unwrap();
                script(Session {
                    stream: StreamOwned::new(conn, sock),
                });
            }
        });
        MockExchange {
            cfg,
            accepted,
            server,
        }
    }

    /// Waits for every script to finish; a failed assertion inside one fails the test.
    fn finish(self) -> Vec<Instant> {
        self.server.join().expect("a server script failed");
        Arc::try_unwrap(self.accepted)
            .unwrap()
            .into_inner()
            .unwrap()
    }
}

/// A gate the server script waits on, so the test can inspect state at a fixed point.
fn gate() -> (Sender<()>, Receiver<()>) {
    mpsc::channel()
}

// ---------------------------------------------------------------------------
// The client side: the real driver on its own thread, as in production.

/// What the driver reported (owned, so it can cross threads).
#[derive(Clone, Debug)]
enum Ev {
    Feed(FeedEvent),
    #[allow(dead_code)] // read through `Debug` in failure messages
    ConnectFailed(String),
    Lost(String),
}

struct Client {
    reader: BookReader,
    events: Receiver<(Instant, Ev)>,
    seen: Vec<(Instant, Ev)>,
    stop: Arc<AtomicBool>,
    thread: JoinHandle<FeedStats>,
}

impl Client {
    fn start<V: Venue + Send + 'static>(venue: V, conn: ConnConfig, cfg: FeedConfig) -> Self {
        Self::start_with_rest(venue, conn, None, cfg)
    }

    /// With a REST server for snapshots (full depth).
    fn start_with_rest<V: Venue + Send + 'static>(
        venue: V,
        conn: ConnConfig,
        rest: Option<ConnConfig>,
        cfg: FeedConfig,
    ) -> Self {
        let (publisher, reader) = book_channel();
        let (tx, events) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let thread = thread::spawn(move || {
            let mut feed = Feed::new(venue, publisher, cfg, 42);
            driver::run_full(
                &mut feed,
                &conn,
                rest.as_ref(),
                &flag,
                |_, event| {
                    let ev = match event {
                        DriverEvent::Feed(e) => Ev::Feed(e),
                        DriverEvent::ConnectFailed(e) => Ev::ConnectFailed(e.to_string()),
                        DriverEvent::ConnectionLost(e) | DriverEvent::SnapshotFailed(e) => {
                            Ev::Lost(e.to_string())
                        }
                    };
                    let _ = tx.send((Instant::now(), ev));
                },
                |_, _| {},
            );
            feed.stats()
        });
        Client {
            reader,
            events,
            seen: Vec::new(),
            stop,
            thread,
        }
    }

    /// Waits (up to 5 s) for an event matching `want`; returns when it happened.
    fn wait_for(&mut self, what: &str, want: impl Fn(&Ev) -> bool) -> (Instant, Ev) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.events.recv_timeout(left) {
                Ok(e) => {
                    self.seen.push(e.clone());
                    if want(&e.1) {
                        return e;
                    }
                }
                Err(_) => panic!("no {what} within 5 s; saw {:#?}", self.seen),
            }
        }
    }

    fn wait_feed(&mut self, event: FeedEvent) -> Instant {
        self.wait_for(
            &format!("{event:?}"),
            |e| matches!(e, Ev::Feed(f) if *f == event),
        )
        .0
    }

    /// Stops the driver (it closes the connection cleanly) and returns its counters and
    /// every event it reported.
    fn stop(mut self) -> (FeedStats, Vec<Ev>) {
        self.stop.store(true, Ordering::Relaxed);
        let stats = self.thread.join().unwrap();
        self.seen.extend(self.events.try_iter());
        (stats, self.seen.into_iter().map(|(_, e)| e).collect())
    }
}

/// An aggregator over one or more feeds. Max age is effectively infinite unless a test
/// says otherwise, so only the feed's own status decides.
fn aggregator(venues: &[(&'static str, &BookReader)], max_age: Duration) -> Aggregator {
    let mut agg = Aggregator::new(Config {
        max_age_ns: u64::try_from(max_age.as_nanos()).unwrap_or(u64::MAX),
        ..Config::default()
    });
    for (name, reader) in venues {
        agg.add_venue(
            VenueConfig {
                name,
                depth: 50,
                taker_fee_ppm: 500,
            },
            (*reader).clone(),
        )
        .unwrap();
    }
    agg
}
fn state(agg: &mut Aggregator, id: VenueId) -> VenueState {
    agg.refresh();
    agg.state(id, mono_ns()).unwrap()
}
fn first(agg: &Aggregator) -> VenueId {
    agg.venue_ids().next().unwrap()
}

/// The book a feed should publish after `texts`, computed without any I/O.
fn expected_book<V: Venue>(venue: V, texts: &[&str]) -> Snapshot {
    let (publisher, reader) = book_channel();
    let mut feed = Feed::new(venue, publisher, FeedConfig::default(), 0);
    let mut cmds = Commands::default();
    feed.on_connected(0, &mut cmds);
    for t in texts {
        feed.on_text(0, t.as_bytes(), &mut cmds);
    }
    snapshot(&reader)
}
fn snapshot(reader: &BookReader) -> Snapshot {
    let mut s = Snapshot::default();
    reader.read_into(&mut s);
    s
}
fn same_levels(a: &Snapshot, b: &Snapshot) -> bool {
    a.bids() == b.bids() && a.asks() == b.asks()
}

// ---------------------------------------------------------------------------
// Scenarios.

/// Subscribe, snapshot, deltas, a lost delta, resync over the same connection, live
/// again. The aggregator stops using the venue for exactly the resync window.
#[test]
fn bybit_gap_resyncs_over_the_same_connection() {
    let f = frames(BYBIT);
    let (go_gap, gap_gate) = gate();
    let (go_snapshot, snapshot_gate) = gate();
    let script: Script = Box::new(move |mut s| {
        let sub = s.accept_subscription();
        assert_eq!(
            sub,
            r#"{"req_id":"sub","op":"subscribe","args":["orderbook.50.BTCUSDT"]}"#
        );
        s.texts(&f[..5]);
        gap_gate.recv().unwrap();
        s.text(f[6]); // frame 5 lost
        // The client must resubscribe on this connection: unsubscribe, then subscribe.
        assert!(s.read_text().contains(r#""op":"unsubscribe""#));
        assert!(s.read_text().contains(r#""op":"subscribe""#));
        snapshot_gate.recv().unwrap();
        s.texts(&f[..10]); // a fresh snapshot, then deltas
        assert_eq!(s.until_close().0, Some(1000));
    });
    let mock = MockExchange::start(vec![script]);
    let mut client = Client::start(bybit(), mock.cfg.clone(), fast());
    let mut agg = aggregator(&[("bybit", &client.reader)], Duration::MAX);
    let id = first(&agg);

    client.wait_feed(FeedEvent::Live);
    // Wait until all five frames are applied.
    let want = expected_book(bybit(), &frames(BYBIT)[..5]);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !same_levels(&snapshot(&client.reader), &want) {
        assert!(Instant::now() < deadline, "book never matched");
    }
    assert_eq!(state(&mut agg, id), VenueState::Included);

    go_gap.send(()).unwrap();
    client.wait_for("gap resync", |e| {
        matches!(e, Ev::Feed(FeedEvent::Resync(ResyncReason::Gap { .. })))
    });
    // The server is holding the snapshot back: the venue must be out.
    assert_eq!(
        state(&mut agg, id),
        VenueState::Excluded(Exclusion::Syncing)
    );
    assert!(agg.best_bids(mono_ns()).is_empty());

    go_snapshot.send(()).unwrap();
    client.wait_feed(FeedEvent::Live);
    let want = expected_book(bybit(), &frames(BYBIT)[..10]);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !same_levels(&snapshot(&client.reader), &want) {
        assert!(Instant::now() < deadline, "rebuilt book never matched");
    }
    assert_eq!(state(&mut agg, id), VenueState::Included);

    let (stats, _) = client.stop();
    mock.finish();
    assert_eq!(
        (stats.resyncs, stats.reconnects, stats.disconnects),
        (1, 0, 0)
    );
    // Frame 2 of the recording is the subscription acknowledgement, as it arrived live:
    // 5 + 10 frames hold 4 + 9 book updates.
    assert_eq!(stats.applied, 4 + 9);
}

/// The server vanishes mid-stream (no close frame, no TLS close_notify). The venue
/// must leave the aggregator at once, and come back after a short backoff.
#[test]
fn an_abrupt_disconnect_takes_the_venue_out_at_once_then_it_reconnects() {
    let f = frames(BINANCE);
    let (go_drop, drop_gate) = gate();
    let f2 = f.clone();
    let mock = MockExchange::start(vec![
        Box::new(move |mut s| {
            s.accept_subscription();
            s.texts(&f[..10]);
            drop_gate.recv().unwrap();
            s.hang_up();
        }),
        Box::new(move |mut s| {
            s.accept_subscription();
            s.text(f2[10]); // one book: the test stops as soon as it's Live
            assert_eq!(s.until_close().0, Some(1000));
        }),
    ]);
    let mut client = Client::start(binance(), mock.cfg.clone(), fast());
    let mut agg = aggregator(&[("binance", &client.reader)], Duration::MAX);
    let id = first(&agg);
    client.wait_feed(FeedEvent::Live);
    assert_eq!(state(&mut agg, id), VenueState::Included);

    go_drop.send(()).unwrap();
    let (lost_at, lost) = client.wait_for("connection lost", |e| matches!(e, Ev::Lost(_)));
    let (_, down) = client.wait_for("disconnected", |e| {
        matches!(e, Ev::Feed(FeedEvent::Disconnected { .. }))
    });
    assert_eq!(state(&mut agg, id), VenueState::Excluded(Exclusion::Down));
    // First retry: equal jitter on a 50 ms cap → 25–50 ms.
    let Ev::Feed(FeedEvent::Disconnected { retry_in }) = down else {
        unreachable!()
    };
    assert!(retry_in >= 25 * MS && retry_in <= 50 * MS, "{retry_in:?}");

    client.wait_feed(FeedEvent::Connected);
    let live_at = client.wait_feed(FeedEvent::Live);
    assert_eq!(state(&mut agg, id), VenueState::Included);
    let outage = live_at - lost_at;
    println!("lost: {lost:?}; back live after {outage:?}");
    assert!(outage < Duration::from_millis(500), "{outage:?}");

    let (stats, _) = client.stop();
    mock.finish();
    assert_eq!((stats.disconnects, stats.reconnects), (1, 0));
}

/// The connection stays open but nothing arrives. Two independent guards: the
/// aggregator's own age check drops the venue first; then the feed declares the
/// connection dead, closes it cleanly and reconnects.
#[test]
fn a_silent_connection_goes_stale_and_is_replaced() {
    let f = frames(BINANCE);
    let f2 = f.clone();
    let mock = MockExchange::start(vec![
        Box::new(move |mut s| {
            s.accept_subscription();
            s.texts(&f[..5]);
            let quiet_since = Instant::now();
            // Say nothing; the client must give up and close properly.
            assert_eq!(s.until_close().0, Some(1000));
            let waited = quiet_since.elapsed();
            assert!(waited >= 500 * MS, "closed after only {waited:?}");
        }),
        Box::new(move |mut s| {
            s.accept_subscription();
            s.text(f2[5]); // one book: the test stops as soon as it's Live
            assert_eq!(s.until_close().0, Some(1000));
        }),
    ]);
    let mut client = Client::start(binance(), mock.cfg.clone(), fast());
    let mut agg = aggregator(&[("binance", &client.reader)], 100 * MS);
    let id = first(&agg);
    let live_at = client.wait_feed(FeedEvent::Live);
    assert_eq!(state(&mut agg, id), VenueState::Included);

    // 250 ms of silence: the aggregator (max age 100 ms) already excludes it, while the
    // feed (stale after 500 ms) still believes the connection.
    thread::sleep(250 * MS);
    assert!(matches!(
        state(&mut agg, id),
        VenueState::Excluded(Exclusion::Stale { .. })
    ));
    assert!(
        client
            .seen
            .iter()
            .all(|(_, e)| !matches!(e, Ev::Feed(FeedEvent::Reconnecting(_))))
    );

    let stale_at = client.wait_feed(FeedEvent::Reconnecting(ReconnectReason::Stale));
    let silence = stale_at - live_at;
    assert!(
        silence >= 500 * MS && silence < 1000 * MS,
        "stale after {silence:?}"
    );
    client.wait_feed(FeedEvent::Live);
    assert_eq!(state(&mut agg, id), VenueState::Included);

    let (stats, _) = client.stop();
    mock.finish();
    assert_eq!((stats.reconnects, stats.disconnects), (1, 1));
}

/// Binance: the server pings at the WebSocket level, and the transport must answer with
/// the same payload (Binance disconnects clients that don't).
#[test]
fn binance_server_pings_are_answered() {
    let f = frames(BINANCE);
    let mock = MockExchange::start(vec![Box::new(move |mut s| {
        s.accept_subscription();
        s.texts(&f[..2]);
        s.frame(Opcode::Ping, b"1795610780582");
        let (op, payload) = s.read_frame().unwrap();
        assert_eq!((op, payload.as_slice()), (0xA, &b"1795610780582"[..]));
        assert_eq!(s.until_close().0, Some(1000));
    })]);
    let mut client = Client::start(binance(), mock.cfg.clone(), fast());
    client.wait_feed(FeedEvent::Live);
    thread::sleep(100 * MS); // let the pong go out before stopping
    client.stop();
    mock.finish();
}

/// Bybit: the client must ping with `{"op":"ping"}`. One ping is answered; the next is
/// not, so the client must reconnect once the pong deadline passes.
#[test]
fn bybit_heartbeats_and_an_unanswered_ping_forces_a_reconnect() {
    let f = frames(BYBIT);
    let f2 = f.clone();
    let mock = MockExchange::start(vec![
        Box::new(move |mut s| {
            s.accept_subscription();
            s.text(f[0]);
            let ping = s.read_text();
            assert_eq!(ping, r#"{"req_id":"hb","op":"ping"}"#);
            s.text(
                r#"{"success":true,"ret_msg":"pong","conn_id":"mock","req_id":"hb","op":"ping"}"#,
            );
            // From now on, no answers.
            let (close, more) = s.until_close();
            assert_eq!(close, Some(1000));
            assert!(!more.is_empty() && more.iter().all(|t| t == &ping));
        }),
        Box::new(move |mut s| {
            s.accept_subscription();
            s.text(f2[0]);
            let _ = s.until_close();
        }),
    ]);
    let cfg = FeedConfig {
        stale_after: Duration::from_secs(5), // only the heartbeat should fire
        ..fast()
    };
    let mut client = Client::start(bybit().with_heartbeat(200 * MS), mock.cfg.clone(), cfg);
    let live_at = client.wait_feed(FeedEvent::Live);
    let timeout_at = client.wait_feed(FeedEvent::Reconnecting(ReconnectReason::PongTimeout));
    // Ping 1 at 200 ms (answered), ping 2 at 400 ms (due by 700 ms): not before that.
    let after = timeout_at - live_at;
    assert!(after >= 650 * MS && after < 1200 * MS, "{after:?}");
    client.wait_feed(FeedEvent::Live);
    let (stats, _) = client.stop();
    mock.finish();
    assert_eq!(stats.reconnects, 1);
}

/// Bad data at two levels. A malformed JSON message on Binance is dropped and the venue
/// stays in. A malformed WebSocket frame means the stream itself can't be trusted: the
/// connection is dropped and replaced.
#[test]
fn malformed_messages_are_dropped_and_broken_frames_kill_the_connection() {
    let f = frames(BINANCE);
    let f2 = f.clone();
    let (go_frame, frame_gate) = gate();
    let mock = MockExchange::start(vec![
        Box::new(move |mut s| {
            s.accept_subscription();
            s.texts(&f[..3]);
            s.text(r#"{"e":"depthUpdate","E":17956107805"#); // truncated
            s.text(f[3]);
            frame_gate.recv().unwrap();
            s.raw(&[0xC1, 0x00]); // RSV1 set without an extension: a protocol error
            // The client drops the connection (it may or may not manage a close frame).
            let _ = s.until_close();
        }),
        Box::new(move |mut s| {
            s.accept_subscription();
            s.text(f2[4]); // one book: the test stops as soon as it's Live
            assert_eq!(s.until_close().0, Some(1000));
        }),
    ]);
    let mut client = Client::start(binance(), mock.cfg.clone(), fast());
    let mut agg = aggregator(&[("binance", &client.reader)], Duration::MAX);
    let id = first(&agg);

    client.wait_for("rejected", |e| {
        matches!(e, Ev::Feed(FeedEvent::Rejected(_)))
    });
    // The frame after the garbage still applies: the book is that frame's snapshot.
    let want = expected_book(binance(), &frames(BINANCE)[..4]);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !same_levels(&snapshot(&client.reader), &want) {
        assert!(Instant::now() < deadline, "book never matched");
    }
    assert_eq!(state(&mut agg, id), VenueState::Included);

    go_frame.send(()).unwrap();
    let (_, lost) = client.wait_for("connection lost", |e| matches!(e, Ev::Lost(_)));
    let Ev::Lost(reason) = lost else {
        unreachable!()
    };
    assert!(reason.contains("ReservedBits"), "{reason}");
    client.wait_feed(FeedEvent::Live);
    assert_eq!(state(&mut agg, id), VenueState::Included);

    let (stats, _) = client.stop();
    mock.finish();
    assert_eq!((stats.rejected, stats.disconnects), (1, 1));
}

/// A venue that refuses every connection: retries must wait longer each time (with
/// jitter inside known bounds), never hammer the venue, and stop as soon as it's back.
#[test]
fn a_venue_that_is_down_is_retried_with_growing_delays() {
    let f = frames(BINANCE);
    let mut scripts: Vec<Script> = (0..5)
        .map(|_| Box::new(|s: Session| drop(s)) as Script) // accept, then close: TLS fails
        .collect();
    scripts.push(Box::new(move |mut s| {
        s.accept_subscription();
        s.texts(&f[..2]); // ack + one book: the test stops as soon as it's Live
        assert_eq!(s.until_close().0, Some(1000));
    }));
    let mock = MockExchange::start(scripts);
    let mut client = Client::start(binance(), mock.cfg.clone(), fast());
    client.wait_feed(FeedEvent::Live);
    let (stats, events) = client.stop();
    let accepted = mock.finish();

    let failures = events
        .iter()
        .filter(|e| matches!(e, Ev::ConnectFailed(_)))
        .count();
    assert_eq!(failures, 5);
    assert_eq!(stats.disconnects, 5);
    // Gap before attempt n+1: jitter in [cap/2, cap], cap = 50, 100, 200, 400, 400 ms.
    let caps = [50, 100, 200, 400, 400];
    let gaps: Vec<Duration> = accepted.windows(2).map(|w| w[1] - w[0]).collect();
    println!("gaps between attempts: {gaps:?}");
    for (gap, cap) in gaps.iter().zip(caps) {
        let cap = Duration::from_millis(cap);
        // Upper bound: plus the failed TLS handshake and scheduling slack.
        assert!(
            *gap >= cap / 2 && *gap <= cap + 100 * MS,
            "{gap:?} vs cap {cap:?}"
        );
    }
}

/// Two venues on two threads. One goes down and stays down; the other must keep
/// updating at full rate, and the aggregator must keep quoting it alone.
#[test]
fn one_venue_down_does_not_disturb_the_other() {
    let bn_frames = frames(BINANCE);
    let bb_frames = frames(BYBIT);
    let streaming = Arc::new(AtomicBool::new(true));
    let keep = Arc::clone(&streaming);
    let binance_mock = MockExchange::start(vec![Box::new(move |mut s| {
        s.accept_subscription();
        s.text(bn_frames[0]);
        // A snapshot every 10 ms until the test is over.
        for f in bn_frames[1..].iter().cycle() {
            if !keep.load(Ordering::Relaxed) {
                break;
            }
            s.text(f);
            thread::sleep(10 * MS);
        }
        assert_eq!(s.until_close().0, Some(1000));
    })]);
    let (go_drop, drop_gate) = gate();
    let bybit_mock = MockExchange::start(vec![Box::new(move |mut s| {
        s.accept_subscription();
        s.texts(&bb_frames[..10]);
        drop_gate.recv().unwrap();
        s.hang_up(); // and the listener closes: every reconnect is refused
    })]);

    let mut bn = Client::start(binance(), binance_mock.cfg.clone(), fast());
    let mut bb = Client::start(bybit(), bybit_mock.cfg.clone(), fast());
    let mut agg = aggregator(&[("binance", &bn.reader), ("bybit", &bb.reader)], 200 * MS);
    let ids: Vec<VenueId> = agg.venue_ids().collect();
    bn.wait_feed(FeedEvent::Live);
    bb.wait_feed(FeedEvent::Live);
    thread::sleep(20 * MS);
    agg.refresh();
    assert_eq!(agg.best_bids(mono_ns()).len(), 2);

    go_drop.send(()).unwrap();
    bb.wait_for("bybit lost", |e| matches!(e, Ev::Lost(_)));
    let before = bn.reader.top().recv_ns;
    for _ in 0..10 {
        thread::sleep(30 * MS);
        assert_eq!(state(&mut agg, ids[0]), VenueState::Included);
        assert_eq!(
            state(&mut agg, ids[1]),
            VenueState::Excluded(Exclusion::Down)
        );
        let bids = agg.best_bids(mono_ns());
        assert_eq!(bids.len(), 1);
        assert_eq!(bids.first().unwrap().venue, ids[0]);
    }
    // Binance kept publishing throughout (about 30 snapshots in 300 ms).
    let after = bn.reader.top().recv_ns;
    assert!(after > before);

    streaming.store(false, Ordering::Relaxed);
    let (bn_stats, _) = bn.stop();
    let (bb_stats, bb_events) = bb.stop();
    binance_mock.finish();
    bybit_mock.finish();
    assert_eq!(bn_stats.disconnects, 0);
    assert!(bn_stats.applied > 20, "{bn_stats:?}"); // ~30 at a 10 ms pace, minus scheduling slack
    assert!(
        bb_events.iter().any(|e| matches!(e, Ev::ConnectFailed(_))),
        "bybit should have kept retrying"
    );
    assert!(bb_stats.disconnects >= 2);
}

// ---------------------------------------------------------------------------
// Full depth, with a REST snapshot server.

/// The REST side: reads one HTTP request and answers `status` with `body`. Returns the
/// request line it got (via the server script's assertion on it).
fn rest_script(status: &'static str, body: String, path_has: &'static str) -> Script {
    Box::new(move |mut s| {
        let mut req = Vec::new();
        let mut b = [0_u8];
        while !req.ends_with(b"\r\n\r\n") {
            s.stream.read_exact(&mut b).unwrap();
            req.push(b[0]);
        }
        let req = String::from_utf8(req).unwrap();
        assert!(
            req.starts_with("GET /fapi/v1/depth?symbol=BTCUSDT&limit=1000 HTTP/1.1"),
            "{req}"
        );
        assert!(req.contains(path_has), "{req}");
        let resp = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        s.raw(resp.as_bytes());
    })
}

fn full_diff(first: u64, last: u64, prev: u64, bid: &str, qty: &str) -> String {
    format!(
        r#"{{"e":"depthUpdate","E":7,"T":7,"s":"BTCUSDT","U":{first},"u":{last},"pu":{prev},"b":[["{bid}","{qty}"]],"a":[]}}"#
    )
}

fn full_snapshot(id: u64) -> String {
    let bids: Vec<String> = (0..100)
        .map(|i| format!(r#"["{}.0","1.000"]"#, 1000 - i))
        .collect();
    let asks: Vec<String> = (0..100)
        .map(|i| format!(r#"["{}.0","1.000"]"#, 1001 + i))
        .collect();
    format!(
        r#"{{"lastUpdateId":{id},"E":5,"T":5,"bids":[{}],"asks":[{}]}}"#,
        bids.join(","),
        asks.join(",")
    )
}

/// Subscribe to the diff stream → first event → REST snapshot → buffered + later events
/// stitched on → Live, with the exact book. All over real sockets and TLS.
#[test]
fn full_depth_syncs_from_a_rest_snapshot_over_real_connections() {
    let (go, gate_rx) = gate();
    let ws = MockExchange::start(vec![Box::new(move |mut s| {
        let sub = s.accept_subscription();
        assert!(sub.contains("btcusdt@depth@100ms"), "{sub}");
        s.text(&full_diff(90, 101, 89, "1000.0", "5")); // straddles L = 100
        gate_rx.recv().unwrap(); // the client has the snapshot
        s.text(&full_diff(102, 105, 101, "1000.0", "7"));
        assert_eq!(s.until_close().0, Some(1000));
    })]);
    let rest = MockExchange::start(vec![rest_script(
        "200 OK",
        full_snapshot(100),
        "Host: localhost",
    )]);
    let mut client = Client::start_with_rest(
        ob_feed::BinanceFullDepth::new(spec(), "BTCUSDT"),
        ws.cfg.clone(),
        Some(rest.cfg.clone()),
        fast(),
    );
    client.wait_feed(FeedEvent::FetchingSnapshot);
    client.wait_feed(FeedEvent::Live);
    // The buffered event (u=101) was applied on top of the snapshot.
    assert_eq!(client.reader.top().bid.unwrap().qty.raw(), 5000);
    go.send(()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while client.reader.top().bid.unwrap().qty.raw() != 7000 {
        assert!(Instant::now() < deadline, "the later event never applied");
    }
    let mut snap = Snapshot::default();
    client.reader.read_into(&mut snap);
    assert_eq!(
        (snap.bids().len(), snap.asks().len()),
        (100, 100),
        "the whole 100-level book is published (DEPTH is 256)"
    );
    assert_eq!(snap.meta.update_id, 105);
    let (stats, _) = client.stop();
    ws.finish();
    rest.finish();
    assert_eq!((stats.resyncs, stats.reconnects), (0, 0));
}

/// A rate-limited snapshot (HTTP 429): reconnect with backoff rather than hammering the
/// REST endpoint; the second attempt succeeds.
#[test]
fn a_failed_snapshot_fetch_reconnects_and_retries() {
    let ws = MockExchange::start(vec![
        Box::new(|mut s| {
            s.accept_subscription();
            s.text(&full_diff(90, 101, 89, "1000.0", "5"));
            let _ = s.until_close();
        }),
        Box::new(|mut s| {
            s.accept_subscription();
            s.text(&full_diff(90, 101, 89, "1000.0", "5"));
            assert_eq!(s.until_close().0, Some(1000));
        }),
    ]);
    let rest = MockExchange::start(vec![
        rest_script("429 Too Many Requests", String::new(), "GET"),
        rest_script("200 OK", full_snapshot(100), "GET"),
    ]);
    let mut client = Client::start_with_rest(
        ob_feed::BinanceFullDepth::new(spec(), "BTCUSDT"),
        ws.cfg.clone(),
        Some(rest.cfg.clone()),
        fast(),
    );
    let (_, lost) = client.wait_for("snapshot failure", |e| matches!(e, Ev::Lost(_)));
    assert!(
        matches!(&lost, Ev::Lost(m) if m.contains("429")),
        "{lost:?}"
    );
    client.wait_feed(FeedEvent::Reconnecting(ReconnectReason::SnapshotFailed));
    client.wait_feed(FeedEvent::Live);
    let (stats, _) = client.stop();
    ws.finish();
    rest.finish();
    assert_eq!((stats.reconnects, stats.disconnects), (1, 1));
}
