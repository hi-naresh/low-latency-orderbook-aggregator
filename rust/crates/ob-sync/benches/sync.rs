//! Seqlock vs `Mutex` vs `RwLock` for sharing a venue's book between threads.
//!
//! * `read_top/*`: best bid + ask, the aggregator's most frequent read.
//! * `read_full/*`: a full copy of a 20+20 (Binance) or 50+50 (Bybit) book.
//! * `publish/*`: the writer's cost per message.
//! * `contended/*`: the same reads while another core publishes every ~2 µs (far busier
//!   than a real feed), which adds the cost of moving cache lines between cores.
//!
//! * `queue/*`: the SPSC log queue vs `std::sync::mpsc`.
//!
//! Run: `cargo bench -p ob-sync --bench sync`
#![allow(clippy::unwrap_used, clippy::cast_possible_wrap, missing_docs)]

use std::hint::black_box;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::{Duration, Instant};

use criterion::{Criterion, criterion_group, criterion_main};
use ob_core::{Level, OrderBook, Px, Qty};
use ob_sync::{BookMeta, Snapshot, book_channel};

fn book(depth: i64, shift: i64) -> OrderBook {
    let l = |px: i64, q: i64| Level::new(Px::from_raw(px), Qty::from_raw(q).unwrap());
    let bids: Vec<Level> = (0..depth).map(|i| l(600_000 + shift - i, i + 1)).collect();
    let asks: Vec<Level> = (0..depth).map(|i| l(600_001 + shift + i, i + 1)).collect();
    let mut b: OrderBook = OrderBook::new();
    b.replace(&bids, &asks).unwrap();
    b
}

/// Copies a book's levels into a snapshot-like pair of vectors (what a Mutex reader does).
fn copy_levels(b: &OrderBook, bids: &mut Vec<Level>, asks: &mut Vec<Level>) {
    bids.clear();
    asks.clear();
    bids.extend(b.bids().iter());
    asks.extend(b.asks().iter());
}

fn uncontended(c: &mut Criterion) {
    for depth in [20, 50] {
        let b = book(depth, 0);
        let (mut publisher, reader) = book_channel();
        publisher.publish(
            &b,
            BookMeta {
                update_id: 1,
                exchange_ts: 1,
                ..BookMeta::default()
            },
        );
        let mutex = Mutex::new(b.clone());
        let rwlock = RwLock::new(b.clone());

        if depth == 20 {
            let mut g = c.benchmark_group("read_top");
            g.bench_function("seqlock", |x| x.iter(|| black_box(&reader).top()));
            g.bench_function("mutex", |x| {
                x.iter(|| {
                    let b = black_box(&mutex).lock().unwrap();
                    (b.best_bid(), b.best_ask())
                });
            });
            g.bench_function("rwlock", |x| {
                x.iter(|| {
                    let b = black_box(&rwlock).read().unwrap();
                    (b.best_bid(), b.best_ask())
                });
            });
            g.finish();
        }

        let mut g = c.benchmark_group(format!("read_full/{depth}x{depth}"));
        let mut snap = Snapshot::default();
        g.bench_function("seqlock", |x| {
            x.iter(|| black_box(&reader).read_into(&mut snap))
        });
        let (mut vb, mut va) = (Vec::with_capacity(64), Vec::with_capacity(64));
        g.bench_function("mutex", |x| {
            x.iter(|| copy_levels(&black_box(&mutex).lock().unwrap(), &mut vb, &mut va))
        });
        g.finish();

        let mut g = c.benchmark_group(format!("publish/{depth}x{depth}"));
        g.bench_function("seqlock", |x| {
            x.iter(|| {
                publisher.publish(
                    black_box(&b),
                    BookMeta {
                        update_id: 2,
                        exchange_ts: 2,
                        ..BookMeta::default()
                    },
                )
            });
        });
        // Mutex design: the writer swaps the new book in under the lock.
        let next = b.clone();
        g.bench_function("mutex", |x| {
            x.iter(|| *black_box(&mutex).lock().unwrap() = next.clone())
        });
        g.finish();
    }
}

