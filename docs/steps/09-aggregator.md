# Step 9: The aggregator: best prices, volume, venue health

Design reference: Phase 3 of the plan ("Aggregator", "Health filtering", "Merge"). This
implements README requirement 2 (combine books from several exchanges, best levels
across venues, handle disconnections gracefully). It's the counterpart of the Java
skeleton's `getBestBid`, `getBestAsk`, `getBidVolumeForPrice` and `getAskVolumeForPrice`.
Execution planning (price for a volume, smart order routing) is step 10.

## What changed

| File | Content |
|---|---|
| `rust/crates/ob-agg/` | **New crate**: `Aggregator`, `Quotes`, `Volume`, venue health |
| `ob-agg/tests/aggregator.rs` | 14 behaviour tests |
| `ob-agg/tests/model.rs` | Property test against a naive reference (5,000 random cases) |
| `ob-agg/tests/real_data.rs` | Whole pipeline on the real Binance + Bybit recordings |
| `ob-agg/tests/concurrent.rs` | Two feed threads publishing while an aggregator thread queries |
| `ob-agg/benches/agg.rs` | Aggregator queries; the quantity-sum question deferred from step 2 |
| `ob-sync/src/book.rs` | **Changed:** the published book now carries the venue's **status** and local **receive time** (below) |
| `ob-core/src/fixed.rs` | `Qty::MAX` |

## 1. A change to step 8: health travels with the book

To decide whether to trust a venue, the aggregator needs its status (live, resyncing,
down) and how fresh its data is. If those were published separately from the levels,
the aggregator could combine one moment's health with another moment's book. So they're
written **in the same seqlock write**:
```rust
pub struct BookMeta { update_id, exchange_ts, recv_ns, status: VenueStatus }
pub enum VenueStatus { Down = 0, Syncing = 1, Live = 2 }   // all-zero = Down: unpublished is safe
```
- **The top-of-book read stays one cache line.** The status is packed into the lengths
  word (`n_bids | n_asks << 16 | status << 32`), and `update_id` moved to the second line.
  `top()` doesn't need it; the full snapshot still has it. A compile-time assertion keeps
  the first seven words within the line.
- **`recv_ns` is local time, not exchange time.** Staleness compares our own clock with
  our own clock. The exchange's timestamp could be skewed or delayed, which says nothing
  about whether *we* are still receiving.
- **A never-published book reads as `Down`,** because zero means `Down`, and garbage status
  bits from a discarded torn read also become `Down`.

Step 8's loom, Miri and stress tests were rerun on the new layout; all clean.

## 2. Design

### One aggregator view per reading thread, refreshed on demand
```rust
let mut agg = Aggregator::new(Config { max_age_ns: 2_000_000_000 });
let bn = agg.add_venue(VenueConfig { name: "binance", depth: 20 }, binance_reader)?;
let bb = agg.add_venue(VenueConfig { name: "bybit",   depth: 50 }, bybit_reader)?;
// hot loop:
agg.refresh();                       // copy only the venues that changed
let bids = agg.best_bids(now_ns);    // queries read the cached copies
```
- **`refresh()` compares versions** with one atomic load per venue, and copies a venue's
  book only if it published since. When nothing changed it costs **3.3 ns for two
  venues**.
- **Queries read the cached copies, not shared memory.** A series of queries (best bid,
  then volume, then routing) sees **one stable picture** until the next `refresh()`.
  Without that, the best bid could come from one version of a book and the volume from
  the next.
- **Each venue's copy is consistent (a whole message), but different venues are copied at
  slightly different moments.** That's unavoidable: the exchanges themselves aren't
  synchronised. Section 4 shows why that matters.

### Time is an input
Every health-dependent query takes `now_ns` instead of reading a clock:
- **Deterministic tests:** "stale at exactly max age + 1 ns" is a plain assertion, with
  no sleeping and no flakiness.
- **The caller owns the clock** (steps 15–16 use one monotonic clock, read once per
  loop iteration).
- **A receive time slightly in the future** (the clock read on another core) counts as
  age 0, not as a huge age caused by unsigned underflow.

