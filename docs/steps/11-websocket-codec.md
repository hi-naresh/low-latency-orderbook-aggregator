# Step 11: The WebSocket codec

Design reference: Phase 4 of the plan ("own websocket codec ... tungstenite allocates a
`Vec` per message"). Both exchanges stream market data over WebSocket (RFC 6455). This
step builds the protocol layer: turning raw bytes into messages, and our requests into
bytes. TLS and sockets are step 12.

## What changed

| File | Content |
|---|---|
| `rust/crates/ob-net/` | **New crate**, `#![forbid(unsafe_code)]` |
| `ob-net/src/ws/frame.rs` | Frame decoding (strict, zero-copy), client/server encoding, masking; 6 tests |
| `ob-net/src/ws/message.rs` | `MessageReader`: fragment reassembly, control frames, close parsing, size and UTF-8 checks; 5 tests |
| `ob-net/src/ws/handshake.rs` | HTTP upgrade request, `101` response validation, `Sec-WebSocket-Accept`, base64; 7 tests |
| `ob-net/tests/stream.rs` | Round trip, chunk-split invariance, real messages, differential vs tungstenite |
| `ob-net/fuzz/` | `ws` fuzz target (chunking invariance + handshake parser) |
| `ob-net/benches/ws.rs` | Decode cost: ob-net (with and without the UTF-8 check) vs tungstenite |
| `scripts/seed_fuzz_corpus.sh`, CI | Seeds the `ws` corpus from real framed messages; 60 s fuzz run per push |

Only one new runtime dependency: `sha1_smol` (dependency-free), used once per
connection for the handshake check. `tungstenite` is a test-only oracle.

## 1. Design: "sans-I/O"

The codec never touches a socket. It's a pure function from bytes to messages, plus a
small amount of state for fragments:
```rust
let mut reader = MessageReader::new(ReaderConfig::default());
loop {                                     // step 12's socket loop
    match reader.decode(&buf)? {           // buf = bytes received and not yet consumed
        Some(d) => { if let Some(m) = d.message { handle(m) }; buf.drain(..d.consumed); }
        None => read_more(&mut buf),       // not a whole frame yet
    }
}
```
**Why sans-I/O:**
- It can be tested exhaustively with plain byte arrays: no network, no mocks, no timing.
- It can be fuzzed at millions of inputs per second.
- It works unchanged on top of any transport (TLS, `io_uring`, or a replay file).

The same design is used by well-regarded protocol libraries in other languages.

## 2. Frames (RFC 6455 §5)

### Decoding is zero-copy
`decode_frame(buf, max) -> Result<Option<(Frame<'_>, consumed)>, WsError>`. The frame's
payload is a **slice of the receive buffer**. Servers never mask their frames, so there's
nothing to transform: the JSON is used where it sits. A test checks the returned slice
points inside the original buffer.

### Decoding is strict
Everything RFC 6455 says a client must reject is rejected, plus things it only
discourages, because a market-data server never needs them:

| Rejected | Why |
|---|---|
| Masked frame from the server | §5.1: the client **MUST** close |
| RSV bit set | No extension was negotiated |
| Unknown opcode (3–7, 0xB–0xF) | §5.2 |
| Control frame not final, or >125 bytes | §5.5 |
| **Length not in its shortest form** | §5.2: "the minimal number of bytes MUST be used" |
| 64-bit length with the top bit set | §5.2 |
| Larger than the configured maximum (default 1 MiB) | Memory bound; checked **from the header alone**, so a hostile length can't make us wait for gigabytes |

Every error maps to the close code to send back (`1002` protocol error, `1007` invalid
UTF-8, `1009` too big).

**Incomplete input is not an error.** `Ok(None)` means "read more". TCP delivers bytes
in arbitrary pieces, so a frame can arrive one byte at a time. Every strict prefix of
every frame length form is tested to return `Ok(None)`.

### Encoding: what we send
Clients **must** mask every frame with a fresh, unpredictable 4-byte key (§5.3). That
exists to stop cache-poisoning attacks on proxies. `encode_client_frame` takes the key
from the caller, so the codec stays pure and step 12 supplies randomness. A
server-style unmasked encoder exists for the tests and for step 14's mock exchange.

## 3. Messages (RFC 6455 §5.4–5.6)

`MessageReader` turns frames into messages:
- **Single-frame messages:** returned in place, **zero-copy**. That's every message in
  the Binance and Bybit recordings; a test checks it on all 120 recorded frames.
- **Fragmented messages** (first frame + `Continuation` frames): copied into a
  reassembly buffer allocated **once** at start-up and reused. The size limit applies to
  the total across fragments.
- **Control frames between fragments** (allowed by §5.4): returned immediately while the
  fragmented message stays in progress. Tested with a ping in the middle.
- **Errors:** a `Continuation` frame with nothing in progress, or a new text frame before
  the previous fragmented one finished.
- **Close frames:** empty, or a code plus a UTF-8 reason. The code must be one a peer may
  send: 1000–1003, 1007–1014, or 3000–4999. 1004–1006 and 1015 are reserved and must
  never appear on the wire.

### The UTF-8 question, measured
RFC 6455 §8.1 says a client **must** close the connection if a text message isn't valid
UTF-8. Our JSON parsers (steps 6–7) don't need valid UTF-8: they only accept ASCII
digits in numbers and compare keys byte for byte. So the check buys protocol compliance,
not safety. Its cost:

| Per message | Without UTF-8 check | With UTF-8 check |
|---|---|---|
| Binance (~1 KB) | **8–10 ns** | 51–53 ns |
| Bybit (mostly ~250 B) | **8 ns** | 27–29 ns |

**Decision:** the check is **on by default** (`ReaderConfig::validate_utf8 = true`):
- correct by default;
- about 50 ns against the ~1.6 µs it takes to parse a Binance message, roughly 3%.

A deployment that has measured and accepted the trade-off can switch it off per
connection. A multi-byte character split across fragments is handled: validation runs
on the complete message (tested with `€`).

## 4. The handshake (RFC 6455 §4)
- **The request:** `GET /ws HTTP/1.1` with `Upgrade: websocket`, `Connection: Upgrade`,
  `Sec-WebSocket-Key` (16 random bytes, base64-encoded) and `Sec-WebSocket-Version: 13`.
- **The response must have:**
  - status **101**. Anything else is reported with its code, e.g. 403/451 from a geo-block
    or 429 from rate limiting;
  - `Upgrade: websocket`, and `Connection` containing `upgrade`. Header names and these
    values are case-insensitive, and `Connection` may list several tokens; both are
    tested;
  - **`Sec-WebSocket-Accept` = base64(SHA-1(key + GUID)).** It proves the server
    understood a WebSocket request and isn't a cache replaying some other response.
    Verified against the RFC's own example (`dGhlIHNhbXBsZSBub25jZQ==` →
    `s3pPLMBiTxaQ9kYGzzhZRbK+xOo=`);
  - **no** `Sec-WebSocket-Extensions` or `-Protocol`. We asked for neither, so getting
    one back is a broken server. That also rules out `permessage-deflate` compression,
    which would add decompression CPU to every message.
