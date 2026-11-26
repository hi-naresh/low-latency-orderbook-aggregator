# Crypto Order Book Aggregator: low-latency Rust implementation

A real-time order book aggregator for Binance and Bybit BTCUSDT perpetual futures,
implementing the take-home in [`docs/ASSIGNMENT.md`](docs/ASSIGNMENT.md) (the original
README, kept verbatim). The Java skeleton is untouched in `src/`; the implementation is
a Rust workspace in [`rust/`](rust/).

**Headline numbers** (measured; details in [`docs/LATENCY.md`](docs/LATENCY.md)):

| | |
|---|---|
| Parse + apply + publish one Binance depth message | **1.5 µs** (3.3× faster than typed serde_json, 16× faster than a JSON tree) |
| Read the best bid/ask from another thread | **3.9 ns**, without blocking the writer |
| Optimal execution for 1 BTC across both venues, fee-aware | **67 ns** |
| Heap allocations per message, hot path and error paths | **0** (proven by test) |
| TLS decryption per record (own record layer over rustls's handshake) | **0.18 µs**, in place; p99.9 under 0.6 µs (rustls alone: 15–18 µs) |
| Largest Binance full-depth message (25 KB, 1,177 levels) | **37 µs** (was 60 µs): SIMD level parser, price-indexed book |
| Best bid/ask staleness removed by the real-time overlay | Binance top was wrong **66%** of the time without it |
| Tests | **299** (+7 ignored live/measurement tests), plus loom model checking, Miri, fuzzing |

All numbers are from the development Mac (Intel i7-8569U, 2019). Tail latencies (p99,
p99.9) per stage come from an open-loop harness (`rust/scripts/bench-tails.sh`);
[`docs/BENCHMARKING.md`](docs/BENCHMARKING.md) runs everything on another machine in
one command. The design targets Linux: [`docs/LINUX.md`](docs/LINUX.md).

## Quick start

```bash
cd rust
cargo test --workspace --release                 # all tests (~1 min)
cargo run --release -p ob-app -- live            # both venues, merged, printed every second; Ctrl-C to stop
```

Excerpt of real output (`ob live --binance-full-depth`, one second of a 60 s run):

```text
── t=  30s ──────────────────────────────────────────────────────────────
binance  Live           bid   60356.8 × 5.106    ask   60356.9 × 4.116    age    93 ms   51 upd/s
bybit    Live           bid   60357.5 × 1.831    ask   60357.6 × 1.021    age    11 ms   56 upd/s
best bid 60357.5 × 1.831 (bybit)   best ask 60356.9 × 4.116 (binance)   ← the venues cross (fees exceed the gap)
buy   1 BTC: avg 60356.9, incl. fees 60387.1  via binance 1
sell  1 BTC: avg 60356.8, incl. fees 60326.6  via binance 1
buy  25 BTC: avg 60362.4, incl. fees 60393.2  via binance 19.91 + bybit 5.09
sell 25 BTC: avg 60354.4, incl. fees 60323.2  via binance 16.294 + bybit 8.706
arbitrage net of fees: none
```

More modes:

```bash
cargo run --release -p ob-app -- live --latency --record session.obrec   # per-stage latency histograms; record
cargo run --release -p ob-app -- replay session.obrec                    # deterministic replay, ~1M msgs/s
cargo run --release -p ob-app -- live --binance-full-depth               # 1,024-level Binance book
cargo run --release -p ob-app -- live --pin 2,3 --rt --busy-poll 50      # OS tuning (docs/LINUX.md)
scripts/run-benchmarks.sh                                                 # every benchmark (docs/BENCHMARKING.md)
```

## The assignment, requirement by requirement

| Requirement | Where | Tests |
|---|---|---|
| **OrderBook**: add/remove levels from WebSocket messages | `ob-core` [`OrderBook::apply`/`replace`](rust/crates/ob-core/src/book.rs); venue semantics (snapshot vs delta) in [`ob-parse`](rust/crates/ob-parse/src) | `ob-core` unit tests, property tests against a `BTreeMap` model |
| Sorted levels, best bid/ask | `bids()` / `asks()` best-first, `best_bid()` / `best_ask()` | `bids_sort_high_to_low_and_asks_low_to_high` |
| Market impact | [`OrderBook::market_impact`](rust/crates/ob-core/src/book.rs): fill, VWAP, slippage, worst price | `market_impact_walks_levels_best_first` |
| `isCrossed` | `OrderBook::is_crossed` | `crossed_and_locked_books_are_detected` |
| **Aggregator**: combine venues | [`ob-agg`](rust/crates/ob-agg/src) over seqlock-published books | `tests/aggregator.rs` |
| Best N levels across all venues | [`Aggregator::best_levels`](rust/crates/ob-agg/src/aggregator.rs) (merged, venue-tagged) | `best_levels_merge_venues_best_first` |
| Optimal execution path | [`optimal_execution`](rust/crates/ob-agg/src/execution.rs): fee-aware greedy, proven optimal | `tests/optimality.rs` (brute force), `tests/real_execution.rs` |
| Handle disconnections gracefully | [`ob-feed`](rust/crates/ob-feed/src/feed.rs): backoff with jitter, heartbeats, staleness, resync; the venue is excluded at once | `tests/feed.rs` (fake clock), `tests/mock_exchange.rs` (real sockets) |
| Tests: zero-quantity removal | | `zero_quantity_removes_the_level` |
| Tests: repeated updates at one price | | `repeated_updates_at_one_price_keep_the_last_quantity` |
| Tests: crossing | | `crossed_and_locked_books_are_detected`; aggregator crossed-venue exclusion |
| Tests: race conditions | seqlock, SPSC queue | `ob-sync/tests/loom.rs` (model checker), `stress.rs`, `spsc_stress.rs`; `ob-agg/tests/concurrent.rs`; Miri |
| `CONSIDERATIONS.md` | [CONSIDERATIONS.md](CONSIDERATIONS.md) | |

The skeleton's bugs that this fixes:
- Binance `depth20` messages are **snapshots**, not changes; applying them as changes
  leaves stale levels.
- Bybit's `type` (snapshot or delta) and `u` sequencing were ignored.
- There was no reconnect, heartbeat or staleness detection.
- Per-level updates let readers see half-applied messages.

### Java → Rust API map

| Java skeleton | Rust |
|---|---|
| `OrderBook.handleOrder(PriceLevel)` | `OrderBook::apply(Side, Level)`; whole messages via `replace` / `apply_to` |
| `getBestBidPrice/Size`, `getBestAskPrice/Size` | `best_bid()` / `best_ask() -> Option<Level>` |
| `isCrossed()` | `is_crossed()` |
| `OrderBookAggregator.getBestBid/getBestAsk` | `Aggregator::best_bids` / `best_asks` (one quote per healthy venue) |
| (best N levels) | `Aggregator::best_levels(side, now, &mut out)` |
| `getBidVolumeForPrice` / `getAskVolumeForPrice` | `bid_volume_at_or_above` / `ask_volume_at_or_below` (per venue, with a completeness flag) |
| `getAskPriceForVolume` / `getBidPriceForVolume` | `ask_price_for_volume` / `bid_price_for_volume` |
| `calculateOptimalExecution(Side, qty)` | `optimal_execution(Action::Buy / Sell, qty, now)` (the side is made explicit: buy takes asks) |
| `BigDecimal` | `Px` / `Qty` fixed-point integers per instrument; `Notional` (`i128`) |

## Why Rust

- **The same machine code as C++** (LLVM), the same control over memory layout, and no
  garbage collector, JIT warm-up or safepoints. Java *can* be made fast, by avoiding
  allocation and garbage-collector pauses everywhere, but only by working against the
  language; its tail latency stays worse.
- **The compiler rules out data races** in safe code. `unsafe` exists in exactly three
  places, each `unsafe` block carrying a `SAFETY:` comment (lint-enforced): `ob-os`
  (system calls), `ob-simd` (16-byte SIMD loads for the parser, step 25) and a
  test-only counting allocator. Every other crate is `#![forbid(unsafe_code)]`.
- **The design ports directly to C++:** structure-of-arrays books, a seqlock, fixed-point
  integers, sans-I/O state machines.

## Architecture

```text
 per venue, one thread (spinning)                                     any reader thread
┌────────────────────────────────────────────────────────────────┐
│ ob-net   TCP → rustls handshake, then our TLS record layer     │
│          (AES-GCM in place) → zero-copy WebSocket codec        │
│ ob-parse fixed-schema JSON scanner + fused decimal parse;      │
│          a whole price level per SIMD call (ob-simd)           │
│ ob-core  OrderBook: sorted arrays (20–50 levels);              │
│          TickBook: price-indexed (1,000 levels, full depth)    │
│ ob-feed  sans-I/O state machine: subscribe, sequence, resync,  │
│          heartbeat, backoff; top-of-book overlay; full depth   │
│ ob-sync  seqlock publish (levels + health, one write) ─────────┼──▶ ob-agg Aggregator
│          SPSC binary log queue ────────────────────────────────┼──▶ logger thread
└────────────────────────────────────────────────────────────────┘
 ob-app: the `ob` binary (live / record / replay)   ob-os: OS tuning (pinning, priority, sockets)
```

## Documentation

| Document | What |
|---|---|
| [CONSIDERATIONS.md](CONSIDERATIONS.md) | Live multi-venue pitfalls (required by the assignment), with evidence |
| [docs/LATENCY.md](docs/LATENCY.md) | Every measured number, methodology, and the measurement mistakes found |
| [docs/LINUX.md](docs/LINUX.md) | **The target platform:** what's in the code for Linux, what to add or change, the expected impact of each, a day-one checklist |
| [docs/BENCHMARKING.md](docs/BENCHMARKING.md) | Running every benchmark on another machine (written for a person or an AI agent), checking the run, writing up results |
| [docs/TAIL-LATENCY-PLAN.md](docs/TAIL-LATENCY-PLAN.md) | Steps 23–29: the p99/p99.9 work, and the review of six reference repositories |
| [docs/TUNING.md](docs/TUNING.md) | Colocation, and the macOS test-bench results |
| [docs/steps/](docs/steps/) | The design log: 28 steps (27 dropped), each with what changed, why, the alternatives, measurements, the Java counterpart, and how to verify it |

Step notes:
- **Foundations:** [01 workspace](docs/steps/01-workspace.md) ·
  [02 fixed point](docs/steps/02-fixed-point.md) ·
  [03 order book](docs/steps/03-order-book.md) ·
  [04 property tests](docs/steps/04-property-tests-and-benchmarks.md)
- **Parsing:** [05 decimal parser](docs/steps/05-fast-decimal-parser.md) ·
  [06 Binance parser](docs/steps/06-binance-parser.md) ·
  [07 Bybit parser](docs/steps/07-bybit-parser.md)
- **Sharing and aggregation:** [08 seqlock](docs/steps/08-seqlock.md) ·
  [09 aggregator](docs/steps/09-aggregator.md) ·
  [10 execution and arbitrage](docs/steps/10-execution-and-arbitrage.md)
- **Network and feeds:** [11 WebSocket codec](docs/steps/11-websocket-codec.md) ·
  [12 TLS and socket loop](docs/steps/12-tls-and-socket-loop.md) ·
  [13 feed state machines](docs/steps/13-feed-state-machines.md) ·
  [14 mock exchange](docs/steps/14-mock-exchange.md)
- **Running and measuring:** [15 app and binary logger](docs/steps/15-app-and-binary-logger.md) ·
  [16 record/replay and latency](docs/steps/16-record-replay-and-latency.md) ·
  [17 zero-allocation proof](docs/steps/17-zero-allocation-proof.md)
- **Market data quality:** [18 top-of-book overlay](docs/steps/18-top-of-book-overlay.md) ·
  [19 Binance full depth](docs/steps/19-binance-full-depth.md)
- **Tuning and wrap-up:** [20 PGO and micro-tuning](docs/steps/20-pgo-and-micro-tuning.md) ·
  [21 OS tuning](docs/steps/21-os-tuning.md) ·
  [22 final benchmarks and docs](docs/steps/22-final-benchmarks-and-docs.md)
- **Tail latency, Linux first:** [23 tail harness](docs/steps/23-tail-harness.md) ·
  [24 TLS record layer](docs/steps/24-tls-record-layer.md) ·
  [25 SIMD level parser](docs/steps/25-simd-levels.md) ·
  [26 price-indexed book](docs/steps/26-tick-book.md) ·
  [28 Linux document](docs/steps/28-linux.md) ·
  [29 benchmarking elsewhere](docs/steps/29-benchmarking-elsewhere.md)

## How it's verified

- **Unit and property tests** against simple models (a `BTreeMap` book, brute-force
  execution).
- **Differential testing** of every parser against an independent serde_json
  implementation of the same rules: recorded live frames, mutated frames, generated
  messages. Plus `cargo-fuzz` targets.
- **Concurrency:** loom (every interleaving and weak-memory outcome of the seqlock and
  the queue), stress tests, Miri.
- **The feed state machine** with a fake clock (deterministic timing), plus a **mock
  exchange** over real TLS sockets: drops, silence, garbage, refused connections, 429s.
- **Live cross-checks:** the full-depth book against Binance's own `depth20` (9,291
  levels, 0 persistent mismatches); the overlay against recorded live streams.
