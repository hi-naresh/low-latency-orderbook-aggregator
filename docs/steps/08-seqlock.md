# Step 8: Sharing books between threads: the seqlock

Design reference: Phase 3 of the plan ("Seqlock done correctly under Rust's memory model",
"Updates are atomic per message", "Verification"). This covers README requirement
"Proper handling of concurrent updates" and the test case "Race conditions in concurrent
updates".

## The problem

Each venue's feed thread updates its own book. Other threads need to read it: the
aggregator (best prices across venues) and the printer. The requirements:
1. **The writer never waits.** A feed that blocks behind a reader falls behind the
   exchange.
2. **Readers never see half a message.** If a 20-level message is half applied, a reader
   sees a book that never existed, possibly crossed.
3. **Reads are as cheap as possible.** The aggregator reads every venue's top of book
   constantly.

## What changed

| File | Content |
|---|---|
| `rust/crates/ob-sync/` | **New crate**, `#![forbid(unsafe_code)]` |
| `ob-sync/src/seqlock.rs` | The primitive: single-writer seqlock over `N` `AtomicU64` words; 2 unit tests |
| `ob-sync/src/book.rs` | `book_channel()` → `BookPublisher` + `BookReader`; `Top`, `Snapshot`; 5 unit tests |
| `ob-sync/tests/loom.rs` | 5 model-checking tests, including 2 deliberately broken seqlocks as controls |
| `ob-sync/tests/stress.rs` | Real threads: 1 writer + 3 readers, plus an unprotected control |
| `ob-sync/benches/sync.rs` | Seqlock vs `Mutex` vs `RwLock`, uncontended and contended |
| `ob-core/src/{divisor,search,spec}.rs` | Exhaustive test loops shortened **only under Miri** (~100× slower) |
| `.github/workflows/ci.yml` | New jobs: Miri (16 scheduler seeds) and loom |

## 1. The design

### Each feed owns its book; it *publishes* a copy after each message
```text
feed thread:  parse → apply to its private OrderBook (plain memory, fast)
                    → publisher.publish(&book, meta)   ← one seqlock write per message
other threads: reader.top() / reader.read_into(&mut snapshot)   ← never block the feed
```
- The book's fast data structure (step 3) stays plain memory with no atomics, so
  `memmove`, binary search and the rest stay fast.
- **Publishing once per message makes each message atomic for readers.** They see the
  book before the message or after it. That solves requirement 2 for both Binance
  snapshots and Bybit deltas.

### The seqlock protocol
A counter `seq` guards the data:
- **Writer:** make `seq` odd → write the data → make `seq` even.
- **Reader:** note `seq` → copy the data → check `seq` again. If it was odd, or it
  changed, the copy may be torn: retry.

The writer never waits for anyone, and readers never block it.

### Doing it correctly under the Rust/C++ memory model
The textbook seqlock copies the data with ordinary loads while the writer may be writing
it. In Rust and C++ that's a **data race, which is undefined behaviour**: the compiler may
assume it never happens and optimise accordingly, even if the torn copy is thrown away.
The correct formulation comes from Boehm, "Can Seqlocks Get Along With Programming
Language Memory Models?" (2012):
- every data word is an `AtomicU64`, accessed with `Relaxed` ordering;
- two **fences** provide the ordering.

```text
writer                                   reader
seq = s+1          (Relaxed)              s1 = seq          (Acquire)  ┐ odd → retry
fence(Release)  ─┐                        data[i] → copy    (Relaxed)  │
data[i] = v        (Relaxed)  │           fence(Acquire)  ─┐            │
seq = s+2          (Release)  │           s2 = seq          (Relaxed)  ┘ s1≠s2 → retry
```
**Why it works.** Suppose the reader's copy saw any value written during a write in
progress. Then the writer's `fence(Release)`, which comes *before* those data stores,
synchronises with the reader's `fence(Acquire)`, which comes *after* those loads. That
forces the reader's second load of `seq` to see at least `s+1`, so it can't equal `s1`
(an even number from before the write), and the copy is rejected.

**What it costs on x86: nothing extra, verified in the generated code.** I compiled
`top()` and `publish()` and disassembled them:
- **0 fence instructions, 0 `lock`-prefixed instructions, 0 `xchg`.** On x86 these
  orderings only stop the *compiler* reordering; the hardware already keeps them.
