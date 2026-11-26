# Step 2: Fixed-point prices, quantities and per-instrument scale

Design reference: Phase 1 of the plan ("Fixed point"). This step decides how every
number in the system is represented. Every later step (book, parsers, aggregator,
routing) is built on it.

**Revision 2.** Your review of the first version found three problems, all fixed here:
1. The single global 10^-8 scale wasn't reusable. It overflows for meme-coin quantities
   and can't hold 18-decimal tokens. **Now each instrument has its own scale.**
2. `Qty` arithmetic only checked overflow in debug builds, so a release build could
   produce a negative `Qty`. **Now it's checked in every build.**
3. The rejection policy wasn't defined. **Errors are now classified, and the policy is
   written down** (below).

Measuring the rework found a fourth: the new tick check made parsing 2× slower. That's
fixed with a division-free divisibility test (section 5).

## What changed

| File | Content |
|---|---|
| `rust/crates/ob-core/src/fixed.rs` | `Px`, `Qty`, `Notional`: raw integers, checked arithmetic, 6 tests |
| `rust/crates/ob-core/src/spec.rs` | `InstrumentSpec` (scale + tick + lot), parsing, display, errors, 17 tests + doc example |
| `rust/crates/ob-core/src/divisor.rs` | Division-free "is it a multiple?" test, 2 tests |
| `rust/crates/ob-core/src/lib.rs` | Exports |

## 1. The core idea: integers whose unit is defined by the instrument

```rust
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Debug)]
pub struct Px(i64);        // price: raw units of the instrument's price scale

pub struct Qty(i64);       // quantity: raw units of the quantity scale, never negative
pub struct Notional(i128); // price × quantity: decimals = price decimals + quantity decimals
```
`Qty` and `Notional` have the same attributes as `Px`.

What a raw unit means comes from the instrument:

```rust
pub struct InstrumentSpec {
    px_decimals: u8,   // BTCUSDT: 1  (tick 0.1)
    qty_decimals: u8,  // BTCUSDT: 3  (lot 0.001)
    tick: i64,         // smallest price step, raw (BTCUSDT: 1 = 0.1)
    lot: i64,          // smallest quantity step, raw (BTCUSDT: 1 = 0.001)
    tick_div: Divisor, // precomputed multiple-of-tick test (section 5)
    lot_div: Divisor,
}
```

For BTCUSDT:
```
spec = InstrumentSpec::from_steps("0.1", "0.001")   // from the venue's instrument info
"50000.1"  → spec.parse_px  → Px(500001)
"1.234"    → spec.parse_qty → Qty(1234)
Px(500001).notional(Qty(1234)) → Notional(617001234), 4 decimals → "61700.1234"
```

- **Newtypes** (`struct Px(i64)`): the compiler treats `Px`, `Qty` and `i64` as different
  types. Passing a quantity where a price belongs won't compile, and `Px + Px` doesn't
  exist (adding prices is meaningless). At run time each one is just the integer.
- **`#[repr(transparent)]`** guarantees the exact layout of the inner integer. Step 8
  relies on it when prices go into atomic slots.
- **`Copy`**: passed in a register like a plain integer; no heap object, no reference.
- **`Ord`**: comparing two prices is one CPU compare instruction.
- **Compile-time size checks** fail the build if a wrapper ever costs anything:
  ```rust
  const _: () = assert!(size_of::<Px>() == 8 && size_of::<Option<Px>>() == 16);
  ```

### Why the scale belongs to the instrument (and not one global scale)
The first version stored everything at 10^-8. It failed two ways:

| Instrument | Problem with a global 10^-8 scale | Per-instrument scale |
|---|---|---|
| Meme coin, quantities of 5 trillion units | 5×10¹² × 10⁸ = 5×10²⁰ > `i64::MAX` (9.2×10¹⁸): **overflow** | `qty_decimals = 0`, fits easily |
| Token with 18 decimals | `1.000000000000000001` **rejected** | `qty_decimals = 18`, exact |
| BTCUSDT | works | works, with more range |

Both failures are tests (`a_per_instrument_scale_fits_values_a_global_one_cannot`).

