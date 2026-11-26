//! A single-writer sequence lock ("seqlock") over `N` 64-bit words.
//!
//! **The problem.** One thread (a venue's feed) updates a book; other threads (the
//! aggregator, the printer) read it. A mutex would make the writer wait for readers and
//! readers wait for each other; the writer must never wait.
//!
//! **The idea.** A counter `seq` guards the data:
//! * The writer makes `seq` odd, writes the data, then makes `seq` even again.
//! * A reader notes `seq`, copies the data, then checks `seq` again. If it was odd, or
//!   changed, the writer was busy and the copy may be torn (half old, half new): retry.
//!
//! Writers never wait. Readers never block the writer, and only retry if they overlap a
//! write. Reading is a handful of loads.
//!
//! **Doing it correctly in Rust.** A textbook seqlock reads the data with plain loads
//! while the writer may be writing it. In Rust (and C++) that is a *data race*, which is
//! undefined behaviour even if the torn result is discarded. Following Boehm ("Can
//! Seqlocks Get Along With Programming Language Memory Models?", MSPC 2012), every data
//! word is an `AtomicU64` accessed with `Relaxed` ordering, and two fences provide the
//! ordering:
//!
//! ```text
//! writer                                   reader
//! seq = s+1          (Relaxed)              s1 = seq          (Acquire)  ┐ odd → retry
//! fence(Release)  ─┐                        data[i] → copy    (Relaxed)  │
//! data[i] = v        (Relaxed)  │           fence(Acquire)  ─┐            │
//! seq = s+2          (Release)  │           s2 = seq          (Relaxed)  ┘ s1≠s2 → retry
//! ```
//! If the reader's copy saw *any* store from a write in progress, the writer's
//! `fence(Release)` and the reader's `fence(Acquire)` synchronise, so the reader's second
//! load of `seq` must see at least `s+1` and the copy is rejected.
//!
//! On x86-64 all of this compiles to plain `mov`s: `Relaxed`/`Acquire`/`Release` loads,
//! stores and fences only restrict how the *compiler* may reorder (x86 already keeps
//! these orders in hardware). The correctness costs nothing at run time. On ARM the
//! fences become real barrier instructions (`dmb`), which is exactly what's needed there.
//!
//! There is **no `unsafe` code** in this module.

#[cfg(loom)]
pub(crate) use loom::sync::Arc;
#[cfg(loom)]
use loom::sync::atomic::{AtomicU64, Ordering, fence};

#[cfg(not(loom))]
pub(crate) use std::sync::Arc;
#[cfg(not(loom))]
use std::sync::atomic::{AtomicU64, Ordering, fence};

/// The shared state. 128-byte alignment keeps it from sharing a cache line (or the
/// adjacent line the CPU prefetches in pairs) with anything else, so two venues' locks
/// never cause each other's cache lines to bounce between cores ("false sharing").
#[repr(C, align(128))]
struct Shared<const N: usize> {
    /// Even: data is stable. Odd: a write is in progress. Starts at 0 and only grows.
    seq: AtomicU64,
    words: [AtomicU64; N],
}

/// Creates a seqlock with all words zero. Returns the only writer and a reader that can
/// be cloned for any number of reading threads.
pub fn seqlock<const N: usize>() -> (Writer<N>, Reader<N>) {
    let shared = Arc::new(Shared {
        seq: AtomicU64::new(0),
        words: core::array::from_fn(|_| AtomicU64::new(0)),
    });
    (
        Writer {
            shared: Arc::clone(&shared),
        },
        Reader { shared },
    )
}

/// The single writer. Not `Clone`: the type system guarantees there is only one, which
/// the protocol requires (two writers could interleave and both leave `seq` even).
pub struct Writer<const N: usize> {
    shared: Arc<Shared<N>>,
}

impl<const N: usize> Writer<N> {
    /// Writes new data. `fill` receives a [`WriteGuard`]; readers see either all of its
    /// stores or none of them.
    #[inline]
    pub fn write(&mut self, fill: impl FnOnce(&mut WriteGuard<'_, N>)) {
        let s = &self.shared;
        // Only this thread ever stores to `seq`, so a Relaxed load sees our own last value.
        let seq = s.seq.load(Ordering::Relaxed);
        s.seq.store(seq.wrapping_add(1), Ordering::Relaxed); // odd: write in progress
        fence(Ordering::Release); // keep the data stores after the odd seq
        fill(&mut WriteGuard { words: &s.words });
        s.seq.store(seq.wrapping_add(2), Ordering::Release); // even: data stores happen-before
    }

    /// The current version (even; grows by 2 per write).
    pub fn version(&self) -> u64 {
        self.shared.seq.load(Ordering::Relaxed)
    }
}

/// Write access to the words during [`Writer::write`].
pub struct WriteGuard<'a, const N: usize> {
    words: &'a [AtomicU64; N],
}

