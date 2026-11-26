# Step 7: Bybit order book parser (snapshots, deltas, sequencing)

Design reference: Phase 2 of the plan ("Bybit: snapshot/delta, `u` rules"). Bybit is
harder than Binance. It sends a snapshot and then only the **changes**, so one missed
or misapplied message leaves the book wrong until the next snapshot. This step parses
both kinds, decides **whether** to apply a message (sequencing), and applies it.

## What changed

| File | Content |
|---|---|
| `ob-parse/src/bybit.rs` | `BybitBookParser`, `BookMsg`, `Kind`, `Sequence`, `Applied`, `Message`/`Op`, errors with policy class |
| `ob-parse/src/json.rs` | `Cursor::boolean` (for `"success":true`), plus a test |
| `ob-parse/tests/bybit.rs` | 18 tests: real frames, a model replay, sequencing, application, every message rule |
| `ob-parse/tests/support/bybit_oracle.rs` | Independent serde_json implementation of the Bybit rules |
| `ob-parse/tests/bybit_differential.rs` | 5 differential tests, including the generator-quality guard |
| `ob-parse/tests/support/{comparable,mutate}.rs` | Shared by both venues' differential tests (refactored out of step 6) |
| `ob-parse/fuzz/fuzz_targets/bybit.rs` | Fuzz target against the oracle; seeding script and CI updated |
| `ob-parse/benches/bybit.rs` | Criterion: snapshot, deltas, full hot path |

## 1. What the real data showed (60 recorded frames)
```json
{"topic":"orderbook.50.BTCUSDT","type":"snapshot","ts":1795610797394,
 "data":{"s":"BTCUSDT","b":[["58656.20","0.835"], ...],"a":[...],"u":22042290,"seq":855165474148},
 "cts":1795610797371}
{"topic":"orderbook.50.BTCUSDT","type":"delta","ts":1795610797394,
 "data":{"s":"BTCUSDT","b":[],"a":[["58656.30","6.140"]],"u":22042291,"seq":855165474295},"cts":1795610797392}
{"success":true,"ret_msg":"","conn_id":"…","req_id":"depth","op":"subscribe"}
```

| Finding | Consequence |
|---|---|
| 1 snapshot (50 + 50 levels), then 58 deltas | Two message kinds with different semantics |
| The **subscription reply came third**, after the first snapshot and delta | Can't wait for "subscribed" before processing data; order isn't guaranteed |
| **`u` rose by exactly 1** in all 58 pairs; `seq` always rose | Any other step in `u` is a gap |
| Deltas are small: mostly 1–4 levels, 247 bytes on average; every 0–120 ms | Bybit is far more frequent and finer-grained than Binance's 100 ms snapshots |
| Deletions are quantity `"0"` (59 of them) | Must remove the level |
| Replaying deltas **never deleted a missing level**; the book stayed at **exactly 50 per side** | Bybit itself deletes levels that leave the top 50, so a "delete of a missing level" means we've drifted |
| `ts − cts` = 2–23 ms | Bybit's own publishing delay, before the network |
| Prices have 2 decimals (tick 0.1), quantities 3 | Same scale handling as Binance |

## 2. Semantics: snapshot vs delta, and when to apply

```rust
pub enum Kind { Snapshot, Delta }

impl BookMsg {
    pub const fn is_reset(&self) -> bool {
        matches!(self.kind, Kind::Snapshot) || self.update_id == 1   // u == 1: service restart
    }
    pub const fn sequence(&self, last_update_id: Option<u64>) -> Sequence { ... }
    pub fn apply_to(&self, book: &mut OrderBook) -> Result<Applied, SnapshotError> { ... }
}
```

**Sequencing:** `sequence(last)` classifies each message *before* it touches the book.

| Case | Result | What the feed does (step 13) |
|---|---|---|
| Snapshot, or delta with `u == 1` | `Reset` | Replace the book |
| Delta with `u == last + 1` | `Next` | Apply it |
| Delta with `u > last + 1` | `Gap { expected }` | The book is wrong: drop it from aggregation and resubscribe for a snapshot |
| Delta with `u <= last` | `Stale` | Duplicate or replay: ignore |
| Delta before any snapshot | `NoBaseline` | Nothing to apply it to: wait for the snapshot |