**How decimals are chosen.** `from_steps(tick, lot)` takes the steps as the venue
publishes them (Binance `exchangeInfo` `tickSize`/`stepSize`, Bybit `instruments-info`
`tickSize`/`qtyStep`). The decimals are the steps' own precision, ignoring trailing
zeros (`"0.10"` → 1). That's the **coarsest scale that represents every valid value
exactly**, so it leaves the most range.

**Several venues, one instrument.** The aggregator compares Binance and Bybit prices
directly, so both must use the same scale. Each venue's spec is moved to the finest
precision of any venue with `rescaled`. Each venue keeps its own tick, which is still
enforced:
```rust
let a = InstrumentSpec::from_steps("0.1",  "0.001")?;   // venue A
let b = InstrumentSpec::from_steps("0.01", "0.0001")?;  // venue B
let (a, b) = (a.rescaled(2, 4)?, b.rescaled(2, 4)?);    // shared scale: 2 / 4 decimals
// a.tick() == 10 (0.10), b.tick() == 1 (0.01): prices compare directly,
// and a.parse_px("50000.11") is still rejected as OffTick for venue A.
```
`rescaled` refuses to make a scale coarser (`CannotCoarsen`), since that could lose
precision.

**Range is checked up front, not discovered mid-session:**
```rust
btc.ensure_headroom("10000000", "1000000")?;   // must hold price 10M and size 1M, or refuse
```
Call it at start-up with generous limits (say 100× today's price). An instrument whose
scale is too small is refused before trading starts.

**Trade-off accepted:** prices of *different* instruments are both `Px`, so the compiler
can't stop you comparing them. Every book and aggregator handles exactly one instrument,
so they never meet. Encoding the instrument in the type (generic parameters everywhere)
would cost a lot of complexity for little benefit.

## 2. Why `Qty` is `i64`, not `u64`
The "never negative" rule is enforced by the **type**: the field is private, and every
constructor (`from_raw`, `parse_qty`) rejects negatives. With that in place, signed is
safer:
- **An unsigned bug produces a believable wrong number.** In a release build,
  `1u64 - 2u64` wraps to 18,446,744,073,709,551,615. That looks like a valid huge size
  and flows into routing. With `i64` a bug gives an impossible negative, easy to catch.
- **Differences are needed:** remaining = target − filled, level changes between updates.
  Mixing `u64` and `i64` means casts everywhere, which is what the step 1 lints flag.
- **Range isn't a reason:** `u64` only doubles the maximum. A too-small range is fixed by
  choosing the scale (section 1).
- **Speed is identical.** The C++ Core Guidelines (ES.106) give the same advice: don't
  use unsigned just to rule out negatives.

## 3. Arithmetic is checked in every build
Version 1 used plain `+` (checked only in debug builds). In release, `Qty + Qty` could
wrap to a negative quantity, breaking the type's own promise. Now:
```rust
impl Add for Qty {
    fn add(self, rhs: Qty) -> Qty {
        match self.checked_add(rhs) {
            Some(q) => q,
            None => arithmetic_bug("Qty overflow"),
        }
    }
}

#[cold]            // "this is rare": the compiler moves it out of the hot path
#[inline(never)]   // keep the panic machinery out of every caller
#[track_caller]    // the panic message points at the caller's line
fn arithmetic_bug(what: &'static str) -> ! { panic!("{what}") }
```
- `-` panics instead of going below zero. `checked_add`/`checked_sub` return `Option`
  for callers that must handle it.
- `Notional` addition is checked the same way.
- Tests prove the checks run **in release too** (`cargo test --release` passes the three
  `should_panic` tests).

**Rule used everywhere: bad data from outside → error value (`Result`); a broken internal
invariant (a bug) → panic.** With `panic = "abort"` (step 1) a bug stops the process
instead of trading on a corrupted number.

**Measured cost:** in a tight loop summing 1M quantities, checked `+` costs 0.43 ns per
element vs 0.28 ns unchecked, about 50% more. The likely cause is that the overflow
check stops the compiler using SIMD (several additions per instruction). For a 50-level
book that's about 7 ns per full sum, which matters against our budget. **It's handled
where sums actually happen (step 9):** accumulate in `i128` (64 values of at most
`i64::MAX` can't overflow it) and check once at the end. That's safe and fast, and it'll
be benchmarked against the per-add check.

## 4. Rejections: classified, with a defined policy
`parse_px`/`parse_qty` return `Result<_, ParseFixedError>`, never panic:

| Variant | Example (BTCUSDT unless stated) | `class()` |
|---|---|---|
| `Empty` | `""`, `"-"` | `Malformed` |
| `InvalidChar` | `"1e5"`, `"+1"`, `"1,000"`, `"NaN"`, `" 1"` | `Malformed` |
| `MissingDigits` | `".5"`, `"5."` | `Malformed` |
| `ExceedsPrecision` | `"50000.15"` (BTCUSDT has 1 decimal); never rounded | `SpecMismatch` |
| `Overflow` | beyond `i64` at this scale (exact boundary tested) | `SpecMismatch` |
| `OffTick` | `"100.3"` when tick is 0.5 | `SpecMismatch` |
| `OffLot` | `"1.001"` when lot is 0.002 | `SpecMismatch` |
| `Negative` | quantity `"-0.001"` | `Invalid` |

Design points:
- **One byte, `Copy`, no heap** (tested: `size_of::<ParseFixedError>() == 1`,
  `Result<Px, ParseFixedError>` is 16 bytes). Cheap to return from the hot path.
- **`class()` groups errors by what to do**, so callers choose a policy without matching
  every variant.
- Implements `core::error::Error` (works in `no_std`). `SpecError` (problems building a
  spec, at start-up only) exposes its cause through `source()`.
- **Context is added by the caller.** The leaf error stays tiny; the exchange parsers
  (steps 6–7) wrap it with venue, field and byte position.

**The policy** (implemented in steps 6, 7 and 13; fixed here so every layer agrees):

| Class | Action |
|---|---|
| `Malformed` | Reject the **whole message**. Never apply part of a message |
| `SpecMismatch` | Reject the message, alert, and reload instrument info (the venue changed tick/lot, or our config is wrong) |
| `Invalid` | Reject the message and resync the venue |

**Resync per feed type:**
- **Binance (each message is a full top-20 snapshot):** a dropped message heals itself
  with the next snapshot, about 100 ms later. The venue is marked stale until then.
- **Bybit (change-only deltas):** the book is now behind. The venue leaves the
  aggregated view immediately and resubscribes for a fresh snapshot.

**How it's surfaced:**
- The error passes up through `?` on a `#[cold]` path, so it costs nothing on the normal
  path.
- The hot path never logs directly. It bumps an error counter and pushes a fixed-size
  record to the logger thread (step 15), rate-limited.
- Repeated errors trip a circuit breaker: after N errors in a time window, disconnect
  and alert.

## 5. Division-free tick/lot check (found by measuring)
The rework added "is the price a multiple of the tick?" to every parse. Written as
`raw % tick == 0`, it made parsing **~2× slower: ~10 ns → ~19.5 ns per price.**

**Why:** `tick` is only known at run time, so `%` compiles to a 64-bit `idiv`
instruction. On this CPU that takes tens of cycles; at 2.8 GHz, 30 cycles ≈ 10 ns. When
a divisor is a compile-time constant the compiler avoids `idiv` automatically, but ours
comes from config.

**Fix (`divisor.rs`):** precompute constants once per spec, then test divisibility with
one multiply, one rotate and one compare. Write `d = odd × 2^k`. An odd number has an
inverse `inv` modulo 2⁶⁴ (`odd × inv` wraps to exactly 1). Then:
```rust
// n is a multiple of d  ⟺  rotate_right(n × inv, k) <= u64::MAX / d   (wrapping multiply)
n.unsigned_abs().wrapping_mul(self.inv).rotate_right(self.shift) <= self.limit
```
`inv` is computed by Newton's iteration: each step doubles the number of correct bits,
3→6→12→24→48→96, so 5 steps suffice. The method is from Granlund & Montgomery (1994)
and *Hacker's Delight* §10-17. It's the same trick compilers use for constant divisors.

**Verified:** against `%` for every divisor 1–300 plus large and power-of-two divisors,
over all n in −2000..2000, the `i64` extremes, random values, exact multiples, and
values just past a multiple. **Result: parse back to ~13 ns.** The remaining ~3 ns over
version 1 is the extra validation work. The fast parser in step 5 reuses this check.

## Alternatives considered

| Option | Why not |
|---|---|
| `f64` | `0.1 + 0.2 != 0.3`. Most decimal prices aren't exactly representable, so equality, map keys and sums break. Never acceptable for prices |
| One global scale (version 1) | Overflows for huge quantities, rejects high-precision tokens |
| Scale as a const generic (`Px<const D: u8>`) | Zero run-time cost, but instruments are only known at run time from venue info, and every book/aggregator would become generic over decimals. A run-time scale costs a table lookup in the parser, nothing in compares |
| A decimal library (`rust_decimal`, 128-bit with per-value scale) | 16 bytes instead of 8; compares must normalise scales; slower. Solves arbitrary scale per *value*, which we don't need |
| Instrument encoded in the type (phantom generic) | Catches cross-instrument comparisons at compile time, but adds generic parameters everywhere; one-instrument-per-book already prevents it |
| `Qty(u64)` | See section 2 |
| Debug-only overflow checks (version 1) | A release build could create a negative `Qty`; the cost of always checking is handled where it matters (step 9) |
| `raw % tick` | A hardware divide on every parse; measured 2× slowdown |
| Round excess precision | Hides a changed tick or a broken config; a wrong price is worse than a rejected one |

## Benefits (measured, rough)
On this Mac (i7-8569U), 1M BTC-style prices (`"6xxxx.d"`), 3 rounds, best round shown.
These come from simple timing loops, **not** criterion or JMH (criterion arrives in
step 4). Also, Java's sort is TimSort on objects while Rust's is an unstable quicksort on
integers, so the sort row compares the whole approach, not just the algorithm.

| | Rust (this version) | Rust (version 1) | Java `BigDecimal` |
|---|---|---|---|
| Parse + validate a price | ~13 ns (incl. tick check) | ~10 ns (no tick check) | ~34 ns (no tick check) |
| Compare two prices | ~1.2 ns | ~1.2 ns | ~4.6–5 ns |
| Sort 1M prices | ~9.5 ms | ~11 ms | ~217 ms (485 ms cold) |
| Memory per price | 8 bytes | 8 bytes | ~76 bytes (heap delta, incl. array slot) |
| Heap allocations per price | 0 | 0 | 1+ |

The sort gap is the biggest because of memory layout. 1M `Px` are one contiguous 8 MB
array the CPU streams through. 1M `BigDecimal`s are pointers to objects scattered across
the heap, so every compare risks two cache misses. Java's cold vs warm round is JIT
warm-up; Rust has no such phase.

Tests: **25 unit tests + 1 doc example**, passing in debug and release. They cover:
- exact parsing, and the scale-equality trap;
- precision, tick and lot rejection;
- malformed input, and overflow at the exact boundary;
- negative quantities, and error classes and sizes;
- the meme-coin and 18-decimal cases;
- the headroom check;
- multi-venue rescaling, and invalid specs;
- display (with padding), notional decimals, and a display→parse round trip at every
  scale from 0 to 18;
- checked arithmetic panicking in release;
- the divisor test against `%`.

`clippy -D warnings` is clean.

## Java counterpart
The skeleton uses `BigDecimal` everywhere and has no instrument concept:
```java
public record PriceLevel(BigDecimal price, BigDecimal quantity, Side side, String exchangeId) {}
// per level of every message:
new PriceLevel(new BigDecimal(bid.get(0).asText()), new BigDecimal(bid.get(1).asText()), Side.BID, "binance")
```
- **Allocation:** each level creates a `String`, two `BigDecimal`s and a record, at least
  4–6 heap objects, all garbage moments later.
- **No tick/lot validation at all:** an off-tick price from a changed venue spec goes
  straight into the book.
- **The `equals` trap** (a classic interview question):
  ```java
  new BigDecimal("50000.0").equals(new BigDecimal("50000.00"))     // false: scale differs
  new BigDecimal("50000.0").compareTo(new BigDecimal("50000.00"))  // 0
  ```
  A `HashMap<BigDecimal, …>` book can keep two levels for one price (`TreeMap` is safe;
  it uses `compareTo`). Our parsing gives one representation per value (tested).
- **Error handling is the opposite of our policy:**
  ```java
  public void onMessage(String message) {
      try {
          root.get("b").forEach(bid -> handler.handleOrder(new PriceLevel(...)));  // applied one by one
          ...
      } catch (Exception e) {
          log.error("Could not parse message", e);   // synchronous log + stack trace, then carry on
      }
  }
  ```
  If level 7 of 20 throws, levels 1–6 are **already applied**. The book is left
  half-updated and nothing resyncs it. Logging a stack trace on the I/O thread is slow
  and allocates.

What a low-latency Java version would need (Chronicle/LMAX style):
```java
final class InstrumentSpec { final int pxDecimals, qtyDecimals; final long tick, lot; }
long px = parseFixed(bytes, off, len, spec.pxDecimals);   // hand-written, no String
if (px % spec.tick != 0) reject(OFF_TICK);                // same idiv cost as ours had
long hi = Math.multiplyHigh(px, qty), lo = px * qty;      // 128-bit notional needs two longs
```
It's possible, but Java has no zero-cost newtype (bare `long` everywhere, or wrapper
objects). Value classes (Project Valhalla) aren't in Java 17. Rust's `struct Px(i64)`
gives safety at zero cost today.

## How to verify it yourself
```bash
cd rust
cargo test -p ob-core              # 25 tests + doc example
cargo test -p ob-core --release    # includes the release-mode overflow panics
cargo clippy --workspace --all-targets -- -D warnings
cargo doc -p ob-core --no-deps --open
```
Things to try:
- Replace `!self.tick_div.divides(raw)` with `raw % self.tick != 0` in `spec.rs`. Tests
  still pass, and rerunning a parse timing loop shows the slowdown.
- Add `let _ = Px::ZERO + Px::ZERO;` or pass a `Qty` where a `Px` is expected: neither
  compiles.
- In `spec.rs`, change `assert_eq!(size_of::<ParseFixedError>(), 1)` to `2`, and the test
  fails.

## Likely interview questions
1. **Why not `f64` for prices?**
   Most decimal prices aren't exactly representable in binary floating point, so equality,
   map keys and sums become unreliable. Fixed-point integers are exact and at least as fast.
2. **Why a per-instrument scale instead of one global scale?**
   A global scale either overflows (huge quantities) or rejects precision (18-decimal
   tokens). Per-instrument decimals from the venue's published steps give exactness with
   maximum range. Venues of one instrument share the finest scale so prices compare
   directly.
3. **Why `i64` for a quantity that can't be negative?**
   The type enforces non-negativity. Signed means a bug shows as an impossible negative
   rather than a plausible huge number, differences don't need casts, and the speed is
   identical.
4. **What happens on overflow?**
   Parsing returns `Overflow`. Notional is `i128`, which can't overflow for `i64 × i64`.
   `Qty`/`Notional` `+`/`-` are checked in every build and panic, because overflow there
   is a bug, and with `panic = "abort"` the process stops rather than trade on bad numbers.
5. **Isn't checking every addition slow?**
   Measured: about 50% slower in a tight sum loop, since it blocks SIMD. Single checks
   cost almost nothing. For sums we'll accumulate in `i128` and check once (step 9).
6. **Why reject `50000.15` instead of rounding?**
   It means the venue changed its tick or our spec is wrong. Rounding would hide that and
   change a price silently. Rejecting it triggers a spec reload and an alert.
7. **How do you avoid the division in the tick check?**
   Precomputed modular inverse: for `d = odd × 2^k`, `n` is a multiple of `d` exactly
   when `rotr(n × inv, k) <= u64::MAX / d`. One multiply, rotate and compare instead of
   an `idiv` that takes tens of cycles. Measured: parse went from ~19.5 ns back to ~13 ns.
8. **What happens to a message with one bad level?**
   The whole message is rejected, never partly applied. Then it depends on the feed: a
   snapshot feed heals on the next snapshot; a delta feed resyncs. The Java skeleton
   applies levels one by one and leaves the book half-updated when one throws.
