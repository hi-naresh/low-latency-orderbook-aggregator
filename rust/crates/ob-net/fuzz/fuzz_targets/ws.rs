//! Fuzz target for the WebSocket codec.
//!
//! Input: byte 0 picks the max message size, byte 1 whether UTF-8 is validated, bytes
//! 2..10 pick where the stream is split into chunks; the rest is the byte stream.
//! Checks: no panic; feeding the stream whole or in chunks gives identical messages and
//! the identical error; `consumed` never exceeds the buffer; the handshake parser never
//! panics on the same bytes.
#![no_main]

use libfuzzer_sys::fuzz_target;
use ob_net::ws::{MessageReader, ReaderConfig, handshake};

/// Decodes `stream` delivered in pieces ending at `cuts`; returns the messages (as debug
/// strings) and the error, if any.
fn run(stream: &[u8], cuts: &[usize], cfg: ReaderConfig) -> (Vec<String>, Option<String>) {
    let mut reader = MessageReader::new(cfg);
    let (mut buf, mut out, mut fed) = (Vec::new(), Vec::new(), 0);
    let mut ends: Vec<usize> = cuts.iter().map(|c| c % (stream.len() + 1)).collect();
    ends.push(stream.len());
    ends.sort_unstable();
    for end in ends {
        buf.extend_from_slice(&stream[fed..end.max(fed)]);
        fed = end.max(fed);
        loop {
            match reader.decode(&buf) {
                Err(e) => return (out, Some(format!("{e:?}"))),
                Ok(None) => break,
                Ok(Some(d)) => {
                    assert!(d.consumed > 0 && d.consumed <= buf.len());
                    if let Some(m) = d.message {
                        out.push(format!("{m:?}"));
                    }
                    buf.drain(..d.consumed);
                }
            }
        }
    }
    (out, None)
}

fuzz_target!(|data: &[u8]| {
    if data.len() < 10 {
        return;
    }
    let cfg = ReaderConfig { max_message: 1 + usize::from(data[0]) * 64, validate_utf8: data[1] & 1 == 1 };
    let cuts: Vec<usize> = data[2..10].iter().map(|&b| usize::from(b) * 7).collect();
    let stream = &data[10..];
    assert_eq!(run(stream, &[], cfg), run(stream, &cuts, cfg), "chunking changed the outcome");
    let _ = handshake::check_response(stream, "dGhlIHNhbXBsZSBub25jZQ==");
});
