# Step 12: TLS and the socket loop

Design reference: Phase 4 of the plan ("Socket path": non-blocking `TcpStream`,
`TCP_NODELAY`, large `SO_RCVBUF`, busy-poll, rustls, AES-128-GCM). This step connects
the codec from step 11 to real exchanges: TCP, then TLS, then the WebSocket upgrade, then
a non-blocking `poll()` a feed thread can spin on.

It was also the first time the system talked to the live exchanges, and that found a
**real correctness bug** the recordings never showed (section 5).

## What changed

| File | Content |
|---|---|
| `ob-net/src/tls.rs` | Client config: `ring` backend, AES-128-GCM preferred, Mozilla roots compiled in; secure random |
| `ob-net/src/conn.rs` | `WsConnection`: `connect` (deadline-bounded), `poll` (non-blocking), `send_text`, `close`; `WaitMode::{Spin, Block}` |
| `ob-net/tests/conn.rs` | 9 end-to-end tests against a local TLS WebSocket server (throwaway certificate) |
| `ob-net/tests/latency.rs` | Spin vs blocking wake-up latency (ignored by default; prints a table) |
| `ob-net/tests/live.rs` | Live Binance + Bybit check (ignored by default; needs the internet) |
| `ob-parse/src/levels.rs` | `MAX_LEVELS` 64 → **128** (live Bybit deltas, section 5) |
| `ob-parse/tests/bybit.rs` | Regression test: Bybit books need capacity ≥ 2 × depth |
| `docs/steps/07-bybit-parser.md` | Correction appended |

New dependencies: `rustls` (no default features: `ring`, `std`, `tls12`),
`webpki-roots`, `socket2`; for tests only, `rcgen` (throwaway certificate).

## 1. TLS choices, and what reading rustls's source revealed

**The plan said to use rustls's "unbuffered" API to decrypt in place, with no copy.**
Reading rustls 0.23.46's source showed that isn't implemented yet:
```rust
// rustls-0.23.46/src/conn/unbuffered.rs
pub struct ReadTraffic<'c, 'i, Data> {
    _incoming_tls: &'i mut [u8],      // "for forwards compatibility; to support in-place decryption in the future"
    chunk: Option<Vec<u8>>,           // ← an owned Vec per record
}
// TODO deprecate in favor of `Iterator` implementation, which requires in-place decryption
pub fn next_record(&mut self) -> Option<Result<AppDataRecord<'_>, Error>> { ... received_plaintext.pop() ... }
```
And the regular buffered API stores plaintext the same way
(`received_plaintext.append(bytes.into_vec())`). So **both APIs cost one allocation and
one copy per received TLS record**, and the unbuffered one would only add complexity.

**Decision:** use the mature buffered `ClientConnection`, and state the cost plainly. The
documented ways to remove it:
- rustls's in-place decryption, when released (the `TODO` above);
- **kernel TLS on Linux.** rustls 0.23 already has a `kernel` module that can hand the
  session keys to the kernel, so decryption happens inside `recv`. That's step 21.

Step 17's zero-allocation test will show exactly this one exception.

**Other TLS choices:**

| Choice | Why |
|---|---|
| `ring` crypto backend | AES-GCM on AES-NI. `aws-lc-rs` is the other rustls backend and is often slightly faster, but it needs `cmake`, which this machine doesn't have. Swapping is one line; step 20 can measure it |
| **AES-128-GCM first** | Fewer rounds than AES-256, and ChaCha20 only wins on CPUs without AES hardware. The server picks among what we offer. **Verified: both Binance and Bybit negotiated `TLS13_AES_128_GCM_SHA256`** |
| Mozilla roots compiled in (`webpki-roots`) | No dependency on the OS certificate store; identical everywhere |
| Secure random from the crypto provider | For WebSocket keys and the per-frame mask (RFC 6455 §5.3). No extra dependency |

## 2. The connection

### Cold path: `connect()`, blocking, with one deadline
DNS (every address tried in turn) → TCP connect → socket options → TLS handshake →
WebSocket handshake, all bounded by `connect_timeout`. It runs once per connection, so
blocking is fine; never hanging is what matters (tested: a server that accepts TCP and
then says nothing hits the 300 ms deadline).

