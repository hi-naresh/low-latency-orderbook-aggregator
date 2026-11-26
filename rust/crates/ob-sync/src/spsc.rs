//! A bounded single-producer single-consumer queue of fixed-size records, `W` 64-bit
//! words each. Built for the binary logger (step 15): a feed thread hands events to a
//! logger thread **without locks, allocation or system calls, and never waits**. When the
//! queue is full, the push fails and the producer counts what it dropped.
//!
//! **Layout.** `CAP` slots (a power of two, so `index & (CAP - 1)` replaces `%`), plus
//! two counters that only grow:
//! * `head`: records pushed so far. Written by the producer only.
//! * `tail`: records popped so far. Written by the consumer only.
//!
//! `head - tail` is the number of records waiting. Each side writes one counter and reads
//! the other, so there is no compare-and-swap anywhere: one store per push and pop.
//!
//! **Ordering** (same technique as the seqlock, so no `unsafe` and no data race):
//! ```text
//! producer                                  consumer
//! slot[h].words = record   (Relaxed)        h = head            (Acquire) ─┐
//! head = h + 1             (Release) ──────▶ record = slot[t]    (Relaxed)  │ sees the
//!                                           tail = t + 1        (Release) ─┘ whole record
//! t = tail                 (Acquire) ◀────── (slot t is free to reuse)
//! ```
//! The producer's `Release` on `head` publishes the words written before it; the
//! consumer's `Acquire` load of `head` makes them visible. Symmetrically, the consumer's
//! `Release` on `tail` tells the producer the slot has been read and may be overwritten.
//! On x86-64 every one of these is a plain `mov`.
//!
//! **Cache behaviour.** `head` and `tail` live on separate 128-byte-aligned lines, so the
//! two threads don't invalidate each other's line on every operation ("false sharing").
//! Each side also keeps a *cached copy* of the other's counter and only reloads it when
//! the cached value says "full" (producer) or "empty" (consumer). In the common case a
//! push touches only the producer's own line and the slot (Rigtorp's optimisation).

#[cfg(loom)]
use loom::sync::atomic::{AtomicU64, Ordering};
#[cfg(not(loom))]
use std::sync::atomic::{AtomicU64, Ordering};

use crate::seqlock::Arc;

#[repr(C, align(128))]
struct Padded(AtomicU64);

struct Shared<const W: usize, const CAP: usize> {
    head: Padded,
    tail: Padded,
    slots: Box<[[AtomicU64; W]]>,
}

/// Creates a queue with `CAP` slots of `W` words. `CAP` must be a power of two
/// (checked at compile time).
pub fn spsc<const W: usize, const CAP: usize>() -> (Producer<W, CAP>, Consumer<W, CAP>) {
    const { assert!(CAP.is_power_of_two(), "CAP must be a power of two") };
    let shared = Arc::new(Shared {
        head: Padded(AtomicU64::new(0)),
        tail: Padded(AtomicU64::new(0)),
        slots: (0..CAP)
            .map(|_| core::array::from_fn(|_| AtomicU64::new(0)))
            .collect(),
    });
    (
        Producer {
            shared: Arc::clone(&shared),
            head: 0,
            tail_cache: 0,
        },
        Consumer {
            shared,
            tail: 0,
            head_cache: 0,
        },
    )
}

#[allow(clippy::cast_possible_truncation)] // `index < CAP`, a usize
fn slot(index: u64, cap: usize) -> usize {
    (index & (cap as u64 - 1)) as usize
}

/// The pushing end. Not `Clone`: exactly one producer.
pub struct Producer<const W: usize, const CAP: usize> {
    shared: Arc<Shared<W, CAP>>,
    /// Our own `head` (we are its only writer, so no need to load it).
    head: u64,
    /// The consumer's `tail` as last seen; may be stale (too low), never too high.
    tail_cache: u64,
}

impl<const W: usize, const CAP: usize> Producer<W, CAP> {
    /// Appends a record. Returns `false`, without waiting, if the queue is full.
    #[inline]
    pub fn try_push(&mut self, record: &[u64; W]) -> bool {
        if self.head - self.tail_cache == CAP as u64 {
            // Looks full: refresh our view of the consumer (one cross-core read).
            self.tail_cache = self.shared.tail.0.load(Ordering::Acquire);
            if self.head - self.tail_cache == CAP as u64 {
                return false;
            }
        }
        let s = &self.shared.slots[slot(self.head, CAP)];
        for (word, value) in s.iter().zip(record) {
            word.store(*value, Ordering::Relaxed);
        }
        self.head += 1;
        self.shared.head.0.store(self.head, Ordering::Release);
        true
    }
}

/// The popping end. Not `Clone`: exactly one consumer.
pub struct Consumer<const W: usize, const CAP: usize> {
    shared: Arc<Shared<W, CAP>>,
    tail: u64,
    head_cache: u64,
}

impl<const W: usize, const CAP: usize> Consumer<W, CAP> {
    /// Takes the oldest record, or `None` if the queue is empty.
    #[inline]
    pub fn pop(&mut self) -> Option<[u64; W]> {
        if self.tail == self.head_cache {
            self.head_cache = self.shared.head.0.load(Ordering::Acquire);
            if self.tail == self.head_cache {
                return None;
            }
        }
        let s = &self.shared.slots[slot(self.tail, CAP)];
        let record = core::array::from_fn(|i| s[i].load(Ordering::Relaxed));
        self.tail += 1;
        self.shared.tail.0.store(self.tail, Ordering::Release);
        Some(record)
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    #[test]
    fn fifo_and_full() {
        let (mut p, mut c) = spsc::<2, 4>();
        assert_eq!(c.pop(), None);
        for i in 0..4 {
            assert!(p.try_push(&[i, i * 10]));
        }
        assert!(!p.try_push(&[9, 9]), "full");
        assert_eq!(c.pop(), Some([0, 0]));
        assert!(p.try_push(&[4, 40]), "a slot was freed");
        for i in 1..=4 {
            assert_eq!(c.pop(), Some([i, i * 10]));
        }
        assert_eq!(c.pop(), None);
    }

    #[test]
    fn wraps_around_many_times() {
        let (mut p, mut c) = spsc::<1, 8>();
        for i in 0..1000 {
            assert!(p.try_push(&[i]));
            assert_eq!(c.pop(), Some([i]));
        }
    }
}
