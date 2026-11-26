//! Per-instrument scale, tick/lot validation, and conversion to and from decimal text.
//!
//! An [`InstrumentSpec`] says what one raw unit of a [`Px`] or [`Qty`] means for one
//! instrument, plus the smallest valid price step (tick) and quantity step (lot).
//!
//! ```
//! use ob_core::InstrumentSpec;
//! // BTCUSDT perpetual on Binance or Bybit: tick 0.1, lot 0.001.
//! let btc = InstrumentSpec::from_steps("0.1", "0.001").unwrap();
//! let px = btc.parse_px("50000.1").unwrap();
//! assert_eq!(px.raw(), 500_001);
//! assert_eq!(btc.fmt_px(px).to_string(), "50000.1");
//! ```
//!
//! **Several venues, one instrument.** The aggregator compares prices from different
//! venues directly, so all venues of one instrument must share a scale. Build each
//! venue's spec with [`InstrumentSpec::from_steps`], take the largest decimals of any of
//! them, and move each spec to that common scale with [`InstrumentSpec::rescaled`]. Each
//! venue keeps its own tick and lot, which are still validated on every value.

use core::fmt;

use crate::divisor::Divisor;
use crate::{Notional, Px, Qty};

/// The most decimal places a spec may use (10^18 is the largest power of ten in `i64`).
pub const MAX_DECIMALS: u8 = 18;

/// `POW10[n] = 10^n` for every allowed decimals value.
const POW10: [i64; MAX_DECIMALS as usize + 1] = {
    let mut table = [1_i64; MAX_DECIMALS as usize + 1];
    let mut i = 1;
    while i < table.len() {
        table[i] = table[i - 1] * 10;
        i += 1;
    }
    table
};

/// Scale and step sizes for one instrument (on one venue, or shared across venues).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct InstrumentSpec {
    px_decimals: u8,
    qty_decimals: u8,
    tick: i64,
    lot: i64,
    // Precomputed "is it a multiple?" tests, so validation needs no division instruction.
    tick_div: Divisor,
    lot_div: Divisor,
}

impl InstrumentSpec {
    /// Builds a spec from decimals and raw step sizes (already in those decimals).
    ///
    /// `tick` and `lot` must be positive. Example: BTCUSDT is
    /// `InstrumentSpec::new(1, 3, 1, 1)`: prices in units of 0.1, quantities in units of
    /// 0.001, and every unit is a valid step.
    pub const fn new(
        px_decimals: u8,
        qty_decimals: u8,
        tick: i64,
        lot: i64,
    ) -> Result<InstrumentSpec, SpecError> {
        if px_decimals > MAX_DECIMALS || qty_decimals > MAX_DECIMALS {
            return Err(SpecError::TooManyDecimals);
        }
        if tick <= 0 || lot <= 0 {
            return Err(SpecError::NonPositiveStep);
        }
        Ok(InstrumentSpec {
            px_decimals,
            qty_decimals,
            tick,
            lot,
            tick_div: Divisor::new(tick.unsigned_abs()),
            lot_div: Divisor::new(lot.unsigned_abs()),
        })
    }

    /// Builds a spec from the step sizes as the venue publishes them, e.g. Binance
    /// `exchangeInfo` `tickSize = "0.10"`, `stepSize = "0.001"`.
    ///
    /// The decimals are the steps' own precision (trailing zeros ignored), which is the
    /// coarsest scale that represents every valid value exactly and so gives the most
    /// range.
    pub fn from_steps(tick: &str, lot: &str) -> Result<InstrumentSpec, SpecError> {
        let px_decimals = significant_decimals(tick.as_bytes())?;
        let qty_decimals = significant_decimals(lot.as_bytes())?;
        let tick_raw = parse_decimal(tick.as_bytes(), px_decimals).map_err(SpecError::BadStep)?;
        let lot_raw = parse_decimal(lot.as_bytes(), qty_decimals).map_err(SpecError::BadStep)?;
        InstrumentSpec::new(px_decimals, qty_decimals, tick_raw, lot_raw)
    }

