//! The measured scenarios.
//!
//! | Scenario | Path measured | What it isolates |
//! |---|---|---|
//! | [`pipeline`], warm | parse → apply → publish, back to back | our code with hot caches |
//! | [`pipeline`], cold | the same, with the caches flushed before each message | our code as sparse live traffic finds it |
//! | [`wire`] | local TLS server → kernel → read + TLS + WS → parse → apply → publish | the whole receive path, open loop |
//! | [`reader`] | aggregator queries on another thread, writer idle or busy | the read side's tail |

use std::hint::black_box;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

use ob_agg::{Action, Aggregator, Config, VenueConfig};
use ob_core::{InstrumentSpec, Side};
use ob_feed::driver::{DriverEvent, Tapped, run_with_tap};
use ob_feed::{
    Binance, BinanceFullDepth, Bybit, Commands, Feed, FeedConfig, FeedStats, Venue, mono_ns,
};
use ob_sync::{BookPublisher, book_channel};

use crate::probe::TailProbe;
use crate::server::{self, SplitMix64};
use crate::stats::Dist;
use crate::stream::{Kind, Stream};

/// The instrument both venues list (tick 0.1, lot 0.001).
pub fn spec() -> InstrumentSpec {
    #[allow(clippy::unwrap_used)] // constants
    InstrumentSpec::from_steps("0.1", "0.001").unwrap()
}

/// Applies the same thread tuning as `ob live --rt` to the calling thread, if asked.
pub fn tune_thread(rt: bool) {
    if rt {
        let _ = ob_os::set_realtime_priority(50);
    }
}

/// A feed for `kind`, measured by a [`TailProbe`], connected and (full depth) synced.
pub(crate) fn feed_for(
    kind: Kind,
    publisher: BookPublisher,
    clock: &quanta::Clock,
) -> Box<dyn AnyFeed> {
    let probe = TailProbe::new(clock.clone());
    let cfg = FeedConfig::default();
    match kind {
        Kind::BinanceTop => Box::new(Feed::with_probe(
            Binance::new(spec(), "BTCUSDT").with_top_of_book(),
            publisher,
            cfg,
            1,
            probe,
        )),
        // No heartbeat during a run: Bybit's 20 s ping needs a pong, which this local
        // server doesn't send, and a heartbeat every 20 s is not per-message latency.
        Kind::BybitTop => Box::new(Feed::with_probe(
            Bybit::new(spec(), "BTCUSDT")
                .with_top_of_book()
                .with_heartbeat(Duration::from_secs(24 * 3600)),
            publisher,
            cfg,
            1,
            probe,
        )),
        Kind::BinanceFull => Box::new(Feed::with_probe(
            BinanceFullDepth::new(spec(), "BTCUSDT"),
            publisher,
            cfg,
            1,
            probe,
        )),
    }
}

/// Object-safe view of a `Feed<V, TailProbe>` for the in-process scenarios (the
/// dynamic call is outside every timed region: the probe times inside `on_text`).
pub(crate) trait AnyFeed {
    fn on_connected(&mut self, now: u64, cmds: &mut Commands);
    fn on_disconnected(&mut self, now: u64, cmds: &mut Commands);
    fn on_text(&mut self, now: u64, text: &[u8], cmds: &mut Commands);
    fn on_snapshot(&mut self, now: u64, body: &[u8], cmds: &mut Commands);
    fn probe_mut(&mut self) -> &mut TailProbe;
    fn stats(&self) -> FeedStats;
}

impl<V: Venue> AnyFeed for Feed<V, TailProbe> {
    fn on_connected(&mut self, now: u64, cmds: &mut Commands) {
        Feed::on_connected(self, now, cmds);
    }
    fn on_disconnected(&mut self, now: u64, cmds: &mut Commands) {
        Feed::on_disconnected(self, now, cmds);
    }
    fn on_text(&mut self, now: u64, text: &[u8], cmds: &mut Commands) {
        Feed::on_text(self, now, text, cmds);
    }
    fn on_snapshot(&mut self, now: u64, body: &[u8], cmds: &mut Commands) {
        Feed::on_snapshot(self, now, body, cmds);
    }
    fn probe_mut(&mut self) -> &mut TailProbe {
        Feed::probe_mut(self)
    }
    fn stats(&self) -> FeedStats {
        Feed::stats(self)
    }
}