### Which venues are used
`state(venue, now)` checks, in order:

| Check | Result | Why |
|---|---|---|
| status `Down` (or never published) | `Excluded(Down)` | Not connected |
| status `Syncing` | `Excluded(Syncing)` | Feed is rebuilding the book (e.g. Bybit gap, step 7) |
| `now − recv_ns > max_age_ns` | `Excluded(Stale { age_ns })` | Silent feed: dead connection not yet noticed |
| best bid ≥ best ask on this venue | `Excluded(Crossed)` | Our copy is wrong (step 3) |
| otherwise | `Included` | |

- **The default `max_age` is 2 s.** On BTCUSDT both venues publish every 20–120 ms, so
  2 s of silence is abnormal. It's configurable.
- **Every exclusion has a reason** (`Exclusion`), so the printer and logs can say *why*
  a venue is missing, not just that it is.
- **This is "handle exchange disconnections gracefully".** The moment a feed marks itself
  down or resyncing, or simply goes quiet, its prices stop being used. When it publishes
  as `Live` again, they come back. The feed's reconnect logic is step 13.

### Queries (the Java skeleton's API)

| Java skeleton | Rust | Semantics |
|---|---|---|
| `getBestBid()` → `List<Quote>` | `best_bids(now) -> Quotes` | One quote per **included** venue, best first: highest price, then larger size, then venue order |
| `getBestAsk()` | `best_asks(now)` | Lowest price first; same tie-breaks |
| `getBidVolumeForPrice(price)` | `bid_volume_at_or_above(limit, now) -> Volume` | Per venue and in total: bid quantity at `limit` **or higher**, i.e. how much could be sold now without going below `limit` |
| `getAskVolumeForPrice(price)` | `ask_volume_at_or_below(limit, now)` | Ask quantity at `limit` **or lower**, i.e. how much could be bought without paying more |
| `Quote(price, qty, exchangeId: String)` | `Quote { venue: VenueId, px, qty }` | `VenueId` is a 1-byte index; `agg.venue(id).name` gives `"binance"` |

**The README's "volume for a price" is ambiguous** (exactly at that price, or at that
price or better?). I chose "at the price or better" because it answers a trading
question, how much could be executed without going past `X`. Exactly-at-`X` volume
rarely matters. The choice is documented on the methods.

**Completeness: volume beyond what we can see.** Binance sends only 20 levels. If every
visible bid is at or above the limit, the venue might have more volume deeper than it
sends us. Each `VenueVolume` has `complete: bool`:
- `true` if we can see where the volume ends: a level beyond the limit, or fewer levels
  than the venue's published depth (so that's the whole book);
- `false` if all visible levels matched. **The number is then a lower bound, and the
  result says so** instead of silently under-reporting.

`Volume::complete` is `true` only if every venue's volume is complete. That's why
`VenueConfig` has `depth` (20 for Binance, 50 for Bybit).

### No allocation in queries
`Quotes` and `Volume` are fixed arrays of `MAX_VENUES = 8`, with insertion sort for
ordering: no allocation, a handful of compares. Venues are added once at start-up, into a
`Vec` with fixed capacity. `VenueId` is checked to fit (at most 8 venues; `add_venue`
returns `None` beyond that).

## 3. Testing

| Test | What it proves |
|---|---|
| **14 behaviour tests** (`aggregator.rs`) | The Java skeleton's own test (one exchange, one bid → one quote); ordering across 3 venues; tie-breaks; every exclusion reason with the exact age; the staleness boundary (inclusive at max age, clock skew is age 0); never-published = down; refresh copies only changed venues; queries are stable between refreshes; volume at inclusive limits, above and below every level; completeness flags; quantity sums report overflow; at most 8 venues |
| **Property test** (`model.rs`, 5,000 cases) | 0–4 venues with random books (sometimes crossed), random status and receive times, random `now` and limit. Best bids, best asks, both volume queries (per venue, totals, completeness) must equal a naive reference that recomputes everything with loops and a full sort |
| **Real data** (`real_data.rs`) | Parse the real Binance and Bybit recordings → apply → publish → aggregate. The aggregate best bid/ask equal the best of the two books; volume equals a direct sum over both books |
| **Concurrency** (`concurrent.rs`, 2 s) | Two feed threads publish self-checking books (10.3 million publishes) while the aggregator refreshes 1.09 million times. Every per-venue view is a whole publish, and results stay ordered |

