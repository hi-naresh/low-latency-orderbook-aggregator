//! WebSocket frames (RFC 6455 §5.2): decoding what the server sends, encoding what we send.
//!
//! ```text
//!  0                   1                   2                   3
//!  0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
//! +-+-+-+-+-------+-+-------------+-------------------------------+
//! |F|R|R|R| opcode|M| Payload len |    Extended payload length    |
//! |I|S|S|S|  (4)  |A|     (7)     |             (16/64)           |
//! |N|V|V|V|       |S|             |   (if payload len==126/127)   |
//! | |1|2|3|       |K|             |                               |
//! +-+-+-+-+-------+-+-------------+ - - - - - - - - - - - - - - - +
//! |     Extended payload length continued, if payload len == 127  |
//! + - - - - - - - - - - - - - - - +-------------------------------+
//! |                               | Masking-key, if MASK set to 1 |
//! +-------------------------------+-------------------------------+
//! | Masking-key (continued)       |          Payload Data         |
//! +---------------------------------------------------------------+
//! ```
//!
//! **Zero-copy:** a decoded frame's payload is a slice of the receive buffer.
//!
//! **Strict:** we reject everything RFC 6455 says a client must reject, plus a few things
//! it merely discourages, because a market-data server never needs them and accepting
//! them only widens what we must handle:
//! * a masked frame from the server (§5.1: the client MUST close);
//! * any RSV bit set (no extension was negotiated);
//! * an unknown opcode;
//! * a control frame (close/ping/pong) that is fragmented or longer than 125 bytes;
//! * a length in the 16- or 64-bit form when a shorter form would do (§5.2: "the minimal
//!   number of bytes MUST be used");
//! * a 64-bit length with the top bit set, or larger than our configured maximum.

/// Frame type (the 4-bit opcode).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Opcode {
    /// Continues a fragmented text or binary message.
    Continuation = 0x0,
    /// UTF-8 text (all exchange market data).
    Text = 0x1,
    /// Binary data.
    Binary = 0x2,
    /// Close the connection.
    Close = 0x8,
    /// Heartbeat request; must be answered with a pong carrying the same payload.
    Ping = 0x9,
    /// Heartbeat answer.
    Pong = 0xA,
}

impl Opcode {
    const fn from_bits(bits: u8) -> Option<Opcode> {
        Some(match bits {
            0x0 => Opcode::Continuation,
            0x1 => Opcode::Text,
            0x2 => Opcode::Binary,
            0x8 => Opcode::Close,
            0x9 => Opcode::Ping,
            0xA => Opcode::Pong,
            _ => return None,
        })
    }

    /// `true` for close, ping and pong.
    pub const fn is_control(self) -> bool {
        matches!(self, Opcode::Close | Opcode::Ping | Opcode::Pong)
    }
}

/// One decoded frame. The payload borrows from the receive buffer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Frame<'a> {
    /// Last fragment of its message.
    pub fin: bool,
    /// Frame type.
    pub opcode: Opcode,
    /// Payload bytes (servers never mask, so they're usable as is).
    pub payload: &'a [u8],
}

/// A protocol violation. Each maps to the close code to send (RFC 6455 §7.4.1).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WsError {
    /// The server masked a frame.
    MaskedFrame,
    /// An RSV bit was set without a negotiated extension.
    ReservedBits,
    /// Opcode 3–7 or 0xB–0xF.
    UnknownOpcode(u8),
    /// A control frame with FIN unset.
    FragmentedControl,
    /// A control frame with more than 125 bytes of payload.
    ControlTooLong,
    /// A length encoded with more bytes than needed.
    NonMinimalLength,
    /// A message or frame larger than the configured maximum.
    TooBig,
    /// A continuation frame with no message in progress.
    UnexpectedContinuation,
    /// A new text/binary frame while a fragmented message is unfinished.
    UnfinishedMessage,
    /// Text that isn't valid UTF-8 (only checked if enabled).
    InvalidUtf8,
    /// A close frame with a 1-byte payload, an invalid code, or a non-UTF-8 reason.
    BadClose,
}

impl WsError {
    /// The close code to send back before dropping the connection.
    pub const fn close_code(self) -> u16 {
        match self {
            WsError::InvalidUtf8 => 1007,
            WsError::TooBig => 1009,
            _ => 1002, // protocol error
        }
    }
}

/// The largest frame payload any of our venues sends is a few KB (Bybit's 50-level
/// snapshot is 2.3 KB); 1 MiB leaves room for deeper streams while bounding memory.
pub const DEFAULT_MAX_PAYLOAD: usize = 1 << 20;

