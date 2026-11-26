//! `ob-core`: the hot-path heart of the aggregator.
//!
//! This crate holds the fixed-point price/quantity types ([`Px`], [`Qty`], [`Notional`]),
//! the per-instrument scale that gives them meaning ([`InstrumentSpec`]), and the
//! single-venue order book ([`OrderBook`]). It is `no_std`: the compiler refuses any use of
//! the heap (`Vec`, `String`, `Box`) or of I/O here, which turns "the hot path never
//! allocates" from a promise into something the build enforces.
//!
//! Tests still link the standard library, so test helpers may allocate freely.
#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

mod book;
mod divisor;
mod fixed;
pub mod search;
mod spec;
mod tick_book;

pub use book::{BookLevels, Impact, Ladder, Level, LevelChange, OrderBook, Side, SnapshotError};
pub use fixed::{Notional, Px, Qty};
pub use spec::{
    ErrorClass, FixedDisplay, InstrumentSpec, MAX_DECIMALS, ParseFixedError, SpecError,
    parse_decimal,
};
pub use tick_book::{TickBook, TickSide, WINDOW};
