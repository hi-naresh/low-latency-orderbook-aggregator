//! An independent implementation of the Bybit message rules (see `src/bybit.rs`), built
//! on serde_json and the reference decimal parser: the oracle the fast parser is checked
//! against. Shared by `tests/bybit_differential.rs` and the fuzz target.

use std::fmt;

use ob_core::{InstrumentSpec, Level};
use ob_parse::bybit::{Kind, Op};
use serde::de::{self, Deserializer, IgnoredAny, MapAccess, Visitor};

/// The oracle's conclusion, comparable with the fast parser's output.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum OracleResult {
    Book {
        top: bool,
        kind: Kind,
        ts: u64,
        cts: Option<u64>,
        u: u64,
        seq: u64,
        bids: Vec<Level>,
        asks: Vec<Level>,
    },
    Response {
        op: Op,
        success: bool,
    },
    Reject,
}

fn set<T, E: de::Error>(slot: &mut Option<T>, v: T, key: &str) -> Result<(), E> {
    if slot.is_some() {
        return Err(E::custom(format!("duplicate field {key}")));
    }
    *slot = Some(v);
    Ok(())
}

#[derive(Default)]
struct Data {
    s: Option<String>,
    b: Option<Vec<[String; 2]>>,
    a: Option<Vec<[String; 2]>>,
    u: Option<u64>,
    seq: Option<u64>,
}

impl<'de> serde::Deserialize<'de> for Data {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Data, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Data;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("the data object")
            }
            fn visit_map<M: MapAccess<'de>>(self, mut m: M) -> Result<Data, M::Error> {
                let mut r = Data::default();
                while let Some(key) = m.next_key::<String>()? {
                    match key.as_str() {
                        "s" => set(&mut r.s, m.next_value()?, &key)?,
                        "b" => set(&mut r.b, m.next_value()?, &key)?,
                        "a" => set(&mut r.a, m.next_value()?, &key)?,
                        "u" => set(&mut r.u, m.next_value()?, &key)?,
                        "seq" => set(&mut r.seq, m.next_value()?, &key)?,
                        _ => {
                            m.next_value::<IgnoredAny>()?;
                        }
                    }
                }
                Ok(r)
            }
        }
        d.deserialize_map(V)
    }
}

#[derive(Default)]
struct Top {
    topic: Option<String>,
    kind: Option<String>,
    ts: Option<u64>,
    cts: Option<u64>,
    data: Option<Data>,
    success: Option<bool>,
    ret_msg: Option<String>,
    conn_id: Option<String>,
    req_id: Option<String>,
    op: Option<String>,
}

impl<'de> serde::Deserialize<'de> for Top {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Top, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Top;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a JSON object")
            }
            fn visit_map<M: MapAccess<'de>>(self, mut m: M) -> Result<Top, M::Error> {
                let mut r = Top::default();
                while let Some(key) = m.next_key::<String>()? {
                    match key.as_str() {
                        "topic" => set(&mut r.topic, m.next_value()?, &key)?,
                        "type" => set(&mut r.kind, m.next_value()?, &key)?,
                        "ts" => set(&mut r.ts, m.next_value()?, &key)?,
                        "cts" => set(&mut r.cts, m.next_value()?, &key)?,
                        "data" => set(&mut r.data, m.next_value()?, &key)?,
                        "success" => set(&mut r.success, m.next_value()?, &key)?,
                        "ret_msg" => set(&mut r.ret_msg, m.next_value()?, &key)?,
                        "conn_id" => set(&mut r.conn_id, m.next_value()?, &key)?,
                        "req_id" => set(&mut r.req_id, m.next_value()?, &key)?,
                        "op" => set(&mut r.op, m.next_value()?, &key)?,
                        _ => {
                            m.next_value::<IgnoredAny>()?;
                        }
                    }
                }
                Ok(r)
            }
        }
        d.deserialize_map(V)
    }
}

fn levels(spec: &InstrumentSpec, raw: &[[String; 2]]) -> Option<Vec<Level>> {
    if raw.len() > ob_parse::MAX_LEVELS {
        return None;
    }
    raw.iter()
        .map(|[p, q]| Some(Level::new(spec.parse_px(p).ok()?, spec.parse_qty(q).ok()?)))
        .collect()
}

