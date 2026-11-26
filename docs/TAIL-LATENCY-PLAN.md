# Tail latency plan: p99 and p99.9 from network to published book

**Goal:** single-digit µs at **p99 and p99.9** for each market-data message, measured
from the moment its bytes are available to our process to the moment the updated book
is published. Aggregator queries should be faster still. p50 is reported alongside but
is not the target.

**Scope:**
- TLS decryption → WebSocket decoding → parsing → applying to the book → publishing;
  plus aggregator queries.
- Out of scope: colocation, order entry, tick-to-trade.

**Rule for every change:** no tricks that only flatter a benchmark or only work on this
Mac. Each change must be one we'd make for the best production code anyway, and must be
proven by the tail harness (step 23), with p50, p99, p99.9 and max shown before and after.

## 1. Where we stand

Live, 60 s, laptop, step 18:

| Stage | Binance p50 / p99 / p99.9 | Bybit p50 / p99 / p99.9 |
|---|---|---|
| read (socket + TLS + WS) | 84 ns / 84.9 µs / 1.28 ms | 35.3 µs / 987 µs / 12.3 ms |
| handle (parse + apply) | 372 ns / 4.7 µs / 24.5 µs | 2.57 µs / 21.5 µs / 328 µs |
| publish | 105 ns / 658 ns / 9.0 µs | 403 ns / 3.3 µs / 14.6 µs |

**What these numbers can't tell us:**
- A 60 s live run gives a few thousand samples per venue: enough for a usable p99, but
  too few for p99.9.
- The criterion benchmarks report means, not tails.
- So we can't yet separate our code's tail from macOS's.

**Four causes of tail latency** (explained in the conversation that led to this plan):
1. The OS interrupting our thread.
2. Cold caches between sparse messages.
3. Expensive messages: large deltas, multi-message TLS records, and full-depth deep
   inserts and 8 KB publishes.
4. Allocation in rustls, and cache lines shared between the writer and readers.

## 2. What the six reference repositories taught

All six were read: their READMEs and their core source files.
- **None of them implements the path we care about.** No TLS + WebSocket + exchange
  JSON → book → lock-free publish.
- **None publishes a credible tail-latency number for a feed handler.**
- Most of what is useful is a lesson in what *not* to do, especially when measuring.

| Repository | What it is | Useful to us | Not to copy |
|---|---|---|---|
| joaquinbejar/OrderBook-rs | Rust matching engine (skiplist, DashMap) | The most honest measurement write-up: HDR percentiles; interleaved A/B rounds with load average logged; a test that fails if allocations exceed a budget; and it states openly that its numbers are service time from a closed loop, not latency under load | Skiplist and heap allocation per order: their own add p99 is ~50× p50 (37 µs vs 750 ns); batched timing makes the p99.9 rows unreliable |
| berdvana/ultra-low-latency-orderbook | C++ top-5 L2 book (intrusive list + hash table + pool) | Fixed-capacity pool, no allocation after construction | Drops levels outside the top 5 (wrong for a diff feed); "single-digit ns" with no readable method; asserts disabled |
| Juswanth-T/Low-Latency-Engine | C++ simulated aggregator | Correct SPSC ordering, indices on separate cache lines | Takes a `shared_mutex` on every tick; percentiles averaged across runs (statistically wrong); closed-loop ping-pong (coordinated omission); integer-overflow undefined behaviour |
| crypto-zero/apex-engine | Rust matching engine (skiplist) | Division by a constant through a precomputed reciprocal (`ob-core/src/divisor.rs` already does this) | `unsafe impl Sync` with data races; benchmark runs against an empty book; `Instant::now().elapsed()` used as a timestamp |
| mrkingsleyobi/quantumedge | C++ demo with marketing claims | – | The parser is a stub returning hard-coded prices; "350 ns end to end" has no harness behind it; AVX-512 picked at compile time with no runtime check (crashes on CPUs without it); ABA-prone pool |
| omerhalid/Real-Time-Market-Data-Feed-Handler… | C++ demo (REST polling) | A checklist of Linux settings: `SO_INCOMING_CPU`, NUMA binding | `-ffast-math` in trading code; `std::cout` on the hot path; claims are targets, not measurements |

**What we adopt:**

1. **Measurement discipline:**
   - Compute percentiles from all samples pooled together, never average them across
     runs.
   - Measure **open loop:** each message has a scheduled arrival time, and its latency
     is counted from that time. This avoids coordinated omission, where a slow message
     delays the next one's start and so hides the delay.
   - Use ≥ 100,000 samples for p99.9 and ≥ 1,000,000 for p99.99.
   - Report warm and cold results separately.
   - Never time in batches when per-operation tails are the subject.
2. **Choose CPU features at run time** for any SIMD code (step 27), with a scalar
   fallback that has to give identical results.
3. **Linux checklist additions** for `docs/TUNING.md`: `SO_INCOMING_CPU`, binding
   memory to the core's NUMA node.

**Already done here, in better form:** fixed-point prices, an SPSC queue with cached
indices, a counting-allocator proof of zero allocations, seqlock publishing, and
reciprocal division.

## 3. The steps

One at a time, each with a step note, as before. Steps 23–28 change code and are
measured on this Mac and on the second Mac. Step 29 is documentation for Linux (now `docs/LINUX.md`; see the revised order below).

