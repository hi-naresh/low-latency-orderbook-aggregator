# Step 13: Venue state machines (subscribe, heartbeat, backoff, staleness, resync)

Steps 6–7 parse each venue's messages; step 12 opens a TLS WebSocket. This step adds the
logic between them: **when to subscribe, when to trust the book, when to rebuild it, and
when to give up on the connection and reconnect**. The README requires this ("handle
disconnections gracefully"). The skeleton only logs `"connection closed"`.

All of this lives in a new crate, `ob-feed`, which is `#![forbid(unsafe_code)]`.

## What changed

| File | What |
|---|---|
| `crates/ob-feed/src/venue.rs` | The `Venue` trait: what differs between exchanges (endpoint, request bytes, heartbeat, how to apply a message). Plus `Outcome`, `Request` and `ResyncReason`. |
| `crates/ob-feed/src/venues.rs` | The `Binance` and `Bybit` adapters: a parser, a book and the request bytes for each. |
| `crates/ob-feed/src/feed.rs` | `Feed<V>`: **the state machine**. Sans-I/O: it takes `now` and returns `Command`s. |
| `crates/ob-feed/src/backoff.rs` | Exponential backoff with equal jitter, plus `mono_ns()`, the clock shared by the whole process. |
| `crates/ob-feed/src/driver.rs` | `run()`: the I/O loop for one venue thread. It does no decision-making of its own. |
| `crates/ob-feed/tests/feed.rs` | 14 deterministic tests: a fake clock, recorded fixtures and the real aggregator. |
| `crates/ob-feed/tests/live.rs` | An ignored test: both venues live, with the aggregator, for 20 s. |

## 1. The design: three layers, with one job each

```text
 venue thread                                                    any reader thread
┌─────────────────────────────────────────────────────────┐
│ driver::run     owns the socket; sleeps/spins; no logic  │
│    │ on_connected / on_text / on_tick / on_disconnected  │
│    ▼                         ▲ Commands (Send, Reconnect, │
│ Feed<V>  state machine,      │  Log), fixed ring of 16    │
│    │     timers, backoff ────┘                            │
│    ▼ on_text(bytes) -> Outcome                            │
│ V: Venue  parser + book; says what a message means        │
│    │ publish(book, status)                                │──▶ BookReader ──▶ Aggregator
└────┼─────────────────────────────────────────────────────┘     (excludes Down/Syncing/
     ▼  seqlock (step 8)                                           stale/crossed, step 9)
```

- **`Venue` interprets messages; `Feed` decides what to do.** The adapter returns an
  `Outcome`: `Applied`, `Ignored`, `Rejected`, `Resync(reason)`, `Pong` or
  `Subscribed{ok}`. Exchange-specific rules stay in the adapter. A Binance gap is
  harmless; a Bybit gap means the book is wrong. The state machine is shared by both.
- **The feed does no I/O.** Every entry point takes `now: u64` (monotonic ns) and pushes
  commands. That's why every timing rule can be tested with a fake clock. "Bybit misses a
  pong at 51 s" runs in microseconds, with no sleeping and no network.
- **The feed is generic, `Feed<V: Venue>`, not `Box<dyn Venue>`.** Each venue gets its
  own compiled copy with `on_text` inlined. There's one feed per thread, so code size
  doesn't matter.
- **`Commands` is a fixed ring of 16, not a `Vec`.** The worst case is a handful of
  commands per event, so this path never allocates.

## 2. The state machine

```text
Disconnected ──connected──▶ Subscribing ──first book──▶ Live
     ▲                           │                        │ gap / crossed / bad data
     │ backoff + jitter          │ subscribe timeout      ▼
     └────── Reconnect ◀─────────┴──── too many ──── Syncing (unsubscribe + resubscribe)
             (stale feed, pong timeout, refused,         │ fresh snapshot
              too many resyncs, sync timeout)            └──────────────▶ Live
```

| State | Published status | Aggregator uses it? | Leaves when |
|---|---|---|---|
| `Disconnected{retry_at}` | `Down` | no | `ready_to_connect(now)` and the driver connects |
| `Subscribing{since}` | `Syncing` | no | the first `Applied` → Live; subscribe refused or 10 s without a book → reconnect |
| `Live{since}` | `Live` | yes (unless stale or crossed) | resync, stale, pong timeout, or the connection drops |
| `Syncing{since}` | `Syncing` | no | a fresh snapshot → Live; 10 s without one → reconnect |

**Health travels with the book.** Every transition republishes the book through the
seqlock with the new `VenueStatus`. The aggregator reads status and book in **one
consistent snapshot** (step 8 packed them together), so it can never combine "Live" with
a book from before a resync. The status changes in the same write that ends trust in the
book.

### The rules, with the defaults (`FeedConfig::default()`)

| Rule | Default | Why this value |
|---|---|---|
| `subscribe_timeout` | 10 s | Both venues send a snapshot within ~100 ms. 10 s covers a slow server without waiting forever. |
| `stale_after` | 5 s | BTCUSDT updates every 20–100 ms, so 5 s of silence means the connection is dead even if TCP doesn't know it yet (a half-open connection). The aggregator has its own, stricter staleness check per query (step 9). |
| Heartbeat | Bybit: ping every 20 s | Bybit's documented requirement. Binance pings *us*, and the transport answers (step 12). |
| `pong_timeout` | 10 s | Within Bybit's limit, and long enough for a slow round trip. |
| `max_resyncs` / `resync_window` | 5 per 60 s | If resubscribing keeps failing, the cause is the connection or the server: reconnect. |
| `healthy_after` | 30 s | Backoff resets only after a connection has stayed Live this long, so a connection that drops right after connecting keeps backing off. |
| Backoff | 250 ms doubling to 30 s, equal jitter | See section 3. |

### Resync vs reconnect: two levels of recovery

- **Resync** is cheap: the connection is fine but the book isn't. Clear the book, mark it
  `Syncing`, send `UNSUBSCRIBE` and `SUBSCRIBE`, and wait for a fresh snapshot. Bybit
  sends one on subscribe. That's a ~100 ms recovery instead of a TLS handshake (hundreds
  of ms to the venue plus backoff).
