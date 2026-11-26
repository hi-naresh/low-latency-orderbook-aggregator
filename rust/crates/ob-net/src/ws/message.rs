//! WebSocket messages (RFC 6455 §5.4–5.6): frames reassembled into messages.
//!
//! A message is usually one frame (FIN set). The protocol also allows splitting one text
//! or binary message into several frames (a first frame, then `Continuation` frames,
//! the last with FIN), and allows control frames (ping, pong, close) to arrive *between*
//! those fragments. [`MessageReader`] handles both:
//!
//! * **Single-frame messages are zero-copy:** the returned text is a slice of the receive
//!   buffer. This is the hot path: every Binance and Bybit message in the recordings is a
//!   single frame.
//! * **Fragmented messages are copied into a reused buffer** (allocated once at start-up,
//!   bounded by the maximum message size).

use super::frame::{DEFAULT_MAX_PAYLOAD, Opcode, WsError, decode_frame};

/// Reader settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReaderConfig {
    /// Largest message accepted (single frame or reassembled), bytes.
    pub max_message: usize,
    /// Check that text messages are valid UTF-8, as RFC 6455 §8.1 requires. Costs a pass
    /// over every message.
    pub validate_utf8: bool,
}

impl Default for ReaderConfig {
    fn default() -> Self {
        ReaderConfig {
            max_message: DEFAULT_MAX_PAYLOAD,
            validate_utf8: true,
        }
    }
}

/// A close frame's content (RFC 6455 §5.5.1).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CloseFrame<'a> {
    /// Why (1000 = normal, 1001 = going away, 1008 = policy, ...).
    pub code: u16,
    /// Optional human-readable reason.
    pub reason: &'a str,
}

/// A complete message.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Message<'a> {
    /// Text (UTF-8 if validation is on): exchange JSON.
    Text(&'a [u8]),
    /// Binary data.
    Binary(&'a [u8]),
    /// Heartbeat request: answer with a pong carrying the same payload.
    Ping(&'a [u8]),
    /// Heartbeat answer.
    Pong(&'a [u8]),
    /// The server is closing (no code if the close frame was empty).
    Close(Option<CloseFrame<'a>>),
}

/// The result of one [`MessageReader::decode`] call.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Decoded<'a> {
    /// A complete message, or `None` if a fragment was absorbed (call again).
    pub message: Option<Message<'a>>,
    /// Bytes to drop from the front of the receive buffer.
    pub consumed: usize,
}

/// Turns received bytes into messages. One per connection.
pub struct MessageReader {
    cfg: ReaderConfig,
    /// Payload of a fragmented message being reassembled.
    partial: Vec<u8>,
    /// Its kind (text/binary), if one is in progress.
    partial_kind: Option<Opcode>,
}

impl MessageReader {
    /// A reader whose reassembly buffer is allocated once, up front.
    pub fn new(cfg: ReaderConfig) -> Self {
        MessageReader {
            cfg,
            partial: Vec::with_capacity(cfg.max_message.min(1 << 16)),
            partial_kind: None,
        }
    }

    /// Decodes the next frame from `buf` (bytes received and not yet consumed).
    ///
    /// * `Ok(Some(d))`: drop `d.consumed` bytes; if `d.message` is `Some`, handle it.
    /// * `Ok(None)`: `buf` doesn't hold a complete frame yet; read more.
    /// * `Err(e)`: protocol violation; send a close with `e.close_code()` and drop the
    ///   connection.
    ///
    /// Bytes may arrive split anywhere (TCP has no message boundaries); feeding them in
    /// any chunking gives the same messages (tested and fuzzed).
    pub fn decode<'a>(&'a mut self, buf: &'a [u8]) -> Result<Option<Decoded<'a>>, WsError> {
        let Some((frame, consumed)) = decode_frame(buf, self.cfg.max_message)? else {
            return Ok(None);
        };
        let done = |message| {
            Ok(Some(Decoded {
                message: Some(message),
                consumed,
            }))
        };
        match frame.opcode {
            Opcode::Ping => done(Message::Ping(frame.payload)),
            Opcode::Pong => done(Message::Pong(frame.payload)),
            Opcode::Close => done(Message::Close(close(frame.payload)?)),
            Opcode::Text | Opcode::Binary => {
                if self.partial_kind.is_some() {
                    return Err(WsError::UnfinishedMessage);
                }
                if frame.fin {
                    // The hot path: a whole message in one frame, returned in place.
                    return done(self.finish(frame.opcode, frame.payload)?);
                }
                self.partial.clear();
                self.partial.extend_from_slice(frame.payload);
                self.partial_kind = Some(frame.opcode);
                Ok(Some(Decoded {
                    message: None,
                    consumed,
                }))
            }
            Opcode::Continuation => {
                let Some(kind) = self.partial_kind else {
                    return Err(WsError::UnexpectedContinuation);
                };
                if self.partial.len() + frame.payload.len() > self.cfg.max_message {
                    return Err(WsError::TooBig);
                }
                self.partial.extend_from_slice(frame.payload);
                if !frame.fin {
                    return Ok(Some(Decoded {
                        message: None,
                        consumed,
                    }));
                }
                self.partial_kind = None;
                let cfg = self.cfg;
                let message = finish(cfg, kind, &self.partial)?;
                Ok(Some(Decoded {
                    message: Some(message),
                    consumed,
                }))
            }
        }
    }

    fn finish<'a>(&self, kind: Opcode, payload: &'a [u8]) -> Result<Message<'a>, WsError> {
        finish(self.cfg, kind, payload)
    }
}

fn finish(cfg: ReaderConfig, kind: Opcode, payload: &[u8]) -> Result<Message<'_>, WsError> {
    if kind == Opcode::Binary {
        return Ok(Message::Binary(payload));
    }
    if cfg.validate_utf8 && core::str::from_utf8(payload).is_err() {
        return Err(WsError::InvalidUtf8);
    }
    Ok(Message::Text(payload))
}

