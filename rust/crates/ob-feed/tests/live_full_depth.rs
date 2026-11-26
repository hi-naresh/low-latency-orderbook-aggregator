//! Live cross-check of full-depth mode (needs the internet; ignored by default).
//!
//! One connection carries both the diff stream and Binance's own 20-level snapshots
//! (`depth20`), via the combined-stream endpoint (`{"stream":…,"data":…}` wrappers tell
//! them apart).
//!
//! **The two streams are cut a millisecond apart:** each `depth20` snapshot is taken just
//! before the diff event of the same 100 ms window is closed, so its `u` falls *inside*
//! that event's range (`U ≤ depth20.u < u`). Exact equality of update IDs is therefore
//! rare (3 in 30 s; all 3 matched). The check that works on every snapshot:
//! * every price in Binance's top 20 that diff event *k* did **not** touch must have
//!   exactly the quantity our book had before *k* (absent = 0, both ways);
//! * a price that *k* did touch must equal its value before *k* or after *k* (or, rarely,
//!   an intermediate value if it changed twice within the window; counted separately).
//!
//! One subtlety the first run exposed: diff events report each level's **net** change over
//! the window, so a level that appears and vanishes within one window is never reported
//! (absent → absent). `depth20`, cut mid-window, can catch it alive. Such a *transient*
//! mismatch disappears by the next snapshot; a real error in our book would **persist**.
//! So an untouched mismatch counts as wrong only if the same price mismatches again in
//! the next checked snapshot.
//!
//! Run: `cargo test -p ob-feed --release --test live_full_depth -- --ignored --nocapture`
#![allow(
    clippy::unwrap_used,
    clippy::print_stdout,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

use std::collections::{BTreeMap, HashSet};
use std::time::{Duration, Instant};

use ob_core::{InstrumentSpec, Level, Px, Side};
use ob_feed::{BinanceFullDepth, Outcome, Venue};
use ob_net::ws::Message;
use ob_net::{ConnConfig, WsConnection, http};
use ob_parse::binance::{BinanceDepthParser, DepthUpdate};

/// Our book's top 64 per side, as price → quantity.
type State = BTreeMap<(bool, Px), i64>;

fn state(v: &BinanceFullDepth) -> State {
    let mut m = State::new();
    for l in v.book().bids().iter().take(64) {
        m.insert((true, l.px), l.qty.raw());
    }
    for l in v.book().asks().iter().take(64) {
        m.insert((false, l.px), l.qty.raw());
    }
    m
}

