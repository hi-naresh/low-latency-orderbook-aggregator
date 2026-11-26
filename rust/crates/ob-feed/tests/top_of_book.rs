//! The top-of-book overlay (step 18), checked differentially on recorded live data.
//!
//! Two feeds receive the same interleaved stream (depth + real-time top of book, as
//! recorded from one connection): one depth-only, one with the overlay. After **every**
//! message, the overlay feed's published book must equal an independent model: the
//! depth-only book, with the newest top-of-book message laid over it whenever that
//! message's sequence number is higher than the book's.
#![allow(clippy::unwrap_used, clippy::print_stdout)]

use ob_core::{InstrumentSpec, Level};
use ob_feed::{Binance, Bybit, Commands, Feed, FeedConfig, Venue};
use ob_parse::binance::{self, BinanceDepthParser, DepthUpdate};
use ob_parse::bybit::{self, BookMsg, BybitBookParser, Channel};
use ob_sync::{BookReader, Snapshot, VenueStatus, book_channel};

const BINANCE: &str =
    include_str!("../../ob-parse/tests/fixtures/binance_depth_and_bookticker.jsonl");
const BYBIT: &str = include_str!("../../ob-parse/tests/fixtures/bybit_orderbook50_and_1.jsonl");

fn spec() -> InstrumentSpec {
    InstrumentSpec::from_steps("0.1", "0.001").unwrap()
}

struct Running<V: Venue> {
    feed: Feed<V>,
    reader: BookReader,
    cmds: Commands,
}

impl<V: Venue> Running<V> {
    fn new(venue: V) -> Self {
        let (publisher, reader) = book_channel();
        let mut feed = Feed::new(venue, publisher, FeedConfig::default(), 1);
        let mut cmds = Commands::default();
        feed.on_connected(0, &mut cmds);
        Running { feed, reader, cmds }
    }
    fn text(&mut self, line: &str) -> Snapshot {
        self.feed.on_text(1, line.as_bytes(), &mut self.cmds);
        while self.cmds.pop().is_some() {}
        let mut s = Snapshot::default();
        self.reader.read_into(&mut s);
        s
    }
}

/// The model: `top` (if any) first, then the depth-only book's strictly worse levels.
fn overlay(depth: &[Level], top: Option<Level>, worse: fn(&Level, &Level) -> bool) -> Vec<Level> {
    let mut v: Vec<Level> = top.into_iter().collect();
    v.extend(depth.iter().filter(|l| top.is_none_or(|t| worse(l, &t))));
    v
}

fn check_step(i: usize, with: &Snapshot, without: &Snapshot, top: Option<(Level, Level)>) {
    let want_bids = overlay(without.bids(), top.map(|t| t.0), |l, t| l.px < t.px);
    let want_asks = overlay(without.asks(), top.map(|t| t.1), |l, t| l.px > t.px);
    assert_eq!(with.bids(), &want_bids[..], "bids after line {i}");
    assert_eq!(with.asks(), &want_asks[..], "asks after line {i}");
    if let (Some(b), Some(a)) = (with.bids().first(), with.asks().first()) {
        assert!(b.px < a.px, "published book crossed after line {i}");
    }
}

#[test]
fn binance_overlay_matches_the_model_after_every_message() {
    let parser = BinanceDepthParser::new(spec(), b"BTCUSDT");
    let mut scratch = DepthUpdate::default();
    let mut with = Running::new(Binance::new(spec(), "BTCUSDT").with_top_of_book());
    let mut without = Running::new(Binance::new(spec(), "BTCUSDT"));
    // The model's state: the book's `u`, and the newest ticker newer than it.
    let (mut book_u, mut top): (Option<u64>, Option<(u64, Level, Level)>) = (None, None);
    let (mut overlaid, mut superseded) = (0, 0);
    for (i, line) in BINANCE.lines().enumerate() {
        match parser.parse(line.as_bytes(), &mut scratch).unwrap() {
            binance::Message::Depth => {
                book_u = Some(scratch.final_update_id);
                if top.is_some_and(|(u, ..)| u <= scratch.final_update_id) {
                    top = None;
                    superseded += 1;
                }
            }
            binance::Message::BookTicker(t) => {
                let newest = top.map_or(book_u, |(u, ..)| Some(u));
                if newest.is_some_and(|n| t.update_id > n) {
                    top = Some((t.update_id, t.bid, t.ask));
                    overlaid += 1;
                }
            }
            binance::Message::Response { .. } => {}
        }
        let (w, wo) = (with.text(line), without.text(line));
        check_step(i, &w, &wo, top.map(|(_, b, a)| (b, a)));
        if book_u.is_some() {
            assert_eq!(w.meta.status, VenueStatus::Live);
        }
    }
    let stats = with.feed.stats();
    println!(
        "binance: {overlaid} tickers laid over the book, {superseded} overlays superseded by depth; {stats:?}"
    );
    assert!(
        overlaid > 500 && superseded >= 2,
        "the fixture must exercise both directions"
    );
    assert_eq!(stats.top_updates, overlaid);
    assert_eq!(
        without.feed.stats().top_updates,
        0,
        "depth-only ignores tickers"
    );
}

