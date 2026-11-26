//! Where the time goes, per message: stage timestamps and latency histograms.
//!
//! ```text
//!  poll() returns a message        venue.on_text()           publish()
//! ├── read: socket + TLS + WS ──┤├── handle: parse + apply ──┤├── publish: seqlock ──┤
//! ├────────────────────────────────── total ─────────────────────────────────────────┤
//! ```
//!
//! The feed is generic over a [`Probe`]. The default, [`NoProbe`], returns constant
//! timestamps and records nothing: after inlining it compiles to no code at all, so the
//! production path pays nothing for the ability to be measured. [`StageProbe`] reads the
//! CPU's time-stamp counter (~6 ns per stamp; `Instant::now` costs ~39 ns on this Mac)
//! and records into HDR histograms, which never allocate after creation.

use std::fmt::Write as _;
use std::time::{SystemTime, UNIX_EPOCH};

use hdrhistogram::Histogram;

/// Timestamps and recording for the per-message stages.
pub trait Probe {
    /// The current time in the probe's own units (CPU ticks for [`StageProbe`]).
    fn stamp(&self) -> u64;
    /// A message came out of the socket: `start` before `poll()`, `end` when it returned.
    fn read(&mut self, start: u64, end: u64);
    /// A book update was applied and published. `exchange_ts_ms` is the venue's
    /// timestamp for it (for the exchange → us delay).
    fn applied(
        &mut self,
        handle_start: u64,
        handle_end: u64,
        publish_end: u64,
        exchange_ts_ms: u64,
    );
    /// A printable table of what was measured, if anything.
    fn summary(&self) -> Option<String> {
        None
    }
    /// The data of the read that delivered a message waited this long in the kernel
    /// (Linux kernel receive timestamps, step 21).
    fn kernel_wait(&mut self, _ns: u64) {}
}

/// Measures nothing, costs nothing.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoProbe;

impl Probe for NoProbe {
    #[inline(always)]
    fn stamp(&self) -> u64 {
        0
    }
    #[inline(always)]
    fn read(&mut self, _: u64, _: u64) {}
    #[inline(always)]
    fn applied(&mut self, _: u64, _: u64, _: u64, _: u64) {}
}

/// Where "now" comes from for the exchange → us delay.
#[derive(Clone, Copy, Debug)]
pub enum WallClock {
    /// The system clock (live).
    System,
    /// A fixed time in Unix ns, set per message (replay: the recorded receive time).
    Fixed(u64),
}

/// Latency histograms for each stage, in nanoseconds, plus the exchange → us delay.
pub struct StageProbe {
    clock: quanta::Clock,
    /// Wall clock for the exchange delay.
    pub wall: WallClock,
    read_start: Option<u64>,
    read: Histogram<u64>,
    handle: Histogram<u64>,
    publish: Histogram<u64>,
    total: Histogram<u64>,
    /// Exchange timestamp → our receive time, µs. Clocks are not synchronised: this
    /// includes the skew between the venue's clock and ours.
    exchange_delay_us: Histogram<u64>,
    /// Messages whose exchange timestamp was *ahead* of our clock (skew > delay).
    negative_delays: u64,
    /// Kernel receive timestamp → our read, ns (Linux only).
    kernel_wait: Histogram<u64>,
}

fn histogram() -> Histogram<u64> {
    // 1 ns .. 100 s at 3 significant digits, allocated once, here. These bounds are
    // constants that satisfy the constructor's rules (low ≥ 1, high ≥ 2·low, sigfig ≤ 5).
    Histogram::new_with_bounds(1, 100_000_000_000, 3).expect("valid constant bounds")
}

impl StageProbe {
    /// Calibrates the TSC clock (a few ms) and allocates the histograms.
    pub fn new(wall: WallClock) -> Self {
        StageProbe {
            clock: quanta::Clock::new(),
            wall,
            read_start: None,
            read: histogram(),
            handle: histogram(),
            publish: histogram(),
            total: histogram(),
            exchange_delay_us: histogram(),
            negative_delays: 0,
            kernel_wait: histogram(),
        }
    }

    fn ns(&self, start: u64, end: u64) -> u64 {
        self.clock.delta_as_nanos(start, end).max(1)
    }

