//! The REST depth snapshot parser (step 19) against a serde_json oracle: the recorded
//! live snapshot (1000 levels a side), mutations of it, and hand-written edge cases.
#![allow(clippy::unwrap_used)]

#[path = "support/comparable.rs"]
mod comparable;
#[path = "support/mutate.rs"]
mod mutate;

use comparable::comparable;
use mutate::{mutate, mutation};
use ob_core::{InstrumentSpec, Level, Side};
use ob_parse::binance::{BinanceDepthParser, SnapshotHeader};
use proptest::prelude::*;
use serde_json::Value;

const SNAPSHOT: &str = include_str!("fixtures/binance_rest_depth1000.json");

fn spec() -> InstrumentSpec {
    InstrumentSpec::from_steps("0.1", "0.001").unwrap()
}

type Parsed = Option<(SnapshotHeader, Vec<Level>, Vec<Level>)>;

fn fast(msg: &[u8]) -> Parsed {
    let (mut bids, mut asks) = (Vec::new(), Vec::new());
    let h = BinanceDepthParser::new(spec(), b"BTCUSDT")
        .parse_rest_snapshot(msg, |side, l| {
            match side {
                Side::Bid => bids.push(l),
                Side::Ask => asks.push(l),
            }
            true
        })
        .ok()?;
    Some((h, bids, asks))
}

/// The same rules, independently, on serde_json's parse.
fn oracle(msg: &[u8]) -> Parsed {
    // serde_json's `Value` keeps the last of duplicate keys; the rules reject duplicates
    // of known fields, so detect those on the raw map first.
    let mut de = serde_json::Deserializer::from_slice(msg);
    let map: serde_json::Map<String, Value> = serde::Deserialize::deserialize(&mut de).ok()?;
    de.end().ok()?;
    let known = ["lastUpdateId", "E", "T", "bids", "asks"];
    let raw = std::str::from_utf8(msg).ok()?;
    for k in known {
        if raw.matches(&format!("\"{k}\":")).count() > 1 {
            return None; // conservative: only used on inputs without such strings in values
        }
    }
    let u = |k: &str| map.get(k).map(Value::as_u64);
    let levels = |k: &str| -> Option<Vec<Level>> {
        map.get(k)?
            .as_array()?
            .iter()
            .map(|pair| match pair.as_array()?.as_slice() {
                [Value::String(p), Value::String(q)] => Some(Level::new(
                    spec().parse_px(p).ok()?,
                    spec().parse_qty(q).ok()?,
                )),
                _ => None,
            })
            .collect()
    };
    let opt = |k| match u(k) {
        None => Some(0),
        Some(v) => v,
    };
    Some((
        SnapshotHeader {
            last_update_id: u("lastUpdateId")??,
            event_time: opt("E")?,
            transaction_time: opt("T")?,
        },
        levels("bids")?,
        levels("asks")?,
    ))
}

#[test]
fn the_recorded_snapshot() {
    let got = fast(SNAPSHOT.as_bytes()).unwrap();
    assert_eq!(Some(got.clone()), oracle(SNAPSHOT.as_bytes()));
    let (h, bids, asks) = got;
    assert_eq!((bids.len(), asks.len()), (1000, 1000));
    assert!(h.last_update_id > 0 && h.event_time > 0);
    assert!(
        bids.windows(2).all(|w| w[0].px > w[1].px),
        "bids best first"
    );
    assert!(
        asks.windows(2).all(|w| w[0].px < w[1].px),
        "asks best first"
    );
    assert!(bids[0].px < asks[0].px);
}

#[test]
fn edge_cases_agree() {
    for msg in [
        r#"{"lastUpdateId":5,"bids":[],"asks":[]}"#,
        r#"{"asks":[["1.0","1"]],"bids":[["0.5","2"]],"lastUpdateId":5,"x":[1,{"y":2}]}"#,
        r#"{"lastUpdateId":5,"bids":[]}"#,
        r#"{"lastUpdateId":-1,"bids":[],"asks":[]}"#,
        r#"{"lastUpdateId":5,"lastUpdateId":6,"bids":[],"asks":[]}"#,
        r#"{"lastUpdateId":5,"bids":[["1.05","1"]],"asks":[]}"#,
        r#"{"lastUpdateId":5,"bids":[["1.0"]],"asks":[]}"#,
        r#"{"lastUpdateId":5,"E":"x","bids":[],"asks":[]}"#,
        r#"[]"#,
    ] {
        assert_eq!(fast(msg.as_bytes()), oracle(msg.as_bytes()), "{msg}");
    }
    assert!(fast(br#"{"lastUpdateId":5,"bids":[],"asks":[]}"#).is_some());
    assert!(fast(br#"{"lastUpdateId":5,"lastUpdateId":6,"bids":[],"asks":[]}"#).is_none());
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 2_000, ..ProptestConfig::default() })]

    #[test]
    fn mutated_snapshots_agree(muts in prop::collection::vec(mutation(), 1..4)) {
        let msg = mutate(SNAPSHOT.as_bytes(), &muts);
        let got = fast(&msg); // must not panic
        // The duplicate-key check in the oracle is textual: skip inputs where a known
        // key's text could also appear inside a value.
        if comparable(&msg) {
            prop_assert_eq!(got, oracle(&msg));
        }
    }
}
