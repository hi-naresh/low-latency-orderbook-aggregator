# Step 15: The `ob` application and the binary logger

Everything up to step 14 is libraries and tests. This step adds the **program**: `ob
live` connects to both venues, aggregates them, and prints the merged market once a
second. It is the counterpart of the skeleton's `BookApp`.

It also enforces one latency rule from the plan: **the feed thread never formats text or
writes to a file or terminal.** Logging is a 32-byte binary record pushed onto a lock-free
queue, and a separate logger thread does the slow part.

## What changed

| File | What |
|---|---|
| `crates/ob-sync/src/spsc.rs` | **New.** A bounded single-producer single-consumer queue of fixed-size records. No `unsafe`; the crate still forbids it. |
| `crates/ob-sync/tests/loom.rs` | A loom model of the queue (records arrive whole, in order, exactly once). |
| `crates/ob-sync/tests/spsc_stress.rs` | 2 million records between two real threads, each one checked. |
| `crates/ob-sync/benches/sync.rs` | `queue/*`: the SPSC queue vs `std::sync::mpsc`. |
| `crates/ob-feed/src/log.rs` | **New.** The binary record format, `LogWriter` (feed side) and `LogReader` (logger side), with drop accounting. |
| `crates/ob-feed/benches/log.rs` | What logging one event costs: a binary record vs formatted text. |
| `crates/ob-app/` | **New crate.** The `ob` binary: live mode, logger thread, printer, Ctrl-C shutdown. |

## 1. The threads

| Thread | Does | Never does |
|---|---|---|
| `feed-binance`, `feed-bybit` | socket → TLS → WebSocket → parse → book → seqlock; logs binary records | format, print, lock, allocate per message |
| `logger` | pops records, formats them, writes them to stderr | touch a socket or a book |
| `main` | reads the published books through the aggregator once a second, prints to stdout | block a feed |

The threads share nothing except the seqlocks (books, step 8) and the SPSC queues (logs,
this step). Neither ever makes a feed thread wait.

## 2. The SPSC queue

```text
producer (feed thread)                        consumer (logger thread)
slot[h].words = record   (Relaxed stores)     h = head            (Acquire) ─┐
head = h + 1             (Release) ─────────▶  record = slot[t]    (Relaxed)  │ the whole
                                              tail = t + 1        (Release) ─┘ record is visible
t = tail                 (Acquire) ◀────────── (slot t may be reused)
```

- **Single producer, single consumer.** Each side writes one counter and only reads the
  other, so there is no compare-and-swap (CAS), only one store per push and per pop.
  (A multi-producer queue needs a CAS per push, and CASes contend.) Each venue gets its
  own queue, and the logger reads them all.
- **Fixed-size records stored as atomic words.** Every slot is `[AtomicU64; 4]`, written
  with `Relaxed` stores and published by the `Release` store of `head`. This is the
  seqlock's technique again, and it means **no `unsafe` and no data race**. The usual
  design stores `T` in an `UnsafeCell<MaybeUninit<T>>` and needs `unsafe`. On x86 both
  compile to the same plain `mov`s.
- **A power-of-two capacity**, so `index & (CAP-1)` replaces a division. It is checked
  at compile time (`const { assert!(CAP.is_power_of_two()) }`).
- **`head` and `tail` on separate 128-byte-aligned lines.** The producer writes `head`
  and the consumer writes `tail`. On one shared line, every push and pop would steal the
  line from the other core (false sharing).
