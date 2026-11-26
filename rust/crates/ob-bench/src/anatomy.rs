//! Where a message's handling time goes: parsing vs applying (step 25).
//!
//! For every recorded message: the parser alone (a pure function: the same bytes give
//! the same work, so it can be repeated), and the whole handler call on a live feed
//! (parse + sequence checks + apply + publish, on the book as it really is at that point
//! of the stream). Medians over many passes; "apply + publish" is the difference.

use ob_core::InstrumentSpec;
use ob_feed::{Commands, mono_ns};
use ob_parse::binance::{BinanceDepthParser, DepthUpdate};
use ob_parse::bybit::{BookMsg, BybitBookParser};
use ob_sync::book_channel;

use crate::scenario::{feed_for, spec};
use crate::stream::{Kind, Stream};

/// One recorded message's costs, ns (medians).
#[derive(Clone, Copy, Debug)]
pub struct Anatomy {
    /// Line in the recording.
    pub line: usize,
    /// Bytes.
    pub bytes: usize,
    /// Price levels in it (both sides).
    pub levels: usize,
    /// The whole handler call.
    pub whole_ns: u64,
    /// The parser alone.
    pub parse_ns: u64,
}

impl Anatomy {
    /// Apply + publish (+ sequence checks): the whole call minus parsing.
    pub fn rest_ns(&self) -> u64 {
        self.whole_ns.saturating_sub(self.parse_ns)
    }
}

/// Per-message anatomy over `passes` passes of `kind`'s recording, sorted slowest first.
pub fn anatomy(kind: Kind, passes: usize, clock: &quanta::Clock) -> Vec<Anatomy> {
    let s: InstrumentSpec = spec();
    let stream = Stream::new(kind);
    let lines: Vec<&'static str> = stream.lines().to_vec();
    let n = lines.len();
    let mut whole: Vec<Vec<u64>> = vec![Vec::with_capacity(passes); n];
    let mut parse: Vec<Vec<u64>> = vec![Vec::with_capacity(passes); n];
    let mut levels = vec![0_usize; n];

    // The parser alone, on every line.
    let bn = BinanceDepthParser::new(s, b"BTCUSDT");
    let bb = BybitBookParser::new(s, b"orderbook.50.BTCUSDT", b"BTCUSDT")
        .with_top_topic(b"orderbook.1.BTCUSDT");
    let (mut du, mut bm) = (DepthUpdate::default(), BookMsg::default());
    for _ in 0..passes {
        for (i, l) in lines.iter().enumerate() {
            let t0 = clock.raw();
            let lv = match kind {
                Kind::BybitTop => bb
                    .parse(l.as_bytes(), &mut bm)
                    .map_or(0, |_| bm.bids.as_slice().len() + bm.asks.as_slice().len()),
                _ => bn
                    .parse(l.as_bytes(), &mut du)
                    .map_or(0, |_| du.bids.as_slice().len() + du.asks.as_slice().len()),
            };
            let t1 = clock.raw();
            parse[i].push(clock.delta_as_nanos(t0, t1));
            levels[i] = lv;
        }
    }

    // The whole call, on a live feed: the same messages in stream order (full depth
    // re-synced per pass, outside the timing, as in the pipeline scenario).
    let (publisher, _reader) = book_channel();
    let mut feed = feed_for(kind, publisher, clock);
    let mut stream = Stream::new(kind);
    let mut cmds = Commands::default();
    feed.on_connected(mono_ns(), &mut cmds);
    let full = kind == Kind::BinanceFull;
    for pass in 0..passes {
        if let Some(snapshot) = kind.snapshot() {
            if pass > 0 {
                feed.on_disconnected(mono_ns(), &mut cmds);
                feed.on_connected(mono_ns(), &mut cmds);
            }
            feed.on_text(mono_ns(), lines[0].as_bytes(), &mut cmds);
            feed.on_snapshot(mono_ns(), snapshot.as_bytes(), &mut cmds);
        }
        while cmds.pop().is_some() {}
        for (i, line) in lines.iter().enumerate() {
            if full && i == 0 {
                continue; // the trigger, handled in the re-sync
            }
            let msg = if full {
                line.as_bytes()
            } else {
                stream.next_message()
            };
            let now = mono_ns();
            let t0 = clock.raw();
            feed.on_text(now, msg, &mut cmds);
            let t1 = clock.raw();
            whole[i].push(clock.delta_as_nanos(t0, t1));
            while cmds.pop().is_some() {}
        }
    }

    let median = |v: &mut Vec<u64>| {
        v.sort_unstable();
        v.get(v.len() / 2).copied().unwrap_or(0)
    };
    let measured: Vec<usize> = (0..n).filter(|&i| !whole[i].is_empty()).collect();
    let mut out: Vec<Anatomy> = measured
        .into_iter()
        .map(|i| Anatomy {
            line: i,
            bytes: lines[i].len(),
            levels: levels[i],
            whole_ns: median(&mut whole[i]),
            parse_ns: median(&mut parse[i]),
        })
        .collect();
    out.sort_by_key(|a| std::cmp::Reverse(a.whole_ns));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_line_is_measured_and_parsing_is_part_of_the_whole() {
        let clock = quanta::Clock::new();
        for kind in [Kind::BybitTop, Kind::BinanceFull] {
            let a = anatomy(kind, 5, &clock);
            assert!(a.len() >= 99, "{kind:?}: {}", a.len());
            assert!(a.iter().any(|x| x.levels > 0));
            assert!(a.windows(2).all(|w| w[0].whole_ns >= w[1].whole_ns));
        }
    }
}
