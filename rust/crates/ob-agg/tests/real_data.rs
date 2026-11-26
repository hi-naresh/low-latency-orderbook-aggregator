//! The whole pipeline on the real recordings: parse Binance and Bybit frames, apply them
//! to each venue's book, publish through the seqlock, and aggregate.
//!
//! The two recordings were made back to back (about 15 s apart), so their prices are
//! close but not simultaneous; the assertions only rely on what must hold regardless.
#![allow(clippy::unwrap_used)]

use ob_agg::{Aggregator, Config, VenueConfig, VenueState};
use ob_core::{InstrumentSpec, OrderBook, Px};
use ob_parse::binance::{BinanceDepthParser, DepthUpdate};
use ob_parse::bybit::{BookMsg, BybitBookParser, Message, Sequence};
use ob_sync::{BookMeta, VenueStatus, book_channel};

const BINANCE: &str = include_str!("../../ob-parse/tests/fixtures/binance_depth20.jsonl");
const BYBIT: &str = include_str!("../../ob-parse/tests/fixtures/bybit_orderbook50.jsonl");

#[test]
fn binance_and_bybit_aggregate_correctly() {
    let spec = InstrumentSpec::from_steps("0.1", "0.001").unwrap();
    let (mut bn_pub, bn_reader) = book_channel();
    let (mut bb_pub, bb_reader) = book_channel();
    let mut agg = Aggregator::new(Config::default());
    let bn = agg
        .add_venue(
            VenueConfig {
                name: "binance",
                depth: 20,
                taker_fee_ppm: 0,
            },
            bn_reader,
        )
        .unwrap();
    let bb = agg
        .add_venue(
            VenueConfig {
                name: "bybit",
                depth: 50,
                taker_fee_ppm: 0,
            },
            bb_reader,
        )
        .unwrap();

    // Binance: every message replaces the book.
    let parser = BinanceDepthParser::new(spec, b"BTCUSDT");
    let (mut book, mut msg) = (OrderBook::<64>::new(), DepthUpdate::default());
    for frame in BINANCE.lines().skip(1) {
        parser.parse(frame.as_bytes(), &mut msg).unwrap();
        msg.apply_to(&mut book).unwrap();
        bn_pub.publish(
            &book,
            BookMeta {
                update_id: msg.final_update_id,
                exchange_ts: msg.event_time,
                recv_ns: 1,
                status: VenueStatus::Live,
            },
        );
    }
    let binance_book = book.clone();

    // Bybit: snapshot then deltas, checked for sequence gaps.
    let parser = BybitBookParser::new(spec, b"orderbook.50.BTCUSDT", b"BTCUSDT");
    let (mut book, mut msg, mut last) = (OrderBook::<64>::new(), BookMsg::default(), None);
    for frame in BYBIT.lines() {
        if parser.parse(frame.as_bytes(), &mut msg).unwrap() != Message::Book {
            continue;
        }
        assert!(matches!(
            msg.sequence(last),
            Sequence::Reset | Sequence::Next
        ));
        msg.apply_to(&mut book).unwrap();
        last = Some(msg.update_id);
        bb_pub.publish(
            &book,
            BookMeta {
                update_id: msg.update_id,
                exchange_ts: msg.ts,
                recv_ns: 1,
                status: VenueStatus::Live,
            },
        );
    }
    let bybit_book = book.clone();

    assert_eq!(agg.refresh(), 2);
    assert_eq!(agg.state(bn, 1), Some(VenueState::Included));
    assert_eq!(agg.state(bb, 1), Some(VenueState::Included));

    // Each venue's best bid is its own book's best bid; the aggregate is the higher one.
    let bids = agg.best_bids(1);
    assert_eq!(bids.len(), 2);
    let best = binance_book
        .best_bid()
        .unwrap()
        .px
        .max(bybit_book.best_bid().unwrap().px);
    assert_eq!(bids[0].px, best);
    assert!(bids[0].px >= bids[1].px);
    let asks = agg.best_asks(1);
    let best = binance_book
        .best_ask()
        .unwrap()
        .px
        .min(bybit_book.best_ask().unwrap().px);
    assert_eq!(asks[0].px, best);

    // Volume within $5 of each venue's best bid adds up to the sum of each book's levels.
    let limit = Px::from_raw(bids[0].px.raw() - 50);
    let vol = agg.bid_volume_at_or_above(limit, 1);
    let expect: i64 = [&binance_book, &bybit_book]
        .iter()
        .flat_map(|b| {
            b.bids()
                .iter()
                .filter(|l| l.px >= limit)
                .map(|l| l.qty.raw())
                .collect::<Vec<_>>()
        })
        .sum();
    assert_eq!(vol.total.unwrap().raw(), expect);

    // The display is in real units: e.g. "58647.4".
    eprintln!(
        "best bid {} on {}, best ask {} on {}; bid volume ≥ {}: {} BTC ({})",
        spec.fmt_px(bids[0].px),
        agg.venue(bids[0].venue).unwrap().name,
        spec.fmt_px(asks[0].px),
        agg.venue(asks[0].venue).unwrap().name,
        spec.fmt_px(limit),
        spec.fmt_qty(vol.total.unwrap()),
        if vol.complete {
            "complete"
        } else {
            "more may exist beyond visible depth"
        },
    );
}
