//! `ob`: the order book aggregator, live.
//!
//! ```text
//! ob live [--secs N] [--latency] [--record FILE] [--depth-only] [--binance-full-depth]
//!     both venues, merged, printed once a second; Ctrl-C to stop.
//!     --binance-full-depth: Binance's diff stream + REST snapshot (1,024 levels a side,
//!     256 published) instead of its 20-level stream (step 19).
//!     OS tuning (step 21; see docs/TUNING.md), each reported as applied / hint /
//!     unsupported at start-up:
//!     --pin A,B           pin the Binance and Bybit feed threads to CPUs A and B
//!     --rt                real-time priority (Linux SCHED_FIFO; macOS QoS hint)
//!     --lock-memory       mlockall (Linux)
//!     --busy-poll US      SO_BUSY_POLL (Linux)     --quickack   TCP_QUICKACK (Linux)
//!     --kernel-timestamps kernel receive timestamps (Linux), shown with --latency
//!     Each venue's real-time best bid/ask stream is laid over its depth book (step 18)
//!     unless --depth-only.
//!     --latency: time every message's stages (rdtsc) and print histograms at exit.
//!     --record:  save every message with its receive time, for replay.
//! ob replay FILE
//!     run a recording through the same feed code as fast as possible; print stage
//!     histograms, throughput and the final books.
//! ```
//!
//! Threads (the Rust counterpart of the skeleton's `BookApp`):
//!
//! | thread | does | never does |
//! |---|---|---|
//! | `feed-binance`, `feed-bybit` | socket → TLS → WebSocket → parse → book → seqlock; logs binary records | format, print, lock, allocate per message |
//! | `logger` | pops log records, formats them, writes to stderr | touch a socket or a book |
//! | `main` | reads the published books through the aggregator once a second, prints to stdout | block a feed |
//!
//! Each feed thread spins on its socket (step 12), so expect two cores at 100%: that's
//! the price of the lowest wake-up latency.
#![allow(clippy::print_stdout, clippy::print_stderr)] // a binary's job is to print
#![forbid(unsafe_code)]

use std::fs::File;
use std::io::{BufReader, BufWriter, Write};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use ob_agg::{Action, Aggregator, Config, Quote, VenueConfig, VenueState};
use ob_core::InstrumentSpec;
use ob_feed::driver::{self, Tapped};
use ob_feed::log::{LogReader, log_channel};
use ob_feed::probe::{NoProbe, Probe, StageProbe, WallClock};
use ob_feed::recording::{Frame, Kind, RecordReader, RecordWriter, Replay, ReplayFeed};
use ob_feed::{Binance, BinanceFullDepth, Bybit, Feed, FeedConfig, FeedStats, Venue, mono_ns};
use ob_sync::{BookReader, Snapshot, book_channel};

const USAGE: &str = "usage: ob live [--secs N] [--latency] [--record FILE] [--depth-only] [--binance-full-depth]\n               [--pin A,B] [--rt] [--lock-memory] [--busy-poll US] [--quickack] [--kernel-timestamps]\n       ob replay FILE";

/// `ob live` options.
#[derive(Default)]
struct LiveOpts {
    duration: Option<Duration>,
    latency: bool,
    record: Option<String>,
    depth_only: bool,
    full_depth: bool,
    pin: Option<[usize; 2]>,
    rt: bool,
    lock_memory: bool,
    socket: ob_net::SocketTuning,
}

