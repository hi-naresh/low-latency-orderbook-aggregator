# Step 25: Large messages: parse vs apply, then a SIMD level parser

Step 23 found that message size sets Bybit's and full depth's p99: about 2.9 ns per byte
warm, so a 25 KB full-depth diff took ~70 µs. This step measured **where** that time
goes, then made the part that's parsing faster with a SIMD kernel. Two other ideas were
measured and dropped.

## What changed

| File | What |
|---|---|
| `crates/ob-simd/` | **New crate:** one function, `level(&[u8; 32]) -> Option<LevelDigits>`, which parses a whole `["price","qty"]` level with SSE (SSSE3 + SSE4.1). `no_std`; the only `unsafe` is the 16-byte loads. 3 tests (two property tests, 40,000 cases each). |
| `crates/ob-parse/src/json.rs` | `Cursor::level_fast`: try the kernel, scale with the same `decimal::finish` as the scalar path; on `None`, nothing is consumed. New integer edge-case tests and a property test against `str::parse::<u64>`. |
| `crates/ob-parse/src/{binance,bybit}.rs` | The level loops try `level_fast` first, then the unchanged scalar code. |
| `crates/ob-bench/src/anatomy.rs` | **New scenario** (`tails --only anatomy`): per recorded message, the parser alone vs the whole handler call (medians). |
| `rust/.cargo/config.toml` | **New:** Linux x86-64 builds target `x86-64-v2`, so the kernel is used there too (see §4). |
| `ob-core`, `ob-parse`, `ob-agg`, `ob-app` | Now `#![forbid(unsafe_code)]`. The README already claimed this, but these four crates didn't have it. |

## 1. Where a large message's time goes

New scenario: for each recorded message, the parser alone (a pure function, so the same
bytes give the same work and it can be repeated) against the whole handler call on a
live feed. Medians over 2,000 passes; the slowest messages before this step:

| Message | Bytes | Levels | Whole | Parse | Apply + publish |
|---|---|---|---|---|---|
| Bybit line 32 | 2,274 | 109 | 5.75 µs | 3.40 µs | 2.36 µs |
| Bybit line 122 | 1,992 | 95 | 5.20 µs | 3.00 µs | 2.20 µs |
| Full depth line 86 | 24,875 | 1,177 | 65.1 µs | 35.9 µs | 29.2 µs |
| Full depth line 99 | 15,622 | 736 | 40.9 µs | 22.6 µs | 18.3 µs |

**Parsing is ~60% of handling time** for both streams, at a steady ~1.45 ns per byte
(~30 ns per level). Applying is the other ~40%: ~22–25 ns per level, most of it the deep
inserts into the 1,000-level array, which is step 26's target (the price-indexed ladder).

**Skipping work doesn't help much here:** I counted how many full-depth levels fall
inside the snapshot's known price range (outside it, step 19 ignores them). 68% are
inside, so skipping the rest would save little.

## 2. Why the scalar parser costs ~30 ns per level

Step 5 measured it: a price or quantity is 5–9 characters, and the loop that reads it
ends at a place that depends on the number's length. **The branch predictor misses about
once per number** (~5–7 ns each). Step 5 also found that SWAR (8 bytes in a 64-bit word)
*lost* on such short numbers. Its fixed cost per number was higher than the scalar loop.

What neither approach did was handle **a whole level at once, with no branch on any
digit.** Real 16-byte SIMD makes that cheap:

```text
["60200.50","1.234"]   ← 32-byte window, starting at '['

1. two 16-byte loads; compare all 32 bytes with '"', '.', and the digit range at once
   → three 32-bit masks (one bit per byte)
2. the four quotes from the quote mask (count trailing zeros, clear the lowest bit)
   → where each number starts and ends; '[', '","' and '"]' must be exactly there
3. each number: only digits and at most one dot, not first or last; at most 8 digits
4. one byte shuffle (pshufb) per number gathers its digits, skipping the dot,
   right-aligned: price in lanes 0–7, quantity in lanes 8–15
5. three multiply-adds (pmaddubsw, pmaddwd, packusdw + pmaddwd) convert all 16 digits
   into two integers at once
```

