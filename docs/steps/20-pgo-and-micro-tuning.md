# Step 20: PGO, assembly review and micro-tuning (only where measured)

The plan's rule for this step was "change only what a measurement says is worth it". The
outcome, honestly:

| Tried | Result | Kept? |
|---|---|---|
| **PGO** (profile-guided optimisation), trained and used on the same binary | **−10% to −16%** per message replayed (unseen data) | **Yes**, as `scripts/pgo.sh` (opt-in) |
| PGO trained on one binary, used on another | **+11% slower** | No; this is the step's main lesson |
| Rewriting the hottest loop (decimal parsing) as two tighter loops | **~7% slower** | No (reverted) |
| `-C target-cpu=native` | −4% to +4%, within noise | No: not worth giving up portability |
| Assembly review of the hot functions | Already tight: no bounds checks in hot loops | Nothing to change |

The step also added a **benchmark of the whole per-message pipeline** on recorded live
data, which every number here is measured with, and a **measurement method** that
survives a noisy laptop (section 2).

## What changed

| File | What |
|---|---|
| `crates/ob-feed/benches/pipeline.rs` | **New.** Parse → apply → publish through the real `Feed`, on 5 recorded streams. |
| `crates/ob-parse/tests/fixtures/binance_full_{snapshot.json,diffs.jsonl}` | **New.** A real snapshot plus the 100 diff events after it (from step 19's recording), for the full-depth benchmark. |
| `scripts/pgo.sh` | **New.** Instrumented build → train (live sessions + replays) → optimised `ob`. |
| `crates/ob-parse/src/json.rs` | Unchanged in the end (the rewrite was measured and reverted). |

## 1. Baseline and profile

### The pipeline benchmark

Each batch starts from a fresh, connected feed; set-up, including loading the snapshot
for full depth, is excluded from the timing. Then every recorded message goes through
`Feed::on_text` once, in order, so delta streams stay valid.

| Stream | Per message |
|---|---|
| Binance `depth20` | 1.58 µs |
| Bybit `orderbook.50` | 0.62 µs |
| Binance `depth20` + `bookTicker` (step 18) | 0.36 µs |
| Bybit 50 + 1 (step 18) | 0.93 µs |
| Binance full depth (step 19; median 51 levels per event) | 5.79 µs |

### Where the time goes: macOS `sample`, no installs needed

I ran the benchmark binary in `--profile-time` mode and pointed the built-in sampling
profiler at it (`sample <pid> 4`). Self time per function:

| | `depth20` | Full depth |
|---|---|---|
| `Cursor::decimal` (parsing `"60236.90"`) | **45%** | **37%** |
| `levels` (brackets, commas, tick/lot checks, push) | 24% | 16% |
| Other parsing (keys, integers, dispatch) | ~16% | ~3% |
| Applying to the book (`apply_known` + `memmove`) | ~3% | **31%** |
| Publishing (seqlock) | 2% | 6% |

So parsing decimals dominates: 80 per `depth20` message, about 9 ns each. Full depth adds
book application, as step 19 predicted: deep inserts and deletes shift array memory.

## 2. Measuring on a noisy machine

After my first change, the parser benchmarks reported **+48%**. But benchmarks the
change *couldn't* affect moved too: the reference decimal parser +15%, the `f64` baseline
+51%.

The cause was the machine: a **load average of 21** (my own parallel builds and tests,
plus background applications) on a 4-core laptop. Criterion's run-to-run comparison
assumes the conditions stayed the same, and they hadn't.

The method used from then on:
- **Build both variants as separate binaries** (copy them aside), then **alternate**
  them: A, B, A, B, A, B. Both variants see the same machine state each round.
- **Compare means and bests over rounds,** not a single run against a saved baseline.
- **Look at a benchmark that shouldn't change.** If it moved, the measurement is
  invalid.

Absolute numbers in sections 3–5 are ~25% above section 1's, because the machine stayed
busy. The comparisons are valid because they're interleaved.

## 3. The decimal loop: a rewrite that lost

The hot loop examines each byte for three things: digit? first dot? closing quote? The
textbook fix is two loops (integer digits, then fraction digits), each with one
predictable test per byte. I implemented it with identical acceptance rules; all 67
parser tests passed, including the differential ones. Interleaved, 3 rounds:

| 59 Binance messages | Old | Two loops |
|---|---|---|
| Round 1 / 2 / 3 | 105.1 / 108.4 / 102.3 µs | 116.4 / 114.0 / 108.5 µs |

About **7% slower**, and no better on Bybit, so it's reverted. Section 5 shows why: the
compiler had already made the original loop tight, and the second loop only added
set-up and an extra branch.

## 4. PGO: −10 to −16%, if you do it right

**What it is.** Build an instrumented binary that counts how often every branch and
function runs; run it on a realistic workload; rebuild with those counts. LLVM then lays
out hot code contiguously, inlines along hot paths, orders branches by real frequency, and
moves cold code (errors, resyncs) out of the way.

**No extra tools needed.** rustc's own `-Cprofile-generate` / `-Cprofile-use` flags,
plus `llvm-profdata` from the official `llvm-tools` component (installed with your
approval; remove with `rustup component remove llvm-tools`).

### Attempt 1: wrong, and slower

I trained `ob` (replaying ~106,000 recorded messages) and applied the profile to the
*benchmark* binary:

| | Normal | PGO |
|---|---|---|
| Binance `depth20` (best of 3) | 112.2 µs | **126.5 µs** (+13%) |
| Full depth (best of 3) | 719.4 µs | 767.3 µs (+7%) |
| Bybit 50 | 47.2 µs | 47.8 µs (no change) |

**Why it lost:** with fat LTO and one codegen unit, functions are inlined and specialised
per binary. A profile from one binary only partly matches another's code, and a partial
profile can tell the optimiser the wrong things are hot. **Train and use the profile on
the same binary.**

### Attempt 2: same binary, unseen data

I trained `ob` on the three earlier recordings, then evaluated `ob replay` on a **new**
30 s recording it had never seen (18,017 records), interleaved over 6 runs:

| `ob replay` (30 s of market data) | Mean | Best |
|---|---|---|
| Normal | 11.52 ms | 11.20 ms |
| PGO (replay training) | **9.62 ms (−16.5%)** | 9.50 ms |
| PGO (live + replay training, `scripts/pgo.sh`) | **10.85 ms (−10%)** | 9.80 ms |

**Why train on live data too:** code the training never runs is treated as **cold** and
optimised for size. Replay-only training never exercises TLS, the WebSocket codec or the
driver loop, so live they'd be compiled as cold paths. `scripts/pgo.sh` trains on two
live sessions (normal and full depth) plus replays. The profile is then spread over more
code, so the replay-only benchmark gains a little less (10% instead of 16%), but the
live path isn't pessimised. Live gains can't be measured reliably on this laptop (the
network jitter is milliseconds).

**Why it's opt-in:** a PGO build needs a training run against the live venues, and its
benefit depends on the workload being representative. `cargo build --release` stays
reproducible; `scripts/pgo.sh` is for production builds.

## 5. Assembly review

I emitted assembly for `ob-parse` (`cargo rustc --release -p ob-parse --lib -- --emit asm`)
and scanned the hot functions for calls to panic, bounds-check and slice-index failure
routines:

| Function | Instructions | Bounds or panic calls |
|---|---|---|
| `Cursor::decimal` (hottest) | 141 | 1, **outside** the loop |
| `BinanceDepthParser::levels` | 300 | 0 |
| `Cursor::u64` | 79 | 0 |
| `Cursor::skip_value` (unknown fields, rare) | 392 | 4 |

The decimal loop body as compiled:

```asm
LBB7_11:  movzbl (%rbx,%rcx), %r15d     ; load byte
          leal   -48(%r15), %r12d       ; b - '0'
          cmpb   $10, %r12b             ; is it a digit?
          jae    LBB7_12                ; no → '.', '"' or the slow path
          leaq   (%rdx,%rdx,4), %rdx    ; value * 5
          leaq   (%r15,%rdx,2), %rdx    ; value * 2 + digit  (= value*10 + digit, no mul)
          incq   %r14                   ; digit count
          ...
          incq   %rcx                   ; next byte
          cmpq   %rcx, %r11             ; end of buffer?  (the bounds check, hoisted into
          jne    LBB7_11                ;  the loop condition: no separate check per byte)
```

- **About 2 cycles per character, with no checks per byte.**
- **The one bounds check** is in `decimal::finish` (`POW10[decimals - frac]`): once per
  number, after the loop, perfectly predicted.
- **Dividing by 10** for trailing zeros (Binance sends `"60236.90"` for a 0.1 tick)
  compiles to a multiply and shift (`movabsq $-3689348814741910323` is the reciprocal
  of 10), as the step 5 comment claimed.

**Conclusion:** at the instruction level there's nothing left to take out. A real gain in
parsing needs a different *algorithm*: SIMD structural scanning, as simdjson does, which
finds all quotes, commas and brackets 32–64 bytes at a time. That's a project of its own,
and the `target-cpu=native` result (no gain) confirms that scalar code doesn't benefit
from newer instruction sets by itself.

## 6. What's left on the table (measured, not done)

| Opportunity | Evidence | Why not now |
|---|---|---|
| A price-indexed ladder for full depth | 31% of full-depth time is array shifting | A structural change, with its own tests; only matters for the opt-in mode at ~10 events/s |
| SIMD JSON scanning | Parsing is 85% of `depth20` time; the scalar loop is already ~2 cycles/char | Large; the gain would be ~2–4× on parse, at the cost of a lot of `unsafe` or a dependency |
| Cache warming between messages | Step 16: live handle time is 2–5× the warm time | Needs core isolation to be meaningful; Linux (step 21) |

## 7. Java counterpart

- **Java has a JIT, which is profile-guided compilation by default.** HotSpot's C2
  compiler profiles while it interprets, then compiles hot methods with that profile.
  That's PGO without a training step, but it costs a warm-up period on every start (the
  first thousands of messages are slow), and a **deoptimisation** when the profile turns
  out wrong (e.g. the first resync after hours of steady state) causes a pause.
