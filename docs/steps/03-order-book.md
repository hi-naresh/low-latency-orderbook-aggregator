# Step 3: The single-venue order book

Design reference: Phase 1 of the plan ("Layout", "Operations"). This step implements
README requirement 1: add and remove price levels, keep both sides sorted, query best
bid/ask, and detect a crossed book. It is the equivalent of the Java skeleton's
`OrderBook.java`.

## What changed

| File | Content |
|---|---|
| `rust/crates/ob-core/src/book.rs` | New: `Side`, `Level`, `LevelChange`, `SnapshotError`, `Ladder`, `OrderBook`; 12 tests + doc example |
| `rust/crates/ob-core/src/lib.rs` | Exports the new types |

Still `no_std`: the book can't allocate, by construction (step 1).

## 1. The data layout

```rust
#[repr(C, align(64))]
pub struct Ladder<const CAP: usize, const BID: bool> {
    px:  [i64; CAP],   // prices, sorted worst → best
    qty: [i64; CAP],   // quantities, parallel to px
    len: usize,        // how many slots are in use
}

pub struct OrderBook<const CAP: usize = 64> {
    bids: Ladder<CAP, true>,
    asks: Ladder<CAP, false>,
}
```

Four decisions, each with its reason:

**a) Fixed-size arrays, not a tree or a map.** The book lives in one contiguous 2,176-byte
block: 64 bids + 64 asks, prices and quantities. It never allocates, never chases a
pointer, and sits comfortably in L1 cache (32 KB on this CPU). Venues only send their
top N levels (Binance 20, Bybit 50), so `CAP = 64` has room to spare. If a side is full,
the worst level is dropped, which is correct because it's beyond what the venue promises
to keep us updated on.

**b) The best level is the *last* element.** Most updates hit the top of the book. At
the end of an array, an insert or delete moves only the few elements above it. With best
at index 0, every top-of-book change would shift the whole array.
```
bids (BID = true):  index 0 → len-1  =  worst (lowest) → best (highest)
asks (BID = false): index 0 → len-1  =  worst (highest) → best (lowest)
```

**c) Structure of arrays (prices in one array, quantities in another).** Finding a price
only reads `px`; `qty` is touched once, at the index found. A cache line (64 bytes) holds
8 prices. With an array of `(px, qty)` pairs, each line would hold only 4 prices, so a
scan would read twice as many lines. `#[repr(C, align(64))]` starts each side on a cache
line boundary, so one side's data never shares a line with the other's.

**d) The side is a compile-time constant (`const BID: bool`).** The only difference
between bids and asks is the direction of "better":
```rust
#[inline(always)]
const fn better(a: i64, b: i64) -> bool {
    if BID { a > b } else { a < b }   // BID is known at compile time
}
```
The compiler generates two copies of the ladder code, one per side, each with the
comparison fixed. There's no run-time branch on the side inside the hot loops. The single
branch is in `OrderBook::apply`, which picks the copy.

Layout is pinned by a test: `Ladder<64>` = 1,088 bytes (64-byte aligned),
`OrderBook<64>` = 2,176 bytes, `Level` = 16 bytes.

## 2. Operations

### Find: scan from the top
```rust
fn find(&self, px: i64) -> Result<usize, usize> {   // Ok(index found) / Err(insert position)
    let prices = &self.px[..self.len];
    let mut i = prices.len();
    while i > 0 {
        let p = prices[i - 1];
        if p == px { return Ok(i - 1); }
        if Self::better(px, p) { return Err(i); }   // px belongs just above p
        i -= 1;
    }
    Err(0)                                          // worse than everything
}
```
It starts at the best end because updates cluster there, so the loop usually stops
within a few elements. Binary search would always do about 6 steps for 64 levels, with
hard-to-predict branches. **Step 4 benchmarks both** and keeps the faster.

### Apply an update
`apply(side, level)` sets the quantity at that price. The quantity encodes the action,
as both venues do:

| Price in book? | Quantity | Action | Returns |
|---|---|---|---|
| yes | > 0 | overwrite quantity | `Updated` |
| yes | 0 | remove level, close the gap | `Removed` |
| no | > 0 | insert, keeping the order | `Inserted` |
| no | 0 | nothing | `RemovedAbsent` |
| no, side full, worse than all | > 0 | not kept | `OutOfDepth` |

`LevelChange` is returned so later steps can count events. For example, many
`RemovedAbsent` results suggest we've missed updates.

Inserting and removing move elements with `copy_within`, which compiles to `memmove`:
- **insert** shifts the *better* levels (above the insert position) up one slot;
- **remove** shifts them down one slot;
- **a full side** drops the worst level by shifting only the levels *below* the insert
  position. A new best on a full side moves the whole array, which is still only 63
  elements, in one `memmove`.

