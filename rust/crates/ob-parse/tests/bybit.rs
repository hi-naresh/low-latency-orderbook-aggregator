//! Bybit order book parser: real recorded frames, snapshot/delta/sequence semantics, then
//! every edge case of the message rules.
#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;

use ob_core::{ErrorClass, InstrumentSpec, Level, OrderBook, ParseFixedError, SnapshotError};
use ob_parse::JsonErrorKind;
use ob_parse::bybit::{
    Applied, BookMsg, BybitBookParser, ErrorKind, Field, Kind, Message, Op, Sequence,
};

const FIXTURE: &str = include_str!("fixtures/bybit_orderbook50.jsonl");

fn spec() -> InstrumentSpec {
    InstrumentSpec::from_steps("0.1", "0.001").unwrap()
}

fn parser() -> BybitBookParser<'static> {
    BybitBookParser::new(spec(), b"orderbook.50.BTCUSDT", b"BTCUSDT")
}

fn parse(msg: &str) -> Result<(Message, BookMsg), ErrorKind> {
    let mut out = BookMsg::default();
    parser()
        .parse(msg.as_bytes(), &mut out)
        .map(|m| (m, out))
        .map_err(|e| e.kind)
}

fn lvl(px: &str, qty: &str) -> Level {
    let s = spec();
    Level::new(s.parse_px(px).unwrap(), s.parse_qty(qty).unwrap())
}

/// The book messages of the fixture, in order (the subscription reply is skipped).
fn book_frames() -> Vec<BookMsg> {
    FIXTURE
        .lines()
        .filter_map(|f| match parse(f).unwrap() {
            (Message::Book, out) => Some(out),
            _ => None,
        })
        .collect()
}

const DELTA: &str = r#"{"topic":"orderbook.50.BTCUSDT","type":"delta","ts":10,"data":{"s":"BTCUSDT","b":[["100.10","1.000"]],"a":[["100.20","0"]],"u":5,"seq":99},"cts":9}"#;

// ---------------------------------------------------------------------------
// Real data
// ---------------------------------------------------------------------------

#[test]
fn every_recorded_frame_parses() {
    let frames: Vec<&str> = FIXTURE.lines().collect();
    assert_eq!(frames.len(), 60);
    let (m, snap) = parse(frames[0]).unwrap();
    assert_eq!((m, snap.kind), (Message::Book, Kind::Snapshot));
    assert_eq!((snap.bids.len(), snap.asks.len()), (50, 50));
    assert_eq!(parse(frames[1]).unwrap().1.kind, Kind::Delta);
    // The subscription reply arrived *after* the first snapshot and delta.
    assert_eq!(
        parse(frames[2]).unwrap().0,
        Message::Response {
            op: Op::Subscribe,
            success: true
        }
    );
    for frame in &frames[3..] {
        let (m, out) = parse(frame).unwrap();
        assert_eq!((m, out.kind), (Message::Book, Kind::Delta));
    }
}

#[test]
fn snapshot_and_delta_values_are_exact() {
    let msgs = book_frames();
    let snap = &msgs[0];
    assert_eq!(
        (snap.ts, snap.cts),
        (1_795_610_797_394, Some(1_795_610_797_371))
    );
    assert_eq!((snap.update_id, snap.seq), (22_042_290, 855_165_474_148));
    assert_eq!(snap.bids[0], lvl("58656.20", "0.835"));
    // Second message: one ask level, no bids.
    let delta = &msgs[1];
    assert_eq!(delta.update_id, 22_042_291);
    assert!(delta.bids.is_empty());
    assert_eq!(delta.asks.to_vec(), vec![lvl("58656.30", "6.140")]);
}

#[test]
fn recorded_deltas_are_strictly_sequential() {
    let msgs = book_frames();
    assert_eq!(msgs[0].sequence(None), Sequence::Reset);
    for pair in msgs.windows(2) {
        assert_eq!(pair[1].sequence(Some(pair[0].update_id)), Sequence::Next);
        assert!(pair[1].seq > pair[0].seq);
    }
}

