# Step 22: Final benchmarks, the required documents, and closing the assignment's gaps

The last step: check the submission against the assignment line by line, measure
everything once more on a quiet machine, and write the documents a reviewer reads first.

## What changed

| File | What |
|---|---|
| `README.md` | **Rewritten:** headline numbers, quick start, a requirement-by-requirement map to code and tests, the Java → Rust API map, why Rust, architecture, how it's verified, limitations. |
| `docs/ASSIGNMENT.md` | The original README (the assignment), **kept verbatim**. |
| `CONSIDERATIONS.md` | **New, required:** race conditions, data consistency, performance bottlenecks, error handling, market structure, each with what was found live and how it's handled. |
| `docs/LATENCY.md` | **New:** every measured number in one place, the methodology, and the measurement mistakes made along the way. |
| `crates/ob-core/src/book.rs` | **New:** `OrderBook::market_impact` (+ `Impact` with `vwap`, `slippage`). |
| `crates/ob-agg/src/aggregator.rs` | **New:** `Aggregator::best_levels`, the best N levels across all venues. |
| Tests | +3: market impact, best levels, and both in the zero-allocation proof. **264 tests** in total. |

## 1. Checking against the assignment: two gaps found and closed

Reading the assignment again, word for word, against the code:

| Assignment wording | Was | Now |
|---|---|---|
| "methods to query best bid/ask prices **and calculate market impact**" (OrderBook) | Impact only at the aggregator level (execution plans) | `OrderBook::market_impact(take, qty)`: filled, notional, best and worst price, levels consumed, complete; `vwap()` (rounded against the taker) and `slippage()` |
| "Get **best N levels across all venues**" | Best quote *per venue*, plus internal sweeps for execution | `Aggregator::best_levels(side, now, &mut out)`: merged best first, venue-tagged, healthy venues only, into a caller buffer |

Both are allocation-free (proven: added to `tests/zero_alloc.rs`). `best_levels` is a
k-way merge with k ≤ `MAX_VENUES`: a linear scan of the heads beats a heap at that size.
`market_impact` walks one side through a generic helper, so each side gets its own
inlined loop. My first draft used `&mut dyn Iterator`, which would have meant a dynamic
call per level; I replaced it.

Everything else maps directly; the README's table lists each requirement with its code
and its tests, including the four named edge cases.

## 2. The final benchmark run

All criterion suites, run one at a time at a load average of ~2–3 (step 20's lesson:
contention distorts everything), summarised in `docs/LATENCY.md`. Highlights:

| | |
|---|---|
| Per message, parse + apply + publish | Binance 1.50 µs · Bybit 0.62 µs · bookTicker mix 0.35 µs · full depth 5.63 µs |
| Parser vs serde_json | 1.36 vs 4.46 µs (typed) vs 22.0 µs (`Value`) per Binance message |
| Seqlock vs `Mutex` | top of book 3.9 vs 16.1 ns; with a busy writer 4.2 vs 57.4 ns |
| Execution, 2 venues | 67 / 276 / 761 ns for 1 / 10 / 60 BTC |
| WebSocket decode | 8 ns per frame (46 ns with UTF-8 checking) vs 155 ns for tungstenite |

**A process mistake worth recording.** My first benchmark loop ran nothing: the shell is
zsh, which doesn't split an unquoted `$b` into words, so `cargo bench -p "ob-feed
pipeline"` was one invalid package name, and the error went into a filtered pipe. The
symptom ("finished instantly, 4 lines") gave it away.

**A documentation mistake caught before it shipped.** My first draft of `LATENCY.md`'s
live table mixed numbers from two different runs. I re-checked every figure against the
saved output of step 18's 60 s run and corrected it. Likewise the README's sample output
is now an exact excerpt of one run, labelled with its command, not a composite.

## 3. The documents

- **`CONSIDERATIONS.md`** is organised under the assignment's five headings. Every claim
  points at evidence:
  - the 65.8% stale-top measurement;
  - the 67-level Bybit delta;
  - the 15 s "fake" cross;
  - the 113 ms clock skew;
  - the zero-allocation proof;
  - the mock-exchange reconnect in 34 ms;
  - the 9,291-level live cross-check.

  It ends with what a production system would add.
