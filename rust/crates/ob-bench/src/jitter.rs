//! How often, and for how long, the OS takes the CPU away from a spinning thread
//!.
//!
//! A thread spins reading the clock as fast as it can. Every gap between two consecutive
//! reads that is longer than the read itself means something else ran on that CPU: an
//! interrupt, the scheduler tick, another thread. This is what sets p99.9 on a machine
//! that isn't isolated (finding 4), and what core isolation on Linux should
//! remove (`docs/LINUX.md` §3.1). Run it on a feed core: on a well-isolated core the
//! largest gap is a few µs.

use std::time::{Duration, Instant};

use crate::stats::Dist;

/// What a spinning thread saw.
pub struct Jitter {
    /// Every gap between consecutive clock reads, ns.
    pub gaps: Dist,
    /// How long it ran, ns.
    pub elapsed_ns: u64,
    /// Gaps longer than 1 µs / 10 µs / 100 µs.
    pub over_1us: u64,
    /// See `over_1us`.
    pub over_10us: u64,
    /// See `over_1us`.
    pub over_100us: u64,
    /// Total time inside gaps longer than 1 µs: time the thread didn't have the CPU.
    pub lost_ns: u64,
}

/// Spins on the calling thread for `duration`, recording every gap.
pub fn jitter(duration: Duration, clock: &quanta::Clock) -> Jitter {
    let mut j = Jitter {
        gaps: Dist::new(),
        elapsed_ns: 0,
        over_1us: 0,
        over_10us: 0,
        over_100us: 0,
        lost_ns: 0,
    };
    let start = Instant::now();
    let first = clock.raw();
    let mut prev = first;
    loop {
        // Check the wall clock only every 4096 reads: the loop must stay tight.
        for _ in 0..4096 {
            let now = clock.raw();
            let gap = clock.delta_as_nanos(prev, now);
            j.gaps.record(gap);
            if gap > 1_000 {
                j.over_1us += 1;
                j.lost_ns += gap;
                j.over_10us += u64::from(gap > 10_000);
                j.over_100us += u64::from(gap > 100_000);
            }
            prev = now;
        }
        if start.elapsed() >= duration {
            break;
        }
    }
    j.elapsed_ns = clock.delta_as_nanos(first, prev);
    j
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_every_gap_and_accounts_for_lost_time() {
        let clock = quanta::Clock::new();
        let j = jitter(Duration::from_millis(50), &clock);
        assert!(j.gaps.len() > 10_000);
        assert!(j.elapsed_ns >= 50_000_000 / 2);
        assert!(j.over_10us <= j.over_1us && j.over_100us <= j.over_10us);
        assert!(j.lost_ns <= j.elapsed_ns);
    }
}
