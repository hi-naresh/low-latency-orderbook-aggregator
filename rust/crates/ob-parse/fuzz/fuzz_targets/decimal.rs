//! Fuzz target: the fast decimal parser must equal the reference on every input.
//! The first byte picks the scale (0..=18); the rest is the number text.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Some((&d, text)) = data.split_first() else { return };
    let d = d % 19;
    assert_eq!(ob_parse::decimal::parse(text, d), ob_core::parse_decimal(text, d));
});