### Replace with a snapshot
`replace(bids, asks)` handles messages that contain the whole book (every Binance
message; Bybit's first message):
1. **Validate both sides first**: strictly best-first (no out-of-order or duplicate
   prices) and no zero quantities. Venues send snapshots sorted, so anything else is bad
   data → `SnapshotError`.
2. **Only then write**, with one linear copy per side (reversed, since we store best
   last). It costs O(n), versus O(n²) for inserting levels one at a time.

Because of step 1, **a bad snapshot leaves the book exactly as it was**. There's a test
where the bids are valid and the asks aren't, and neither side changes. That's the
"never apply half a message" rule from step 2's policy, at the book level.

### The other operations
- **`clear()` is O(1):** it only resets the two lengths. Old values stay in memory but are
  never read (everything reads `px[..len]`).
- **`best_bid()` / `best_ask()`:** read the last element, one load.
- **`level(depth)`:** the level `depth` places from the top.
- **`iter()`:** levels best → worst.
- **`is_crossed()`:** best bid **≥** best ask. The README defines crossed as "best bid
  higher than best ask". I also count **locked** (equal) books: on one venue, a bid and
  an ask at the same price would trade immediately, so seeing one means our copy is
  wrong. A book with an empty side isn't crossed. The *cross-venue* case (Binance bid >
  Bybit ask) is different: it's a real opportunity, not an error, and belongs to the
  aggregator (step 10).

## 3. What the book deliberately doesn't do
- **No venue ID per level.** The Java `PriceLevel` carries `exchangeId` on every level.
  Here, one book = one venue, and the aggregator (step 9) knows which book is which. That
  saves memory and work per update.
- **No validation of ticks or lots.** That happened when the text was parsed (step 2). The
  book trusts `Px`/`Qty` values because their types already guarantee them.
- **No locking.** The book has one writer, the venue's feed thread. Safe concurrent
  reading is step 8's job (seqlock). Keeping concurrency out of the data structure keeps
  each piece simple and testable on its own.
- **No reaction to a crossed book.** The book reports it; the feed decides what to do,
  usually a resync (step 13).

## 4. Tests (README edge cases)

| README requirement | Test |
|---|---|
| Zero-quantity updates remove a level | `zero_quantity_removes_the_level`: also removing the best, and removing an absent price |
| Multiple updates at the same price | `repeated_updates_at_one_price_keep_the_last_quantity`: also the same price written as `"100"` vs `"100.0"` |
| Crossing | `crossed_and_locked_books_are_detected`: one side empty, normal, locked, crossed, then fixed |
| Sorted levels | `bids_sort_high_to_low_and_asks_low_to_high`, `any_insert_order_gives_the_same_book` |
| Best bid/ask | `empty_book_has_no_best_and_is_not_crossed`, and every test above |
| (extra) depth limit | `a_full_side_keeps_the_best_levels`: both sides; worse-than-all, better-than-worst, new best, update and remove while full |
| (extra) snapshots | `snapshot_replaces_everything`, `a_bad_snapshot_is_refused_and_changes_nothing`, `snapshot_deeper_than_capacity_keeps_the_best` |
| (extra) layout | `layout_is_compact_and_cache_aligned` |

The README's "race conditions" case belongs to step 8, where concurrent access exists.

Totals for `ob-core`: **37 unit tests + 2 doc examples**, passing in debug and release.
`clippy -D warnings` is clean.

## 5. Benefits (measured, rough)
I applied 10M bid updates spread uniformly over the top 40 price levels, with about 20%
removals, reading the best bid after every update. Both languages ran the identical
stream. All three books produced the **same checksum**, so they agree on the result.
These are simple timing loops, not criterion or JMH (step 4).

| Book | ns per update (apply + best) | Allocation per insert |
|---|---|---|
| **Rust `OrderBook`** | **25.7–27.6** (steady) | none |
| Java `TreeMap<BigDecimal, BigDecimal>` | 86–258 (varies run to run) | one tree node per new price |
| Java `ConcurrentSkipListMap<BigDecimal, BigDecimal>` | 177–230 | skip-list node(s) per new price |

Java's BigDecimals were created *before* timing, so this compares only the data
structures. A real Java feed also pays for parsing `BigDecimal`s on every level (step 2:
~34 ns each).

Reading the numbers honestly:
- **Rust is about 3–10× faster here and, just as important, steady.** Java swings
  86 → 258 ns between runs: GC pauses, JIT recompilation, and tree nodes scattered across
  the heap. For trading, the *spread* of latencies matters as much as the average.
- **26 ns is above my 5–15 ns target, and I know why.** This test stream is deliberately
  harsh: prices are spread *uniformly* over 40 levels, so:
  - the average scan is about 20 elements;
  - inserts and removes shift about 20 elements;
  - the random mix of insert/update/remove makes branches unpredictable. A mispredicted
    branch costs ~15–20 cycles (5–7 ns at 2.8 GHz).

  Real feeds are concentrated at the top, where this layout is cheapest. **Step 4**
  measures properly with criterion, compares linear scan vs branchless binary search, and
  later (step 16) replays recorded real data. The design decision ("array beats tree")
  doesn't depend on that tuning: even with this harsh stream it's several times faster
  than the trees.

