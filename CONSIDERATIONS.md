# Considerations: live order book data from multiple venues

The pitfalls of building one view of liquidity from Binance and Bybit BTCUSDT perpetual
futures. For each: what goes wrong, how this implementation handles it, and the evidence.
**Most of these were found by running against the live venues, not anticipated.** Step
numbers refer to the design log in `docs/steps/`.

## 1. Race conditions

**Torn reads.** A reader on another thread can see a book half-updated: some levels from
the new message, some from the old. With a lock-per-level design (the skeleton's
`handleOrder`, one level at a time) this is the normal case, not an edge case.
- *Here:* each venue's feed thread applies a **whole message** to its private book, then
  publishes it through a **seqlock**. Readers get the state before or after a message,
  never in between, and never block the writer. Done correctly under the Rust/C++
  memory model (atomics plus fences, not "plain loads that happen to work on x86").
- *Evidence:* the loom model checker explores every interleaving and weak-memory outcome;
  a multi-threaded stress test checks every snapshot; Miri reports no undefined
  behaviour. Reading the top of book costs 3.9 ns, against 16 ns with a mutex and 57 ns
  with a mutex while the writer is busy (steps 8, 22).

**Health and data must change together.** If "is this venue usable?" is stored apart from
the book, a reader can combine "Live" with a book from before a resync.
- *Here:* the venue's status (Down / Syncing / Live) and receive time are **published in
  the same seqlock write** as the levels (step 8).

**Cross-venue timing skew.** The two books are never from the same instant: Binance's depth
is conflated to 100 ms, and the venues' clocks differ. A "cross" between venues can be an
artefact of comparing a fresh book with a stale one.
- *Here:* the arbitrage scan only compares books whose exchange timestamps are within
  150 ms. Step 9 found a "fake" cross between books 15 s apart.

**Logging from the hot thread.** Formatting a log line costs ~200 ns, and writing it can
block for milliseconds.
- *Here:* the feed threads push 32-byte binary records onto a lock-free single-producer
  queue (13.9 ns), and a logger thread formats them. If the logger falls behind, records
  are dropped and *counted*, never waited for (step 15).

**Concurrent updates in Java specifically:** `synchronized` around a `TreeMap` makes
readers and the writer wait for each other. A `ConcurrentSkipListMap` avoids blocking but
still gives readers a half-applied message. The fix in any language is the same: build
privately, publish atomically.

## 2. Data consistency

**Snapshots versus deltas: know which one each stream sends.**
- **Binance `depth20@100ms` sends a complete top-20 snapshot every time.** Applying it as
  changes, as the skeleton's client does, leaves stale levels behind forever: a level
  that drops out of the top 20 is simply absent from the next message, never deleted.
  *Here:* each message replaces the book (step 6).
- **Bybit `orderbook.50` sends a snapshot, then deltas;** a delta with `u = 1` is a reset.
  *Here:* applied by `type`, with `u` sequencing (step 7).

**Sequence gaps.** A lost delta makes the book wrong until the next snapshot, silently.
- Binance: `pu` must equal the previous `u`. With snapshot streams a gap is harmless
  (counted, not acted on).
- Bybit: `u` must increase by exactly 1. A gap means the book is wrong, so **resync**:
  unsubscribe, resubscribe, rebuild from a fresh snapshot (~100 ms), with the venue
  marked Syncing and excluded meanwhile.
- Binance full depth (optional): the documented diff-stream rules (drop `u < lastUpdateId`,
  the first event must straddle it, then `pu` continuity) (step 19).

**Detecting a book that has drifted.** Beyond gaps, check invariants after every update.
A book that is crossed on one venue, or a delete for a level we don't have (on Bybit),
means our copy disagrees with the venue's, so resync. On Binance's diff stream that same
delete is documented as *normal*: the same symptom means different things on different
venues.

**Depth beyond what a stream shows.** Binance's diff stream reports changes anywhere in the
book (prices 10× away), while the snapshot covers 1,000 levels. Inserting levels outside
the snapshot's range creates a book with holes. *Here:* each side tracks a boundary of
exact knowledge and ignores updates beyond it (step 19). Validated live against Binance's
own `depth20`: 9,291 levels checked, 0 persistent mismatches.

