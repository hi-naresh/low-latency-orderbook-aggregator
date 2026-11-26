//! Price levels: a fixed-capacity list, and the parser for a JSON array of levels
//! shared by every venue.

use ob_core::{InstrumentSpec, Level, ParseFixedError, Px, Qty};

use crate::json::{Cursor, JsonError, JsonErrorKind};

/// The most levels one side of one message may carry.
///
/// Snapshots carry at most the stream's depth (Binance 20, Bybit 50), but deltas list
/// changes: a Bybit delta can delete levels leaving the top 50 *and* insert their
/// replacements (66 on one side were seen live), and Binance's diff stream lists every
/// level that changed anywhere in the book in 100 ms (up to 887 in one event live). The
/// cost is memory only (16 KiB per buffer, allocated once); parsing touches only the
/// levels present.
pub const MAX_LEVELS: usize = 1024;

/// Up to [`MAX_LEVELS`] levels in a fixed array: no heap.
///
/// Meant to be **reused**: a feed keeps one per side and the parser overwrites it for
/// every message. [`LevelBuf::clear`] only resets the length, so reuse costs nothing,
/// whereas building a fresh 1 KB array per message would mean writing it every time.
#[derive(Clone, Debug)]
pub struct LevelBuf {
    levels: [Level; MAX_LEVELS],
    len: usize,
}

impl Default for LevelBuf {
    fn default() -> Self {
        LevelBuf {
            levels: [Level::new(Px::ZERO, Qty::ZERO); MAX_LEVELS],
            len: 0,
        }
    }
}

impl LevelBuf {
    /// Removes all levels. O(1).
    #[inline]
    pub fn clear(&mut self) {
        self.len = 0;
    }

    /// Appends a level; `false` if the buffer is full.
    #[inline]
    pub fn push(&mut self, level: Level) -> bool {
        match self.levels.get_mut(self.len) {
            Some(slot) => {
                *slot = level;
                self.len += 1;
                true
            }
            None => false,
        }
    }

    /// The levels, in the order the venue sent them.
    #[inline]
    pub fn as_slice(&self) -> &[Level] {
        &self.levels[..self.len]
    }
}

impl core::ops::Deref for LevelBuf {
    type Target = [Level];
    fn deref(&self) -> &[Level] {
        self.as_slice()
    }
}

/// Why an array of levels was refused; each venue's parser turns it into its own error
/// for the field it was reading.
#[derive(Clone, Copy, Debug)]
pub(crate) enum LevelsError {
    /// Malformed JSON, or a number that isn't a valid decimal.
    Json(JsonError),
    /// A level that isn't a `[price, quantity]` pair of strings, at this offset.
    BadLevel(usize),
    /// A price off the tick or a quantity off the lot, in the level at this offset.
    Spec(ParseFixedError, usize),
    /// More levels than the receiver can take, at this offset.
    TooMany(usize),
}

/// Parses `[["price","qty"], ...]`, handing each level to `push` (which returns `false`
/// when it can take no more). Each level is parsed by the SIMD kernel when it has the
/// usual shape, else byte by byte with identical results.
pub(crate) fn parse_levels(
    c: &mut Cursor<'_>,
    spec: &InstrumentSpec,
    mut push: impl FnMut(Level) -> bool,
) -> Result<(), LevelsError> {
    c.expect(b'[').map_err(LevelsError::Json)?;
    if !c.has_first(b']').map_err(LevelsError::Json)? {
        return Ok(());
    }
    let (pd, qd) = (spec.px_decimals(), spec.qty_decimals());
    // Wrong shape is a bad level; a bad number keeps its precise decimal error.
    let shape = |e: JsonError| match e.kind {
        JsonErrorKind::Decimal(_) | JsonErrorKind::Truncated => LevelsError::Json(e),
        _ => LevelsError::BadLevel(e.offset),
    };
    loop {
        let at = c.pos();
        let (px, qty) = match c.level_fast(pd, qd) {
            Some(v) => v,
            None => {
                c.expect(b'[').map_err(shape)?;
                let px = c.decimal(pd).map_err(shape)?;
                c.expect(b',').map_err(shape)?;
                let qty = c.decimal(qd).map_err(shape)?;
                c.expect(b']').map_err(shape)?;
                (px, qty)
            }
        };
        let px = spec.px_from_raw(px).map_err(|e| LevelsError::Spec(e, at))?;
        let qty = spec
            .qty_from_raw(qty)
            .map_err(|e| LevelsError::Spec(e, at))?;
        if !push(Level::new(px, qty)) {
            return Err(LevelsError::TooMany(at));
        }
        if !c.next_or_close(b']').map_err(LevelsError::Json)? {
            return Ok(());
        }
    }
}