Every branch depends on the message's **shape**, which is the same for every level, so
they are predicted. None depends on a **digit**. Anything that doesn't fit exactly
(whitespace, escapes, a sign, more than 8 digits, fewer than 32 bytes left) returns
`None`. The caller then parses that level with the unchanged scalar code, so results and
error messages are identical by construction.

**Platform and safety:**
- **SSSE3 (the shuffle) and SSE4.1:** on by default for macOS x86-64 (target CPU
  "penryn") and on every x86 CPU since 2008. So no run-time detection is needed. Where
  they aren't enabled at compile time, `level` always returns `None`.
- **No ARM NEON version yet:** it would be untested on this machine.
- **Safe portable SIMD (`std::simd`) is still unstable,** so this uses `core::arch`.
  Its intrinsics are safe to call only inside `#[target_feature]` functions. The crate
  has two `unsafe` blocks: the one call into those functions (justified by the
  compile-time `cfg`), and the 16-byte loads (inside the 32-byte array they're given).
  `ob-parse` itself stays `#![forbid(unsafe_code)]`.

## 3. Results

### The kernel's own correctness
- **Property tests:** the kernel must equal a plain reference implementation on 40,000
  realistic-or-hostile levels with random trailing bytes, and on 40,000 arbitrary 32-byte
  inputs.
- **Hand-picked cases:** whitespace, `"1."`, `".5"`, two dots, 9 digits, a sign, an
  escape, an empty number.
- **Mutation testing,** five planted bugs, all caught:
  - accept 9 digits;
  - allow a trailing dot;
  - skip the `,` check;
  - skip the dot one lane off;
  - count fractional digits off by one.

  One integration mutant, ignoring the price's decimal places in `level_fast`, fails 4
  parser tests, including the serde_json differential suites.

### Speed: interleaved A/B, same build, kernel on vs forced off

The parse alone, best of 7, two rounds each:

| Stream (parse per message) | Scalar | With the kernel |
|---|---|---|
| Binance full depth, line 86 (1,177 levels) | 30.2–31.5 ns per level | **18.9–19.7 ns per level (−37%)** |
| Binance full-depth diffs (100 messages) | 3,026–3,051 ns | **2,368–2,408 ns (−21%)** |
| Binance depth20 | 1,291–1,318 ns | **1,093–1,141 ns (−14%)** |
| Bybit orderbook.50 | 377–379 ns | **336–337 ns (−11%)** |
| Binance depth20 + bookTicker | 246–248 ns | 247–251 ns (tickers have no level arrays: unchanged) |

The kernel itself takes ~15–16 ns of the ~19 ns per level. The assembly is about 120
instructions on the success path. The remaining cost is a dependent chain: the
trailing-zero counts that find the quotes, then the multiply-add conversion (about 16
cycles of latency). Shaving that would be micro-tuning.

### The tail harness run: too loaded to compare

Reference file: `rust/bench-results/tails-MacBookPro15,2_-20261126-140506.txt`. Other
work drove the load average to **10.8 at the start, peaking near 14**, on 4 cores. In
this run even the same measurement drifted 15–30%: Bybit line 32's parse read 3.32 µs,
against 2.87 µs in a quieter run of the same build. So this file doesn't support
before/after conclusions. The evidence for this step is the interleaved A/B above,
where load affects both variants equally. The second Mac's run should give the clean
picture.

What the run does show, regardless of load: **apply + publish is now about half of the
time on the large messages** (line 86: parse 29.8 µs, rest 30.1 µs), up from 40%. That
makes the price-indexed ladder (step 26) the next lever for full depth.

### Tried and reverted: eight digits at a time for integers

Binance messages carry five 13–14-digit integers (update IDs, timestamps). Step 5 noted
that SWAR wins on long digit runs, so the integer parser got an 8-digit SWAR fast path.
The interleaved A/B: tickers **262 vs 250–255 ns** (slightly *worse*), depth20 and full
depth within noise, Bybit slightly better. **Reverted.** The digit counts are the same in
every message, so the branch predictor learns the scalar loop's exit, and SWAR's fixed
cost buys nothing. The lesson: the kernel won because price and quantity lengths vary;
where lengths are constant, scalar is already predicted. The new integer tests stayed
(they check behaviour, whichever implementation).

## 4. Linux: the kernel needs to be enabled at build time

The kernel is selected at **compile time**. macOS targets enable SSSE3 and SSE4.1;
Linux's default x86-64 target is the 2003 baseline (SSE2 only), which would silently use
the scalar path. `rust/.cargo/config.toml` now builds Linux x86-64 for `x86-64-v2`. That
covers every x86 server CPU since ~2009, and is what production would build for anyway
(often `x86-64-v3` or `native`). An `RUSTFLAGS` environment variable replaces it.

**Expectation on Linux** (not measured here): the same −11% to −37% on parsing. The
kernel uses no OS facilities.

## How to verify it yourself

```bash
cd rust
cargo test -p ob-simd --release                    # kernel vs model, 80,000 cases
cargo test -p ob-parse --release                   # 68 tests incl. serde_json differentials
target/release/tails --rt --only anatomy           # parse vs apply, slowest messages
cargo bench -p ob-parse --bench binance            # parser benchmarks
```

Things to try:
- In `ob-simd/src/lib.rs`, make `pub fn level` return `None` and rerun
  `tails --only anatomy`: full depth's parse goes back to ~36 µs for line 86.
- Change `(len <= 8)` to `(len <= 9)` in `shape`: `kernel_matches_model` fails
  (9 digits overflow the 8-lane conversion).
- Drop `|| w[q1 + 1] != b','`: the property test finds a level with the wrong separator.

## Java counterpart

The JDK Vector API (`jdk.incubator.vector`, still incubating after many releases) can
express the same idea: `ByteVector.fromArray`, `compare(EQ, '"')` → `VectorMask` →
`toLong()`, and `rearrange` for the shuffle. Widely used Java JSON parsers (Jackson,
for example) remain scalar, and hand-written Java hot paths typically use SWAR over
`long` via `VarHandle`. The Vector API's speed depends on the JIT recognising each
operation as an intrinsic. The structural idea (one
level at a time, branches only on shape) carries over unchanged.

## Likely interview questions

1. **Why did SIMD win here when SWAR lost in step 5?** Different unit of work. Step 5
   parsed one short number at a time, and SWAR's fixed setup per number outweighed a
   5–9-byte loop. Here a whole level (two numbers and their framing) is one
   branch-free operation, and real SIMD has the byte shuffle and multiply-add that SWAR
   lacks. What's removed is the per-number branch mispredict, which was most of the
   scalar cost.
2. **How do you know the fast path gives exactly the same results?** By construction
   and by test. It only accepts a strict subset of inputs, scales with the same
   function as the scalar path, and returns `None` (consuming nothing) otherwise, so the
   scalar path handles everything else, including every error. Property tests compare
   it with a reference model, and the existing serde_json differential tests run
   through it.
3. **What about Apple Silicon / ARM?** NEON has equivalents (`tbl` for the shuffle;
   multiply-add via widening multiplies); the missing piece is a movemask, emulated with
   a narrowing shift. It isn't written because I can't test it on this machine. Shipping
   untested `unsafe` code is the one thing not to do here. On ARM today, `level` returns
   `None` and the scalar path runs.
4. **Why not runtime CPU detection?** Every x86-64 Mac and every server since ~2009
   has SSSE3 and SSE4.1, so a compile-time choice costs nothing and keeps the code
   simple. Runtime dispatch would matter for AVX2 or AVX-512 variants (32- or 64-byte
   windows), which this message format doesn't need: a level fits in 32 bytes.
5. **What did you try that didn't work?** An 8-digit SWAR path for the long integers.
   It measured neutral to slightly worse and was reverted, because constant-length
   numbers are already predicted.
