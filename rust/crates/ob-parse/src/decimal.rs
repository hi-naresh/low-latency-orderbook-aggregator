//! Fast decimal parsers for exchange numbers such as `"105123.40"` or `"0.001"`.
//!
//! Exchanges send every price and quantity as decimal text, so a 20-level snapshot means
//! 80 numbers to parse per message. These parsers turn that text into the same raw
//! integers as the reference parser [`ob_core::parse_decimal`] (see step 2):
//! `parse(b"105123.40", 1) == Ok(1051234)`.
//!
//! Fast strategies, benchmarked against the reference (step 5 notes):
//!
//! * [`scalar`]: one pass, one byte at a time, minimal work per byte. **The default**
//!   ([`parse_px`], [`parse_qty`]): fastest on real exchange numbers (5–9 characters).
//! * [`swar`] / [`swar_padded`]: "SIMD within a register". Validates and converts 8
//!   digits at a time with plain 64-bit integer operations. Measured slower than
//!   [`scalar`] on short numbers, because its fixed cost per number (finding the point,
//!   aligning both parts, two 3-multiply conversions) exceeds a short byte loop. Kept,
//!   tested, for re-evaluation inside the message parser.
//!
//! **Correctness contract.** For every input and every `decimals`, each fast parser
//! returns exactly what the reference returns, errors included. They achieve this by
//! handling only the common, valid shapes themselves; anything unusual (a minus sign,
//! more than 16 characters, any invalid byte, overflow) goes to the reference parser on
//! a `#[cold]` path, which produces the precise error. Errors are rare in real feeds, so
//! the slow path costs nothing in practice. The differential tests in
//! `tests/differential.rs` check the contract on millions of inputs.

use ob_core::{InstrumentSpec, ParseFixedError, Px, Qty};

/// `POW10[n] = 10^n` for n in 0..=18 (the largest power of ten in `i64`).
const POW10: [i64; 19] = {
    let mut t = [1_i64; 19];
    let mut i = 1;
    while i < t.len() {
        t[i] = t[i - 1] * 10;
        i += 1;
    }
    t
};

/// The hot-path price parser: fast decimal parse + tick check.
/// Identical results to [`InstrumentSpec::parse_px`].
///
/// Uses [`scalar`], the fastest strategy on exchange-format numbers in the step 5
/// benchmarks.
#[inline]
pub fn parse_px(spec: &InstrumentSpec, text: &[u8]) -> Result<Px, ParseFixedError> {
    spec.px_from_raw(scalar(text, spec.px_decimals())?)
}

/// The hot-path quantity parser: fast decimal parse + sign and lot checks.
/// Identical results to [`InstrumentSpec::parse_qty`].
#[inline]
pub fn parse_qty(spec: &InstrumentSpec, text: &[u8]) -> Result<Qty, ParseFixedError> {
    spec.qty_from_raw(scalar(text, spec.qty_decimals())?)
}

/// Everything unusual goes here: the reference parser gives the exact answer or error.
///
/// `#[cold]` + `#[inline(never)]` keep this out of the fast path's code, so the common
/// case is straight-line code with the fallback as a rarely-taken jump.
#[cold]
#[inline(never)]
fn slow(text: &[u8], decimals: u8) -> Result<i64, ParseFixedError> {
    ob_core::parse_decimal(text, decimals)
}

// ---------------------------------------------------------------------------
// Scalar: one byte at a time
// ---------------------------------------------------------------------------

