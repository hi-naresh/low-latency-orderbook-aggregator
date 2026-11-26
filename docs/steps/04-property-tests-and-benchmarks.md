# Step 4: Property tests and the first real benchmarks

Design reference: Phase 1 of the plan ("Tests" and the linear-vs-binary decision). Step 3
built the book; this step answers two questions with evidence:
1. **Is the book correct for *any* sequence of operations,** not just the cases I thought
   to write down? → property tests against a model.
2. **Which way of finding a price is actually fastest,** and how does the book compare
   with a tree? → criterion benchmarks.

The benchmarking itself went wrong three times before the numbers could be trusted.
Those three traps are probably the most interview-relevant part of this step (section 3).

## What changed

| File | Content |
|---|---|
| `rust/crates/ob-core/src/search.rs` | New: four search strategies + the `Search` trait; exhaustive agreement test |
| `rust/crates/ob-core/src/book.rs` | The book takes the search strategy as a type parameter; default **`StdBinary`** (chosen by measurement) |
| `rust/crates/ob-core/tests/model.rs` | New: 5 property tests against a `BTreeMap` model |
| `rust/crates/ob-core/benches/book.rs` | New: criterion benchmarks (search, apply, snapshot) |
| `rust/crates/ob-core/Cargo.toml` | Dev-dependencies `proptest 1.12`, `criterion 0.8` (test/bench only; the library still has **zero** dependencies) |
| `.github/workflows/ci.yml` | New job: benchmarks must compile |

## 1. Search strategy as a zero-cost type parameter

```rust
pub trait Search {
    fn find<const BID: bool>(prices: &[i64], px: i64) -> Result<usize, usize>;
}
pub struct OrderBook<const CAP: usize = 64, S = StdBinary> { ... }
```
- `OrderBook<64, LinearFromBest>` and `OrderBook<64, StdBinary>` are different types. The
  compiler builds a separate copy of the book for each, with the search inlined.
- There's no function pointer, no `dyn`, and no run-time branch to pick the strategy.
- Code that writes plain `OrderBook` gets the default and never sees this.
- It's what made it possible to benchmark **whole updates** under each strategy, not just
  the search in isolation. That turned out to matter (section 4).

The strategies (all in `search.rs`):

| Strategy | How it works |
|---|---|
| `LinearFromBest` | Step 3's scan from the top. Cheap when the price is near the top, but the loop exit is a data-dependent branch |
| `BranchlessBinary` | Binary search where each step picks a half with a conditional move (`cmov`) instead of a jump, so there's nothing to mispredict. Always about log₂(n) steps |
| `StdBinary` | The standard library's `binary_search_by` |
| `Hybrid` | Linear over the top 8 levels (one cache line of prices), then branchless binary over the rest |

All four must return exactly the same answer. That's checked exhaustively: every length
0–69, both sides, every price in range, present and absent. A property test then checks
them on arbitrary `i64` values, including the extremes.

## 2. Property tests: the book vs an obviously-correct model

