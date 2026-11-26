//! **Zero-allocation proof.** A counting global allocator wraps the system one, and each
//! test asserts how many heap allocations a path makes per message once warmed up.
//!
//! Why it matters: an allocation is a call into the allocator (tens of ns, sometimes a
//! lock or a system call for fresh memory), it scatters data across the heap (cache
//! misses), and in Java it is future garbage-collector work. A hot path that never
//! allocates has none of those costs, and its latency doesn't depend on heap state.
//!
//! The counter is **per thread** (a `const`-initialised thread-local, which itself never
//! allocates), so tests running in parallel don't count each other's allocations.
#![allow(clippy::unwrap_used, clippy::print_stdout, clippy::cast_precision_loss)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::io::{Cursor, Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::thread;

use ob_agg::{Action, Aggregator, Config, VenueConfig};
use ob_core::{InstrumentSpec, Px};
use ob_feed::driver;
use ob_feed::log::{LogEvent, log_channel};
use ob_feed::probe::{NoProbe, Probe, StageProbe, WallClock};
use ob_feed::recording::{Frame, Kind, RecordReader, RecordWriter};
use ob_feed::{Binance, Bybit, Commands, Feed, FeedConfig, FeedEvent, Venue};
use ob_net::ws::{Opcode, encode_server_frame, handshake};
use ob_net::{ConnConfig, tls};
use ob_sync::{BookReader, book_channel};

// ---------------------------------------------------------------------------
// The counting allocator.

thread_local! {
    // `const` initialisation: no lazy set-up, so reading it inside the allocator can't
    // itself allocate (which would recurse).
    static ALLOCATIONS: Cell<u64> = const { Cell::new(0) };
}

struct Counting;

fn bump() {
    // `try_with`: during thread teardown the slot may be gone; then just don't count.
    let _ = ALLOCATIONS.try_with(|c| c.set(c.get() + 1));
}

