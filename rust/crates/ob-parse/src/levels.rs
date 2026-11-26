//! A fixed-capacity list of price levels, filled by the parsers.

use ob_core::{Level, Px, Qty};

/// The most levels one side of one message may carry.
///
/// Snapshots carry at most the stream's depth (Binance 20, Bybit 50), but a Bybit
/// **delta** lists changes: in a fast market one delta can delete levels leaving the top
/// 50 *and* insert their replacements, so up to 2 × 50 = 100 entries per side. The live
/// test in step 12 saw deltas with 66 levels on one side; the recording never did.
/// 128 covered the theoretical 100 with room to spare.
///
/// Step 19 raised it to 1024: Binance's **diff** depth stream (full-depth mode) lists
/// every level that changed anywhere in the book in the last 100 ms. A 15 s live sample
/// had up to 487 on one side (887 in one event). The cost is memory only (16 KiB per
/// buffer, allocated once); parsing touches only the levels present.
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