## 6. Java counterpart
The skeleton's stub:
```java
public class OrderBook implements OrderBookUpdateHandler {
    @Override
    public void handleOrder(PriceLevel priceLevel) {
        // Implement method to update internal order book state based on price priority ...
    }
    public BigDecimal getBestBidPrice() { return null; }
    ...
    public boolean isCrossed() { return false; }
}
```

What a typical Java solution writes (and what I benchmarked):
```java
private final NavigableMap<BigDecimal, BigDecimal> bids =
        new ConcurrentSkipListMap<>(Comparator.reverseOrder());   // best = first
private final NavigableMap<BigDecimal, BigDecimal> asks = new ConcurrentSkipListMap<>();

public void handleOrder(PriceLevel l) {
    var side = l.side() == Side.BID ? bids : asks;
    if (l.quantity().signum() == 0) side.remove(l.price());
    else side.put(l.price(), l.quantity());
}
public BigDecimal getBestBidPrice() {
    var e = bids.firstEntry(); return e == null ? null : e.getKey();
}
public boolean isCrossed() {
    var b = bids.firstEntry(); var a = asks.firstEntry();
    return b != null && a != null && b.getKey().compareTo(a.getKey()) >= 0;
}
```
How the two compare:
- **Memory:** each level is a skip-list node + an index node + two `BigDecimal`s, roughly
  100–150 bytes scattered across the heap, versus 16 bytes in two contiguous arrays.
- **Lookups:** about log₂(n) pointer hops, each a likely cache miss, versus a linear scan
  of contiguous `i64`s.
- **Allocation:** every new price allocates nodes, and every removal makes garbage that
  the GC must collect later.
- **Concurrency:** `ConcurrentSkipListMap` makes each `put` thread-safe, but **not each
  message**. A reader can see levels 1–6 of a 20-level message applied and 7–20 not,
  which is a torn book. Our design keeps the book single-writer, and step 8 makes each
  *message* atomic for readers.
- **Snapshots:** the Java interface (`handleOrder` per level) can't express "replace the
  whole book". Old levels from Binance's previous snapshot linger, which is the stale-level
  bug from my first analysis. Ours has `replace()`.
- **`BigDecimal` keys:** `TreeMap`/`ConcurrentSkipListMap` use `compareTo`, so `"100.0"`
  and `"100.00"` are one key (correct). A `HashMap` would split them (step 2).

What the fastest Java would look like: the same design as ours, with
`long[] px, qty` arrays, `System.arraycopy` for shifts, and no objects. It's possible in
Java, but you lose the type safety of `Px`/`Qty`, and GC/JIT jitter remains elsewhere in
the process.

## How to verify it yourself
```bash
cd rust
cargo test -p ob-core book          # the 12 book tests
cargo test -p ob-core               # everything in ob-core (37 + 2 doc examples)
cargo test -p ob-core --release
cargo doc -p ob-core --no-deps --open   # OrderBook page has a runnable example
```
Things to try:
- In `is_crossed`, change `>=` to `>`. Then `crossed_and_locked_books_are_detected` fails
  on the locked case.
- In `replace`, move `self.bids.load_snapshot(bids);` above the asks validation. Then
  `a_bad_snapshot_is_refused_and_changes_nothing` fails: half of a bad message got
  applied.
- In `Ladder::better`, swap `>` and `<`. Most tests fail, and the doc example too.

## Likely interview questions
1. **Why arrays instead of a `BTreeMap`/`TreeMap`?**
   With 20–64 levels, a contiguous array fits in a few cache lines and the CPU's
   prefetcher streams it. A tree is a pointer hop (likely a cache miss) per level visited,
   plus an allocation per insert. Inserting into an array is O(n) in theory, but at n ≤ 64
   a `memmove` of a few hundred bytes is cheaper than one cache miss.
2. **Why put the best level at the end?**
   Most updates are near the top. At the end of an array, inserts and deletes move few
   elements; at the front they'd move everything.
3. **Why separate price and quantity arrays?**
   Searching reads only prices. Separate arrays mean 8 prices per cache line instead of 4,
   so half as much memory is read per scan.
4. **What happens when a side is full?**
   The worst level is dropped. Venues only guarantee their top N levels, and `CAP` is
   above that N, so we never drop a level the venue is still updating.
5. **Why is a locked book "crossed"?**
   On one venue, a bid and ask at the same price would have traded. Seeing it means we
   missed an update, so it calls for a resync. Across venues, a lock or cross is a real
   price difference, handled by the aggregator.
6. **How is a snapshot applied atomically?**
   Validate everything first, then write. No path can leave half of a snapshot in the
   book. For readers on other threads, step 8 adds a seqlock so they never see a
   half-written book either.
7. **Why `const BID: bool` instead of passing the side at run time?**
   It makes the compiler generate a specialised copy per side, with the comparison fixed.
   There's no branch on the side inside the search and shift loops.
8. **Your benchmark shows 26 ns, not 5–15. Why?**
   The test stream is uniform over 40 levels with random operations, which is the worst
   case for scan length, shift length and branch prediction. Step 4 measures the
   alternatives properly; real top-heavy traffic is cheaper. Even this worst case is
   3–10× faster than the Java trees, and much steadier.
