//! Fuzz target: the Bybit parser must never panic, and on every comparable input must
//! agree exactly with the serde_json oracle (the same oracle the proptest suite uses).
#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../../tests/support/bybit_oracle.rs"]
#[allow(dead_code)]
mod oracle;
#[path = "../../tests/support/comparable.rs"]
mod comparable;

fuzz_target!(|data: &[u8]| {
    let spec = ob_core::InstrumentSpec::from_steps("0.1", "0.001").unwrap();
    let topic = "orderbook.50.BTCUSDT";
    let got = oracle::fast(data, &spec, topic, "BTCUSDT");
    if comparable::comparable(data) {
        assert_eq!(got, oracle::oracle(data, &spec, topic, "BTCUSDT"));
    }
});