- The read loop is exactly the protocol:
  ```asm
  pause                    ; spin hint (only on retry)
  movq 128(%rbx), %rcx     ; s1 = seq
  testb $1, %cl            ; odd?
  jne  retry
  movq 136(%rbx), %r10     ; 7 data loads: offsets 136..184
  ...
  movq 184(%rbx), %rdi
  movq 128(%rbx), %r14     ; s2 = seq
  cmpq %rcx, %r14          ; changed?
  jne  retry
  ```
- On ARM the same source emits the barrier instructions ARM needs. The code is correct by
  the language's rules, not by luck on one CPU.

**No `unsafe` at all.** The crate has `#![forbid(unsafe_code)]`, so the compiler rejects
any. Many seqlock implementations use `unsafe` with `ptr::read_volatile`. That's both
undefined behaviour in theory and unnecessary.

### Single writer, enforced by the type system
```rust
pub fn seqlock<const N: usize>() -> (Writer<N>, Reader<N>)
```
- `Writer` isn't `Clone`, and `write` takes `&mut self`. **There can only be one
  writer.** The protocol requires that: two writers could interleave and both leave `seq`
  even.
- `Reader` is `Clone` and `Send`, so every reading thread gets its own.

### Layout: the top of book is one cache line
```text
offset 0   [seq][lens][best bid px][best bid qty][best ask px][best ask qty][update id][exchange ts]   ← 64 bytes
then       bids 1..64 (px, qty), asks 1..64 (px, qty)                                              ← 2,072 bytes total
```
- `top()`, the aggregator's most frequent read, touches **exactly one cache line**. The
  disassembly confirms it: all eight loads fall in offsets 128–191, one 64-byte line.
- The whole structure is `#[repr(C, align(128))]`, so two venues' seqlocks never share a
  cache line. Intel CPUs also prefetch the *adjacent* line, hence 128 rather than 64 bytes.
  Otherwise one venue's writes would slow the other venue's readers ("false sharing").

### Reading torn values safely
A discarded attempt may read garbage, such as a length of 4 billion or a negative
quantity. Every value read is **clamped before use**:
- lengths are capped at 64;
- quantities go through `Qty::from_raw(...).unwrap_or(ZERO)`.

So a torn attempt can never index out of bounds or panic. Only the consistent attempt's
result is returned.

### A bug I caught in my own first draft
`top()` first labelled its result with `self.r.version()`, a **separate** load of `seq`
made after the consistent read. If the writer published in between, the label belonged
to a *newer* book than the data. A reader that uses versions to skip unchanged books
would then skip an update it never actually saw.

**Fix:** `read_versioned` returns the exact version that was validated, so data and label
always match. A loom test (`versions_label_the_data_they_came_with`) now proves that
data and version agree in every interleaving.

## 2. Verification: four independent methods

### a) loom: exhaustive model checking
loom runs a small test under **every** thread interleaving and every value a
`Relaxed`/`Acquire` load may legally return.

| Test | Executions explored | Result |
|---|---|---|
| `seqlock_reads_are_never_torn` (reader races a writer that writes twice) | **60,284** | never torn |
| `versions_label_the_data_they_came_with` | **55,511** | version always matches data |
| `control_correct_orderings_pass` | 60,284 | passes |
| `control_missing_reader_fence_is_caught` | n/a | **loom finds a torn read** ✔ |
| `control_missing_writer_fence_is_caught` | n/a | **loom finds a torn read** ✔ |

The two controls matter most. **Remove either fence and loom finds the execution where a
torn read is accepted.** So the fences are necessary, not decoration, and the model
checker can see exactly this class of bug.

### b) Miri: undefined behaviour and data-race detection
Miri interprets the program, flagging any undefined behaviour or data race, and emulates
weak memory ordering.
- `ob-sync`: all tests pass, including the multi-threaded stress test (shortened under
  Miri).
- **The stress test under 16 different scheduler seeds:** exit status 0, nothing reported.
- `ob-core` (38 tests): clean.
  - The first run was too slow: the exhaustive loops added in steps 2–4 check hundreds of
    thousands of cases, and Miri runs about 100× slower than native.
  - I shortened those loops **only under `cfg(miri)`**: still the same shapes (every
    edge case kind), just fewer values. The full run now takes 20 s. Native runs are
    unchanged.

### c) Stress test on real hardware (1 writer + 3 readers, 5 s)
| | Result |
|---|---|
| Publishes (back to back, as fast as possible) | 4,008,107 |
| Full-book reads | 41,769,515 |
| **Inconsistent reads accepted** | **0** |
| Retried attempts | 24% |
| **Control: same race without the protocol** | **75.1% of reads torn** (5.6 M of 7.5 M) |

