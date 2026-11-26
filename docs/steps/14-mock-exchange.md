# Step 14: Mock-exchange integration tests

Step 13 proved the feed's **decisions** with a fake clock: "after 5 s of silence, ask
for a reconnect". This step proves that those decisions are **carried out over a real
connection**:
- requests reach the server;
- closes are clean;
- a dead socket is noticed;
- retries really wait;
- the aggregator drops the venue and re-admits it at the right moments.

To do that, a **mock exchange** (a local TLS WebSocket server) plays scripted sessions
against the real code path: driver → socket → TLS → WebSocket → parser → book →
seqlock → aggregator.

## What changed

| File | What |
|---|---|
| `crates/ob-feed/tests/mock_exchange.rs` | The mock exchange, a client harness and 8 scenarios. |
| `crates/ob-feed/src/venues.rs` | `Bybit::with_heartbeat(interval)`, so tests can ping every 200 ms. Production keeps Bybit's 20 s. |
| `crates/ob-feed/Cargo.toml` | Test-only dependencies: `rustls` (the server side) and `rcgen` (a throwaway certificate). |

## 1. The design

```text
 test thread                     driver thread (real code)          mock server thread
┌──────────────┐  events (mpsc)  ┌──────────────────────────┐  TLS  ┌────────────────────┐
│ wait_for(..) │◀────────────────│ driver::run → Feed<V>    │◀─────▶│ script per conn:   │
│ Aggregator   │                 │   → seqlock publisher    │ 127.0.│  handshake, read   │
│  .state(..)  │◀─ BookReader ───│                          │  0.1  │  subscribe, send   │
│ gate.send()──┼─────────────────┼──────────────────────────┼──────▶│  fixtures, wait on │
└──────────────┘                 └──────────────────────────┘       │  gate, hang up ... │
                                                                    └────────────────────┘
```

- **One script per incoming connection, in order.** Connection 1 might stream and then
  hang up; connection 2 checks that the client came back. After the last script the
  listener closes, so any further attempt is refused: the venue is "down".
- **Gates (`mpsc` channels) hold the server at an exact point** while the test inspects
  the aggregator. For example, the server holds the fresh snapshot back after a gap, and
  the test asserts the venue is `Excluded(Syncing)` *during* the resync. Without gates
  that window lasts microseconds and the test would be a race.
- **The client is the production driver** on its own thread. Its log callback sends
  events to the test with a timestamp, so the test can measure timing: how long the
  outage lasted, and when the stale or pong timeout fired.
- **Expected books are computed without I/O.** `expected_book()` runs the same frames
  through a `Feed` directly, and the test compares the levels the real pipeline
  published. So the socket path must produce exactly the same book as the unit-tested
  path.
- **Timings are scaled down** through `FeedConfig`: stale after 500 ms, pong timeout
  300 ms, backoff 50 ms doubling to 400 ms. The whole suite runs in **0.9 s**.
- **A broken test fails rather than hangs.** The server's sockets have a 10 s read
  timeout, and each wait has a 5 s deadline that prints every event seen so far.

## 2. The scenarios (8)

