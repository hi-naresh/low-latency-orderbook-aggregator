//! A WebSocket client codec (RFC 6455), "sans-I/O": it turns bytes into frames and
//! messages and back, and never touches a socket; [`crate::conn`] runs it over TLS.
//!
//! * [`frame`]: frame headers, strict validation, masking.
//! * [`message`]: fragment reassembly, control frames, size and UTF-8 checks.
//! * [`handshake`]: the HTTP upgrade request and the server's answer.

pub mod frame;
pub mod handshake;
pub mod message;

pub use frame::{
    DEFAULT_MAX_PAYLOAD, Frame, Opcode, WsError, decode_frame, encode_client_frame,
    encode_server_frame,
};
pub use handshake::HandshakeError;
pub use message::{CloseFrame, Decoded, Message, MessageReader, ReaderConfig};