/// Applies the Bybit message rules to `msg` using serde_json.
#[allow(dead_code)] // used by the fuzz target; the differential test uses `_with_top`
pub(crate) fn oracle(msg: &[u8], spec: &InstrumentSpec, topic: &str, symbol: &str) -> OracleResult {
    oracle_with_top(msg, spec, topic, None, symbol)
}

/// [`oracle`], also accepting `top_topic` (its messages are marked `top: true`).
pub(crate) fn oracle_with_top(
    msg: &[u8],
    spec: &InstrumentSpec,
    topic: &str,
    top_topic: Option<&str>,
    symbol: &str,
) -> OracleResult {
    let mut de = serde_json::Deserializer::from_slice(msg);
    let top: Top = match serde::Deserialize::deserialize(&mut de).and_then(|t| de.end().map(|()| t))
    {
        Ok(t) => t,
        Err(_) => return OracleResult::Reject,
    };
    // Levels are validated wherever they appear.
    let (mut bids, mut asks) = (None, None);
    if let Some(d) = &top.data {
        for (raw, out) in [(&d.b, &mut bids), (&d.a, &mut asks)] {
            if let Some(raw) = raw {
                match levels(spec, raw) {
                    Some(v) => *out = Some(v),
                    None => return OracleResult::Reject,
                }
            }
        }
    }
    if let Some(t) = top.topic {
        let Some(d) = top.data else {
            return OracleResult::Reject;
        };
        let kind = match top.kind.as_deref() {
            Some("snapshot") => Kind::Snapshot,
            Some("delta") => Kind::Delta,
            _ => return OracleResult::Reject,
        };
        match (top.ts, d.s, bids, asks, d.u, d.seq) {
            (Some(ts), Some(s), Some(bids), Some(asks), Some(u), Some(seq))
                if (t == topic || Some(t.as_str()) == top_topic) && s == symbol =>
            {
                OracleResult::Book {
                    top: t != topic,
                    kind,
                    ts,
                    cts: top.cts,
                    u,
                    seq,
                    bids,
                    asks,
                }
            }
            _ => OracleResult::Reject,
        }
    } else if let Some(op) = top.op {
        match top.success {
            Some(success) => {
                let op = match op.as_str() {
                    "subscribe" => Op::Subscribe,
                    "unsubscribe" => Op::Unsubscribe,
                    "ping" | "pong" => Op::Pong,
                    _ => Op::Other,
                };
                OracleResult::Response { op, success }
            }
            None => OracleResult::Reject,
        }
    } else {
        OracleResult::Reject
    }
}

/// Runs the fast parser and converts its output for comparison with [`oracle`].
#[allow(dead_code)] // used by the fuzz target; the differential test uses `_with_top`
pub(crate) fn fast(msg: &[u8], spec: &InstrumentSpec, topic: &str, symbol: &str) -> OracleResult {
    fast_with_top(msg, spec, topic, None, symbol)
}

/// [`fast`] with an optional top-of-book topic.
pub(crate) fn fast_with_top(
    msg: &[u8],
    spec: &InstrumentSpec,
    topic: &str,
    top_topic: Option<&str>,
    symbol: &str,
) -> OracleResult {
    use ob_parse::bybit::{BookMsg, BybitBookParser, Channel, Message};
    let mut out = BookMsg::default();
    let mut parser = BybitBookParser::new(*spec, topic.as_bytes(), symbol.as_bytes());
    if let Some(t) = top_topic {
        parser = parser.with_top_topic(t.as_bytes());
    }
    match parser.parse(msg, &mut out) {
        Ok(Message::Book) => OracleResult::Book {
            top: out.channel == Channel::Top,
            kind: out.kind,
            ts: out.ts,
            cts: out.cts,
            u: out.update_id,
            seq: out.seq,
            bids: out.bids.to_vec(),
            asks: out.asks.to_vec(),
        },
        Ok(Message::Response { op, success }) => OracleResult::Response { op, success },
        Err(_) => OracleResult::Reject,
    }
}
