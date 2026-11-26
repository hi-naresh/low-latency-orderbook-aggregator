//! Streaming properties of the WebSocket codec, plus a differential check of frame
//! headers against tungstenite (an independent, widely used implementation).
//!
//! 1. **Round trip:** random messages, randomly fragmented and interleaved with control
//!    frames, encoded as a server would and decoded by `MessageReader`, come back exactly.
//! 2. **Any chunking gives the same result:** TCP has no message boundaries, so the same
//!    byte stream fed in arbitrary pieces must produce the same messages, or the same
//!    error, as feeding it whole.
//! 3. **Real data:** the recorded Binance and Bybit messages, framed, decode zero-copy.
//! 4. **Differential:** every header we accept, tungstenite parses identically; every one
//!    we reject is explained by a rule we enforce and it doesn't (we're stricter).
#![allow(clippy::unwrap_used, clippy::cast_possible_truncation)]

use std::io::Cursor;

use ob_net::ws::{
    Decoded, Message, MessageReader, Opcode, ReaderConfig, WsError, decode_frame,
    encode_server_frame,
};
use proptest::prelude::*;

/// A message as a test describes it (owned).
#[derive(Clone, Debug, PartialEq, Eq)]
enum Msg {
    Text(Vec<u8>),
    Binary(Vec<u8>),
    Ping(Vec<u8>),
    Pong(Vec<u8>),
}

fn owned(m: Message<'_>) -> Msg {
    match m {
        Message::Text(t) => Msg::Text(t.to_vec()),
        Message::Binary(b) => Msg::Binary(b.to_vec()),
        Message::Ping(p) => Msg::Ping(p.to_vec()),
        Message::Pong(p) => Msg::Pong(p.to_vec()),
        Message::Close(_) => panic!("not generated"),
    }
}

/// Decodes a stream delivered in `chunks`, the way a socket loop would: append each
/// chunk to a buffer and decode as many messages as are complete.
fn decode_chunked(stream: &[u8], cuts: &[usize], cfg: ReaderConfig) -> Result<Vec<Msg>, WsError> {
    let mut reader = MessageReader::new(cfg);
    let (mut buf, mut out, mut fed) = (Vec::new(), Vec::new(), 0);
    let mut boundaries: Vec<usize> = cuts.iter().map(|c| c % (stream.len() + 1)).collect();
    boundaries.push(stream.len());
    boundaries.sort_unstable();
    for end in boundaries {
        buf.extend_from_slice(&stream[fed..end.max(fed)]);
        fed = end.max(fed);
        loop {
            let step: Option<Decoded<'_>> = reader.decode(&buf)?;
            let Some(d) = step else { break };
            if let Some(m) = d.message {
                out.push(owned(m));
            }
            buf.drain(..d.consumed);
        }
    }
    assert!(buf.is_empty(), "complete stream left bytes undecoded");
    Ok(out)
}