#[test]
fn bybit_overlay_matches_the_model_after_every_message() {
    let parser = BybitBookParser::new(spec(), b"orderbook.50.BTCUSDT", b"BTCUSDT")
        .with_top_topic(b"orderbook.1.BTCUSDT");
    let mut m = BookMsg::default();
    let mut with = Running::new(Bybit::new(spec(), "BTCUSDT").with_top_of_book());
    let mut without = Running::new(Bybit::new(spec(), "BTCUSDT"));
    let (mut book_seq, mut top): (Option<u64>, Option<(u64, Level, Level)>) = (None, None);
    let (mut overlaid, mut superseded) = (0, 0);
    let mut applied_before = 0;
    let mut wo = Snapshot::default();
    for (i, line) in BYBIT.lines().enumerate() {
        let is_top = matches!(
            parser.parse(line.as_bytes(), &mut m),
            Ok(bybit::Message::Book)
        ) && m.channel == Channel::Top;
        // The depth-only feed never receives the top topic (it didn't subscribe; fed one
        // anyway, it would rightly treat the unknown topic as unparseable and resync).
        if !is_top {
            wo = without.text(line);
        }
        // A depth message counts only if the depth-only feed applied it.
        let applied_now = without.feed.stats().applied;
        let depth_applied = applied_now > applied_before;
        applied_before = applied_now;
        if let bybit::Message::Book = parser.parse(line.as_bytes(), &mut m).unwrap() {
            match m.channel {
                Channel::Depth if depth_applied => {
                    book_seq = Some(m.seq);
                    if top.is_some_and(|(s, ..)| s <= m.seq) {
                        top = None;
                        superseded += 1;
                    }
                }
                Channel::Depth => {}
                Channel::Top => {
                    let newest = top.map_or(book_seq, |(s, ..)| Some(s));
                    if newest.is_some_and(|n| m.seq > n) {
                        top = Some((m.seq, m.bids[0], m.asks[0]));
                        overlaid += 1;
                    }
                }
            }
        }
        let w = with.text(line);
        check_step(i, &w, &wo, top.map(|(_, b, a)| (b, a)));
    }
    let stats = with.feed.stats();
    println!(
        "bybit: {overlaid} top snapshots laid over the book, {superseded} superseded by depth; {stats:?}"
    );
    assert!(
        overlaid > 5 && superseded > 5,
        "the fixture must exercise both directions"
    );
    assert_eq!(stats.top_updates, overlaid);
}

/// A resync or reconnect must drop the overlay along with the book.
#[test]
fn a_reset_drops_the_overlay() {
    let mut r = Running::new(Binance::new(spec(), "BTCUSDT").with_top_of_book());
    let lines: Vec<&str> = BINANCE.lines().collect();
    let first_depth = lines
        .iter()
        .position(|l| l.contains("depthUpdate"))
        .unwrap();
    for l in &lines[..=first_depth + 50] {
        r.text(l);
    }
    assert!(r.feed.stats().top_updates > 0);
    r.feed.on_disconnected(2, &mut r.cmds);
    r.feed.on_connected(3, &mut r.cmds);
    // Tickers alone don't make a book: ignored until the next depth snapshot.
    let ticker = lines[first_depth + 51..]
        .iter()
        .find(|l| l.contains("bookTicker"))
        .unwrap();
    let s = r.text(ticker);
    assert!(s.bids().is_empty() && s.asks().is_empty());
    assert_eq!(s.meta.status, VenueStatus::Syncing);
}

// ---------------------------------------------------------------------------
// The sequencing rules on synthetic messages built to disagree. (Real data is
// self-consistent: when depth catches up with an overlay, the overlay's prices *are* the
// depth's top, so keeping a stale overlay would go unnoticed on recordings. Mutation
// testing showed exactly that.)