- **Proofs about the code:** zero allocations per message (a counting allocator); the
  assembly of the hot loop (no bounds checks).
- **Mutation testing throughout:** deliberately breaking a rule must fail a test. It
  found several weak tests, each fixed and recorded in the step notes.
- **CI** (`.github/workflows/ci.yml`): fmt, clippy (Linux and macOS), tests in debug and
  release on both, benches compile, Miri, loom, fuzz smoke runs, docs. Linux builds use
  `x86-64-v2`, so CI tests the SIMD parser there and the scalar fallback on macOS
  (Apple Silicon runners).
- **The TLS record layer** against a real rustls server: every cipher suite on TLS 1.2
  and 1.3, every split point, key updates, session tickets, tampering at every byte.

## Limitations, stated plainly

- **Measured on a laptop over the public internet.** The network (234 ms round trip to
  Binance) dwarfs everything; real deployment means colocation (`docs/TUNING.md`).
- **Linux is the target, but nothing has run on a Linux host under load.** The Linux
  features are compile- and lint-checked, and CI's Ubuntu jobs build and test them.
  [`docs/LINUX.md`](docs/LINUX.md) lists what's left to do there, with the expected
  impact of each (labelled estimates).
- **p99.9 on macOS is set by the OS** (thousands of interruptions per second; measured by
  `tails --only jitter`), not by the code. An isolated Linux core is the fix.
- **Apple Silicon runs the scalar parser:** the SIMD level kernel is x86-only (results
  identical, parsing 11–37% slower there).
- **Visible depth is not executable depth:** execution plans are optimistic bounds
  (`CONSIDERATIONS.md` §5).
- **A Java JMH baseline** of the naive design (`TreeMap` + `BigDecimal`) wasn't built; the
  comparisons here are against serde_json, `BTreeMap`, `Mutex` and `std` channels in Rust.