**Socket options:**
- **`TCP_NODELAY`:** turns off Nagle's algorithm, which would hold back our small frames
  (subscriptions, pings) waiting to batch them.
- **`SO_RCVBUF` = 4 MB:** a burst of market data can't overflow the kernel's buffer while
  we're busy.

**Frames that arrive with the handshake response** are kept: everything after the HTTP
response's blank line is already WebSocket data. Tested.

### Hot path: `poll()`, never blocks in spin mode
```rust
loop {
    match conn.poll()? {
        Some(msg) => handle(msg),     // borrows the connection's buffer; handle it, then poll again
        None => std::hint::spin_loop(),
    }
}
```
1. Consume the previous message's bytes (just an offset move).
2. **If a whole frame is already buffered, decode it without touching the socket.** If
   not, do **one** non-blocking read; `WouldBlock` means `Ok(None)` at once.
3. Decode: single-frame messages are returned **zero-copy** from the plaintext buffer.

**The plaintext buffer** is allocated once per connection (largest message plus two TLS
records), tracked with `start`/`end` offsets. Consuming a message moves no bytes; the
unread tail is moved to the front only when free space runs out.

**Protocol duties handled inside `poll`:** a ping is answered with a pong carrying the
same payload, and a server close is echoed once. Both are still returned to the caller.

### A borrow-checker wall, and the cleaner design it forced
My first `poll` decoded every message **twice**. The first version returned a message
borrowed from the connection's buffer, but sometimes also needed `&mut self` to send a
pong. Rust's current borrow checker rejects "return the borrow on one branch, use `self`
mutably on another" (a known limitation; the next-generation checker, Polonius, accepts
it). The double decode was my workaround.

The fix that also reads better: **look at the frame's opcode first** (a cheap header
parse that returns a plain value), then branch.
- **Data frames** (the hot path) are decoded **once** and returned.
- **Ping and close** (rare) copy what they need, do their protocol duty, then decode again
  to return the message. Control frames never touch fragment state, so decoding again
  gives the same result.

### Two bugs I fixed in my own first draft
1. **The double decode above:** wasted work on every message.
2. **Silent EOF:** `fill()` returned `1` instead of `0` when the socket closed without a TLS
   `close_notify`, so an abrupt disconnect would have looked like "no data yet" forever.
   Now any `read == 0` is EOF. The test `a_dropped_connection_is_an_error_not_a_hang`
   covers it.

## 3. Tests against a local TLS WebSocket server (9)
The tests run a server in the same process, with a throwaway certificate from `rcgen`.
That means real sockets and real TLS, deterministically, without the internet.

| Test | What it proves |
|---|---|
| All 120 recorded messages, **spin** mode | Arrive intact when the server writes in random sizes (1–3,000 bytes): TLS records end mid-frame, frames span records, several frames share a record. AES-128-GCM negotiated |
| Same, **block** mode | The blocking path delivers the same |
| Pings answered, closes echoed | Pong carries the same payload, masked; close echoed with the code; next `poll` → `Closed(Some(1000))` |
| Frames with the handshake response | Not lost |
| Client messages | Masked text frames |
| Refused upgrade | `403` reported as `Handshake(Status(403))` |
| Untrusted certificate | Refused (`UnknownIssuer`) with the production root set |
| Server drops the socket | An `Io` error, not a hang |
| Silent server | `Timeout` within the 300 ms deadline |

## 4. Spin vs block: measured
A local server sends ~1 KB messages carrying their send time. The client records
send → decoded, which covers the kernel's loopback, the wake-up, TLS decryption and the
WebSocket decode. Two runs:

