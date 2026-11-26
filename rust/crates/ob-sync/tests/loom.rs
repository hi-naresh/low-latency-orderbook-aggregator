//! Model checking with loom: every interleaving of the threads, and every value a
//! `Relaxed`/`Acquire` load is *allowed* to return under the C++/Rust memory model, is
//! explored. A test passes only if its assertion holds in all of them.
//!
//! Run: `RUSTFLAGS="--cfg loom" cargo test -p ob-sync --release --test loom`
//!
//! The data is two words that the writer always sets to the same value. A torn read
//! (half old, half new) shows up as two different values.
#![cfg(loom)]
#![allow(clippy::unwrap_used)]

use loom::sync::Arc;
use loom::sync::atomic::{AtomicU64, Ordering, fence};
use loom::thread;
use ob_sync::seqlock::seqlock;

/// The real seqlock: a reader racing a writer that writes twice never sees a torn pair.
#[test]
fn seqlock_reads_are_never_torn() {
    loom::model(|| {
        let (mut w, r) = seqlock::<2>();
        let writer = thread::spawn(move || {
            for v in 1..=2 {
                w.write(|g| {
                    g.set(0, v);
                    g.set(1, v);
                });
            }
        });
        let (a, b) = r.read(|g| (g.get(0), g.get(1)));
        assert_eq!(a, b, "torn read");
        writer.join().unwrap();
        // After the writer finished, the reader must see the final value.
        assert_eq!(r.read(|g| (g.get(0), g.get(1))), (2, 2));
    });
}

/// Versions grow and match the data they label.
#[test]
fn versions_label_the_data_they_came_with() {
    loom::model(|| {
        let (mut w, r) = seqlock::<1>();
        let writer = thread::spawn(move || {
            for v in 1..=2 {
                w.write(|g| g.set(0, v));
            }
        });
        let first = r.read_versioned(|g| g.get(0));
        let second = r.read_versioned(|g| g.get(0));
        // Write k sets the value k and produces version 2k.
        assert_eq!(first.version, 2 * first.value);
        assert_eq!(second.version, 2 * second.value);
        assert!(second.version >= first.version);
        writer.join().unwrap();
    });
}

// ---------------------------------------------------------------------------
// Negative controls: deliberately broken seqlocks. If loom couldn't catch these, the
// passing tests above would prove nothing.
// ---------------------------------------------------------------------------

struct Broken {
    seq: AtomicU64,
    words: [AtomicU64; 2],
}

/// Runs a writer (two writes) against one reader using the given orderings and fences,
/// and asserts the reader never accepts a torn pair.
fn race(writer_fence: bool, reader_fence: bool) {
    loom::model(move || {
        let s = Arc::new(Broken {
            seq: AtomicU64::new(0),
            words: [AtomicU64::new(0), AtomicU64::new(0)],
        });
        let w = Arc::clone(&s);
        let writer = thread::spawn(move || {
            for v in 1..=2 {
                let seq = w.seq.load(Ordering::Relaxed);
                w.seq.store(seq + 1, Ordering::Relaxed);
                if writer_fence {
                    fence(Ordering::Release);
                }
                w.words[0].store(v, Ordering::Relaxed);
                w.words[1].store(v, Ordering::Relaxed);
                w.seq.store(seq + 2, Ordering::Release);
            }
        });
        loop {
            let s1 = s.seq.load(Ordering::Acquire);
            if s1 % 2 == 1 {
                thread::yield_now();
                continue;
            }
            let (a, b) = (
                s.words[0].load(Ordering::Relaxed),
                s.words[1].load(Ordering::Relaxed),
            );
            if reader_fence {
                fence(Ordering::Acquire);
            }
            if s.seq.load(Ordering::Relaxed) == s1 {
                assert_eq!(a, b, "torn read accepted");
                break;
            }
            thread::yield_now();
        }
        writer.join().unwrap();
    });
}

#[test]
fn control_correct_orderings_pass() {
    race(true, true);
}

#[test]
#[should_panic(expected = "torn read accepted")]
fn control_missing_reader_fence_is_caught() {
    race(true, false);
}

#[test]
#[should_panic(expected = "torn read accepted")]
fn control_missing_writer_fence_is_caught() {
    race(false, true);
}

/// The SPSC queue, with 2 slots so "full" and wrap-around are reached: every record
/// arrives whole (its two words always equal), in order, exactly once, and a push only
/// fails when the queue really is full.
#[test]
fn spsc_records_arrive_whole_in_order() {
    use ob_sync::spsc::spsc;
    loom::model(|| {
        let (mut p, mut c) = spsc::<2, 2>();
        let producer = thread::spawn(move || {
            let mut pushed = 0;
            for v in 1..=3_u64 {
                if p.try_push(&[v, v]) {
                    pushed += 1;
                } else {
                    break; // full: a real producer would count a drop
                }
            }
            pushed
        });
        let mut got = Vec::new();
        for _ in 0..3 {
            if let Some([a, b]) = c.pop() {
                assert_eq!(a, b, "torn record");
                got.push(a);
            }
        }
        let pushed = producer.join().unwrap();
        while let Some([a, b]) = c.pop() {
            assert_eq!(a, b, "torn record");
            got.push(a);
        }
        assert_eq!(got, (1..=pushed).collect::<Vec<_>>());
        assert!(pushed >= 2, "two slots always fit two records");
    });
}
