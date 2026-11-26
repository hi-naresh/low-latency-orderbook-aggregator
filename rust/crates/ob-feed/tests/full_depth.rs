//! Binance full-depth mode (step 19): every sync rule, on synthetic messages built to
//! hit it, plus the feed-level fetch/answer cycle.
#![allow(clippy::unwrap_used)]

use ob_core::{InstrumentSpec, Level, Px, Qty};
use ob_feed::full_depth::{FULL_CAP, MIN_KNOWN};
use ob_feed::{
    BinanceFullDepth, Command, Commands, Feed, FeedConfig, FeedEvent, Outcome, ReconnectReason,
    ResyncReason, State, Venue,
};
use ob_sync::{BookReader, Snapshot, VenueStatus, book_channel};

fn spec() -> InstrumentSpec {
    InstrumentSpec::from_steps("0.1", "0.001").unwrap()
}
fn px(s: &str) -> Px {
    spec().parse_px(s).unwrap()
}
fn lv(p: &str, q: &str) -> Level {
    Level::new(px(p), spec().parse_qty(q).unwrap())
}

/// A diff event. Levels as (price, qty) strings; qty "0" removes.
fn diff(first: u64, last: u64, prev: u64, bids: &[(&str, &str)], asks: &[(&str, &str)]) -> String {
    let side = |v: &[(&str, &str)]| {
        v.iter()
            .map(|(p, q)| format!(r#"["{p}","{q}"]"#))
            .collect::<Vec<_>>()
            .join(",")
    };
    format!(
        r#"{{"e":"depthUpdate","E":7,"T":7,"s":"BTCUSDT","U":{first},"u":{last},"pu":{prev},"b":[{}],"a":[{}]}}"#,
        side(bids),
        side(asks)
    )
}

/// A snapshot with `n` bids (100.0 down by 0.1) and `n` asks (100.1 up by 0.1).
fn snapshot(last_update_id: u64, n: usize) -> String {
    let fmt = |i: usize, base: i64, dir: i64| {
        let raw = base + dir * i64::try_from(i).unwrap();
        format!(r#"["{}.{}","1.000"]"#, raw / 10, raw % 10)
    };
    let bids: Vec<String> = (0..n).map(|i| fmt(i, 1000, -1)).collect();
    let asks: Vec<String> = (0..n).map(|i| fmt(i, 1001, 1)).collect();
    format!(
        r#"{{"lastUpdateId":{last_update_id},"E":5,"T":5,"bids":[{}],"asks":[{}]}}"#,
        bids.join(","),
        asks.join(",")
    )
}

fn venue() -> BinanceFullDepth {
    BinanceFullDepth::new(spec(), "BTCUSDT")
}

/// A venue that has applied a 100-level snapshot with id 100 (the whole side: no
/// boundary) and one straddling event.
fn synced(n: usize) -> BinanceFullDepth {
    let mut v = venue();
    assert_eq!(
        v.on_text(diff(90, 101, 89, &[], &[]).as_bytes()),
        Outcome::NeedSnapshot
    );
    assert!(matches!(
        v.on_snapshot(snapshot(100, n).as_bytes()),
        Outcome::Applied { .. }
    ));
    v
}

#[test]
fn sync_buffers_drops_stale_requires_a_straddle_then_continuity() {
    let mut v = venue();
    // 1. The first event asks for a snapshot; later ones are buffered quietly.
    assert_eq!(
        v.on_text(diff(80, 90, 79, &[("99.0", "5")], &[]).as_bytes()),
        Outcome::NeedSnapshot
    );
    assert_eq!(
        v.on_text(diff(91, 105, 90, &[("100.0", "9")], &[]).as_bytes()),
        Outcome::Ignored
    );
    // 2. Snapshot L = 100: event u=90 is dropped (rule 3); u=105 straddles (91 ≤ 100 ≤ 105).
    let out = v.on_snapshot(snapshot(100, 50).as_bytes());
    assert!(
        matches!(out, Outcome::Applied { update_id: 105, .. }),
        "{out:?}"
    );
    assert_eq!(v.book().bids().best(), Some(lv("100.0", "9"))); // from the buffered event
    assert_eq!(v.book().bids().level(10), Some(lv("99.0", "1"))); // not the dropped u=90's 5
    // 3. Continuity: pu must equal the previous u.
    assert!(matches!(
        v.on_text(diff(106, 110, 105, &[], &[("100.1", "2")]).as_bytes()),
        Outcome::Applied { update_id: 110, .. }
    ));
    assert_eq!(
        v.on_text(diff(112, 115, 111, &[], &[]).as_bytes()),
        Outcome::Resync(ResyncReason::Gap {
            expected: 110,
            got: 111
        })
    );
}

#[test]
fn a_snapshot_older_than_every_event_is_a_gap() {
    let mut v = venue();
    v.on_text(diff(200, 210, 199, &[], &[]).as_bytes());
    // L = 100 but the first event starts at 200: the snapshot can't be stitched on.
    assert_eq!(
        v.on_snapshot(snapshot(100, 50).as_bytes()),
        Outcome::Resync(ResyncReason::Gap {
            expected: 100,
            got: 200
        })
    );
    // A snapshot newer than everything buffered is fine: the next live event straddles.
    let mut v = venue();
    v.on_text(diff(80, 90, 79, &[], &[]).as_bytes());
    assert!(matches!(
        v.on_snapshot(snapshot(100, 50).as_bytes()),
        Outcome::Applied { update_id: 100, .. }
    ));
    assert!(matches!(
        v.on_text(diff(95, 103, 90, &[], &[]).as_bytes()),
        Outcome::Applied { update_id: 103, .. }
    ));
}

#[test]
fn removing_an_absent_level_is_normal_here() {
    let mut v = synced(50);
    // Binance's rule 9: not a resync (unlike Bybit, where it means drift).
    let out = v.on_text(diff(102, 103, 101, &[("12.3", "0")], &[]).as_bytes());
    assert!(matches!(out, Outcome::Applied { .. }), "{out:?}");
}

#[test]
fn a_truncated_snapshot_bounds_the_known_region() {
    // A full 1000-level snapshot (Binance truncated it): the region ends at its worst.
    let mut v = venue();
    v.on_text(diff(90, 101, 89, &[], &[]).as_bytes());
    v.on_snapshot(snapshot(100, 1000).as_bytes());
    let (floor, ceiling) = v.known_region();
    assert_eq!((floor, ceiling), (Some(px("0.1")), Some(px("200.0"))));
    // A deep update beyond the boundary is ignored; inside it applies.
    v.on_text(diff(102, 103, 101, &[("0.0", "0")], &[("250.0", "3")]).as_bytes());
    assert_eq!(v.book().asks().len(), 1000);
    // A shorter snapshot is the whole side: no boundary.
    let v = synced(50);
    assert_eq!(v.known_region(), (None, None));
}

#[test]
fn a_full_book_moves_its_boundary_when_it_drops_or_refuses_a_level() {
    let mut v = venue();
    v.on_text(diff(90, 101, 89, &[], &[]).as_bytes());
    v.on_snapshot(snapshot(100, 1000).as_bytes());
    // Push the bid side past capacity: remove the 30 best asks (100.1 … 103.0), then add
    // 30 new, better bids in the space they leave (100.1 … 103.0).
    let asks_out: Vec<(String, &str)> = (0..30)
        .map(|i| (format!("{}.{}", (1001 + i) / 10, (1001 + i) % 10), "0"))
        .collect();
    let asks_ref: Vec<(&str, &str)> = asks_out.iter().map(|(p, q)| (p.as_str(), *q)).collect();
    v.on_text(diff(102, 103, 101, &[], &asks_ref).as_bytes());
    let new_bids: Vec<(String, &str)> = (1..=30)
        .map(|i| (format!("{}.{}", (1000 + i) / 10, (1000 + i) % 10), "1"))
        .collect();
    let bids_ref: Vec<(&str, &str)> = new_bids.iter().map(|(p, q)| (p.as_str(), *q)).collect();
    v.on_text(diff(104, 105, 103, &bids_ref, &[]).as_bytes());
    assert_eq!(v.book().bids().len(), 1030.min(FULL_CAP));
    // 1030 > 1024: the six worst (0.1 … 0.6) were dropped, so exact knowledge now
    // starts just above 0.6: at 0.7, the worst level kept.
    let worst_kept = v.book().bids().level(FULL_CAP - 1).unwrap().px;
    assert_eq!(worst_kept, px("0.7"));
    assert_eq!(v.known_region().0, Some(px("0.7")));
    // An update at a dropped price is now outside the region: ignored, book unchanged.
    let before = v.book().bids().len();
    v.on_text(diff(106, 107, 105, &[("0.3", "4")], &[]).as_bytes());
    assert_eq!(v.book().bids().len(), before);
}

#[test]
fn a_side_that_runs_short_of_known_levels_asks_for_a_new_snapshot() {
    let mut v = venue();
    v.on_text(diff(90, 101, 89, &[], &[]).as_bytes());
    v.on_snapshot(snapshot(100, 1000).as_bytes());
    // Remove the best 1000 - MIN_KNOWN + 1 bids: fewer than MIN_KNOWN remain known.
    let n = 1000 - MIN_KNOWN + 1;
    let gone: Vec<(String, &str)> = (0..n)
        .map(|i| {
            let raw = 1000 - i64::try_from(i).unwrap();
            (format!("{}.{}", raw / 10, raw % 10), "0")
        })
        .collect();
    let gone_ref: Vec<(&str, &str)> = gone.iter().map(|(p, q)| (p.as_str(), *q)).collect();
    assert_eq!(
        v.on_text(diff(102, 103, 101, &gone_ref, &[]).as_bytes()),
        Outcome::Resync(ResyncReason::Depleted)
    );
}

#[test]
fn a_bad_snapshot_or_a_bad_event_resyncs() {
    let mut v = venue();
    v.on_text(diff(90, 101, 89, &[], &[]).as_bytes());
    assert_eq!(
        v.on_snapshot(b"{\"lastUpdateId\":1}"),
        Outcome::Resync(ResyncReason::BadSnapshot)
    );
    let mut v = synced(50);
    assert!(matches!(
        v.on_text(b"{\"e\":\"depthUpdate\",\"U\":"),
        Outcome::Resync(ResyncReason::Unparseable(_))
    ));
}

#[test]
fn the_overlay_works_on_full_depth_too() {
    let mut v = BinanceFullDepth::new(spec(), "BTCUSDT").with_top_of_book();
    let ticker = |u: u64, b: &str, a: &str| {
        format!(
            r#"{{"e":"bookTicker","u":{u},"s":"BTCUSDT","b":"{b}","B":"2.000","a":"{a}","A":"2.000","T":1,"E":1}}"#
        )
    };
    assert_eq!(
        v.on_text(ticker(50, "99.0", "99.5").as_bytes()),
        Outcome::Ignored,
        "no book yet"
    );
    v.on_text(diff(90, 101, 89, &[], &[]).as_bytes());
    v.on_snapshot(snapshot(100, 50).as_bytes());
    assert!(matches!(
        v.on_text(ticker(120, "100.0", "100.1").as_bytes()),
        Outcome::Applied { top: true, .. }
    ));
    assert_eq!(
        v.on_text(ticker(110, "100.0", "100.1").as_bytes()),
        Outcome::Ignored,
        "older"
    );
}

// ---------------------------------------------------------------------------
// Through the feed: the fetch command, the answer, failures.

fn status(r: &BookReader) -> VenueStatus {
    let mut s = Snapshot::default();
    r.read_into(&mut s);
    s.meta.status
}

#[test]
fn the_feed_asks_once_goes_live_on_the_answer_and_reconnects_on_failure() {
    let (publisher, reader) = book_channel();
    let mut feed = Feed::new(venue(), publisher, FeedConfig::default(), 1);
    let mut cmds = Commands::default();
    feed.on_connected(0, &mut cmds);
    cmds.drain();
    feed.on_text(1, diff(90, 101, 89, &[], &[]).as_bytes(), &mut cmds);
    feed.on_text(2, diff(102, 103, 101, &[], &[]).as_bytes(), &mut cmds);
    let c = cmds.drain();
    assert_eq!(
        c.iter().filter(|c| **c == Command::FetchSnapshot).count(),
        1,
        "asked once: {c:?}"
    );
    assert!(c.contains(&Command::Log(FeedEvent::FetchingSnapshot)));
    assert_eq!(status(&reader), VenueStatus::Syncing);
    feed.on_snapshot(3, snapshot(100, 50).as_bytes(), &mut cmds);
    assert!(matches!(feed.state(), State::Live { .. }));
    assert_eq!(status(&reader), VenueStatus::Live);
    assert_eq!(
        feed.venue().book().bids().best().map(|l| l.qty),
        Qty::from_raw(1000)
    );

    // A failed fetch: give up on this connection (with backoff).
    let (publisher, _reader) = book_channel();
    let mut feed = Feed::new(venue(), publisher, FeedConfig::default(), 1);
    feed.on_connected(0, &mut cmds);
    feed.on_text(1, diff(90, 101, 89, &[], &[]).as_bytes(), &mut cmds);
    cmds.drain();
    feed.on_snapshot_failed(2, &mut cmds);
    let c = cmds.drain();
    assert!(c.contains(&Command::Reconnect));
    assert!(c.contains(&Command::Log(FeedEvent::Reconnecting(
        ReconnectReason::SnapshotFailed
    ))));
}

/// Step 26: the price-indexed book also refuses a level more than its window (8,192
/// ticks, 819.2 here) from the best, and that refusal bounds the known region exactly
/// like a full book's.
#[test]
fn a_level_beyond_the_price_window_bounds_the_region() {
    let mut v = synced(300); // whole sides known: no boundary yet
    assert_eq!(v.known_region(), (None, None));
    // Best ask 100.1; 100.1 + 819.2 = 919.3 is exactly one window away: refused.
    v.on_text(diff(102, 103, 101, &[], &[("919.3", "2")]).as_bytes());
    assert_eq!(v.book().asks().len(), 300);
    assert_eq!(v.known_region().1, Some(px("919.2")));
    // Inside the window it's accepted; beyond the new boundary it's ignored.
    v.on_text(diff(104, 105, 103, &[], &[("900.0", "1"), ("950.0", "1")]).as_bytes());
    assert_eq!(v.book().asks().len(), 301);
    assert_eq!(v.book().asks().worst(), Some(lv("900.0", "1")));
}

/// A better level more than a window above the worst drops the levels left behind; the
/// region then ends just inside the innermost one dropped. Here so many go that fewer
/// than MIN_KNOWN remain, so the venue asks for a new snapshot: the right outcome when
/// the market has moved that far.
#[test]
fn a_jump_beyond_the_window_drops_the_far_levels_then_resyncs() {
    let mut v = synced(300); // bids 100.0 down to 70.1
    // Clear the asks first (a bid above them would cross the book).
    let asks: Vec<(String, &str)> = (0..300)
        .map(|i| (format!("{}.{}", (1001 + i) / 10, (1001 + i) % 10), "0"))
        .collect();
    let asks_ref: Vec<(&str, &str)> = asks.iter().map(|(p, q)| (p.as_str(), *q)).collect();
    v.on_text(diff(102, 103, 101, &[], &asks_ref).as_bytes());
    assert_eq!(v.book().asks().len(), 0);
    // A bid at 904.2: every bid at or below 904.2 - 819.2 = 85.0 is now a window or more
    // below the best, i.e. 70.1 … 85.0.
    let out = v.on_text(diff(104, 105, 103, &[("904.2", "1")], &[]).as_bytes());
    // Bids 70.1 … 85.0 (150 levels) are dropped; 85.0 was the innermost.
    assert_eq!(v.known_region().0, Some(px("85.1")));
    assert_eq!(v.book().bids().worst(), Some(lv("85.1", "1.000")));
    assert_eq!(v.book().bids().len(), 151);
    assert_eq!(out, Outcome::Resync(ResyncReason::Depleted));
}
