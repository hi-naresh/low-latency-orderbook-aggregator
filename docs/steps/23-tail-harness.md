# Step 23: A tail-latency harness (p50, p99, p99.9 from encrypted bytes to published book)

Everything before this step reported **means** (criterion) or a few thousand **live**
samples. Neither can answer the question that now matters: *what are p99 and p99.9 per
message, and which part of the path sets them?* This step builds the instrument that
answers it, and every later step (24–30) is judged with it.

## What changed

| File | What |
|---|---|
| `crates/ob-bench/` | **New crate.** The `tails` binary and its library: streams, probe, local exchange, scenarios, report. 7 tests. |
| `crates/ob-bench/src/stream.rs` | Endless *valid* streams from the recordings (ids shifted per pass; Binance `pu` continuity kept exactly). |
| `crates/ob-bench/src/probe.rs` | `TailProbe`: plugs into the production `Feed` through its `Probe` trait; per-stage histograms; subscription-start messages kept apart. |
| `crates/ob-bench/src/server.rs` | A local TLS WebSocket exchange that sends **open loop** (Poisson arrivals), stamping each send. |
| `crates/ob-bench/src/scenario.rs` | Four scenarios: pipeline warm, pipeline cold, wire (busy and sparse), reader (writer idle and busy). |
| `crates/ob-bench/src/stats.rs` | Pooled HDR distributions; rows of p50/p90/p99/p99.9/p99.99/max with a "too few samples" mark. |
| `scripts/bench-tails.sh` | One command per machine: records the machine, its load and the command, sizes the cache flush, writes `bench-results/tails-<model>-<date>.txt`. |
| `bench-results/` | **New.** The reference result for this Mac. |
| `docs/TAIL-LATENCY-PLAN.md` | The plan for steps 23–30 and the review of the six reference repositories (written before this step), plus the revised order this step's results call for. |
| `.gitignore` (root) | The Java skeleton's rule `bin/` would have kept `rust/crates/ob-bench/src/bin/tails.rs` out of git; an exception now keeps Rust binaries' sources in. Checked with a throwaway git directory: only build output, IDE files and the regenerable fuzz corpus are ignored. |

No production code changed.

## 1. What is measured, and how

```text
 local exchange thread                          feed thread (the production driver, unchanged)
 ─────────────────────                          ────────────────────────────────────────────────
 plan send time (Poisson)                       poll(): recv → TLS decrypt → WS decode   ← "read"
 encrypt one TLS record                         on_text(): parse → checks → apply         ← "handle"
 stamp[i] = now ───── loopback TCP ──────────▶  publish (seqlock)                         ← "publish"
 write                                          tap: now − stamp[i]                       ← "sent → published"
```

| Scenario | Path | What it isolates |
|---|---|---|
| 1. Pipeline, warm | `Feed::on_text`, back to back, in process | our code with hot caches |
| 2. Pipeline, cold | the same, reading 32 MiB (4× the L3) between messages | our code as sparse live traffic finds it |
| 3. Wire, busy / sparse | local TLS server → kernel → the production driver | the whole receive path, open loop, at 20,000 and 1,000 msg/s |
| 4. Reader | aggregator queries on another thread, writer idle / publishing every 10 µs | the read side's tail |

**Design decisions, each with the alternative it beat:**

1. **The production code, not a copy.** The feed and its driver are generic over a
   `Probe`; the harness supplies one that records into histograms. The wire scenario runs
   `run_with_tap` (the live driver) against a real TLS connection. *Alternative:* a
   re-implementation of the pipeline inside the benchmark: measures something else.