    /// The same spec expressed at finer decimals (to share a scale with another venue).
    ///
    /// Steps are converted exactly. Making a scale coarser could lose precision, so it is
    /// refused with [`SpecError::CannotCoarsen`].
    pub const fn rescaled(
        self,
        px_decimals: u8,
        qty_decimals: u8,
    ) -> Result<InstrumentSpec, SpecError> {
        if px_decimals > MAX_DECIMALS || qty_decimals > MAX_DECIMALS {
            return Err(SpecError::TooManyDecimals);
        }
        if px_decimals < self.px_decimals || qty_decimals < self.qty_decimals {
            return Err(SpecError::CannotCoarsen);
        }
        let px_factor = POW10[(px_decimals - self.px_decimals) as usize];
        let qty_factor = POW10[(qty_decimals - self.qty_decimals) as usize];
        match (
            self.tick.checked_mul(px_factor),
            self.lot.checked_mul(qty_factor),
        ) {
            (Some(tick), Some(lot)) => InstrumentSpec::new(px_decimals, qty_decimals, tick, lot),
            _ => Err(SpecError::RangeTooSmall),
        }
    }

    /// Checks that a plausible maximum price and quantity fit in this scale.
    ///
    /// Call it once at start-up with generous limits (e.g. 100× today's price), so an
    /// instrument whose values could overflow is refused before trading, not mid-session.
    pub fn ensure_headroom(&self, max_px: &str, max_qty: &str) -> Result<(), SpecError> {
        let fits = |text: &str, decimals| match parse_decimal(text.as_bytes(), decimals) {
            Ok(_) => Ok(()),
            Err(ParseFixedError::Overflow) => Err(SpecError::RangeTooSmall),
            Err(e) => Err(SpecError::BadStep(e)),
        };
        fits(max_px, self.px_decimals)?;
        fits(max_qty, self.qty_decimals)
    }

    /// Decimal places of a raw [`Px`].
    pub const fn px_decimals(&self) -> u8 {
        self.px_decimals
    }

    /// Decimal places of a raw [`Qty`].
    pub const fn qty_decimals(&self) -> u8 {
        self.qty_decimals
    }

    /// Decimal places of a raw [`Notional`] (price decimals + quantity decimals).
    pub const fn notional_decimals(&self) -> u8 {
        self.px_decimals + self.qty_decimals
    }

    /// The smallest price step, as a raw [`Px`].
    pub const fn tick(&self) -> Px {
        Px::from_raw(self.tick)
    }

    /// The smallest quantity step, as a raw [`Qty`].
    pub const fn lot(&self) -> Qty {
        // `new` guarantees lot > 0.
        match Qty::from_raw(self.lot) {
            Some(q) => q,
            None => Qty::ZERO,
        }
    }

    /// Parses decimal text into a [`Px`], exactly, and checks it is a multiple of the tick.
    ///
    /// Uses the clear reference parser ([`parse_decimal`]). The hot path parses with the
    /// faster `ob-parse` crate and validates with [`InstrumentSpec::px_from_raw`]; both
    /// give identical results.
    pub fn parse_px(&self, text: impl AsRef<[u8]>) -> Result<Px, ParseFixedError> {
        self.px_from_raw(parse_decimal(text.as_ref(), self.px_decimals)?)
    }

    /// Parses decimal text into a [`Qty`], exactly, and checks it is a non-negative
    /// multiple of the lot. Zero is valid (it means "remove this level").
    pub fn parse_qty(&self, text: impl AsRef<[u8]>) -> Result<Qty, ParseFixedError> {
        self.qty_from_raw(parse_decimal(text.as_ref(), self.qty_decimals)?)
    }

    /// Validates a raw price (already at this spec's decimals) against the tick.
    #[inline]
    pub fn px_from_raw(&self, raw: i64) -> Result<Px, ParseFixedError> {
        if !self.tick_div.divides(raw) {
            return Err(ParseFixedError::OffTick);
        }
        Ok(Px::from_raw(raw))
    }

    /// Validates a raw quantity (already at this spec's decimals): non-negative and a
    /// multiple of the lot.
    #[inline]
    pub fn qty_from_raw(&self, raw: i64) -> Result<Qty, ParseFixedError> {
        let qty = Qty::from_raw(raw).ok_or(ParseFixedError::Negative)?;
        if !self.lot_div.divides(raw) {
            return Err(ParseFixedError::OffLot);
        }
        Ok(qty)
    }