**Idea:** instead of hand-picking cases, write a second, dead-simple implementation, the
*model*: a `BTreeMap<price, qty>` per side, about 60 lines, no performance concerns.
Generate thousands of random operation sequences, run each on both, and require them to
agree **after every single operation** on:
- what the operation did (`LevelChange`, or the snapshot's `Ok`/`Err`);
- every level on both sides, in order;
- whether the book is crossed.

**How the operations are generated** (designed to reach the hard paths):
- **85% single updates**, with prices from a narrow 40-price band so the same prices keep
  recurring (updates, removals, collisions). Quantity 0–4, so about 1 in 5 is a removal.
- **10% snapshots:** half valid (sorted, unique, non-zero), half arbitrary, which are
  usually invalid. This exercises "a bad snapshot changes nothing".
- **5% clears.**

**Five properties, 2,000 random sequences each, up to 300 operations per sequence:**

| Test | Why |
|---|---|
| `small_book_matches_model` (capacity 4) | With 40 possible prices, sides are full almost all the time, so "drop the worst" and `OutOfDepth` run constantly |
| `single_level_book_matches_model` (capacity 1) | Edge case: every better price replaces the only level |
| `full_size_book_matches_model` (capacity 64) | The production configuration |
| `binary_search_books_match_model` | The same checks with `BranchlessBinary`, `StdBinary` and `Hybrid` at capacities 4 and 64 |
| `searches_agree_on_arbitrary_prices` | All search functions agree on random sorted `i64` values, including extremes |

Those are roughly 10,000 sequences and over a million checked operations per run. They
take about 2 s in release and 8 s in debug.

### Proof the tests catch bugs
A test suite that has never failed hasn't proven anything. So I planted a subtle bug: in
the "side is full" path, I changed `pos == 0` to `pos <= 1`. That wrongly discards a new
level that's better than only the worst one. The 38 unit tests caught it, and so did the
property test. **proptest then shrank the failure to the smallest sequence that still
fails:**
```
minimal failing input: ops = [
    Apply(Bid, 1004, 1), Apply(Bid, 1005, 1), Apply(Bid, 1006, 1), Apply(Bid, 1000, 1),
    Apply(Bid, 1001, 1),
]
left: `OutOfDepth`, right: `Inserted`: step 4
```
Read it as: fill a capacity-4 side, then add 1001, which beats only the worst (1000). The
model inserts it; the buggy book refuses. Five operations, with the cause obvious from
the output. **Shrinking is the main reason to use proptest** over a hand-rolled random
loop. The code was restored afterwards and everything passes again.

## 3. Three benchmarking traps, found and fixed

The first runs gave numbers that didn't make sense. Each oddity turned out to be a flaw
in the *measurement*, not in the code.

### Trap 1: calling through a function pointer blocks inlining
The first harness stored the strategies in an array of function pointers and called
`find(prices, q)`. The CPU can't inline through a pointer, so each query paid a real
call, plus the loss of optimisations across the call. **"Find the top price" measured
6.5 ns**, which is absurd for one or two compares.

**Fix:** make the harness generic (`bench_one::<S: Search>`), so the search is inlined
into the loop exactly as it is inside the book. The same query then measured **0.87 ns**
(still flattered by trap 3, below; the honest figure is 4.2 ns).

**Lesson:** benchmark code the way it's called in production. A function pointer or
trait object in the harness measures the call, not the function.

### Trap 2: an `if` that should become `cmov` became a branch
My "branchless" binary search was written so the compiler *could* use a conditional move:
```rust
base = if better(px, prices[mid]) { mid } else { base };
```
But its time **depended on the access pattern** (5.6 ns for the top, 21 ns for uniform
access). A truly branchless search can't do that: it executes the same instructions
whatever the data. So LLVM had compiled the `if` into a branch, and random data made it
mispredict. Meanwhile the standard library's search was flat, so *it* was the branchless
one.

**Fix:** tell the compiler explicitly:
```rust
base = core::hint::select_unpredictable(better(px, prices[mid]), mid, base);
```
`select_unpredictable` (stable since Rust 1.88) means "this condition is random, so use
a conditional move". After the fix: a **flat ~4.6 ns** for every pattern. I confirmed it
by disassembling a non-inlined copy: the loop has a `cmovg` and no data-dependent jump.

**Lesson:** "branchless" is a property of the *machine code*, not the source. Check it
in the assembly, or with a benchmark that varies the data pattern.

### Trap 3: the branch predictor memorised the test input
The first harness reused the same 1,024 queries (and 4,096 updates) on every iteration.
Modern branch predictors keep a long history and can learn a repeating sequence of about
that size. The branchy searches looked great because the CPU had effectively memorised
the answers:

| Linear search, depth 20 | 1,024 repeated queries | 65,536 queries |
|---|---|---|
| Always the top level | **0.87 ns** | **4.2 ns** |
| Top-heavy | 1.4 ns | 9.0 ns |

The "top" case still involves a branch: half the queries are present prices and half
fall between levels, at random. With 1,024 queries the predictor learned that sequence;
with 65,536 it can't.

**Fix:** 65,536 queries and 262,144 updates per iteration, far beyond what the predictor
can memorise. Real market data never repeats like that, so this is the honest setting.

**Lesson:** small, repeated benchmark inputs flatter branchy code. Production branch
behaviour needs input that doesn't repeat.

## 4. Results (trustworthy version)

Machine: i7-8569U, macOS, release profile (fat LTO, 1 codegen unit), criterion with 50
samples, inputs too large to memorise. Values are criterion's median, divided per
operation. Run-to-run noise is about ±5–10%.

### Search only (ns per query)

| Depth 20 | top | top-heavy | uniform | bottom |
|---|---|---|---|---|
| `LinearFromBest` | **4.2** | 9.0 | 12.6 | 13.9 |
| `BranchlessBinary` | 5.3 | **4.9** | **4.8** | **5.4** |
| `StdBinary` | 10.9 | 11.0 | 12.2 | 11.9 |
| `Hybrid` | 5.2 | 10.4 | 13.0 | 14.2 |

| Depth 50 | top | top-heavy | uniform | bottom |
|---|---|---|---|---|
| `LinearFromBest` | **4.2** | 9.1 | 18.6 | 27.0 |
| `BranchlessBinary` | 6.0 | **5.5** | **5.6** | **5.4** |
| `StdBinary` | 12.8 | 12.2 | 11.7 | 13.2 |
| `Hybrid` | 6.2 | 10.1 | 13.6 | 13.8 |

In isolation, `BranchlessBinary` wins every realistic pattern and is **flat**: its
latency doesn't depend on where the price is. Linear only wins when every query hits
exactly the same level. Hybrid doesn't help, because its top-8 scan has the same
mispredictions as linear.

### Whole update: search + shift + read best (ns per update, both sides)

This is what the book actually does on every message level, so it decides. Three
separate runs; ranges shown.

| Book | top-heavy | uniform over 40 levels |
|---|---|---|
| Array + `LinearFromBest` (step 3's) | 21.7–23.4 | 33.6–36.0 |
| Array + `BranchlessBinary` | 20.3–21.5 | 24.0–26.3 |
| **Array + `StdBinary` (chosen)** | **18.6–19.8** | **22.0–24.5** |
| Array + `Hybrid` | 21.9–25.8 | 27.2–27.9 |
| `BTreeMap` book | 36.0 | 39.0 |

### Snapshot (20 bids + 20 asks; Binance sends one every message)

| Method | Time |
|---|---|
| **`OrderBook::replace`** (validate, then one linear copy) | **~68–75 ns** |
| `clear()` + `apply` each level | ~690–850 ns (≈10× slower) |
| `BTreeMap` clear + insert each | ~1.4–1.7 µs (≈20× slower) |

### Decision: `StdBinary` is the default
- It was the **fastest for whole updates on both workloads**, in all three runs, by about
  5–10% over `BranchlessBinary` and 15–35% over linear.
- Its latency is **flat**: it doesn't grow with depth, which keeps the worst case
  (tail latency) predictable. Linear's worst case at depth 50 is 27 ns for the search
  alone.
- It's the standard library's tested implementation, so there's less custom code to
  trust.

**Open question, stated honestly:** in isolation `BranchlessBinary` is about 2× faster
than `StdBinary`, yet inside a whole update it's slightly slower. I don't know why yet.
Candidates include different instruction scheduling once inlined next to the shift code,
the extra final bounds check in my version, and code alignment. Step 20 (assembly
review) investigates. The decision rests on the end-to-end number, which is the one that
matters. All strategies stay in the code and under test, so switching is one type name.

### What the numbers say about where time goes
With the best search (about 5 ns), a whole update still costs about 19–24 ns. The
remaining ~15 ns is mostly:
- **Shifting:** `copy_within` becomes a call to the system `memmove`, which carries call
  overhead and its own size checks for moves of only a few hundred bytes.
- **Branches on the kind of update** (insert / update / remove), which in this test data
  is random and so mispredicts often.

Those are the targets for later tuning (step 20): an inlined fixed-size shift, and
looking at real data, where the kind of update is far more predictable. Against the
step 3 estimate (26 ns, bids only, linear): the chosen strategy is 22–24 ns on the harder
two-sided uniform stream and 19–20 ns on realistic top-heavy traffic.

**Array vs `BTreeMap`:** in Rust the array book is about 1.6–1.9× faster. That's a
smaller gap than against Java's trees (step 3: 3–10×), because Rust's `BTreeMap` stores
up to 11 keys per node, contiguously, which is far more cache-friendly than Java's
one-object-per-node `TreeMap`. The array still wins, and it never allocates.

## 5. Java counterpart
- **Property testing in Java:** jqwik or junit-quickcheck do the same job:
  ```java
  @Property(tries = 2000)
  void bookMatchesModel(@ForAll("ops") List<Op> ops) { ... compare after each op ... }
  ```
  The skeleton has no tests beyond one stub, and the README's "test edge cases" list is
  exactly what a model-based property test covers systematically.
- **Benchmarking in Java is JMH,** and it has the same traps under different names:

  | Our trap | Java/JMH equivalent |
  |---|---|
  | 1. Function pointer blocks inlining | Calling through an interface with 3+ implementations makes the call site *megamorphic*, so the JIT stops inlining it. Benchmark one implementation per run (`@Param`), or the numbers show call cost |
  | 2. `if` vs `cmov` | The JIT decides on its own, based on runtime branch profiles. There's no `select_unpredictable`, so you can't force it, only measure it |
  | 3. Memorised input | Same hardware effect. Plus JIT warm-up: JMH needs `@Warmup` iterations, and the optimised code depends on the profile seen during warm-up |
  | Dead code elimination | `Blackhole.consume(x)` ↔ our `black_box(x)` |

- **The step 3 Java numbers came from a plain timing loop.** A proper JMH comparison
  (with `-prof gc` showing allocation per update) is planned for step 22.

## How to verify it yourself
```bash
cd rust
cargo test -p ob-core --release                      # 38 unit + 5 property tests + 2 doc examples
cargo test -p ob-core --test model                    # property tests only (debug, ~8 s)
cargo bench -p ob-core --bench book                   # full benchmark suite (~3 min)
cargo bench -p ob-core --bench book -- 'apply'        # just whole updates
# Results print to the terminal and are saved under target/criterion/ (raw data; HTML
# reports are switched off to keep the dependency small).
```
Things to try:
- **Watch shrinking work:** in `book.rs` change `} else if pos == 0 {` to
  `} else if pos <= 1 {` and run `cargo test -p ob-core --release --test model`. You get
  the 5-operation minimal case above. Revert afterwards.
- **See Trap 3:** set `QUERIES` to `1024` in `benches/book.rs` and re-run
  `cargo bench -p ob-core --bench book -- search/depth20/linear`. The "top" case drops
  from ~4 ns to ~1 ns.
- **See Trap 2:** in `search.rs`, replace the `select_unpredictable(...)` line with
  `base = if better::<BID>(px, prices[mid]) { mid } else { base };`. `branchless_binary`
  then becomes pattern-dependent again.
- **Switch strategy:** write `OrderBook<64, BranchlessBinary>` anywhere; the tests prove
  it behaves identically.

## Likely interview questions
1. **What is property-based testing, and why use it for an order book?**
   You state rules that must hold for all inputs (here: "behaves exactly like this simple
   model") and the tool generates thousands of inputs, including ones you'd never think
   of. On failure it shrinks to a minimal case. An order book has a huge state space, and
   bugs hide in sequences, not single operations.
2. **How do you know the property tests can actually fail?**
   I planted a bug and watched them fail. proptest shrank it to 5 operations that show
   the cause directly.
3. **Linear or binary search for an order book?**
   It depends on data and depth, so measure whole updates on realistic input. Here the
   standard binary search won end to end, and its latency is flat across depth, which
   protects the tail. Linear only wins if every update hits the same level.
4. **What's a branch misprediction, and why does it matter here?**
   The CPU guesses which way a branch goes and runs ahead speculatively. A wrong guess
   throws that work away: about 15–20 cycles, or 5–7 ns at 2.8 GHz. Market data is
   random, so data-dependent branches mispredict often. Branchless code (`cmov`) avoids
   the gamble at the cost of always doing all the work.
5. **How do you make sure code is actually branchless?**
   Check the assembly (look for `cmov` and no conditional jump in the loop) and benchmark
   with varied data patterns: branchless code has flat timing. Use
   `core::hint::select_unpredictable` to tell the compiler the condition is random.
6. **Name some micro-benchmarking pitfalls.**
   Indirect calls that block inlining; the compiler deleting unused results (use
   `black_box`); inputs small enough for the branch predictor or caches to memorise;
   measuring a component in isolation instead of in context; noise (repeat runs and look
   at the spread).
7. **Why is the search 2× faster in isolation but slower end to end?**
   It's an open question I'm investigating in the assembly review. In-context effects
   (scheduling next to the shift code, alignment, an extra bounds check) can reverse
   micro-benchmark results, which is why decisions use the end-to-end benchmark.
8. **Why is `BTreeMap` only 1.7× slower here, but Java's `TreeMap` 3–10× slower?**
   Rust's `BTreeMap` packs up to 11 keys per node contiguously (cache-friendly, few
   allocations). Java's `TreeMap` is one heap object per entry, with pointer-chasing and
   GC pressure.
