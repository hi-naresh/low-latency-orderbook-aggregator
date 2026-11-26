# Step 6: Binance depth message parser

Design reference: Phase 2 of the plan ("Fixed-schema pull scanners", "Fuzzing",
"Fixtures"). This step turns a raw Binance WebSocket frame into validated book levels,
checked against **real recorded data**, an **independent oracle**, and a **coverage-guided
fuzzer**.

## What changed

| File | Content |
|---|---|
| `rust/scripts/record_fixtures.py` | Records real frames from Binance and Bybit (same subscriptions as the Java skeleton) |
| `ob-parse/tests/fixtures/*.jsonl` | 60 real Binance frames + 60 real Bybit frames (Bybit is used in step 7) |
| `ob-parse/src/json.rs` | Minimal JSON scanner: reads what's needed, validates and skips the rest; never panics; 5 tests |
| `ob-parse/src/levels.rs` | `LevelBuf`: up to 64 levels in a fixed array, reused per message |
| `ob-parse/src/binance.rs` | `BinanceDepthParser`, `DepthUpdate`, `Message`, errors with policy class |
| `ob-parse/tests/binance.rs` | 16 tests: every real frame, then every message rule |
| `ob-parse/tests/support/binance_oracle.rs` | Independent implementation of the same rules on serde_json |
| `ob-parse/tests/binance_differential.rs` | 5 differential tests (mutated real frames, generated messages, random bytes) |
| `ob-parse/fuzz/` | cargo-fuzz targets `binance` and `decimal` (nightly) |
| `ob-parse/benches/binance.rs` | Criterion: this parser vs two serde_json approaches |
| `rust/scripts/seed_fuzz_corpus.sh`, `.github/workflows/ci.yml` | Fuzz corpus seeding; a 60-second fuzz run per target on every push |

## 1. What the real data showed

I recorded 60 frames from `wss://fstream.binance.com/ws` (`btcusdt@depth20@100ms`):
```json
{"e":"depthUpdate","E":1795610780582,"T":1795610780581,"s":"BTCUSDT","ps":"BTCUSDT",
 "U":12235913555065,"u":12235913562909,"pu":12235913554835,
 "b":[["58631.90","6.774"],["58631.80","0.002"], ...18 more],
 "a":[["58632.00","3.242"], ...19 more],"st":1}
```