    /// A [`Px`] as decimal text: `format!("{}", spec.fmt_px(px))`.
    pub fn fmt_px(&self, px: Px) -> FixedDisplay {
        FixedDisplay {
            raw: i128::from(px.raw()),
            decimals: self.px_decimals,
        }
    }

    /// A [`Qty`] as decimal text.
    pub fn fmt_qty(&self, qty: Qty) -> FixedDisplay {
        FixedDisplay {
            raw: i128::from(qty.raw()),
            decimals: self.qty_decimals,
        }
    }

    /// A [`Notional`] as decimal text.
    pub fn fmt_notional(&self, notional: Notional) -> FixedDisplay {
        FixedDisplay {
            raw: notional.raw(),
            decimals: self.notional_decimals(),
        }
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Why a decimal value from outside (an exchange message, a config file) was rejected.
///
/// One byte, `Copy`, no heap: cheap enough to return from the hot path. The layer that
/// called the parser adds the context (venue, field, position in the message).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ParseFixedError {
    /// The input was empty, or was only a sign.
    Empty,
    /// A character other than a digit, one leading `-`, or one `.`.
    InvalidChar,
    /// A `.` with no digit before or after it, such as `".5"` or `"5."`.
    MissingDigits,
    /// A non-zero digit beyond the instrument's decimals. Rejected rather than rounded:
    /// silently changing a price is worse than refusing it.
    ExceedsPrecision,
    /// The value does not fit in 64 bits at the instrument's scale.
    Overflow,
    /// A negative quantity.
    Negative,
    /// A price that is not a multiple of the instrument's tick.
    OffTick,
    /// A quantity that is not a multiple of the instrument's lot.
    OffLot,
}

/// What kind of problem a [`ParseFixedError`] points to, which decides how to react.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ErrorClass {
    /// The text is not a valid number at all: corrupt or truncated data.
    Malformed,
    /// A valid number our instrument settings can't hold: the venue changed its tick or
    /// lot, or our spec is wrong. Reload the instrument info and alert.
    SpecMismatch,
    /// A well-formed value that makes no sense, such as a negative quantity.
    Invalid,
}

impl ParseFixedError {
    /// Classifies the error so callers can pick a policy without matching every variant.
    pub const fn class(self) -> ErrorClass {
        match self {
            ParseFixedError::Empty | ParseFixedError::InvalidChar => ErrorClass::Malformed,
            ParseFixedError::MissingDigits => ErrorClass::Malformed,
            ParseFixedError::ExceedsPrecision | ParseFixedError::Overflow => {
                ErrorClass::SpecMismatch
            }
            ParseFixedError::OffTick | ParseFixedError::OffLot => ErrorClass::SpecMismatch,
            ParseFixedError::Negative => ErrorClass::Invalid,
        }
    }
}

impl fmt::Display for ParseFixedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            ParseFixedError::Empty => "empty decimal",
            ParseFixedError::InvalidChar => "invalid character in decimal",
            ParseFixedError::MissingDigits => "decimal point without digits on both sides",
            ParseFixedError::ExceedsPrecision => "more decimal places than the instrument allows",
            ParseFixedError::Overflow => "decimal out of range for the instrument's scale",
            ParseFixedError::Negative => "negative quantity",
            ParseFixedError::OffTick => "price is not a multiple of the tick size",
            ParseFixedError::OffLot => "quantity is not a multiple of the lot size",
        })
    }
}

impl core::error::Error for ParseFixedError {}

/// Why an [`InstrumentSpec`] could not be built. Happens at start-up or on reload,
/// never on the hot path.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SpecError {
    /// More than [`MAX_DECIMALS`] decimal places.
    TooManyDecimals,
    /// A tick or lot of zero or less.
    NonPositiveStep,
    /// A step or limit that is not a valid decimal.
    BadStep(ParseFixedError),
    /// The scale can't represent the required values (or the rescaled steps) in `i64`.
    RangeTooSmall,
    /// Asked to move a spec to fewer decimals, which could lose precision.
    CannotCoarsen,
}

