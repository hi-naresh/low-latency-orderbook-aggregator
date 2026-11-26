//! A minimal, allocation-free JSON scanner over raw bytes.
//!
//! Exchange messages have a fixed, known shape, so we don't need a general JSON
//! library that builds a tree. This cursor reads exactly what a venue parser asks for
//! (a key, an unsigned integer, a decimal in quotes) and **skips** everything else, while
//! still checking that what it skips is valid JSON (RFC 8259): numbers, strings,
//! escapes, literals, nesting.
//!
//! Deliberate limits:
//! * Strings are returned as raw bytes. Escapes are validated but not decoded, so an
//!   escaped key like `"\u0045"` doesn't match `"E"` (venues never escape keys or
//!   numbers). Invalid UTF-8 inside strings is not checked (the venue's TLS stream is
//!   trusted to be text; checking would cost time on every message).
//! * Nesting deeper than [`MAX_DEPTH`] is rejected, so hostile input can't exhaust anything.
//! * The cursor never panics and never reads out of bounds, whatever the input
//!   (property-tested and fuzzed).

use crate::decimal;
use ob_core::ParseFixedError;

/// Maximum nesting of arrays/objects (the same limit serde_json uses by default).
pub(crate) const MAX_DEPTH: usize = 128;

/// What went wrong, without allocating. The byte offset is in [`JsonError::offset`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum JsonErrorKind {
    /// The input ended in the middle of a value.
    Truncated,
    /// A byte that isn't allowed at this point.
    Unexpected,
    /// A malformed number (leading zero, missing digits, ...).
    BadNumber,
    /// An integer field that is negative, fractional or too large for `u64`.
    NotU64,
    /// A malformed escape sequence, or a raw control character in a string.
    BadString,
    /// Nesting deeper than 128 levels.
    TooDeep,
    /// Bytes after the end of the top-level value.
    TrailingBytes,
    /// A quoted decimal (price or quantity) that isn't a valid number at the scale.
    Decimal(ParseFixedError),
}

/// A JSON scanning error and where it happened.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct JsonError {
    /// What went wrong.
    pub kind: JsonErrorKind,
    /// Byte offset into the message.
    pub offset: usize,
}

/// A read position in a message. Every method either consumes a complete value or
/// returns an error; nothing panics.
pub(crate) struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

type Result<T> = core::result::Result<T, JsonError>;

impl<'a> Cursor<'a> {
    pub(crate) const fn new(buf: &'a [u8]) -> Self {
        Cursor { buf, pos: 0 }
    }

    pub(crate) const fn pos(&self) -> usize {
        self.pos
    }

    #[cold]
    fn err<T>(&self, kind: JsonErrorKind) -> Result<T> {
        Err(JsonError {
            kind,
            offset: self.pos,
        })
    }

    /// Skips JSON whitespace and returns the next byte without consuming it.
    #[inline]
    pub(crate) fn peek(&mut self) -> Option<u8> {
        while let Some(&b) = self.buf.get(self.pos) {
            if !matches!(b, b' ' | b'\t' | b'\n' | b'\r') {
                return Some(b);
            }
            self.pos += 1;
        }
        None
    }

    /// Skips whitespace, then consumes `byte` or fails.
    #[inline]
    pub(crate) fn expect(&mut self, byte: u8) -> Result<()> {
        match self.peek() {
            Some(b) if b == byte => {
                self.pos += 1;
                Ok(())
            }
            Some(_) => self.err(JsonErrorKind::Unexpected),
            None => self.err(JsonErrorKind::Truncated),
        }
    }

    /// After an element of an object or array: consumes `,` (→ `true`, more follow) or
    /// `close` (→ `false`, done).
    #[inline]
    pub(crate) fn next_or_close(&mut self, close: u8) -> Result<bool> {
        match self.peek() {
            Some(b',') => {
                self.pos += 1;
                Ok(true)
            }
            Some(b) if b == close => {
                self.pos += 1;
                Ok(false)
            }
            Some(_) => self.err(JsonErrorKind::Unexpected),
            None => self.err(JsonErrorKind::Truncated),
        }
    }