/// Parses one byte at a time, in a single pass.
///
/// Each byte costs a subtract, a compare and a multiply-add; the reference parser's
/// separate pass to find the `.`, its per-digit overflow checks and its place-value table
/// are gone. At most 18 digits are accepted here (so the `u64` accumulator can't
/// overflow); longer inputs fall back to the reference.
#[inline]
pub fn scalar(text: &[u8], decimals: u8) -> Result<i64, ParseFixedError> {
    let n = text.len();
    // 19 bytes = up to 18 digits + a point.
    if n == 0 || n > 19 || decimals > ob_core::MAX_DECIMALS {
        return slow(text, decimals);
    }
    let mut value: u64 = 0;
    let mut dot = n; // position of '.', or n if none
    let mut digits = 0;
    for (i, &b) in text.iter().enumerate() {
        let d = b.wrapping_sub(b'0'); // '0'..='9' → 0..=9; anything else → >= 10
        if d < 10 {
            value = value * 10 + u64::from(d);
            digits += 1;
        } else if b == b'.' && dot == n {
            dot = i;
        } else {
            return slow(text, decimals); // '-', a second '.', or an invalid byte
        }
    }
    // ".5", "5." and overlong digit runs are the reference's business.
    if dot == 0 || dot == n - 1 || digits > 18 {
        return slow(text, decimals);
    }
    let frac = if dot == n { 0 } else { n - dot - 1 };
    finish(value, frac, decimals)
        .ok_or(())
        .or_else(|()| slow(text, decimals))
}

/// Scales `value` (all digits read, `frac` of them after the point) to `decimals`
/// places. `None` means "let the reference decide" (excess non-zero digits, overflow).
#[inline(always)]
pub(crate) fn finish(mut value: u64, frac: usize, decimals: u8) -> Option<i64> {
    let decimals = usize::from(decimals);
    let mut frac = frac;
    // More fractional digits than the scale holds: they must all be zeros. Exchanges do
    // this routinely (Binance sends "105123.40" for a 0.1-tick instrument).
    while frac > decimals {
        if !value.is_multiple_of(10) {
            return None; // ExceedsPrecision
        }
        value /= 10; // division by the constant 10 compiles to a multiply and shift
        frac -= 1;
    }
    i64::try_from(value)
        .ok()?
        .checked_mul(POW10[decimals - frac])
}

// ---------------------------------------------------------------------------
// SWAR: 8 digits per operation
// ---------------------------------------------------------------------------

/// 8 copies of a byte, as one `u64`.
const fn splat(b: u8) -> u64 {
    u64::from_le_bytes([b; 8])
}
const ZEROS: u64 = splat(b'0');

/// A mask covering the low `k` bytes (`k` in 0..=8).
#[inline(always)]
fn low_bytes(k: usize) -> u64 {
    // 64 - 8k is in 0..=64; shifting by 64 is out of range, and means "no bytes".
    shr(u64::MAX, 64 - 8 * k)
}

/// A shift amount as `u32`; anything too large becomes `u32::MAX`, which the checked
/// shifts below treat as "out of range" (result 0), exactly like any amount >= 64.
#[inline(always)]
fn amount(s: usize) -> u32 {
    u32::try_from(s).unwrap_or(u32::MAX)
}

/// `x << s`, or 0 when `s >= 64` (where a plain shift would be out of range).
#[inline(always)]
fn shl(x: u64, s: usize) -> u64 {
    x.checked_shl(amount(s)).unwrap_or(0)
}

/// `x >> s`, or 0 when `s >= 64`.
#[inline(always)]
fn shr(x: u64, s: usize) -> u64 {
    x.checked_shr(amount(s)).unwrap_or(0)
}

/// Loads up to 16 bytes as two little-endian words (byte `i` of the text → byte `i` of
/// `lo`, then `hi`), with zero bytes past the end. Never reads past the slice.
///
/// For 8–16 bytes it uses two overlapping 8-byte loads: the first 8 bytes, and the last
/// 8 shifted down so only the bytes after the first 8 remain. Same trick with 4-byte
/// loads for 4–7 bytes. No loop except for 1–3 bytes.
#[inline(always)]
fn load(text: &[u8]) -> (u64, u64) {
    let n = text.len();
    if let (Some(head), Some(tail)) = (text.first_chunk::<8>(), text.last_chunk::<8>()) {
        // tail = bytes n-8..n; we want bytes 8..n at the bottom: drop the 16-n overlap.
        (
            u64::from_le_bytes(*head),
            shr(u64::from_le_bytes(*tail), 8 * (16 - n)),
        )
    } else if let (Some(head), Some(tail)) = (text.first_chunk::<4>(), text.last_chunk::<4>()) {
        // Overlapping bytes hold identical values, so OR-ing them is harmless.
        let lo = u64::from(u32::from_le_bytes(*head))
            | u64::from(u32::from_le_bytes(*tail)) << (8 * (n - 4));
        (lo, 0)
    } else {
        let mut lo = 0;
        for (i, &b) in text.iter().enumerate() {
            lo |= u64::from(b) << (8 * i);
        }
        (lo, 0)
    }
}

