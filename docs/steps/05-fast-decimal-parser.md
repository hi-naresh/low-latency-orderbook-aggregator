# Step 5: Fast decimal parser (and why SWAR lost)

Design reference: Phase 2 of the plan ("SWAR decimal parser"). Exchanges send every
price and quantity as decimal text: a Binance 20-level snapshot is 80 numbers per
message. This step builds the hot-path number parser and measures it.

**Headline:** the plan assumed a SWAR ("SIMD within a register") parser would be the
fastest. I built two versions; **both lost** to a simple one-pass scalar parser on real
exchange-format numbers. The scalar parser is now the default. The final gain over the
step 2 reference parser is modest (about 10–15%), and this note explains why. The
negative result, and the reasons behind it, is the most useful part of this step.

## What changed

| File | Content |
|---|---|
| `rust/crates/ob-parse/` | **New crate** (`no_std`, depends only on `ob-core`): the parsers for exchange data. This step: numbers. Steps 6–7: Binance and Bybit messages |
| `ob-parse/src/decimal.rs` | `scalar` (default), `swar`, `swar_padded`; hot-path `parse_px` / `parse_qty`; 6 unit tests |
| `ob-parse/tests/differential.rs` | 7 differential tests: every fast parser must match the reference exactly, errors included |
| `ob-parse/benches/decimal.rs` | Criterion benchmarks on 65,536 realistic numbers |
| `ob-core/src/spec.rs` | The reference parser is now public (`ob_core::parse_decimal`); new `px_from_raw` / `qty_from_raw` validate an already-parsed raw value (tick, lot, sign) |

## 1. The correctness contract: fast must equal reference

The step 2 reference parser (`ob_core::parse_decimal`) is simple and obviously correct.
Every fast parser must return **exactly** what it returns, for every input and every
scale (0–18), **errors included**.

How each fast parser meets that contract without duplicating all the error logic:
- It handles only the **common, valid shapes** itself.
- Anything unusual goes to the reference parser on a cold path: a minus sign, more than
  16 or 19 characters, an invalid byte, `".5"`, `"5."`, a non-zero digit past the scale,
  or overflow.
  ```rust
  #[cold]            // rare: compiled out of line, so the fast path is straight-line code
  #[inline(never)]
  fn slow(text: &[u8], decimals: u8) -> Result<i64, ParseFixedError> {
      ob_core::parse_decimal(text, decimals)
  }
  ```
- Errors in real feeds are rare, so the slow path costs nothing in practice. And error
  messages can't drift between the two parsers, because there's only one implementation
  of the error logic.

### Differential tests

| Test | What it covers |
|---|---|
| `exhaustive_short_strings` | **Every** string of 0–6 characters from `0 1 9 . - x /` (137,257 strings), at 6 scales: 823,542 cases, each checked against all three fast parsers |
| `every_length_from_1_to_20_...` | Runs of `9` of every length 1–20 with the point at every position, every scale 0–18 (overflow and length limits) |
| `..._on_well_formed` (20,000 cases) | Random valid decimals: 1–20 integer digits, 0–20 fraction digits, trailing zeros, rare minus signs |
| `..._on_near_misses` (20,000 cases) | Random strings over `0-9 . - + e E space x / : \0` |
| `..._on_arbitrary_bytes` (20,000 cases) | Completely random bytes |
| `spec_level_calls_match` | `ob_parse::parse_px/qty` equal `InstrumentSpec::parse_px/qty` for random ticks and lots |
| `error_type_is_unchanged` | Spot checks of specific error kinds |

**These tests caught two real bugs during this step:**
1. **A planted one,** to prove the tests have teeth: I let SWAR accept `"5."`. proptest
   shrank the failure to `"0."` at 0 decimals: *swar got Ok(0), reference
   Err(MissingDigits)*.
