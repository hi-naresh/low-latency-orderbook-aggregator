//! Binance depth parser: real recorded frames, then every edge case of the message rules.
#![allow(clippy::unwrap_used)]

use ob_core::{ErrorClass, InstrumentSpec, Level, OrderBook, ParseFixedError, Side};
use ob_parse::JsonErrorKind;
use ob_parse::binance::{BinanceDepthParser, DepthUpdate, ErrorKind, Field, Message};

const FIXTURE: &str = include_str!("fixtures/binance_depth20.jsonl");

fn spec() -> InstrumentSpec {
    InstrumentSpec::from_steps("0.1", "0.001").unwrap()
}

fn parser() -> BinanceDepthParser<'static> {
    BinanceDepthParser::new(spec(), b"BTCUSDT")
}

fn parse(msg: &str) -> Result<(Message, DepthUpdate), ErrorKind> {
    let mut out = DepthUpdate::default();
    parser()
        .parse(msg.as_bytes(), &mut out)
        .map(|m| (m, out))
        .map_err(|e| e.kind)
}

fn lvl(px: &str, qty: &str) -> Level {
    let s = spec();
    Level::new(s.parse_px(px).unwrap(), s.parse_qty(qty).unwrap())
}

/// A small valid message to mutate in the edge-case tests.
const SMALL: &str = r#"{"e":"depthUpdate","E":10,"T":9,"s":"BTCUSDT","U":1,"u":5,"pu":0,"b":[["100.10","1.000"]],"a":[["100.20","2.500"]]}"#;

// ---------------------------------------------------------------------------
// Real data
// ---------------------------------------------------------------------------

#[test]
fn every_recorded_frame_parses() {
    let frames: Vec<&str> = FIXTURE.lines().collect();
    assert_eq!(frames.len(), 60);
    // First frame: Binance's reply to our SUBSCRIBE (id 1).
    assert_eq!(
        parse(frames[0]).unwrap().0,
        Message::Response { id: 1, ok: true }
    );
    for frame in &frames[1..] {
        let (msg, out) = parse(frame).unwrap();
        assert_eq!(msg, Message::Depth);
        assert_eq!((out.bids.len(), out.asks.len()), (20, 20));
    }
}

#[test]
fn first_snapshot_values_are_exact() {
    let (_, out) = parse(FIXTURE.lines().nth(1).unwrap()).unwrap();
    assert_eq!(out.event_time, 1_795_610_780_582);
    assert_eq!(out.transaction_time, 1_795_610_780_581);
    assert_eq!(out.first_update_id, 12_235_913_555_065);
    assert_eq!(out.final_update_id, 12_235_913_562_909);
    assert_eq!(out.prev_final_update_id, 12_235_913_554_835);
    // "58631.90" at a 0.1 tick → 586319; "6.774" at a 0.001 lot → 6774.
    assert_eq!(out.bids[0], lvl("58631.90", "6.774"));
    assert_eq!(out.bids[0].px.raw(), 586_319);
    assert_eq!(out.bids[0].qty.raw(), 6_774);
    assert_eq!(out.asks[0], lvl("58632.00", "3.242"));
}

#[test]
fn recorded_frames_form_an_unbroken_sequence() {
    let mut prev: Option<DepthUpdate> = None;
    for frame in FIXTURE.lines().skip(1) {
        let (_, out) = parse(frame).unwrap();
        if let Some(p) = &prev {
            assert!(
                out.prev_final_update_id == p.final_update_id,
                "gap: pu {} != previous u {}",
                out.prev_final_update_id,
                p.final_update_id
            );
            assert!(out.event_time > p.event_time);
        }
        prev = Some(out);
    }
}

#[test]
fn replaying_snapshots_keeps_the_book_exact() {
    let mut book: OrderBook = OrderBook::new();
    for frame in FIXTURE.lines().skip(1) {
        let (_, out) = parse(frame).unwrap();
        out.apply_to(&mut book).unwrap();
        // The book is exactly the latest snapshot: nothing stale, never crossed.
        assert_eq!(book.bids().iter().collect::<Vec<_>>(), out.bids.to_vec());
        assert_eq!(book.asks().iter().collect::<Vec<_>>(), out.asks.to_vec());
        assert!(!book.is_crossed());
    }
}

