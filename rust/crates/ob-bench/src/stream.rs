//! Endless, valid market-data streams built from the recorded fixtures.
//!
//! A tail measurement needs hundreds of thousands of messages; the recordings hold about
//! a hundred each. Replaying a recording unchanged would break the feeds' sequence
//! rules on the second pass (update ids going backwards → ignored messages or resyncs),
//! so the harness would measure the wrong code path. Instead, pass *k* adds `k × span`
//! to every sequence number and timestamp:
//!
//! * the messages are byte-for-byte the recorded ones except for those digits, so
//!   parsing cost is the real cost;
//! * ids keep increasing across passes;
//! * **Binance diff continuity is kept exactly:** `span = max(u) − min(pu)`, so the first
//!   event of pass *k + 1* has `pu` equal to the last `u` of pass *k* (the rule full
//!   depth checks). Bybit streams start each pass with a snapshot, which resets.
//!
//! The harness checks the outcome: after a run, the feed's counters must show no
//! resyncs, rejections or reconnects.

/// Which recorded stream, and which venue configuration reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Binance `depth20@100ms` + `bookTicker` (the `ob live` default).
    BinanceTop,
    /// Bybit `orderbook.50` + `orderbook.1` (the `ob live` default).
    BybitTop,
    /// Binance full depth: the diff stream over a 1,000-level REST snapshot.
    BinanceFull,
}

macro_rules! fixture {
    ($name:literal) => {
        include_str!(concat!("../../ob-parse/tests/fixtures/", $name))
    };
}

impl Kind {
    /// Short name for reports.
    pub fn name(self) -> &'static str {
        match self {
            Kind::BinanceTop => "binance depth20+bookTicker",
            Kind::BybitTop => "bybit orderbook.50+1",
            Kind::BinanceFull => "binance full depth",
        }
    }

    fn lines(self) -> &'static str {
        match self {
            Kind::BinanceTop => fixture!("binance_depth_and_bookticker.jsonl"),
            Kind::BybitTop => fixture!("bybit_orderbook50_and_1.jsonl"),
            Kind::BinanceFull => fixture!("binance_full_diffs.jsonl"),
        }
    }

    /// The REST snapshot that full depth starts from.
    pub fn snapshot(self) -> Option<&'static str> {
        match self {
            Kind::BinanceFull => Some(fixture!("binance_full_snapshot.json")),
            _ => None,
        }
    }

    fn keys(self) -> &'static [&'static str] {
        match self {
            Kind::BinanceTop | Kind::BinanceFull => &["U", "u", "pu", "E", "T"],
            Kind::BybitTop => &["u", "seq", "ts", "cts"],
        }
    }
}

/// A number inside a recorded line: where its digits are, which key, its value.
#[derive(Clone, Copy, Debug)]
struct Field {
    start: usize,
    end: usize,
    key: usize,
    value: u64,
}

/// An endless stream of messages: the recording, then the recording shifted, and so on.
pub struct Stream {
    kind: Kind,
    lines: Vec<&'static str>,
    fields: Vec<Vec<Field>>,
    /// Added per pass, per key.
    span: Vec<u64>,
    pass: u64,
    next: usize,
    buf: Vec<u8>,
}

impl Stream {
    /// The stream for `kind`, starting at the recording's first message.
    pub fn new(kind: Kind) -> Stream {
        let keys = kind.keys();
        let lines: Vec<&'static str> = kind.lines().lines().filter(|l| !l.is_empty()).collect();
        let fields: Vec<Vec<Field>> = lines.iter().map(|l| find_fields(l, keys)).collect();
        let range = |key: &str| {
            let k = keys.iter().position(|x| *x == key);
            let vals = fields.iter().flatten().filter(|f| Some(f.key) == k);
            let min = vals.clone().map(|f| f.value).min();
            min.zip(vals.map(|f| f.value).max())
        };
        let mut span: Vec<u64> = keys
            .iter()
            .map(|k| range(k).map_or(0, |(lo, hi)| hi - lo + 1))
            .collect();
        // Binance ids: keep `pu` continuity across passes (see the module docs).
        if let (Some((_, max_u)), Some((min_pu, _))) = (range("u"), range("pu")) {
            let continuity = max_u - min_pu;
            for (i, k) in keys.iter().enumerate() {
                if matches!(*k, "U" | "u" | "pu") {
                    span[i] = continuity;
                }
            }
        }
        Stream {
            kind,
            lines,
            fields,
            span,
            pass: 0,
            next: 0,
            buf: Vec::with_capacity(64 * 1024),
        }
    }

    /// Which stream this is.
    pub fn kind(&self) -> Kind {
        self.kind
    }

    /// The recording's lines, unshifted.
    pub fn lines(&self) -> &[&'static str] {
        &self.lines
    }