- **Ahead-of-time PGO** exists for Java too: GraalVM Native Image's
  `--pgo-instrument` / `--pgo`, the same two-pass workflow as here.
- **Inspecting generated code:** `-XX:+UnlockDiagnosticVMOptions -XX:+PrintAssembly`
  (with hsdis) shows the JIT's machine code; JMH's `-prof perfasm` shows the hottest
  assembly with sample counts, like this step's `sample` plus `--emit asm`.
- **Noisy measurement** is worse in Java: JIT tiers, GC, safepoints. JMH's forks,
  warm-up iterations and `-prof` modes exist for exactly the problem section 2 hit.

## How to verify it yourself

```bash
cd rust
cargo bench -p ob-feed --bench pipeline                      # the baseline table
scripts/pgo.sh 30 <recordings…>                              # PGO build → target/pgo/release/ob
# A/B on a recording neither binary was trained on, several times each, alternating:
for i in 1 2 3; do target/release/ob replay new.obrec | head -1; target/pgo/release/ob replay new.obrec | head -1; done
cargo rustc --release -p ob-parse --lib -- --emit asm         # then search for Cursor7decimal
```

Things to try:
- Run the pipeline benchmark while something else loads the machine (e.g. a parallel
  `cargo build`), then compare against an idle run. A single before/after comparison
  under load can mislead by tens of percent; interleaving doesn't.
