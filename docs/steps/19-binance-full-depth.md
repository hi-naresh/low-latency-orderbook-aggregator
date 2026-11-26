# Step 19: Binance full depth (diff stream + REST snapshot)

The `depth20` stream shows Binance's best 20 levels a side, about $2 of BTCUSDT's book.
That's enough for the best bid and ask, not for **pricing a large order**: with
`depth20`, a 25 BTC buy was regularly reported **partial** (only 10–15 BTC visible across
both venues).

This step adds an optional **full-depth mode**: Binance's diff stream applied to a REST
snapshot, the documented way to keep a complete local book. It keeps up to 1,024 levels
a side, every update sequence-checked, and publishes 256 of them.

```bash
cargo run --release -p ob-app -- live --binance-full-depth
```

## What changed

| Area | File | What |
|---|---|---|
| HTTP | `ob-net/src/http.rs` | **New.** A minimal, bounded HTTPS GET: one deadline, a body limit, `Content-Length` and chunked bodies, no compression. `NetError::HttpStatus`. |
| Parsing | `ob-parse/src/binance.rs` | `parse_rest_snapshot`: levels go to a callback (the crate stays `no_std`). |
| | `ob-parse/src/levels.rs` | `MAX_LEVELS` 128 → **1024** (diff events carry up to 487 levels a side). |
| | `ob-parse/tests/binance_rest.rs` | Differential tests against serde_json, on a recorded live snapshot. |
| Feed | `ob-feed/src/full_depth.rs` | **New.** `BinanceFullDepth`: buffering, the sync rules, the known region. |
| | `ob-feed/src/{venue,feed,driver,record,log}.rs` | `Outcome::NeedSnapshot` → `Command::FetchSnapshot` → driver GET → `Feed::on_snapshot`; snapshots recorded for replay; new log events. |
| Publishing | `ob-sync/src/book.rs` | Published `DEPTH` 64 → **256** (see section 5). |
| App | `ob-app` | `--binance-full-depth`; 25 BTC execution lines; replay detects full depth by itself. |
| Tests | `ob-feed/tests/full_depth.rs` | Every sync rule on synthetic messages. |
| | `ob-feed/tests/mock_exchange.rs` | +2: syncing over real WebSocket + REST, and a 429 → reconnect. |
| | `ob-feed/tests/live_full_depth.rs` | Live cross-check against Binance's own `depth20`. |
| | `ob-net/tests/http.rs`, `zero_alloc.rs` | The HTTP client; zero allocations once synced. |

## 1. Checking the facts first

- **Sync rules:** read from Binance's current documentation ("How to manage a local order
  book correctly", last modified the day before this step). The rules are quoted in
  `full_depth.rs`.
- **REST snapshot:** fetched live. 42 KB, 1,000 levels a side, `Content-Length` (not
  chunked, not compressed), request weight 20 as documented.
- **Diff stream:** recorded live for 15 s, because its documentation page didn't load.
  Two findings shaped the design:
  1. **Diff events have exactly the same shape as `depth20` events** (`"e":"depthUpdate"`,
     the same fields). On one plain connection they can't be told apart. So full-depth
     mode subscribes to the diff stream *instead of* `depth20`; `bookTicker` (step 18)
     still works alongside it.
  2. **They report changes anywhere in the book:** up to 487 levels on one side of one
     event, including prices like 6,020 with BTC at 60,000. That forced the
     parser-buffer increase, and the "known region" (section 3).

## 2. The sync rules, sans-I/O

```text
diff event ──▶ venue: Idle?  buffer it, Outcome::NeedSnapshot
                  feed: log FetchingSnapshot, Command::FetchSnapshot (once)
                  driver: blocking GET /fapi/v1/depth?limit=1000 ──▶ feed.on_snapshot(body)
                  venue: load the book (L = lastUpdateId), replay the buffer, then live:
                     u < L                       → drop                  (rule 3)
                     first event: needs U ≤ L ≤ u, else gap → resync   (rule 4)
                     afterwards: pu == previous u, else gap → resync   (rule 5)
                     qty 0 removes; removing an absent level is normal  (rules 7–9)
```

- **The state machine still does no I/O.** The venue says it needs a snapshot, the feed
  issues a command, and the driver performs the GET. The tests drive the same code with
  synthetic snapshots and no network.
- **Events arriving during the GET wait in the socket's kernel buffer,** in order. The
  venue keeps an application buffer only for the event(s) that arrived before it asked,
  bounded at 600.
- **A failed fetch** (timeout, HTTP 429) leads to a reconnect with backoff, which also
  respects Binance's REST rate limit.
- **Snapshots are recorded** (record kind 2). A replay rebuilds books exactly as live,
  including syncs, and detects full-depth mode by itself.
- **Unlike Bybit**, a removal of an absent level is *not* a resync. Binance documents it
  as normal: its diffs cover the whole book, ours covers part of it.