    /// Per line of the recording: is it a subscription-start message, which a live
    /// connection receives once but a looped recording repeats every pass? The
    /// acknowledgement, and Bybit's 50-level snapshot.
    pub fn seam_mask(&self) -> Vec<bool> {
        self.lines
            .iter()
            .map(|l| {
                let ack = l.contains("\"success\"") || l.contains("\"result\"");
                let bybit_snapshot =
                    l.contains("\"type\":\"snapshot\"") && l.contains("\"orderbook.50.");
                ack || bybit_snapshot
            })
            .collect()
    }

    /// Messages in one pass of the recording.
    pub fn pass_len(&self) -> usize {
        self.lines.len()
    }

    /// The next message. Reuses one buffer: no allocation once it has grown.
    pub fn next_message(&mut self) -> &[u8] {
        let line = self.lines[self.next].as_bytes();
        self.buf.clear();
        if self.pass == 0 {
            self.buf.extend_from_slice(line);
        } else {
            let mut at = 0;
            for f in &self.fields[self.next] {
                self.buf.extend_from_slice(&line[at..f.start]);
                let v = f.value + self.pass * self.span[f.key];
                push_decimal(&mut self.buf, v);
                at = f.end;
            }
            self.buf.extend_from_slice(&line[at..]);
        }
        self.next += 1;
        if self.next == self.lines.len() {
            self.next = 0;
            self.pass += 1;
        }
        &self.buf
    }
}

/// Every `"key":<digits>` in `line`, in order, for the given keys.
fn find_fields(line: &str, keys: &[&str]) -> Vec<Field> {
    let b = line.as_bytes();
    let mut out = Vec::new();
    for (key, name) in keys.iter().enumerate() {
        let pat = format!("\"{name}\":");
        let mut from = 0;
        while let Some(i) = line[from..].find(&pat) {
            let start = from + i + pat.len();
            let end = start + b[start..].iter().take_while(|c| c.is_ascii_digit()).count();
            if end > start
                && let Ok(value) = line[start..end].parse()
            {
                out.push(Field {
                    start,
                    end,
                    key,
                    value,
                });
            }
            from = end.max(start);
        }
    }
    out.sort_by_key(|f| f.start);
    out
}

fn push_decimal(buf: &mut Vec<u8>, mut v: u64) {
    let mut digits = [0_u8; 20];
    let mut i = digits.len();
    loop {
        i -= 1;
        digits[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    buf.extend_from_slice(&digits[i..]);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn num(msg: &[u8], key: &str) -> Option<u64> {
        let s = std::str::from_utf8(msg).unwrap();
        let f = find_fields(s, &[key]);
        f.first().map(|f| f.value)
    }

    #[test]
    fn the_first_pass_is_the_recording_unchanged() {
        for kind in [Kind::BinanceTop, Kind::BybitTop, Kind::BinanceFull] {
            let mut s = Stream::new(kind);
            for line in kind.lines().lines() {
                assert_eq!(s.next_message(), line.as_bytes());
            }
        }
    }

    #[test]
    fn seams_are_the_acks_and_the_bybit_book_snapshot() {
        let count = |k| Stream::new(k).seam_mask().iter().filter(|s| **s).count();
        assert_eq!(count(Kind::BinanceTop), 1); // {"result":null,"id":1}
        assert_eq!(count(Kind::BybitTop), 2); // the ack + the orderbook.50 snapshot
        assert_eq!(count(Kind::BinanceFull), 0);
    }

    #[test]
    fn binance_diff_continuity_holds_across_passes() {
        let mut s = Stream::new(Kind::BinanceFull);
        let n = s.pass_len();
        let mut prev_u = None;
        for _ in 0..3 * n {
            let m = s.next_message().to_vec();
            let (pu, u) = (num(&m, "pu").unwrap(), num(&m, "u").unwrap());
            if let Some(p) = prev_u {
                assert_eq!(pu, p, "each event's pu is the previous event's u");
            }
            prev_u = Some(u);
        }
    }

    #[test]
    fn ids_and_times_keep_increasing_and_only_digits_change() {
        for kind in [Kind::BinanceTop, Kind::BybitTop] {
            let mut s = Stream::new(kind);
            let n = s.pass_len();
            let first: Vec<Vec<u8>> = (0..n).map(|_| s.next_message().to_vec()).collect();
            let second: Vec<Vec<u8>> = (0..n).map(|_| s.next_message().to_vec()).collect();
            for (a, b) in first.iter().zip(&second) {
                let strip = |m: &[u8]| m.iter().filter(|c| !c.is_ascii_digit()).count();
                assert_eq!(strip(a), strip(b), "only digits differ");
                for key in kind.keys() {
                    if let (Some(x), Some(y)) = (num(a, key), num(b, key)) {
                        assert!(y > x, "{key} increases");
                    }
                }
            }
            // Every value in pass 2 is above every value in pass 1, key by key.
            for key in kind.keys() {
                let max1 = first.iter().filter_map(|m| num(m, key)).max();
                let min2 = second.iter().filter_map(|m| num(m, key)).min();
                if let (Some(a), Some(b)) = (max1, min2) {
                    assert!(b > a, "{key}: {b} > {a}");
                }
            }
        }
    }
}