/// A mask with `0x80` in every zero byte of `x` (exact for the lowest one, which is all
/// we use). Classic bit trick: subtracting 1 from a zero byte borrows and sets its top
/// bit; `!x` rules out bytes that already had it set.
#[inline(always)]
const fn zero_bytes(x: u64) -> u64 {
    x.wrapping_sub(splat(0x01)) & !x & splat(0x80)
}

/// `true` if every byte is an ASCII digit `'0'..='9'` (0x30..=0x39).
/// High nibble must be 3, and adding 6 must not carry out of the low nibble (≤ 9).
#[inline(always)]
const fn all_digits(x: u64) -> bool {
    let high = splat(0xF0);
    (x & high) == ZEROS && (x.wrapping_add(splat(0x06)) & high) == ZEROS
}

/// Converts 8 ASCII digits (byte 0 = most significant) to their value with three
/// multiply-add steps instead of eight.
#[inline(always)]
const fn eight_digits(chunk: u64) -> u64 {
    let v = chunk - ZEROS; // each byte '0'..'9' → 0..9
    let v = (v * 10 + (v >> 8)) & 0x00FF_00FF_00FF_00FF; // 4 two-digit numbers
    let v = (v * 100 + (v >> 16)) & 0x0000_FFFF_0000_FFFF; // 2 four-digit numbers
    (v * 10_000 + (v >> 32)) & 0x0000_0000_FFFF_FFFF // 1 eight-digit number
}

/// Puts the low `k` bytes of `x` (k in 0..=8 digits) at the *end* of an 8-byte word and
/// fills the front with `'0'`, so `eight_digits` reads them as a k-digit number.
#[inline(always)]
fn right_align(x: u64, k: usize) -> u64 {
    shl(x, 8 * (8 - k)) | shr(ZEROS, 8 * k)
}

/// Parses with SWAR ("SIMD within a register"): plain 64-bit integer operations that
/// work on 8 bytes at once.
///
/// 1. Load the text (≤ 16 bytes) as two 64-bit words.
/// 2. Find the `.` with the zero-byte trick on `word XOR "........"`.
/// 3. Integer part (≤ 8 digits): right-align it in a word, pad the front with `'0'`.
/// 4. Fraction (≤ 8 digits): shift it down to the bottom of a word. If it has more
///    digits than the scale, check the extra ones are `'0'` (one masked compare) and
///    keep only the first `decimals`.
/// 5. Check both words are all digits (two masked compares each).
/// 6. Convert each word with three multiplies, and combine:
///    `raw = int × 10^decimals + frac × 10^(decimals − kept)`.
///
/// Integer or fractional parts longer than 8 digits are rare in market data and go to
/// [`scalar`]; anything invalid or unusual goes to the reference.
#[inline]
pub fn swar(text: &[u8], decimals: u8) -> Result<i64, ParseFixedError> {
    let n = text.len();
    if n == 0 || n > 16 || decimals > ob_core::MAX_DECIMALS {
        return slow(text, decimals);
    }
    let (lo, hi) = load(text);
    swar_words(lo, hi, text, decimals)
}

/// [`swar`] for text that sits inside a larger, padded buffer: `window` is the 16 bytes
/// starting at the number (they may run past it into the rest of the message).
///
/// Two plain 8-byte loads plus a mask replace the length-dependent branches of `swar`'s safe load. The
/// message parser (step 6) keeps at least 16 bytes of padding after every message, so a
/// full window always exists. `text` is the number itself (for the fallback).
#[inline]
pub fn swar_padded(text: &[u8], window: &[u8; 16], decimals: u8) -> Result<i64, ParseFixedError> {
    let n = text.len();
    if n == 0 || n > 16 || decimals > ob_core::MAX_DECIMALS {
        return slow(text, decimals);
    }
    debug_assert_eq!(&window[..n], text, "window must start at the number");
    let (head, tail) = window.split_at(8);
    let lo = u64::from_le_bytes(head.try_into().unwrap_or([0; 8]));
    let hi = u64::from_le_bytes(tail.try_into().unwrap_or([0; 8]));
    // Keep the n bytes of the number; zero everything after it.
    swar_words(
        lo & low_bytes(n.min(8)),
        hi & low_bytes(n.saturating_sub(8)),
        text,
        decimals,
    )
}

