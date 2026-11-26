# Step 24: The read path: measured, then our own TLS record layer

Step 23 found the read stage (socket read, TLS decryption, WebSocket decode) to be the
largest part of our in-process time: 3.6 µs at p50 and 23 µs at p99. This step first
**took that stage apart**, then removed the part that was ours to remove.

## What changed

| File | What |
|---|---|
| `crates/ob-bench/src/readpath.rs` | **New scenario** (`tails --only readpath`): each part of the read stage timed separately. |
| `crates/ob-net/src/record.rs` | **New, public:** a TLS 1.2 / 1.3 record layer. AES-128-GCM, AES-256-GCM and ChaCha20-Poly1305 via `ring`; decrypts in place. 5 tests. |
| `crates/ob-net/src/session.rs` | **New:** the handshake through rustls's unbuffered API, the hand-over through `rustls::kernel`, in-place receive, key updates, session tickets and alerts. 7 tests against a real rustls server. |
| `crates/ob-net/src/conn.rs` | `WsConnection` now runs on the session: one receive buffer where ciphertext arrives, is decrypted in place and decoded. Sends `close_notify` on close. New `protocol_version()`. |
| `crates/ob-net/src/tls.rs` | `enable_secret_extraction = true` (needed for the hand-over); module docs. |
| `crates/ob-feed/tests/zero_alloc.rs` | The over-TLS test now asserts **zero** allocations per message (was: exactly one, inside rustls). |
| `README.md`, `CONSIDERATIONS.md`, `docs/TUNING.md` | The "one allocation per TLS record remains in rustls" limitation is gone; kTLS's hand-over now exists. |

## 1. Taking the read stage apart

The breakdown runs rustls over an in-memory connection, with a real handshake and no
sockets, so its crypto, buffering and allocation are exactly production's. The system
call is timed separately on a loopback socket. Per message, p50 / p99:

| Part | Binance (192-byte records on average) | Bybit (360 bytes) |
|---|---|---|
| **`recv` returning data** | **3.21 µs / 19.6 µs** | 3.27 / 19.7 µs |
| `recv` with nothing waiting (each idle spin) | 586 / 897 ns | 596 ns / 1.30 µs |
| rustls: take in (`read_tls`) | 22 / 92 ns | 22 / 114 ns |
| rustls: decrypt + its `Vec` (`process_new_packets`) | 367 ns / 1.37 µs | 418 ns / 1.58 µs |
| rustls: copy out (`reader().read`) | 66 / 166 ns | 86 / 273 ns |
| WebSocket decode | 53 / 191 ns | 68 / 231 ns |
| *AES-128-GCM alone, in place: the floor* | *134 / 249 ns* | *144 / 475 ns* |

Two conclusions:
- **~85% of the read stage is the operating system's `recv`.** No user-space change
  reaches it on macOS. Part of the 3.2 µs is likely the loopback test path (the receiver
  processing its own TCP acknowledgements), but I haven't measured how much. The fixes are
  Linux's: busy polling, `io_uring` (completions read from shared memory, no system
  call), kernel bypass. Step 28 documents them with this measurement as the evidence.
- **rustls costs ~0.46 µs, of which ~0.3 µs is above the AES floor:** one allocation and
  two copies per record. Its p99 is 1.4–1.6 µs, its p99.9 ~15–17 µs (allocator stalls).
  That part is ours to fix.

## 2. Why rustls allocates, and the sanctioned way around it

rustls 0.23.46 decrypts each record inside its own buffer, then
`received_plaintext.append(...)` makes an owned `Vec`. Both read APIs return those
`Vec`s, and in the "unbuffered" API, in-place decryption is literally a TODO
(`ReadTraffic::next_record`). We then copied the plaintext into our WebSocket buffer.

rustls offers **`rustls::kernel`**, built for kernel TLS. rustls does the handshake and
hands over the traffic keys. A `KernelConnection` keeps the secrets, so it can still
compute new keys on a key update and store session tickets. "Everything else you will
need to implement yourself", in the module's words.

Options considered:

