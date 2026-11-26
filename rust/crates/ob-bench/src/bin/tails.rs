//! `tails`: per-message latency distributions (p50 … p99.99, max) for every stage from
//! encrypted bytes to the published book, and for the aggregator's queries.
//!
//! ```text
//! cargo run --release -p ob-bench --bin tails -- [options]
//!   --quick           ÷10 samples (a smoke test, not a measurement)
//!   --only NAME       pipeline | wire | reader | readpath | anatomy | jitter
//!   --rt              the measured threads ask for real-time priority (as `ob live --rt`)
//!   --cold-mb N       bytes read between messages in cold mode (default 32 MiB; use
//!                     more than the last-level cache: scripts/bench-tails.sh does)
//! ```
//!
//! Prefer `scripts/bench-tails.sh`, which also records the machine and its load.
#![allow(clippy::print_stdout)] // a report printer

use std::time::Instant;

use ob_bench::scenario::{self, Cold, FeedResult, ReaderResult};
use ob_bench::stats::{Dist, HEADER, human, row};
use ob_bench::stream::Kind;

struct Opts {
    scale: f64,
    only: Option<String>,
    rt: bool,
    cold_bytes: usize,
}

fn opts() -> Result<Opts, String> {
    let mut o = Opts {
        scale: 1.0,
        only: None,
        rt: false,
        cold_bytes: 32 << 20,
    };
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--quick" => o.scale = 0.1,
            "--rt" => o.rt = true,
            "--only" => o.only = args.next(),
            "--cold-mb" => {
                let mb: usize = args
                    .next()
                    .and_then(|s| s.parse().ok())
                    .ok_or("--cold-mb needs a number")?;
                o.cold_bytes = mb << 20;
            }
            other => return Err(format!("unknown option {other}")),
        }
    }
    Ok(o)
}

fn main() {
    let o = match opts() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };
    let clock = quanta::Clock::new();
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let n = |base: usize| ((base as f64 * o.scale) as usize).max(1000);
    let run = |name: &str| o.only.as_deref().is_none_or(|x| x == name);

    println!("ob-bench tails (step 23)");
    println!(
        "  arch {} · os {} · {} hardware threads · real-time priority {}",
        std::env::consts::ARCH,
        std::env::consts::OS,
        std::thread::available_parallelism().map_or(0, std::num::NonZero::get),
        if o.rt { "requested" } else { "off" },
    );
    println!(
        "  SIMD level parser: {}",
        if ob_parse::SIMD_LEVELS {
            "on (SSSE3/SSE4.1)"
        } else {
            "OFF: scalar parser (build without target-cpu x86-64-v2, or not x86-64)"
        }
    );
    println!(
        "  timer: one clock-read pair costs ~{} ns (subtract nothing: it is part of every row)",
        timer_floor(&clock)
    );
    println!(
        "  `~` after a value: fewer than 10 samples beyond that percentile (too few to trust)"
    );
    scenario::tune_thread(o.rt);

    let kinds = [Kind::BinanceTop, Kind::BybitTop, Kind::BinanceFull];
    if run("pipeline") {
        section("1. Pipeline, warm: parse → apply → publish, back to back (in process)");
        for kind in kinds {
            let t = Instant::now();
            let r = scenario::pipeline(kind, n(1_000_000), None, &clock);
            feed_report(kind, &r, t);
        }
        section(&format!(
            "2. Pipeline, cold: the same, reading {} MiB between messages to flush the caches",
            o.cold_bytes >> 20
        ));
        for kind in kinds {
            let t = Instant::now();
            let r = scenario::pipeline(
                kind,
                n(10_000),
                Some(Cold {
                    bytes: o.cold_bytes,
                }),
                &clock,
            );
            feed_report(kind, &r, t);
        }
    }
    if run("wire") {
        for (label, gap_ns, count) in [
            ("busy: mean gap 50 µs (20,000 msg/s)", 50_000, 200_000),
            ("sparse: mean gap 1 ms (1,000 msg/s)", 1_000_000, 30_000),
        ] {
            section(&format!(
                "3. Wire, open loop, {label}: local TLS server → kernel → read + TLS + WS → parse → apply → publish"
            ));
            for kind in [Kind::BinanceTop, Kind::BybitTop] {
                let t = Instant::now();
                let r = scenario::wire(kind, n(count), gap_ns, o.rt, &clock);
                feed_report(kind, &r, t);
            }
        }
    }
    if run("readpath") {
        section("5. The read stage, taken apart (rustls in memory; recv on loopback)");
        for kind in [Kind::BinanceTop, Kind::BybitTop] {
            let t = Instant::now();
            let r = ob_bench::readpath::read_breakdown(kind, n(200_000), &clock);
            println!(
                "  {} ({:.1} s, mean record {} B)",
                kind.name(),
                t.elapsed().as_secs_f64(),
                r.mean_bytes
            );
            for (name, d) in [
                ("recv, data waiting", &r.recv_data),
                ("recv, nothing yet (each idle spin)", &r.recv_empty),
                ("rustls: take in (read_tls)", &r.tls_take),
                ("rustls: decrypt + Vec (process)", &r.tls_decrypt),
                ("rustls: copy out (reader.read)", &r.tls_copy),
                ("rustls: all three", &r.tls_total),
                ("WebSocket decode", &r.ws),
                ("AES-128-GCM alone, in place (floor)", &r.aead),
                ("ours: decrypt in place (step 24)", &r.ours),
            ] {
                println!("{}", row(&format!("  {name}"), d));
            }
        }
    }
    if run("anatomy") {
        println!(
            "\n6. Message anatomy: the slowest recorded messages, parse vs the rest (medians)"
        );
        println!(
            "  {:<28} {:>7} {:>7} {:>10} {:>10} {:>12} {:>8}",
            "", "bytes", "levels", "whole", "parse", "apply+pub", "parse/B"
        );
        for kind in [Kind::BybitTop, Kind::BinanceFull] {
            let all = ob_bench::anatomy::anatomy(kind, n(2_000).min(2_000), &clock);
            let (p, w): (u64, u64) = all
                .iter()
                .fold((0, 0), |(p, w), a| (p + a.parse_ns, w + a.whole_ns));
            #[allow(clippy::cast_precision_loss)]
            let share = 100.0 * p as f64 / w.max(1) as f64;
            println!(
                "  {} (parse is {share:.0}% of all handling time)",
                kind.name()
            );
            for a in all.iter().take(5) {
                #[allow(clippy::cast_precision_loss)]
                let per_byte = a.parse_ns as f64 / a.bytes.max(1) as f64;
                println!(
                    "    line {:<22} {:>7} {:>7} {:>10} {:>10} {:>12} {:>6.2} ns",
                    a.line,
                    a.bytes,
                    a.levels,
                    human(a.whole_ns),
                    human(a.parse_ns),
                    human(a.rest_ns()),
                    per_byte
                );
            }
        }
    }
    if run("jitter") {
        let secs = if o.scale < 1.0 { 2 } else { 10 };
        println!(
            "\n7. Jitter: a thread spinning on the clock for {secs} s; every gap is the OS taking the CPU"
        );
        let j = ob_bench::jitter::jitter(std::time::Duration::from_secs(secs), &clock);
        println!("  {:<40} {HEADER}", "");
        println!("{}", row("gap between consecutive clock reads", &j.gaps));
        #[allow(clippy::cast_precision_loss)]
        let lost = 100.0 * j.lost_ns as f64 / j.elapsed_ns.max(1) as f64;
        #[allow(clippy::cast_precision_loss)]
        let per_s = |n: u64| n as f64 * 1e9 / j.elapsed_ns.max(1) as f64;
        println!(
            "    interruptions per second: {:.0} over 1 µs, {:.0} over 10 µs, {:.1} over 100 µs; CPU time lost: {lost:.3}%",
            per_s(j.over_1us),
            per_s(j.over_10us),
            per_s(j.over_100us)
        );
    }
    if run("reader") {
        for (label, busy) in [
            ("writer idle", false),
            ("writer busy: a message every 10 µs", true),
        ] {
            section(&format!(
                "4. Reader: aggregator queries on another thread, {label}"
            ));
            let t = Instant::now();
            let r = scenario::reader(n(1_000_000), busy, 10_000, o.rt, &clock);
            reader_report(&r, t);
        }
    }
}