/// Results of one feed scenario.
pub struct FeedResult {
    /// Every message, timed around the whole handler call (pipeline scenarios), or from
    /// the server's send to published (wire).
    pub message: Dist,
    /// Stage timings from the feed's probe (applied messages only).
    pub read: Dist,
    /// Parse + apply.
    pub handle: Dist,
    /// Publish.
    pub publish: Dist,
    /// Start of the read → published.
    pub total: Dist,
    /// Subscription-start messages (acks, Bybit's book snapshot), kept out of the rows
    /// above: same measure as `message`.
    pub seam: Dist,
    /// In-process only: the recorded messages that cost the most, by mean.
    pub slowest: Vec<LineCost>,
    /// The feed's own counters: must show no resyncs, rejections or reconnects.
    pub stats: FeedStats,
    /// Deliberate re-syncs between passes (full depth only); each shows up once in
    /// `stats.disconnects`.
    pub resets: u64,
    /// Wire only: what the server did.
    pub sent: Option<server::Sent>,
    /// Wire only: driver events (connection problems), which should be none.
    pub events: Vec<String>,
}

/// What one recorded message costs across all its repetitions.
#[derive(Clone, Copy, Debug, Default)]
pub struct LineCost {
    /// Line number in the recording (0-based).
    pub line: usize,
    /// Size in bytes.
    pub bytes: usize,
    /// Repetitions measured.
    pub count: u64,
    /// Mean without the single slowest repetition (one OS interruption would otherwise
    /// dominate a small message's mean), ns.
    pub mean_ns: u64,
    /// Slowest repetition, ns.
    pub max_ns: u64,
}

/// How to flush the caches between messages ("cold" mode).
#[derive(Clone, Copy, Debug)]
pub struct Cold {
    /// Bytes to read through between messages (more than the last-level cache).
    pub bytes: usize,
}

