//! An independent implementation of the Binance message rules (see `src/binance.rs`),
//! built on serde_json and the reference decimal parser. Slow and simple on purpose: it
//! is the oracle that the fast hand-written parser is checked against.
//!
//! Shared by `tests/binance_differential.rs` and the fuzz target (`fuzz/`).

use std::fmt;

use ob_core::{InstrumentSpec, Level};
use serde::de::{self, Deserializer, IgnoredAny, MapAccess, Visitor};

/// What the oracle concluded, in a form comparable with the fast parser's output.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum OracleResult {
    Depth {
        e_time: u64,
        t_time: u64,
        first: u64,
        last: u64,
        prev: u64,
        bids: Vec<Level>,
        asks: Vec<Level>,
    },
    Ticker {
        u: u64,
        e_time: u64,
        t_time: u64,
        bid: Level,
        ask: Level,
    },
    Response {
        id: u64,
        ok: bool,
    },
    Reject,
}

#[derive(Default)]
struct Raw {
    e: Option<String>,
    big_e: Option<u64>,
    t: Option<u64>,
    s: Option<String>,
    first: Option<u64>,
    last: Option<u64>,
    prev: Option<u64>,
    // Depth: an array of [price, qty] pairs; bookTicker: a price string.
    b: Option<serde_json::Value>,
    a: Option<serde_json::Value>,
    big_b: Option<String>,
    big_a: Option<String>,
    id: Option<u64>,
    result: bool,
    error: bool,
}

impl<'de> serde::Deserialize<'de> for Raw {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Raw, D::Error> {
        d.deserialize_map(RawVisitor)
    }
}

struct RawVisitor;

fn set<T, E: de::Error>(slot: &mut Option<T>, v: T, key: &str) -> Result<(), E> {
    if slot.is_some() {
        return Err(E::custom(format!("duplicate field {key}")));
    }
    *slot = Some(v);
    Ok(())
}

impl<'de> Visitor<'de> for RawVisitor {
    type Value = Raw;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a JSON object")
    }
    fn visit_map<M: MapAccess<'de>>(self, mut m: M) -> Result<Raw, M::Error> {
        let mut r = Raw::default();
        while let Some(key) = m.next_key::<String>()? {
            match key.as_str() {
                "e" => set(&mut r.e, m.next_value()?, &key)?,
                "E" => set(&mut r.big_e, m.next_value()?, &key)?,
                "T" => set(&mut r.t, m.next_value()?, &key)?,
                "s" => set(&mut r.s, m.next_value()?, &key)?,
                "U" => set(&mut r.first, m.next_value()?, &key)?,
                "u" => set(&mut r.last, m.next_value()?, &key)?,
                "pu" => set(&mut r.prev, m.next_value()?, &key)?,
                "b" => set(&mut r.b, m.next_value()?, &key)?,
                "a" => set(&mut r.a, m.next_value()?, &key)?,
                "B" => set(&mut r.big_b, m.next_value()?, &key)?,
                "A" => set(&mut r.big_a, m.next_value()?, &key)?,
                "id" => set(&mut r.id, m.next_value()?, &key)?,
                "result" | "error" => {
                    let flag = if key == "result" {
                        &mut r.result
                    } else {
                        &mut r.error
                    };
                    if *flag {
                        return Err(de::Error::custom(format!("duplicate field {key}")));
                    }
                    *flag = true;
                    m.next_value::<IgnoredAny>()?;
                }
                _ => {
                    m.next_value::<IgnoredAny>()?;
                }
            }
        }
        Ok(r)
    }
}

fn levels(spec: &InstrumentSpec, raw: &[serde_json::Value]) -> Option<Vec<Level>> {
    if raw.len() > ob_parse::MAX_LEVELS {
        return None;
    }
    raw.iter()
        .map(|pair| match pair.as_array()?.as_slice() {
            [serde_json::Value::String(p), serde_json::Value::String(q)] => {
                Some(Level::new(spec.parse_px(p).ok()?, spec.parse_qty(q).ok()?))
            }
            _ => None,
        })
        .collect()
}