/// The Java skeleton applies every level of every message as an *update*. Because these
/// messages are full snapshots, levels that drop out of the top 20 are never removed:
/// the book fills with stale prices. This test reproduces that bug with our own book.
#[test]
fn treating_snapshots_as_updates_leaves_stale_levels() {
    let mut wrong: OrderBook = OrderBook::new();
    let mut right: OrderBook = OrderBook::new();
    for frame in FIXTURE.lines().skip(1) {
        let (_, out) = parse(frame).unwrap();
        for l in out.bids.iter() {
            let _ = wrong.apply(Side::Bid, *l);
        }
        for l in out.asks.iter() {
            let _ = wrong.apply(Side::Ask, *l);
        }
        out.apply_to(&mut right).unwrap();
    }
    assert_eq!(right.bids().len(), 20);
    assert!(
        wrong.bids().len() + wrong.asks().len() > 40,
        "stale levels piled up"
    );
    // Worse: stale levels sit *above* the real best prices, so the wrong book is
    // crossed (best bid ≥ best ask) and its top of book is wrong on both sides.
    assert!(wrong.is_crossed());
    assert_ne!(wrong.best_bid(), right.best_bid());
    assert_ne!(wrong.best_ask(), right.best_ask());
    assert!(!right.is_crossed());
    // And the stale book disagrees with the truth below the top as well.
    assert_ne!(
        wrong.bids().iter().collect::<Vec<_>>(),
        right.bids().iter().collect::<Vec<_>>()
    );
}

#[test]
fn every_truncation_of_a_real_frame_is_an_error_not_a_panic() {
    let frame = FIXTURE.lines().nth(1).unwrap().as_bytes();
    let mut out = DepthUpdate::default();
    for end in 0..frame.len() {
        assert!(
            parser().parse(&frame[..end], &mut out).is_err(),
            "prefix of {end} bytes"
        );
    }
    assert!(parser().parse(frame, &mut out).is_ok());
}

// ---------------------------------------------------------------------------
// Message rules
// ---------------------------------------------------------------------------

#[test]
fn field_order_whitespace_and_unknown_fields_dont_matter() {
    let reordered = r#"
        { "a" : [ [ "100.20" , "2.500" ] ] , "st": 1, "pu":0,"u":5,"U":1,
          "ps":"BTCUSDT", "x": {"nested": [1, 2.5e3, true, null, "s\"q"]},
          "b":[["100.10","1.000"]],"s":"BTCUSDT","T":9,"E":10,"e":"depthUpdate" }
    "#;
    let (m1, a) = parse(SMALL).unwrap();
    let (m2, b) = parse(reordered).unwrap();
    assert_eq!((m1, m2), (Message::Depth, Message::Depth));
    assert_eq!(
        (a.event_time, a.final_update_id),
        (b.event_time, b.final_update_id)
    );
    assert_eq!(a.bids.to_vec(), b.bids.to_vec());
    assert_eq!(a.asks.to_vec(), b.asks.to_vec());
}

