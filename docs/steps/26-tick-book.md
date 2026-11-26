# Step 26: A price-indexed book for full depth

After step 25, applying levels was half the time of a large full-depth message: ~30 µs of
the 25 KB message's ~60 µs. This step replaces the full-depth book's sorted arrays with
a **price-indexed** book. The small books (Binance depth20, Bybit 50) keep the sorted
arrays, which step 4 measured as the fastest structure at their size.

## What changed

| File | What |
|---|---|
| `crates/ob-core/src/tick_book.rs` | **New:** `TickBook<CAP>` / `TickSide`: quantities indexed by price tick in a circular window of 8,192 ticks, a bitmap of occupied ticks, best and worst tracked. 7 tests, including a differential property test against `OrderBook`. |
| `crates/ob-core/src/divisor.rs` | `exact_quotient`: price → tick index by shift and multiply (no `idiv`), for prices already known to be on the tick. 1 new test. |
| `crates/ob-core/src/book.rs` | **New trait `BookLevels`:** bids and asks best-first, implemented by both books. |
| `crates/ob-sync/src/book.rs` | `publish` / `publish_with_top` take any `BookLevels`; same code, same output. |
| `crates/ob-feed/src/full_depth.rs` | The book is `Box<TickBook<FULL_CAP>>`; the boundary rule uses the dropped price the book reports. |
| `crates/ob-feed/tests/full_depth.rs` | 2 new tests: the window rule moving the known-region boundary. |
| `crates/ob-feed/benches/book_apply.rs` | **New:** the two books, A/B on the recorded snapshot and diffs. |
| `CONSIDERATIONS.md`, `docs/LINUX.md`, `docs/TAIL-LATENCY-PLAN.md` | Updated with the results. |

## 1. What the slowest message does to the book

Simulating the recorded stream, the 25 KB message (line 86, 1,177 levels) is:

| Updates | Inserts | Removals | Removals of absent levels | Outside the known region |
|---|---|---|---|---|
| 651 | 133 | 116 | 27 | 250 |

The inserts land a median **216 levels below the top**. With sorted arrays, all ~930
in-region changes start with a binary search over ~1,000 prices: about 10 steps, and
the branches are unpredictable. Each insert or removal then shifts the better levels,
about 3.5 KB of the two arrays at the median depth.

## 2. The design

```text
qty:  [ q | 0 | q | q | 0 | ... ]   one slot per price tick: slot = tick mod 8192
bits: [ 1   0   1   1   0   ... ]   which ticks hold a level, 64 per word
best, worst                          the occupied ticks at the two ends
```

- **Update / insert / remove: O(1).** An array write and a bit flip. There's no search
  and no shifting.
- **Removing the best or the worst** scans the bitmap for the next set bit, 64 ticks per
  word (`leading_zeros` / `trailing_zeros`). In a dense book that's the same word or the
  next one.
- **Reading best-first** (publishing 256 levels) walks the set bits.
- **Price → slot:** `tick = price / tick_size` with the precomputed reciprocal from step 5
  (the tick size is only known at run time, so a plain `/` would be a ~25-cycle
  `idiv`). The slot is `tick & 8191`.
- **Size:** 2 × (64 KB quantities + 1 KB bitmap) ≈ 130 KB, so the adapter keeps it in a
  `Box`. Only the slots near the market are ever touched (~11 KB for a 1,000-level
  side).

**The circular window, and why it keeps the book exact.** Slots repeat every 8,192
ticks, so two prices 8,192 ticks apart would share one. The book therefore keeps every
level **within 8,192 ticks of the best** (819 USD at BTC's 0.1 tick; Binance's 1,000-level
snapshot spans ~140 USD). Like the 1,024-level capacity, the window can force levels out:

| Situation | `OrderBook` (before) | `TickBook` |
|---|---|---|
| Full side, a worse level arrives | `OutOfDepth` | `OutOfDepth` |
| Full side, a better level arrives | drops the worst | drops the worst, **reports its price** |
| A level a window or more beyond the best | (no such rule) | `OutOfDepth` |
| A better level a window or more above the worst | (no such rule) | drops worst levels until it fits, **reports the innermost** |

