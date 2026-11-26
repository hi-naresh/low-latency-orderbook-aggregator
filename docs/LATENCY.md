# Latency: what was measured, how, and what it means

Every number below was measured on this project's code; none is estimated. Where a
number comes from a specific step, the step note has the details.

**Machine:** MacBook Pro, Intel Core i7-8569U (4 cores / 8 threads, 2.8 GHz base),
macOS (Darwin 24.6), Rust 1.100.0. Release profile: fat LTO, 1 codegen unit,
`panic = "abort"`, portable (no `target-cpu=native`; step 20 found no measurable gain).
Final benchmark runs were made with a load average of ~2–3; step 20 shows why the
comparisons that matter were interleaved A/B runs (absolute numbers drift ~±25% with
machine load on a laptop).

## Since step 23: tails, TLS, parsing, the full-depth book

Steps 23–28 measured **p50, p99 and p99.9 per stage** with an open-loop harness
(`rust/scripts/bench-tails.sh`, results in `rust/bench-results/`), then improved what it
found. Interleaved A/B results (load-independent):

| Change | Before → after |
|---|---|
| TLS decryption per record (step 24: own record layer, decrypting in place) | 459–513 ns → **173–180 ns** at p50; p99.9 15–18 µs → **0.4–0.5 µs**; 1 allocation → **0** |
| Parsing a 1,000-level full-depth message (step 25: SIMD, one whole price level per call) | 30–31 → **19–20 ns per level** (−37%); depth20 −14%, Bybit −11% |
| Applying full-depth diffs (step 26: price-indexed book) | 172 → **69–73 µs** for the recorded stream (−58%); largest message 60 → **37 µs** |