**Conflation: a stream can be "correct" and still stale.** Measured: with only the 100 ms
depth stream, Binance's published best bid/ask was **wrong 65.8% of the time** and
typically **52 ms old**.
- *Here:* the real-time top-of-book streams (`bookTicker`, `orderbook.1`) are laid over
  the depth, reconciled by sequence number. The newest one wins; an older one arriving
  late is ignored (step 18).
- One trap found by testing: real data is self-consistent, so a stale overlay can look
  right. Synthetic out-of-order sequences are needed to test the rule.

**Capacity assumptions.** A 50-level Bybit stream sent deltas with **67 levels on one
side**. A fast market deletes levels leaving the top 50 and inserts their replacements
in one message. A book sized to exactly 50 silently loses levels. *Here:* capacity
≥ 2 × the stream's depth, with a regression test (step 12).

**Exact numbers.** Floating-point prices make level matching fragile: `0.1 + 0.2 != 0.3`.
`BigDecimal` is exact but allocates on every operation.
- *Here:* fixed-point integers per instrument (tick 0.1, lot 0.001). Off-tick or off-lot
  values are **rejected, not rounded**: a venue changing its tick size must be noticed,
  not absorbed (step 2).

## 3. Performance bottlenecks

In order of size, all measured (`docs/LATENCY.md`):

