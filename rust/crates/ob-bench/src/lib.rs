//! `ob-bench`: the tail-latency harness.
//!
//! Measures p50 / p99 / p99.9 / p99.99 / max per message, from encrypted bytes to the
//! published book, through the production code (the feed, its driver, the connection),
//! plus the aggregator's queries on the reading side. Run it with `scripts/bench-tails.sh`;
//! the method is described in `docs/LATENCY.md`.
#![forbid(unsafe_code)]
// A measurement harness: a set-up step that fails means a broken environment, and a
// panic is the clearest report of it.
#![allow(clippy::unwrap_used)]

pub mod anatomy;
pub mod jitter;
pub mod probe;
pub mod readpath;
pub mod scenario;
pub mod server;
pub mod stats;
pub mod stream;