- Use the PGO profile from `ob` to build the *benchmark* binary
  (`RUSTFLAGS=-Cprofile-use=…/merged.profdata cargo bench -p ob-feed --bench pipeline`):
  it comes out slower, as in attempt 1.

## Likely interview questions

1. **Where does your per-message time go?**
   Measured with a sampling profiler on recorded data: decimal parsing 45%, level
   parsing 24%, other parsing ~16%, the book 3%, publishing 2% for Binance `depth20`
   (1.58 µs a message). Full depth adds ~31% for array shifting.
2. **Did PGO help?**
   10–16% per message, but only when the profile is collected from the same binary it's
   applied to. Applied to a different binary (with fat LTO) it made code 11% *slower*.
   I also train on live sessions, because code the training never runs is compiled as
   cold.
3. **How do you benchmark on a noisy machine?**
   Build both variants, alternate them several rounds, and compare means and bests. And
   check a control benchmark that shouldn't move: here it moved 51%, which is what
   exposed the problem.
4. **You rewrote the hottest loop and it got slower. Why?**
   The compiler had already turned the single loop into ~2 cycles per character, with
   the bounds check folded into the loop condition and multiply-by-10 done as two
   `lea`s. The two-loop version added an extra branch and set-up. That's why changes
   have to be measured, not assumed.
5. **What would you do next for parsing speed?**
   Change the algorithm, not the loop: SIMD structural scanning (simdjson-style) to find
   delimiters many bytes at a time. Compiler flags alone (`target-cpu=native`) gave
   nothing measurable, as expected for scalar code.
6. **How does this compare with Java's JIT?**
   The JIT does PGO automatically at run time, but pays a warm-up on every start and
   deoptimisation pauses when the profile changes. Ahead-of-time PGO (here, or GraalVM's)
   gives the optimised code from the first message, at the cost of a training step and
   the risk of an unrepresentative profile.