// SAFETY: every method forwards to `System` with the caller's arguments
// unchanged, so `System`'s guarantees are ours; counting touches only a thread-local
// `Cell<u64>`, which neither allocates nor unwinds.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        bump();
        // SAFETY: forwarded unchanged; the caller upholds `alloc`'s contract.
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        bump();
        // SAFETY: as above.
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        bump(); // growing a Vec is an allocation too
        // SAFETY: `ptr`/`layout` came from this allocator (i.e. from `System`).
        unsafe { System.realloc(ptr, layout, new_size) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: as above.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

fn allocations() -> u64 {
    ALLOCATIONS.with(Cell::get)
}

/// Runs `f` and returns how many allocations this thread made during it.
fn count<R>(f: impl FnOnce() -> R) -> (R, u64) {
    let before = allocations();
    let r = f();
    (r, allocations() - before)
}

/// The control: the counter must see allocations, or every "0" below proves nothing.
#[test]
fn the_counter_counts() {
    let (v, n) = count(|| Vec::<u64>::with_capacity(10));
    assert_eq!(n, 1);
    let (mut v, n2) = count(move || {
        let mut v = v;
        v.extend(0..100); // grows past capacity: a realloc
        v
    });
    assert!(n2 >= 1);
    let ((), n3) = count(|| v.clear()); // freeing or clearing isn't an allocation
    assert_eq!(n3, 0);
    // Another thread's allocations aren't counted here: spawning a thread that
    // allocates 1 MiB many times costs this thread exactly what spawning an idle one
    // does (the spawn's own bookkeeping).
    let ((), idle) = count(|| thread::spawn(|| {}).join().unwrap());
    let ((), busy) = count(|| {
        thread::spawn(|| {
            for _ in 0..100 {
                drop(std::hint::black_box(vec![0_u8; 1 << 20]));
            }
        })
        .join()
        .unwrap();
    });
    assert_eq!(idle, busy);
}

// ---------------------------------------------------------------------------

const BINANCE: &str = include_str!("../../ob-parse/tests/fixtures/binance_depth20.jsonl");
const BYBIT: &str = include_str!("../../ob-parse/tests/fixtures/bybit_orderbook50.jsonl");
const BINANCE_TOP: &str =
    include_str!("../../ob-parse/tests/fixtures/binance_depth_and_bookticker.jsonl");
const BYBIT_TOP: &str = include_str!("../../ob-parse/tests/fixtures/bybit_orderbook50_and_1.jsonl");

fn spec() -> InstrumentSpec {
    InstrumentSpec::from_steps("0.1", "0.001").unwrap()
}

/// Feeds `lines` to a connected feed: the first one as warm-up, the rest counted.
/// Returns (allocations, messages counted).
fn feed_allocations<V: Venue, P: Probe>(venue: V, probe: P, lines: &[&str]) -> (u64, u64) {
    let (publisher, _reader) = book_channel();
    let mut feed = Feed::with_probe(venue, publisher, FeedConfig::default(), 1, probe);
    let mut cmds = Commands::default();
    feed.on_connected(0, &mut cmds);
    feed.on_text(1, lines[0].as_bytes(), &mut cmds); // warm-up
    while cmds.pop().is_some() {}
    let ((), n) = count(|| {
        for (i, l) in lines[1..].iter().enumerate() {
            feed.on_text(2 + i as u64, l.as_bytes(), &mut cmds);
            while cmds.pop().is_some() {}
        }
    });
    assert!(
        feed.stats().applied >= (lines.len() * 9 / 10) as u64, // replies and pre-book tickers are ignored
        "{:?}",
        feed.stats()
    );
    (n, (lines.len() - 1) as u64)
}

#[test]
fn parse_apply_publish_allocates_nothing() {
    let bn: Vec<&str> = BINANCE.lines().collect();
    let bb: Vec<&str> = BYBIT.lines().collect();
    // Depth and top of book interleaved, as recorded from one connection.
    let bn_top: Vec<&str> = BINANCE_TOP.lines().collect();
    let bb_top: Vec<&str> = BYBIT_TOP.lines().collect();
    for (name, (allocs, msgs)) in [
        (
            "binance, no probe",
            feed_allocations(Binance::new(spec(), "BTCUSDT"), NoProbe, &bn),
        ),
        (
            "bybit, no probe",
            feed_allocations(Bybit::new(spec(), "BTCUSDT"), NoProbe, &bb),
        ),
        (
            "binance, latency probe (live clock)",
            feed_allocations(
                Binance::new(spec(), "BTCUSDT"),
                StageProbe::new(WallClock::System),
                &bn,
            ),
        ),
        (
            "binance + bookTicker overlay",
            feed_allocations(
                Binance::new(spec(), "BTCUSDT").with_top_of_book(),
                NoProbe,
                &bn_top,
            ),
        ),
        (
            "bybit + orderbook.1 overlay",
            feed_allocations(
                Bybit::new(spec(), "BTCUSDT").with_top_of_book(),
                NoProbe,
                &bb_top,
            ),
        ),
        (
            "bybit, latency probe (live clock)",
            feed_allocations(
                Bybit::new(spec(), "BTCUSDT"),
                StageProbe::new(WallClock::System),
                &bb,
            ),
        ),
    ] {
        println!("{name:<38} {allocs} allocations in {msgs} messages");
        assert_eq!(allocs, 0, "{name}");
    }
}

/// The unhappy paths too: a gap, a resync, garbage, a reconnect with backoff. Recovery
/// shouldn't be the moment the feed starts allocating.
#[test]
fn errors_resyncs_and_reconnects_allocate_nothing() {
    let bb: Vec<&str> = BYBIT.lines().collect();
    let (publisher, _reader) = book_channel();
    let mut feed = Feed::new(
        Bybit::new(spec(), "BTCUSDT"),
        publisher,
        FeedConfig::default(),
        1,
    );
    let mut cmds = Commands::default();
    feed.on_connected(0, &mut cmds);
    feed.on_text(1, bb[0].as_bytes(), &mut cmds);
    while cmds.pop().is_some() {}
    let mut events = 0;
    let ((), n) = count(|| {
        let mut t = 2;
        let mut step = |feed: &mut Feed<Bybit>, text: &[u8]| {
            t += 1;
            feed.on_text(t, text, &mut cmds);
            while let Some(c) = cmds.pop() {
                events += 1;
                let _ = c;
            }
        };
        step(&mut feed, bb[1].as_bytes());
        step(&mut feed, bb[5].as_bytes()); // gap → resync (unsubscribe + subscribe)
        step(&mut feed, b"{\"topic\":\"orderbook.50.BTCUSDT\",\"type\":"); // garbage → resync
        step(&mut feed, bb[0].as_bytes()); // fresh snapshot → live
        // Silence → stale → reconnect → disconnected (backoff) → connected again.
        feed.on_tick(t + 60_000_000_000, &mut cmds);
        feed.on_disconnected(t + 60_000_000_001, &mut cmds);
        feed.on_connected(t + 70_000_000_000, &mut cmds);
        while cmds.pop().is_some() {
            events += 1;
        }
    });
    assert!(
        events >= 8,
        "the scenario should have produced commands: {events}"
    );
    assert!(
        feed.stats().resyncs >= 2 && feed.stats().disconnects == 1,
        "{:?}",
        feed.stats()
    );
    println!("gap + garbage + 2 resyncs + stale + reconnect: {n} allocations");
    assert_eq!(n, 0);
}

fn published(venue: impl Venue, fixture: &str) -> BookReader {
    let (publisher, reader) = book_channel();
    let mut feed = Feed::new(venue, publisher, FeedConfig::default(), 1);
    let mut cmds = Commands::default();
    feed.on_connected(0, &mut cmds);
    for l in fixture.lines() {
        feed.on_text(1, l.as_bytes(), &mut cmds);
    }
    reader
}

#[test]
fn aggregator_queries_allocate_nothing() {
    let mut agg = Aggregator::new(Config {
        max_age_ns: u64::MAX,
        max_skew_ms: u64::MAX,
    });
    for (name, depth, fee, reader) in [
        (
            "binance",
            20,
            500,
            published(Binance::new(spec(), "BTCUSDT"), BINANCE),
        ),
        (
            "bybit",
            50,
            550,
            published(Bybit::new(spec(), "BTCUSDT"), BYBIT),
        ),
    ] {
        agg.add_venue(
            VenueConfig {
                name,
                depth,
                taker_fee_ppm: fee,
            },
            reader,
        )
        .unwrap();
    }
    let qty = spec().parse_qty("2.5").unwrap();
    let px = Px::from_raw(586_400);
    let mut sink = 0_i128;
    let first = agg.venue_ids().next().unwrap();
    let ((), n) = count(|| {
        for i in 0..1000_u64 {
            agg.refresh();
            let now = 1 + i;
            sink += agg.best_bids(now).len() as i128 + agg.best_asks(now).len() as i128;
            sink += agg
                .bid_volume_at_or_above(px, now)
                .total
                .map_or(0, |q| q.raw()) as i128;
            sink += agg
                .ask_volume_at_or_below(px, now)
                .total
                .map_or(0, |q| q.raw()) as i128;
            sink += agg.optimal_execution(Action::Buy, qty, now).gross.raw();
            sink += agg.optimal_execution(Action::Sell, qty, now).gross.raw();
            sink += i128::from(agg.scan_arbitrage(now).best.is_some());
            // Best N levels across venues, into a caller buffer.
            let mut top = [ob_agg::Quote {
                venue: first,
                px,
                qty,
            }; 16];
            sink += agg.best_levels(ob_core::Side::Bid, now, &mut top) as i128;
        }
    });
    assert!(sink != 0);
    println!(
        "1000 × (refresh + 2 best + 2 volume + 2 executions + arbitrage scan + best 16 levels): {n} allocations"
    );
    assert_eq!(n, 0);
}

#[test]
fn logging_and_replay_reading_allocate_nothing() {
    let (mut w, mut r) = log_channel(0);
    let ((), n) = count(|| {
        for t in 0..10_000 {
            w.log(t, LogEvent::Feed(FeedEvent::Gap));
            if t % 100 == 0 {
                while r.pop().is_some() {}
            }
        }
    });
    println!("10000 log records pushed: {n} allocations");
    assert_eq!(n, 0);

    let mut rec = RecordWriter::new(Vec::new()).unwrap();
    for (i, l) in BINANCE.lines().chain(BYBIT.lines()).enumerate() {
        rec.write(&Frame {
            wall_ns: 1,
            mono_ns: i as u64,
            venue: 0,
            kind: Kind::Text,
            text: l.as_bytes(),
        })
        .unwrap();
    }
    let bytes = rec.finish().unwrap();
    let mut reader = RecordReader::new(Cursor::new(&bytes)).unwrap(); // allocates its buffer once
    let (frames, n) = count(|| {
        let mut frames = 0;
        while reader.read_frame().unwrap().is_some() {
            frames += 1;
        }
        frames
    });
    println!("{frames} recorded frames read: {n} allocations");
    assert_eq!((frames, n), (120, 0));
}

// ---------------------------------------------------------------------------
// The whole feed thread over real TLS: what's left is the TLS library's.

/// A local TLS WebSocket server: sends `frames` (one TLS record each, 200 µs apart),
/// reports on `sent`, then waits for the client's close.
fn serve(
    frames: Vec<Vec<u8>>,
    sent: std::sync::mpsc::Sender<()>,
) -> (ConnConfig, thread::JoinHandle<()>) {
    let ck = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    let cert = ck.cert.der().clone();
    let key = rustls::pki_types::PrivatePkcs8KeyDer::from(ck.signing_key.serialize_der());
    let server_cfg = Arc::new(
        rustls::ServerConfig::builder_with_provider(tls::crypto_provider())
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![cert.clone()], key.into())
            .unwrap(),
    );
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let mut cfg = ConnConfig::new("localhost", "/ws").unwrap();
    cfg.port = listener.local_addr().unwrap().port();
    let mut roots = rustls::RootCertStore::empty();
    roots.add(cert).unwrap();
    cfg.tls = tls::client_config_with_roots(roots).unwrap();
    let server = thread::spawn(move || {
        let (sock, _) = listener.accept().unwrap();
        let conn = rustls::ServerConnection::new(server_cfg).unwrap();
        let mut s = rustls::StreamOwned::new(conn, sock);
        let mut req = Vec::new();
        let mut b = [0_u8];
        while !req.ends_with(b"\r\n\r\n") {
            s.read_exact(&mut b).unwrap();
            req.push(b[0]);
        }
        let req = String::from_utf8(req).unwrap();
        let key = req
            .lines()
            .find_map(|l| l.strip_prefix("Sec-WebSocket-Key: "))
            .unwrap()
            .trim();
        let resp = format!(
            "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {}\r\n\r\n",
            handshake::expected_accept(key)
        );
        s.write_all(resp.as_bytes()).unwrap();
        s.flush().unwrap();
        let mut buf = [0_u8; 512];
        let _ = s.read(&mut buf).unwrap(); // the subscription
        for f in frames {
            s.write_all(&f).unwrap();
            s.flush().unwrap();
            thread::sleep(std::time::Duration::from_micros(200));
        }
        sent.send(()).unwrap();
        while s.read(&mut buf).is_ok_and(|n| n > 0) {} // until the client closes
    });
    (cfg, server)
}

