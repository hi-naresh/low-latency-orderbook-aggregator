//! `ob-parse`: turning exchange bytes into book updates, fast and without allocating.
//!
//! * [`decimal`]: number parsers (step 5).
//! * [`binance`]: Binance futures partial depth messages (step 6).
//! * [`bybit`]: Bybit linear order book snapshots and deltas (step 7).
//!
//! Like `ob-core`, this crate is `no_std`: the hot path can't allocate or do I/O.
#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

pub mod binance;
pub mod bybit;
pub mod decimal;
pub mod json;
pub mod levels;

pub use decimal::{parse_px, parse_qty};
pub use levels::{LevelBuf, MAX_LEVELS};

/// `true` when price levels are parsed by the SIMD kernel (step 25): this build enables
/// SSSE3 and SSE4.1. `false`: the scalar parser runs (identical results, slower).
pub const SIMD_LEVELS: bool = ob_simd::ACCELERATED;