/// Replays the recording into the book and into an independent `BTreeMap` model of
/// Bybit's rules; they must match after every message.
#[test]
fn replaying_recorded_messages_matches_a_model() {
    let mut book: OrderBook = OrderBook::new();
    let (mut bids, mut asks) = (BTreeMap::<i64, i64>::new(), BTreeMap::<i64, i64>::new());
    let mut deleted = 0;
    for msg in book_frames() {
        let applied = msg.apply_to(&mut book).unwrap();
        assert_eq!(
            applied,
            Applied::default(),
            "the healthy recording never deletes a missing level"
        );
        if msg.is_reset() {
            bids.clear();
            asks.clear();
        }
        for (model, levels) in [(&mut bids, &msg.bids), (&mut asks, &msg.asks)] {
            for l in levels.iter() {
                if l.qty.is_zero() {
                    model.remove(&l.px.raw());
                    deleted += 1;
                } else {
                    model.insert(l.px.raw(), l.qty.raw());
                }
            }
        }
        let got_bids: Vec<(i64, i64)> = book
            .bids()
            .iter()
            .map(|l| (l.px.raw(), l.qty.raw()))
            .collect();
        let got_asks: Vec<(i64, i64)> = book
            .asks()
            .iter()
            .map(|l| (l.px.raw(), l.qty.raw()))
            .collect();
        assert_eq!(
            got_bids,
            bids.iter().rev().map(|(&p, &q)| (p, q)).collect::<Vec<_>>()
        );
        assert_eq!(
            got_asks,
            asks.iter().map(|(&p, &q)| (p, q)).collect::<Vec<_>>()
        );
        assert!(!book.is_crossed());
        assert_eq!(
            (book.bids().len(), book.asks().len()),
            (50, 50),
            "Bybit keeps exactly the top 50"
        );
    }
    assert_eq!(deleted, 59);
}

/// The Java skeleton ignores `type`, so a later snapshot (Bybit re-sends one after a
/// reconnect, a resubscribe, or a service restart) is applied as if it were a delta:
/// levels that are gone from the market stay in the book.
#[test]
fn ignoring_the_snapshot_type_leaves_stale_levels() {
    let msgs = book_frames();
    // A second snapshot, as after a reconnect when the market has moved down $60:
    // every price 600 ticks lower.
    let mut later = msgs[0].clone();
    later.update_id = msgs.last().unwrap().update_id + 1;
    for buf in [&mut later.bids, &mut later.asks] {
        let shifted: Vec<Level> = buf
            .iter()
            .map(|l| Level::new(ob_core::Px::from_raw(l.px.raw() - 600), l.qty))
            .collect();
        buf.clear();
        for l in shifted {
            buf.push(l);
        }
    }
    let replay = |treat_snapshot_as_delta: bool| {
        let mut book: OrderBook = OrderBook::new();
        for m in msgs.iter().chain(std::iter::once(&later)) {
            let mut m = m.clone();
            if treat_snapshot_as_delta && m.update_id != msgs[0].update_id {
                m.kind = Kind::Delta;
            }
            m.apply_to(&mut book).unwrap();
        }
        book
    };
    let right = replay(false);
    let wrong = replay(true);
    assert!(!right.is_crossed());
    assert_eq!(right.best_bid(), later.bids.first().copied());
    // The old, higher bids are still there: the wrong book is crossed.
    assert!(wrong.is_crossed());
    assert_ne!(wrong.best_bid(), right.best_bid());
}

#[test]
fn every_truncation_of_a_real_frame_is_an_error_not_a_panic() {
    let frame = FIXTURE.lines().next().unwrap().as_bytes();
    let mut out = BookMsg::default();
    for end in 0..frame.len() {
        assert!(
            parser().parse(&frame[..end], &mut out).is_err(),
            "prefix of {end} bytes"
        );
    }
    assert!(parser().parse(frame, &mut out).is_ok());
}

// ---------------------------------------------------------------------------
// Sequencing and application
// ---------------------------------------------------------------------------