/// Long numbers (more than 8 integer or fractional digits): rare, handled by [`scalar`].
#[cold]
#[inline(never)]
fn long_number(text: &[u8], decimals: u8) -> Result<i64, ParseFixedError> {
    scalar(text, decimals)
}

/// Steps 2–6 of [`swar`], on the number loaded as `lo`/`hi` (zero bytes past the end).
#[inline(always)]
fn swar_words(lo: u64, hi: u64, text: &[u8], decimals: u8) -> Result<i64, ParseFixedError> {
    let n = text.len();
    let d = usize::from(decimals);

    // 2. First '.', if any. Bytes past the end are 0x00, never '.'.
    let (dots_lo, dots_hi) = (zero_bytes(lo ^ splat(b'.')), zero_bytes(hi ^ splat(b'.')));
    let dot = if dots_lo != 0 {
        (dots_lo.trailing_zeros() / 8) as usize
    } else if dots_hi != 0 {
        8 + (dots_hi.trailing_zeros() / 8) as usize
    } else {
        n // no point: all integer
    };
    if dot == 0 || dot == n - 1 && dot < n {
        return slow(text, decimals); // ".5" or "5."
    }
    let int_len = dot.min(n);
    let frac_len = n.saturating_sub(dot + 1);
    if int_len > 8 || frac_len > 8 {
        return long_number(text, decimals);
    }

    // 3. Integer part: the first int_len bytes of `lo`.
    let int_word = right_align(lo & low_bytes(int_len), int_len);

    // 4. Fraction: the frac_len bytes after the point, moved to the bottom of a word.
    let start = dot + 1;
    let frac_all = if start < 8 {
        shr(lo, 8 * start) | shl(hi, 8 * (8 - start))
    } else {
        shr(hi, 8 * (start - 8))
    };
    let frac_all = frac_all & low_bytes(frac_len);
    let keep = frac_len.min(d);
    if frac_len > keep {
        // Digits beyond the scale must all be '0' (Binance sends "105123.40" for a
        // 0.1-tick instrument).
        let extra = low_bytes(frac_len) & !low_bytes(keep);
        if frac_all & extra != ZEROS & extra {
            return slow(text, decimals);
        }
    }
    let frac_word = right_align(frac_all & low_bytes(keep), keep);

    // 5. Any '-', second '.', or other non-digit fails here.
    if !all_digits(int_word) || !all_digits(frac_word) {
        return slow(text, decimals);
    }

    // 6. Convert and scale. `frac × 10^(d − keep)` is below 10^d, so it can't overflow;
    //    `int × 10^d` can, and so can the final sum ("9.9" at 18 decimals is
    //    9×10^18 + 9×10^17 > i64::MAX). Both are checked.
    let int_value = eight_digits(int_word).cast_signed();
    let frac_value = eight_digits(frac_word).cast_signed() * POW10[d - keep];
    match int_value
        .checked_mul(POW10[d])
        .and_then(|v| v.checked_add(frac_value))
    {
        Some(raw) => Ok(raw),
        None => slow(text, decimals), // Overflow: the reference reports it
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    type Parser = fn(&[u8], u8) -> Result<i64, ParseFixedError>;

    /// `swar_padded` through the same signature: copy the text into a padded window
    /// filled with junk after the number, to prove the mask ignores it.
    fn padded(text: &[u8], d: u8) -> Result<i64, ParseFixedError> {
        let mut window = [b'9'; 16];
        let n = text.len().min(16);
        window[..n].copy_from_slice(&text[..n]);
        swar_padded(text, &window, d)
    }

    const PARSERS: [(&str, Parser); 3] =
        [("scalar", scalar), ("swar", swar), ("swar_padded", padded)];

    #[test]
    fn eight_digit_conversion() {
        assert_eq!(eight_digits(u64::from_le_bytes(*b"12345678")), 12_345_678);
        assert_eq!(eight_digits(u64::from_le_bytes(*b"00000000")), 0);
        assert_eq!(eight_digits(u64::from_le_bytes(*b"99999999")), 99_999_999);
        assert_eq!(eight_digits(u64::from_le_bytes(*b"00000105")), 105);
    }

    #[test]
    fn load_handles_every_length() {
        let text = b"0123456789ABCDEF";
        for n in 0..=16 {
            let (lo, hi) = load(&text[..n]);
            let mut expected = [0u8; 16];
            expected[..n].copy_from_slice(&text[..n]);
            let x = u128::from(lo) | u128::from(hi) << 64;
            assert_eq!(x, u128::from_le_bytes(expected), "n = {n}");
        }
    }

    #[test]
    fn digit_check_and_dot_search() {
        assert!(all_digits(ZEROS));
        assert!(all_digits(u64::from_le_bytes(*b"01234567")));
        for bad in [b'/', b':', b'.', b'-', b' ', 0x00, 0xFF, b'a'] {
            let mut bytes = *b"01234567";
            bytes[5] = bad;
            assert!(!all_digits(u64::from_le_bytes(bytes)), "byte {bad:#x}");
        }
        let (lo, _) = load(b"105123.40");
        assert_eq!(zero_bytes(lo ^ splat(b'.')).trailing_zeros() / 8, 6);
        assert_eq!(zero_bytes(load(b"12345").0 ^ splat(b'.')), 0);
        assert_eq!(
            right_align(u64::from_le_bytes(*b"105\0\0\0\0\0") & low_bytes(3), 3),
            u64::from_le_bytes(*b"00000105")
        );
    }

    #[test]
    fn exchange_formats() {
        // (text, decimals, expected raw)
        let cases: [(&[u8], u8, i64); 12] = [
            (b"105123.40", 1, 1_051_234), // Binance: 2 decimals sent, tick 0.1
            (b"105123.4", 1, 1_051_234),  // Bybit
            (b"105123", 1, 1_051_230),
            (b"0.001", 3, 1),
            (b"12.345", 3, 12_345),
            (b"1.500", 3, 1_500),
            (b"0", 3, 0),
            (b"0.000", 3, 0),
            (b"50000.10000000", 8, 5_000_010_000_000),
            (b"1234567890123456", 0, 1_234_567_890_123_456), // 16 digits, no point
            (b"123456789012345.6", 1, 1_234_567_890_123_456), // 17 bytes: scalar only
            (b"0.000000000000000001", 18, 1),                // 20 bytes: reference
        ];
        for (text, d, want) in cases {
            for (name, parse) in PARSERS {
                assert_eq!(
                    parse(text, d),
                    Ok(want),
                    "{name} {:?}",
                    core::str::from_utf8(text)
                );
            }
        }
    }

    #[test]
    fn errors_match_the_reference() {
        let bad: [&[u8]; 14] = [
            b"",
            b"-",
            b".5",
            b"5.",
            b"1.2.3",
            b"+1",
            b"1e5",
            b" 1",
            b"1,000",
            b"NaN",
            b"105123.45", // non-zero beyond 1 decimal
            b"-1.5",
            b"99999999999999999999",
            b"92233720368547758.08",
        ];
        for text in bad {
            for d in [0, 1, 2, 8, 18] {
                let want = ob_core::parse_decimal(text, d);
                for (name, parse) in PARSERS {
                    assert_eq!(
                        parse(text, d),
                        want,
                        "{name} {:?} d={d}",
                        core::str::from_utf8(text)
                    );
                }
            }
        }
    }

    #[test]
    fn spec_level_parsers_match_the_spec() {
        let spec = InstrumentSpec::from_steps("0.5", "0.002").unwrap();
        for text in ["100.5", "100.3", "100.50", "-3.0", "x"] {
            assert_eq!(
                parse_px(&spec, text.as_bytes()),
                spec.parse_px(text),
                "{text}"
            );
        }
        for text in ["1.004", "1.001", "-0.002", "0", "1.0040"] {
            assert_eq!(
                parse_qty(&spec, text.as_bytes()),
                spec.parse_qty(text),
                "{text}"
            );
        }
    }
}