2. **Open loop.** The server plans its send times in advance (exponential gaps: Poisson
   arrivals) and never waits for the client. If the client stalls, later messages queue,
   and their latency counts the wait. *Alternative:* closed loop (send, wait, send), which
   pauses exactly when the client is slow and hides it: coordinated omission. Two of the
   six reference repositories (step 22's review) measured this way.
3. **Pooled percentiles, ≥ 10⁶ samples where it matters.** One HDR histogram per row, all
   samples. Rows mark a percentile `~` when fewer than 10 samples lie beyond it.
   *Alternative:* averaging per-run percentiles, which isn't a percentile of anything.
4. **Timed per message, never in batches.** A batch of 32 divided by 32 hides the one slow
   message: the thing being measured. The cost is the timer itself: 8–9 ns per pair
   (rdtsc), shown as the "timer floor" row.
5. **Valid endless streams.** ~100–1,000 recorded messages are replayed with every id and
   timestamp shifted by a per-pass span, so sequence rules keep passing. Binance's span is
   `max(u) − min(pu)`, which keeps the diff chain unbroken. Every run reports the feed's
   counters and is marked `INVALID RUN` if anything resynced, was rejected or reconnected.
6. **Steady state, separated from subscription start.** A looped recording repeats its
   acknowledgement and Bybit's 50-level snapshot every pass: 1.3% of Bybit messages,
   which would sit exactly at p99. Live, they come once per connection. They're
   measured in their own row.
7. **Cold means cold.** Before each cold message the harness reads and writes one byte
   per cache line of a buffer 4× the largest cache, evicting our code's data and TLB
   entries. Branch predictors aren't reset, so this is a lower bound on coldness.

**Three harness problems found by its own first runs, and fixed:**
- *Looping Binance full depth crossed the book.* Diff events set absolute quantities, so
  replaying the first events on top of the last ones' result is a state no exchange
  ever produced. Each full-depth pass now re-syncs the feed (disconnect → connect → first
  event → REST snapshot) through the real code, outside the timed region.
  `every_stream_loops_without_resyncs` fails without the fix.
- *The loop seam set Bybit's p99* (decision 6 above). Covered by
  `seams_are_the_acks_and_the_bybit_book_snapshot`.
- *A wake-up every 1 ms in the idle reader's writer thread* showed up at the reader's
  p99.9 (one query round in ~1,000). The idle writer now parks.

A fourth display fix: the "slowest recorded messages" list ranks by the mean *without*
the single worst repetition. Otherwise one 2.5 ms OS interruption of a 164-byte ticker
put it at the top.

## 2. Results on this Mac

Reference file: `rust/bench-results/tails-MacBookPro15,2_-20261126-122250.txt`
(i7-8569U, macOS 15.6, on AC power). Load average was 2.4 at the start and 7.0 at the
end; other applications were running. Rows below are p50 / p99 / p99.9.

### Our code: parse → apply → publish (steady state; "read start → published")

| Stream | Warm (10⁶ samples) | Cold (10⁴ samples) |
|---|---|---|
| Binance depth20 + bookTicker | 355 ns / **408 ns** / **5.98 µs** | 1.62 µs / **4.39 µs** / 25.0 µs |
| Bybit orderbook.50 + 1 | 560 ns / **5.46 µs** / 19.5 µs | 2.42 µs / 14.7 µs / 23.3 µs |
| Binance full depth | 2.98 µs / 64.0 µs / 102 µs | 7.68 µs / 90.5 µs / 117 µs |

### The whole receive path (wire scenario, open loop)

| | Read (recv + TLS + WS) | Handle | Publish | Read start → published | Sent → published |
|---|---|---|---|---|---|
| Binance, busy (20k msg/s) | 3.62 / 22.7 / 57.2 µs | 0.38 / 1.29 / 18.9 µs | 93 / 202 / 327 ns | **4.12 / 24.2 / 63.3 µs** | 8.97 / 79.1 / 783 µs |
| Bybit, busy | 3.85 / 22.7 / 60.2 µs | 0.58 / 6.65 / 20.4 µs | 98 / 483 / 693 ns | **4.66 / 25.8 / 69.2 µs** | 9.83 / 111 / 1,850 µs |
| Binance, sparse (1k msg/s) | 6.78 / 58.4 / 585 µs | 0.56 / 5.13 / 33.5 µs | 117 / 479 ns / 18.6 µs | **7.46 / 64.3 / 753 µs** | 49.7 / 210 / 13,450 µs |
| Bybit, sparse | 9.94 / 143 / 2,170 µs | 1.48 / 19.5 / 101 µs | 251 / 948 ns / 20.8 µs | **12.8 / 171 / 2,570 µs** | 51.5 / 3,390 / 14,460 µs |

The sparse rows ran last, as the load climbed to 7 on 4 cores. In the previous complete
run (load ~4) the same rows were Binance 7.04 / 57.0 / 159 µs and Bybit 7.67 / 52.8 /
251 µs. **The wire tails move with machine load by 3–10×; the in-process rows above
don't.** That is the point of having both.

"Sent → published" also includes the local server's `write`, the macOS loopback and the
moment our spinning thread notices the data. Its sparse p50 of ~50 µs is mostly macOS
waking things up.

### The reading side (aggregator on another thread, 10⁶ rounds)

| Query | Writer idle | Writer publishing every 10 µs |
|---|---|---|
| refresh (copy changed books) | 9 / 11 / 40 ns | 9 / 223 / 400 ns |
| best bid + best ask | 43 / 68 / 127 ns | 43 / 76 / 112 ns |
| best 10 bid levels | 55 / 78 / 132 ns | 55 / 84 / 121 ns |
| optimal execution, buy 1 BTC | 344 / 504 ns / 3.62 µs | 152 / 450 / 686 ns |
| arbitrage scan | 32 / 53 / 117 ns | 32 / 56 / 98 ns |

The timer floor is 9 / 11 / 12 ns.

### Against the target (p99 and p99.9 under 10 µs)

| | p99 | p99.9 |
|---|---|---|
| Queries (reader side) | **met** (≤ 0.5 µs) | **met** (≤ 3.6 µs) |
| Binance depth20 + ticker, our code, warm / cold | **met** (0.4 / 4.4 µs) | warm met (6.0 µs), cold not (25 µs) |
| Bybit 50 + 1, our code, warm / cold | warm met (5.5 µs), cold not (14.7 µs) | not (19.5 / 23.3 µs) |
| Binance full depth, our code | not (64 µs) | not (102 µs) |
| Whole receive path, read start → published | not (24–26 µs busy) | not (63–69 µs busy) |

## 3. What sets the tail: five findings

1. **The read stage is the biggest part of our in-process time, at the median and at
   p99.** Read is `recv` + TLS decryption + copying out + WebSocket decode. Under
   20,000 msg/s it takes 3.6 µs at p50 (9× parse and apply) and 23 µs at p99. Today it's
   one number, so the next step must split it into the system call, decryption and the
   copy before changing anything. This moves step 26 (TLS) up the plan.
2. **Message size sets the rest.** The slowest recorded messages, by mean without their
   worst repetition:

   | Message | Warm | Cold |
   |---|---|---|
   | Bybit, 2,274-byte delta | 6.2 µs | 14.9 µs |
   | Binance full depth, 24,875-byte diff | 71 µs | 113 µs |

   That's ~2.9 ns per byte warm, and these few messages are Bybit's and full depth's
   p99. A 25 KB message can't be handled in under 10 µs at this speed; it needs about
   0.3 ns per byte. That is SIMD scanning territory (step 27), plus no array shifting
   on deep inserts (step 28). Whether parse or apply dominates in those messages is not
   yet measured; step 27 must find out first.
3. **Cold costs 3–5× warm at the median** (Binance 355 ns → 1.62 µs; Bybit 560 ns →
   2.42 µs). This is step 24's target (keeping the hot path warm).
4. **On macOS, p99.9 is set by interrupts, and it sits on a cliff edge.** The same
   Binance messages read 6.0 µs at p99.9 in one row and 13.9 µs in another (the
   whole-call row). A diagnostic showed the extra time is not tied to any recorded
   message: it lands at random. With roughly 1–2 interrupts per millisecond and 0.4 µs
   per message, about one message in a thousand is hit, which is exactly p99.9. So
   p99.9 flips between the clean and the interrupted value depending on the run.
   Interrupts are a rate in time, so the fraction of messages hit is (time per message)
   × (interrupt rate). Under 10 µs at p99.9 therefore needs **both** shorter work per
   message and cores without interrupts. Only Linux can give the second (`nohz_full`,
   interrupt affinity: step 28's documentation).
5. **Publishing is not the problem except in full depth:** p99 ≤ 1 µs everywhere. Full
   depth's publish costs 381 ns at p50 against 66 ns (it rewrites 256 levels a side every
   message), which is step 25's target.

**The reading side already meets the target with room to spare,** including while the
writer publishes every 10 µs. Optimal execution varies with the book: the idle case
freezes one particular book, and its median is 344 ns against 152 ns while the books
change. Its idle p99.9 of 3.6 µs is most likely finding 4 again: longer work, more
chance of an interrupt. I haven't investigated it further; it passes either way.

## How to verify it yourself

```bash
cd rust
cargo test -p ob-bench --release           # 7 tests: streams, probe, stats, looping validity
scripts/bench-tails.sh --quick             # ~25 s smoke run
scripts/bench-tails.sh                     # the reference run (~4 min); plug in the power first
target/release/tails --rt --only reader    # one scenario
```

Things to try:
- Break the stream's continuity: in `stream.rs`, change `max_u - min_pu` to
  `max_u - min_pu + 1`, run `cargo test -p ob-bench`:
  `binance_diff_continuity_holds_across_passes` fails.
- Remove `feed.probe_mut().begin(true)` before the full-depth re-sync in
  `scenario.rs`, run `target/release/tails --only pipeline --quick`: the full-depth
  "handle" count grows by one per pass (the 1,000-level snapshot leaking into the
  steady-state rows) and its tail moves.
- Make the stream loop without the per-pass re-sync (delete the `if resets > 0` block's
  two calls): the feed reports resyncs, and the run is marked `INVALID RUN`.
- Run `scripts/bench-tails.sh` on another Mac and compare the two files row by row.

## Java counterpart

The usual Java approach is JMH with `Mode.SampleTime` (per-call samples into a histogram)
or HdrHistogram directly, which Gil Tene wrote originally for Java, together with the
coordinated-omission correction. JMH's `AverageTime` mode is the equivalent of a
criterion mean: no tails. In Java the warm/cold split matters even more: a cold path
also means interpreted or C1-compiled code, and a GC pause lands in whichever percentile
it falls in.

## Likely interview questions

1. **Why open loop?** Because latency is what a message experiences from when it is
   sent, including waiting behind a slow predecessor. A closed-loop sender stops
   sending exactly when the system is slow, so the slow periods get few samples:
   coordinated omission. The fix is to plan send times independently of responses and
   measure from the planned (here: actual) send.
2. **Why not average percentiles across runs?** The mean of ten p99s is not the p99 of the
   union of samples; with skewed distributions it can be off in either direction. Pool the
   samples, then take percentiles.
3. **How many samples does p99.9 need?** Enough that tens of samples lie beyond it:
   10⁴ gives 10 (the bare minimum), 10⁵ gives 100. The harness marks percentiles with
   fewer than 10 beyond.
4. **How do you know the harness measures the real path?** It drives the production
   `Feed` and driver through their own extension point (`Probe`, the driver's tap), and
   checks the feed's counters after every run; a test fails if looping breaks the
   sequence rules.
5. **What is "cold", and is it realistic?** Evicting caches and TLB between messages, as
   other work would during a 10–100 ms gap. It doesn't reset branch predictors, so real
   cold can be a little worse; the sparse wire scenario is the natural version.