fn delta(u: u64) -> BookMsg {
    let mut m = parse(DELTA).unwrap().1;
    m.update_id = u;
    m
}

#[test]
fn sequence_classification() {
    assert_eq!(delta(5).sequence(Some(4)), Sequence::Next);
    assert_eq!(delta(7).sequence(Some(4)), Sequence::Gap { expected: 5 });
    assert_eq!(delta(4).sequence(Some(4)), Sequence::Stale);
    assert_eq!(delta(3).sequence(Some(4)), Sequence::Stale);
    assert_eq!(delta(5).sequence(None), Sequence::NoBaseline);
    // u == 1 is Bybit's "service restarted" signal: a delta that must be treated as a snapshot.
    assert_eq!(delta(1).sequence(Some(1_000_000)), Sequence::Reset);
    assert_eq!(delta(1).sequence(None), Sequence::Reset);
    let mut snap = delta(42);
    snap.kind = Kind::Snapshot;
    assert_eq!(snap.sequence(Some(41)), Sequence::Reset);
}

#[test]
fn a_restart_delta_replaces_the_book() {
    let mut book: OrderBook = OrderBook::new();
    book_frames()[0].apply_to(&mut book).unwrap();
    assert_eq!(book.bids().len(), 50);
    // u == 1: Bybit restarted, and this "delta" is really a full snapshot.
    let restart_msg = DELTA.replacen(r#"[["100.20","0"]]"#, r#"[["100.20","3.000"]]"#, 1);
    let mut restart = parse(&restart_msg).unwrap().1;
    restart.update_id = 1;
    restart.apply_to(&mut book).unwrap();
    assert_eq!(
        book.bids().iter().collect::<Vec<_>>(),
        restart.bids.to_vec()
    );
    assert_eq!(
        book.asks().iter().collect::<Vec<_>>(),
        restart.asks.to_vec()
    );
    // Being a snapshot, it can't contain a deletion: one with "0" is refused as invalid,
    // and the book is left as it was.
    let bad = delta(1);
    assert_eq!(
        bad.apply_to(&mut book),
        Err(SnapshotError::ZeroQty(ob_core::Side::Ask))
    );
    assert_eq!(
        book.asks().iter().collect::<Vec<_>>(),
        restart.asks.to_vec()
    );
}

#[test]
fn deltas_report_suspicious_deletes() {
    let mut book: OrderBook = OrderBook::new();
    // Deleting 100.20 from an empty ask side: a sign our book has drifted.
    assert_eq!(
        delta(5).apply_to(&mut book).unwrap(),
        Applied {
            removed_absent: 1,
            out_of_depth: 0
        }
    );
}

#[test]
fn a_snapshot_with_a_zero_quantity_is_refused() {
    let msg = DELTA.replacen("delta", "snapshot", 1);
    let (_, m) = parse(&msg).unwrap(); // parses fine...
    let mut book: OrderBook = OrderBook::new();
    // ...but a snapshot can't contain a deletion: the book refuses it, unchanged.
    assert_eq!(
        m.apply_to(&mut book),
        Err(SnapshotError::ZeroQty(ob_core::Side::Ask))
    );
    assert!(book.bids().is_empty());
}

// ---------------------------------------------------------------------------
// Message rules
// ---------------------------------------------------------------------------

#[test]
fn field_order_whitespace_and_unknown_fields_dont_matter() {
    let reordered = r#" { "cts":9, "data" : { "seq":99, "u":5, "x":[1,{"y":null}],
        "a":[["100.20","0"]], "b":[ ["100.10" , "1.000"] ], "s":"BTCUSDT" },
        "zz": {"nested": true}, "ts":10, "type":"delta", "topic":"orderbook.50.BTCUSDT" } "#;
    let (_, a) = parse(DELTA).unwrap();
    let (_, b) = parse(reordered).unwrap();
    assert_eq!(
        (a.kind, a.ts, a.cts, a.update_id, a.seq),
        (b.kind, b.ts, b.cts, b.update_id, b.seq)
    );
    assert_eq!(a.bids.to_vec(), b.bids.to_vec());
    assert_eq!(a.asks.to_vec(), b.asks.to_vec());
}