/// Parse → apply → publish, in process, `n` messages. `cold`: flush the caches before
/// each message (outside the timed region), as a sparse live stream finds them.
///
/// Full depth can't loop its recording: diff events set absolute quantities, so
/// replaying the first events on top of the last ones' result is a book no exchange
/// ever sent (the first attempt crossed it). Each pass therefore re-syncs the feed
/// outside the timed region, through the real path (disconnect, connect, first event,
/// REST snapshot), reusing the same feed and memory, then times the diffs.
pub fn pipeline(kind: Kind, n: usize, cold: Option<Cold>, clock: &quanta::Clock) -> FeedResult {
    let (publisher, _reader) = book_channel();
    let mut feed = feed_for(kind, publisher, clock);
    let mut stream = Stream::new(kind);
    let mut cmds = Commands::default();
    let mut resets = 0_u64;
    // Full depth: the recording, unshifted (see above).
    let full: Vec<&str> = match kind {
        Kind::BinanceFull => stream.lines().to_vec(),
        _ => Vec::new(),
    };
    let mut next_diff = 0;
    let mask = stream.seam_mask();
    let mut seam = Dist::new();
    let mut line = 0;
    let pass_len = if full.is_empty() {
        mask.len()
    } else {
        full.len()
    };
    let mut per_line = vec![(0_u64, 0_u64, 0_u64); pass_len]; // (count, sum, max)
    feed.on_connected(mono_ns(), &mut cmds);
    while cmds.pop().is_some() {}
    let mut flush = cold.map(|c| vec![1_u8; c.bytes]);
    let mut message = Dist::new();
    let mut sink = 0_u64;
    for _ in 0..n {
        if let Some(snapshot) = kind.snapshot()
            && next_diff == 0
        {
            if resets > 0 {
                feed.on_disconnected(mono_ns(), &mut cmds);
                feed.on_connected(mono_ns(), &mut cmds);
            }
            feed.probe_mut().begin(true); // the re-sync is not a steady-state message
            feed.on_text(mono_ns(), full[0].as_bytes(), &mut cmds);
            feed.on_snapshot(mono_ns(), snapshot.as_bytes(), &mut cmds);
            while cmds.pop().is_some() {}
            resets += 1;
            next_diff = 1;
        }
        if let Some(buf) = flush.as_mut() {
            sink = sink.wrapping_add(evict(buf));
        }
        let now = mono_ns();
        let is_seam = full.is_empty() && mask[line % mask.len()];
        let this_line = if full.is_empty() {
            line % pass_len
        } else {
            next_diff
        };
        line += 1;
        feed.probe_mut().begin(is_seam);
        let msg = if full.is_empty() {
            stream.next_message()
        } else {
            let m = full[next_diff].as_bytes();
            next_diff = (next_diff + 1) % full.len();
            m
        };
        let t0 = clock.raw();
        feed.on_text(now, msg, &mut cmds);
        let t1 = clock.raw();
        let ns = clock.delta_as_nanos(t0, t1);
        if is_seam {
            seam.record(ns);
        } else {
            message.record(ns);
        }
        let c = &mut per_line[this_line];
        *c = (c.0 + 1, c.1 + ns, c.2.max(ns));
        while cmds.pop().is_some() {}
    }
    black_box(sink);
    let lines = if full.is_empty() {
        stream.lines().to_vec()
    } else {
        full.clone()
    };
    let mut slowest: Vec<LineCost> = per_line
        .iter()
        .enumerate()
        .filter(|(_, c)| c.0 > 0)
        .map(|(i, &(count, sum, max))| LineCost {
            line: i,
            bytes: lines[i].len(),
            count,
            mean_ns: if count > 1 {
                (sum - max) / (count - 1)
            } else {
                sum
            },
            max_ns: max,
        })
        .collect();
    slowest.sort_by_key(|c| std::cmp::Reverse(c.mean_ns));
    slowest.truncate(5);
    let stats = feed.stats();
    let p = feed.probe_mut();
    FeedResult {
        message,
        read: Dist::new(),
        handle: std::mem::take(&mut p.handle),
        publish: std::mem::take(&mut p.publish),
        total: std::mem::take(&mut p.total),
        seam,
        slowest,
        stats,
        resets: resets.saturating_sub(1),
        sent: None,
        events: Vec::new(),
    }
}

/// Reads one byte per cache line of `buf` and writes one back, pushing everything else
/// out of the caches (and the TLB). Returns a value so it can't be optimised away.
fn evict(buf: &mut [u8]) -> u64 {
    let mut acc = 0_u64;
    for line in buf.chunks_mut(64) {
        acc = acc.wrapping_add(u64::from(line[0]));
        line[0] = line[0].wrapping_add(1);
    }
    acc
}