- **`docs/LATENCY.md`:** the end-to-end picture (network ≫ wake-up/kernel ≫ our code),
  component tables, live histograms, freshness, how to reproduce, and a section on my
  own measurement mistakes (clock skew, machine load, mismatched PGO profiles). Being
  honest about those is part of the measurement.
- **`README.md`:** written for a reviewer with five minutes: what it does, the numbers,
  how to run it, where each requirement lives, and what it doesn't do.

## 4. Not done, deliberately

- **A Java JMH baseline** (a naive `TreeMap` + `BigDecimal` + Jackson implementation)
  would put numbers on the language choice directly. It needs a Java implementation of
  the skeleton plus JMH from Maven Central. The comparisons made instead are in Rust:
  serde_json vs our parser, `BTreeMap` vs arrays, `Mutex` vs seqlock, `std` channels vs
  SPSC. The README lists the omission under limitations.
- **Linux benchmarks:** Linux code is lint-checked here and run in CI, not measured
  (step 21).

## 5. Java counterpart

The skeleton's own test (`OrderBookAggregatorTests.shouldAggregateBestPrices`) asserts one
venue's best bid after one `handleOrder`. The Rust suite's equivalent, and what it adds
for each edge case the assignment names:

| Assignment edge case | Rust test |
|---|---|
| Zero-quantity removal | `zero_quantity_removes_the_level` (+ Bybit's "removing an absent level means drift" and Binance full depth's "it's normal") |
| Multiple updates at the same price | `repeated_updates_at_one_price_keep_the_last_quantity` |
| Crossing | `crossed_and_locked_books_are_detected`; crossed venues excluded from aggregation; cross-*venue* crosses reported as (fee-checked) arbitrage, not errors |
| Race conditions | loom model checking, multi-threaded stress tests, Miri, `tests/concurrent.rs` |

## How to verify it yourself

```bash
cd rust
cargo test --workspace --release                   # 264 tests
cargo test -p ob-agg --release --test aggregator best_levels
cargo test -p ob-core --lib market_impact
cargo bench -p ob-feed --bench pipeline            # and the other suites in docs/LATENCY.md §5
```

Then read, in order: `README.md` → `CONSIDERATIONS.md` → `docs/LATENCY.md` →
`docs/TUNING.md` → `docs/steps/`.

## Likely interview questions

1. **Walk me through the system in two minutes.**
   - One spinning thread per venue: TLS WebSocket, an allocation-free parser, and a
     sorted-array book.
   - A sans-I/O state machine handles sequencing, resyncs, heartbeats and backoff.
   - Each whole message is published through a seqlock, with the venue's health in the
     same write.
   - An aggregator on any thread merges the healthy venues: best levels, volume,
     fee-aware optimal execution, and arbitrage with a timestamp-skew guard.
   - Binary logging off the hot thread; record/replay; per-stage latency histograms.
2. **What's the biggest thing you'd change for production?**
   Location. The network round trip from here (234 ms to Binance) is ~100,000× our
   per-message processing. Then Linux host isolation, then kernel TLS or bypass.
3. **What are you least sure of?**
   - Behaviour under real stress (volatility spikes, venue incidents) beyond the 60 s
     samples and the mock scenarios.
   - The Linux tuning, which is compiled and linted here but not run.
   - Executable versus visible depth.
4. **How did you make sure your tests actually test something?**
   Mutation testing at almost every step. It found real gaps:
   - a truncated-header case;
   - two reconciliation rules the recorded data couldn't exercise;
   - a vacuous zero-allocation proof (now guarded by a control test);
   - generators that rarely produced the new message types.
5. **What did you get wrong along the way?**
   - "Exchange → us 43 ms" was clock skew.
   - A 48% "regression" was machine load.
   - A PGO profile from the wrong binary made code slower.
   - A rewritten hot loop was slower than the original.

   Each was caught by measuring again differently, and is recorded rather than hidden.