| Test | Server does | Must happen |
|---|---|---|
| `bybit_gap_resyncs_over_the_same_connection` | Snapshot + deltas, then skips a delta; checks it receives `unsubscribe` then `subscribe` on the **same** connection; holds the snapshot | Venue `Excluded(Syncing)` with no bids quoted; after the snapshot, the book exactly matches the expected one. 1 resync, 0 reconnects. |
| `an_abrupt_disconnect_takes_the_venue_out_at_once_then_it_reconnects` | Streams, then drops TCP (no close frame, no TLS close_notify) | `Excluded(Down)` immediately; retry delay 25–50 ms; Live again. **Measured outage: 34 ms.** |
| `a_silent_connection_goes_stale_and_is_replaced` | Streams, then says nothing with the socket left open | At 250 ms the **aggregator** already excludes the venue (`Stale`, max age 100 ms) while the feed hasn't acted yet. At 500 ms the feed reconnects with a **clean close (1000)**, which the server checks. |
| `binance_server_pings_are_answered` | Sends a WebSocket ping | A pong with the **same payload** (Binance drops clients that don't answer). |
| `bybit_heartbeats_and_an_unanswered_ping_forces_a_reconnect` | Checks the exact ping JSON and answers the first ping only | `PongTimeout` between 650 ms and 1.2 s (ping at 400 ms + 300 ms deadline), then reconnect. Every later client frame was a ping. |
| `malformed_messages_are_dropped_and_broken_frames_kill_the_connection` | Sends truncated JSON, then a frame with a reserved bit set | The JSON is `Rejected` and the venue stays in, with the next snapshot applied. The bad frame → `Lost(Ws(ReservedBits))` → reconnect. |
| `a_venue_that_is_down_is_retried_with_growing_delays` | Accepts and immediately closes 5 times | 5 `ConnectFailed`; the gaps between attempts fit the jitter bounds (below). |
| `one_venue_down_does_not_disturb_the_other` | Binance streams every 10 ms; Bybit hangs up and refuses from then on | For 300 ms: Binance `Included` and still updating, Bybit `Excluded(Down)`, and only Binance quoted. Binance: 0 disconnects. |

Measured gaps between connection attempts (one run):

```text
caps (jitter range [cap/2, cap]):  50      100      200      400      400   ms
gaps:                              34.4    100.0    142.7    320.5    261.1 ms
```

Each gap falls within `[cap/2, cap]` plus the time a failed TLS handshake takes. The
test allows 100 ms of slack above the cap and none below it.

## 3. What testing it found

These were bugs **in my tests**, not in the feed code. They are worth knowing because
they're the classic mistakes in this kind of test:

1. **The recording contains a non-book message.** The third line of the Bybit fixture
   is the subscription acknowledgement, recorded live as it arrived. So 5 + 10 frames
   hold 4 + 9 book updates, not 15. The code was right; my expected count was wrong.
2. **The race between the server writing and the test stopping.** The client goes Live
   on the first book frame, and the test then stopped it while the server was still
   writing the rest. The result was an occasional `BrokenPipe` in the server: 3 of 5
   runs failed, and the failure never showed up when the test ran alone, only under
   parallel load. The fix: once the test stops at Live, the server sends only one book.
   Lesson: in a networked test, decide **who ends the conversation**.
3. **A timing count was too tight.** "More than 30 snapshots in 300 ms at a 10 ms pace"
   ignores scheduling time on top of each sleep. I measured exactly 30 once, so the
   bound is now more than 20.

I then ran the suite 10 times in a row: all passed.

## 4. Proof that the tests catch real bugs (mutation testing)

I broke the code on purpose three times and ran the suite each time:

| Mutation | Failing mock tests | Would step 13's tests catch it? |
|---|---|---|
| The driver drops the socket on `Reconnect` instead of `close(1000)` | silent-connection, heartbeat (2 of 8) | **No**: the driver had no tests before this step |
| The driver ignores the feed's `Send` commands | all 8 | **No**: same reason |
| `go_down` doesn't publish `Down` | abrupt-disconnect, one-venue-down | Yes, 1 test (the fake-clock backoff test) |

**This is the point of the step.** The sans-I/O design moved nearly all the logic out
of the driver, but the driver still carries it out, and only an end-to-end test checks
that.

## 5. What this step deliberately doesn't do

- **Fault injection at the TCP level** (packet loss, latency, partial writes). Step 12's
  connection tests already split frames across TLS records at random. Loss and latency
  need a proxy such as `toxiproxy`, which I mention in CONSIDERATIONS.md rather than
  adding a dependency.
- **The real 24 h Binance disconnect.** It looks identical to `hang_up`, which is
  covered.
- **TLS failures** (wrong certificate, expired certificate). Step 12 covers refusal in
  `ob-net`; here every TLS failure looks like `ConnectFailed`.

## 6. Java counterpart

The skeleton has no tests of its own beyond a smoke test, and no mock server. A Java
version of this step would typically use:

```java
// OkHttp's MockWebServer can upgrade to WebSocket and script frames:
MockWebServer server = new MockWebServer();
server.enqueue(new MockResponse().withWebSocketUpgrade(new WebSocketListener() {
    @Override public void onOpen(WebSocket ws, Response r) {
        ws.send(snapshotJson);   // then deltas, then ws.cancel() to simulate a drop
    }
}));
// …and Awaitility for timing assertions:
await().atMost(1, SECONDS).until(() -> aggregator.state("bybit") == EXCLUDED_DOWN);
```

- Same idea: one scripted response per connection, `cancel()` as a hang-up.
- Differences:
  - MockWebServer runs plain TCP unless you configure TLS certificates (OkHttp's
    `HeldCertificate` makes that easy), so many Java tests skip TLS altogether. Ours
    always runs real TLS.
  - Java timing tests also suffer GC and JIT warm-up pauses. That's why Java suites
    usually allow much larger margins than our 100 ms.

## How to verify it yourself

```bash
cd rust
cargo test -p ob-feed --release --test mock_exchange                  # 8 scenarios, < 1 s
cargo test -p ob-feed --release --test mock_exchange -- --nocapture   # prints outage and retry gaps
for i in $(seq 10); do cargo test -p ob-feed --release --test mock_exchange 2>&1 | grep 'test result'; done
```

Things to try (I ran all three):
- In `driver.rs`, replace `let _ = c.close(1000);` (in the `Command::Reconnect` arm)
  with `drop(c);`. The silent-connection and heartbeat tests fail: the server sees the
  connection end without a close frame.
- In `feed.rs`'s `go_down`, delete `self.publish(now, VenueStatus::Down);`. The
  abrupt-disconnect test fails on `Excluded(Down)`: the aggregator would keep quoting
  the dead venue's last book until its age check fires.
- In `mock_exchange.rs`, change the gap test's `s.text(f[6])` to `s.text(f[5])` (no
  gap). It fails waiting for a `Resync`, and the failure message lists every event
  seen. That's the debugging output you get when a scenario goes wrong.

