//! `ob-parse`: turning exchange bytes into book updates, fast and without allocating.
//!
//! * [`binance`]: Binance USDⓈ-M futures depth, bookTicker and REST snapshot messages.
//! * [`bybit`]: Bybit linear order book snapshots and deltas.
//! * [`decimal`]: decimal text → fixed-point integers, identical to the reference parser.
//!
//! Like `ob-core`, this crate is `no_std`: the hot path can't allocate or do I/O.
#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

pub mod binance;
pub mod bybit;
pub mod decimal;
mod json;
mod levels;

pub use decimal::{parse_px, parse_qty};
pub use json::{JsonError, JsonErrorKind};
pub use levels::{LevelBuf, MAX_LEVELS};

/// `true` when price levels are parsed by the SIMD kernel (`ob-simd`): this build enables
/// SSSE3 and SSE4.1. `false`: the scalar parser runs (identical results, slower).
pub const SIMD_LEVELS: bool = ob_simd::ACCELERATED;