Each publish `k` writes a self-checking book: every quantity equals `k`, prices are
shifted by `k`, and the level count depends on `k`. So any mix of two publishes is
detected. The control shows tearing is real and frequent on this CPU. Without it,
"zero inconsistent reads" would prove nothing.

### d) Why not ThreadSanitizer (the plan mentioned it)
TSan only reports races on *non-atomic* memory, and every shared access here is atomic,
so it has nothing to report. TSan also doesn't model standalone fences, which is exactly
what this design relies on. loom and Miri check fences properly. I dropped TSan rather
than run a check that can't fail.

## 3. Performance

Criterion, i7-8569U, two runs. "Contended" means another core publishes every ~2 µs,
which is about 10,000× busier than a real feed.

| Operation | Seqlock | `Mutex` | `RwLock` |
|---|---|---|---|
| Read top of book | **3.3–3.4 ns** | 17.0–17.6 ns | 17.3–18.2 ns |
| Read top, contended | **3.6–3.7 ns** | 60–61 ns | n/a |
| Read full 20+20 book | **29–31 ns** | 50 ns | n/a |
| Read full 20+20, contended | **32–34 ns** | 155–165 ns | n/a |
| Read full 50+50 book | 71–103 ns | 103–107 ns | n/a |
| Publish 20+20 (writer, per message) | **36–40 ns** | 64–86 ns | n/a |
| Publish 50+50 (writer, per message) | 76–164 ns | **62–67 ns** | n/a |

**Reading these honestly:**
- **Top-of-book reads are 5× faster uncontended and 17× faster contended.** A mutex makes
  readers and the writer fight over the lock's cache line; seqlock readers never write
  shared memory, so they don't fight at all.
- **`RwLock` is no better than `Mutex` for readers.** Taking a read lock still *writes*
  the lock's reader count, a shared cache line.
- **Publishing a large (50+50) book is slower than the mutex design**:
  - the mutex version swaps 2 KB in with one fast SIMD `memcpy`;
  - the seqlock does 200 separate atomic stores (plain `mov`s, but one by one).

  At about 76 ns per message it's still small next to the 0.5–1.6 µs of parsing. The fix
  if it ever matters is to publish only the levels that changed: a Bybit delta touches
  1–4 levels, not 100. That's noted for step 20.
- **Under contention, the seqlock read barely changes** (3.4 → 3.6 ns). The cache line is
  pulled across cores once per write (~70–100 ns); the hundreds of reads in between are
  cache hits. Criterion reports averages; tail latency per read is measured in step 16.

**A risk found by the stress test: reader starvation.** With the writer publishing back
to back (~800,000 times a second), one read needed **106,268 retries**. A seqlock favours
the writer, so if writes never stop, a reader can keep losing the race. Real feeds publish
every 20–100 ms and a write window is ~40–80 ns, so the chance of a reader overlapping
even one write is tiny. If it ever mattered, the standard fix is **double buffering**:
two slots, with the writer alternating between them, so a reader always has a stable slot
and needs at most one retry. Step 16 measures retries with real message timing.

## 4. Alternatives considered

| Option | Why not |
|---|---|
| `Mutex<OrderBook>` | Writer can block behind readers; 5–17× slower reads (measured) |
| `RwLock` | Readers still write the lock word (shared cache line); no faster (measured); writers can starve |
| `Arc` swap of immutable books (RCU, `arc-swap`) | An allocation per publish, plus reference-count traffic on every read |
| Send every update over a channel to one aggregator thread | Good design for some systems, but every reader must be on that thread, and queue hops add latency; still an option for the strategy thread later |
| Textbook seqlock with plain or `volatile` reads | Data race = undefined behaviour in Rust/C++; needs `unsafe`; no faster (the correct version compiles to the same `mov`s) |
| Make the book itself atomic | Every book operation (search, `memmove`) would have to use atomics; slows the writer's hot path |

## 5. Java counterpart
The skeleton:
```java
private final ConcurrentHashMap<String, OrderBook> books = new ConcurrentHashMap<>();
public void handleOrder(PriceLevel priceLevel) {          // called per *level* by the WS thread
    books.computeIfAbsent(priceLevel.exchangeId(), OrderBook::new).handleOrder(priceLevel);
}
// BookApp: a scheduler thread calls aggregator.getBestBid() every second
```
- **`ConcurrentHashMap` only protects the map of books, not the books.** The WebSocket
  thread mutates an `OrderBook` while the scheduler thread reads it, with no
  synchronisation. Whatever goes inside (`TreeMap`?) isn't thread-safe. That's a real
  data race in Java too: readers can see a structure mid-rebalance, or throw a
  `ConcurrentModificationException`.
