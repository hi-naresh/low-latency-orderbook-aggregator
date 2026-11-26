//! Live check against the real exchanges (needs the internet; ignored by default):
//! connect over TLS, upgrade to WebSocket, subscribe exactly as the Java skeleton does,
//! and parse what arrives with this project's own parsers.
//!
//! Run: `cargo test -p ob-net --release --test live -- --ignored --nocapture`
#![allow(
    clippy::unwrap_used,
    clippy::print_stdout,
    clippy::cast_possible_truncation
)]

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ob_core::{InstrumentSpec, OrderBook};
use ob_net::ws::Message;
use ob_net::{ConnConfig, WsConnection};
use ob_parse::binance::{self, BinanceDepthParser, DepthUpdate};
use ob_parse::bybit::{self, BookMsg, BybitBookParser};

fn wall_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

/// Connects and returns the connection plus how long connect + TLS + upgrade took.
fn connect(host: &str, path: &str) -> (WsConnection, Duration) {
    let cfg = ConnConfig::new(host, path).unwrap();
    let start = Instant::now();
    let conn = WsConnection::connect(&cfg).unwrap();
    (conn, start.elapsed())
}

/// Polls (spinning) for the next text message, as an owned string.
fn next_text(conn: &mut WsConnection) -> String {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        match conn.poll().unwrap() {
            Some(Message::Text(t)) => return String::from_utf8(t.to_vec()).unwrap(),
            Some(_) => {}
            None => assert!(Instant::now() < deadline, "no message within 15 s"),
        }
    }
}

#[test]
#[ignore = "needs the internet"]
fn binance_live() {
    let spec = InstrumentSpec::from_steps("0.1", "0.001").unwrap();
    let (mut conn, took) = connect("fstream.binance.com", "/ws");
    println!(
        "binance: connected in {took:?}, cipher {:?}",
        conn.cipher_suite()
    );
    conn.send_text(br#"{"method":"SUBSCRIBE","params":["btcusdt@depth20@100ms"],"id":1}"#)
        .unwrap();
    let parser = BinanceDepthParser::new(spec, b"BTCUSDT");
    let (mut out, mut depth, mut delays, mut prev): (
        DepthUpdate,
        usize,
        Vec<u64>,
        Option<DepthUpdate>,
    ) = (DepthUpdate::default(), 0, Vec::new(), None);
    while depth < 20 {
        let text = next_text(&mut conn);
        match parser.parse(text.as_bytes(), &mut out).unwrap() {
            binance::Message::Response { id, ok } => {
                println!("binance: subscribe reply id={id} ok={ok}")
            }
            // Not subscribed here (the feed adds it); ignore if it ever arrives.
            binance::Message::BookTicker(_) => {}
            binance::Message::Depth => {
                depth += 1;
                delays.push(wall_ms().saturating_sub(out.event_time));
                if let Some(p) = &prev {
                    assert_eq!(out.prev_final_update_id, p.final_update_id, "sequence gap");
                }
                prev = Some(out.clone());
            }
        }
    }
    let last = prev.unwrap();
    println!(
        "binance: 20 depth snapshots, unbroken pu→u chain; best bid {} ask {}; exchange→us delay (wall clocks) min {} ms, max {} ms",
        spec.fmt_px(last.bids[0].px),
        spec.fmt_px(last.asks[0].px),
        delays.iter().min().unwrap(),
        delays.iter().max().unwrap()
    );
    conn.close(1000).unwrap();
}

#[test]
#[ignore = "needs the internet"]
fn bybit_live() {
    let spec = InstrumentSpec::from_steps("0.1", "0.001").unwrap();
    let (mut conn, took) = connect("stream.bybit.com", "/v5/public/linear");
    println!(
        "bybit: connected in {took:?}, cipher {:?}",
        conn.cipher_suite()
    );
    conn.send_text(br#"{"req_id":"depth","op":"subscribe","args":["orderbook.50.BTCUSDT"]}"#)
        .unwrap();
    let parser = BybitBookParser::new(spec, b"orderbook.50.BTCUSDT", b"BTCUSDT");
    let (mut out, mut last, mut books, mut pong) = (BookMsg::default(), None, 0, false);
    // Capacity ≥ 2 × depth: a delta can list 50 deletions and 50 insertions.
    let mut book: OrderBook<128> = OrderBook::new();
    let mut widest = 0;
    let mut delays = Vec::new();
    conn.send_text(br#"{"op":"ping"}"#).unwrap(); // Bybit's application-level heartbeat
    while books < 300 || !pong {
        let text = next_text(&mut conn);
        match parser.parse(text.as_bytes(), &mut out).unwrap() {
            bybit::Message::Response { op, success } => {
                println!("bybit: response {op:?} success={success}");
                pong |= op == bybit::Op::Pong && success;
            }
            bybit::Message::Book => {
                assert!(
                    matches!(
                        out.sequence(last),
                        bybit::Sequence::Reset | bybit::Sequence::Next
                    ),
                    "gap"
                );
                last = Some(out.update_id);
                widest = widest.max(out.bids.len()).max(out.asks.len());
                out.apply_to(&mut book).unwrap();
                assert!(!book.is_crossed());
                assert!(book.bids().len() <= 50 && book.asks().len() <= 50);
                books += 1;
                delays.push(wall_ms().saturating_sub(out.ts));
            }
        }
    }
    println!(
        "bybit: 300 book messages in sequence, applied (never crossed, ≤ 50 levels), widest message {} levels/side, pong received; best bid {} ask {}; exchange→us delay (wall clocks) min {} ms, max {} ms",
        widest,
        spec.fmt_px(book.best_bid().unwrap().px),
        spec.fmt_px(book.best_ask().unwrap().px),
        delays.iter().min().unwrap(),
        delays.iter().max().unwrap()
    );
    conn.close(1000).unwrap();
}