- **Data can follow the headers in the same read.** `check_response` returns how many
  bytes the HTTP response used; everything after that is already WebSocket frames, and
  the caller keeps it. Tested.
- **Base64 is a 20-line function** checked against RFC 4648's vectors, rather than
  another dependency.

## 5. Verification

### a) RFC test vectors (unit tests, 17)
- RFC 6455 §5.7's example frames: unmasked `Hello`, fragmented `Hel`+`lo`, ping, and
  **masked `Hello`, which our encoder reproduces byte for byte**;
- every length form at the boundaries (0, 125, 126, 65,535, 65,536, 70,000);
- every strictness rule;
- the §1.3 handshake example;
- the RFC 4648 base64 vectors.

### b) Streaming properties (`tests/stream.rs`)
| Property | Cases |
|---|---|
| **Round trip:** random text (any Unicode), binary up to 70 KB (crossing the 16/64-bit length forms), pings and pongs, randomly fragmented with pings interleaved, decode exactly | 500 streams |
| **Chunking can't change the outcome:** the same stream fed whole or split at up to 20 random points gives identical messages | 500 streams |
| Same for **random garbage**: identical messages up to the identical error | 500 |
| All 120 **real recorded frames**, framed, decode zero-copy | all |

### c) Differential testing against tungstenite
`tungstenite`'s `FrameHeader::parse` reads a header without judging it (it allows
masking and RSV bits, because servers need that). The rule:
- every header **we accept**, it must parse identically (FIN, opcode, length, header
  size);
- every header **we reject** must be explained by the header it parsed (masked, RSV set,
  reserved opcode, control rules, non-minimal length, or too big).

Over 40,000 cases (random bytes plus mostly-plausible headers).

**It found a real misclassification in my decoder.** A close frame with a 0-byte payload,
written in the 16-bit length form, was rejected as `ControlTooLong`. My code checked the
length *form* (≥126 means "too long") instead of the decoded length. Rejecting it was
right; the *reason* was wrong. It's really `NonMinimalLength`. That matters because the
reason picks the close code and what gets logged. Fixed: the 125-byte limit is now
checked on the decoded length, and the case is a unit test.

### d) Coverage-guided fuzzing
| Target | Runs | Time | Result |
|---|---|---|---|
| `ws`: random size limit and UTF-8 setting, random chunking; whole vs chunked must match; handshake parser on the same bytes | **12.4 million** | 5 min (41k/s) | no crash, no mismatch |

Seeded with real framed Binance and Bybit messages, a fragmented message and a close.

### e) Miri
The 17 unit tests pass under Miri (no undefined behaviour). The crate also forbids
`unsafe` code.