impl fmt::Display for SpecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SpecError::TooManyDecimals => write!(f, "more than {MAX_DECIMALS} decimal places"),
            SpecError::NonPositiveStep => f.write_str("tick and lot must be positive"),
            SpecError::BadStep(e) => write!(f, "invalid step or limit: {e}"),
            SpecError::RangeTooSmall => f.write_str("scale cannot represent the required range"),
            SpecError::CannotCoarsen => f.write_str("cannot rescale to fewer decimals"),
        }
    }
}

impl core::error::Error for SpecError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            SpecError::BadStep(e) => Some(e),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// Parses `[-]digits[.digits]` into a raw integer at `decimals` places, exactly
/// (`parse_decimal(b"50000.1", 1) == Ok(500001)`).
///
/// This is the clear, obviously-correct **reference** parser: used at the API edge, and
/// the oracle the fast parsers in `ob-parse` must match exactly, errors included. It
/// never rounds. Digits beyond `decimals` must be zeros.
///
/// # Panics
/// If `decimals > MAX_DECIMALS` (a caller bug: every [`InstrumentSpec`] is within range).
#[track_caller]
pub fn parse_decimal(s: &[u8], decimals: u8) -> Result<i64, ParseFixedError> {
    assert!(
        decimals <= MAX_DECIMALS,
        "decimals {decimals} > {MAX_DECIMALS}"
    );
    let decimals = usize::from(decimals);
    let (negative, digits) = match s {
        [b'-', rest @ ..] => (true, rest),
        _ => (false, s),
    };
    if digits.is_empty() {
        return Err(ParseFixedError::Empty);
    }

    let (int_part, frac_part) = match digits.iter().position(|&b| b == b'.') {
        Some(dot) => (&digits[..dot], Some(&digits[dot + 1..])),
        None => (digits, None),
    };
    if int_part.is_empty() || frac_part.is_some_and(<[u8]>::is_empty) {
        return Err(ParseFixedError::MissingDigits);
    }

    let mut int_value: i64 = 0;
    for &b in int_part {
        let d = digit(b)?;
        int_value = int_value
            .checked_mul(10)
            .and_then(|v| v.checked_add(d))
            .ok_or(ParseFixedError::Overflow)?;
    }

    // The first `decimals` fractional digits are significant; any further must be zeros.
    // Digit i (0-based) is worth 10^(decimals - 1 - i) raw units.
    let mut frac_value: i64 = 0;
    for (i, &b) in frac_part.unwrap_or(&[]).iter().enumerate() {
        let d = digit(b)?;
        if i < decimals {
            frac_value += d * POW10[decimals - 1 - i];
        } else if d != 0 {
            return Err(ParseFixedError::ExceedsPrecision);
        }
    }

    let magnitude = int_value
        .checked_mul(POW10[decimals])
        .and_then(|v| v.checked_add(frac_value))
        .ok_or(ParseFixedError::Overflow)?;
    Ok(if negative { -magnitude } else { magnitude })
}

#[inline]
fn digit(b: u8) -> Result<i64, ParseFixedError> {
    if b.is_ascii_digit() {
        Ok(i64::from(b - b'0'))
    } else {
        Err(ParseFixedError::InvalidChar)
    }
}

/// Number of fractional digits in a step size, ignoring trailing zeros
/// (`"0.10"` → 1, `"1"` → 0, `"0.001"` → 3).
fn significant_decimals(step: &[u8]) -> Result<u8, SpecError> {
    let frac = match step.iter().position(|&b| b == b'.') {
        Some(dot) => &step[dot + 1..],
        None => &[],
    };
    let significant = frac
        .iter()
        .rposition(|&b| b != b'0')
        .map_or(0, |last| last + 1);
    match u8::try_from(significant) {
        Ok(d) if d <= MAX_DECIMALS => Ok(d),
        _ => Err(SpecError::TooManyDecimals),
    }
}

// ---------------------------------------------------------------------------
// Display
// ---------------------------------------------------------------------------

/// A raw value plus its decimals, printable with `{}`. Made by
/// [`InstrumentSpec::fmt_px`], [`InstrumentSpec::fmt_qty`], [`InstrumentSpec::fmt_notional`].
#[derive(Clone, Copy, Debug)]
pub struct FixedDisplay {
    raw: i128,
    decimals: u8,
}