Step 23 found what sets the tails: on this Mac, the `recv` system call (~3 µs, ~85% of
the read stage) and OS interruptions (~4,000 per second; step 28's jitter test), not our
code. `docs/LINUX.md` covers what changes on the Linux target. The tables below are the
step 22 baseline.

## 1. The whole picture, per market-data message

```text
exchange ──── network ────▶ kernel ──▶ read + TLS + WebSocket ──▶ parse + apply ──▶ publish ──▶ readers
          234 ms round trip         ~45 µs live (cold), 84 ns–4.5 µs (warm)   1.5 µs     ~0.1–0.3 µs   3.9 ns
          to Binance from here      (step 16, 18)                   (Binance depth20)               (top of book)
```

| Stage | Measured | Source |
|---|---|---|
| Network (round trip, TCP connect) | **Binance 234–244 ms; Bybit edge 7–10 ms** | step 18 correction |
| Kernel → our read (Linux only, `--kernel-timestamps`) | not measurable on macOS (no TCP receive timestamps) | step 21 |
| Socket read + TLS + WebSocket, live | p50 **~45 µs** at 10 msg/s (cold); **84 ns** p50 at ~1,265 msg/s (several messages per TLS record) | steps 16, 18 |
| Same, local loopback | 4.5 µs per read at a message every 200 µs; 19.6 µs every 20 ms | step 16 |
| Parse + apply + publish, warm (pipeline benchmark) | Binance depth20 **1.50 µs**; Bybit 50 **0.62 µs**; bookTicker mix **0.35 µs**; full depth **5.63 µs** | `benches/pipeline.rs` |
| Same, live (cold) | 2–5× the warm numbers | step 16 |
| Publish (seqlock), live p50 | 105–403 ns | steps 16, 18 |
| Read top of book (another thread) | **3.9 ns**; 4.2 ns while the writer publishes | `ob-sync/benches/sync.rs` |
| Optimal execution, 2 venues | **67 ns** (1 BTC), 276 ns (10 BTC), 761 ns (60 BTC) | `ob-agg/benches/agg.rs` |

**The ranking that matters:** network (hundreds of ms) ≫ cold-thread wake-up and kernel
(~45 µs) ≫ our code (~1.5 µs). That's why colocation comes first in `docs/TUNING.md`, and
why "measure, don't assume" mattered in every step.

## 2. Component benchmarks (criterion, final run)

### Parsing (59 recorded messages per Binance batch)

| | ob-parse | serde_json typed | serde_json `Value` |
|---|---|---|---|
| Binance depth20, per message | **1.36 µs** | 4.46 µs (3.3×) | 22.0 µs (16×) |
| Bybit 50×50 snapshot | **3.45 µs** | 9.50 µs (2.8×) | – |
| Bybit 59 deltas | **23.2 µs** (0.39 µs each) | 68.2 µs (2.9×) | – |

Plus: **0 allocations** per message (proven, step 17), and byte-for-byte agreement with an
independent serde_json oracle on recorded, mutated and generated messages (steps 6–7, 18,
19).

### Decimal parsing (per batch)

| scalar (kept) | SWAR (rejected, step 5) | reference | `f64` parse |
|---|---|---|---|
| 678 µs | 889 µs (+31%) | 743 µs | 1,374 µs |

### The order book (`ob-core`)

| | Array + linear search (kept) | Array + branchless binary | `BTreeMap` |
|---|---|---|---|
| Apply, top-heavy updates (real distribution) | **4.97 ms** | 5.24 ms | 8.58 ms |
| Replace a 20×20 snapshot | **58.7 ns** | – | 1.45 µs (rebuild) |

### Sharing between threads (`ob-sync`)

| | Seqlock (kept) | `Mutex` | `RwLock` |
|---|---|---|---|
| Read top of book | **3.9 ns** | 16.1 ns | 15.8 ns |
| … while the writer publishes | **4.2 ns** | 57.4 ns | – |
| Read full 20×20 book | **26.6 ns** | 47.2 ns | – |
| Read full 20×20, contended | **29.9 ns** | 158.5 ns | – |
| Publish 20×20 | **32.4 ns** | 59.8 ns | – |

| Log queue | SPSC (kept) | `std::sync::mpsc` |
|---|---|---|
| Push, consumer on another core | **6.7 ns** | 19.9 ns |

### Aggregation (2 venues)

| refresh (unchanged / both changed) | best bid + ask | volume at or better | execution 1 / 10 / 60 BTC | arbitrage scan |
|---|---|---|---|---|
| 3.4 ns / 201 ns | 36.8 ns | 33.9 ns | 67 / 276 / 761 ns | 27.1 ns |

### WebSocket decoding (per recorded batch)

| | ob-net, no UTF-8 check | ob-net, UTF-8 checked (default) | tungstenite |
|---|---|---|---|
| Binance (60 frames) | 485 ns (8 ns/frame) | 2.78 µs (46 ns/frame) | 9.29 µs (155 ns/frame) |
| Bybit (60 frames) | 479 ns | 1.59 µs | 6.75 µs |

### Observability costs

| | Cost |
|---|---|
| Binary log record (encode + push) | **13.9 ns** (text formatting: 197 ns; format + write: 815 ns) |
| Latency probe on, per message | +4% (replay clock) to +6% (system clock) |
| rdtsc timestamp | 6.2 ns (`Instant::now`: 39 ns) |

## 3. Live, end to end (60 s, `ob live --latency`)

From step 18 (Binance depth20 + bookTicker, Bybit 50 + 1):

| Stage | Binance p50 / p99 / p99.9 | Bybit p50 / p99 / p99.9 |
|---|---|---|
| read (socket + TLS + WS) | 84 ns / 84.9 µs / 1.28 ms | 35.3 µs / 987 µs / 12.3 ms |
| handle (parse + apply) | 372 ns / 4.7 µs / 24.5 µs | 2.57 µs / 21.5 µs / 328 µs |
| publish | 105 ns / 658 ns / 9.0 µs | 403 ns / 3.3 µs / 14.6 µs |

The p99.9 and maximum values are the OS descheduling the thread on a laptop with no core
isolation. `ob live --rt` (macOS QoS class) cut wake-up p99 by 20–40% and maxima 4–8×
in step 21's test; Linux isolation, the subject of `docs/TUNING.md`, is the real fix.

## 4. Market-data freshness (step 18)

Measured with exchange timestamps only (immune to clock skew), 59 s live:

| | Binance | Bybit |
|---|---|---|
| Depth-only best bid/ask wrong | **65.8%** of the time | 4.2% |
| Depth-only top older by (p50 / max) | **52 / 108 ms** | 0 / 112 ms |

The real-time top-of-book overlay removes this for the price of ~0.4 µs per ticker.

## 5. How to reproduce

```bash
cd rust
cargo bench -p ob-feed --bench pipeline          # per-message pipeline on recorded data
cargo bench -p ob-parse --bench binance          # vs serde_json
cargo bench -p ob-sync  --bench sync             # seqlock vs Mutex/RwLock; SPSC vs mpsc
cargo bench -p ob-agg   --bench agg
cargo run --release -p ob-app -- live --secs 60 --latency      # live stage histograms
cargo test -p ob-net --release --test latency -- --ignored --nocapture   # wake-up latency
```

Compare two builds with interleaved runs (A, B, A, B…) rather than against a saved
baseline: step 20 found machine load moving results by up to 50%.

## 6. Mistakes in my own measurements, kept for the record

- **"Exchange → us ~43 ms" (step 16) was mostly clock skew:** the laptop clock was 113 ms
  off. Corrected in step 18. One-way latency needs synchronised clocks; round trips don't.
- **A "+48%" regression (step 20) was machine load,** exposed by a control benchmark that
  shouldn't have moved (+51%).
- **PGO "slower" (step 20) was a profile from a different binary.** With the same binary
  and unseen data, it gave −10 to −16%.