1. **The network:** a 234 ms round trip to Binance from this laptop, against ~1.5 µs of
   processing. Colocation (each venue's AWS region) is worth 1,000× any code change.
2. **Waking the thread and the kernel path:** ~45 µs per read when messages are sparse
   (cold caches, power states), against 4.5 µs when busy. Mitigations: spin rather than
   block, pin to isolated cores, real-time priority, kernel busy polling
   (`docs/TUNING.md`). On macOS, the QoS class cut the wake-up tail (p99) by 20–40%.
3. **Parsing:** ~85% of our in-process time. A hand-written, allocation-free parser takes
   1.36 µs per Binance message against 4.46 µs for typed serde_json and 22 µs for a
   generic JSON tree. Since step 25, each `["price","qty"]` level is parsed in one
   branch-free SIMD operation (SSSE3/SSE4.1): parsing is 11–37% faster depending on
   the message (most for large depth updates), because the per-number branch
   mispredicts are gone.
4. **Allocation:** zero per message on every hot path, proven by a counting allocator,
   including the error and resync paths, and since step 24 including TLS. rustls
   allocated and copied each decrypted record; it now only does the handshake, and our
   own record layer decrypts in place (rustls's p99 ~1.5 µs → 0.3–0.5 µs; its p99.9
   ~15 µs → under 1 µs).
5. **Data structures:** at 20–50 levels, a sorted array beats a tree (`BTreeMap`) by
   ~1.7× on real update patterns. At 1,000 levels it doesn't: binary searches and
   shifting deep inserts made applying half of a large full-depth message's time. Since
   step 26 the full-depth book is price-indexed (an array per tick plus a bitmap, O(1)
   per change): applying the recorded diffs takes 58% less time, and the largest
   message's apply-and-publish went from 30 to 9.7 µs.
6. **Measuring itself:** `Instant::now` costs 39 ns on macOS; rdtsc costs 6 ns. Latency
   probes are compiled out entirely unless enabled (steps 16, 20).

**Measurement pitfalls that misled me, and would mislead anyone:**
- micro-benchmarks run warm while production runs cold (2–5× difference);
- machine load moved results by up to 50%, so interleave A/B runs and keep a control;
- one-way latency against an unsynchronised clock measured 113 ms of skew, not the
  network.

## 4. Error handling

**Disconnections are routine**, not exceptional: venue restarts, Binance's 24-hour forced
disconnect, NAT timeouts, rate limits.
- **Reconnect with exponential backoff and jitter** (250 ms up to 30 s), so thousands of
  clients don't retry in lock-step. The backoff resets only after 30 s of health, so a
  connection that drops right after connecting keeps backing off.
- **The venue is excluded at once.** Its status becomes Down in the same write, and the
  aggregator quotes the other venue alone. Tested with a mock exchange that drops TCP
  mid-stream: back Live 34 ms after the loss (step 14).

**Silent failure is worse than loud failure.** A half-open TCP connection looks healthy for
minutes.
- *Here:* a feed with no message for 5 s is declared dead (both streams tick every
  20–100 ms), and Bybit's application heartbeat must be answered.
- Two independent guards: the aggregator also excludes any venue whose last message is
  older than 2 s at query time, even if the feed thread itself is stuck.

**Bad data is classified, not just dropped:**
- unparseable JSON on a snapshot stream: drop it (the next message repairs the book);
- on a delta stream: resync;
- a broken WebSocket frame: the stream itself can't be trusted, so reconnect;
- off-tick or off-lot values: the instrument changed, so surface it.

The parsers were differential-fuzzed against an independent serde_json implementation of
the same rules, on recorded, mutated and generated messages.

**Escalation.** Repeated resyncs (more than 5 a minute) mean resubscribing isn't fixing it,
so reconnect. A failed REST snapshot (e.g. HTTP 429) means reconnect with backoff, which
also respects the rate limit.

**Never block, never allocate under failure.** Gap, resync, reconnect and backoff paths
make zero allocations (proven). Recovery happens when markets are busiest.

## 5. Market structure issues

- **Different depths and update models:** Binance sends 20 levels as snapshots every
  100 ms; Bybit sends 50 levels as snapshot plus deltas every 20 ms. A merged book has to
  know each venue's visible depth, or a large order looks fillable when it isn't. *Here:*
  execution plans are marked **partial** when the visible depth runs out. With depth 20,
  a 25 BTC buy was routinely partial; full-depth mode fixes that (step 19).
- **Fees change the best venue.** Taker fees (0.05% Binance, 0.055% Bybit, base tier) are
  ~30 USDT on 1 BTC, far larger than typical price differences between the venues.
  *Here:* routing ranks levels by fee-adjusted price, which is provably optimal: costs
  are linear, so the problem is a fractional knapsack (step 10).
- **"Crossed" across venues is not an error.** Live, Binance traded ~5–8 USDT above Bybit
  for long stretches, so the merged market is "crossed" most of the time. Net of ~60 USDT
  of round-trip fees on 1 BTC it's not an arbitrage, and the scan correctly reports none.
  A cross *within* one venue's book, by contrast, means our copy is wrong.
- **Perpetual futures are not spot:**
  - funding payments make the two venues' contracts slightly different instruments;
  - the basis between venues persists rather than closing instantly;
  - margin, leverage and liquidation risk differ per venue.

  A "cheap" price on one venue can reflect its funding rate, not mispricing.
- **Tick and lot sizes differ per instrument and can change:** the design keeps them per
  instrument and rejects values that don't fit.
- **Clock skew between venues and us:** never compare a venue's timestamp with our own
  clock without synchronisation (PTP). Use round trips, or compare a venue's timestamps
  with each other.
- **Rate limits and connection limits:** the REST snapshot costs request weight 20; a
  resync storm can hit the limit, hence backoff on failure.
- **Hidden liquidity:** visible depth is not executable depth. Icebergs, latency-sensitive
  quotes that vanish when approached, and other takers racing for the same levels mean
  the price for volume is an **optimistic bound**.

## 6. What a production system would add

- **Colocation in each venue's region,** with a strategy-aware choice of where the
  cross-venue logic runs (Tokyo–Singapore ≈ 70 ms round trip).
- **Redundant connections per venue** (two sessions, first message wins), so a single
  reconnect never leaves a gap. The same "make before break" covers Binance's 24-hour
  disconnect.
- **Order entry with the same rigour:** the market-data side here is half of a trading
  system.
- **Kernel TLS, io_uring or kernel bypass, and PTP-synchronised hardware timestamps**
  (`docs/TUNING.md`).
- **Monitoring of the counters already kept:** gaps, resyncs, reconnects, log drops,
  latency histograms, and alerts on staleness.