/// Parses a close frame's payload: empty, or a 2-byte code plus a UTF-8 reason.
fn close(payload: &[u8]) -> Result<Option<CloseFrame<'_>>, WsError> {
    match payload {
        [] => Ok(None),
        [_] => Err(WsError::BadClose),
        [hi, lo, reason @ ..] => {
            let code = u16::from_be_bytes([*hi, *lo]);
            // Codes a peer may send (RFC 6455 §7.4, plus 1012–1014 registered later with
            // IANA). 1004–1006 and 1015 are reserved and must never appear on the wire.
            let valid = matches!(code, 1000..=1003 | 1007..=1014 | 3000..=4999);
            let reason = core::str::from_utf8(reason).map_err(|_| WsError::BadClose)?;
            if valid {
                Ok(Some(CloseFrame { code, reason }))
            } else {
                Err(WsError::BadClose)
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::super::frame::encode_server_frame;
    use super::*;

    fn frames(parts: &[(bool, Opcode, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        for &(fin, op, payload) in parts {
            encode_server_frame(&mut out, fin, op, payload);
        }
        out
    }

    /// Decodes everything in `buf`, returning the messages as owned values.
    fn all(reader: &mut MessageReader, mut buf: &[u8]) -> Result<Vec<String>, WsError> {
        let mut out = Vec::new();
        while let Some(d) = reader.decode(buf)? {
            if let Some(m) = d.message {
                out.push(format!("{m:?}"));
            }
            buf = &buf[d.consumed..];
        }
        assert!(buf.is_empty(), "leftover bytes");
        Ok(out)
    }

    #[test]
    fn a_single_frame_text_message_is_zero_copy() {
        let buf = frames(&[(true, Opcode::Text, br#"{"e":"depthUpdate"}"#)]);
        let mut r = MessageReader::new(ReaderConfig::default());
        let d = r.decode(&buf).unwrap().unwrap();
        let Some(Message::Text(text)) = d.message else {
            panic!()
        };
        assert_eq!(text, br#"{"e":"depthUpdate"}"#);
        // Zero-copy: the slice points into `buf` itself.
        assert!(buf.as_ptr_range().contains(&text.as_ptr()));
    }

    #[test]
    fn fragments_are_reassembled_with_control_frames_in_between() {
        let buf = frames(&[
            (false, Opcode::Text, b"Hel"),
            (true, Opcode::Ping, b"hb"), // allowed between fragments
            (false, Opcode::Continuation, b"lo, "),
            (true, Opcode::Continuation, b"world"),
            (true, Opcode::Binary, &[1, 2]),
        ]);
        let mut r = MessageReader::new(ReaderConfig::default());
        assert_eq!(
            all(&mut r, &buf).unwrap(),
            [
                format!("{:?}", Message::Ping(b"hb")),
                format!("{:?}", Message::Text(b"Hello, world")),
                format!("{:?}", Message::Binary(&[1, 2])),
            ]
        );
    }

    #[test]
    fn fragmentation_errors() {
        let mut r = MessageReader::new(ReaderConfig::default());
        assert_eq!(
            all(&mut r, &frames(&[(true, Opcode::Continuation, b"x")])),
            Err(WsError::UnexpectedContinuation)
        );
        let mut r = MessageReader::new(ReaderConfig::default());
        let buf = frames(&[(false, Opcode::Text, b"a"), (true, Opcode::Text, b"b")]);
        assert_eq!(all(&mut r, &buf), Err(WsError::UnfinishedMessage));
        // Reassembly respects the size limit across fragments.
        let mut r = MessageReader::new(ReaderConfig {
            max_message: 4,
            validate_utf8: true,
        });
        let buf = frames(&[
            (false, Opcode::Binary, b"abc"),
            (true, Opcode::Continuation, b"de"),
        ]);
        assert_eq!(all(&mut r, &buf), Err(WsError::TooBig));
    }

    #[test]
    fn utf8_validation_is_optional() {
        let buf = frames(&[(true, Opcode::Text, &[0x7B, 0xFF, 0x7D])]);
        let mut strict = MessageReader::new(ReaderConfig::default());
        assert_eq!(strict.decode(&buf), Err(WsError::InvalidUtf8));
        let mut fast = MessageReader::new(ReaderConfig {
            validate_utf8: false,
            ..ReaderConfig::default()
        });
        assert!(fast.decode(&buf).unwrap().unwrap().message.is_some());
        // A multi-byte character split across fragments is fine: validated when complete.
        let euro = "€".as_bytes();
        let buf = frames(&[
            (false, Opcode::Text, &euro[..1]),
            (true, Opcode::Continuation, &euro[1..]),
        ]);
        assert_eq!(
            all(&mut MessageReader::new(ReaderConfig::default()), &buf).unwrap(),
            [format!("{:?}", Message::Text(euro))]
        );
    }

    #[test]
    fn close_frames() {
        let mut r = MessageReader::new(ReaderConfig::default());
        let parse = |r: &mut MessageReader, payload: &[u8]| {
            let buf = frames(&[(true, Opcode::Close, payload)]);
            r.decode(&buf)
                .map(|d| format!("{:?}", d.unwrap().message.unwrap()))
        };
        assert_eq!(
            parse(&mut r, b""),
            Ok(format!("{:?}", Message::Close(None)))
        );
        let mut ok = vec![0x03, 0xE8]; // 1000
        ok.extend_from_slice(b"bye");
        assert_eq!(
            parse(&mut r, &ok),
            Ok(format!(
                "{:?}",
                Message::Close(Some(CloseFrame {
                    code: 1000,
                    reason: "bye"
                }))
            ))
        );
        assert_eq!(parse(&mut r, &[0x03]), Err(WsError::BadClose)); // 1 byte
        assert_eq!(parse(&mut r, &[0x03, 0xEE]), Err(WsError::BadClose)); // 1006: reserved
        assert_eq!(parse(&mut r, &[0x03, 0xE8, 0xFF]), Err(WsError::BadClose)); // bad reason
        assert!(parse(&mut r, &[0x0F, 0xA0]).is_ok()); // 4000: application-defined
    }
}