**A planted bug is caught.** Changing the staleness check from `>` to `>=` (an
off-by-one) makes the property test fail and shrink to a minimal case. Restored
afterwards.

**A gap in my own overflow test, found while writing this note.** The test checked
`MAX + 1`. An unchecked sum wraps that to a *negative* number, which `Qty::from_raw`
rejects anyway, so a wrapping implementation would still have passed. I added
`MAX + MAX + 2`, which wraps to exactly **0**, a valid-looking quantity. Then I swapped
in a wrapping sum to prove the test now fails (`left: Some(Qty(0)), right: None`), and
restored the code.

## 4. What the real data revealed: a fake cross-venue arbitrage
```text
best bid 58656.2 on bybit, best ask 58647.5 on binance; bid volume ≥ 58651.2: 0.621 BTC (complete)
```
**Bybit's best bid is above Binance's best ask by $8.70.** Taken at face value: buy on
Binance, sell on Bybit, instant profit. But my two recordings were made about 15 seconds
apart, and BTC rose about $9 in between. Each venue's book was correct *for its own
moment*; comparing two different moments produced the "opportunity".

The same thing happens live, on a smaller scale. Two venues' messages arrive at different
times, with different network delays and different publishing cadences (Binance
conflates to 100 ms). **So step 10's arbitrage signal will require both books' exchange
timestamps to be within a tolerance,** and will report the skew alongside every signal.
Recording that lesson here, from real data, before writing the detector.

## 5. Performance

Criterion, the real recordings (Binance 20+20, Bybit 50+50), two venues, i7-8569U, two
runs:

| Operation | Time |
|---|---|
| `refresh()`, nothing changed | **3.3–3.8 ns** (two version loads) |
| Both venues publish + `refresh()` copies both | 203–205 ns (publish ~36 + ~76 ns, copy ~30 + ~70 ns) |
| `best_bids()` + `best_asks()` | 39–40 ns |
| `bid_volume_at_or_above`, top level only | 40 ns |
| `bid_volume_at_or_above`, $10 deep (both books) | 72 ns |

For context: one Binance message takes ~1.6 µs to parse (step 6). A full
publish → refresh → query cycle across both venues stays well under 0.5 µs.

### The quantity-sum question from step 2, answered (and my prediction was wrong)
Step 2 found checked `Qty + Qty` ~50% slower than unchecked in a tight loop, and
predicted that **accumulating in `i128` and checking once** would recover the speed.
Measured over 1 million quantities:

