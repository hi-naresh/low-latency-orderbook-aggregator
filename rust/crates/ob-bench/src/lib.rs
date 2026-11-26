//! `ob-bench`: the tail-latency harness (step 23).
//!
//! Measures p50 / p99 / p99.9 / p99.99 / max per message, from encrypted bytes to the
//! published book, through the production code (the feed, its driver, the connection),
//! plus the aggregator's queries on the reading side. See `docs/steps/23-tail-harness.md`
//! and `scripts/bench-tails.sh`.
#![forbid(unsafe_code)]

pub mod anatomy;
pub mod jitter;
pub mod probe;
pub mod readpath;
pub mod scenario;
pub mod server;
pub mod stats;
pub mod stream;