/// The production driver on its own thread against a local TLS server sending the
/// recorded Binance messages. The tap (called after each message is handled) reads the
/// thread's counter, giving the allocations made while receiving and processing each
/// message: socket read, TLS decryption, WebSocket decode, parse, apply, publish, and
/// every idle poll in between.
///
/// rustls's own reader hands each decrypted record over in a new `Vec`; our record layer
/// decrypts in place, so there is no allocation per record. This also catches subtler
/// ones, such as an error message built eagerly on the success path.
#[test]
fn the_feed_thread_over_real_tls_allocates_nothing() {
    let lines: Vec<&str> = BINANCE.lines().collect();
    let frames: Vec<Vec<u8>> = lines
        .iter()
        .map(|l| {
            let mut f = Vec::new();
            encode_server_frame(&mut f, true, Opcode::Text, l.as_bytes());
            f
        })
        .collect();
    let (sent_tx, sent_rx) = std::sync::mpsc::channel();
    let (cfg, server) = serve(frames, sent_tx);
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = Arc::clone(&stop);
    let client = thread::spawn(move || {
        let (publisher, _reader) = book_channel();
        let mut feed = Feed::new(
            Binance::new(spec(), "BTCUSDT"),
            publisher,
            FeedConfig::default(),
            1,
        );
        // Allocated up front: pushing within capacity doesn't allocate.
        let mut marks: Vec<u64> = Vec::with_capacity(1024);
        driver::run_with_tap(
            &mut feed,
            &cfg,
            &stop2,
            |_, _| {},
            |_, tapped| {
                if let driver::Tapped::Text(_) = tapped {
                    marks.push(allocations());
                }
            },
        );
        (marks, feed.stats())
    });
    sent_rx.recv().unwrap();
    thread::sleep(std::time::Duration::from_millis(100));
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let (marks, stats) = client.join().unwrap();
    server.join().unwrap();
    assert_eq!(marks.len(), lines.len(), "{stats:?}");
    // Per message, after a few warm-up messages (buffers reach their working size).
    let per_msg: Vec<u64> = marks.windows(2).skip(4).map(|w| w[1] - w[0]).collect();
    let total: u64 = per_msg.iter().sum();
    let max = per_msg.iter().max().copied().unwrap_or(0);
    println!(
        "feed thread over TLS: {total} allocations in {} messages (max {max} per message)",
        per_msg.len()
    );
    assert!(per_msg.len() >= 50, "{per_msg:?}");
    assert_eq!(total, 0, "{per_msg:?}");
}