- **Reconnect** is expensive: something is wrong below the book (silence, an unanswered
  ping, a refused subscription, resyncs that don't fix things). Close the connection,
  back off, and start over.

What triggers a resync differs by venue, and the adapter decides:

| | Binance (`depth20`) | Bybit (`orderbook.50`) |
|---|---|---|
| Sequence gap (`pu ≠ last u` / `u ≠ last u + 1`) | Count it and apply. Every message is a full snapshot, so the book is right anyway. | **Resync**: a lost delta can't be recovered. |
| Unparseable message | `Rejected`: drop it. The next snapshot (≤ 100 ms later) is complete. | **Resync**. |
| Delta removes a level we don't have | n/a | **Resync**: our book has drifted from theirs. |
| Book crossed after applying | n/a (`apply_to` rejects it) | **Resync**. |
| Delta before the first snapshot | n/a | `Ignored`: it comes from the old subscription during a resync. |

## 3. Backoff: why exponential, why jitter, and the seeding bug

`next_delay()` returns a random delay in `[cap/2, cap]`, where `cap = min(30 s, 250 ms × 2^attempt)`:

```rust
let cap = self.cfg.base.saturating_mul(1 << self.attempt.min(20)).min(self.cfg.max);
// xorshift64 …
half + Duration::from_nanos(self.rng % (extra + 1))
```

- **Exponential** so a venue that is down (or rate-limiting us) isn't hammered.
- **Jitter** because after a venue-wide outage every client reconnects at once. Without
  jitter they retry in lock-step and knock the venue over again (a "thundering herd").
  "Equal jitter" keeps half the delay fixed, so a retry is never instant by chance.
- `attempt.min(20)` keeps the shift in range, and `saturating_mul` prevents overflow.

**The bug the tests caught.** My first version seeded with `seed | 1`, because xorshift
must never have state 0. That maps seeds 2k and 2k+1 to the same state, and small
neighbouring seeds produced correlated sequences. `different_seeds_spread_out` failed.
The fix scrambles the seed with **SplitMix64** first, the standard way to seed a small
PRNG: neighbouring seeds get unrelated states, and 0 is handled explicitly.

## 4. Two subtle bugs, found by writing the tests first

1. **Idempotent disconnect vs repeated failure.** When the feed asks for a reconnect and
   the socket also errors, the same loss gets reported twice. Advancing backoff twice
   would be wrong, so `on_disconnected` does nothing if already `Disconnected`. But my
   first draft also routed **failed connect attempts** through it, and those happen
   while already `Disconnected`. So a venue that was down got retried every 250 ms
   forever, with no backoff. Fix: a separate `on_connect_failed` that always backs off.
   The test `a_loss_reported_twice_counts_once_but_failed_connects_keep_backing_off`
   covers both cases.
2. **"Messages still arriving" is not "healthy".** During a Bybit resync, deltas from the
   old subscription can keep arriving (we ignore them) while no snapshot ever comes. A
   silence-only check would call that feed alive forever. So `Syncing` has its own
   timeout (`SyncTimeout`), and `Subscribing` likewise (`SubscribeTimeout`), independent
   of message flow.

Two of my own test expectations were also wrong, and the code was right:
- `recv_ns` is the time the message was handled (60 ms), not one tick later.
- With complete silence after subscribing, `Stale` (5 s) fires before
  `SubscribeTimeout` (10 s). That is correct: silence is the more specific diagnosis. The
  test now checks both cases: replies arriving but no book gives `SubscribeTimeout`;
  silence gives `Stale`.

## 5. The driver: the only code that touches the socket

```rust
while !stop.load(Relaxed) {
    let now = mono_ns();
    match conn.as_mut() {
        None    => if feed.ready_to_connect(now) { connect → on_connected | on_connect_failed }
                   else { sleep(1 ms) }                  // nothing is time-critical when down
        Some(c) => match c.poll() {
            Ok(Some(Text(t))) => feed.on_text(now, t, &mut cmds),
            Ok(None)          => { feed.on_tick(now, &mut cmds); spin_loop() }
            Err(e)            => { conn = None; feed.on_disconnected(now, &mut cmds) }
            ..
        },
    }
    while let Some(cmd) = cmds.pop() { Send → send_text | Reconnect → close + on_disconnected | Log }
}
```

- **One thread per venue.** A Binance reconnect (TLS handshake, backoff) never delays
  Bybit's book.
- `on_tick` runs only when there is nothing to read, so timers cost nothing while
  messages are flowing. Under constant flow, staleness can't happen anyway.
- `mono_ns()` is one process-wide monotonic clock. Feeds stamp `recv_ns` with it, and the
  aggregator compares against it. Wall-clock time could jump (NTP) and fake staleness.

## 6. Tests (14 deterministic + 1 live)

Every deterministic test drives a `Feed` with a fake clock and **recorded real
messages** (step 6/7 fixtures). It then reads the published book back through a
`BookReader`, as the aggregator does.

| Test | What it proves |
|---|---|
| `binance_connects_subscribes_and_goes_live` | Connect → SUBSCRIBE sent → Live on the first book; `recv_ns`, best bid and counters are exact. |
| `binance_gap_is_counted_but_the_next_snapshot_repairs_it` | A skipped frame is logged as a gap, with **no** resync (no sends). |
| `binance_bad_message_is_dropped…` | Garbage → `Rejected`; the book stays Live and valid. |
| `binance_needs_no_heartbeat_but_goes_stale` | No pings are ever sent; silence → `Stale` at 5 s + ε. |
| `bybit_snapshot_then_deltas` | Snapshot + deltas give the exact best bid. |
| `bybit_gap_resyncs…` | Gap → `Syncing` → the **aggregator excludes the venue** → fresh snapshot → included again. |
| `bybit_unparseable_or_crossed_data_resyncs` | Both triggers resync. |
| `bybit_heartbeat_every_20s_and_reconnect_if_unanswered` | Pings at 20 s and 40 s; an answered ping clears the deadline; an unanswered one → `PongTimeout` at 51 s. |
| `a_refused_subscription_reconnects` | `{"success":false}` → reconnect. |
| `too_many_resyncs_escalate_to_a_reconnect` | The 6th resync within 60 s → reconnect. |
| `no_book_after_subscribing_times_out` | Replies but no book → `SubscribeTimeout`; silence → `Stale`. |
| `a_resync_that_never_gets_a_snapshot_times_out` | Old deltas keep coming; still `SyncTimeout`. |
| `disconnects_back_off_exponentially_and_reset_once_healthy` | Delays grow; 30 s Live resets them. |
| `a_loss_reported_twice_counts_once_but_failed_connects_keep_backing_off` | Section 4, bug 1. |

### Live: both venues, the real driver, and the aggregator (20 s)

```text
[binance] Connected   [binance] Live   [bybit] Connected   [bybit] Live
t= 2s binance=Included bybit=Included | best bid 2.691 58189.4 @binance | best ask 2.315 58188.8 @bybit | buy 1 BTC: vwap 58189.5 …
t=14s … | buy 1 BTC: vwap 58144.6 incl. fees 58175.5 via [("binance", "0.355"), ("bybit", "0.645")] | arb: None
binance FeedStats { messages: 186, applied: 185, gaps: 0, rejected: 0, resyncs: 0, reconnects: 0, disconnects: 0 }
bybit   FeedStats { messages: 919, applied: 918, gaps: 0, rejected: 0, resyncs: 0, reconnects: 0, disconnects: 0 }
```

- The whole pipeline now works end to end: socket → TLS → WebSocket → parser → book →
  seqlock → aggregator → routing.
- At t=14s the router **split a 1 BTC order across both venues**, because Bybit's best
  ask was cheaper but only 0.001 BTC deep.
- Message counts match the streams: Binance about 9/s (100 ms snapshots), Bybit about
  46/s (20 ms deltas). The one non-book message on each is the subscribe acknowledgement.
- **The best bid on one venue is often above the best ask on the other** (for example,
  t=2s: 58189.4 bid on Binance vs 58188.8 ask on Bybit). That is a real cross-venue
  cross, but `arb: None`: it's worth 0.6 USDT, while fees on 1 BTC are about 60 USDT.
  This is exactly why step 10 made arbitrage fee-aware.

## 7. What this step deliberately doesn't do

- **Real network failures in tests** (abrupt close, a server that stops responding,
  malformed frames over a real socket). That's step 14: a mock exchange server.
- **Binance's 24 h forced disconnect.** That's handled generically: the connection
  drops → `on_disconnected` → reconnect after a 250 ms backoff (backoff resets after
  30 s healthy). A planned "make before break" (open the new connection before the old
  one closes) is noted for CONSIDERATIONS.md.
- **Logging on the hot path** still uses a callback. Step 15 replaces it with a binary
  SPSC logger so formatting never happens on the feed thread.

## 8. Java counterpart

The skeleton's whole lifecycle handling:

```java
@Override public void onClose(int code, String reason, boolean remote) {
    log.info("Bybit connection closed: {}", reason);   // and that's it: no reconnect
}
@Override public void onError(Exception ex) { log.error("Bybit error", ex); }
```

- **No reconnect, no heartbeat, no staleness, no resync.** Bybit drops the connection
  after missing pings (no pings are sent), and the book then stays frozen at its last
  state. Nothing marks it stale, so the aggregator would keep quoting dead prices.
- **A typical Java fix** would be a `ScheduledExecutorService` for pings and timeouts,
  `reconnect()` from `onClose` (Java-WebSocket has one), and a `volatile` status flag.
  That works, but timers on another thread race with `onMessage`. For example, a
  timeout fires just as the snapshot arrives. You then need locks, and the timing logic
  can only be tested with real sleeps.
- **Our sans-I/O design avoids both problems:** one thread owns all of a venue's state,
  so there are no races, and time is an argument, so tests are deterministic. The same
  pattern works in Java: a plain class with `onText(long nowNs, …)` that returns
  commands. Protocol libraries such as quinn-proto (QUIC) and h11 (HTTP/1.1) are built
  exactly this way ("sans-I/O").

## How to verify it yourself

```bash
cd rust
cargo test -p ob-feed --release                                          # 3 unit + 14 state-machine tests
cargo test -p ob-feed --release --test live -- --ignored --nocapture     # both venues live, 20 s
cargo clippy --workspace --all-targets -- -D warnings
```

Things to try (I ran the first two; the third is a manual check):
- In `feed.rs`, make `on_disconnected` call `self.go_down(now, cmds)` unconditionally
  (remove the `if !matches!(…Disconnected…)` guard).
  `a_loss_reported_twice_counts_once_but_failed_connects_keep_backing_off` fails: one
  loss would be counted twice and the backoff would jump a step.
- In `feed.rs`'s `on_text`, change `Outcome::Pong => self.pong_due_ns = None` to
  `Outcome::Pong => {}`. `bybit_heartbeat_every_20s_and_reconnect_if_unanswered` fails,
  because answered pings would still time out and the feed would reconnect every 30 s.
- During the live test, turn Wi-Fi off for ~10 s and back on. You'll see `Stale`,
  `Disconnected{retry_in}`, failed connects with growing delays, then `Connected` and
  `Live`.

## Likely interview questions

1. **How do you handle disconnections?**
   Two levels. If the book is wrong but the connection is fine, resync: unsubscribe,
   resubscribe, wait for a fresh snapshot (~100 ms). If the connection is unhealthy
   (silence, an unanswered ping, a refused subscription, repeated resyncs), reconnect
   with exponential backoff and jitter. In both cases the venue is marked `Syncing` or
   `Down` in the same seqlock write, so the aggregator drops it immediately.
2. **Why not just rely on TCP to tell you the connection is dead?**
   A half-open TCP connection can look healthy for minutes (keepalive defaults to hours).
   An application-level check, no message for 5 s on a stream that ticks every 20–100 ms,
   catches it in seconds. Bybit's ping/pong covers quiet periods.
3. **Why jitter in the backoff?**
   After a venue-wide outage, every client reconnects at once. Without jitter they stay
   synchronised and overload the venue on every retry. Equal jitter spreads them out but
   keeps a minimum delay.
4. **Why is the state machine sans-I/O?**
   Testability and the absence of races. Time is an argument, so a 51-second heartbeat
   scenario runs in microseconds and deterministically. One thread owns all of a venue's
   state, so there are no timer threads racing with message handling.
5. **Why does a Binance gap not trigger a resync but a Bybit gap does?**
   Binance `depth20` messages are full snapshots, so the next one repairs the book.
   Bybit sends deltas, so a lost delta leaves the book permanently wrong until a new
   snapshot arrives.
6. **What happens if the book goes stale but the connection is up?**
   Two independent guards. The aggregator excludes a venue whose `recv_ns` is too old at
   query time (step 9), and the feed reconnects after 5 s of silence. Readers are
   protected even if the feed thread itself is stuck.
7. **What bugs did your tests find?**
   (a) Repeated connect failures skipped backoff, because they went through the
   idempotent disconnect path. (b) Seeding xorshift with `seed | 1` merged neighbouring
   seeds; SplitMix64 fixes it. Both were caught by tests written before trusting the code.
8. **How would you avoid the gap during Binance's 24-hour forced disconnect?**
   Make before break. Open a second connection a few minutes before the 24 h mark, wait
   until it is Live, then switch the publisher over and close the old one. The book is
   never `Down`.