fn parse_live(mut args: &[String]) -> Option<LiveOpts> {
    let mut o = LiveOpts::default();
    while let Some(arg) = args.first() {
        match arg.as_str() {
            "--latency" => {
                o.latency = true;
                args = &args[1..];
            }
            "--depth-only" => {
                o.depth_only = true;
                args = &args[1..];
            }
            "--binance-full-depth" => {
                o.full_depth = true;
                args = &args[1..];
            }
            "--pin" => {
                let (a, b) = args.get(1)?.split_once(',')?;
                o.pin = Some([a.parse().ok()?, b.parse().ok()?]);
                args = &args[2..];
            }
            "--rt" => {
                o.rt = true;
                args = &args[1..];
            }
            "--lock-memory" => {
                o.lock_memory = true;
                args = &args[1..];
            }
            "--busy-poll" => {
                o.socket.busy_poll_us = Some(args.get(1)?.parse().ok()?);
                args = &args[2..];
            }
            "--quickack" => {
                o.socket.quickack = true;
                args = &args[1..];
            }
            "--kernel-timestamps" => {
                o.socket.kernel_timestamps = true;
                args = &args[1..];
            }
            "--secs" => {
                o.duration = Some(Duration::from_secs(args.get(1)?.parse().ok()?));
                args = &args[2..];
            }
            "--record" => {
                o.record = Some(args.get(1)?.clone());
                args = &args[2..];
            }
            _ => return None,
        }
    }
    Some(o)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("live") => match parse_live(&args[1..]) {
            Some(opts) => live(opts),
            None => return usage(),
        },
        Some("replay") if args.len() == 2 => replay(&args[1]),
        _ => return usage(),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn usage() -> ExitCode {
    eprintln!("{USAGE}");
    ExitCode::from(2)
}

/// What a feed thread hands back when it stops.
struct FeedExit {
    stats: FeedStats,
    unreported_log_drops: u64,
    latency: Option<String>,
    record_errors: u64,
}

type Recorder = Arc<Mutex<RecordWriter<BufWriter<File>>>>;

/// Starts one venue's feed thread. Returns its book reader, its log reader and the
/// thread.
/// Per-thread OS tuning for a feed thread (step 21).
#[derive(Clone)]
struct ThreadTuning {
    cpu: Option<usize>,
    rt: bool,
    socket: ob_net::SocketTuning,
    /// Where to report what was applied (the feed thread itself never prints).
    report: std::sync::mpsc::Sender<String>,
}

fn spawn_feed<V: Venue + Send + 'static, P: Probe + Send + 'static>(
    id: u8,
    venue: V,
    probe: P,
    recorder: Option<Recorder>,
    tuning: ThreadTuning,
    stop: &Arc<AtomicBool>,
) -> Result<(BookReader, LogReader, JoinHandle<FeedExit>), String> {
    let name = venue.name();
    let mut conn = driver::connection_config(&venue).map_err(|e| format!("{name}: {e}"))?;
    conn.tuning = tuning.socket;
    let (publisher, books) = book_channel();
    let (mut log, logs) = log_channel(id);
    let stop = Arc::clone(stop);
    let thread = thread::Builder::new()
        .name(format!("feed-{name}"))
        .spawn(move || {
            // Thread tuning first, before anything is allocated or connected. Priority
            // before pinning: on macOS a thread whose policy was set with
            // `thread_policy_set` (the affinity tag) refuses QoS changes (EPERM).
            if tuning.rt {
                let r = ob_os::set_realtime_priority(50);
                let _ = tuning
                    .report
                    .send(format!("{name}: real-time priority: {r:?}"));
            }
            if let Some(cpu) = tuning.cpu {
                let r = ob_os::pin_current_thread(cpu);
                let _ = tuning
                    .report
                    .send(format!("{name}: pin to CPU {cpu}: {r:?}"));
            }
            drop(tuning.report);
            let mut feed =
                Feed::with_probe(venue, publisher, FeedConfig::default(), mono_ns(), probe);
            let mut record_errors = 0_u64;
            // The hot path's only logging: encode 32 bytes, push to the queue.
            let log_event = |_, event: driver::DriverEvent<'_>| log.log(mono_ns(), event.into());
            match recorder {
                None => driver::run(&mut feed, &conn, &stop, log_event),
                // Recording (a capture tool, off by default): a mutex shared by the two
                // feeds and a 1 MiB buffer. The tap runs after the message has been
                // handled and published, so it doesn't delay this message.
                Some(rec) => {
                    driver::run_with_tap(&mut feed, &conn, &stop, log_event, |mono_ns, t| {
                        let (kind, text) = match t {
                            Tapped::Connected => (Kind::Connected, &b""[..]),
                            Tapped::Text(text) => (Kind::Text, text),
                            Tapped::Snapshot(body) => (Kind::Snapshot, body),
                        };
                        let frame = Frame {
                            wall_ns: unix_ns(),
                            mono_ns,
                            venue: id,
                            kind,
                            text,
                        };
                        let ok = rec.lock().is_ok_and(|mut w| w.write(&frame).is_ok());
                        record_errors += u64::from(!ok);
                    })
                }
            }
            FeedExit {
                stats: feed.stats(),
                unreported_log_drops: log.pending_drops(),
                latency: feed.probe().summary(),
                record_errors,
            }
        })
        .map_err(|e| e.to_string())?;
    Ok((books, logs, thread))
}

/// Drains every venue's log queue until `done` is set and the queues are empty.
fn spawn_logger(
    mut queues: Vec<(&'static str, LogReader)>,
    start_ns: u64,
    done: Arc<AtomicBool>,
) -> Result<JoinHandle<()>, String> {
    thread::Builder::new()
        .name("logger".into())
        .spawn(move || {
            let mut err = std::io::stderr().lock();
            loop {
                // Read the flag first: anything pushed before it was set is drained below.
                let finished = done.load(Ordering::Acquire);
                let mut idle = true;
                for (name, q) in &mut queues {
                    while let Some(r) = q.pop() {
                        idle = false;
                        let t = r.t_ns.saturating_sub(start_ns);
                        let _ = writeln!(
                            err,
                            "{:>4}.{:03}s {name:<8} {}",
                            t / 1_000_000_000,
                            t / 1_000_000 % 1000,
                            r.event
                        );
                    }
                }
                if idle {
                    if finished {
                        return;
                    }
                    thread::sleep(Duration::from_millis(1)); // off the hot path: sleeping is fine
                }
            }
        })
        .map_err(|e| e.to_string())
}

type Spawned = (BookReader, LogReader, JoinHandle<FeedExit>);

/// Starts both venues' feed threads as `opts` asks (each venue type is generic, so each
/// combination is its own compiled code).
fn spawn_venues<P: Probe + Send + 'static>(
    opts: &LiveOpts,
    spec: InstrumentSpec,
    probe: impl Fn() -> P,
    recorder: &Option<Recorder>,
    report: &std::sync::mpsc::Sender<String>,
    stop: &Arc<AtomicBool>,
) -> Result<(Spawned, Spawned), String> {
    let top = !opts.depth_only;
    let tuning = |i: usize| ThreadTuning {
        cpu: opts.pin.map(|p| p[i]),
        rt: opts.rt,
        socket: opts.socket,
        report: report.clone(),
    };
    let bn = if opts.full_depth {
        let v = BinanceFullDepth::new(spec, "BTCUSDT");
        let v = if top { v.with_top_of_book() } else { v };
        spawn_feed(0, v, probe(), recorder.clone(), tuning(0), stop)?
    } else {
        let v = Binance::new(spec, "BTCUSDT");
        let v = if top { v.with_top_of_book() } else { v };
        spawn_feed(0, v, probe(), recorder.clone(), tuning(0), stop)?
    };
    let bb = Bybit::new(spec, "BTCUSDT");
    let bb = if top { bb.with_top_of_book() } else { bb };
    Ok((
        bn,
        spawn_feed(1, bb, probe(), recorder.clone(), tuning(1), stop)?,
    ))
}

fn unix_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
}