| Finding | Consequence |
|---|---|
| Two **undocumented fields**, `"ps":"BTCUSDT"` and `"st":1` | The parser must skip unknown fields, and validate them as JSON |
| Prices have **2 decimals** (`"58631.90"`) but the tick is 0.1 | The trailing `0` must be checked and dropped (step 5's rule) |
| First frame is `{"result":null,"id":1}` | Subscription replies arrive on the same stream and must be recognised |
| Always exactly 20 + 20 levels, about every **100–120 ms** | Each message is a **full snapshot** |
| `pu` equals the previous message's `u` in **58 of 58** pairs | A missed message is detectable even on this snapshot stream |
| Bybit: the subscription reply arrived **after** the first snapshot and delta | Noted for step 7 |

## 2. Snapshot semantics, and the Java skeleton's bug, measured

Every Binance partial-depth message is the **complete** top 20. Levels that drop out are
simply absent next time; no "quantity 0" is sent for them. So a message must **replace**
the book:
```rust
impl DepthUpdate {
    pub fn apply_to(&self, book: &mut OrderBook) -> Result<(), SnapshotError> {
        book.replace(&self.bids, &self.asks)   // step 3: validates first, all-or-nothing
    }
}
```
The Java skeleton instead calls `handleOrder` for every level, treating the snapshot as a
list of *updates*. I replayed the 59 real snapshots both ways:

| After 6 seconds of real data | Replace (correct) | Apply as updates (Java skeleton) |
|---|---|---|
| Bid levels in the book | 20 | **109** |
| Best bid | 58,647.4 | **58,649.9 (stale)** |
| Best ask | 58,647.5 | **58,632.0 (stale)** |
| Crossed? | no | **yes: best bid above best ask** |

Within seconds the skeleton's design reports a crossed market with both best prices
wrong. The test `treating_snapshots_as_updates_leaves_stale_levels` reproduces this and
asserts it.

**Gap detection:** `DepthUpdate::follows(&previous)` checks `pu == previous.u`.
Step 13's feed will use it to notice a dropped message, and for this stream the cure is
simple: the next snapshot fixes the book.

## 3. Design

### The message rules (one specification, two implementations)
Written once in `binance.rs`'s module docs, and implemented by both the fast parser and
the oracle:
- **One JSON object.** Fields may come in any order. **Unknown fields are skipped**, but
  still validated as JSON.
- **Known fields** (`e E T s U u pu b a id result error`):
  - each may appear **at most once**. JSON permits duplicates, but parsers disagree about
    which value wins, which is dangerous for prices;
  - they're **type-checked wherever they appear**: `E T U u pu id` must be non-negative
    integers, `e s` strings, and `b a` arrays of `[price, quantity]` string pairs that
    are valid at the instrument's scale, tick and lot.
- **With `e`:** it's an event. It must be `"depthUpdate"`, for the configured symbol,
  with all of `E T U u pu b a` present → `Message::Depth`.
- **Without `e` but with `id`:** a reply to our request → `Message::Response { id, ok }`.
- **Anything else** → `UnknownMessage`.

### The JSON scanner (`json.rs`)
No JSON library and no tree. A cursor reads exactly what the venue parser asks for:
- **Pull style:** the parser asks "give me a key", "give me a u64", "give me a quoted
  decimal". The JSON is never materialised.
- **Skipping is validating:** `skip_value` checks the full RFC 8259 grammar (number
  syntax, escapes, literals, nesting). Garbage inside an unknown field still rejects the
  message, so the fast parser doesn't accept anything a real JSON parser would refuse.
- **No recursion:** nesting is tracked with a 128-bit stack in two `u64` words, with a
  depth limit of 128 (serde_json's default). A million `[`s is just an error, not a stack
  overflow (tested).
- **Never panics, never reads out of bounds:** every read is `buf.get(i)`. This is fuzzed
  (section 4).

### Fused decimal parsing
Step 5 ended with "fuse number parsing into the scan". `Cursor::decimal` converts digits
**while** looking for the closing quote: one pass, instead of "find the end of the
string, then parse it". Anything unusual (an escape, a minus sign, junk, more than 18
digits) leaves on a `#[cold]` path that finds the string's end properly and hands it to
the step 2 reference parser. Errors are therefore always exactly the reference's.

### Zero allocation: reused output
```rust
let mut out = DepthUpdate::default();            // once per feed
loop { parser.parse(frame, &mut out)?; ... }     // every message overwrites it
```
`LevelBuf` holds up to 64 levels in a fixed array. Clearing resets one length field, so
nothing is re-initialised per message. Creating a fresh 2 KB output for every message
would mean writing 2 KB every time. If a message is rejected, `out` holds partial data
and the contract says don't use it. The book is only touched on `Ok`, so step 2's "never
apply half a message" rule holds.

### Errors
```rust
pub struct Error { pub kind: ErrorKind, pub offset: usize }   // Copy, no heap
```
- `ErrorKind` says what went wrong: JSON/type errors (with the field when known),
  `DuplicateField`, `MissingField`, `WrongEvent`, `WrongSymbol`, `BadLevel`,
  `TooManyLevels`, or `UnknownMessage`.
- `offset` is the byte position, for logs.
- `error.class()` maps the error onto step 2's policy:
  - off-tick or extra precision → `SpecMismatch` (reload instrument info);
  - negative quantity → `Invalid`;
  - everything else → `Malformed`.

## 4. How it's tested

**1. Real frames** (`tests/binance.rs`, 16 tests):
- all 60 frames parse, with exact values checked;
- the `pu` chain is unbroken;
- replaying through the book reproduces each snapshot exactly and is never crossed;
- the Java-style replay is crossed (section 2);
- **every truncation of a real frame** (each of its ~1,000 prefixes) is an error, never
  a panic;
- the message rules one by one: field order, whitespace and unknown fields, missing and
  duplicate fields, wrong event or symbol, level shapes, error classes, too many levels,
  responses, wrong types, and buffer reuse.

**2. Differential testing against an oracle** (`tests/binance_differential.rs`). The
oracle re-implements the rules on serde_json, using step 2's reference decimal parser.
Over 20,000 cases per generator, the fast parser must reach the **same accept/reject
decision and the same output**:
- real frames with random **mutations**: digit edits, inserted valid fields, byte edits,
  deletions, duplications, inserted JSON fragments;
- **generated** messages: a complete valid message with shuffled field order, extra
  unknown fields, random whitespace, and sometimes one field swapped for a random variant
  (invalid number, wrong type, missing or duplicate key); plus fully random field sets;
- arbitrary bytes (no-panic check).

**The tests had a weakness, and a test caught it.** I added a check that each generator
produces plenty of *accepted* messages. A generator that only produces garbage "agrees"
with the oracle trivially. The first generators produced only **5–6% accepted** input,
so the accept path was barely tested. I made them structure-aware (digit-preserving
edits, valid field insertion, shuffling a complete message): now **46% and 53%**. The
check `generators_exercise_both_accept_and_reject` keeps it that way.

**Proof the oracle catches bugs:** I planted one by removing `pu` from the required
fields. Both generators independently found messages the fast parser wrongly accepted,
and proptest printed the failing input. Then I restored the code.

**3. Coverage-guided fuzzing** (`fuzz/`, libFuzzer via cargo-fuzz, nightly, with
AddressSanitizer). The same oracle checks every comparable input:

| Target | Runs | Time | Rate | Result |
|---|---|---|---|---|
| `binance` | **25.1 million** | 5 min | 83k/s | no crash, no disagreement; 2,067 code edges covered |
| `decimal` (all 3 fast parsers vs reference) | **36.6 million** | 2 min | 303k/s | no crash, no mismatch |

CI runs each target for 60 s on every push.

**Deliberate differences from serde_json** (excluded from comparison, documented in
`json.rs`):
- **Backslash escapes are validated but not decoded.** An escaped key such as `"E"`
  won't match `"E"`. Venues never escape keys or numbers.
- **Invalid UTF-8 inside strings isn't checked.** The TLS stream from the venue is
  trusted to be text, and checking would cost time on every message.
- **Nesting near the limit:** serde_json counts depth slightly differently.

In all three cases the input must still never cause a panic, and the fuzzer checks that.

**Bug found while building:** in the fused decimal path, `usize::MAX` was a sentinel
meaning "no decimal point", and `dot + 1` overflowed for numbers without one: a panic in
debug, a silent wraparound in release. A unit test caught it before any fixture did.

## 5. Performance

Criterion, the 59 real depth frames (~1,000 bytes each, 20 + 20 levels), i7-8569U, two
runs. Every variant produces the same validated `Px`/`Qty` levels:

| Approach | µs / message | vs ours | Allocates |
|---|---|---|---|
| serde_json → `Value` tree, then extract (the "usual" way, like Java's `JsonNode`) | 22.5–22.7 | 14× slower | yes, heavily |
| serde_json → `#[derive(Deserialize)]` struct borrowing `&str` (strongest idiomatic serde) | 4.74–4.78 | 3× slower | yes (`Vec` per side) |
| **`ob-parse`** | **1.59** | | **no** |
| `ob-parse` + `apply_to` (replace the book) | 1.67–1.71 | | no |

**Reading this honestly:**
- **3× faster than the best serde approach, 14× faster than the tree approach,** and
  allocation-free (steady latency, no allocator calls).
- **Replacing the book adds only ~0.1 µs.** Parsing dominates the hot path.
- **The plan's target was 0.2–0.5 µs per message; we're at 1.59 µs, about 3× off.**
  1.59 µs over ~1,000 bytes is ~1.6 ns per byte, or ~20 ns per level (two numbers plus
  punctuation). Likely costs:
  - two number parses per level (step 5 measured ~11 ns per number in isolation);
  - a whitespace-skipping `peek` before every `[`, `,` and `]`;
  - branch mispredictions on number lengths.

  Step 20 profiles this rather than guessing. Candidate fixes: a fast path for
  `["…","…"]` with no whitespace; SIMD to find the next `"`; and checking whether
  Binance's fixed price format (always 2 decimals) allows a specialised number path.

## 6. Java counterpart
What the skeleton does for every message:
```java
JsonNode root = mapper.readTree(message);               // builds a full tree: dozens of objects
root.get("b").forEach(bid -> handler.handleOrder(new PriceLevel(
        new BigDecimal(bid.get(0).asText()),            // String + BigDecimal per number
        new BigDecimal(bid.get(1).asText()), Side.BID, "binance")));
```
- **`readTree` is the serde `Value` approach.** It allocates a node for every value
  (about 130 per message here), then each number becomes a `String` and then a
  `BigDecimal`.
- **It can't tell a snapshot from an update** (the `handleOrder` interface), which is the
  crossed-book bug measured in section 2.
- **It doesn't check `e`, `s`, `pu` or `u`,** and doesn't recognise the subscription
  reply. A frame without `"b"` and `"a"` is silently ignored.
- **A level that fails to parse throws** half-way through `forEach`: earlier levels are
  already applied, and the `catch` logs and carries on.

The low-latency Java equivalent of this step: Jackson's **streaming** `JsonParser`
(token by token, no tree), or DSL-JSON. Read numbers from the token's byte offsets
without creating a `String`, and fill a reusable pre-allocated `long[]` level buffer.
That avoids the tree, though Jackson still allocates some internal state per message
unless carefully pooled. Fuzzing in Java is Jazzer (libFuzzer for the JVM).

## How to verify it yourself
```bash
cd rust
cargo test -p ob-parse --release                       # all 39 ob-parse tests
cargo test -p ob-parse --test binance                  # real frames + message rules
cargo test -p ob-parse --release --test binance_differential -- --nocapture  # prints acceptance rates
cargo bench -p ob-parse --bench binance                # serde vs ob-parse (~1 min)

# Fuzzing (needs nightly + cargo-fuzz, both now installed on this machine)
scripts/seed_fuzz_corpus.sh
cd crates/ob-parse && cargo +nightly fuzz run binance -- -max_total_time=300

# Re-record fresh fixtures (needs Python websockets >= 12)
python3 scripts/record_fixtures.py 60
```
Things to try:
- **See the oracle catch a bug:** in `binance.rs`, delete `Field::PrevFinalUpdateId,`
  from `REQUIRED_FOR_DEPTH` (and change `9` to `8`), then run the differential tests.
- **See the Java bug:** read `treating_snapshots_as_updates_leaves_stale_levels` in
  `tests/binance.rs`. It asserts the wrong book is crossed.

## Likely interview questions
1. **Why write your own JSON parser instead of using serde?**
   The schema is fixed and tiny, and the hot path matters. Measured: 3× faster than the
   best serde approach with zero allocation. The risk of a hand-written parser is
   correctness, so it's checked against a serde-based oracle over tens of thousands of
   generated and mutated messages, plus 25 million fuzzed inputs.
2. **How do you handle unknown or extra fields?**
   Skip them, but validate them as JSON. Binance already sends two undocumented fields
   (`ps`, `st`); a strict schema would have broken in production.
3. **What happens with duplicate keys?**
   Rejected for known fields. JSON allows duplicates, but implementations disagree on
   which wins, so a duplicated price field is ambiguous data.
4. **Snapshot or delta: why does it matter?**
   Binance partial depth is a full snapshot every 100 ms. Treating it as deltas left 109
   bid levels and a crossed book after 6 seconds of real data. Snapshots must replace the
   book.
5. **How do you detect a missed message on a snapshot stream?**
   `pu` must equal the previous `u`; it held in all 58 recorded pairs. On a gap the book
   is still correct after the next snapshot, but the gap is logged and counted.
6. **What does fuzzing add over property tests?**
   Coverage guidance: libFuzzer mutates towards inputs that reach new code, finding edge
   cases random generation misses. It ran 25 million inputs in 5 minutes under
   AddressSanitizer with no disagreement.
7. **How do you know your property tests are any good?**
   Two checks. A meta-test asserts each generator yields 10–90% accepted inputs (it
   caught mine at 5%). And planting a bug confirms the oracle catches it.
8. **Why reuse the output buffer?**
   Allocating, or even zero-initialising, 2 KB per message is wasted work on the hot
   path. Clearing a length is O(1).