## 6. Performance

Per message, the 59 real Binance and 59 real Bybit messages, criterion, two runs:

| | Binance (~1 KB) | Bybit (~250 B) | Allocates per message |
|---|---|---|---|
| **ob-net, UTF-8 not checked** | **8–10 ns** | **8 ns** | no |
| **ob-net, UTF-8 checked (default)** | 51–53 ns | 27–29 ns | no |
| tungstenite (always checks UTF-8) | ~127–129 ns | ~105–108 ns | yes (owned message buffer) |

**The tungstenite figures are corrected for my harness.** Its benchmark copies the
59 KB stream into an in-memory socket on every iteration. I measured that copy separately
(1.56 µs for Binance, 0.25 µs for Bybit) and subtracted it. Without the correction it
looked about 30% worse. Even corrected, it's **2.5–3.7× slower with UTF-8 checking**,
about **15× slower than ob-net without it**, and it allocates per message.

**In context:** the WebSocket layer costs 8–53 ns per message, against ~400 ns (Bybit
delta) to ~1.6 µs (Binance snapshot) for JSON parsing (steps 6–7). The protocol layer is
now a small fraction of the per-message work.

## 7. Java counterpart
The skeleton uses the `Java-WebSocket` library (`org.java_websocket.client.WebSocketClient`):
```java
@Override public void onMessage(String message) { ... }   // the library hands over a String
```
- **Every message becomes a `String`:** the library decodes UTF-8 bytes into a UTF-16
  `String`, which allocates and copies every message, and then Jackson parses that
  `String`. Zero-copy (parsing the bytes where they arrived) is impossible through this
  API.
- **Its own thread model:** the library runs a reader thread per client and calls
  `onMessage` on it. Steps 12–13 instead run one pinned thread per venue that reads,
  decodes and applies with no hand-offs.
- **Ping/pong:** the library answers server pings automatically, but Bybit expects
  *application-level* JSON pings (`{"op":"ping"}`) every 20 s, which the skeleton never
  sends. Step 13 handles that.

A low-latency Java version would use a byte-oriented client (Netty's WebSocket codec
with pooled `ByteBuf`s) and parse JSON straight from the buffer, with no `String` at all.

## How to verify it yourself
```bash
cd rust
cargo test -p ob-net                                 # unit + streaming + differential
cargo test -p ob-net --release --test stream         # just the properties (~1 s)
cargo bench -p ob-net --bench ws                      # decode cost vs tungstenite
cargo +nightly miri test -p ob-net --lib              # undefined-behaviour check (~15 s)
scripts/seed_fuzz_corpus.sh
cd crates/ob-net && cargo +nightly fuzz run ws -- -max_total_time=300
```
Things to try:
- **Reintroduce the bug the differential test found:** in `frame.rs`, move the
  `ControlTooLong` check back before the length is decoded (`if short > 125`). Then
  `plausible_headers_agree_with_tungstenite` fails with "unexplained rejection
  ControlTooLong ... (len 0)", and the unit test for that exact case fails too.
- **Check zero-copy:** in `a_single_frame_text_message_is_zero_copy`, the assertion
  `buf.as_ptr_range().contains(&text.as_ptr())` proves the message points into the
  receive buffer.

## Likely interview questions
1. **Why write your own WebSocket codec?**
   For zero-copy and no allocation on the hot path. Measured 8 ns per message (51 ns with
   UTF-8 validation) against ~105–129 ns for tungstenite, which also allocates. It's a
   small protocol, and the risk of writing it is covered by RFC vectors, property tests,
   a differential oracle, and 12 M fuzzed inputs.
2. **What does "sans-I/O" mean, and why?**
   The protocol logic is a pure function of bytes and doesn't do I/O. That makes it
   deterministic to test and fuzz, and reusable over any transport.
3. **TCP splits messages arbitrarily. How do you know you handle it?**
   A property test and the fuzzer check that the same stream, split anywhere, gives
   identical messages, or the identical error.
4. **Why must the client mask its frames, and the server must not?**
   Masking with a random key prevents an attacker's script from making a browser send
   bytes that a buggy proxy interprets as a cacheable HTTP request (cache poisoning).
   Server frames are unmasked, which is why we can parse them in place.
5. **Should a market-data client validate UTF-8?**
   The RFC requires it; our parsers don't need it. It costs ~50 ns per 1 KB message
   (~3% of parse time), so it's on by default and can be switched off deliberately.
6. **What's in `Sec-WebSocket-Accept`, and why check it?**
   base64(SHA-1(our key + a fixed GUID)). It proves the server processed *this*
   WebSocket request, rather than an intermediary replaying a cached response.
7. **How did testing against another implementation help?**
   It found a header I rejected for the wrong reason (`ControlTooLong` instead of
   `NonMinimalLength`), because I checked the length's *encoding* instead of its value.
   Unit tests written from my own understanding wouldn't have caught that.