## 3. What we know exactly: a region, not the whole book

The snapshot gives the best 1,000 levels; diff events also report levels far beyond them.
Inserting those would put levels next to gaps we know nothing about. So each side has a
**boundary**:

- bids at or above the boundary (asks at or below) are known exactly, and updates beyond
  it are ignored;
- it starts at the snapshot's worst level (none if the snapshot held the whole side);
- it moves inwards whenever the 1,024-level book is full and must drop its worst level,
  or refuses a level worse than all it holds;
- if a side ends up knowing fewer than 256 levels (the market moved far), the venue
  resyncs with a fresh snapshot (`ResyncReason::Depleted`).

Live, the book held about 900–1,000 levels a side, covering about $260 each way.

## 4. Verification

### Unit tests and mutation testing

There are 9 synthetic tests, one or more per rule. Six mutations, each **caught by the
test aimed at it**:

| Mutation | Caught by |
|---|---|
| No straddle check (rule 4) | `a_snapshot_older_than_every_event_is_a_gap` |
| No continuity check (rule 5) | `sync_buffers_drops_stale_requires_a_straddle_then_continuity` |
| Stale events not dropped (rule 3) | both of the above |
| Boundary ignored | `a_truncated_snapshot_bounds_the_known_region` |
| Boundary not moved when capacity drops a level | `a_full_book_moves_its_boundary_when_it_drops_or_refuses_a_level` |
| No depletion check | `a_side_that_runs_short_of_known_levels_asks_for_a_new_snapshot` |

All 9 passed on the first run, so I tightened the capacity test from a loose bound to the
exact expected boundary (0.7) before mutating.

### Over real connections (mock exchange)

- A WebSocket server sends diff events and a separate HTTPS server answers the GET. The
  real driver syncs: it buffers an event, fetches, stitches it on, applies a later event,
  and publishes the exact book.
- A 429 response: the failure is logged, the feed reconnects, and the second attempt
  succeeds.

### Live: our book against Binance's own

Comparing our top 20 with Binance's `depth20` at the same update ID turned out to be
almost never possible. Diffs and `depth20` are cut about **1 ms apart**: each `depth20`
`u` falls *inside* the next diff event's range (`U ≤ depth20.u < u`). Exact matches:
5 in 30 s on two connections, 3 on one; **all matched**.

So the check uses every snapshot instead (`ob-feed/tests/live_full_depth.rs`, one
connection via the combined-stream endpoint):

- every price in Binance's top 20 that diff event *k* **didn't touch** must have exactly
  our quantity before *k*;
- a touched price must equal its value before or after *k*.

| 30 s live, 279 `depth20` snapshots | Count |
|---|---|
| Untouched levels exactly equal | **9,291** |
| Touched levels at their before/after value | 2,038 |
| Intermediate (changed twice within a window) | 340 |
| Transient mismatches | 1 |
| **Persistent mismatches** | **0** |

**The one mismatch taught something about the stream.** Diff events report each level's
**net** change over the 100 ms window. A level that appears and vanishes within one window
is never reported (absent → absent), but `depth20`, cut mid-window, can catch it alive.
That's a *transient* mismatch, gone by the next snapshot. A real error in our book would
*persist*, so the test fails only on a mismatch at the same price in consecutive
snapshots. The first version of the test counted it as an error; it had to learn this
distinction.

## 5. Publishing 256 levels: what the first live run showed

The first full-depth live run still printed **PARTIAL for 24 of 60 buy-25-BTC lines**:
only ~11 BTC on Binance. The book had ~1,000 levels, but only the top **64** were
published. On a thin ask side, 64 ticks of 0.1 is about $6 of book.

- **The fix:** published `DEPTH` 64 → 256.
- **The cost, checked before changing it:** readers copy only the levels a book has
  (`read_into` loops to `n_bids`/`n_asks`), and writers write only those. So the 20- and
  50-level venues pay nothing; the price is 8 KB of seqlock memory per venue.
- **The result:** 1 partial in 40 lines (at t = 1 s, before Binance had synced), against
  26 in 120 before. With `depth20`, partial buys were routine.

```text
buy  25 BTC: avg 60273.8, incl. fees 60304.7  via binance 18.773 + bybit 6.227
sell 25 BTC: avg 60265,   incl. fees 60234.4  via binance 21.273 + bybit 3.727
```

## 6. Measured cost

| | `depth20` (step 16) | Full depth |
|---|---|---|
| Apply one depth message (live, p50 / p90 / p99 / max) | ~1.5 µs (warm) | **6 / 21 / 51 / 66 µs** |
| Sync (connect → Live) | ~0.3 s | ~0.6 s (one REST round trip, ~0.3 s) |
| Allocations once synced | 0 | **0** (proven: 200 events × 50 levels) |
| Live 60 s: gaps / resyncs / reconnects | 0 / 0 / 0 | 0 / 0 / 0 |

