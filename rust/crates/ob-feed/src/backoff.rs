//! Reconnect delays: exponential backoff with jitter, and a process-wide monotonic clock.

use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// Nanoseconds on a monotonic clock shared by the whole process (feeds stamp receive
/// times with it; the aggregator compares against it for staleness).
pub fn mono_ns() -> u64 {
    static BASE: OnceLock<Instant> = OnceLock::new();
    u64::try_from(BASE.get_or_init(Instant::now).elapsed().as_nanos()).unwrap_or(u64::MAX)
}

/// Backoff settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BackoffConfig {
    /// Delay before the first retry (upper bound; jitter takes half to all of it).
    pub base: Duration,
    /// Largest delay.
    pub max: Duration,
}

impl Default for BackoffConfig {
    /// 250 ms, doubling, capped at 30 s.
    fn default() -> Self {
        BackoffConfig {
            base: Duration::from_millis(250),
            max: Duration::from_secs(30),
        }
    }
}

/// Exponential backoff with "equal jitter": attempt `n` waits a random time in
/// `[cap/2, cap]` where `cap = min(max, base × 2^n)`.
///
/// * **Exponential** so a venue that's down isn't hammered.
/// * **Jitter** so many clients (or our own feeds) that lost the same venue at the same
///   moment don't all reconnect in lock-step and overload it again.
/// * **Half fixed** so a retry never comes almost instantly by chance.
#[derive(Clone, Debug)]
pub struct Backoff {
    cfg: BackoffConfig,
    attempt: u32,
    rng: u64,
}

impl Backoff {
    /// `seed` feeds the jitter (tests pass a fixed one; the driver a random one).
    pub fn new(cfg: BackoffConfig, seed: u64) -> Self {
        // SplitMix64 scrambles the seed: neighbouring seeds (1, 2, 3, ...) get unrelated
        // xorshift states, and the state is never 0 (xorshift's one forbidden value).
        // (`seed | 1` would merge seeds 2k and 2k+1, and leave small seeds correlated;
        // the `different_seeds_spread_out` test caught that.)
        let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        Backoff {
            cfg,
            attempt: 0,
            rng: if z == 0 { 0x9E37_79B9_7F4A_7C15 } else { z },
        }
    }

    /// The delay before the next attempt; each call counts as one more failure.
    pub fn next_delay(&mut self) -> Duration {
        let cap = self
            .cfg
            .base
            .saturating_mul(1 << self.attempt.min(20))
            .min(self.cfg.max);
        self.attempt = self.attempt.saturating_add(1);
        // xorshift64: plenty for jitter (this isn't security).
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        let half = cap / 2;
        let extra = u64::try_from(half.as_nanos()).unwrap_or(u64::MAX);
        half + Duration::from_nanos(if extra == 0 {
            0
        } else {
            self.rng % (extra + 1)
        })
    }

    /// Back to the first delay (after the connection has proved healthy for a while).
    pub fn reset(&mut self) {
        self.attempt = 0;
    }

    /// Failures since the last reset.
    pub fn attempts(&self) -> u32 {
        self.attempt
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grows_exponentially_with_bounded_jitter_and_caps() {
        let mut b = Backoff::new(BackoffConfig::default(), 42);
        let ms: Vec<u128> = (0..12).map(|_| b.next_delay().as_millis()).collect();
        let caps = [
            250, 500, 1000, 2000, 4000, 8000, 16000, 30000, 30000, 30000, 30000, 30000,
        ];
        for (d, cap) in ms.iter().zip(caps) {
            assert!(
                *d >= cap / 2 && *d <= cap,
                "delay {d} ms outside [{}, {cap}]",
                cap / 2
            );
        }
        b.reset();
        assert!(b.next_delay() <= Duration::from_millis(250));
    }

    #[test]
    fn different_seeds_spread_out() {
        let delays: Vec<Duration> = (1..20)
            .map(|s| Backoff::new(BackoffConfig::default(), s).next_delay())
            .collect();
        let distinct: std::collections::BTreeSet<_> = delays.iter().collect();
        assert!(distinct.len() > 15, "jitter should desynchronise clients");
    }

    #[test]
    fn the_clock_is_monotonic() {
        let a = mono_ns();
        let b = mono_ns();
        assert!(b >= a);
    }
}