## Likely interview questions

1. **How do you test disconnection handling without the internet?**
   A local TLS WebSocket server plays scripted sessions to the production driver:
   streaming recorded messages, gaps, silence, garbage, abrupt hang-ups and refused
   connections. Timings are scaled down through config, so 8 scenarios run in under
   a second.
2. **How do you make timing-dependent tests not flaky?**
   - Assert on **events**, not sleeps: wait for "Live", with a deadline.
   - Use gates to freeze the server at the exact point you want to inspect.
   - Put lower bounds exactly where they're guaranteed (backoff is never shorter than
     cap/2) and add generous slack on upper bounds.
   - Make sure the test controls who ends each conversation.
   - Run the suite repeatedly to flush out flakes; that is how the broken pipe showed
     up.
3. **What do integration tests catch that unit tests don't?**
   Here, measured: two driver bugs (no clean close, dropped requests) that 14
   state-machine tests can't see, because the driver only exists in the I/O path.
4. **How do you simulate a network failure?**
   Shut down the TCP socket with no WebSocket close and no TLS close_notify. That's
   exactly what a crash, a NAT timeout or a cable pull looks like to the client. For
   silence, keep the socket open and send nothing (a half-open connection).
5. **How quickly do you recover from a drop?**
   Measured locally: 34 ms from detecting the loss to being Live again (25–50 ms of
   jittered backoff plus the handshake). Against the real exchanges the handshake adds
   several round trips, hundreds of ms from this laptop. That's why a resync stays on
   the same connection whenever it can.
6. **Does one venue's failure affect the other?**
   No, and the test shows it. Each venue has its own thread, connection, backoff and
   seqlock slot. While Bybit was down and retrying, Binance kept publishing and the
   aggregator quoted Binance alone.
