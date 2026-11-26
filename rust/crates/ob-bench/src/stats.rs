//! Latency distributions and how they're printed.
//!
//! **Pooled, never averaged.** Every sample of a scenario goes into one HDR histogram;
//! percentiles are read from it. Averaging per-run percentiles (as some published
//! benchmarks do) gives a number that isn't a percentile of anything.

use std::fmt::Write as _;

use hdrhistogram::Histogram;

/// One latency distribution, in nanoseconds (1 ns to 60 s, 3 significant figures).
pub struct Dist {
    h: Histogram<u64>,
}

impl Default for Dist {
    fn default() -> Self {
        Dist::new()
    }
}

impl Dist {
    /// An empty distribution. Allocates once; recording never allocates.
    pub fn new() -> Dist {
        Dist {
            #[allow(clippy::unwrap_used)] // constant, valid bounds
            h: Histogram::new_with_bounds(1, 60_000_000_000, 3).unwrap(),
        }
    }

    /// Adds one sample (values below 1 ns are recorded as 1 ns).
    #[inline]
    pub fn record(&mut self, ns: u64) {
        let _ = self.h.record(ns.clamp(1, 60_000_000_000));
    }

    /// Number of samples.
    pub fn len(&self) -> u64 {
        self.h.len()
    }

    /// True if nothing was recorded.
    pub fn is_empty(&self) -> bool {
        self.h.is_empty()
    }

    /// The value at quantile `q` (0..=1), ns.
    pub fn at(&self, q: f64) -> u64 {
        self.h.value_at_quantile(q)
    }

    /// The largest sample, ns.
    pub fn max(&self) -> u64 {
        self.h.max()
    }

    /// Mean, ns.
    pub fn mean(&self) -> f64 {
        self.h.mean()
    }
}

/// The columns of every report row.
pub const HEADER: &str = "count       p50       p90       p99     p99.9    p99.99       max";

/// One row: name, then count and percentiles in human units. A percentile backed by
/// fewer than 10 samples beyond it is marked `~` (too few to trust).
pub fn row(name: &str, d: &Dist) -> String {
    let mut s = format!("  {name:<40} ");
    if d.is_empty() {
        s.push_str("(no samples)");
        return s;
    }
    let n = d.len();
    let _ = write!(s, "{n:>7}");
    for q in [0.5, 0.9, 0.99, 0.999, 0.9999] {
        // Samples at or beyond this percentile.
        #[allow(
            clippy::cast_precision_loss,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss
        )]
        let beyond = ((1.0 - q) * n as f64).round() as u64;
        let mark = if beyond < 10 { "~" } else { " " };
        let _ = write!(s, " {:>8}{mark}", human(d.at(q)));
    }
    let _ = write!(s, " {:>9}", human(d.max()));
    s
}

/// `845 ns`, `1.23 µs`, `4.56 ms`.
pub fn human(ns: u64) -> String {
    #[allow(clippy::cast_precision_loss)]
    let f = ns as f64;
    if ns < 1_000 {
        format!("{ns} ns")
    } else if ns < 1_000_000 {
        format!("{:.2} µs", f / 1e3)
    } else if ns < 1_000_000_000 {
        format!("{:.2} ms", f / 1e6)
    } else {
        format!("{:.2} s", f / 1e9)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn units_and_rows() {
        assert_eq!(human(845), "845 ns");
        assert_eq!(human(1_234), "1.23 µs");
        assert_eq!(human(4_560_000), "4.56 ms");
        let mut d = Dist::new();
        for i in 1..=100_000 {
            d.record(i);
        }
        assert_eq!(d.len(), 100_000);
        // 3 significant figures: within 0.1%.
        assert!(d.at(0.99).abs_diff(99_000) <= 99);
        let r = row("x", &d);
        assert!(r.contains("100000"), "{r}");
        // p99.99 of 100,000 has exactly 10 samples beyond it: trusted, no `~`.
        assert!(!r.contains('~'), "{r}");
        let mut small = Dist::new();
        small.record(5);
        assert!(row("y", &small).contains('~'));
    }
}