| Option | Verdict |
|---|---|
| Keep rustls end to end | ~0.3 µs and an allocation per record; p99.9 ~15 µs |
| A pre-faulted allocation pool behind rustls | removes the allocator's slow path, keeps both copies; works around rustls instead of using its API |
| Fork rustls for in-place decryption | a fork to maintain, in security code |
| Kernel TLS | Linux only, and still needs exactly this hand-over (it's what `rustls::kernel` is for) |
| **Our record layer via `rustls::kernel`** | **chosen:** rustls keeps everything hard (certificates, key exchange, key schedule); we only frame, build nonces, count sequence numbers and call `ring`'s AEAD, which is the same implementation rustls uses here |

## 3. The design

```text
one receive buffer per connection
[ consumed | plaintext: start..end | gap | ciphertext: raw_start..raw_end | free ]
                                ▲
     recv() appends ciphertext ─┘   each complete record is decrypted from raw_start
     straight to end (ring's open_within writes the plaintext at the start of the range
     it is given): the plaintext stays contiguous, so a WebSocket message spanning
     records is decoded in place.
```

- **Records** (`record.rs`, RFC 8446 §5 and RFC 5246 / 5288 / 7905). TLS 1.3: nonce =
  iv XOR sequence number, AAD = the 5-byte header, the inner content type is the last
  non-zero byte (padding stripped). TLS 1.2 AES-GCM: an 8-byte explicit nonce in each
  record, and a 13-byte AAD (sequence number, type, version, length). Every record's
  header must say version 3.3. Lengths are bounded (2^14 + 256 ciphertext, 2^14
  plaintext). A record that isn't wholly present is an error, never a panic.
- **Handshake** (`session.rs`): rustls's unbuffered client, driven over the blocking
  socket within the connect deadline. Handshake records are read straight into the
  connection's receive buffer, so nothing is copied when we take over. Then
  `dangerous_into_kernel_connection()` gives us the keys and sequence numbers.
- **After the handshake:**
  - **Session tickets** go to rustls (`handle_new_session_ticket`), so the next
    connection can resume. A test proves resumption happens.
  - **Key updates** take the new receive key from rustls (`update_rx_secret`). If the
    peer asks, we answer under the old key, then switch (`update_tx_secret`). RFC 8446
    §4.6.3: a key update must end its record; anything after it in the same record is
    an error.
  - **Handshake messages split across records** are reassembled, up to 64 KiB.
  - **Alerts:** `close_notify` ends the stream, and a record after it is an error.
    `user_canceled` is a warning; anything else is fatal.
- **Sending:** frames are sealed into a reused output buffer (split at 2^14). If the
  number of records under one key ever reached the cipher's confidentiality limit, TLS
  1.3 updates our key first and TLS 1.2 reconnects. `close()` now sends `close_notify`
  after the WebSocket close frame.

## 4. Proving it correct

Against **a real rustls server** (in memory, `session.rs` tests), for **all six cipher
suites** (AES-128/256-GCM and ChaCha20-Poly1305, on TLS 1.2 and 1.3):

| Test | What it proves |
|---|---|
| `every_suite_and_split_gives_exactly_the_servers_plaintext` | messages of 0 B to 40 KB (spanning records) fed in pieces of 1, 2+3, 5, 4096, mixed and all-at-once bytes → exactly the server's plaintext |
| `key_updates_in_both_directions` | a server key-update request mid-stream: we decrypt under its new key, answer, and rustls reads our data under *our* new key |
| `the_server_reads_what_we_send` | rustls accepts our records at 1 B, 2^14, 2^14 + 1 bytes, and our `close_notify` |
| `session_tickets_enable_resumption` | the next connection resumes: tickets really reach rustls |
| `close_notify_then_nothing_more` | data before `close_notify` is delivered, then the session is closed |
| `a_record_after_close_notify_is_an_error` | forged with the server's own keys (secret extraction): what a real server never sends |
| `a_changed_bit_is_an_error` | one flipped bit at every byte position: an error, or (a changed length) nothing delivered; never wrong plaintext, never a panic |

Plus `record.rs` unit tests: round trips at every size limit, content types, padding,
tampering at every byte, replay and reordering, oversized records.

**Mutation testing.** Seven deliberate bugs:
- nonce XOR on the wrong bytes;
- ignoring a key update;
- not answering a key-update request;
- the wrong TLS 1.2 AAD length;
- not stripping TLS 1.3 padding;
- dropping application data;
- accepting records after `close_notify`.

The first six were caught at once. The seventh survived, which is why
`a_record_after_close_notify_is_an_error` exists; it now fails too.

**Bugs the tests found in my first version:**
1. **TLS 1.2 record versions weren't checked.** TLS 1.2's AAD uses a fixed version, so a
   changed version byte passed. Found by the tamper test; every record must now say 3.3.
2. **A changed length field could make `open` index past the buffer**, which is a panic,
   so a crash with `panic = "abort"`. `receive` never passes an incomplete record, but
   `open` must not rely on that. It now checks.
3. **25 bytes allocated per record**, from `ok_or(Error::General("…".into()))` in the
   sequence-number increment: `ok_or` builds its argument even on success. Found by the
   zero-allocation proof (a backtrace from inside the counting allocator) and fixed with
   `ok_or_else`. A good illustration of why the proof is a test, not a claim.

**Live:** both exchanges negotiate TLS 1.3 with AES-128-GCM. `ob live` ran for 12 s on
both venues with the new layer: about 7,000 messages, no gaps, rejections, resyncs or
reconnects.

## 5. Results

Reference file: `rust/bench-results/tails-MacBookPro15,2_-20261126-130544.txt`.

### The A/B that matters: the same records, interleaved, same run

For every message, the read-path scenario decrypts the record with rustls (as before)
and with our layer, one after the other. Machine load hits both equally, so this
comparison holds even on a busy machine. Per record, p50 / p99 / p99.9:

| | rustls (decrypt + `Vec` + both copies) | **Ours (decrypt in place)** | AES-128-GCM alone |
|---|---|---|---|
| Binance (192 B mean) | 459 ns / 820 ns / 18.2 µs | **173 / 276 / 381 ns** | 128 / 175 / 278 ns |
| Bybit (360 B mean) | 503 ns / 897 ns / 15.4 µs | **180 / 483 / 524 ns** | 131 / 441 / 468 ns |

- **p50: −62% to −64%** (about 0.3 µs per record), now within ~45 ns of the raw AES-GCM
  cost: the nonce, the record checks and moving the plaintext.
- **p99.9: from 15–18 µs to under 0.6 µs.** The tail was the allocator and copying,
  not the cryptography.
- The earlier standalone run (moderate load) agreed: rustls 470 / 513 ns, ours
  175 / 185 ns at p50.

### The whole receive path: honest about what moved

The machine was heavily loaded during this run (load average 9.0 at the start, 11.3 over
five minutes, on 4 cores; step 23's reference started at 2.4). The wire scenario runs
over a real socket, so its rows moved with the load, not with this change:

| Wire, busy, read stage (p50 / p99) | Step 23 (load ~2–7) | Step 24 (load ~9–11) |
|---|---|---|
| Binance | 3.62 / 22.7 µs | 3.49 / 30.6 µs |
| Bybit | 3.85 / 22.7 µs | 3.64 / 26.3 µs |

The p50 dropped by 0.13–0.21 µs, in the expected direction, but the p99 is set by the
load. The read stage is ~85% `recv` (still 2.98–3.10 µs at p50 in this run), so on this
Mac, removing 0.3 µs of TLS work shows as ~5–9% at the median. That is exactly what the
breakdown predicted. The tail win is real (the interleaved table above), but a socket
benchmark on a busy laptop can't resolve it. The second Mac's run, on a quieter machine,
is the place to see it.

**Zero allocations** over real TLS: the feed thread makes 0 allocations in 55 messages,
down from 55.

## How to verify it yourself

```bash
cd rust
cargo test -p ob-net --release --lib session     # 7 tests against a real rustls server
cargo test -p ob-net --release --lib record      # 5 record-layer tests
cargo test -p ob-feed --release --test zero_alloc -- --nocapture   # "0 allocations in 55 messages"
target/release/tails --rt --only readpath        # rustls vs ours vs the AES floor
cargo test -p ob-net --release --test live -- --ignored --nocapture # live: TLS 1.3, AES-128-GCM
```

Things to try:
- In `record.rs` `next_seq`, change `ok_or_else(|| …)` back to `ok_or(…)` and run the
  zero-allocation test: `25 allocations in 55 messages`.
- Remove the `header[1..3] != [3, 3]` check: `tampering_and_reordering_are_rejected`
  fails on byte 1 of a TLS 1.2 record.
- In `session.rs`, make `if requested {` read `if false {`:
  `key_updates_in_both_directions` fails.

## Java counterpart

The JDK's `SSLEngine` is the equivalent of rustls's unbuffered API: you feed it
`ByteBuffer`s of ciphertext, and it `unwrap`s into a plaintext `ByteBuffer` you supply.
With direct buffers, that is already one copy and no allocation per record, which is
more than rustls offers today. Its decryption runs through JCE AES-GCM, which is
intrinsified with AES-NI and CLMUL but still costs more than `ring` per record, and some
paths create objects per call. Low-latency Java shops either pool everything around
`SSLEngine` or terminate TLS outside the JVM: a proxy, or kernel TLS via JNI. There is no
standard Java API for handing keys to kTLS.

## Likely interview questions

1. **Isn't writing your own TLS code dangerous?** Yes, which is why the scope is the
   record layer only. The handshake, certificate verification and key schedule remain
   rustls's, and the AEAD is `ring`'s. What we wrote is framing, nonce construction and
   sequence numbers: small, specified exactly by the RFCs, and tested against rustls
   itself across every suite, split point, key update and tampered byte, with mutation
   testing to show the tests have teeth. It's the same split kernel TLS uses.
2. **What happens on a TLS key update?** The server sends KeyUpdate. rustls's
   `KernelConnection` derives the next receive secret (it kept the traffic secret), and
   we rebuild the AES key with the sequence number back at 0. If the server asked, we
   send our own KeyUpdate under the old key and switch our sending key. A KeyUpdate must
   end its record, because the next record is already under the new key.
3. **How can a WebSocket message span TLS records and still be zero-copy?** Each record
   is decrypted *to the end of the existing plaintext*, not where it arrived: `ring`'s
   `open_within` takes the range from the end of the plaintext to the end of the record,
   and writes the plaintext at its start. The headers and tags in between disappear,
   and the plaintext is one contiguous run.
4. **Why does the read stage still take ~3 µs on this Mac?** The `recv` system call. Only
   the OS path can change that: Linux busy polling, `io_uring` or kernel bypass (step 28).
5. **What did the zero-allocation test catch?** `ok_or` evaluates its argument eagerly,
   so building an error message cost an allocation on every successful record. Lazy
   variants (`ok_or_else`, `unwrap_or_else`) exist for exactly this.