The full-depth adapter already turns every refused or dropped level into its
**known-region boundary** (step 19): prices beyond it are ignored, so the book never sits
next to a gap it can't see. The window rule feeds the same mechanism. If so much drops
that fewer than 256 levels remain known, the adapter asks for a new snapshot (`Depleted`),
which is the right outcome when the market has moved 800 USD. The book reports the
dropped price directly, where before the adapter peeked at the worst level before
applying.

**Alternatives considered:**

| Option | Verdict |
|---|---|
| Keep sorted arrays, search with a branchless binary search | Removes the mispredicts, keeps the shifting (step 4 had measured branchless as slower on top-heavy small books) |
| A B-tree or skip list | Allocates per node and chases pointers: the reason step 4 rejected `BTreeMap` |
| A non-circular ladder, re-centred when the price drifts | A full copy on every re-centre: a latency spike exactly when the market moves |
| **A circular window + bitmap** | **Chosen:** O(1) changes, no allocation, no re-centring; exactness preserved by the boundary |

## 3. Proving it

| Test | What it proves |
|---|---|
| `matches_the_sorted_book` (property, 2,000 cases × up to 200 operations) | While prices fit the window, **exactly** the same as `OrderBook` at capacity 16, so dropping and refusing happen constantly: every operation's result, every dropped price, both sides' contents, `is_crossed`, snapshots, clears |
| `wide_prices_keep_the_best_that_fit` (property) | Prices spread over 4 windows: always sorted, within the window, at most `CAP`; anything dropped was worse than everything kept |
| `neighbours_across_the_circular_wrap` | Levels on both sides of slot 8191 → 0 |
| `scans_stop_before_wrapping_onto_themselves` | The neighbour scan's limit (added after mutation testing: see below) |
| `the_window_refuses_or_drops_and_reports` | Both window cases, with the reported price |
| `a_level_beyond_the_price_window_bounds_the_region` | Full depth: a window refusal becomes the boundary |
| `a_jump_beyond_the_window_drops_the_far_levels_then_resyncs` | Full depth: a 800-USD jump drops the far levels and asks for a new snapshot |
| The 9 existing full-depth tests, and the zero-allocation proof | Unchanged and passing |

**Mutation testing:** six planted bugs.
- Five were caught by the tests above:
  - removing the "between worst and best" guard (slot aliasing);
  - a window one tick too wide;
  - not updating the worst level on removal;
  - a full side not making room;
  - one of the two bitmap-scan limits.
- The last survivor was **the other scan limit**, which the book itself never reaches:
  every scan starts from a level that has a neighbour within the window. It is the
  helper's contract, though, so `scans_stop_before_wrapping_onto_themselves` now tests
  it directly, and both scan-limit mutants fail it.

**Live:** `cargo test -p ob-feed --release --test live_full_depth -- --ignored` (step 19's
cross-check) ran 30 s against Binance. 291 diff events were applied with no resyncs.
Against 286 of Binance's own depth20 snapshots, **8,558 untouched levels matched, with 0
transient and 0 persistent mismatches.**

## 4. Results

**The data structure alone, A/B on the same work** (`cargo bench -p ob-feed --bench
book_apply`): the recorded 1,000-level snapshot, then all 100 diff events (10,313
levels; in-region ones applied), back to back, twice:

| | Run 1 | Run 2 |
|---|---|---|
| Sorted arrays (`OrderBook<1024>`) | 172.3 µs | 173.3 µs |
| **Price-indexed (`TickBook<1024>`)** | **69.4 µs** | **72.8 µs** |

That's **−58%**: about 7 ns per level including the known-region check, against about
17 ns.

**The slowest messages, whole handler call** (`tails --only anatomy`, medians):