fn live(opts: LiveOpts) -> Result<(), String> {
    let duration = opts.duration;
    let spec = InstrumentSpec::from_steps("0.1", "0.001").map_err(|e| format!("{e:?}"))?;
    let start_ns = mono_ns();
    let stop = Arc::new(AtomicBool::new(false));
    {
        let stop = Arc::clone(&stop);
        ctrlc::set_handler(move || stop.store(true, Ordering::Relaxed))
            .map_err(|e| e.to_string())?;
    }

    let recorder: Option<Recorder> = match &opts.record {
        None => None,
        Some(path) => {
            let file = File::create(path).map_err(|e| format!("{path}: {e}"))?;
            let w = RecordWriter::new(BufWriter::with_capacity(1 << 20, file))
                .map_err(|e| format!("{path}: {e}"))?;
            Some(Arc::new(Mutex::new(w)))
        }
    };
    // The probe's type is chosen here, at compile time per branch: without `--latency`
    // the feeds are built with `NoProbe` and the timing code vanishes.
    if opts.lock_memory {
        println!("lock memory: {:?}", ob_os::lock_memory());
    }
    let (report, reports) = std::sync::mpsc::channel();
    let ((bn_books, bn_logs, bn), (bb_books, bb_logs, bb)) = if opts.latency {
        let probe = || StageProbe::new(WallClock::System);
        spawn_venues(&opts, spec, probe, &recorder, &report, &stop)?
    } else {
        spawn_venues(&opts, spec, || NoProbe, &recorder, &report, &stop)?
    };
    drop(report);
    for line in reports {
        println!("{line}"); // ends once both feed threads have reported
    }
    let s = opts.socket;
    if s != ob_net::SocketTuning::default() {
        // Socket options are applied per connection inside the feeds; show here what
        // this OS supports, on a throwaway local socket.
        let probe_sock = std::net::TcpListener::bind("127.0.0.1:0")
            .and_then(|l| std::net::TcpStream::connect(l.local_addr()?))
            .map_err(|e| e.to_string())?;
        if let Some(us) = s.busy_poll_us {
            println!(
                "busy poll {us} µs: {:?}",
                ob_os::set_busy_poll(&probe_sock, us)
            );
        }
        if s.quickack {
            println!("quickack: {:?}", ob_os::set_quickack(&probe_sock));
        }
        if s.kernel_timestamps {
            println!(
                "kernel timestamps: {:?}",
                ob_os::enable_rx_timestamps(&probe_sock)
            );
        }
    }
    let logger_done = Arc::new(AtomicBool::new(false));
    let logger = spawn_logger(
        vec![("binance", bn_logs), ("bybit", bb_logs)],
        start_ns,
        Arc::clone(&logger_done),
    )?;

    let mut agg = Aggregator::new(Config::default());
    // Taker fees: Binance USDⓈ-M 0.05%, Bybit linear 0.055% (base tier).
    for (name, depth, fee, books) in [
        (
            "binance",
            if opts.full_depth { ob_sync::DEPTH } else { 20 },
            500,
            &bn_books,
        ),
        ("bybit", 50, 550, &bb_books),
    ] {
        agg.add_venue(
            VenueConfig {
                name,
                depth,
                taker_fee_ppm: fee,
            },
            books.clone(),
        )
        .ok_or("too many venues")?;
    }

    let started = Instant::now();
    let mut versions = [bn_books.version(), bb_books.version()];
    let mut next_print = started + Duration::from_secs(1);
    while !stop.load(Ordering::Relaxed) && duration.is_none_or(|d| started.elapsed() < d) {
        thread::sleep(
            next_print
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(50)),
        );
        if Instant::now() < next_print {
            continue;
        }
        next_print += Duration::from_secs(1);
        let now_versions = [bn_books.version(), bb_books.version()];
        let rates = [0, 1].map(|i| now_versions[i].wrapping_sub(versions[i]) / 2); // +2 per publish
        versions = now_versions;
        print_summary(
            &mut agg,
            &spec,
            &[&bn_books, &bb_books],
            rates,
            started.elapsed(),
        );
    }

    stop.store(true, Ordering::Relaxed);
    let exits = [bn, bb].map(|t| t.join().map_err(|_| "a feed thread panicked".to_string()));
    logger_done.store(true, Ordering::Release);
    logger.join().map_err(|_| "the logger panicked")?;
    let mut latency = String::new();
    for (name, exit) in ["binance", "bybit"].iter().zip(exits) {
        let exit = exit?;
        if let Some(table) = &exit.latency {
            latency.push_str(&format!("\n{name}\n{table}"));
        }
        if exit.record_errors > 0 {
            eprintln!(
                "{name}: {} records could not be written",
                exit.record_errors
            );
        }
        let s = exit.stats;
        println!(
            "{name:<8} messages {} applied {} (top of book {}) gaps {} rejected {} resyncs {} reconnects {} disconnects {} | log drops {}",
            s.messages,
            s.applied,
            s.top_updates,
            s.gaps,
            s.rejected,
            s.resyncs,
            s.reconnects,
            s.disconnects,
            exit.unreported_log_drops
        );
    }
    print!("{latency}");
    if let Some(rec) = recorder {
        let w = Arc::try_unwrap(rec)
            .map_err(|_| "recorder still shared")?
            .into_inner()
            .map_err(|_| "recorder poisoned")?;
        let n = w.records();
        w.finish().map_err(|e| e.to_string())?;
        println!(
            "recorded {n} records to {}",
            opts.record.unwrap_or_default()
        );
    }
    Ok(())
}

