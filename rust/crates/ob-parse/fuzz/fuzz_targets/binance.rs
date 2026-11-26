//! Fuzz target: the Binance parser must never panic, and on every comparable input must
//! agree exactly with the serde_json oracle (the same oracle the proptest suite uses).
#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../../tests/support/binance_oracle.rs"]
#[allow(dead_code)]
mod oracle;
#[path = "../../tests/support/comparable.rs"]
mod comparable;

fuzz_target!(|data: &[u8]| {
    let spec = ob_core::InstrumentSpec::from_steps("0.1", "0.001").unwrap();
    let got = oracle::fast(data, &spec, "BTCUSDT");
    if comparable::comparable(data) {
        assert_eq!(got, oracle::oracle(data, &spec, "BTCUSDT"));
    }
});