| Message | Before (step 25) | After | Apply + publish before → after |
|---|---|---|---|
| Line 86 (24,875 B, 1,177 levels) | 59.8 µs | **36.8 µs** | 30.1 → **9.7 µs** (−68%) |
| Line 99 (15,622 B, 736 levels) | 37.3 µs | **23.8 µs** | 18.7 → **6.8 µs** |
| Line 85 (15,698 B, 740 levels) | 34.9 µs | **24.2 µs** | 16.0 → **7.0 µs** |

The machine's load average was ~8 during the after-run, so take ±10% on these. The
direction and size match the A/B above, which ran under equal load.

**What this changes:**
- **Parsing is now ~75% of a large message** (27 µs of 37 µs, ~1.1 ns per byte). The
  messages carry 1,177 price changes; under 10 µs would need SIMD across whole arrays of
  levels, not one level at a time.
- **Step 27 (publish only what changed) was dropped:** publishing is ~0.4 µs of the
  largest message's 37 µs (under 0.1 µs for the small books), too little to justify a
  more complex publish path.

## How to verify it yourself

```bash
cd rust
cargo test -p ob-core --release --lib tick_book        # 7 tests incl. the differential one
cargo test -p ob-feed --release --test full_depth      # 11 tests incl. the window boundary
cargo bench -p ob-feed --bench book_apply              # sorted vs price-indexed
target/release/tails --rt --only anatomy               # whole-message effect
cargo test -p ob-feed --release --test live_full_depth -- --ignored --nocapture  # live
```

Things to try:
- In `tick_book.rs` `set`, change `if inside && self.occupied(t)` to
  `if self.occupied(t)`: a tick-book test and two full-depth tests fail (a price a window
  away would hit another level's slot).
- Change `>= WINDOW_I` to `> WINDOW_I` in the refusal: the window tests fail.
- Switch `full_depth.rs` back to `OrderBook<FULL_CAP>` (and the boundary code from
  step 19): the anatomy shows apply + publish back near 30 µs.

## Java counterpart

The sorted-array book was the equivalent of a primitive-array book. Most Java order books
use `TreeMap<Long, …>` or `ConcurrentSkipListMap`: O(log n) with pointer chasing and a
node allocated per level. The price-indexed design is the classic Java low-latency
answer too: a `long[]` of quantities indexed by `(price / tick) & mask` and a `long[]`
bitmap scanned with `Long.numberOfLeadingZeros` / `numberOfTrailingZeros` (intrinsified
to `lzcnt` / `tzcnt` by HotSpot). It's allocation-free and GC-invisible, and the same
circular-window caveat applies.

## Likely interview questions

1. **Why not use the price ladder for every book?** At 20–50 levels the sorted array is
   faster (step 4): the whole side is two or three cache lines, and updates near the top
   move almost nothing. The ladder costs its fixed work per operation plus 130 KB of
   address space. It wins only when searching and shifting a deep array dominate.
2. **What happens when the price moves more than the window?** Levels too far from the
   new best are dropped, and the known-region boundary moves inwards, so the book never
   claims to know prices it has dropped. If fewer than 256 known levels remain, it asks
   for a new snapshot. An 800-USD move in one 100 ms diff is a market event worth a
   resync anyway.
3. **How do you find the next best level after removing the best?** Scan the bitmap from
   the removed slot towards worse prices: mask the current word, then one `lzcnt` or
   `tzcnt` per 64 ticks. In a dense book that's one or two instructions.
4. **Why divide by the tick without a divide instruction?** The tick size comes from
   configuration, so the compiler can't turn `/` into a multiply, and `idiv` costs ~25
   cycles. A price known to be a multiple of the tick divides exactly as
   `(n >> trailing_zeros(d)) × inverse(odd part of d) mod 2^64` (Granlund–Montgomery):
   one shift, one multiply.
5. **How did you make sure the new book is a drop-in replacement?** A property test runs
   random operation sequences on both books at a capacity small enough to hit every edge
   (drops, refusals, snapshots, clears) and demands identical results. A mutation run
   showed the tests catch the plausible mistakes, and one gap it exposed got its own
   test. A live 30-second cross-check against Binance's own depth20 found no mismatches.