/// Encodes `msgs`, splitting data messages into `pieces` fragments and putting a ping
/// between fragments when `interleave` is set.
fn encode_stream(msgs: &[(Msg, usize, bool)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (m, pieces, interleave) in msgs {
        match m {
            Msg::Ping(p) => encode_server_frame(&mut out, true, Opcode::Ping, p),
            Msg::Pong(p) => encode_server_frame(&mut out, true, Opcode::Pong, p),
            Msg::Text(d) | Msg::Binary(d) => {
                let op = if matches!(m, Msg::Text(_)) {
                    Opcode::Text
                } else {
                    Opcode::Binary
                };
                let n = (*pieces).clamp(1, d.len().max(1));
                let size = d.len().div_ceil(n).max(1);
                let parts: Vec<&[u8]> = if d.is_empty() {
                    vec![&d[..]]
                } else {
                    d.chunks(size).collect()
                };
                for (i, part) in parts.iter().enumerate() {
                    let fin = i + 1 == parts.len();
                    encode_server_frame(
                        &mut out,
                        fin,
                        if i == 0 { op } else { Opcode::Continuation },
                        part,
                    );
                    if *interleave && !fin {
                        encode_server_frame(&mut out, true, Opcode::Ping, b"between");
                    }
                }
            }
        }
    }
    out
}

/// The messages a reader should report for that stream, in order.
fn expected(msgs: &[(Msg, usize, bool)]) -> Vec<Msg> {
    let mut out = Vec::new();
    for (m, pieces, interleave) in msgs {
        if let Msg::Text(d) | Msg::Binary(d) = m {
            let n = (*pieces).clamp(1, d.len().max(1));
            let size = d.len().div_ceil(n).max(1);
            let frames = if d.is_empty() {
                1
            } else {
                d.len().div_ceil(size)
            };
            if *interleave {
                out.extend(std::iter::repeat_n(
                    Msg::Ping(b"between".to_vec()),
                    frames - 1,
                ));
            }
        }
        out.push(m.clone());
    }
    out
}

fn msg() -> impl Strategy<Value = (Msg, usize, bool)> {
    let text = prop::collection::vec(any::<char>(), 0..300)
        .prop_map(|c| Msg::Text(c.into_iter().collect::<String>().into_bytes()));
    let binary = prop::collection::vec(any::<u8>(), 0..70_000).prop_map(Msg::Binary); // crosses the 16/64-bit length forms
    let small = prop::collection::vec(any::<u8>(), 0..=125);
    let control = prop_oneof![small.clone().prop_map(Msg::Ping), small.prop_map(Msg::Pong)];
    (
        prop_oneof![4 => text, 1 => binary, 2 => control],
        1_usize..5,
        any::<bool>(),
    )
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 500, ..ProptestConfig::default() })]

    #[test]
    fn messages_round_trip_in_any_chunking(
        msgs in prop::collection::vec(msg(), 0..8),
        cuts in prop::collection::vec(any::<usize>(), 0..20),
    ) {
        let stream = encode_stream(&msgs);
        let want = expected(&msgs);
        prop_assert_eq!(decode_chunked(&stream, &[], ReaderConfig::default()).unwrap(), want.clone());
        prop_assert_eq!(decode_chunked(&stream, &cuts, ReaderConfig::default()).unwrap(), want);
    }

    /// Garbage in any chunking: same messages up to the same error (or the same success).
    #[test]
    fn chunking_never_changes_the_outcome(
        stream in prop::collection::vec(any::<u8>(), 0..400),
        cuts in prop::collection::vec(any::<usize>(), 0..20),
    ) {
        let cfg = ReaderConfig { max_message: 1 << 16, validate_utf8: true };
        let whole = std::panic::catch_unwind(|| decode_whole_prefix(&stream, cfg)).unwrap();
        let pieces = std::panic::catch_unwind(|| decode_prefix_chunked(&stream, &cuts, cfg)).unwrap();
        prop_assert_eq!(whole, pieces);
    }
}

/// Like `decode_chunked`, but stops at the first error or at an incomplete tail instead of
/// asserting that the whole stream was consumed (arbitrary bytes rarely end on a frame).
fn decode_prefix_chunked(
    stream: &[u8],
    cuts: &[usize],
    cfg: ReaderConfig,
) -> (Vec<Msg>, Option<WsError>) {
    let mut reader = MessageReader::new(cfg);
    let (mut buf, mut out, mut fed) = (Vec::new(), Vec::new(), 0);
    let mut boundaries: Vec<usize> = cuts.iter().map(|c| c % (stream.len() + 1)).collect();
    boundaries.push(stream.len());
    boundaries.sort_unstable();
    for end in boundaries {
        buf.extend_from_slice(&stream[fed..end.max(fed)]);
        fed = end.max(fed);
        loop {
            match reader.decode(&buf) {
                Err(e) => return (out, Some(e)),
                Ok(None) => break,
                Ok(Some(d)) => {
                    if let Some(m) = d.message {
                        out.push(match m {
                            Message::Close(c) => Msg::Text(format!("{c:?}").into_bytes()),
                            m => owned(m),
                        });
                    }
                    buf.drain(..d.consumed);
                }
            }
        }
    }
    (out, None)
}

fn decode_whole_prefix(stream: &[u8], cfg: ReaderConfig) -> (Vec<Msg>, Option<WsError>) {
    decode_prefix_chunked(stream, &[], cfg)
}

#[test]
fn recorded_exchange_messages_decode_zero_copy() {
    let fixtures = [
        include_str!("../../ob-parse/tests/fixtures/binance_depth20.jsonl"),
        include_str!("../../ob-parse/tests/fixtures/bybit_orderbook50.jsonl"),
    ];
    for fixture in fixtures {
        let mut stream = Vec::new();
        for line in fixture.lines() {
            encode_server_frame(&mut stream, true, Opcode::Text, line.as_bytes());
        }
        let mut reader = MessageReader::new(ReaderConfig::default());
        let (mut rest, mut lines) = (&stream[..], fixture.lines());
        while let Some(d) = reader.decode(rest).unwrap() {
            let Some(Message::Text(text)) = d.message else {
                panic!("expected text")
            };
            assert_eq!(text, lines.next().unwrap().as_bytes());
            assert!(
                stream.as_ptr_range().contains(&text.as_ptr()),
                "copied, not borrowed"
            );
            rest = &rest[d.consumed..];
        }
        assert!(rest.is_empty() && lines.next().is_none());
    }
}

