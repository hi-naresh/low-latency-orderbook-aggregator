//! Differential tests: every fast parser must return exactly what the reference parser
//! (`ob_core::parse_decimal`) returns, for every input and every scale, errors included.
//!
//! Two layers:
//! 1. **Exhaustive** over every string of up to 6 characters from an alphabet chosen to
//!    hit every branch (digits, `.`, `-`, a junk byte), at several scales.
//! 2. **Property-based** (proptest) over well-formed numbers of every length 1–24,
//!    near-miss strings, and completely arbitrary bytes, at every scale 0–18.
#![allow(clippy::unwrap_used)]

use ob_core::{InstrumentSpec, ParseFixedError, parse_decimal};
use ob_parse::decimal::{scalar, swar, swar_padded};
use proptest::prelude::*;

/// `swar_padded` with a window whose bytes after the number are junk (quotes, commas,
/// digits), as they would be inside a JSON message.
fn padded(text: &[u8], decimals: u8) -> Result<i64, ParseFixedError> {
    let mut window = *br#"","9"],["0.5"]]}"#;
    let n = text.len().min(16);
    window[..n].copy_from_slice(&text[..n]);
    swar_padded(text, &window, decimals)
}

fn agree(text: &[u8], decimals: u8) -> Result<(), String> {
    let want = parse_decimal(text, decimals);
    for (name, got) in [
        ("scalar", scalar(text, decimals)),
        ("swar", swar(text, decimals)),
        ("swar_padded", padded(text, decimals)),
    ] {
        if got != want {
            return Err(format!(
                "{name} disagrees on {:?} at {decimals} decimals: got {got:?}, reference {want:?}",
                String::from_utf8_lossy(text)
            ));
        }
    }
    Ok(())
}

#[test]
fn exhaustive_short_strings() {
    // '0' '1' '9' cover digit values and both ends of the digit range; '.' and '-' are
    // the only other legal characters; 'x' and '/' (just below '0') are junk.
    let alphabet = b"019.-x/";
    let mut checked = 0_u64;
    let mut text = Vec::with_capacity(6);
    for len in 0..=6_u32 {
        for code in 0..alphabet.len().pow(len) {
            text.clear();
            let mut c = code;
            for _ in 0..len {
                text.push(alphabet[c % alphabet.len()]);
                c /= alphabet.len();
            }
            for decimals in [0, 1, 2, 3, 8, 18] {
                agree(&text, decimals).unwrap();
                checked += 1;
            }
        }
    }
    assert_eq!(checked, 6 * (0..=6).map(|l| 7_u64.pow(l)).sum::<u64>());
}

#[test]
fn every_length_from_1_to_20_with_and_without_a_point() {
    // Long runs of 9s at every point position stress the 16-byte limit and overflow.
    for len in 1..=20 {
        for dot in 0..=len {
            let mut text = vec![b'9'; len];
            if dot < len {
                text[dot] = b'.';
            }
            for decimals in 0..=18 {
                agree(&text, decimals).unwrap();
            }
        }
    }
}

/// A well-formed decimal: optional '-', 1–20 integer digits, optional fraction of 1–20
/// digits, often ending in zeros (as exchanges send).
fn well_formed() -> impl Strategy<Value = Vec<u8>> {
    (
        any::<bool>(),
        prop::collection::vec(b'0'..=b'9', 1..20),
        prop::option::of(prop::collection::vec(b'0'..=b'9', 1..20)),
        0_usize..4,
    )
        .prop_map(|(neg, int, frac, zeros)| {
            let mut t = Vec::new();
            if neg && int.len() % 7 == 0 {
                t.push(b'-'); // negatives are rare in book data; keep them rare here too
            }
            t.extend(int);
            if let Some(mut f) = frac {
                f.extend(std::iter::repeat_n(b'0', zeros));
                t.push(b'.');
                t.extend(f);
            }
            t
        })
}

/// Strings that are almost valid: built from the legal characters plus a few junk ones.
fn near_miss() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(
        prop::sample::select(b"0123456789..-+eE x/:\0".to_vec()),
        0..24,
    )
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 20_000, ..ProptestConfig::default() })]

    #[test]
    fn fast_parsers_match_reference_on_well_formed(text in well_formed(), d in 0_u8..=18) {
        agree(&text, d).map_err(TestCaseError::fail)?;
    }

    #[test]
    fn fast_parsers_match_reference_on_near_misses(text in near_miss(), d in 0_u8..=18) {
        agree(&text, d).map_err(TestCaseError::fail)?;
    }

    #[test]
    fn fast_parsers_match_reference_on_arbitrary_bytes(
        text in prop::collection::vec(any::<u8>(), 0..24),
        d in 0_u8..=18,
    ) {
        agree(&text, d).map_err(TestCaseError::fail)?;
    }

    // The hot-path calls (parse + tick/lot check) match the spec's own methods.
    #[test]
    fn spec_level_calls_match(text in well_formed(), tick in 1_i64..50, lot in 1_i64..50) {
        let spec = InstrumentSpec::new(2, 3, tick, lot).unwrap();
        prop_assert_eq!(ob_parse::parse_px(&spec, &text), spec.parse_px(&text));
        prop_assert_eq!(ob_parse::parse_qty(&spec, &text), spec.parse_qty(&text));
    }
}

#[test]
fn error_type_is_unchanged() {
    // The fast path must report the same error kinds as the reference.
    assert_eq!(
        swar(b"105123.45", 1),
        Err(ParseFixedError::ExceedsPrecision)
    );
    assert_eq!(swar(b"1e5", 1), Err(ParseFixedError::InvalidChar));
    assert_eq!(swar(b"5.", 1), Err(ParseFixedError::MissingDigits));
    assert_eq!(
        swar(b"99999999999999999999", 1),
        Err(ParseFixedError::Overflow)
    );
}
