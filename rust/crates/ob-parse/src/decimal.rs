//! Fast decimal parsing for exchange numbers such as `"105123.40"` or `"0.001"`.
//!
//! Exchanges send every price and quantity as decimal text. [`parse_px`] and
//! [`parse_qty`] turn it into the same raw integers as the reference parser
//! [`ob_core::parse_decimal`] (`"105123.40"` at 1 decimal → `1051234`), in one pass of
//! a few operations per byte.
//!
//! **Correctness contract.** For every input and every scale, the result is exactly the
//! reference's, errors included. The fast path handles only the common valid shapes;
//! anything unusual (a sign, more than 18 digits, an invalid byte, overflow) goes to the
//! reference parser on a `#[cold]` path, which produces the precise error.
//! `tests/differential.rs` checks the contract on generated and mutated inputs.

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
#[inline]
pub fn parse_px(spec: &InstrumentSpec, text: &[u8]) -> Result<Px, ParseFixedError> {
    spec.px_from_raw(parse(text, spec.px_decimals())?)
}

/// The hot-path quantity parser: fast decimal parse + sign and lot checks.
/// Identical results to [`InstrumentSpec::parse_qty`].
#[inline]
pub fn parse_qty(spec: &InstrumentSpec, text: &[u8]) -> Result<Qty, ParseFixedError> {
    spec.qty_from_raw(parse(text, spec.qty_decimals())?)
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

/// Parses one byte at a time, in a single pass: a subtract, a compare and a multiply-add
/// per byte. At most 18 digits are accepted (so the `u64` accumulator can't overflow);
/// longer inputs fall back to the reference.
#[inline]
pub fn parse(text: &[u8], decimals: u8) -> Result<i64, ParseFixedError> {
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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

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
            (b"123456789012345.6", 1, 1_234_567_890_123_456), // 17 bytes
            (b"0.000000000000000001", 18, 1),                // 20 bytes: reference
        ];
        for (text, d, want) in cases {
            assert_eq!(parse(text, d), Ok(want), "{:?}", core::str::from_utf8(text));
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
                assert_eq!(
                    parse(text, d),
                    want,
                    "{:?} d={d}",
                    core::str::from_utf8(text)
                );
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