#[test]
fn cts_is_optional_everything_else_required() {
    let no_cts = DELTA.replacen(r#","cts":9"#, "", 1);
    assert_eq!(parse(&no_cts).unwrap().1.cts, None);
    for (field, cut) in [
        (Field::Type, r#""type":"delta","#),
        (Field::Ts, r#""ts":10,"#),
        (Field::Symbol, r#""s":"BTCUSDT","#),
        (Field::UpdateId, r#""u":5,"#),
        (Field::Asks, r#""a":[["100.20","0"]],"#),
    ] {
        assert_eq!(
            parse(&DELTA.replacen(cut, "", 1)).unwrap_err(),
            ErrorKind::MissingField(field),
            "{cut}"
        );
    }
    let no_data = r#"{"topic":"orderbook.50.BTCUSDT","type":"delta","ts":10}"#;
    assert_eq!(
        parse(no_data).unwrap_err(),
        ErrorKind::MissingField(Field::Data)
    );
}

#[test]
fn duplicates_are_rejected_at_both_levels() {
    let msg = DELTA.replacen(r#""ts":10,"#, r#""ts":10,"ts":11,"#, 1);
    assert_eq!(
        parse(&msg).unwrap_err(),
        ErrorKind::DuplicateField(Field::Ts)
    );
    let msg = DELTA.replacen(r#""u":5,"#, r#""u":5,"u":6,"#, 1);
    assert_eq!(
        parse(&msg).unwrap_err(),
        ErrorKind::DuplicateField(Field::UpdateId)
    );
    let msg = DELTA.replacen(r#""cts":9"#, r#""cts":9,"data":{}"#, 1);
    assert_eq!(
        parse(&msg).unwrap_err(),
        ErrorKind::DuplicateField(Field::Data)
    );
}

#[test]
fn wrong_topic_type_or_symbol() {
    assert_eq!(
        parse(&DELTA.replacen("orderbook.50", "orderbook.200", 1)).unwrap_err(),
        ErrorKind::WrongTopic
    );
    assert_eq!(
        parse(&DELTA.replacen(r#""delta""#, r#""update""#, 1)).unwrap_err(),
        ErrorKind::WrongType
    );
    assert_eq!(
        parse(&DELTA.replacen(r#""s":"BTCUSDT""#, r#""s":"ETHUSDT""#, 1)).unwrap_err(),
        ErrorKind::WrongSymbol
    );
}

#[test]
fn bad_numbers_are_classified_for_the_policy() {
    let p = parser();
    let mut out = BookMsg::default();
    for (level, want, class) in [
        (
            r#"["100.15","1.000"]"#,
            ParseFixedError::ExceedsPrecision,
            ErrorClass::SpecMismatch,
        ),
        (
            r#"["100.10","-1"]"#,
            ParseFixedError::Negative,
            ErrorClass::Invalid,
        ),
        (
            r#"["100.10","x"]"#,
            ParseFixedError::InvalidChar,
            ErrorClass::Malformed,
        ),
    ] {
        let msg = DELTA.replacen(r#"["100.10","1.000"]"#, level, 1);
        let err = p.parse(msg.as_bytes(), &mut out).unwrap_err();
        assert_eq!(
            err.kind,
            ErrorKind::Json(JsonErrorKind::Decimal(want), Some(Field::Bids)),
            "{level}"
        );
        assert_eq!(err.class(), class);
    }
    let msg = DELTA.replacen(r#"[["100.10","1.000"]]"#, r#"[["100.10"]]"#, 1);
    assert_eq!(parse(&msg).unwrap_err(), ErrorKind::BadLevel(Field::Bids));
}

#[test]
fn too_many_levels_is_rejected() {
    let levels: Vec<String> = (0..=i64::try_from(ob_parse::MAX_LEVELS).unwrap())
        .map(|i| format!(r#"["{}.0","1.000"]"#, 1000 - i))
        .collect();
    let msg = DELTA.replacen(
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
fn responses() {
    let sub = r#"{"success":true,"ret_msg":"","conn_id":"abc","req_id":"depth","op":"subscribe"}"#;
    assert_eq!(
        parse(sub).unwrap().0,
        Message::Response {
            op: Op::Subscribe,
            success: true
        }
    );
    let failed =
        r#"{"success":false,"ret_msg":"error:handler not found","conn_id":"abc","op":"subscribe"}"#;
    assert_eq!(
        parse(failed).unwrap().0,
        Message::Response {
            op: Op::Subscribe,
            success: false
        }
    );
    let pong = r#"{"success":true,"ret_msg":"pong","conn_id":"abc","req_id":"","op":"ping"}"#;
    assert_eq!(
        parse(pong).unwrap().0,
        Message::Response {
            op: Op::Pong,
            success: true
        }
    );
    assert_eq!(
        parse(r#"{"op":"subscribe"}"#).unwrap_err(),
        ErrorKind::MissingField(Field::Success)
    );
    assert!(matches!(
        parse(r#"{"success":"yes","op":"x"}"#),
        Err(ErrorKind::Json(_, Some(Field::Success)))
    ));
    assert_eq!(parse("{}").unwrap_err(), ErrorKind::UnknownMessage);
}

#[test]
fn the_buffer_is_reused_without_leftovers() {
    let mut out = BookMsg::default();
    let p = parser();
    p.parse(FIXTURE.lines().next().unwrap().as_bytes(), &mut out)
        .unwrap();
    assert_eq!(out.bids.len(), 50);
    p.parse(DELTA.replacen(r#","cts":9"#, "", 1).as_bytes(), &mut out)
        .unwrap();
    assert_eq!(
        (out.bids.len(), out.cts),
        (1, None),
        "no levels or cts from the previous message"
    );
}

/// Found by the live test in step 12: a Bybit delta can carry more levels than the stream's
/// depth (66 per side seen live), because it lists deletions *and* insertions. Applied in
/// order to a book with too little capacity, the temporary overflow drops "worst" levels
/// that the same delta's later deletions would have brought back into the top 50.
#[test]
fn a_big_delta_needs_capacity_for_twice_the_depth() {
    let l = |px: i64, qty: i64| {
        Level::new(
            ob_core::Px::from_raw(px),
            ob_core::Qty::from_raw(qty).unwrap(),
        )
    };
    // A full 50-level bid side: 1000 down to 951.
    let snapshot: Vec<Level> = (0..50).map(|i| l(1000 - i, 1)).collect();
    // The market moves up: 20 new better bids (1020..1001) are inserted, and the old top
    // 20 (1000..981) deleted. Bybit lists them best-first, so the inserts come first.
    let mut delta = BookMsg {
        kind: Kind::Delta,
        update_id: 2,
        ..BookMsg::default()
    };
    for px in (1001..=1020).rev() {
        delta.bids.push(l(px, 2));
    }
    for px in (981..=1000).rev() {
        delta.bids.push(l(px, 0));
    }
    // What Bybit's book is afterwards: 1020..1001 then 980..951, 50 levels.
    let truth: Vec<i64> = (1001..=1020).rev().chain((951..=980).rev()).collect();

    let mut small: OrderBook<64> = OrderBook::new();
    small.replace(&snapshot, &[]).unwrap();
    delta.apply_to(&mut small).unwrap();
    let got: Vec<i64> = small.bids().iter().map(|l| l.px.raw()).collect();
    assert_eq!(
        got.len(),
        44,
        "capacity 64 loses the 6 worst levels during the delta"
    );
    assert_ne!(got, truth);

    let mut big: OrderBook<128> = OrderBook::new();
    big.replace(&snapshot, &[]).unwrap();
    delta.apply_to(&mut big).unwrap();
    let got: Vec<i64> = big.bids().iter().map(|l| l.px.raw()).collect();
    assert_eq!(got, truth, "capacity ≥ 2 × depth keeps every level");
}