| Cadence | Mode | p50 | p90 | p99 | max |
|---|---|---|---|---|---|
| Busy (every 200 µs) | **spin** | 10.8–11.5 µs | 39–47 µs | 71–79 µs | 131–145 µs |
| Busy (every 200 µs) | block | 12.2–12.6 µs | 26–52 µs | 66–87 µs | 91–**625** µs |
| Realistic (every 20 ms) | **spin** | 72–78 µs | 101–102 µs | 125–152 µs | 164–234 µs |
| Realistic (every 20 ms) | block | 88–91 µs | 115–123 µs | 159–170 µs | 205–**1,103** µs |

**Reading these honestly:**
- **Spinning is faster at the median** (about 1 µs when busy, about 15 µs when sparse),
  and **blocking produced the large outliers** (up to 1.1 ms: the thread had to be woken
  and rescheduled). For a feed thread on a dedicated core, spin is the default. `Block`
  remains for non-critical connections and development.
- **A surprise I can't fully explain on macOS:** with sparse messages (every 20 ms),
  latency is about **7× higher in both modes**. So it isn't the client's wait strategy;
  the system penalises idle gaps. Likely causes are the macOS loopback/TCP path after
  idle, and CPU power management (cores dropping into deeper sleep states between
  bursts). macOS doesn't allow kernel scheduling tracing without disabling its system
  protections, so I'm reporting it rather than guessing. It's exactly what step 21's Linux
  tuning targets: busy-polling sockets, isolated cores, disabled deep C-states.
- **Scale check:** these are microseconds on loopback. Over the internet to Binance and
  Bybit, the exchange-to-us delay was **75–506 ms** (section 5). The code path is now
  four orders of magnitude smaller than the distance. That's why real firms colocate.

## 5. Live: what the real exchanges revealed
`cargo test -p ob-net --release --test live -- --ignored --nocapture`:
```text
binance: connected in 892ms, cipher Some(TLS13_AES_128_GCM_SHA256)
binance: subscribe reply id=1 ok=true
binance: 20 depth snapshots, unbroken pu→u chain; best bid 58638.9 ask 58639; exchange→us delay (wall clocks) min 86 ms, max 91 ms
bybit: connected in 695ms, cipher Some(TLS13_AES_128_GCM_SHA256)
bybit: response Subscribe success=true
bybit: response Pong success=true
bybit: 300 book messages in sequence, applied (never crossed, ≤ 50 levels), widest message 67 levels/side, pong received; best bid 58640.9 ask 58641; exchange→us delay (wall clocks) min 75 ms, max 506 ms
```
The live test parses everything with steps 6–7's parsers, checks the sequences, applies
the Bybit book, and sends Bybit's JSON heartbeat (which the Java skeleton never sends).
The two venues' best prices are about $2 apart and not crossed, as simultaneous books
should be, unlike step 9's recordings taken 15 s apart.

### The bug it found: Bybit deltas carry more than 50 levels
The first live run **failed**: `TooManyLevels(Bids)`. A probe of 399 live messages
showed **deltas with up to 66–67 levels on one side** for a 50-level stream; the
60-message recording never exceeded 50. A delta lists *changes*: in a fast market one
20 ms delta deletes levels that leave the top 50 **and** inserts their replacements, up to
50 + 50 = 100 per side.

That broke two things:
1. **Parsing:** the per-message limit was 64. It's now 128, which covers the theoretical
   100 with room.
2. **A silent book bug:** Bybit's book used capacity 64. Applying a big delta in order
   (Bybit lists best-first, so inserts of better prices come before deletes of the old
   top), the book briefly exceeds 64 levels. The overflow drops the "worst" levels, but
   the same delta's later deletions would have brought them back into the top 50. **Real
   levels are lost, silently.** The new test `a_big_delta_needs_capacity_for_twice_the_depth`
   shows capacity 64 ending with **44 levels instead of 50**, and capacity 128 matching
   Bybit's book exactly. **The Bybit feed uses `OrderBook<128>`** (capacity ≥ 2 × depth).

Step 7's note has a correction appended. **Lesson:** a 60-message recording isn't a
specification; live data has to be part of testing.

## 6. What this step deliberately doesn't do
- **Reconnection, heartbeat timing, venue health:** the feed's state machine (step 13)
  owns those. `WsConnection` reports errors; the feed decides what to do.