impl fmt::Display for FixedDisplay {
    /// Plain decimal text with trailing zeros removed (`500001` at 1 decimal →
    /// `"50000.1"`). Written into a stack buffer, so it needs no heap and still honours
    /// width/alignment flags such as `{:>12}`.
    // `(x % 10) as u8` is always 0..=9, so the narrowing casts cannot truncate.
    #[allow(clippy::cast_possible_truncation)]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Largest i128 has 39 digits; plus sign and point is 41, so 48 bytes always fit.
        let mut buf = [0u8; 48];
        let mut pos = buf.len();
        let mut push = |b: u8| {
            pos -= 1;
            buf[pos] = b;
        };

        // decimals <= 36 (two specs' worth), and 10^36 fits in u128.
        let scale = 10_u128.pow(u32::from(self.decimals));
        let magnitude = self.raw.unsigned_abs();
        let (mut int_part, mut frac_part) = (magnitude / scale, magnitude % scale);

        if frac_part != 0 {
            // Drop trailing zeros, then write the remaining fractional digits backwards.
            let mut width = self.decimals;
            while frac_part % 10 == 0 {
                frac_part /= 10;
                width -= 1;
            }
            for _ in 0..width {
                push(b'0' + (frac_part % 10) as u8);
                frac_part /= 10;
            }
            push(b'.');
        }
        loop {
            push(b'0' + (int_part % 10) as u8);
            int_part /= 10;
            if int_part == 0 {
                break;
            }
        }
        if self.raw < 0 {
            push(b'-');
        }

        // Every byte written is an ASCII digit, '.' or '-', so this cannot fail.
        let text = core::str::from_utf8(&buf[pos..]).map_err(|_| fmt::Error)?;
        f.pad(text)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::cast_possible_wrap)]
mod tests {
    use super::*;

    fn btc() -> InstrumentSpec {
        InstrumentSpec::from_steps("0.1", "0.001").unwrap()
    }

    #[test]
    fn error_is_one_byte_and_results_stay_small() {
        assert_eq!(size_of::<ParseFixedError>(), 1);
        assert_eq!(size_of::<Result<Px, ParseFixedError>>(), 16);
    }

    #[test]
    fn steps_define_the_scale() {
        let s = btc();
        assert_eq!((s.px_decimals(), s.qty_decimals()), (1, 3));
        assert_eq!((s.tick().raw(), s.lot().raw()), (1, 1));
        // Trailing zeros in the published step don't add decimals.
        assert_eq!(InstrumentSpec::from_steps("0.10", "0.0010").unwrap(), s);
        // Whole-number steps (e.g. a contract traded in lots of 5) need no decimals.
        let s = InstrumentSpec::from_steps("5", "100").unwrap();
        assert_eq!(
            (s.px_decimals(), s.tick().raw(), s.lot().raw()),
            (0, 5, 100)
        );
    }

    #[test]
    fn parses_exchange_values_exactly() {
        let s = btc();
        assert_eq!(s.parse_px("50000.1").unwrap().raw(), 500_001);
        assert_eq!(s.parse_px(b"50000").unwrap().raw(), 500_000);
        assert_eq!(s.parse_px("007.50").unwrap().raw(), 75);
        assert_eq!(s.parse_px("-1.5").unwrap().raw(), -15);
        assert_eq!(s.parse_qty("0.001").unwrap().raw(), 1);
        assert_eq!(s.parse_qty("12.345").unwrap().raw(), 12_345);
        assert_eq!(s.parse_qty("0").unwrap(), Qty::ZERO);
    }

    #[test]
    fn equal_values_with_different_scales_are_equal() {
        // The BigDecimal trap: in Java, new BigDecimal("50000.0").equals(new BigDecimal("50000.00"))
        // is false, so a HashMap-based book can hold two levels for one price.
        let s = btc();
        assert_eq!(s.parse_px("50000.0"), s.parse_px("50000.00"));
        assert_eq!(s.parse_px("50000.0"), s.parse_px("50000"));
        assert_eq!(s.parse_qty("0.000"), Ok(Qty::ZERO));
        assert_eq!(s.parse_px("-0"), Ok(Px::ZERO));
    }