    /// A table of percentiles for every stage.
    pub fn report(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(
            out,
            "{:<26} {:>9} {:>9} {:>9} {:>9} {:>9} {:>10}",
            "stage (ns)", "count", "p50", "p90", "p99", "p99.9", "max"
        );
        let rows: [(&str, &Histogram<u64>); 5] = [
            ("kernel wait (Linux)", &self.kernel_wait),
            ("read (socket+TLS+WS)", &self.read),
            ("handle (parse+apply)", &self.handle),
            ("publish (seqlock)", &self.publish),
            ("total", &self.total),
        ];
        for (name, h) in rows {
            if h.is_empty() {
                continue;
            }
            let _ = writeln!(
                out,
                "{name:<26} {:>9} {:>9} {:>9} {:>9} {:>9} {:>10}",
                h.len(),
                h.value_at_quantile(0.5),
                h.value_at_quantile(0.9),
                h.value_at_quantile(0.99),
                h.value_at_quantile(0.999),
                h.max()
            );
        }
        let d = &self.exchange_delay_us;
        if !d.is_empty() {
            let _ = writeln!(
                out,
                "{:<26} {:>9} {:>9} {:>9} {:>9} {:>9} {:>10}   (µs; includes clock skew; {} negative)",
                "exchange → us",
                d.len(),
                d.value_at_quantile(0.5),
                d.value_at_quantile(0.9),
                d.value_at_quantile(0.99),
                d.value_at_quantile(0.999),
                d.max(),
                self.negative_delays
            );
        }
        out
    }

    /// The handle-stage histogram (parse + apply), ns.
    pub fn handle(&self) -> &Histogram<u64> {
        &self.handle
    }
    /// The total histogram, ns.
    pub fn total(&self) -> &Histogram<u64> {
        &self.total
    }
}

impl Probe for StageProbe {
    fn summary(&self) -> Option<String> {
        Some(self.report())
    }

    fn kernel_wait(&mut self, ns: u64) {
        let _ = self.kernel_wait.record(ns.max(1));
    }

    #[inline]
    fn stamp(&self) -> u64 {
        self.clock.raw()
    }

    #[inline]
    fn read(&mut self, start: u64, end: u64) {
        let ns = self.ns(start, end);
        let _ = self.read.record(ns);
        self.read_start = Some(start);
    }

    #[inline]
    fn applied(
        &mut self,
        handle_start: u64,
        handle_end: u64,
        publish_end: u64,
        exchange_ts_ms: u64,
    ) {
        let _ = self.handle.record(self.ns(handle_start, handle_end));
        let _ = self.publish.record(self.ns(handle_end, publish_end));
        // Total from the start of the read that delivered this message (replay has no
        // read: total = handle + publish).
        let start = self.read_start.take().unwrap_or(handle_start);
        let _ = self.total.record(self.ns(start, publish_end));

        let now_ns = match self.wall {
            WallClock::System => SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)),
            WallClock::Fixed(ns) => ns,
        };
        let exchange_ns = exchange_ts_ms.saturating_mul(1_000_000);
        if exchange_ts_ms > 0 && now_ns > 0 {
            if now_ns >= exchange_ns {
                let _ = self
                    .exchange_delay_us
                    .record(((now_ns - exchange_ns) / 1000).max(1));
            } else {
                self.negative_delays += 1;
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn records_each_stage_and_the_total_from_the_read() {
        let mut p = StageProbe::new(WallClock::Fixed(10_005_000_000));
        let t0 = p.stamp();
        let t1 = p.stamp();
        p.read(t0, t1);
        let (h0, h1, h2) = (p.stamp(), p.stamp(), p.stamp());
        p.applied(h0, h1, h2, 10_000); // exchange at 10.000 s, we at 10.005 s
        assert_eq!(
            (p.read.len(), p.handle.len(), p.publish.len(), p.total.len()),
            (1, 1, 1, 1)
        );
        assert!(p.total.max() >= p.handle.max());
        // 5 ms delay, within the histogram's 3-digit precision.
        let d = p.exchange_delay_us.max();
        assert!((4995..=5005).contains(&d), "{d}");
        assert!(p.report().contains("exchange → us"));
    }

    #[test]
    fn an_exchange_clock_ahead_of_ours_is_counted_not_recorded() {
        let mut p = StageProbe::new(WallClock::Fixed(1_000_000_000));
        let t = p.stamp();
        p.applied(t, t, t, 2_000); // exchange says 2 s, we say 1 s
        assert_eq!((p.exchange_delay_us.len(), p.negative_delays), (0, 1));
    }
}
