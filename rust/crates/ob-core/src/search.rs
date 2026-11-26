//! Ways to find a price in one side of the book.
//!
//! A side stores prices sorted **worst → best** (best last); see [`crate::Ladder`].
//! Every function here answers the same question:
//!
//! * `Ok(i)`: the price is at index `i`;
//! * `Err(pos)`: it isn't there, and `pos` is where it would be inserted
//!   (equivalently, the number of stored prices worse than it).
//!
//! `BID` gives the direction of "better": higher is better for bids, lower for asks.
//!
//! They're public so the benchmarks (`benches/book.rs`) and property tests
//! (`tests/model.rs`) can compare them side by side. The order book defaults to
//! [`StdBinary`], which was fastest for whole updates (search + shift + best read) on
//! both benchmark workloads; see `docs/steps/04-property-tests-and-benchmarks.md`.

/// A search strategy for the order book, chosen at compile time.
///
/// `OrderBook<CAP, S>` calls `S::find`; because `S` is a type parameter, the compiler
/// generates a separate copy of the book for each strategy with the search inlined, so
/// choosing a strategy costs nothing at run time (no function pointer, no branch).
pub trait Search {
    /// See the module docs for the meaning of the result.
    fn find<const BID: bool>(prices: &[i64], px: i64) -> Result<usize, usize>;
}

/// [`Search`] using [`linear_from_best`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LinearFromBest;

/// [`Search`] using [`branchless_binary`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BranchlessBinary;

/// [`Search`] using [`hybrid`]: linear over the top 8 levels, then branchless binary.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Hybrid;

/// [`Search`] using [`std_binary`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StdBinary;

impl Search for LinearFromBest {
    #[inline(always)]
    fn find<const BID: bool>(prices: &[i64], px: i64) -> Result<usize, usize> {
        linear_from_best::<BID>(prices, px)
    }
}

impl Search for BranchlessBinary {
    #[inline(always)]
    fn find<const BID: bool>(prices: &[i64], px: i64) -> Result<usize, usize> {
        branchless_binary::<BID>(prices, px)
    }
}

impl Search for Hybrid {
    #[inline(always)]
    fn find<const BID: bool>(prices: &[i64], px: i64) -> Result<usize, usize> {
        hybrid::<BID>(prices, px)
    }
}

impl Search for StdBinary {
    #[inline(always)]
    fn find<const BID: bool>(prices: &[i64], px: i64) -> Result<usize, usize> {
        std_binary::<BID>(prices, px)
    }
}

/// `true` if price `a` is better than price `b` on this side.
#[inline(always)]
pub const fn better<const BID: bool>(a: i64, b: i64) -> bool {
    if BID { a > b } else { a < b }
}

/// Scans from the best end downwards and stops at the first price that is not better
/// than `px`.
///
/// Cost grows with the distance from the top of the book, so it's cheapest exactly
/// where most updates land. The loop exit is a data-dependent branch, which the CPU
/// predicts well when updates cluster at a consistent depth and poorly when they don't.
#[inline]
pub fn linear_from_best<const BID: bool>(prices: &[i64], px: i64) -> Result<usize, usize> {
    let mut i = prices.len();
    while i > 0 {
        let p = prices[i - 1];
        if p == px {
            return Ok(i - 1);
        }
        if better::<BID>(px, p) {
            // Everything above i was better than px and p is worse: px goes right above p.
            return Err(i);
        }
        i -= 1;
    }
    Err(0)
}

/// Binary search with no data-dependent branches in the loop.
///
/// Each step picks the next half with a conditional move (`cmov`) instead of a jump, so
/// there is nothing for the CPU to mispredict. It always takes about `log2(len)` steps
/// (6 for 64 levels) wherever the price is, which is slower than a short linear scan at
/// the top of the book but immune to unpredictable access patterns.
#[inline]
pub fn branchless_binary<const BID: bool>(prices: &[i64], px: i64) -> Result<usize, usize> {
    let n = prices.len();
    if n == 0 {
        return Err(0);
    }
    // The prices worse than `px` form a prefix of the slice. Narrow down `base` to the last
    // index of that prefix (or 0 if the prefix is empty).
    let mut base = 0;
    let mut size = n;
    while size > 1 {
        let half = size / 2;
        let mid = base + half;
        // `select_unpredictable` tells the compiler the condition is data-dependent and
        // random, so it must emit a conditional move (`cmov`), not a branch. A plain
        // `if` here was measured to compile to a branch (see step 4 notes).
        base = core::hint::select_unpredictable(better::<BID>(px, prices[mid]), mid, base);
        size -= half;
    }
    let pos = base + usize::from(better::<BID>(px, prices[base]));
    if pos < n && prices[pos] == px {
        Ok(pos)
    } else {
        Err(pos)
    }
}

/// How many top levels [`hybrid`] scans linearly: 8 prices = one 64-byte cache line.
pub const HYBRID_TOP: usize = 8;

/// Linear scan over the top [`HYBRID_TOP`] levels, then [`branchless_binary`] over the
/// rest.
///
/// Most updates land in the top few levels, where the scan answers in one or two
/// compares. A price deeper than that costs the scan of one cache line plus a short
/// binary search, so the worst case stays bounded instead of growing with depth.
#[inline]
pub fn hybrid<const BID: bool>(prices: &[i64], px: i64) -> Result<usize, usize> {
    let n = prices.len();
    let stop = n.saturating_sub(HYBRID_TOP);
    let mut i = n;
    while i > stop {
        let p = prices[i - 1];
        if p == px {
            return Ok(i - 1);
        }
        if better::<BID>(px, p) {
            return Err(i);
        }
        i -= 1;
    }
    // px is worse than every top level, so it is in, or belongs in, `prices[..stop]`.
    // Indices in that prefix are the same as in the full slice.
    branchless_binary::<BID>(&prices[..stop], px)
}

/// The standard library's binary search, for comparison.
#[inline]
pub fn std_binary<const BID: bool>(prices: &[i64], px: i64) -> Result<usize, usize> {
    prices.binary_search_by(|&p| {
        if p == px {
            core::cmp::Ordering::Equal
        } else if better::<BID>(px, p) {
            core::cmp::Ordering::Less // p is worse, so it sorts before px
        } else {
            core::cmp::Ordering::Greater
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every search must agree with the simple linear one on every position, both sides,
    /// for every length up to 70 (covering empty, odd and even sizes, and past 64).
    #[test]
    fn all_searches_agree_exhaustively() {
        // Under Miri (~100× slower) check fewer lengths; still covers empty, odd, even, > 8.
        let lengths = if cfg!(miri) { 0..18_i64 } else { 0..70_i64 };
        for n in lengths {
            // Bids worst → best = ascending; asks worst → best = descending.
            let bids: Vec<i64> = (0..n).map(|i| 100 + 2 * i).collect();
            let asks: Vec<i64> = bids.iter().rev().copied().collect();
            for px in 95..(110 + 2 * n) {
                let expected = linear_from_best::<true>(&bids, px);
                assert_eq!(
                    branchless_binary::<true>(&bids, px),
                    expected,
                    "bid n={n} px={px}"
                );
                assert_eq!(std_binary::<true>(&bids, px), expected, "bid n={n} px={px}");
                let expected = linear_from_best::<false>(&asks, px);
                assert_eq!(
                    branchless_binary::<false>(&asks, px),
                    expected,
                    "ask n={n} px={px}"
                );
                assert_eq!(
                    std_binary::<false>(&asks, px),
                    expected,
                    "ask n={n} px={px}"
                );
            }
        }
    }
}