fn section(title: &str) {
    println!("\n{title}");
    println!("  {:<40} {HEADER}", "");
}

fn feed_report(kind: Kind, r: &FeedResult, started: Instant) {
    println!(
        "  {} ({:.1} s)",
        kind.name(),
        started.elapsed().as_secs_f64()
    );
    let rows: [(&str, &Dist); 6] = [
        if r.sent.is_some() {
            ("  sent → published (end to end)", &r.message)
        } else {
            ("  message (whole handler call)", &r.message)
        },
        ("  read (socket + TLS + WS)", &r.read),
        ("  handle (parse + apply)", &r.handle),
        ("  publish (seqlock)", &r.publish),
        ("  read start → published", &r.total),
        ("  subscription-start msgs (excluded)", &r.seam),
    ];
    for (name, d) in rows {
        if !d.is_empty() {
            println!("{}", row(name, d));
        }
    }
    let s = r.stats;
    println!(
        "    feed: {} messages, {} applied, {} gaps, {} rejected, {} resyncs, {} reconnects{}{}",
        s.messages,
        s.applied,
        s.gaps,
        s.rejected,
        s.resyncs,
        s.reconnects,
        if r.resets > 0 {
            format!(", {} re-syncs between passes (untimed)", r.resets)
        } else {
            String::new()
        },
        if r.valid() { "" } else { "  ← INVALID RUN" }
    );
    if let Some(sent) = r.sent {
        println!(
            "    server: {} sent, {} started > 20 µs late (sender descheduled)",
            sent.messages, sent.late
        );
    }
    if !r.slowest.is_empty() {
        let list: Vec<String> = r
            .slowest
            .iter()
            .map(|c| {
                format!(
                    "line {} ({} B): mean {} (without the worst), max {}",
                    c.line,
                    c.bytes,
                    ob_bench::stats::human(c.mean_ns),
                    ob_bench::stats::human(c.max_ns)
                )
            })
            .collect();
        println!("    slowest recorded messages: {}", list.join("; "));
    }
    for e in &r.events {
        println!("    event: {e}");
    }
}

fn reader_report(r: &ReaderResult, started: Instant) {
    println!(
        "  ({:.1} s, {} messages published meanwhile)",
        started.elapsed().as_secs_f64(),
        r.published
    );
    for (name, d) in [
        ("timer floor (empty)", &r.timer),
        ("refresh (copy changed books)", &r.refresh),
        ("best bid + best ask", &r.best_bid_ask),
        ("best 10 bid levels", &r.best_levels),
        ("optimal execution, buy 1 BTC", &r.execution),
        ("arbitrage scan", &r.arbitrage),
    ] {
        println!("{}", row(name, d));
    }
}

/// Median cost of two consecutive clock reads.
fn timer_floor(clock: &quanta::Clock) -> u64 {
    let mut d = Dist::new();
    for _ in 0..100_000 {
        let a = clock.raw();
        let b = clock.raw();
        d.record(clock.delta_as_nanos(a, b));
    }
    d.at(0.5)
}