- **Kernel bypass or kernel TLS:** Linux only (step 21).
- **Removing rustls's per-record allocation:** not possible in 0.23 (section 1).

## 7. Java counterpart
The skeleton:
```java
public class BinanceWebsocketClient extends WebSocketClient {   // org.java_websocket
    public void onMessage(String message) { ... }               // called on the library's thread
}
binanceClient.connect();   // returns at once; no timeout, no reconnect, no error path
```
- **Threads:** Java-WebSocket runs its own reader thread per client and calls `onMessage`
  on it. Our feed thread polls the socket itself and parses in place, with no hand-off.
- **TLS:** the JDK's `SSLEngine` (plus the library's buffers) copies on every read,
  similar to rustls today. Kernel TLS on Linux is available in Java only through native
  libraries (e.g. netty-tcnative).
- **No connect deadline, no handshake validation hooks, no heartbeat:** `connect()`
  returns immediately; a stalled TLS handshake is never noticed. Bybit closes the
  connection after missing pings, and the skeleton just logs "closed".
- **Busy polling in Java:** Netty's native epoll transport exposes
  `EpollChannelOption.EPOLL_BUSY_POLL_USEC`, or you spin on a non-blocking
  `SocketChannel` yourself. Same trade-off as `WaitMode::Spin`, plus GC pauses on top.

## How to verify it yourself
```bash
cd rust
cargo test -p ob-net --release --test conn                                       # 9 local TLS tests
cargo test -p ob-net --release --test latency -- --ignored --nocapture           # spin vs block table
cargo test -p ob-net --release --test live -- --ignored --nocapture --test-threads=1   # real exchanges
cargo test -p ob-parse --release --test bybit a_big_delta                         # the capacity regression
```
Things to try:
- In `bybit.rs`'s capacity test, change `OrderBook<128>` to `OrderBook<64>` in the second
  half: it fails with 44 levels.
- In `conn.rs`'s `poll()`, change `Ok(0) => return Err(...UnexpectedEof...)` to
  `Ok(0) => {}`, so EOF is treated as "no data yet".
  `a_dropped_connection_is_an_error_not_a_hang` then fails with "no error within 5 s":
  a dead connection would look alive forever. (I ran this. My first suggestion here,
  removing `fill()`'s early return, turned out not to change behaviour, because `fill()`
  returns 0 on EOF either way.)

## Likely interview questions
1. **Why spin instead of blocking on the socket?**
   Measured: spinning saves ~1–15 µs at the median and avoids the wake-up outliers
   (blocking hit 0.6–1.1 ms) at the cost of a full core. On a feed thread with a dedicated
   core that's the right trade.
2. **What does `TCP_NODELAY` do?**
   It disables Nagle's algorithm, which delays small writes to coalesce them; our pings
   and subscriptions are small and latency-sensitive.
3. **Why AES-128-GCM?**
   It's the cheapest authenticated cipher on AES-NI hardware: fewer rounds than AES-256,
   and ChaCha20 wins only without AES instructions. Both exchanges negotiated it.
4. **Is your TLS path zero-copy?**
   No, and I checked why: rustls 0.23 hands decrypted data over as an owned `Vec` per
   record in both APIs. In-place decryption is a `TODO` upstream; on Linux, kernel TLS
   removes the copy entirely.
5. **What did testing against the live exchanges find?**
   Bybit deltas with 67 levels on a 50-level stream. That broke parsing, and showed that a
   book with capacity 64 silently loses real levels during big deltas. Capacity must be
   ≥ 2 × depth, and a regression test now proves it.
6. **How do you test TLS networking deterministically?**
   A local server in the same process with a throwaway certificate: real TLS, real
   sockets, scripted behaviour (random record sizes, pings, closes, drops, silence), no
   internet.
7. **What's the biggest latency in this system?**
   Distance: 75–500 ms from the exchanges to this laptop, against ~10–80 µs for TLS,
   wake-up and decode. That's why HFT firms colocate in the exchange's data centre (AWS
   Tokyo for Binance, AWS Singapore for Bybit).