// ---------------------------------------------------------------------------
// Differential check against tungstenite's frame header parser
// ---------------------------------------------------------------------------

fn differential(buf: &[u8]) -> Result<(), TestCaseError> {
    use tungstenite::protocol::frame::FrameHeader;
    use tungstenite::protocol::frame::coding::{Control, OpCode};
    const MAX: usize = 1 << 20;
    let ours = decode_frame(buf, MAX);
    let mut cursor = Cursor::new(buf);
    let theirs = FrameHeader::parse(&mut cursor);
    match (ours, theirs) {
        // We accept: they must parse the same header.
        (Ok(Some((frame, consumed))), Ok(Some((h, len)))) => {
            prop_assert_eq!(h.is_final, frame.fin);
            prop_assert_eq!(u8::from(h.opcode), frame.opcode as u8);
            prop_assert!(h.mask.is_none());
            prop_assert_eq!(len as usize, frame.payload.len());
            prop_assert_eq!(cursor.position() as usize + frame.payload.len(), consumed);
        }
        (Ok(Some(_)), other) => prop_assert!(
            false,
            "we accepted, tungstenite: {:?}",
            other.map(|o| o.is_some())
        ),
        // We need more bytes: either they do too, or they have the header and the
        // payload is what's missing.
        (Ok(None), Ok(None)) => {}
        (Ok(None), Ok(Some((_, len)))) => {
            prop_assert!(cursor.position() as usize + len as usize > buf.len())
        }
        (Ok(None), Err(e)) => prop_assert!(false, "we wait for more, tungstenite errors: {e}"),
        // We reject: the header they parsed must show the reason.
        (Err(err), Ok(Some((h, len)))) => {
            let op = u8::from(h.opcode);
            let control = matches!(
                h.opcode,
                OpCode::Control(Control::Close | Control::Ping | Control::Pong)
            );
            let explained = match err {
                WsError::MaskedFrame => h.mask.is_some(),
                WsError::ReservedBits => h.rsv1 || h.rsv2 || h.rsv3,
                WsError::UnknownOpcode(o) => o == op && !matches!(op, 0 | 1 | 2 | 8 | 9 | 10),
                WsError::FragmentedControl => control && !h.is_final,
                WsError::ControlTooLong => control && len > 125,
                WsError::NonMinimalLength => {
                    let header = cursor.position() as usize - if h.mask.is_some() { 4 } else { 0 };
                    (header == 4 && len < 126) || (header == 10 && len <= 0xFFFF)
                }
                WsError::TooBig => len > MAX as u64,
                _ => false,
            };
            prop_assert!(
                explained,
                "unexplained rejection {:?} of {:?} (len {})",
                err,
                h,
                len
            );
        }
        // Both reject, or we reject from a header they don't have the bytes for yet:
        // fine either way (rejecting early from the first bytes is allowed).
        (Err(_), _) => {}
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 20_000, ..ProptestConfig::default() })]

    #[test]
    fn headers_agree_with_tungstenite(buf in prop::collection::vec(any::<u8>(), 0..24)) {
        differential(&buf)?;
    }

    /// Mostly-valid headers (FIN/opcode/lengths drawn from legal values) so the accept
    /// path is exercised, not just rejections of random bytes.
    #[test]
    fn plausible_headers_agree_with_tungstenite(
        b0 in prop::sample::select(vec![0x80_u8, 0x81, 0x82, 0x88, 0x89, 0x8A, 0x00, 0x01, 0x02, 0x09, 0xC1, 0x83]),
        len_form in 0_u8..3,
        len in any::<u16>(),
        masked in prop::bool::weighted(0.1),
        tail in prop::collection::vec(any::<u8>(), 0..300),
    ) {
        let mut buf = vec![b0];
        let mask = if masked { 0x80 } else { 0 };
        match len_form {
            0 => buf.push(mask | (len % 126) as u8),
            1 => { buf.push(mask | 126); buf.extend_from_slice(&len.to_be_bytes()); }
            _ => { buf.push(mask | 127); buf.extend_from_slice(&u64::from(len).to_be_bytes()); }
        }
        if masked { buf.extend_from_slice(&[1, 2, 3, 4]); }
        buf.extend_from_slice(&tail);
        differential(&buf)?;
    }
}