    #[test]
    fn precision_beyond_the_scale_is_rejected_not_rounded() {
        let s = btc();
        assert_eq!(s.parse_px("50000.100").unwrap().raw(), 500_001); // extra zeros are fine
        assert_eq!(
            s.parse_px("50000.15"),
            Err(ParseFixedError::ExceedsPrecision)
        );
        assert_eq!(
            s.parse_qty("0.0015"),
            Err(ParseFixedError::ExceedsPrecision)
        );
    }

    #[test]
    fn off_tick_and_off_lot_values_are_rejected() {
        let s = InstrumentSpec::from_steps("0.5", "0.002").unwrap();
        assert_eq!(s.parse_px("100.5").unwrap().raw(), 1005);
        assert_eq!(s.parse_px("100.3"), Err(ParseFixedError::OffTick));
        assert_eq!(s.parse_qty("1.004").unwrap().raw(), 1004);
        assert_eq!(s.parse_qty("1.001"), Err(ParseFixedError::OffLot));
    }

    #[test]
    fn rejects_malformed_input() {
        use ParseFixedError::*;
        let cases = [
            ("", Empty),
            ("-", Empty),
            (".5", MissingDigits),
            ("5.", MissingDigits),
            ("-.5", MissingDigits),
            ("1.2.3", InvalidChar),
            ("+1", InvalidChar),
            ("1e5", InvalidChar),
            (" 1", InvalidChar),
            ("1,000", InvalidChar),
            ("--1", InvalidChar),
            ("NaN", InvalidChar),
        ];
        for (input, expected) in cases {
            assert_eq!(btc().parse_px(input), Err(expected), "input {input:?}");
            assert_eq!(expected.class(), ErrorClass::Malformed);
        }
    }

    #[test]
    fn overflow_is_detected_at_the_exact_boundary() {
        // At 1 decimal the largest price is i64::MAX / 10 = 922337203685477580.7
        let s = InstrumentSpec::new(1, 3, 1, 1).unwrap();
        assert_eq!(s.parse_px("922337203685477580.7").unwrap().raw(), i64::MAX);
        assert_eq!(
            s.parse_px("922337203685477580.8"),
            Err(ParseFixedError::Overflow)
        );
        assert_eq!(
            s.parse_px("99999999999999999999"),
            Err(ParseFixedError::Overflow)
        );
    }

    #[test]
    fn negative_quantities_are_rejected() {
        assert_eq!(btc().parse_qty("-0.001"), Err(ParseFixedError::Negative));
        assert_eq!(ParseFixedError::Negative.class(), ErrorClass::Invalid);
    }

    #[test]
    fn errors_are_classified_for_policy() {
        use ParseFixedError::*;
        for e in [ExceedsPrecision, Overflow, OffTick, OffLot] {
            assert_eq!(e.class(), ErrorClass::SpecMismatch, "{e:?}");
        }
    }

    #[test]
    fn a_per_instrument_scale_fits_values_a_global_one_cannot() {
        // A meme coin: tick 0.00000001, quantities in whole coins, sizes in the trillions.
        let meme = InstrumentSpec::from_steps("0.00000001", "1").unwrap();
        assert_eq!(
            meme.parse_qty("5000000000000").unwrap().raw(),
            5_000_000_000_000
        );
        meme.ensure_headroom("1", "1000000000000000").unwrap();
        // The old design (every quantity at 8 decimals) overflows on the same size.
        let global8 = InstrumentSpec::new(8, 8, 1, 1).unwrap();
        assert_eq!(
            global8.parse_qty("5000000000000"),
            Err(ParseFixedError::Overflow)
        );
        // An 18-decimal token quantity works where 8 decimals would reject it.
        let token = InstrumentSpec::from_steps("0.01", "0.000000000000000001").unwrap();
        assert_eq!(
            token.parse_qty("1.000000000000000001").unwrap().raw(),
            1_000_000_000_000_000_001
        );
    }

    #[test]
    fn headroom_check_refuses_a_scale_that_is_too_small() {
        btc().ensure_headroom("10000000", "1000000").unwrap();
        let fine = InstrumentSpec::from_steps("0.000000000000000001", "1").unwrap();
        assert_eq!(
            fine.ensure_headroom("100", "1"),
            Err(SpecError::RangeTooSmall)
        );
    }