impl<const N: usize> WriteGuard<'_, N> {
    /// Stores word `i`. Panics if `i >= N` (a bug in the caller's layout).
    #[inline]
    pub fn set(&mut self, i: usize, value: u64) {
        self.words[i].store(value, Ordering::Relaxed);
    }
}

/// The result of a consistent read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Versioned<R> {
    /// What the read closure computed.
    pub value: R,
    /// The version (even `seq`) the value was read at.
    pub version: u64,
    /// Attempts discarded because a write was in progress.
    pub retries: u32,
}

/// A reader. Cheap to clone; give each reading thread its own.
pub struct Reader<const N: usize> {
    shared: Arc<Shared<N>>,
}

impl<const N: usize> Clone for Reader<N> {
    fn clone(&self) -> Self {
        Reader {
            shared: Arc::clone(&self.shared),
        }
    }
}

/// Read access during [`Reader::read`]. Values may be torn: use them only to compute a
/// result, never to index memory without bounds checks or to cause side effects. The
/// result is returned only if the read was consistent.
pub struct ReadGuard<'a, const N: usize> {
    words: &'a [AtomicU64; N],
}

impl<const N: usize> ReadGuard<'_, N> {
    /// Loads word `i`. Panics if `i >= N`.
    #[inline]
    pub fn get(&self, i: usize) -> u64 {
        self.words[i].load(Ordering::Relaxed)
    }
}

impl<const N: usize> Reader<N> {
    /// Runs `copy` until it sees a consistent snapshot, and returns its result.
    ///
    /// `copy` may run several times and may see torn values on the attempts that are
    /// discarded, so it must only read through the guard and compute a value (no side
    /// effects, and no indexing by values it read without clamping them first).
    #[inline]
    pub fn read<R>(&self, copy: impl FnMut(&ReadGuard<'_, N>) -> R) -> R {
        self.read_versioned(copy).value
    }

    /// [`Reader::read`], also returning the version the result belongs to (the even
    /// `seq` that was validated) and how many attempts were discarded (for metrics).
    ///
    /// Use this version, not a separate [`Reader::version`] call, to label the result: a
    /// separate load could already belong to a newer write.
    #[inline]
    pub fn read_versioned<R>(&self, mut copy: impl FnMut(&ReadGuard<'_, N>) -> R) -> Versioned<R> {
        let s = &self.shared;
        let mut retries = 0;
        loop {
            let s1 = s.seq.load(Ordering::Acquire);
            if s1 & 1 == 0 {
                let value = copy(&ReadGuard { words: &s.words });
                fence(Ordering::Acquire); // keep the data loads before the second seq load
                if s.seq.load(Ordering::Relaxed) == s1 {
                    return Versioned {
                        value,
                        version: s1,
                        retries,
                    };
                }
            }
            retries += 1;
            spin();
        }
    }

    /// The current version: changes on every write. Lets a reader skip work when nothing
    /// has changed since its last read.
    #[inline]
    pub fn version(&self) -> u64 {
        self.shared.seq.load(Ordering::Acquire)
    }
}

#[cfg(not(loom))]
#[inline]
fn spin() {
    core::hint::spin_loop(); // x86 `pause`: lets the other hyper-thread run, saves power
}

#[cfg(loom)]
fn spin() {
    loom::thread::yield_now(); // tell the model checker this thread is waiting
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    #[test]
    fn writes_are_seen_and_versions_grow_by_two() {
        let (mut w, r) = seqlock::<3>();
        assert_eq!(r.read(|g| (g.get(0), g.get(1), g.get(2))), (0, 0, 0));
        assert_eq!(r.version(), 0);
        w.write(|g| {
            g.set(0, 7);
            g.set(2, 9);
        });
        assert_eq!(r.read(|g| (g.get(0), g.get(1), g.get(2))), (7, 0, 9));
        assert_eq!((w.version(), r.version()), (2, 2));
        let r2 = r.clone();
        w.write(|g| g.set(1, 1));
        assert_eq!(
            r2.read_versioned(|g| g.get(1)),
            Versioned {
                value: 1,
                version: 4,
                retries: 0
            }
        );
        assert_eq!(r2.version(), 4);
    }

    #[test]
    fn the_shared_state_is_cache_line_isolated() {
        assert_eq!(align_of::<Shared<4>>(), 128);
        assert_eq!(size_of::<Shared<4>>(), 128); // seq + 4 words, padded
        assert_eq!(size_of::<Shared<16>>(), 256);
    }
}