/// Full depth, once synced: diff events touching levels all over a 1000-level
/// book (inserts, updates, removals, capacity drops) allocate nothing. (Waiting for a
/// snapshot buffers events as owned bytes: a cold path, not counted.)
#[test]
fn full_depth_steady_state_allocates_nothing() {
    use ob_feed::BinanceFullDepth;
    let mut v = BinanceFullDepth::new(spec(), "BTCUSDT");
    let ev = |u: u64, levels: &str| {
        format!(
            r#"{{"e":"depthUpdate","E":7,"T":7,"s":"BTCUSDT","U":{u},"u":{u},"pu":{},"b":[{levels}],"a":[]}}"#,
            u - 1
        )
    };
    v.on_text(ev(100, "").as_bytes());
    let bids: Vec<String> = (0..1000)
        .map(|i| format!(r#"["{}.0","1.000"]"#, 5000 - i))
        .collect();
    let asks: Vec<String> = (0..1000)
        .map(|i| format!(r#"["{}.0","1.000"]"#, 5001 + i))
        .collect();
    let snap = format!(
        r#"{{"lastUpdateId":100,"bids":[{}],"asks":[{}]}}"#,
        bids.join(","),
        asks.join(",")
    );
    v.on_snapshot(snap.as_bytes());
    // Pre-built events: each touches 50 levels spread through the book.
    let events: Vec<String> = (101..301_u64)
        .map(|u| {
            let lv: Vec<String> = (0..50_u64)
                .map(|k| {
                    format!(
                        r#"["{}.{}","{}"]"#,
                        4990 - (k * 19 + u) % 900,
                        u % 10,
                        if (u + k) % 3 == 0 { "0" } else { "2.500" }
                    )
                })
                .collect();
            ev(u, &lv.join(","))
        })
        .collect();
    let (applied, n) = count(|| {
        events
            .iter()
            .filter(|e| matches!(v.on_text(e.as_bytes()), ob_feed::Outcome::Applied { .. }))
            .count()
    });
    println!("full depth: {applied} diff events × 50 levels: {n} allocations");
    assert_eq!((applied, n), (200, 0));
}

/// Single-venue market impact: no allocation either.
#[test]
fn market_impact_allocates_nothing() {
    let mut book: ob_core::OrderBook = ob_core::OrderBook::new();
    let mut cmds = Commands::default();
    let (publisher, _r) = book_channel();
    let mut feed = Feed::new(
        Binance::new(spec(), "BTCUSDT"),
        publisher,
        FeedConfig::default(),
        1,
    );
    feed.on_connected(0, &mut cmds);
    let mut msg = ob_parse::binance::DepthUpdate::default();
    let parser = ob_parse::binance::BinanceDepthParser::new(spec(), b"BTCUSDT");
    parser
        .parse(BINANCE.lines().nth(1).unwrap().as_bytes(), &mut msg)
        .unwrap();
    msg.apply_to(&mut book).unwrap();
    let q = spec().parse_qty("3.5").unwrap();
    let (filled, n) = count(|| {
        (0..1000)
            .map(|_| book.market_impact(ob_core::Side::Ask, q).filled.raw())
            .sum::<i64>()
    });
    assert!(filled > 0);
    assert_eq!(n, 0);
}