The extra cost comes from the array book. Inserting or deleting deep in a ~1,000-level
side shifts up to ~16 KB of memory, and a diff event touches ~75 levels (p50) all over the
book. For 20–50 levels the array beat every alternative (step 4); for 1,000 levels **a
price-indexed ladder** (direct indexing by tick: O(1) per update, no shifting) is the
classic structure. That's a step 20 candidate. It only matters here because ~10 diff
events a second each spend up to tens of µs.

## 7. What this step deliberately doesn't do

- **Full depth for Bybit.** Its deepest public stream is `orderbook.1000` (1,000 levels,
  snapshot + delta). The same adapter as `orderbook.50` with a bigger capacity would do;
  the 50 levels already reach further than Binance's 20.
- **Making full depth the default.** It costs 4× more per update and a REST dependency,
  for value only on large orders. Opt-in fits.
- **Refreshing the snapshot periodically** "just in case": unnecessary while every event
  is sequence-checked.

## 8. Java counterpart

The skeleton has nothing like this; it uses `depth20` only.

A Java version would fetch the snapshot with `java.net.http.HttpClient`, parse it with
Jackson into a `TreeMap<BigDecimal, BigDecimal>` per side, and apply events under a lock.
That's the textbook approach, with the costs measured throughout this project:

- tree nodes and `BigDecimal`s allocated per level;
- O(log n) pointer-chasing per update;
- GC pauses under the diff stream's bursts.

The sync rules themselves are identical, and so are the classic mistakes:
- applying events before the snapshot, or dropping the buffered ones;
- treating a removal of an absent level as an error;
- inserting levels outside the snapshot's range;
- comparing `depth20` with full depth by update ID and concluding the book is wrong.

## How to verify it yourself

```bash
cd rust
cargo test -p ob-feed --release --test full_depth                 # the sync rules
cargo test -p ob-feed --release --test mock_exchange full_depth   # over real sockets
cargo test -p ob-net  --release --test http                       # the HTTPS GET
cargo test -p ob-feed --release --test live_full_depth -- --ignored --nocapture   # vs Binance, 30 s
cargo run  --release -p ob-app -- live --secs 30 --binance-full-depth             # watch the 25 BTC lines
cargo run  --release -p ob-app -- live --secs 30                                  # … and with depth20
```

Things to try (the first two are in the mutation table; the third is what the first live
run in section 5 showed, before the change):
- In `full_depth.rs`, change `None if first > self.snapshot_id =>` to `None if false =>`.
  `a_snapshot_older_than_every_event_is_a_gap` fails.
- Replace `if bound.is_some_and(|b| outside(l.px, b)) {` with `if false {`.
  `a_truncated_snapshot_bounds_the_known_region` fails.
- Set `DEPTH` back to 64 in `ob-sync/src/book.rs` and run live with
  `--binance-full-depth`: PARTIAL 25 BTC lines return.

## Likely interview questions

1. **How do you build a correct local order book from Binance's diff stream?**
   Buffer events, fetch the REST snapshot (`lastUpdateId` = L), drop events with u < L,
   require the first applied event to straddle L, then require `pu` = previous `u`;
   otherwise resync. Quantities are absolute; zero removes. And removing an absent level
   is normal here.
2. **The diff stream reports levels far outside your snapshot. What do you do with
   them?**
   Ignore them. Each side has a boundary of exact knowledge (the snapshot's worst level,
   tightened whenever capacity forces a level out). A level beyond it would sit next to
   gaps; showing it would misstate the depth. If too few known levels remain, re-snapshot.
3. **How did you prove the book is right?**
   Live, against Binance's own `depth20` on the same connection. The two are cut ~1 ms
   apart, so exact update-ID matches are rare (all matched). Instead, every untouched
   price must equal our pre-event quantity: 9,291 levels matched, 0 persistent
   mismatches. The 1 transient mismatch is a level that appeared and vanished inside one
   window, which net-change diffs don't report.
4. **Why did your 25 BTC order still look partial with a 1,000-level book?**
   Only 64 levels were published to readers. The fix (256) cost nothing per read for the
   other venues, because readers copy only the levels present. I checked that before
   making the change.
5. **What does full depth cost, and how would you make it cheaper?**
   6 µs p50 per diff event against ~1.5 µs, because deep inserts shift array memory in a
   ~1,000-level book. A price-indexed ladder makes updates O(1); that's the right
   structure at this depth, whereas the array wins at 20–50 levels.
6. **How does your sans-I/O design handle a REST call in the middle of a WebSocket
   feed?**
   The venue returns `NeedSnapshot`, the feed emits `FetchSnapshot` once, and the driver
   does the blocking GET and calls `on_snapshot`. Messages that arrive meanwhile wait in
   the kernel buffer. Failures lead to a reconnect with backoff. All of it is testable
   with no network, and replayable because snapshots are recorded.
