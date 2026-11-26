//! Fuzz target: every fast decimal parser must equal the reference on every input.
//! The first byte picks the scale (0..=18); the rest is the number text.
#![no_main]

use libfuzzer_sys::fuzz_target;
use ob_parse::decimal::{scalar, swar, swar_padded};

fuzz_target!(|data: &[u8]| {
    let Some((&d, text)) = data.split_first() else { return };
    let d = d % 19;
    let want = ob_core::parse_decimal(text, d);
    assert_eq!(scalar(text, d), want);
    assert_eq!(swar(text, d), want);
    let mut window = *b"\",\"9\"],[\"0.5\"]]}";
    let n = text.len().min(16);
    window[..n].copy_from_slice(&text[..n]);
    assert_eq!(swar_padded(text, &window, d), want);
});