/// Runs `body` while another thread publishes every ~2 µs.
fn contended(c: &mut Criterion) {
    let books = [book(20, 0), book(20, 1)];
    let (mut publisher, reader) = book_channel();
    let mutex = Arc::new(Mutex::new(books[0].clone()));
    let stop = Arc::new(AtomicBool::new(false));
    let writer = {
        let (stop, mutex, books) = (Arc::clone(&stop), Arc::clone(&mutex), books.clone());
        thread::spawn(move || {
            let mut k = 0_u64;
            while !stop.load(Ordering::Relaxed) {
                k += 1;
                let b = &books[(k % 2) as usize];
                publisher.publish(
                    b,
                    BookMeta {
                        update_id: k,
                        exchange_ts: k,
                        ..BookMeta::default()
                    },
                );
                b.clone_into(&mut mutex.lock().unwrap());
                let until = Instant::now() + Duration::from_micros(2);
                while Instant::now() < until {
                    std::hint::spin_loop();
                }
            }
        })
    };

    let mut g = c.benchmark_group("contended");
    g.bench_function("read_top/seqlock", |x| x.iter(|| black_box(&reader).top()));
    g.bench_function("read_top/mutex", |x| {
        x.iter(|| {
            let b = black_box(&*mutex).lock().unwrap();
            (b.best_bid(), b.best_ask())
        });
    });
    let mut snap = Snapshot::default();
    g.bench_function("read_full_20x20/seqlock", |x| {
        x.iter(|| black_box(&reader).read_into(&mut snap))
    });
    let (mut vb, mut va) = (Vec::with_capacity(64), Vec::with_capacity(64));
    g.bench_function("read_full_20x20/mutex", |x| {
        x.iter(|| copy_levels(&black_box(&*mutex).lock().unwrap(), &mut vb, &mut va));
    });
    g.finish();
    stop.store(true, Ordering::Relaxed);
    writer.join().unwrap();
}

/// The log queue: the producer's cost per record, which is what a feed thread
/// pays to log. `same_thread`: push then pop, no other core involved. `cross_thread`: a
/// consumer drains on another core, so each push also pays for moving cache lines.
fn queues(c: &mut Criterion) {
    use ob_sync::spsc::spsc;
    use std::sync::mpsc::{TrySendError, sync_channel};
    type Rec = [u64; 4];
    let rec: Rec = [1, 2, 3, 4];

    let mut g = c.benchmark_group("queue/same_thread");
    let (mut p, mut q) = spsc::<4, 1024>();
    g.bench_function("spsc", |b| {
        b.iter(|| {
            p.try_push(black_box(&rec));
            black_box(q.pop())
        })
    });
    let (tx, rx) = sync_channel::<Rec>(1024);
    g.bench_function("std_mpsc_sync_channel", |b| {
        b.iter(|| {
            tx.try_send(black_box(rec)).unwrap();
            black_box(rx.try_recv().ok())
        })
    });
    g.finish();

    let mut g = c.benchmark_group("queue/cross_thread_push");
    g.bench_function("spsc", |b| {
        b.iter_custom(|iters| {
            let (mut p, mut q) = spsc::<4, 1024>();
            let done = Arc::new(AtomicBool::new(false));
            let stop = Arc::clone(&done);
            let consumer = thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    if black_box(q.pop()).is_none() {
                        std::hint::spin_loop();
                    }
                }
            });
            let start = Instant::now();
            for _ in 0..iters {
                while !p.try_push(black_box(&rec)) {
                    std::hint::spin_loop();
                }
            }
            let t = start.elapsed();
            done.store(true, Ordering::Relaxed);
            consumer.join().unwrap();
            t
        })
    });
    g.bench_function("std_mpsc_sync_channel", |b| {
        b.iter_custom(|iters| {
            let (tx, rx) = sync_channel::<Rec>(1024);
            let consumer = thread::spawn(move || {
                // Drains until every sender is gone.
                loop {
                    match rx.try_recv() {
                        Ok(r) => {
                            black_box(r);
                        }
                        Err(std::sync::mpsc::TryRecvError::Empty) => std::hint::spin_loop(),
                        Err(_) => break,
                    }
                }
            });
            let start = Instant::now();
            for _ in 0..iters {
                let mut r = black_box(rec);
                while let Err(TrySendError::Full(back)) = tx.try_send(r) {
                    r = back;
                    std::hint::spin_loop();
                }
            }
            let t = start.elapsed();
            drop(tx);
            consumer.join().unwrap();
            t
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
    targets = uncontended, contended, queues
}
criterion_main!(benches);