- **Cached counters (Rigtorp's optimisation).** The producer keeps its last view of
  `tail` and rereads the real one only when the queue *looks* full. The consumer does the
  same with `head` when the queue looks empty. So a typical push touches only the
  producer's own line and the slot.
- **It never waits.** `try_push` returns `false` when full. Blocking a feed thread
  because the logger is slow would let logging add latency to market data, which is
  backwards.

### Measured (criterion, i7-8569U)

| Benchmark | SPSC | `std::sync::mpsc::sync_channel` |
|---|---|---|
| Push + pop on one thread | **5.8 ns** | 22.5 ns |
| Push, with the consumer draining on another core | **6.6 ns** | 20.3 ns |
| Same, **without cached counters** | 15.7 ns | – |

- The cached counters make a cross-thread push **2.4× faster** (15.7 → 6.6 ns). I
  measured it by temporarily removing the optimisation.
- `std`'s channel is a general multi-producer queue (since Rust 1.67 it is based on
  crossbeam's), so it pays for features we don't need.

### Verification

- **loom** checks the queue with 2 slots and 3 pushes, so it reaches "full" and
  wraparound. In every interleaving, records arrive whole, in order and exactly once.
  - Mutation check: weakening the consumer's `Acquire` load of `head` to `Relaxed` makes
    loom fail. The consumer can then see a slot before the producer's words are visible
    (it read stale zeros).
  - **A limitation, found by testing it:** weakening the consumer's `Release` store of
    `tail` is *not* caught. That bug lets the producer overwrite a slot the consumer is
    still reading, and seeing it requires a weak-memory behaviour ("load buffering") that
    loom doesn't model. It's documented in loom's README. The `Release` stays, justified
    by the reasoning in the diagram rather than by a test.
- **Stress test:** 2,000,000 records between two threads. Each record's four words are
  derived from its index, so a torn or reordered record is detected. It runs in 20 ms.
- **Miri** runs all ob-sync tests (with a smaller count under Miri): no undefined
  behaviour.

## 3. The binary record (`ob_feed::log`)

```text
word 0: monotonic time (ns)
word 1: venue (8 bits) | event tag (8) | sub-code a (16) | detail b (32)
word 2, 3: payloads (expected/got sequence numbers, a delay in ns, a drop count)
```

- **Every event the feed and driver report is encodable:** 7 `FeedEvent` kinds (with
  their resync and reconnect reasons), connect failures and connection losses. A
  `NetError` can own a `String`, so it is summarised as a `NetFault`: an I/O kind,
  TLS, the handshake status (403, 429), the WebSocket error, the close code, or a
  timeout. The summary keeps what an operator needs and drops what doesn't fit in
  32 bytes (rustls's error text).
- **Codes start at 1, never 0.** A zeroed or corrupt record then decodes to `None`
  instead of silently becoming the first variant. A test checks this.
- **No silent loss.** If the queue is full, `LogWriter` counts the drop. The next time
  there is room, it first pushes `Dropped(n)`, so the log says exactly how much is
  missing. A test fills the queue, overflows it by 5, and checks for `Dropped(5)`.
- **A round-trip test** encodes and decodes every variant (more than 60), including the
  extremes (`u64::MAX`, `UnknownOpcode(0xF)`).

### Measured: what one log call costs the feed thread

| How | Cost |
|---|---|
| **Binary record: encode and push** (this step) | **13.8 ns** |
| Format the same line into a reused `String` (no I/O) | 200 ns |
| Format and `write` it unbuffered to `/dev/null` (an `eprintln!`-style logger) | 802 ns |

That is **58× cheaper** than the plain approach. With a real terminal or file behind the
write, the difference is larger and much less predictable: the system call can block.
The 13.8 ns includes, averaged in, the benchmark draining the queue every 1024 calls.

In practice the feed logs only *events* (connects, gaps, resyncs), not every message. The
point is that the hot path's worst case never includes formatting or I/O, even during
a reconnect storm.

## 4. `ob live`

```bash
cd rust
cargo run --release -p ob-app -- live            # until Ctrl-C
cargo run --release -p ob-app -- live --secs 6
```

Real output (6 s run; log lines go to stderr, the summary to stdout):

```text
   0.818s binance  Connected
   0.818s bybit    Connected
   1.049s bybit    Live
   1.161s binance  Live
── t=   3s ──────────────────────────────────────────────────────────────
binance  Live           bid   60017.8 × 3.405    ask   60017.9 × 5.733    age     1 ms   10 upd/s
bybit    Live           bid   60012.8 × 0.282    ask   60012.9 × 2.307    age     7 ms   50 upd/s
best bid 60017.8 × 3.405 (binance)   best ask 60012.9 × 2.307 (bybit)   ← the venues cross (fees exceed the gap)
buy  1 BTC: avg 60012.9, incl. fees 60046  via bybit 1
sell 1 BTC: avg 60017.8, incl. fees 59987.7  via binance 1
arbitrage net of fees: none
...
binance  messages 49 applied 48 gaps 0 rejected 0 resyncs 0 reconnects 0 disconnects 0 | log drops 0
bybit    messages 239 applied 238 gaps 0 rejected 0 resyncs 0 reconnects 0 disconnects 0 | log drops 0
```

What the output shows:
- **Per venue:** its state as the aggregator sees it, top of book, the age of the last
  message, and updates per second (from the seqlock version, which grows by 2 per
  publish). Binance runs about 10/s (100 ms snapshots) and Bybit about 50/s (20 ms
  deltas).
- **Merged:** the best bid and best ask across venues. Throughout this run Binance traded
  about 5 USDT (0.008%) **above** Bybit, so the merged market is "crossed": buy on Bybit,
  sell on Binance. Fees are 0.05% + 0.055% = 0.105%, more than 10× the gap. So the
  arbitrage scan correctly reports none, and the router buys on Bybit and sells on
  Binance.
- **Health:** at t=1s both venues were still `Syncing` and the quote lines said
  `(no live venue)`. Nothing is quoted from a book the feed doesn't trust yet.
- **Shutdown:** Ctrl-C (tested with `kill -INT`) sets the stop flag. Feeds send a
  WebSocket close (1000) and return their counters, the logger drains everything left,
  and the process exits 0. If a feed is stuck, the logger still drains, because it is
  only told to finish *after* the feeds have joined.
- **Cost:** each feed thread spins on its socket (step 12), so `ob live` keeps two cores
  at 100%. That's the deliberate trade-off for the lowest wake-up latency. Step 21
  covers pinning those threads to isolated cores on Linux.

## 5. What this step deliberately doesn't do

- **A log file.** The logger writes to stderr. Writing to a file is a one-line change,
  and the format is the point.
- **Per-message timing records.** Step 16 adds the receive → parse → apply → publish
  breakdown, using rdtsc timestamps and HDR histograms.
- **Thread pinning.** macOS has no hard CPU affinity (only "affinity tags" as hints).
  That's step 21, for Linux.
- **Command-line parsing with `clap`.** Two arguments don't justify a dependency. `ctrlc`
  is the only new runtime dependency, because std has no portable signal handling.

## 6. Java counterpart

The skeleton's `BookApp`:

```java
ScheduledExecutorService scheduler = Executors.newScheduledThreadPool(1);
scheduler.scheduleAtFixedRate(() -> {
    aggregator.getBestBid().forEach(quote ->
        log.info("Best Bid: {} @ {} ({})", quote.price(), quote.quantity(), quote.exchangeId()));
}, 0, 1, TimeUnit.SECONDS);
Runtime.getRuntime().addShutdownHook(new Thread(() -> { binanceClient.close(); ... }));
```

- **The structure is the same as ours:** a printer on its own schedule, plus a shutdown
  hook (our `ctrlc` handler).
- **Logging.** In the skeleton, `log.info` (SLF4J, then Logback) runs **on the WebSocket
  thread** in `onMessage` and `onClose`. By default it formats and writes synchronously
  (≈1 µs or more) and allocates (garbage-collector pressure).
- **The Java low-latency equivalents:**
  - **Log4j2's async loggers** are built on the LMAX Disruptor (a ring buffer, like ours)
    and have a "garbage-free" mode.
  - **Chronicle Queue** writes binary records to memory-mapped files. That's the same
    idea as our records, persisted.
  - **The SPSC queue itself:** Agrona's `OneToOneRingBuffer` or JCTools' `SpscArrayQueue`.
    JCTools uses the same cached-index trick and pads its counters against false sharing
    (Java has `@Contended` for this, which needs `-XX:-RestrictContended`).
- **What Java still can't avoid:** `log.info("{}", price)` boxes or formats arguments
  unless you write garbage-free code carefully, and a GC pause can still stall the feed
  thread regardless of the logger.

## How to verify it yourself

```bash
cd rust
cargo run --release -p ob-app -- live --secs 10
cargo test -p ob-sync --release                                    # includes the 2M-record stress test
RUSTFLAGS="--cfg loom" cargo test -p ob-sync --release --test loom spsc
cargo test -p ob-feed --release --lib log                          # codec round trip, drop accounting
cargo bench -p ob-sync --bench sync -- queue
cargo bench -p ob-feed --bench log
```

Things to try (I ran all three):
- In `spsc.rs`'s `pop`, change the `Acquire` load of `head` to `Relaxed`, then run the
  loom command above. `spsc_records_arrive_whole_in_order` fails.
- In `spsc.rs`, remove the two cached-counter checks (always reload the other side's
  counter), then run `cargo bench -p ob-sync --bench sync -- queue/cross`. The SPSC
  push goes from ~6.6 ns to ~15.7 ns.
- Run `ob live` and press Ctrl-C. You get final statistics and exit code 0; the driver
  sends each venue a close frame (1000) on the way out, as the mock-exchange tests in
  step 14 check.

## Likely interview questions

1. **Why not just log with `println!` / `log.info`?**
   Formatting costs ~200 ns, and writing costs a system call that can block for
   milliseconds if the terminal or disk is slow. On the feed thread that is latency
   added directly to market data. A binary record costs 13.8 ns and can never block.
2. **Why SPSC rather than one shared multi-producer queue?**
   With one producer per queue, each side writes only its own counter, so there's no CAS
   and no contention between venues. Each push is one store. The logger reads N queues,
   which is cheap because it's off the hot path.
3. **What happens when the logger can't keep up?**
   The feed never waits: the record is dropped and counted. A `Dropped(n)` record goes
   out as soon as there is room, so the loss is visible. Blocking the feed would let the
   logger decide our market-data latency.
4. **What is false sharing, and how do you avoid it here?**
   Two variables written by different cores share a cache line, so every write
   invalidates the other core's copy. `head` and `tail` sit on separate 128-byte-aligned
   lines: 128 rather than 64 because Intel's adjacent-line prefetcher pulls in lines in
   pairs.
5. **What do the cached indices buy you?**
   A push normally reads only the producer's own state; it looks at the consumer's
   counter only when the queue seems full. Measured: 15.7 → 6.6 ns per push.
6. **How do you know the queue is correct?**
   Three independent checks: loom (every interleaving and weak-memory outcome it
   models), a 2-million-record stress test on real threads, and Miri. Plus the honest
   limit: loom doesn't model load buffering, so one ordering (`Release` on `tail`) is
   justified by reasoning rather than by a failing test.
7. **Why no `unsafe` in the queue?**
   Records are stored as atomic words: `Relaxed` stores and loads compile to plain
   `mov`s on x86, so the safety costs nothing. It does restrict the queue to
   fixed-size, word-encodable records, which is exactly what a binary log wants.