**`u == 1` means restart.** Bybit documents that a delta with `u == 1` follows a
service restart and is really a full snapshot, so it must replace the book. One
consequence I hit in my own test: such a message can't contain a deletion. When my
synthetic restart message included `"0"`, `apply_to` refused it with `ZeroQty`. That's
correct, because a snapshot with a deletion in it is invalid data.

**Applying:**
- A reset replaces the book with the step 3 snapshot path: validated, all-or-nothing.
- A delta updates level by level with `book.apply`: `"0"` removes the level, a new price
  inserts, an existing price is overwritten.
- `Applied { removed_absent, out_of_depth }` counts suspicious events. In the healthy
  recording both are 0. A delete of a level we don't have means our copy has drifted
  from Bybit's.
- After a delta, the caller checks `book.is_crossed()`: a crossed single-venue book is
  also proof of drift.

**Why the parser doesn't decide what to do:** parsing, sequencing and applying are
separate functions. The feed (step 13) owns the state (`last_update_id`, venue health)
and the policy. Each piece can be tested on its own, and the parser stays a pure
function of its input.

## 3. The Java skeleton's bug for Bybit, measured
```java
if (root.has("data")) {
    data.get("b").forEach(bid -> handler.handleOrder(...));   // "type" is never read
```
The skeleton never reads `type`, so it can't tell a snapshot from a delta. Every message
is applied as updates. The first snapshot happens to work (on an empty book), but Bybit
sends a **new** snapshot after a reconnect, a resubscribe, or a service restart. Applied
as updates, it leaves every level that has since disappeared.

`ignoring_the_snapshot_type_leaves_stale_levels` replays the recording, then a second
snapshot 600 ticks ($60) lower, as after a reconnect in a falling market:
- **Correct:** the book equals the new snapshot and isn't crossed.
- **Java-style:** the old, higher bids remain, so the book is **crossed** and the best
  bid is wrong.

The skeleton also doesn't check `u`, so a dropped message (a gap) silently corrupts the
book until the next snapshot, which may never come.

## 4. Message rules
These are the same principles as Binance (step 6), written in `bybit.rs`'s module docs
and implemented identically by the parser and the oracle:
- **Unknown fields** are skipped but validated, at the top level and inside `data`.
- **Known fields** (top level: `topic type ts cts data success ret_msg conn_id req_id op`;
  in `data`: `s b a u seq`) may appear at most once and are type-checked wherever they
  appear.
- **With `topic`:** a book message. `topic` must match the subscription, `type` must be
  `snapshot`/`delta`, `data.s` must be the symbol, and every field we use must be present.
- **Without `topic` but with `op`:** a response. `success` is required. `op` is
  `subscribe`, `unsubscribe`, `ping`/`pong` (heartbeat replies) or other.

**`cts` is optional on purpose.** It's only used for latency metrics (matching engine →
Bybit's publisher). Rejecting book data because a metrics field was missing would trade
correctness for observability, the wrong way round. Every field the book depends on is
required.

**Nested `data` object:** the parser reads it in place, in the same single pass. Top-level
and `data` fields share one 32-bit "seen" mask, so duplicates are caught at both levels
with no extra state.

## 5. Testing

**Real frames and rules** (`tests/bybit.rs`, 18 tests):
- every recorded frame parses, with exact values;
- `u` is strictly sequential;
- **replaying the recording matches an independent `BTreeMap` model of Bybit's rules
  after every message:** 50 + 50 levels throughout, never crossed, all 59 deletions hit
  existing levels;
- every truncation of the snapshot (each of 2,256 prefixes) is an error, never a panic;
- sequence classification for each case, the `u == 1` restart (valid and invalid),
  delete-of-missing reporting, and a snapshot with a deletion refused;