/// Decodes one frame from the start of `buf`.
///
/// * `Ok(Some((frame, consumed)))`: a complete frame; drop `consumed` bytes from `buf`.
/// * `Ok(None)`: not enough bytes yet; read more and call again with the larger buffer.
/// * `Err(_)`: protocol violation; close the connection with `err.close_code()`.
///
/// Never reads beyond `buf`, never panics, whatever the bytes (fuzzed).
#[inline]
pub fn decode_frame(buf: &[u8], max_payload: usize) -> Result<Option<(Frame<'_>, usize)>, WsError> {
    let (Some(&b0), Some(&b1)) = (buf.first(), buf.get(1)) else {
        return Ok(None);
    };
    let fin = b0 & 0x80 != 0;
    if b0 & 0x70 != 0 {
        return Err(WsError::ReservedBits);
    }
    let opcode = Opcode::from_bits(b0 & 0x0F).ok_or(WsError::UnknownOpcode(b0 & 0x0F))?;
    if b1 & 0x80 != 0 {
        return Err(WsError::MaskedFrame);
    }
    let short = b1 & 0x7F;
    if opcode.is_control() && !fin {
        return Err(WsError::FragmentedControl);
    }
    let (len, header) = match short {
        126 => {
            let Some(b) = buf.get(2..4) else {
                return Ok(None);
            };
            let len = u64::from(u16::from_be_bytes([b[0], b[1]]));
            if len < 126 {
                return Err(WsError::NonMinimalLength);
            }
            (len, 4_usize)
        }
        127 => {
            let Some(b) = buf.get(2..10).and_then(|b| <[u8; 8]>::try_from(b).ok()) else {
                return Ok(None);
            };
            let len = u64::from_be_bytes(b);
            if len <= u64::from(u16::MAX) {
                return Err(WsError::NonMinimalLength);
            }
            if len >> 63 != 0 {
                return Err(WsError::TooBig); // §5.2: the most significant bit MUST be 0
            }
            (len, 10_usize)
        }
        n => (u64::from(n), 2_usize),
    };
    // A control frame's payload is at most 125 bytes. Checked on the *decoded* length, so a
    // 0-byte close written in the 16-bit form is reported as what it is (non-minimal,
    // above), not as "too long". The differential test against tungstenite caught that.
    if opcode.is_control() && len > 125 {
        return Err(WsError::ControlTooLong);
    }
    // Check the size before using it, so a hostile length can't cause a huge wait.
    let len = usize::try_from(len)
        .ok()
        .filter(|&l| l <= max_payload)
        .ok_or(WsError::TooBig)?;
    // `checked_add`: even with a caller-chosen max of usize::MAX this can't overflow.
    let end = header.checked_add(len).ok_or(WsError::TooBig)?;
    let Some(payload) = buf.get(header..end) else {
        return Ok(None);
    };
    Ok(Some((
        Frame {
            fin,
            opcode,
            payload,
        },
        end,
    )))
}

/// Length of the header for a payload of `len` bytes, with or without a mask.
const fn header_len(len: usize, masked: bool) -> usize {
    let base = if len < 126 {
        2
    } else if len <= u16::MAX as usize {
        4
    } else {
        10
    };
    if masked { base + 4 } else { base }
}

/// Appends a frame as a **client** sends it: masked with `mask` (RFC 6455 §5.3 requires
/// every client frame to be masked with a fresh, unpredictable key; the caller supplies
/// it from a random source).
pub fn encode_client_frame(
    out: &mut Vec<u8>,
    fin: bool,
    opcode: Opcode,
    payload: &[u8],
    mask: [u8; 4],
) {
    encode(out, fin, opcode, payload, Some(mask));
}

/// Appends a frame as a **server** sends it (unmasked). Used by tests and the mock
/// exchange.
pub fn encode_server_frame(out: &mut Vec<u8>, fin: bool, opcode: Opcode, payload: &[u8]) {
    encode(out, fin, opcode, payload, None);
}

// `len as u8` runs only when len < 126; `len as u64` widens.
#[allow(clippy::cast_possible_truncation)]
fn encode(out: &mut Vec<u8>, fin: bool, opcode: Opcode, payload: &[u8], mask: Option<[u8; 4]>) {
    let len = payload.len();
    out.reserve(header_len(len, mask.is_some()) + len);
    out.push(if fin { 0x80 } else { 0 } | opcode as u8);
    let mask_bit = if mask.is_some() { 0x80 } else { 0 };
    if len < 126 {
        out.push(mask_bit | len as u8); // < 126 fits in 7 bits
    } else if let Ok(l) = u16::try_from(len) {
        out.push(mask_bit | 126);
        out.extend_from_slice(&l.to_be_bytes());
    } else {
        out.push(mask_bit | 127);
        out.extend_from_slice(&(len as u64).to_be_bytes());
    }
    match mask {
        None => out.extend_from_slice(payload),
        Some(key) => {
            out.extend_from_slice(&key);
            let start = out.len();
            out.extend_from_slice(payload);
            apply_mask(&mut out[start..], key);
        }
    }
}