/// The whole receive path, open loop: a local TLS server sends `n` messages with
/// exponential gaps of mean `mean_gap_ns`; the production driver receives them.
pub fn wire(kind: Kind, n: usize, mean_gap_ns: u64, rt: bool, clock: &quanta::Clock) -> FeedResult {
    assert!(
        kind != Kind::BinanceFull,
        "full depth needs a REST server: pipeline only"
    );
    let stamps: Arc<[AtomicU64]> = (0..n).map(|_| AtomicU64::new(0)).collect();
    let mask = Stream::new(kind).seam_mask();
    let (cfg, server) = server::spawn(
        Stream::new(kind),
        n,
        mean_gap_ns,
        clock.clone(),
        Arc::clone(&stamps),
        0x5EED,
    );
    let clock = clock.clone();
    let client = thread::spawn(move || {
        tune_thread(rt);
        let (publisher, _reader) = book_channel();
        let probe = TailProbe::new(clock.clone()).with_mask(mask.clone());
        let stop = AtomicBool::new(false);
        let mut message = Dist::new();
        let mut seam = Dist::new();
        let mut events = Vec::new();
        let mut k = 0;
        let mut log = |venue: &str, e: DriverEvent<'_>| {
            if !matches!(e, DriverEvent::Feed(_)) {
                events.push(format!("{venue}: {e:?}"));
            }
        };
        let mut tap = |_: u64, t: Tapped<'_>| {
            if let Tapped::Text(_) = t {
                let now = clock.raw();
                if let Some(s) = stamps.get(k) {
                    let sent = s.load(Ordering::Acquire);
                    let ns = clock.delta_as_nanos(sent, now);
                    if mask[k % mask.len()] {
                        seam.record(ns);
                    } else {
                        message.record(ns);
                    }
                }
                k += 1;
                if k == n {
                    stop.store(true, Ordering::Relaxed);
                }
            }
        };
        macro_rules! drive {
            ($venue:expr) => {{
                let mut feed = Feed::with_probe($venue, publisher, FeedConfig::default(), 1, probe);
                run_with_tap(&mut feed, &cfg, &stop, &mut log, &mut tap);
                let stats = feed.stats();
                let p = feed.probe_mut();
                (
                    std::mem::take(&mut p.read),
                    std::mem::take(&mut p.handle),
                    std::mem::take(&mut p.publish),
                    std::mem::take(&mut p.total),
                    stats,
                )
            }};
        }
        let (read, handle, publish, total, stats) = match kind {
            Kind::BinanceTop => drive!(Binance::new(spec(), "BTCUSDT").with_top_of_book()),
            _ => drive!(
                Bybit::new(spec(), "BTCUSDT")
                    .with_top_of_book()
                    .with_heartbeat(Duration::from_secs(24 * 3600))
            ),
        };
        (message, seam, read, handle, publish, total, stats, events)
    });
    #[allow(clippy::unwrap_used)] // a panic in either thread is a harness bug: surface it
    let (message, seam, read, handle, publish, total, stats, events) = client.join().unwrap();
    #[allow(clippy::unwrap_used)]
    let sent = server.join().unwrap();
    FeedResult {
        message,
        read,
        handle,
        publish,
        total,
        seam,
        slowest: Vec::new(),
        stats,
        resets: 0,
        sent: Some(sent),
        events,
    }
}

/// Reader-side results: one distribution per query.
pub struct ReaderResult {
    /// Clock-read pair with nothing between: the floor under every row.
    pub timer: Dist,
    /// `refresh`: copy every venue's book out of its seqlock if it changed.
    pub refresh: Dist,
    /// Best bid and best ask across venues.
    pub best_bid_ask: Dist,
    /// The best 10 bid levels across venues.
    pub best_levels: Dist,
    /// Fee-aware plan to buy 1 BTC.
    pub execution: Dist,
    /// Cross-venue arbitrage scan.
    pub arbitrage: Dist,
    /// Messages the writer published meanwhile.
    pub published: u64,
}

