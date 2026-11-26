//! Recordings: the format round-trips, damage is detected, and a replay reproduces
//! exactly the book that feeding the messages directly produces, every time.
#![allow(clippy::unwrap_used)]

use std::io::Cursor;

use ob_core::InstrumentSpec;
use ob_feed::recording::{Frame, Kind, RecordReader, RecordWriter, Replay, ReplayFeed};
use ob_feed::{Binance, Bybit, Commands, Feed, FeedConfig, Venue};
use ob_sync::{BookReader, Snapshot, book_channel};

const BINANCE: &str = include_str!("../../ob-parse/tests/fixtures/binance_depth20.jsonl");
const BYBIT: &str = include_str!("../../ob-parse/tests/fixtures/bybit_orderbook50.jsonl");

fn spec() -> InstrumentSpec {
    InstrumentSpec::from_steps("0.1", "0.001").unwrap()
}

/// Both fixtures interleaved as they might arrive live, each venue starting with a
/// connection record.
fn recording() -> Vec<u8> {
    let mut w = RecordWriter::new(Vec::new()).unwrap();
    let (mut t, mut wall) = (1_000_u64, 1_795_610_800_000_000_000_u64);
    for venue in [0, 1] {
        w.write(&Frame {
            wall_ns: wall,
            mono_ns: t,
            venue,
            kind: Kind::Connected,
            text: b"",
        })
        .unwrap();
    }
    let (bn, bb): (Vec<&str>, Vec<&str>) = (BINANCE.lines().collect(), BYBIT.lines().collect());
    for i in 0..bn.len().max(bb.len()) {
        for (venue, lines) in [(0_u8, &bn), (1, &bb)] {
            if let Some(line) = lines.get(i) {
                t += 1_000;
                wall += 1_000_000;
                w.write(&Frame {
                    wall_ns: wall,
                    mono_ns: t,
                    venue,
                    kind: Kind::Text,
                    text: line.as_bytes(),
                })
                .unwrap();
            }
        }
    }
    assert_eq!(w.records(), 2 + 120);
    w.finish().unwrap()
}

fn snapshot(r: &BookReader) -> Snapshot {
    let mut s = Snapshot::default();
    r.read_into(&mut s);
    s
}

fn replay(bytes: &[u8]) -> (Vec<Snapshot>, Vec<Box<dyn Replay>>) {
    let (bn_pub, bn_books) = book_channel();
    let (bb_pub, bb_books) = book_channel();
    let mut feeds: Vec<Box<dyn Replay>> = vec![
        Box::new(ReplayFeed::new(Binance::new(spec(), "BTCUSDT"), bn_pub)),
        Box::new(ReplayFeed::new(Bybit::new(spec(), "BTCUSDT"), bb_pub)),
    ];
    let mut r = RecordReader::new(Cursor::new(bytes)).unwrap();
    while let Some(frame) = r.read_frame().unwrap() {
        feeds[usize::from(frame.venue)].replay(&frame);
    }
    (vec![snapshot(&bn_books), snapshot(&bb_books)], feeds)
}

/// The book from feeding the lines straight into a feed (no recording involved).
fn direct<V: Venue>(venue: V, fixture: &str) -> Snapshot {
    let (publisher, reader) = book_channel();
    let mut feed = Feed::new(venue, publisher, FeedConfig::default(), 0);
    let mut cmds = Commands::default();
    feed.on_connected(0, &mut cmds);
    for line in fixture.lines() {
        feed.on_text(0, line.as_bytes(), &mut cmds);
    }
    snapshot(&reader)
}

#[test]
fn the_format_round_trips() {
    let bytes = recording();
    let mut r = RecordReader::new(Cursor::new(&bytes)).unwrap();
    let first = r.read_frame().unwrap().unwrap();
    assert_eq!(
        (first.venue, first.kind, first.text),
        (0, Kind::Connected, &b""[..])
    );
    r.read_frame().unwrap().unwrap();
    let text = r.read_frame().unwrap().unwrap();
    assert_eq!(text.text, BINANCE.lines().next().unwrap().as_bytes());
    assert_eq!(
        (text.mono_ns, text.venue, text.kind),
        (2_000, 0, Kind::Text)
    );
    let mut n = 3;
    while r.read_frame().unwrap().is_some() {
        n += 1;
    }
    assert_eq!(n, 122);
}

#[test]
fn replay_rebuilds_exactly_the_directly_built_books_every_time() {
    let bytes = recording();
    let (books, feeds) = replay(&bytes);
    let want = [
        direct(Binance::new(spec(), "BTCUSDT"), BINANCE),
        direct(Bybit::new(spec(), "BTCUSDT"), BYBIT),
    ];
    for (got, want) in books.iter().zip(&want) {
        assert!(!got.bids().is_empty());
        assert_eq!((got.bids(), got.asks()), (want.bids(), want.asks()));
    }
    // Deterministic: a second replay gives the same books and counters.
    let (again, feeds2) = replay(&bytes);
    for (a, b) in books.iter().zip(&again) {
        assert_eq!((a.bids(), a.asks(), a.meta), (b.bids(), b.asks(), b.meta));
    }
    for (f, g) in feeds.iter().zip(&feeds2) {
        assert_eq!(f.stats(), g.stats());
    }
    // Every book update was timed; exchange → us delays were computed from the
    // recorded wall clock.
    assert_eq!(feeds[0].probe().handle().len(), feeds[0].stats().applied);
    assert!(feeds[1].probe().report().contains("exchange → us"));
}

#[test]
fn damage_is_detected_not_misread() {
    let bytes = recording();
    // Wrong magic.
    let mut bad = bytes.clone();
    bad[0] = b'X';
    assert!(RecordReader::new(Cursor::new(&bad)).is_err());
    // Cut mid-record (the recorder was killed): every whole record reads, then an error.
    let cut = &bytes[..bytes.len() - 10];
    let mut r = RecordReader::new(Cursor::new(cut)).unwrap();
    let mut whole = 0;
    let err = loop {
        match r.read_frame() {
            Ok(Some(_)) => whole += 1,
            Ok(None) => panic!("a truncated file must not look complete"),
            Err(e) => break e,
        }
    };
    assert_eq!(
        (whole, err.kind()),
        (121, std::io::ErrorKind::UnexpectedEof)
    );
    // Cut inside a record *header* (a different code path from a cut payload): an
    // error too, not a clean end. (Mutation testing found this case untested.)
    let mut r = RecordReader::new(Cursor::new(&bytes[..8 + 10])).unwrap();
    assert_eq!(
        r.read_frame().unwrap_err().kind(),
        std::io::ErrorKind::UnexpectedEof
    );
    // An absurd length is refused before allocating.
    let mut huge = bytes[..8 + 24].to_vec();
    huge[8 + 20..8 + 24].copy_from_slice(&u32::MAX.to_le_bytes());
    let mut r = RecordReader::new(Cursor::new(&huge)).unwrap();
    assert_eq!(
        r.read_frame().unwrap_err().kind(),
        std::io::ErrorKind::InvalidData
    );
}