/// XORs `data` with the repeating 4-byte `key` (masking and unmasking are the same).
pub fn apply_mask(data: &mut [u8], key: [u8; 4]) {
    for (i, b) in data.iter_mut().enumerate() {
        *b ^= key[i & 3];
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    const MAX: usize = DEFAULT_MAX_PAYLOAD;

    #[test]
    fn rfc_6455_examples() {
        // §5.7: a single-frame unmasked text message "Hello".
        let (f, n) = decode_frame(&[0x81, 0x05, 0x48, 0x65, 0x6c, 0x6c, 0x6f], MAX)
            .unwrap()
            .unwrap();
        assert_eq!(
            (f.fin, f.opcode, f.payload, n),
            (true, Opcode::Text, &b"Hello"[..], 7)
        );
        // §5.7: fragmented "Hel" + "lo".
        let (f, _) = decode_frame(&[0x01, 0x03, 0x48, 0x65, 0x6c], MAX)
            .unwrap()
            .unwrap();
        assert_eq!(
            (f.fin, f.opcode, f.payload),
            (false, Opcode::Text, &b"Hel"[..])
        );
        let (f, _) = decode_frame(&[0x80, 0x02, 0x6c, 0x6f], MAX)
            .unwrap()
            .unwrap();
        assert_eq!(
            (f.fin, f.opcode, f.payload),
            (true, Opcode::Continuation, &b"lo"[..])
        );
        // §5.7: unmasked ping "Hello".
        let (f, _) = decode_frame(&[0x89, 0x05, 0x48, 0x65, 0x6c, 0x6c, 0x6f], MAX)
            .unwrap()
            .unwrap();
        assert_eq!(f.opcode, Opcode::Ping);
        // §5.7: the masked "Hello" a *client* sends; our encoder must produce exactly it.
        let mut out = Vec::new();
        encode_client_frame(
            &mut out,
            true,
            Opcode::Text,
            b"Hello",
            [0x37, 0xfa, 0x21, 0x3d],
        );
        assert_eq!(
            out,
            [
                0x81, 0x85, 0x37, 0xfa, 0x21, 0x3d, 0x7f, 0x9f, 0x4d, 0x51, 0x58
            ]
        );
        // ...and a client must refuse it if a server sends it masked.
        assert_eq!(decode_frame(&out, MAX), Err(WsError::MaskedFrame));
    }

    #[test]
    fn length_forms_and_incomplete_input() {
        for len in [0, 1, 125, 126, 127, 65_535, 65_536, 70_000] {
            let mut out = Vec::new();
            encode_server_frame(&mut out, true, Opcode::Binary, &vec![7; len]);
            assert_eq!(out.len(), header_len(len, false) + len);
            let (f, n) = decode_frame(&out, MAX).unwrap().unwrap();
            assert_eq!((f.payload.len(), n), (len, out.len()));
            // Every strict prefix is "not enough bytes yet", never an error.
            for cut in [0, 1, out.len() / 2, out.len() - 1] {
                if cut < out.len() {
                    assert_eq!(
                        decode_frame(&out[..cut], MAX),
                        Ok(None),
                        "len {len}, cut {cut}"
                    );
                }
            }
        }
    }

    #[test]
    fn strictness() {
        assert_eq!(decode_frame(&[0xC1, 0x00], MAX), Err(WsError::ReservedBits));
        assert_eq!(
            decode_frame(&[0x83, 0x00], MAX),
            Err(WsError::UnknownOpcode(3))
        );
        assert_eq!(
            decode_frame(&[0x09, 0x00], MAX),
            Err(WsError::FragmentedControl)
        );
        assert_eq!(
            decode_frame(&[0x89, 0x7E, 0x00, 0x7E], MAX),
            Err(WsError::ControlTooLong)
        );
        // A 0-byte close written with the 16-bit length form: the fault is the encoding,
        // not the size (found by the tungstenite differential test).
        assert_eq!(
            decode_frame(&[0x88, 0x7E, 0x00, 0x00], MAX),
            Err(WsError::NonMinimalLength)
        );
        // 5 encoded in the 16-bit form, 200 in the 64-bit form: not minimal.
        assert_eq!(
            decode_frame(&[0x82, 0x7E, 0x00, 0x05], MAX),
            Err(WsError::NonMinimalLength)
        );
        assert_eq!(
            decode_frame(&[0x82, 0x7F, 0, 0, 0, 0, 0, 0, 0, 200], MAX),
            Err(WsError::NonMinimalLength)
        );
        // Top bit of the 64-bit length set.
        assert_eq!(
            decode_frame(&[0x82, 0x7F, 0x80, 0, 0, 0, 0, 1, 0, 0], MAX),
            Err(WsError::TooBig)
        );
        // Larger than our limit: refused from the header alone, without waiting for 1 GiB.
        assert_eq!(
            decode_frame(&[0x82, 0x7F, 0, 0, 0, 0, 0x40, 0, 0, 0], MAX),
            Err(WsError::TooBig)
        );
        assert_eq!(decode_frame(&[0x82, 0x05], 4), Err(WsError::TooBig));
    }

    #[test]
    fn masking_is_its_own_inverse() {
        let mut data = b"subscribe to orderbook.50.BTCUSDT".to_vec();
        let original = data.clone();
        apply_mask(&mut data, [1, 2, 3, 4]);
        assert_ne!(data, original);
        apply_mask(&mut data, [1, 2, 3, 4]);
        assert_eq!(data, original);
    }

    #[test]
    fn close_codes_for_errors() {
        assert_eq!(WsError::InvalidUtf8.close_code(), 1007);
        assert_eq!(WsError::TooBig.close_code(), 1009);
        assert_eq!(WsError::MaskedFrame.close_code(), 1002);
    }
}