    #[test]
    fn venues_with_different_ticks_share_one_scale() {
        // Venue A quotes in 0.1, venue B in 0.01: compare them at 2 decimals.
        let a = InstrumentSpec::from_steps("0.1", "0.001").unwrap();
        let b = InstrumentSpec::from_steps("0.01", "0.0001").unwrap();
        let (pd, qd) = (
            a.px_decimals().max(b.px_decimals()),
            a.qty_decimals().max(b.qty_decimals()),
        );
        let (a, b) = (a.rescaled(pd, qd).unwrap(), b.rescaled(pd, qd).unwrap());
        assert_eq!((a.tick().raw(), b.tick().raw()), (10, 1));
        let pa = a.parse_px("50000.1").unwrap();
        let pb = b.parse_px("50000.11").unwrap();
        assert!(pb > pa); // directly comparable
        // Venue A's own tick is still enforced at the shared scale.
        assert_eq!(a.parse_px("50000.11"), Err(ParseFixedError::OffTick));
        assert_eq!(a.rescaled(1, 4), Err(SpecError::CannotCoarsen));
    }

    #[test]
    fn invalid_specs_are_refused() {
        assert_eq!(
            InstrumentSpec::new(19, 0, 1, 1),
            Err(SpecError::TooManyDecimals)
        );
        assert_eq!(
            InstrumentSpec::new(1, 1, 0, 1),
            Err(SpecError::NonPositiveStep)
        );
        assert_eq!(
            InstrumentSpec::from_steps("0", "1"),
            Err(SpecError::NonPositiveStep)
        );
        assert_eq!(
            InstrumentSpec::from_steps("0.0000000000000000001", "1"),
            Err(SpecError::TooManyDecimals)
        );
        assert_eq!(
            InstrumentSpec::from_steps("abc", "1"),
            Err(SpecError::BadStep(ParseFixedError::InvalidChar))
        );
        let big = InstrumentSpec::new(0, 0, i64::MAX, 1).unwrap();
        assert_eq!(big.rescaled(1, 0), Err(SpecError::RangeTooSmall));
    }

    #[test]
    fn display_is_plain_and_trims_trailing_zeros() {
        let s = btc();
        assert_eq!(s.fmt_px(Px::from_raw(500_001)).to_string(), "50000.1");
        assert_eq!(s.fmt_px(Px::from_raw(500_000)).to_string(), "50000");
        assert_eq!(s.fmt_px(Px::from_raw(-5)).to_string(), "-0.5");
        assert_eq!(s.fmt_px(Px::ZERO).to_string(), "0");
        assert_eq!(s.fmt_qty(Qty::from_raw(1).unwrap()).to_string(), "0.001");
        assert_eq!(
            format!("[{:>8}]", s.fmt_qty(Qty::from_raw(1500).unwrap())),
            "[     1.5]"
        );
        let s8 = InstrumentSpec::new(8, 8, 1, 1).unwrap();
        assert_eq!(
            s8.fmt_px(Px::from_raw(i64::MIN)).to_string(),
            "-92233720368.54775808"
        );
    }

    #[test]
    fn notional_uses_combined_decimals() {
        let s = btc();
        let n = s
            .parse_px("50000.1")
            .unwrap()
            .notional(s.parse_qty("1.234").unwrap());
        assert_eq!(s.notional_decimals(), 4);
        assert_eq!(s.fmt_notional(n).to_string(), "61700.1234");
    }

    #[test]
    fn display_then_parse_round_trips_at_every_scale() {
        // Deterministic pseudo-random sweep (xorshift); step 4 adds proper property tests.
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        for decimals in 0..=MAX_DECIMALS {
            let s = InstrumentSpec::new(decimals, decimals, 1, 1).unwrap();
            for _ in 0..(if cfg!(miri) { 30 } else { 10_000 }) {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                let raw = x as i64 >> (x % 40); // spread values over many magnitudes
                let p = Px::from_raw(raw);
                assert_eq!(
                    s.parse_px(s.fmt_px(p).to_string()),
                    Ok(p),
                    "raw {raw} at {decimals}"
                );
            }
        }
    }
}