fn bn_depth(u: u64, bid: &str, ask: &str) -> String {
    format!(
        r#"{{"e":"depthUpdate","E":1,"T":1,"s":"BTCUSDT","U":{u},"u":{u},"pu":{},"b":[["{bid}","1.000"],["50000.0","1.000"]],"a":[["{ask}","1.000"],["70000.0","1.000"]]}}"#,
        u - 1
    )
}
fn bn_ticker(u: u64, bid: &str, ask: &str) -> String {
    format!(
        r#"{{"e":"bookTicker","u":{u},"s":"BTCUSDT","b":"{bid}","B":"2.000","a":"{ask}","A":"2.000","T":1,"E":1}}"#
    )
}
fn top(s: &Snapshot) -> (String, String) {
    let spec = spec();
    (
        spec.fmt_px(s.bids()[0].px).to_string(),
        spec.fmt_px(s.asks()[0].px).to_string(),
    )
}
fn t(bid: &str, ask: &str) -> (String, String) {
    (bid.to_string(), ask.to_string())
}

#[test]
fn binance_newest_sequence_wins_in_both_directions() {
    let mut r = Running::new(Binance::new(spec(), "BTCUSDT").with_top_of_book());
    assert_eq!(
        top(&r.text(&bn_depth(100, "60000", "60001"))),
        t("60000", "60001")
    );
    // A newer ticker wins.
    assert_eq!(
        top(&r.text(&bn_ticker(105, "60000.5", "60000.6"))),
        t("60000.5", "60000.6")
    );
    // An older ticker (u 103 < 105) is ignored.
    assert_eq!(
        top(&r.text(&bn_ticker(103, "59999", "60009"))),
        t("60000.5", "60000.6")
    );
    // Depth catches up (u 110 > 105) with a *different* top: depth wins, overlay gone.
    assert_eq!(
        top(&r.text(&bn_depth(110, "59990", "59991"))),
        t("59990", "59991")
    );
    // A ticker older than that depth (108) is ignored, not laid over it.
    assert_eq!(
        top(&r.text(&bn_ticker(108, "60000.5", "60000.6"))),
        t("59990", "59991")
    );
    // A newer one (111) wins again.
    assert_eq!(
        top(&r.text(&bn_ticker(111, "59992", "59993"))),
        t("59992", "59993")
    );
    // A crossed ticker is rejected and changes nothing.
    assert_eq!(
        top(&r.text(&bn_ticker(112, "59995", "59994"))),
        t("59992", "59993")
    );
    assert_eq!(r.feed.stats().rejected, 1);
}

fn bb_msg(topic: &str, kind: &str, u: u64, seq: u64, bid: &str, ask: &str) -> String {
    format!(
        r#"{{"topic":"{topic}.BTCUSDT","type":"{kind}","ts":1,"data":{{"s":"BTCUSDT","b":[["{bid}","1"]],"a":[["{ask}","1"]],"u":{u},"seq":{seq}}}}}"#
    )
}

#[test]
fn bybit_newest_seq_wins_in_both_directions() {
    let depth = |u, seq, b, a| bb_msg("orderbook.50", "snapshot", u, seq, b, a);
    let top1 = |u, seq, b, a| bb_msg("orderbook.1", "snapshot", u, seq, b, a);
    let mut r = Running::new(Bybit::new(spec(), "BTCUSDT").with_top_of_book());
    assert_eq!(
        top(&r.text(&depth(1, 1000, "60000", "60001"))),
        t("60000", "60001")
    );
    assert_eq!(
        top(&r.text(&top1(1, 1005, "60000.5", "60000.6"))),
        t("60000.5", "60000.6")
    );
    // Older top (seq 1003 < 1005): ignored.
    assert_eq!(
        top(&r.text(&top1(2, 1003, "59999", "60009"))),
        t("60000.5", "60000.6")
    );
    // Depth with seq 1010 and a different top supersedes the overlay.
    assert_eq!(
        top(&r.text(&depth(2, 1010, "59990", "59991"))),
        t("59990", "59991")
    );
    // A top older than that depth (seq 1008): ignored.
    assert_eq!(
        top(&r.text(&top1(3, 1008, "60000.5", "60000.6"))),
        t("59990", "59991")
    );
    assert_eq!(
        top(&r.text(&top1(4, 1011, "59992", "59993"))),
        t("59992", "59993")
    );
}