#[test]
fn missing_fields_are_reported() {
    for (field, key) in [
        (Field::EventTime, r#""E":10,"#),
        (Field::Symbol, r#""s":"BTCUSDT","#),
        (Field::PrevFinalUpdateId, r#""pu":0,"#),
        (Field::Bids, r#""b":[["100.10","1.000"]],"#),
    ] {
        let msg = SMALL.replacen(key, "", 1);
        assert_eq!(
            parse(&msg).unwrap_err(),
            ErrorKind::MissingField(field),
            "{msg}"
        );
    }
}

#[test]
fn duplicate_known_fields_are_rejected() {
    // JSON allows duplicate keys but parsers disagree on which wins; for prices that's
    // dangerous, so we refuse. Unknown duplicates are harmless and skipped.
    let msg = SMALL.replacen(r#""u":5,"#, r#""u":5,"u":6,"#, 1);
    assert_eq!(
        parse(&msg).unwrap_err(),
        ErrorKind::DuplicateField(Field::FinalUpdateId)
    );
    let msg = SMALL.replacen(r#""E":10,"#, r#""zz":1,"zz":2,"E":10,"#, 1);
    assert!(parse(&msg).is_ok());
}

#[test]
fn wrong_event_or_symbol_is_rejected() {
    let msg = SMALL.replacen("depthUpdate", "trade", 1);
    assert_eq!(parse(&msg).unwrap_err(), ErrorKind::WrongEvent);
    let msg = SMALL.replacen(r#""s":"BTCUSDT""#, r#""s":"ETHUSDT""#, 1);
    assert_eq!(parse(&msg).unwrap_err(), ErrorKind::WrongSymbol);
}

#[test]
fn level_shapes_are_strict() {
    for bad in [
        r#"[["100.10"]]"#,                 // one element
        r#"[["100.10","1.000","x"]]"#,     // three
        r#"[[100.10,"1.000"]]"#,           // unquoted number
        r#"["100.10","1.000"]"#,           // not nested
        r#"[{"p":"100.10","q":"1.000"}]"#, // object
    ] {
        let msg = SMALL.replacen(r#"[["100.10","1.000"]]"#, bad, 1);
        assert_eq!(
            parse(&msg).unwrap_err(),
            ErrorKind::BadLevel(Field::Bids),
            "{bad}"
        );
    }
    // Empty sides are valid (a quiet book).
    let msg = SMALL.replacen(r#"[["100.10","1.000"]]"#, "[]", 1);
    assert_eq!(parse(&msg).unwrap().1.bids.len(), 0);
}

#[test]
fn bad_numbers_are_classified_for_the_policy() {
    let p = parser();
    let mut out = DepthUpdate::default();
    let cases = [
        (
            "100.15",
            "1.000",
            ParseFixedError::ExceedsPrecision,
            ErrorClass::SpecMismatch,
        ),
        (
            "100.10",
            "1.0005",
            ParseFixedError::ExceedsPrecision,
            ErrorClass::SpecMismatch,
        ),
        (
            "100.10",
            "-1.000",
            ParseFixedError::Negative,
            ErrorClass::Invalid,
        ),
        (
            "1e2",
            "1.000",
            ParseFixedError::InvalidChar,
            ErrorClass::Malformed,
        ),
    ];
    for (px, qty, want, class) in cases {
        let msg = SMALL.replacen(r#""100.10","1.000""#, &format!(r#""{px}","{qty}""#), 1);
        let err = p.parse(msg.as_bytes(), &mut out).unwrap_err();
        assert_eq!(
            err.kind,
            ErrorKind::Json(JsonErrorKind::Decimal(want), Some(Field::Bids)),
            "{msg}"
        );
        assert_eq!(err.class(), class);
    }
    // Off-tick: valid decimal, but not a multiple of a 0.5 tick.
    let half = InstrumentSpec::from_steps("0.5", "0.001").unwrap();
    let err = BinanceDepthParser::new(half, b"BTCUSDT")
        .parse(SMALL.as_bytes(), &mut out)
        .unwrap_err();
    assert_eq!(
        err.kind,
        ErrorKind::Json(
            JsonErrorKind::Decimal(ParseFixedError::OffTick),
            Some(Field::Bids)
        )
    );
    assert_eq!(err.class(), ErrorClass::SpecMismatch);
}

#[test]
fn too_many_levels_is_rejected() {
    let levels: Vec<String> = (0..=i64::try_from(ob_parse::MAX_LEVELS).unwrap())
        .map(|i| format!(r#"["{}.0","1.000"]"#, 1000 - i))
        .collect();
    let msg = SMALL.replacen(
        r#"[["100.10","1.000"]]"#,
        &format!("[{}]", levels.join(",")),
        1,
    );
    assert_eq!(
        parse(&msg).unwrap_err(),
        ErrorKind::TooManyLevels(Field::Bids)
    );
}

#[test]
fn responses_and_unknown_messages() {
    assert_eq!(
        parse(r#"{"result":null,"id":7}"#).unwrap().0,
        Message::Response { id: 7, ok: true }
    );
    assert_eq!(
        parse(r#"{"error":{"code":2,"msg":"Invalid request"},"id":7}"#)
            .unwrap()
            .0,
        Message::Response { id: 7, ok: false }
    );
    assert_eq!(parse("{}").unwrap_err(), ErrorKind::UnknownMessage);
    assert_eq!(parse(r#"{"x":1}"#).unwrap_err(), ErrorKind::UnknownMessage);
}

#[test]
fn not_json_or_wrong_types() {
    for bad in [
        "",
        "[]",
        "null",
        "{",
        r#"{"E":"10"}"#,
        r#"{"id":-1}"#,
        r#"{"id":1.0}"#,
        "{} {}",
    ] {
        assert!(matches!(parse(bad), Err(ErrorKind::Json(..))), "{bad:?}");
    }
}

#[test]
fn the_buffer_is_reused_without_leftovers() {
    let mut out = DepthUpdate::default();
    let p = parser();
    let big = FIXTURE.lines().nth(1).unwrap();
    p.parse(big.as_bytes(), &mut out).unwrap();
    assert_eq!(out.bids.len(), 20);
    p.parse(SMALL.as_bytes(), &mut out).unwrap();
    assert_eq!(out.bids.len(), 1, "previous message's levels must not leak");
}