| Strategy | ns / element | Safe? |
|---|---|---|
| unchecked wrapping `i64` | 0.30 | **No** (wraps silently on overflow) |
| checked `+` on every element | **0.43** | yes |
| checked fold, stops at first overflow (**chosen**) | 0.47 | yes |
| `u64` sum with a branch-free overflow flag | 0.50–0.51 | yes |
| `i128` accumulate, check once (step 2's prediction) | 0.49–0.53 | yes |

**The `i128` idea is not faster.** An `i128` add is two dependent instructions
(`add` + `adc`), and like the other safe variants it doesn't vectorise. Only the unchecked
loop gets SIMD.

**A second lesson: code alignment.** Over 50 elements (book-sized), `checked + each`
measured **13 ns in one build and 22 ns in the next, with identical code**. The only
change was two other functions added to the benchmark binary, which shifted where the
loop sits in memory. At this scale, layout noise is as large as the differences between
strategies.

**Decision:** `sum_qty` uses the short-circuiting checked fold. It's exact, returns
`Option` (overflow is reported, never wrapped), and among the safe variants it's
consistently in the fast group. Real books have at most 64 levels, so the choice is worth
a few nanoseconds at most. Clarity wins.

## 6. Java counterpart
The skeleton's stubs:
```java
private final ConcurrentHashMap<String, OrderBook> books = new ConcurrentHashMap<>();
public List<Quote> getBestBid()                          { throw new UnsupportedOperationException(); }
public List<Quote> getBidVolumeForPrice(BigDecimal price) { throw new UnsupportedOperationException(); }
```
A typical Java implementation:
```java
public List<Quote> getBestBid() {
    return books.entrySet().stream()
        .filter(e -> e.getValue().getBestBidPrice() != null)
        .map(e -> new Quote(e.getValue().getBestBidPrice(), e.getValue().getBestBidSize(), e.getKey()))
        .sorted(Comparator.comparing(Quote::price).reversed())
        .toList();
}
```
How it compares with this step:
- **Allocation:** a stream pipeline, an `ArrayList`, and one `Quote` per venue on every
  call. Our `Quotes` is a fixed array on the stack.
- **Consistency:** `getBestBidPrice()` and `getBestBidSize()` are two separate reads of a
  book another thread is mutating, so the price can come from one update and the size
  from the next. Here a venue's book is a consistent copy.
- **No health:** a disconnected exchange's last prices stay in `books` forever and keep
  being quoted. There's no status and no staleness. "Handle exchange disconnections
  gracefully" isn't addressed.
- **Exchange IDs are `String`s** compared and hashed on every lookup; here a `VenueId` is
  one byte.

The low-latency Java version: per-venue `StampedLock` (or a hand-written seqlock) around
`long[]` books; a pre-allocated `Quote[]` reused per call; and a health enum plus
`System.nanoTime()` receive stamp written with the book.

## How to verify it yourself
```bash
cd rust
cargo test -p ob-agg --release                                         # all ob-agg tests
cargo test -p ob-agg --release --test real_data -- --nocapture         # prints the real-data line
cargo test -p ob-agg --release --test concurrent -- --nocapture        # publish/refresh counts
cargo bench -p ob-agg --bench agg                                      # queries + qty_sum strategies
```
Things to try:
- In `aggregator.rs`, change `if age_ns > self.config.max_age_ns` to `>=`. Both
  `staleness_boundary_is_inclusive` and the property test fail.
- In `quotes.rs`, make `sum_qty` an unchecked `i64` sum. Then
  `quantity_sums_are_exact_and_overflow_is_reported` fails (it wraps instead of
  reporting).

## Likely interview questions
1. **How do you handle an exchange disconnecting?**
   Health travels with the book in the same atomic publish: status (down, syncing, live)
   plus the local receive time. The aggregator excludes venues that are down or syncing,
   that have been silent longer than a threshold (2 s by default), or whose book is
   internally crossed, and reports why. They come back automatically when healthy.
2. **Why measure staleness with your own clock, not the exchange's timestamp?**
   The exchange clock can be skewed, and its timestamp says when it sent the message, not
   whether we're still receiving. Local receive time versus local now measures what
   matters: is the data flowing to us?
3. **"Volume for a price": exactly at the price, or at or better?**
   At or better, because that answers "how much can I trade without going past X". I
   also flag when the visible depth may hide more volume (Binance sends only 20 levels).
4. **Why cache per-venue copies instead of reading shared memory on every query?**
   One stable picture across a series of queries, and almost nothing to pay when nothing
   changed (3.3 ns for two version loads).
5. **Your recorded data shows Bybit's bid above Binance's ask. Arbitrage?**
   No: the recordings were 15 seconds apart. Comparing books from different moments
   creates fake crosses, so cross-venue signals must check timestamp alignment.
6. **Is accumulating in `i128` faster than checked adds?**
   No, I measured it: 0.49–0.53 ns per element against 0.43 for checked adds. And at book
   sizes, code alignment moved one variant from 13 to 22 ns with no code change. So I
   chose the clearest safe version.
7. **Why is time passed in instead of read inside?**
   It makes the code deterministic and testable at exact boundaries, with one clock owned
   by the caller.