- the message rules one by one: order, whitespace and unknown fields at both levels,
  optional `cts`, missing and duplicate fields at both levels, wrong topic/type/symbol,
  number classes, level shape, too many levels, responses including failure and pong,
  and buffer reuse (no levels or `cts` left over from the previous message).

**Differential testing against the serde_json oracle** (`tests/bybit_differential.rs`,
20,000 cases per generator):
- mutated real frames;
- generated messages: a complete message with **both the top level and `data` shuffled**,
  sometimes a field swapped for a random variant, unknown fields, responses with and
  without `success`, and random field sets;
- arbitrary bytes.

The generator-quality guard from step 6 is built in: **41% / 41% accepted.**

**A planted bug is caught.** I made the topic check always true. Both generators found
messages that should have been rejected (a mutated frame with topic
`orderbook.00.BTCUSDT`, and a generated one with topic `snapshot`). My first attempt at
the mutation didn't compile, so it proved nothing; I redid it properly. The code was
restored afterwards.

**Fuzzing** (libFuzzer, AddressSanitizer, against the same oracle):

| Target | Runs | Time | Result |
|---|---|---|---|
| `bybit` | **23.6 million** | 5 min (78k/s) | no crash, no disagreement; 2,479 code edges |
| `binance` (re-checked after the refactor) | 2.9 million | 30 s | clean |

**A mistake of mine the build caught:** I'd put the dependency-free `comparable()` rule
in the same support module as the proptest-based mutations. The fuzz crate doesn't
depend on proptest, so the Bybit fuzz target **failed to build, and my first "5-minute
fuzz run" silently did nothing** (the output was empty). I split the module in two
(`comparable.rs` with no dependencies, `mutate.rs` for tests), checked that all three
targets build, then ran the fuzzing for real. Lesson: an empty result isn't a pass.

**Test mistakes found by the tests themselves:** three expected values in my first draft
of `tests/bybit.rs` were wrong: a `cts` and a `seq` copied from the wrong message, and a
"later snapshot" that kept the old `update_id`, so the replay never treated it as a
delta. In each case the parser was right and the test was wrong. I corrected the tests
using values read from the fixture by script, not from memory.

## 6. Performance

Criterion on the real recording, i7-8569U, two runs:

| | serde typed (`&str`, best idiomatic) | **ob-parse** | Speed-up |
|---|---|---|---|
| Snapshot, 2,256 bytes, 50 + 50 levels | 9.7–9.9 µs | **3.5 µs** | 2.7× |
| Delta, 247 bytes on average | 1.24 µs | **0.40–0.43 µs** | 3× |

**Full hot path** (parse + sequence check + apply to book + crossed check) over the whole
recording (1 snapshot + 58 deltas): **0.54–0.63 µs per message** on average.

**Against the plan's target (0.2–0.5 µs per message):**
- **Bybit deltas, which are most of Bybit's traffic, are now within it:** about 0.4 µs to
  parse, about 0.5 µs including sequencing and applying.
- Snapshots (3.5 µs) are rare: one after subscribing, and after a reset.
- The cost per byte (~1.6 ns) is the same as Binance's. Step 20's profiling of the
  scanner benefits both venues.

## 7. Java counterpart
The skeleton's `BybitWebsocketClient.onMessage`:
```java
log.debug(message);                       // every raw message, and Logback defaults to DEBUG
JsonNode root = mapper.readTree(message); // full tree per message
if (root.has("data")) {                   // "type" never read: snapshot == delta
    data.get("b").forEach(bid -> handler.handleOrder(new PriceLevel(...)));
```
What's missing compared with this step:
- **No snapshot/delta distinction:** later snapshots leave stale levels (the crossed book
  in section 3).
- **No `u` sequencing:** gaps go undetected; `u == 1` restarts are applied as updates.
- **No `topic`/`s` check:** any data on the connection is applied to the BTCUSDT book.
- **Logs every raw message.** With no `logback.xml`, Logback's default level is DEBUG,
  so every frame is formatted and printed on the I/O thread.