#[test]
#[ignore = "needs the internet"]
fn full_depth_agrees_with_binance_depth20_at_every_snapshot() {
    let spec = InstrumentSpec::from_steps("0.1", "0.001").unwrap();
    let mut venue = BinanceFullDepth::new(spec, "BTCUSDT");
    let parser = BinanceDepthParser::new(spec, b"BTCUSDT");
    let (mut d20, mut diff) = (DepthUpdate::default(), DepthUpdate::default());
    let path = "/stream?streams=btcusdt@depth@100ms/btcusdt@depth20@100ms";
    let mut conn =
        WsConnection::connect(&ConnConfig::new("fstream.binance.com", path).unwrap()).unwrap();
    let rest = ConnConfig::new("fapi.binance.com", "/").unwrap();
    let mut pending: Option<(u64, Vec<Level>, Vec<Level>)> = None; // a depth20 awaiting its diff
    let mut before = State::new();
    let (mut applied, mut snapshots, mut resyncs, mut apply_ns) = (0, 0, 0, Vec::new());
    let (mut checked, mut untouched_ok, mut touched_ok, mut intermediate) = (0, 0, 0, 0);
    let (mut transient, mut persistent) = (0, 0);
    let mut last_mismatches: HashSet<(bool, Px)> = HashSet::new();
    let end = Instant::now() + Duration::from_secs(30);
    while Instant::now() < end {
        let Some(Message::Text(t)) = conn.poll().unwrap() else {
            continue;
        };
        let t = t.to_vec();
        let Some(start) = t.windows(8).position(|w| w == b"\"data\":{") else {
            continue;
        };
        let inner = &t[start + 7..t.len() - 1];
        if t.starts_with(br#"{"stream":"btcusdt@depth20@100ms""#) {
            parser.parse(inner, &mut d20).unwrap();
            pending = Some((
                d20.final_update_id,
                d20.bids.as_slice().to_vec(),
                d20.asks.as_slice().to_vec(),
            ));
            continue;
        }
        parser.parse(inner, &mut diff).unwrap();
        let t0 = Instant::now();
        let mut outcome = venue.on_text(inner);
        let took = t0.elapsed().as_nanos(); // the event alone, not a snapshot fetch
        if outcome == Outcome::NeedSnapshot {
            snapshots += 1;
            let body = http::get(&rest, venue.snapshot_endpoint().unwrap().1, 8 << 20).unwrap();
            outcome = venue.on_snapshot(&body);
        }
        match outcome {
            Outcome::Applied { .. } => {
                apply_ns.push(took);
                applied += 1;
                let after = state(&venue);
                // Evaluate the depth20 snapshot that falls inside this diff event.
                if let Some((u, bids, asks)) = pending.take()
                    && !before.is_empty()
                    && diff.first_update_id <= u
                    && u < diff.final_update_id
                {
                    checked += 1;
                    let mut mismatches = HashSet::new();
                    let touched: HashSet<(bool, Px)> = diff
                        .bids
                        .as_slice()
                        .iter()
                        .map(|l| (true, l.px))
                        .chain(diff.asks.as_slice().iter().map(|l| (false, l.px)))
                        .collect();
                    // Every price within Binance's top-20 range, on each side.
                    let theirs: BTreeMap<(bool, Px), i64> = bids
                        .iter()
                        .map(|l| ((true, l.px), l.qty.raw()))
                        .chain(asks.iter().map(|l| ((false, l.px), l.qty.raw())))
                        .collect();
                    let (low_bid, high_ask) = (bids.last().unwrap().px, asks.last().unwrap().px);
                    let in_range =
                        |k: &(bool, Px)| if k.0 { k.1 >= low_bid } else { k.1 <= high_ask };
                    let keys: HashSet<(bool, Px)> = theirs
                        .keys()
                        .copied()
                        .chain(before.keys().copied().filter(in_range))
                        .chain(after.keys().copied().filter(in_range))
                        .collect();
                    for k in keys {
                        let (want, b, a) = (
                            theirs.get(&k).copied().unwrap_or(0),
                            before.get(&k).copied().unwrap_or(0),
                            after.get(&k).copied().unwrap_or(0),
                        );
                        if !touched.contains(&k) {
                            if want == b {
                                untouched_ok += 1;
                            } else {
                                if last_mismatches.contains(&k) {
                                    persistent += 1;
                                    println!(
                                        "PERSISTENT {:?} {}: theirs {want} ours {b}",
                                        if k.0 { Side::Bid } else { Side::Ask },
                                        spec.fmt_px(k.1)
                                    );
                                } else {
                                    transient += 1;
                                }
                                mismatches.insert(k);
                            }
                        } else if want == b || want == a {
                            touched_ok += 1;
                        } else {
                            intermediate += 1;
                        }
                    }
                    last_mismatches = mismatches;
                }
                before = after;
            }
            Outcome::Resync(r) => {
                println!("resync: {r:?}");
                resyncs += 1;
                venue.reset();
                before.clear();
            }
            _ => {}
        }
    }
    apply_ns.sort_unstable();
    let p = |q: f64| apply_ns[((apply_ns.len() - 1) as f64 * q) as usize] / 1000;
    println!("diff events applied {applied}, snapshots {snapshots}, resyncs {resyncs}");
    println!(
        "apply time per diff event (µs): p50 {} p90 {} p99 {} max {}",
        p(0.5),
        p(0.9),
        p(0.99),
        p(1.0)
    );
    println!(
        "depth20 snapshots checked {checked}: untouched levels equal {untouched_ok}, touched levels before/after {touched_ok}, intermediate {intermediate}"
    );
    println!(
        "untouched mismatches: transient {transient} (gone by the next snapshot), PERSISTENT {persistent}"
    );
    let (floor, ceiling) = venue.known_region();
    println!(
        "known region: bids >= {:?}, asks <= {:?}; book holds {} bids, {} asks",
        floor.map(|p| spec.fmt_px(p).to_string()),
        ceiling.map(|p| spec.fmt_px(p).to_string()),
        venue.book().bids().len(),
        venue.book().asks().len()
    );
    assert!(checked > 200, "too few snapshots checked");
    assert!(untouched_ok > 1000);
    assert_eq!(
        persistent, 0,
        "our book disagrees with Binance's at a price, persistently"
    );
}