    /// At the start of an object or array: consumes `close` if it's empty (→ `false`),
    /// otherwise leaves the first element in place (→ `true`).
    #[inline]
    pub(crate) fn has_first(&mut self, close: u8) -> Result<bool> {
        match self.peek() {
            Some(b) if b == close => {
                self.pos += 1;
                Ok(false)
            }
            Some(_) => Ok(true),
            None => self.err(JsonErrorKind::Truncated),
        }
    }

    /// Requires that only whitespace remains.
    pub(crate) fn end(&mut self) -> Result<()> {
        match self.peek() {
            None => Ok(()),
            Some(_) => self.err(JsonErrorKind::TrailingBytes),
        }
    }

    /// A string; returns its raw contents (between the quotes, escapes not decoded).
    #[inline]
    pub(crate) fn string(&mut self) -> Result<&'a [u8]> {
        self.expect(b'"')?;
        let start = self.pos;
        loop {
            let Some(&b) = self.buf.get(self.pos) else {
                return self.err(JsonErrorKind::Truncated);
            };
            match b {
                b'"' => {
                    let s = &self.buf[start..self.pos];
                    self.pos += 1;
                    return Ok(s);
                }
                b'\\' => self.escape()?,
                0x00..=0x1F => return self.err(JsonErrorKind::BadString),
                _ => self.pos += 1,
            }
        }
    }

    /// Validates one escape sequence starting at the backslash.
    #[cold]
    fn escape(&mut self) -> Result<()> {
        match self.buf.get(self.pos + 1) {
            Some(b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't') => {
                self.pos += 2;
                Ok(())
            }
            Some(b'u') => {
                let hex = self.buf.get(self.pos + 2..self.pos + 6);
                match hex {
                    Some(h) if h.iter().all(u8::is_ascii_hexdigit) => {
                        self.pos += 6;
                        Ok(())
                    }
                    Some(_) => self.err(JsonErrorKind::BadString),
                    None => self.err(JsonErrorKind::Truncated),
                }
            }
            Some(_) => self.err(JsonErrorKind::BadString),
            None => self.err(JsonErrorKind::Truncated),
        }
    }

    /// A non-negative JSON integer that fits in `u64` (timestamps, update IDs).
    #[inline]
    pub(crate) fn u64(&mut self) -> Result<u64> {
        let start = match self.peek() {
            Some(_) => self.pos,
            None => return self.err(JsonErrorKind::Truncated),
        };
        let mut value: u64 = 0;
        while let Some(&b) = self.buf.get(self.pos) {
            let d = b.wrapping_sub(b'0');
            if d >= 10 {
                break;
            }
            value = match value
                .checked_mul(10)
                .and_then(|v| v.checked_add(u64::from(d)))
            {
                Some(v) => v,
                None => return self.err(JsonErrorKind::NotU64),
            };
            self.pos += 1;
        }
        let digits = self.pos - start;
        match self.buf.get(self.pos) {
            // A fraction, exponent or minus sign: valid JSON maybe, but not a u64.
            Some(b'.' | b'e' | b'E' | b'-' | b'+') => self.err(JsonErrorKind::NotU64),
            _ if digits == 0 => self.err(JsonErrorKind::Unexpected),
            _ if digits > 1 && self.buf[start] == b'0' => {
                self.pos = start;
                self.err(JsonErrorKind::BadNumber) // leading zero
            }
            _ => Ok(value),
        }
    }

    /// A price or quantity sent as a quoted decimal (`"58631.90"`), parsed straight into
    /// a raw integer at `decimals` places.
    ///
    /// The digits are converted **while** looking for the closing quote: one pass over
    /// the bytes, not "find the string, then parse it". Anything unusual (escapes, minus
    /// signs, junk, too many digits) goes to the cold path, which finds the end of the
    /// string properly and hands it to the exact reference logic.
    #[inline]
    pub(crate) fn decimal(&mut self, decimals: u8) -> Result<i64> {
        self.expect(b'"')?;
        let start = self.pos;
        let mut value: u64 = 0;
        let mut digits = 0_usize;
        let mut dot = usize::MAX;
        loop {
            let Some(&b) = self.buf.get(self.pos) else {
                return self.err(JsonErrorKind::Truncated);
            };
            let d = b.wrapping_sub(b'0');
            if d < 10 {
                value = value.wrapping_mul(10).wrapping_add(u64::from(d));
                digits += 1;
            } else if b == b'.' && dot == usize::MAX {
                dot = self.pos;
            } else if b == b'"' {
                break;
            } else {
                return self.decimal_slow(start, decimals);
            }
            self.pos += 1;
        }
        let end = self.pos;
        self.pos += 1; // closing quote
        // Same acceptance rules as `decimal::scalar`: ≤ 18 digits, no ".5" / "5.".
        // (`dot == usize::MAX` means "no point"; don't add to it.)
        let has_dot = dot != usize::MAX;
        if digits == 0 || digits > 18 || has_dot && (dot == start || dot + 1 == end) {
            return self.decimal_exact(start, end, decimals);
        }
        let frac = if has_dot { end - dot - 1 } else { 0 };
        match decimal::finish(value, frac, decimals) {
            Some(raw) => Ok(raw),
            None => self.decimal_exact(start, end, decimals),
        }
    }

    /// Fast path for one price level `["price","qty"]`: parsed by the SIMD
    /// kernel when the next 32 bytes hold one in exactly that shape, with the same
    /// scaling as [`Cursor::decimal`]. `None` (nothing consumed) means: parse it the
    /// scalar way, which gives the identical result or the precise error.
    #[inline]
    pub(crate) fn level_fast(&mut self, px_decimals: u8, qty_decimals: u8) -> Option<(i64, i64)> {
        let window: &[u8; 32] = self.buf.get(self.pos..self.pos + 32)?.try_into().ok()?;
        let d = ob_simd::level(window)?;
        let px = decimal::finish(u64::from(d.px), usize::from(d.px_frac), px_decimals)?;
        let qty = decimal::finish(u64::from(d.qty), usize::from(d.qty_frac), qty_decimals)?;
        self.pos += usize::from(d.len);
        Some((px, qty))
    }

    /// Slow path for an unusual quoted decimal: find the end of the string (validating
    /// escapes), then let the reference parser give the exact result or error.
    #[cold]
    #[inline(never)]
    fn decimal_slow(&mut self, start: usize, decimals: u8) -> Result<i64> {
        self.pos = start - 1; // back to the opening quote
        let text = self.string()?;
        let end = start + text.len();
        self.decimal_exact(start, end, decimals)
    }

    /// Reference-exact decimal parse of `buf[start..end]` (cursor already past the quote).
    #[cold]
    #[inline(never)]
    fn decimal_exact(&mut self, start: usize, end: usize, decimals: u8) -> Result<i64> {
        ob_core::parse_decimal(&self.buf[start..end], decimals).map_err(|e| JsonError {
            kind: JsonErrorKind::Decimal(e),
            offset: start,
        })
    }

    /// Skips one complete JSON value of any type, validating it. Iterative (an explicit
    /// depth counter instead of recursion), so deep nesting can't overflow the stack.
    pub(crate) fn skip_value(&mut self) -> Result<()> {
        // Bit i of `stack` is 1 if nesting level i is an object, 0 if an array.
        let mut stack = [0_u64; MAX_DEPTH / 64];
        let mut depth = 0_usize;
        loop {
            // Expecting a value.
            match self.peek() {
                None => return self.err(JsonErrorKind::Truncated),
                Some(b'{' | b'[') => {
                    let object = self.buf[self.pos] == b'{';
                    self.pos += 1;
                    if depth == MAX_DEPTH {
                        return self.err(JsonErrorKind::TooDeep);
                    }
                    let (word, bit) = (depth / 64, depth % 64);
                    if object {
                        stack[word] |= 1 << bit;
                    } else {
                        stack[word] &= !(1 << bit);
                    }
                    depth += 1;
                    let close = if object { b'}' } else { b']' };
                    if self.has_first(close)? {
                        if object {
                            self.key_colon()?;
                        }
                        continue; // read the first element
                    }
                    // Empty container: fall through to "after a value".
                    depth -= 1;
                }
                Some(b'"') => {
                    self.string()?;
                }
                Some(b'-' | b'0'..=b'9') => self.number()?,
                Some(b't') => self.literal(b"true")?,
                Some(b'f') => self.literal(b"false")?,
                Some(b'n') => self.literal(b"null")?,
                Some(_) => return self.err(JsonErrorKind::Unexpected),
            }
            // After a value: close containers until one has a next element.
            loop {
                if depth == 0 {
                    return Ok(());
                }
                let object = stack[(depth - 1) / 64] >> ((depth - 1) % 64) & 1 == 1;
                let close = if object { b'}' } else { b']' };
                if self.next_or_close(close)? {
                    if object {
                        self.key_colon()?;
                    }
                    break; // read the next element
                }
                depth -= 1;
            }
        }
    }

    /// An object key and its colon.
    pub(crate) fn key_colon(&mut self) -> Result<&'a [u8]> {
        let key = self.string()?;
        self.expect(b':')?;
        Ok(key)
    }

    fn literal(&mut self, word: &[u8]) -> Result<()> {
        match self.buf.get(self.pos..self.pos + word.len()) {
            Some(w) if w == word => {
                self.pos += word.len();
                Ok(())
            }
            Some(_) => self.err(JsonErrorKind::Unexpected),
            None if word.starts_with(&self.buf[self.pos..]) => self.err(JsonErrorKind::Truncated),
            None => self.err(JsonErrorKind::Unexpected),
        }
    }

    /// `true` or `false`.
    pub(crate) fn boolean(&mut self) -> Result<bool> {
        match self.peek() {
            Some(b't') => self.literal(b"true").map(|()| true),
            Some(b'f') => self.literal(b"false").map(|()| false),
            Some(_) => self.err(JsonErrorKind::Unexpected),
            None => self.err(JsonErrorKind::Truncated),
        }
    }

    /// Validates a number: `-? (0 | [1-9][0-9]*) (. [0-9]+)? ([eE] [+-]? [0-9]+)?`
    fn number(&mut self) -> Result<()> {
        let digits = |c: &mut Self| {
            let start = c.pos;
            while c.buf.get(c.pos).is_some_and(u8::is_ascii_digit) {
                c.pos += 1;
            }
            c.pos - start
        };
        if self.buf.get(self.pos) == Some(&b'-') {
            self.pos += 1;
        }
        let first = self.pos;
        match digits(self) {
            0 => return self.number_error(),
            n if n > 1 && self.buf[first] == b'0' => {
                self.pos = first;
                return self.err(JsonErrorKind::BadNumber);
            }
            _ => {}
        }
        if self.buf.get(self.pos) == Some(&b'.') {
            self.pos += 1;
            if digits(self) == 0 {
                return self.number_error();
            }
        }
        if matches!(self.buf.get(self.pos), Some(b'e' | b'E')) {
            self.pos += 1;
            if matches!(self.buf.get(self.pos), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            if digits(self) == 0 {
                return self.number_error();
            }
        }
        Ok(())
    }

    fn number_error<T>(&self) -> Result<T> {
        if self.pos >= self.buf.len() {
            self.err(JsonErrorKind::Truncated)
        } else {
            self.err(JsonErrorKind::BadNumber)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skip(s: &str) -> Result<usize> {
        let mut c = Cursor::new(s.as_bytes());
        c.skip_value()?;
        c.end()?;
        Ok(c.pos())
    }

    #[test]
    fn skips_every_kind_of_value() {
        for ok in [
            "0",
            "-0",
            "12",
            "-1.5e+10",
            "1E5",
            "0.25",
            "\"\"",
            "\"a\\\"b\\\\c\\u00e9\"",
            "true",
            "false",
            "null",
            "[]",
            "{}",
            "[1,[2,[3]],{\"a\":{}}]",
            "{\"a\":[],\"b\":{\"c\":null}}",
            " [ 1 , 2 ] ",
        ] {
            assert!(skip(ok).is_ok(), "{ok}");
        }
    }

    #[test]
    fn rejects_invalid_json() {
        use JsonErrorKind::*;
        for (bad, kind) in [
            ("01", BadNumber),
            ("-", Truncated),
            ("1.", Truncated),
            ("1.e5", BadNumber),
            ("1e", Truncated),
            (".5", Unexpected),
            ("+1", Unexpected),
            ("\"\\x\"", BadString),
            ("\"\\u12g4\"", BadString),
            ("\"a\u{1}b\"", BadString),
            ("tru", Truncated),
            ("trux", Unexpected),
            ("[1,]", Unexpected),
            ("[1 2]", Unexpected),
            ("{\"a\" 1}", Unexpected),
            ("{1:2}", Unexpected),
            ("[", Truncated),
            ("{\"a\":1", Truncated),
            ("1 2", TrailingBytes),
        ] {
            assert_eq!(skip(bad).map_err(|e| e.kind), Err(kind), "{bad}");
        }
    }

    #[test]
    fn depth_limit_is_enforced_without_recursion() {
        let ok = "[".repeat(MAX_DEPTH) + &"]".repeat(MAX_DEPTH);
        assert!(skip(&ok).is_ok());
        let deep = "[".repeat(MAX_DEPTH + 1) + &"]".repeat(MAX_DEPTH + 1);
        assert_eq!(skip(&deep).map_err(|e| e.kind), Err(JsonErrorKind::TooDeep));
        // Far deeper than any stack could recurse: still just an error.
        let hostile = "[".repeat(1_000_000);
        assert_eq!(
            skip(&hostile).map_err(|e| e.kind),
            Err(JsonErrorKind::TooDeep)
        );
    }

    #[test]
    fn booleans() {
        assert_eq!(Cursor::new(b" true").boolean(), Ok(true));
        assert_eq!(Cursor::new(b"false").boolean(), Ok(false));
        for bad in ["True", "1", "\"true\"", "nul", ""] {
            assert!(Cursor::new(bad.as_bytes()).boolean().is_err(), "{bad}");
        }
    }

    #[test]
    fn unsigned_integers() {
        let mut c = Cursor::new(b"1795610780582");
        assert_eq!(c.u64(), Ok(1_795_610_780_582));
        assert_eq!(Cursor::new(b"18446744073709551615").u64(), Ok(u64::MAX));
        for bad in ["18446744073709551616", "-1", "1.0", "1e3", "01", "x"] {
            assert!(Cursor::new(bad.as_bytes()).u64().is_err(), "{bad}");
        }
        // Long runs:
        // long leading zero, 8 and 16 digits, overflow at 21 digits, a point inside.
        for bad in ["0123456789012", "123456789012345678901", "1234567.89012345"] {
            assert!(Cursor::new(bad.as_bytes()).u64().is_err(), "{bad}");
        }
        assert_eq!(Cursor::new(b"12345678").u64(), Ok(12_345_678));
        assert_eq!(
            Cursor::new(b"1234567890123456,").u64(),
            Ok(1_234_567_890_123_456)
        );
    }

    proptest::proptest! {
        /// Any run of digits followed by anything: the same answer as the standard
        /// library (plus JSON's no-leading-zero rule), and the same end position.
        #[test]
        fn u64_matches_std(digits in "[0-9]{1,22}", tail in r"[,}\] .eE0-9a]{0,3}") {
            let text = std::format!("{digits}{tail}");
            let mut c = Cursor::new(text.as_bytes());
            let got = c.u64();
            let run = text.bytes().take_while(u8::is_ascii_digit).count();
            let next = text.as_bytes().get(run).copied();
            let expected = if matches!(next, Some(b'.' | b'e' | b'E' | b'-' | b'+'))
                || (run > 1 && text.starts_with('0'))
            {
                None
            } else {
                text[..run].parse::<u64>().ok()
            };
            proptest::prop_assert_eq!(got.ok(), expected, "{}", text);
            if expected.is_some() {
                proptest::prop_assert_eq!(c.pos(), run);
            }
        }
    }

    #[test]
    fn quoted_decimals_match_the_reference() {
        for (text, d) in [
            ("\"58631.90\"", 1_u8),
            ("\"6.774\"", 3),
            ("\"0\"", 3),
            ("\"58631.95\"", 1),
            ("\"-1.5\"", 1),
            ("\"1e5\"", 1),
            ("\"\"", 1),
            ("\".5\"", 1),
            ("\"5.\"", 1),
            ("\"1\\u0030\"", 1),
            ("\"12345678901234567890\"", 0),
        ] {
            let inner = &text.as_bytes()[1..text.len() - 1];
            let want = ob_core::parse_decimal(inner, d);
            let got = Cursor::new(text.as_bytes()).decimal(d);
            match (got, want) {
                (Ok(g), Ok(w)) => assert_eq!(g, w, "{text}"),
                (
                    Err(JsonError {
                        kind: JsonErrorKind::Decimal(g),
                        ..
                    }),
                    Err(w),
                ) => {
                    assert_eq!(g, w, "{text}");
                }
                other => panic!("{text}: {other:?}"),
            }
        }
    }
}