- **Bybit's required 20 s ping isn't sent,** so the server drops the connection. That's
  handled in step 13 (`Op::Pong` is recognised here).

The low-latency Java version of this step: a streaming `JsonParser`, a per-venue state
machine holding `lastUpdateId`, `snapshot → replace`, `delta → apply`, and `gap → resync`.
That's the same structure as here, written with `long[]` books.

## How to verify it yourself
```bash
cd rust
cargo test -p ob-parse --test bybit                                         # 18 tests
cargo test -p ob-parse --release --test bybit_differential -- --nocapture   # prints acceptance rates
cargo bench -p ob-parse --bench bybit
scripts/seed_fuzz_corpus.sh
cd crates/ob-parse && cargo +nightly fuzz run bybit -- -max_total_time=300
```
Things to try:
- In `bybit.rs`, make `is_reset` return only `matches!(self.kind, Kind::Snapshot)`.
  Then `sequence_classification` and `a_restart_delta_replaces_the_book` fail: restarts
  would be treated as ordinary deltas.
- Change `Some(last) if self.update_id == last + 1 => Sequence::Next` to
  `self.update_id > last`. Then gaps are silently accepted, and
  `sequence_classification` fails.

## Likely interview questions
1. **How do you maintain a book from snapshots and deltas?**
   Replace on a snapshot; apply deltas in order (`0` deletes, otherwise set). Check the
   sequence number before applying; on a gap, discard the book and get a fresh snapshot.
   Check for a crossed book and deletes of missing levels as signs of drift.
2. **What does `u == 1` mean on Bybit?**
   The service restarted. The message is a full snapshot even though it's labelled
   `delta`, so it replaces the book, and it can't contain deletions.
3. **Why is a gap so serious on Bybit but not on Binance?**
   Binance sends full snapshots, so the next message repairs the book. On Bybit a missed
   delta is lost for good, and the book stays wrong until we resync.
4. **Why is `cts` optional but `u` required?**
   `u` decides correctness (sequencing). `cts` is a latency metric. Never reject book data
   over a metrics field.
5. **The subscription reply arrived after data. Does that matter?**
   Yes, for state machines. If the feed waited for "subscribed" before accepting data, it
   would drop the first snapshot. Our design handles messages in any order and keys on
   the content.
6. **How do you know the tests are correct, not just the code?**
   Tests are checked like code. Generators have a minimum acceptance rate; planted bugs
   must be caught; expected values come from the fixture by script, not from memory. And
   an empty fuzz run is investigated, not counted as a pass.
7. **Why are parsing, sequencing and applying separate?**
   Pure functions are easy to test and fuzz on their own. The stateful policy (what to do
   on a gap, venue health) lives in one place, the feed state machine (step 13).

## Correction (found in step 12 by the live test)

Section 1 above says Bybit deltas are "small: mostly 1–4 levels". That's true of the
60-message recording, but **live data showed deltas with up to 66–67 levels on one
side**, more than the stream's depth of 50. A delta lists *changes*: in a fast market a
single 20 ms delta can delete levels that drop out of the top 50 **and** insert their
replacements, up to 50 + 50 = 100 entries per side.

Two consequences, both fixed in step 12:
1. **The parser's per-message limit was 64 levels, so live messages were rejected**
   (`TooManyLevels`). `ob_parse::MAX_LEVELS` is now 128.
2. **A Bybit book needs capacity ≥ 2 × depth.** Applying a big delta in order, the book
   can briefly hold more than 50 levels. With capacity 64 the overflow drops the "worst"
   levels, some of which the same delta's later deletions would have brought back into
   the top 50, so real levels are silently lost. The test
   `a_big_delta_needs_capacity_for_twice_the_depth` shows capacity 64 ending with 44
   levels instead of 50, and capacity 128 ending exactly right. The Bybit feed uses
   `OrderBook<128>`.

Lesson: a 60-message recording isn't a specification. Live data is part of the test
suite (`ob-net/tests/live.rs`, run manually because it needs the internet).