/// Runs a recording through the feeds (same code as live, minus the socket) as fast as
/// possible.
fn replay(path: &str) -> Result<(), String> {
    let spec = InstrumentSpec::from_steps("0.1", "0.001").map_err(|e| format!("{e:?}"))?;
    let open = || -> Result<_, String> {
        let file = File::open(path).map_err(|e| format!("{path}: {e}"))?;
        RecordReader::new(BufReader::with_capacity(1 << 20, file))
            .map_err(|e| format!("{path}: {e}"))
    };
    // Was Binance recorded in full-depth mode? Its REST snapshots are in the file.
    let mut scan = open()?;
    let mut full_depth = false;
    while let Ok(Some(f)) = scan.read_frame() {
        if f.venue == 0 && f.kind == Kind::Snapshot {
            full_depth = true;
            break;
        }
    }
    let mut reader = open()?;
    let (bn_pub, bn_books) = book_channel();
    let (bb_pub, bb_books) = book_channel();
    // Top of book enabled: harmless for a recording without it (no such messages).
    let binance: Box<dyn Replay> = if full_depth {
        Box::new(ReplayFeed::new(
            BinanceFullDepth::new(spec, "BTCUSDT").with_top_of_book(),
            bn_pub,
        ))
    } else {
        Box::new(ReplayFeed::new(
            Binance::new(spec, "BTCUSDT").with_top_of_book(),
            bn_pub,
        ))
    };
    let mut feeds: [Box<dyn Replay>; 2] = [
        binance,
        Box::new(ReplayFeed::new(
            Bybit::new(spec, "BTCUSDT").with_top_of_book(),
            bb_pub,
        )),
    ];
    let (mut records, mut first_mono, mut last_mono) = (0_u64, None, 0_u64);
    let started = Instant::now();
    loop {
        match reader.read_frame() {
            Ok(Some(frame)) => {
                records += 1;
                first_mono.get_or_insert(frame.mono_ns);
                last_mono = frame.mono_ns;
                if let Some(feed) = feeds.get_mut(usize::from(frame.venue)) {
                    feed.replay(&frame);
                }
            }
            Ok(None) => break,
            Err(e) => {
                eprintln!("stopped at record {records}: {e} (file cut short?)");
                break;
            }
        }
    }
    let took = started.elapsed();
    let span = Duration::from_nanos(last_mono.saturating_sub(first_mono.unwrap_or(0)));
    #[allow(clippy::cast_precision_loss)] // display only
    let rate = records as f64 / took.as_secs_f64();
    println!(
        "replayed {records} records ({:.1} s of market data) in {:.1} ms: {rate:.0} records/s, {:.0}x real time",
        span.as_secs_f64(),
        took.as_secs_f64() * 1e3,
        span.as_secs_f64() / took.as_secs_f64()
    );
    for (feed, books) in feeds.iter().zip([&bn_books, &bb_books]) {
        let s = feed.stats();
        let mut snap = Snapshot::default();
        books.read_into(&mut snap);
        let best = |l: Option<&ob_core::Level>| {
            l.map_or("-".to_string(), |l| {
                format!("{} x {}", spec.fmt_px(l.px), spec.fmt_qty(l.qty))
            })
        };
        println!(
            "\n{}: messages {} applied {} (top of book {}) gaps {} rejected {} resyncs {} | final book: {} levels, bid {}, ask {}, update id {}",
            feed.name(),
            s.messages,
            s.applied,
            s.top_updates,
            s.gaps,
            s.rejected,
            s.resyncs,
            snap.bids().len() + snap.asks().len(),
            best(snap.bids().first()),
            best(snap.asks().first()),
            snap.meta.update_id
        );
        print!("{}", feed.probe().report());
    }
    Ok(())
}

