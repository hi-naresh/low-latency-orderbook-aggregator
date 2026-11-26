//! `ob-agg`: the cross-venue view of the order books.
//!
//! Each venue's feed publishes its book through `ob-sync`; an [`Aggregator`] (one per
//! reading thread) keeps a cached copy of each, decides which venues are healthy enough to
//! use, and answers the README's queries: best bid/ask across venues, volume available at
//! a price, the price of a volume, fee-aware optimal execution across venues, and
//! cross-venue arbitrage.
//!
//! Queries never allocate: results are fixed-capacity ([`Quotes`], [`Volume`]).
#![forbid(unsafe_code)]

mod aggregator;
mod arbitrage;
mod execution;
mod quotes;

pub use aggregator::{Aggregator, Config, Exclusion, VenueConfig, VenueState};
pub use arbitrage::{ArbScan, Arbitrage};
pub use execution::{Action, ChildOrder, ExecutionPlan, PPM};
pub use quotes::{MAX_VENUES, Quote, Quotes, VenueId, VenueVolume, Volume, sum_qty};