- **Even with `ConcurrentSkipListMap` inside, updates are per level.** A reader can see
  levels 1–6 of a 20-level message applied and 7–20 not: a book that never existed.

The low-latency Java version of this step:
- **The seqlock exists in Java too:** `StampedLock.tryOptimisticRead()` / `validate()` is
  exactly this protocol, in the JDK since Java 8. LMAX/Agrona-style code does it by hand
  with `VarHandle` `getAcquire` / `setRelease` and `VarHandle.acquireFence()` /
  `releaseFence()`.
- **Java's memory model** (JSR-133, JDK 9+ `VarHandle` modes) maps onto the same
  acquire/release reasoning.
- **Java has no loom or Miri.** The closest tool is jcstress, which runs tests millions
  of times on real hardware and reports the outcomes it observed: empirical, not
  exhaustive.

## How to verify it yourself
```bash
cd rust
cargo test -p ob-sync                                                   # unit + 3 s stress test
STRESS_SECS=10 cargo test -p ob-sync --release --test stress -- --nocapture   # prints counts, incl. control
RUSTFLAGS="--cfg loom" cargo test -p ob-sync --release --test loom      # model checking (~1 min)
LOOM_LOG=info RUSTFLAGS="--cfg loom" cargo test -p ob-sync --release --test loom torn -- --nocapture   # prints iterations
cargo +nightly miri test -p ob-sync                                     # UB / data races (~1.5 min)
MIRIFLAGS="-Zmiri-many-seeds=0..16" cargo +nightly miri test -p ob-sync --test stress
cargo +nightly miri test -p ob-core --lib                               # ~20 s
cargo bench -p ob-sync --bench sync                                     # seqlock vs Mutex vs RwLock
```
Things to try:
- **Remove a fence:** in `seqlock.rs`, delete `fence(Ordering::Release);` in `write`, and
  run the loom tests. `seqlock_reads_are_never_torn` fails.
- **Bring back the version bug:** in `book.rs`, change `version: read.version` in `top()`
  to `version: self.r.version()`. It still compiles and the unit tests pass, but now data
  and version can disagree under a race. This is why the loom version test exists.

## Likely interview questions
1. **What is a seqlock, and when is it better than a mutex?**
   An optimistic read protocol: readers copy and validate against a counter; one writer
   never waits. It's best when reads vastly outnumber writes and the data is small enough
   to copy, which is exactly a top of book.
2. **Isn't a seqlock a data race?**
   The textbook version is, and that's undefined behaviour in Rust and C++. Making the
   data atomic with `Relaxed` ordering, plus a release fence in the writer and an acquire
   fence in the reader, makes it race-free (Boehm 2012). On x86 it compiles to the same
   plain `mov`s; I checked the disassembly.
3. **Why does the writer need a fence after the first increment?**
   Without it, the data stores could become visible before the odd `seq`. A reader could
   then see new data with an old even `seq` on both loads and accept a torn copy. loom
   finds exactly that execution when the fence is removed.
4. **Why does the reader need an acquire fence before the second load of `seq`?**
   Without it, the second `seq` load could effectively happen before the data loads, so
   the check wouldn't cover them. loom also finds that when the fence is removed.
5. **How do you prove concurrent code correct?**
   Several independent methods:
   - loom, exhaustive over 60k executions, with negative controls showing it catches
     missing fences;
   - Miri, for undefined behaviour and races with weak-memory emulation, across 16
     scheduler seeds;
   - a real-hardware stress test of 41 M reads, with a control showing 75% tearing
     without the protocol.
6. **What's false sharing, and how do you avoid it?**
   Two threads writing different data on the same cache line force that line to bounce
   between cores. Each seqlock is aligned to 128 bytes, covering Intel's adjacent-line
   prefetch.
7. **Can readers starve?**
   Yes, under continuous writes. The stress test saw 106k retries at 800k writes per
   second. Real feeds write every 20–100 ms, so it's negligible. The fix if needed is
   double buffering.
8. **Why copy the book instead of making it atomic?**
   The writer's hot path (search, `memmove`) stays plain fast memory; the copy is one
   bounded write per message (36–76 ns).