fn print_summary(
    agg: &mut Aggregator,
    spec: &InstrumentSpec,
    books: &[&BookReader],
    rates: [u64; 2],
    elapsed: Duration,
) {
    agg.refresh();
    let now = mono_ns();
    let mut out = String::new();
    use std::fmt::Write as _;
    let _ = writeln!(
        out,
        "── t={:>4}s ──────────────────────────────────────────────────────────────",
        elapsed.as_secs()
    );
    let ids: Vec<_> = agg.venue_ids().collect();
    for ((id, reader), rate) in ids.iter().zip(books).zip(rates) {
        let name = agg.venue(*id).map_or("?", |v| v.name);
        let state = match agg.state(*id, now) {
            Some(VenueState::Included) => "Live".to_string(),
            Some(VenueState::Excluded(why)) => format!("out: {why:?}"),
            None => "?".to_string(),
        };
        let top = reader.top();
        let side = |l: Option<ob_core::Level>| {
            l.map_or("-".to_string(), |l| {
                format!("{:>9} × {:<7}", spec.fmt_px(l.px), spec.fmt_qty(l.qty))
            })
        };
        let age_ms = now.saturating_sub(top.recv_ns) / 1_000_000;
        let _ = writeln!(
            out,
            "{name:<8} {state:<14} bid {}  ask {}  age {age_ms:>5} ms  {rate:>3} upd/s",
            side(top.bid),
            side(top.ask)
        );
    }
    let quote = |q: Option<&Quote>| {
        q.map_or("-".to_string(), |q| {
            format!(
                "{} × {} ({})",
                spec.fmt_px(q.px),
                spec.fmt_qty(q.qty),
                agg.venue(q.venue).map_or("?", |v| v.name)
            )
        })
    };
    let (bids, asks) = (agg.best_bids(now), agg.best_asks(now));
    let (bid, ask) = (bids.first(), asks.first());
    let cross = match (bid, ask) {
        (Some(b), Some(a)) if b.px >= a.px => "   ← the venues cross (fees exceed the gap)",
        _ => "",
    };
    let _ = writeln!(
        out,
        "best bid {}   best ask {}{cross}",
        quote(bid),
        quote(ask)
    );
    // 1 BTC, and 25 BTC: a size the 20-level streams often can't fully see (step 19).
    for size in ["1", "25"] {
        let Ok(qty) = spec.parse_qty(size) else {
            continue;
        };
        for action in [Action::Buy, Action::Sell] {
            let plan = agg.optimal_execution(action, qty, now);
            let route: Vec<String> = plan
                .children()
                .iter()
                .map(|c| {
                    format!(
                        "{} {}",
                        agg.venue(c.venue).map_or("?", |v| v.name),
                        spec.fmt_qty(c.qty)
                    )
                })
                .collect();
            let _ = writeln!(
                out,
                "{:<4} {size:>2} BTC: avg {}, incl. fees {}  via {}{}",
                if action == Action::Buy { "buy" } else { "sell" },
                plan.vwap()
                    .map_or("-".into(), |p| spec.fmt_px(p).to_string()),
                plan.vwap_including_fees()
                    .map_or("-".into(), |p| spec.fmt_px(p).to_string()),
                route.join(" + "),
                if plan.filled.is_zero() {
                    "(no live venue)"
                } else if plan.partial {
                    "  (PARTIAL: not enough visible depth)"
                } else {
                    ""
                }
            );
        }
    }
    let arb = agg.scan_arbitrage(now);
    let _ = writeln!(
        out,
        "arbitrage net of fees: {}",
        arb.best.map_or("none".to_string(), |a| format!("{a:?}"))
    );
    print!("{out}");
}