/// `b`/`a` in either form: `Some(Ok(levels))` for an array, `Some(Err(price))` for a
/// price string, `None` if invalid.
type Side = Result<Vec<Level>, ob_core::Px>;
fn side(spec: &InstrumentSpec, v: &serde_json::Value) -> Option<Side> {
    match v {
        serde_json::Value::Array(a) => levels(spec, a).map(Ok),
        serde_json::Value::String(p) => spec.parse_px(p).ok().map(Err),
        _ => None,
    }
}

/// Applies the Binance message rules to `msg` using serde_json.
pub(crate) fn oracle(msg: &[u8], spec: &InstrumentSpec, symbol: &str) -> OracleResult {
    let mut de = serde_json::Deserializer::from_slice(msg);
    let raw: Raw = match serde::Deserialize::deserialize(&mut de).and_then(|r| de.end().map(|()| r))
    {
        Ok(r) => r,
        Err(_) => return OracleResult::Reject,
    };
    // `b a` (either form) and `B A` are validated wherever they appear, like in the fast
    // parser.
    let bids = match raw.b.as_ref().map(|b| side(spec, b)) {
        Some(None) => return OracleResult::Reject,
        Some(Some(v)) => Some(v),
        None => None,
    };
    let asks = match raw.a.as_ref().map(|a| side(spec, a)) {
        Some(None) => return OracleResult::Reject,
        Some(Some(v)) => Some(v),
        None => None,
    };
    let qty = |q: &Option<String>| q.as_deref().map(|q| spec.parse_qty(q).ok());
    let (bid_qty, ask_qty) = (qty(&raw.big_b), qty(&raw.big_a));
    if matches!(bid_qty, Some(None)) || matches!(ask_qty, Some(None)) {
        return OracleResult::Reject;
    }
    if raw.e.as_deref() == Some("bookTicker") {
        return match (
            raw.big_e, raw.t, raw.s, raw.last, bids, asks, bid_qty, ask_qty,
        ) {
            (
                Some(e_time),
                Some(t_time),
                Some(s),
                Some(u),
                Some(Err(bid_px)),
                Some(Err(ask_px)),
                Some(Some(bq)),
                Some(Some(aq)),
            ) if s == symbol => OracleResult::Ticker {
                u,
                e_time,
                t_time,
                bid: Level::new(bid_px, bq),
                ask: Level::new(ask_px, aq),
            },
            _ => OracleResult::Reject,
        };
    }
    // Depth: `b`/`a` must be arrays (`Some(Some(levels))`); the string form is rejected.
    let (bids, asks) = (bids.map(Result::ok), asks.map(Result::ok));
    if let Some(e) = raw.e {
        match (
            raw.big_e, raw.t, raw.s, raw.first, raw.last, raw.prev, bids, asks,
        ) {
            (
                Some(e_time),
                Some(t_time),
                Some(s),
                Some(first),
                Some(last),
                Some(prev),
                Some(Some(bids)),
                Some(Some(asks)),
            ) if e == "depthUpdate" && s == symbol => OracleResult::Depth {
                e_time,
                t_time,
                first,
                last,
                prev,
                bids,
                asks,
            },
            _ => OracleResult::Reject,
        }
    } else if let Some(id) = raw.id {
        OracleResult::Response { id, ok: !raw.error }
    } else {
        OracleResult::Reject
    }
}

/// Runs the fast parser and converts its output for comparison with [`oracle`].
pub(crate) fn fast(msg: &[u8], spec: &InstrumentSpec, symbol: &str) -> OracleResult {
    use ob_parse::binance::{BinanceDepthParser, DepthUpdate, Message};
    let mut out = DepthUpdate::default();
    match BinanceDepthParser::new(*spec, symbol.as_bytes()).parse(msg, &mut out) {
        Ok(Message::Depth) => OracleResult::Depth {
            e_time: out.event_time,
            t_time: out.transaction_time,
            first: out.first_update_id,
            last: out.final_update_id,
            prev: out.prev_final_update_id,
            bids: out.bids.to_vec(),
            asks: out.asks.to_vec(),
        },
        Ok(Message::BookTicker(t)) => OracleResult::Ticker {
            u: t.update_id,
            e_time: t.event_time,
            t_time: t.transaction_time,
            bid: t.bid,
            ask: t.ask,
        },
        Ok(Message::Response { id, ok }) => OracleResult::Response { id, ok },
        Err(_) => OracleResult::Reject,
    }
}
