//! Real threads on real hardware: one writer publishing as fast as it can, several
//! readers checking that **every** read is a consistent book.
//!
//! Each publish `k` writes a book that is internally checkable: every quantity equals `k`
//! and the update ID equals `k`, the prices are a fixed ladder shifted by `k`, and the
//! number of levels depends on `k`. A torn read (some words from publish `k`, some from
//! `k+1`) breaks at least one of these.
//!
//! A control runs the same race with the protocol removed, to show that tearing really
//! happens on the machine running the test; otherwise "zero torn reads" would prove nothing.
//!
//! Duration: `STRESS_SECS` (default 3; Miri uses a handful of iterations instead).
#![cfg(not(loom))]
#![allow(
    clippy::unwrap_used,
    clippy::print_stdout,
    clippy::cast_possible_wrap,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)] // test bookkeeping with small, non-negative counters

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use ob_core::{Level, OrderBook, Px, Qty};
use ob_sync::{BookMeta, Snapshot, book_channel};

const READERS: usize = 3;

fn duration() -> Duration {
    let secs = std::env::var("STRESS_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(3);
    Duration::from_secs(secs)
}

/// The book for publish `k`: 20–51 levels per side (varies with k), prices shifted by k,
/// every quantity = k.
fn book_for(k: u64) -> OrderBook {
    let k = k as i64;
    let n = 20 + (k % 32);
    let bids: Vec<Level> = (0..n)
        .map(|i| Level::new(Px::from_raw(1_000_000 + k - i), Qty::from_raw(k).unwrap()))
        .collect();
    let asks: Vec<Level> = (0..n)
        .map(|i| Level::new(Px::from_raw(1_000_001 + k + i), Qty::from_raw(k).unwrap()))
        .collect();
    let mut b: OrderBook = OrderBook::new();
    b.replace(&bids, &asks).unwrap();
    b
}

/// Checks a snapshot is exactly `book_for(k)` for its own k.
fn consistent(s: &Snapshot) -> bool {
    let k = s.meta.update_id as i64;
    let n = (20 + (k % 32)) as usize;
    if k == 0 {
        return s.bids().is_empty() && s.asks().is_empty(); // nothing published yet
    }
    s.meta.exchange_ts == k as u64
        && s.meta.recv_ns == k as u64
        && s.meta.status == ob_sync::VenueStatus::Live
        && s.bids().len() == n
        && s.asks().len() == n
        && s.bids()
            .iter()
            .enumerate()
            .all(|(i, l)| l.px.raw() == 1_000_000 + k - i as i64 && l.qty.raw() == k)
        && s.asks()
            .iter()
            .enumerate()
            .all(|(i, l)| l.px.raw() == 1_000_001 + k + i as i64 && l.qty.raw() == k)
        && s.version == 2 * k as u64
}

#[test]
fn readers_never_see_a_torn_book() {
    let (mut publisher, reader) = book_channel();
    // Pre-build the books so the writer spends its time publishing, not allocating.
    let books: Vec<OrderBook> = (1..=64).map(book_for).collect();
    let stop = Arc::new(AtomicBool::new(false));
    let iterations = if cfg!(miri) { 20 } else { u64::MAX };

    let writer = {
        let stop = Arc::clone(&stop);
        thread::spawn(move || {
            let mut k = 0_u64;
            while !stop.load(Ordering::Relaxed) && k < iterations {
                k += 1;
                // The book content cycles through 64 shapes; k keeps growing, so reuse the
                // shape but rebuild quantities/prices when needed.
                let book = if k <= 64 {
                    books[(k - 1) as usize].clone()
                } else {
                    book_for(k)
                };
                publisher.publish(
                    &book,
                    BookMeta {
                        update_id: k,
                        exchange_ts: k,
                        recv_ns: k,
                        status: ob_sync::VenueStatus::Live,
                    },
                );
            }
            k
        })
    };

    let readers: Vec<_> = (0..READERS)
        .map(|_| {
            let (reader, stop) = (reader.clone(), Arc::clone(&stop));
            thread::spawn(move || {
                let mut snap = Snapshot::default();
                let (mut reads, mut retries, mut max_retries, mut bad) =
                    (0_u64, 0_u64, 0_u32, 0_u64);
                let mut last_version = 0;
                while !stop.load(Ordering::Relaxed) && (!cfg!(miri) || reads < 20) {
                    let r = reader.read_into(&mut snap);
                    reads += 1;
                    retries += u64::from(r);
                    max_retries = max_retries.max(r);
                    if !consistent(&snap) || snap.version < last_version {
                        bad += 1;
                    }
                    last_version = snap.version;
                    // The one-cache-line read must be consistent too.
                    let top = reader.top();
                    let k = top.exchange_ts as i64;
                    if k > 0
                        && (top.bid.unwrap().qty.raw() != k
                            || top.ask.unwrap().px.raw() != 1_000_001 + k)
                    {
                        bad += 1;
                    }
                }
                (reads, retries, max_retries, bad)
            })
        })
        .collect();

    if !cfg!(miri) {
        thread::sleep(duration());
    }
    let start = Instant::now();
    if !cfg!(miri) {
        stop.store(true, Ordering::Relaxed);
    }
    let publishes = writer.join().unwrap();
    stop.store(true, Ordering::Relaxed);
    let mut total = (0, 0, 0, 0);
    for r in readers {
        let (reads, retries, max_retries, bad) = r.join().unwrap();
        total = (
            total.0 + reads,
            total.1 + retries,
            total.2.max(max_retries),
            total.3 + bad,
        );
    }
    println!(
        "publishes: {publishes}, full reads: {}, retried attempts: {} ({:.2}%), max retries in one read: {}, inconsistent: {} ({:?} to stop)",
        total.0,
        total.1,
        100.0 * total.1 as f64 / total.0.max(1) as f64,
        total.2,
        total.3,
        start.elapsed()
    );
    assert_eq!(total.3, 0, "a reader accepted an inconsistent book");
    assert!(total.0 > 0 && publishes > 0);
}

/// Control: the same race with no sequence counter. Readers copy 41 words while the
/// writer rewrites them. Counts how often the copy mixes two publishes.
#[test]
#[cfg_attr(
    miri,
    ignore = "timing-dependent demonstration, not a correctness check"
)]
fn without_the_protocol_reads_do_tear() {
    const N: usize = 41;
    let words: Arc<[AtomicU64; N]> = Arc::new(core::array::from_fn(|_| AtomicU64::new(0)));
    let stop = Arc::new(AtomicBool::new(false));
    let writer = {
        let (words, stop) = (Arc::clone(&words), Arc::clone(&stop));
        thread::spawn(move || {
            let mut k = 0;
            while !stop.load(Ordering::Relaxed) {
                k += 1;
                for w in words.iter() {
                    w.store(k, Ordering::Relaxed);
                }
            }
        })
    };
    let (mut reads, mut torn) = (0_u64, 0_u64);
    let end = Instant::now() + Duration::from_secs(1);
    while Instant::now() < end {
        let first = words[0].load(Ordering::Relaxed);
        if words.iter().any(|w| w.load(Ordering::Relaxed) != first) {
            torn += 1;
        }
        reads += 1;
    }
    stop.store(true, Ordering::Relaxed);
    writer.join().unwrap();
    println!(
        "unprotected: {torn} torn of {reads} reads ({:.1}%)",
        100.0 * torn as f64 / reads as f64
    );
}