2. **A genuine one in my rewrite:** `"9.9"` at 18 decimals is 9×10¹⁸ + 9×10¹⁷, which
   overflows `i64`. My comment said "only the multiply can overflow", but the final
   *addition* can too, and in a release build it silently wrapped to
   **−8,546,744,073,709,551,616**. The exhaustive test caught it on the first run. Fixed
   with `checked_add`. This is exactly why fast code gets checked against a slow oracle.

## 2. The three parsers

### `scalar` (the default)
One pass, one byte at a time, minimal work per byte:
```rust
for (i, &b) in text.iter().enumerate() {
    let d = b.wrapping_sub(b'0');          // '0'..='9' → 0..=9; anything else → ≥ 10
    if d < 10 {
        value = value * 10 + u64::from(d);
        digits += 1;
    } else if b == b'.' && dot == n {
        dot = i;
    } else {
        return slow(text, decimals);       // '-', a second '.', or junk
    }
}
```
Compared with the reference parser, it removes:
- the separate pass to find the `.`;
- checked arithmetic on every digit (with at most 18 digits, a `u64` can't overflow);
- the place-value table.

**Extra fractional zeros:** Binance sends `"105123.40"` for an instrument whose tick is
0.1 (1 decimal). The trailing `0` must be checked and dropped:
```rust
while frac > decimals {
    if !value.is_multiple_of(10) { return None; }  // non-zero → ExceedsPrecision (via reference)
    value /= 10;   // division by the *constant* 10: the compiler emits multiply + shift, not idiv
    frac -= 1;
}
```
(Step 2 taught us that a division by a *run-time* value costs tens of cycles; a constant
divisor doesn't.)

### `swar` (64-bit SWAR, kept but not used)
The idea: instead of a loop over bytes, treat 8 bytes as one 64-bit integer and process
them all at once with ordinary integer instructions.
1. **Load** up to 16 bytes as two `u64`s. To stay within the slice without a loop, it
   uses overlapping loads: the first 8 bytes, plus the last 8 shifted down.
2. **Find the `.`** with the classic zero-byte trick. `x XOR "........"` turns the `.`
   into a zero byte. Then `(v − 0x0101…) & !v & 0x8080…` sets the top bit of the first
   zero byte, and `trailing_zeros() / 8` gives its index.
3. **Integer part:** move it to the end of an 8-byte word and pad the front with `'0'`.
4. **Fraction:** shift it down, check any digits past the scale are `'0'` with a single
   masked compare, and keep the rest.
5. **Validate** 8 bytes at once: every byte must have high nibble `3`, and adding 6
   mustn't carry out of the low nibble.
6. **Convert** 8 digits with 3 multiplies instead of 8:
   ```rust
   let v = chunk - 0x3030_3030_3030_3030;                   // bytes → 0..9
   let v = (v * 10 + (v >> 8)) & 0x00FF_00FF_00FF_00FF;     // 4 two-digit numbers
   let v = (v * 100 + (v >> 16)) & 0x0000_FFFF_0000_FFFF;   // 2 four-digit numbers
   (v * 10_000 + (v >> 32)) & 0x0000_0000_FFFF_FFFF         // 1 eight-digit number
   ```

### `swar_padded` (kept but not used)
`swar` needs branches on the number's length to load bytes safely. When the number sits
inside a larger buffer with guaranteed padding after it (the simdjson technique), you can
always load a full 16-byte window and mask off what isn't the number. `swar_padded` does
that. Step 6's message parser can guarantee the padding, so this stays available to
re-measure there.

## 3. Measurements, and how the conclusion changed

Machine: i7-8569U, macOS, release profile. 65,536 numbers (half prices, half quantities)
stored back to back like the fields of a message, too many for the branch predictor to
memorise. Each parser is inlined into the loop (step 4's lessons). Results are
nanoseconds per number, criterion medians over 2–3 runs.

### Attempt 1: SWAR on `u128`, mixed formats
| Parser | ns / number |
|---|---|
| reference | 13.1 |
| scalar | 12.9–14.1 |
| SWAR (`u128`) | 21–24 |
| `f64` parse (context) | 22 |

SWAR was much *slower*. **Why, from the assembly:** it compiled to **253 instructions**,
against 106 for scalar. x86-64 has no native 128-bit shift, so every variable `u128`
shift becomes about 5 instructions plus conditional moves, and the design did about six
of them to cut out the decimal point.

### Attempt 2: SWAR redesigned on `u64` (the current `swar`)
Instead of physically removing the `.`, it parses the integer part and the fraction as
two separate 8-digit words. A variable `u64` shift is a single instruction. Two corpora:
- **mixed:** the format changes randomly between numbers (worst case);
- **binance:** one venue's consistent format (realistic).

| Parser | mixed | binance (realistic) |
|---|---|---|
| reference | 14.7–14.9 | 13.2–13.6 |
| **scalar** | **13.5–14.0** | **11.1–11.2** |
| SWAR (`u64`) | 18.1–18.2 | 16.1 |
| SWAR padded | 16.8 | 14.9–15.5 |
| `f64` parse (context, inexact) | 22.0–22.6 | 19.6–20.2 |

Better (21 → 15–16 ns), but **still slower than scalar**.

### The full hot-path call: parse + tick/lot validation (binance corpus)
| | ns / number |
|---|---|
| `InstrumentSpec::parse_px/qty` (reference) | 13.6–14.1 |
| **`ob_parse::parse_px/qty` (scalar)** | **12.5** |

### Why SWAR lost here
- **The numbers are short.** BTC prices are 6+2 digits and sizes are 1–4+3 digits. SWAR
  has a **fixed cost per number**: find the point, align both parts, mask, then two
  chains of 3 dependent multiplies (3 cycles of latency each). A scalar loop over 5–9
  bytes finishes sooner.
- **SWAR wins on long digit runs** (16-digit IDs, timestamps, high-precision decimals).
  That's where libraries like simdjson and fast_float use it. Exchange prices and sizes
  aren't that.
- **The remaining cost is mostly branch prediction.** Numbers have different lengths, so
  the loop exit (or SWAR's checks) can't be predicted perfectly. At ~11 ns per number,
  most of the time is roughly one mispredict (~5–7 ns) plus the per-byte work. The
  consistent-format corpus is 2–3 ns faster than the mixed one for every parser, which
  points the same way.

### What this means for the budget
80 numbers per Binance message × ~12 ns ≈ **1 µs per message just for numbers**. That's
above the plan's target of 0.2–0.5 µs for the whole message. The levers that remain
(steps 6, 20) are:
1. **Fuse number parsing into the JSON scan** (step 6). While the scanner walks the
   message it already knows where each number starts and ends, and where the `.` is. A
   second pass over each number disappears, along with a share of the per-number
   mispredictions.
2. **Real SIMD** (SSE4.2/AVX2 `pshufb` shuffles) can compact and convert 16 bytes in a
   handful of instructions, with no 128-bit-shift penalty. It's x86-specific `unsafe`
   code, so it's deferred to step 20, and adopted only if measured faster.
3. **Skip work:** don't parse levels that don't change the book.

## 4. Alternatives considered

| Option | Verdict |
|---|---|
| Keep the reference parser | Already within ~15% of the best. Kept as the oracle and the fallback |
| `scalar` | **Chosen**: fastest on real data, simple, easy to review |
| SWAR on `u128` | Built and measured: 128-bit shifts made it 60% slower than scalar |
| SWAR on `u64` | Built and measured: fixed per-number cost too high for short numbers; kept for re-evaluation |
| `str::parse::<f64>()` | Slowest (~20 ns), **inexact** (binary floating point), and needs UTF-8 validation first. Unacceptable for prices (step 2) |
| A JSON library's number parsing (`serde_json`) | Step 6 compares whole-message parsing, where it belongs |
| Explicit SIMD intrinsics now | Platform-specific `unsafe`; only worth it with evidence → step 20 |

## 5. Java counterpart
The skeleton parses every number like this:
```java
new BigDecimal(bid.get(0).asText())   // JsonNode → String → BigDecimal
```
That's several heap allocations per number:
- Jackson's tree node, created while building the `JsonNode` tree;
- the `String` from `asText()`;
- the `BigDecimal`, and possibly a `BigInteger` inside it.

Step 2 measured `new BigDecimal(String)` at about 34 ns, before the tree and `String`
costs. Our fast path is about 12 ns with **zero allocation**.

What low-latency Java does instead: parse straight from the bytes of the network buffer
into a `long`, with no `String` at all. Real Java parsers make the same SWAR-vs-scalar
trade-off:
```java
long v = 0; int dot = -1;
for (int i = off; i < end; i++) {
    int d = buf[i] - '0';
    if (d >= 0 && d <= 9) v = v * 10 + d;
    else if (buf[i] == '.' && dot < 0) dot = i;
    else return slowPath(buf, off, end);
}
```
For SWAR in Java you'd read 8 bytes with a `VarHandle` (`byteArrayViewVarHandle`) or
`Unsafe.getLong`. The JIT may or may not compile it as tightly, and you can't inspect it
as easily as `objdump` on a Rust binary. JMH plus `-prof perfasm` is the Java way to look.

## How to verify it yourself
```bash
cd rust
cargo test -p ob-parse --release                     # 6 unit + 7 differential tests
cargo test -p ob-parse --test differential           # differential tests in debug (~1 s)
cargo bench -p ob-parse --bench decimal              # all parsers, both corpora (~2 min)
cargo bench -p ob-parse --bench decimal -- validated # just the hot-path call
```
Things to try:
- **Watch the overflow bug come back:** in `decimal.rs` (`swar_words`, step 6), replace
  `.and_then(|v| v.checked_add(frac_value))` with `.map(|v| v + frac_value)`. Then run
  `cargo test -p ob-parse --release --test differential` and see the
  `"9.9" at 18 decimals` failure. Revert afterwards.
- **Switch the default to SWAR:** change `scalar(` to `swar(` in `parse_px`/`parse_qty`.
  Tests still pass (identical results); the `validated` benchmark gets slower.

## Likely interview questions
1. **What is SWAR?**
   "SIMD within a register": using ordinary 64-bit integer operations to process 8 bytes
   at once, such as finding a byte, validating digits, or converting 8 digits with 3
   multiplies. It needs no special instructions and is portable.
2. **You planned SWAR. Why isn't it the default?**
   I built two versions and measured on realistic data: both were slower. Exchange
   numbers are 5–9 characters, so SWAR's fixed setup cost per number outweighs the
   saving; it wins on long digit runs. The first version also used `u128` shifts, which
   x86-64 can't do natively; the assembly showed 253 instructions against 106.
3. **How do you make sure a fast parser is correct?**
   Differential testing against a simple reference on every input class: exhaustive short
   strings, every length and point position, random valid, near-valid and arbitrary
   bytes, at every scale. It caught a real release-only overflow bug that wrapped to a
   negative price.
4. **How do the fast parsers report errors?**
   They don't decide errors themselves. Anything unusual goes to the reference on a
   `#[cold]` path, so error behaviour is identical by construction, and the fast path
   stays small.
5. **Why not parse as `f64`?**
   It's inexact for decimal prices (step 2), slower here (~20 ns), and needs UTF-8
   validation first.
6. **Where does the remaining ~11 ns go?**
   Mostly about one branch misprediction per number, because lengths vary, plus the
   per-byte work. Next: fuse number parsing into the JSON scan (step 6), and consider
   real SIMD (step 20).
7. **Why `value /= 10` in a loop instead of dividing by `10^excess`?**
   Dividing by the constant 10 compiles to a multiply and shift. Dividing by a run-time
   power of ten would be a hardware divide (tens of cycles). The loop usually runs once
   (Binance's extra trailing zero).
