//! The feed's per-stage probe, recording into pooled distributions.
//!
//! Plugs into the production [`ob_feed::Feed`] through its [`Probe`] trait, so the code
//! measured is exactly the code that runs live; only the clock reads are added (~6 ns
//! each with rdtsc).
//!
//! **Steady state vs subscription start.** A looped recording repeats its first
//! messages (the subscription acknowledgement, Bybit's 50-level snapshot) once per pass:
//! about 1.3% of Bybit messages, which would sit exactly at p99. Live, they arrive once
//! per connection. Those "seam" messages are recorded separately, so the stage rows
//! describe the live stream; their own cost is still reported (one row).

use ob_feed::probe::Probe;

use crate::stats::Dist;

/// Stage distributions for one feed.
pub struct TailProbe {
    clock: quanta::Clock,
    /// One `poll()` that returned a message: socket read + TLS + WebSocket decode.
    pub read: Dist,
    /// Parse + sequence checks + apply.
    pub handle: Dist,
    /// Seqlock publish.
    pub publish: Dist,
    /// Start of the read that delivered the message → published (just handle + publish
    /// when there is no read, e.g. replay).
    pub total: Dist,
    /// `total` for subscription-start messages (kept out of the rows above).
    pub seam: Dist,
    read_start: Option<u64>,
    /// Per recorded line: is it a subscription-start message? (Wire mode: classified by
    /// counting reads.)
    mask: Vec<bool>,
    next: usize,
    in_seam: bool,
}

impl TailProbe {
    /// Empty distributions, timed with `clock`.
    pub fn new(clock: quanta::Clock) -> TailProbe {
        TailProbe {
            clock,
            read: Dist::new(),
            handle: Dist::new(),
            publish: Dist::new(),
            total: Dist::new(),
            seam: Dist::new(),
            read_start: None,
            mask: Vec::new(),
            next: 0,
            in_seam: false,
        }
    }

    /// Classifies messages by counting reads: message *k* is a seam if
    /// `mask[k % mask.len()]` (wire mode, where the driver calls `read` once per message).
    pub fn with_mask(mut self, mask: Vec<bool>) -> TailProbe {
        self.mask = mask;
        self
    }

    /// The next message is (or isn't) a subscription-start message (in-process mode,
    /// where there is no read to count).
    pub fn begin(&mut self, seam: bool) {
        self.in_seam = seam;
    }

    #[inline]
    fn ns(&self, start: u64, end: u64) -> u64 {
        self.clock.delta_as_nanos(start, end)
    }
}

impl Probe for TailProbe {
    #[inline]
    fn stamp(&self) -> u64 {
        self.clock.raw()
    }

    #[inline]
    fn read(&mut self, start: u64, end: u64) {
        if !self.mask.is_empty() {
            self.in_seam = self.mask[self.next % self.mask.len()];
            self.next += 1;
        }
        if !self.in_seam {
            let ns = self.ns(start, end);
            self.read.record(ns);
        }
        self.read_start = Some(start);
    }

    #[inline]
    fn applied(&mut self, handle_start: u64, handle_end: u64, publish_end: u64, _: u64) {
        let start = self.read_start.take().unwrap_or(handle_start);
        let total = self.ns(start, publish_end);
        if self.in_seam {
            self.seam.record(total);
            return;
        }
        self.handle.record(self.ns(handle_start, handle_end));
        self.publish.record(self.ns(handle_end, publish_end));
        self.total.record(total);
    }
}