| # | Step | Fixes cause | Expected effect (estimate until measured) |
|---|---|---|---|
| 23 | **Tail harness.** The whole chain from *encrypted* bytes, using an in-memory TLS server, so decryption is included: WS → parse → apply → publish. rdtsc-timed HDR histogram per stage and end to end, open loop, pooled, ≥ 1M samples. Warm mode (back to back) and cold mode (realistic gaps plus cache pollution). Reader-side query tails while the writer runs. `scripts/bench-tails.sh` writes the machine details and results to one file, so the two Macs compare directly | (measurement) | Separates our tail from the OS's; gives the baseline every later step is judged by |
| 24 | **Keep the hot path warm while idle.** When no data is waiting, run the real parse → apply → publish code on a recorded message into a *shadow* book and shadow publish area. It never touches what readers see | 2 | Cold-mode p99 approaches warm-mode p99 (step 16 saw cold at 2–5× warm) |
| 25 | **Publish only what changed.** The seqlock write covers only the levels that moved; best bid/ask, counts and status share the first cache line | 3, 4 | Full-depth publish falls from rewriting 8 KB to the changed levels; less writer–reader contention in the p99.9 |
| 26 | **TLS without a per-record allocation.** Decrypt into a buffer we own and reuse. First check whether the current rustls version offers in-place decryption; if not, a pre-faulted buffer pool behind rustls's API. No forked TLS code | 4 | Removes the occasional µs-scale allocator stall from the read tail |
| 27 | **SIMD structural scanning parser** (simdjson-style: find quotes, commas and brackets 32 or 64 bytes at a time). CPU features chosen at run time, scalar fallback, differential tests against the scalar parser and serde_json | 3 | Binance depth20 parse from 1.36 µs to roughly 0.4–0.7 µs; shrinks the large-message tail most |
| 28 | **Price-indexed ladder for full depth.** No O(n) shifting on deep inserts; a bitmap to find the best level | 3 | Full depth from 5.63 µs to roughly 2–3 µs; flatter worst case |
| 29 | **Linux documentation** (`docs/TUNING.md`, extended). Each Linux-only change: what it does, why it matters for p99.9, the expected effect, and how to verify it on the day a Linux box exists. Covers core isolation, IRQ affinity, busy-poll and `io_uring`, `SO_INCOMING_CPU`, NUMA, huge pages, fixed CPU frequency and C-states, kernel TLS, and a jitter test | 1 | On tuned Linux, received bytes → published at about p99 2–4 µs and p99.9 < 10 µs (estimate) |
| 30 | **Final measurement** on both Macs; update `docs/LATENCY.md` and the README with p50, p99, p99.9 and max | – | – |

### Revised order after step 23's measurements

Step 23 found the read stage (`recv` + TLS + copy + WebSocket decode) to be the largest
in-process stage at both p50 (3.6 µs) and p99 (23 µs), and message size to set Bybit's
and full depth's p99 (see `docs/steps/23-tail-harness.md`, section 3). Each next step
starts by splitting its stage further before changing it.

| Next | Step | Why this order |
|---|---|---|
| 24 | **Read path:** split `recv` / decrypt / copy / decode, then remove the per-record TLS allocation and copy (was 26). **Done:** `recv` is ~85% of the stage (OS: step 28); our TLS record layer cut TLS from 0.46 to 0.18 µs at p50, and its p99.9 from 15–18 µs to under 0.6 µs; zero allocations | the largest stage at p50 and p99 |
| 25 | **Large messages:** split parse vs apply on the slowest recorded messages, then SIMD structural scanning (was 27). **Done:** parse was ~60% (~30 ns/level, branch mispredicts); a SIMD kernel parses a whole level branch-free: parse −11% to −37%. An 8-digit integer path measured neutral and was reverted | sets Bybit's and full depth's p99 |
| ~~26~~ | ~~Keep the hot path warm while idle~~ → **not built**: the design targets an isolated Linux core, where nothing cools the caches between messages (`docs/LINUX.md` §3.1) | – |
| 26 | Price-indexed ladder for full depth (was 27). **Done:** `TickBook` (array per tick + bitmap): applying the recorded diffs −58% (172 → 70 µs); the 25 KB message 60 → 37 µs, its apply + publish 30 → 9.7 µs | apply is now half of a large message's time |
| ~~27~~ | ~~Publish only what changed~~ → **dropped**: after step 26, publishing is ~0.4 µs of the largest message's ~37 µs, and under 0.1 µs for the small books; the gain no longer justifies a more complex publish path. (Numbers 28 and 29 are kept so earlier notes' references stay valid.) | – |
| 28 | Linux documentation (was 29). **Done:** `docs/LINUX.md` complete; a jitter test in the harness; the bench script runs on Linux; CI and `pgo.sh` no longer drop the SIMD parser on Linux | p99.9 needs interrupt-free cores (finding 4) |
| 29 | Final measurement on both Macs (was 30) | – |

**Design rule:** the project targets Linux; the Mac is the test bench. Linux-only
facilities are requested and their outcome reported; no macOS workarounds are built.
Everything Linux needs, and its expected impact, is in `docs/LINUX.md`.

**On macOS:** it can't isolate a core, so p99.9 there will still include some macOS
scheduling after steps 24–28. The harness reports this honestly: the warm and cold
modes show what our code does, and the live mode shows what the machine adds.
