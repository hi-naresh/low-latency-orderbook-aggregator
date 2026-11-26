//! What logging one event costs the feed thread.
//!
//! * `binary_record`: encode 32 bytes + push onto the SPSC queue (what we do).
//! * `format_only`: format the same event as text into a reused `String` (no I/O at all;
//!   the cheapest possible text logging).
//! * `format_and_write`: format, then `write` to `/dev/null` unbuffered (what a plain
//!   `eprintln!`-style logger costs: a system call per line, with no terminal behind it).
//!
//! Run: `cargo bench -p ob-feed --bench log`
#![allow(clippy::unwrap_used, missing_docs)]

use std::fmt::Write as _;
use std::hint::black_box;
use std::io::Write as _;
use std::time::Duration;

use criterion::{Criterion, criterion_group, criterion_main};
use ob_feed::log::{LogEvent, log_channel};
use ob_feed::{FeedEvent, ResyncReason};

fn event() -> LogEvent {
    LogEvent::Feed(FeedEvent::Resync(ResyncReason::Gap {
        expected: 22_042_296,
        got: 22_042_298,
    }))
}

fn bench(c: &mut Criterion) {
    let mut g = c.benchmark_group("log_one_event");
    let (mut w, mut r) = log_channel(1);
    let mut n = 0_u64;
    g.bench_function("binary_record", |b| {
        b.iter(|| {
            w.log(black_box(123_456_789), black_box(event()));
            n += 1;
            if n.is_multiple_of(1024) {
                while r.pop().is_some() {} // the logger thread's job, kept out of most iterations
            }
        })
    });
    let mut line = String::with_capacity(128);
    g.bench_function("format_only", |b| {
        b.iter(|| {
            line.clear();
            let _ = writeln!(
                line,
                "{:>4}.{:03}s {:<8} {}",
                12,
                345,
                "bybit",
                black_box(event())
            );
            black_box(&line);
        })
    });
    let mut null = std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/null")
        .unwrap();
    g.bench_function("format_and_write", |b| {
        b.iter(|| {
            line.clear();
            let _ = writeln!(
                line,
                "{:>4}.{:03}s {:<8} {}",
                12,
                345,
                "bybit",
                black_box(event())
            );
            null.write_all(line.as_bytes()).unwrap();
        })
    });
    g.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .warm_up_time(Duration::from_millis(500))
        .measurement_time(Duration::from_secs(3))
        .sample_size(50);
    targets = bench
}
criterion_main!(benches);
