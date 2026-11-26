//! `ob-net`: getting exchange bytes off the network.
//!
//! * [`ws`]: a zero-copy, strict WebSocket codec with no I/O of its own (step 11).
//! * [`tls`]: TLS client configuration (rustls, AES-128-GCM preferred) (step 12).
//! * [`conn`]: a WebSocket-over-TLS connection: blocking connect with a deadline, then a
//!   non-blocking `poll` for a busy-polling loop, or blocking waits (step 12).
//! * [`http`]: a minimal, bounded HTTPS GET for REST snapshots (step 19).
//! * [`record`]: our TLS record layer (TLS 1.2 and 1.3 AEAD records), used after a
//!   rustls handshake: records decrypted in place, no allocation per record (step 24).
//!   `session` (internal) drives it: handshake hand-over, key updates, alerts.
//!
//! No `unsafe` code.
#![forbid(unsafe_code)]

pub mod conn;
pub mod http;
pub mod record;
mod session;
pub mod tls;
pub mod ws;

pub use conn::{ConnConfig, NetError, SocketTuning, WaitMode, WsConnection};