/// Aggregator queries on this thread while a writer thread replays both venues into
/// their seqlocks (`busy`: open loop, mean gap `writer_gap_ns` per message; otherwise
/// one message each, then idle). `n` rounds of every query.
pub fn reader(
    n: usize,
    busy: bool,
    writer_gap_ns: u64,
    rt: bool,
    clock: &quanta::Clock,
) -> ReaderResult {
    let (p1, r1) = book_channel();
    let (p2, r2) = book_channel();
    let stop = Arc::new(AtomicBool::new(false));
    let ready = Arc::new(AtomicBool::new(false));
    let writer = {
        let (stop, ready, clock) = (Arc::clone(&stop), Arc::clone(&ready), clock.clone());
        thread::spawn(move || {
            tune_thread(rt);
            let mut a = feed_for(Kind::BinanceTop, p1, &clock);
            let mut b = feed_for(Kind::BybitTop, p2, &clock);
            let (mut sa, mut sb) = (Stream::new(Kind::BinanceTop), Stream::new(Kind::BybitTop));
            let mut cmds = Commands::default();
            a.on_connected(mono_ns(), &mut cmds);
            b.on_connected(mono_ns(), &mut cmds);
            // One full pass each, so both books are complete and live.
            for _ in 0..sa.pass_len() {
                a.on_text(mono_ns(), sa.next_message(), &mut cmds);
            }
            for _ in 0..sb.pass_len() {
                b.on_text(mono_ns(), sb.next_message(), &mut cmds);
            }
            while cmds.pop().is_some() {}
            ready.store(true, Ordering::Release);
            let mut published = 0_u64;
            let mut rng = SplitMix64(0xFEED);
            let mut next = clock.raw();
            let ns_per_tick = clock.delta_as_nanos(0, 1_000_000_000) as f64 / 1e9;
            while !stop.load(Ordering::Relaxed) {
                if !busy {
                    // Idle: parked, so no wake-ups disturb the reader's core.
                    thread::park_timeout(Duration::from_millis(200));
                    continue;
                }
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let gap = (rng.exponential(writer_gap_ns as f64) / ns_per_tick) as u64;
                next += gap;
                while clock.raw() < next {
                    std::hint::spin_loop();
                }
                if published.is_multiple_of(2) {
                    a.on_text(mono_ns(), sa.next_message(), &mut cmds);
                } else {
                    b.on_text(mono_ns(), sb.next_message(), &mut cmds);
                }
                while cmds.pop().is_some() {}
                published += 1;
            }
            published
        })
    };
    while !ready.load(Ordering::Acquire) {
        std::hint::spin_loop();
    }
    tune_thread(rt);
    // Staleness exclusion is not under test: in the idle case the books are older than
    // the 2 s limit by the end, and every query must still do its full work.
    let mut agg = Aggregator::new(Config {
        max_age_ns: u64::MAX,
        ..Config::default()
    });
    let venue = |name, depth, fee| VenueConfig {
        name,
        depth,
        taker_fee_ppm: fee,
    };
    let _ = agg.add_venue(venue("binance", 20, 500), r1);
    let _ = agg.add_venue(venue("bybit", 50, 550), r2);
    agg.refresh();
    #[allow(clippy::unwrap_used)] // constant
    let one_btc = spec().parse_qty("1").unwrap();
    let mut out = [agg.best_bids(mono_ns())[0]; 10];
    let mut r = ReaderResult {
        timer: Dist::new(),
        refresh: Dist::new(),
        best_bid_ask: Dist::new(),
        best_levels: Dist::new(),
        execution: Dist::new(),
        arbitrage: Dist::new(),
        published: 0,
    };
    macro_rules! time {
        ($dist:expr, $e:expr) => {{
            let t0 = clock.raw();
            black_box($e);
            let t1 = clock.raw();
            $dist.record(clock.delta_as_nanos(t0, t1));
        }};
    }
    for _ in 0..n {
        let now = mono_ns();
        time!(r.timer, ());
        time!(r.refresh, agg.refresh());
        time!(r.best_bid_ask, (agg.best_bids(now), agg.best_asks(now)));
        time!(r.best_levels, agg.best_levels(Side::Bid, now, &mut out));
        time!(
            r.execution,
            agg.optimal_execution(Action::Buy, one_btc, now)
        );
        time!(r.arbitrage, agg.scan_arbitrage(now));
    }
    stop.store(true, Ordering::Relaxed);
    writer.thread().unpark();
    #[allow(clippy::unwrap_used)]
    {
        r.published = writer.join().unwrap();
    }
    r
}

impl FeedResult {
    /// The run exercised only the normal path: no resync, rejection or reconnect, and no
    /// disconnect other than the deliberate full-depth re-syncs.
    pub fn valid(&self) -> bool {
        let s = self.stats;
        s.resyncs == 0
            && s.rejected == 0
            && s.reconnects == 0
            && s.disconnects == self.resets
            && self.events.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Several passes of every stream stay on the normal path: the looping is sound.
    #[test]
    fn every_stream_loops_without_resyncs() {
        let clock = quanta::Clock::new();
        for kind in [Kind::BinanceTop, Kind::BybitTop, Kind::BinanceFull] {
            let n = 5 * Stream::new(kind).pass_len();
            let r = pipeline(kind, n, None, &clock);
            assert!(r.valid(), "{kind:?}: {:?}, resets {}", r.stats, r.resets);
            assert_eq!(r.message.len() + r.seam.len(), n as u64);
            // Nearly every message is a book update (the rest: subscription acks).
            assert!(
                r.stats.applied * 100 >= r.stats.messages * 95,
                "{kind:?}: {:?}",
                r.stats
            );
        }
    }
}
