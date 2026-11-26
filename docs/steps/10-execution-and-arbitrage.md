# Step 10: Price for volume, fee-aware optimal execution, arbitrage

Design reference: Phase 3 of the plan ("Fee-aware routing", "Cross-venue crosses"). This
completes README requirement 2, "Calculate optimal execution path for a given size". It
implements the last three Java skeleton methods: `getAskPriceForVolume`,
`getBidPriceForVolume` and `calculateOptimalExecution`. It also adds a cross-venue
arbitrage scan, using the lesson from step 9's real data.

## What changed

| File | Content |
|---|---|
| `ob-agg/src/execution.rs` | New: `Action`, `ExecutionPlan`, `ChildOrder`; `optimal_execution`, `ask_price_for_volume`, `bid_price_for_volume` |
| `ob-agg/src/arbitrage.rs` | New: `scan_arbitrage` → `ArbScan { best, rejected_for_skew }` |
| `ob-agg/src/aggregator.rs` | `VenueConfig::taker_fee_ppm`; `Config::max_skew_ms`; `config()`, `set_taker_fee()` |
| `ob-agg/tests/execution.rs` | 14 behaviour tests (routing, fees, rounding, partial fills, arbitrage) |
| `ob-agg/tests/optimality.rs` | **Brute-force optimality proof** on random books + a sort-based reference |
| `ob-agg/tests/real_execution.rs` | Routing and arbitrage on the real Binance + Bybit recordings |
| `ob-agg/benches/agg.rs` | Execution and arbitrage benchmarks |

## 1. Optimal execution

### The question
Buy (or sell) `Q` units across all healthy venues at the lowest total cost **after
fees**. Each visible level is a block of quantity at a fixed cost per unit: its price,
adjusted by its venue's taker fee.

```text
buy:  effective price = px × (1 + taker_fee)      cheapest first
sell: effective price = px × (1 − taker_fee)      highest first
```

### Why greedy is optimal (the interview answer)
Choose `x_l` units from each level `l`, with `0 ≤ x_l ≤ qty_l` and `Σ x_l = Q`,
minimising `Σ cost_l · x_l`. That's a linear programme with a single constraint, the
*fractional knapsack*. **Exchange argument:** if a plan took a unit at cost `c` while a
cheaper unit `c' < c` was left untaken, swapping those two units makes the plan cheaper.
So an optimal plan takes the cheapest units first, which is exactly the greedy merge.

**Proved by brute force, not just argued.** `tests/optimality.rs` enumerates **every**
possible split of the order across every level of every venue, on 3,000 random small
books with fees of 0–0.5%. It checks that no split is cheaper than the plan, using exact
integer costs, for buys and sells. To show the test has teeth, I planted a bug
(`optimal_execution` ignoring fees). The brute force immediately found a cheaper split
and shrank it to a minimal case.

### How it's computed: a k-way merge, no sorting, no allocation
Each venue's levels are already sorted, and its fee is constant, so its levels are
already in effective-price order. The planner keeps one "head" per venue (at most 8). At
each step it takes the head with the best effective price, consumes as much as it needs,
and moves on:
```rust
// key = effective price × 10^6, exact in i128 (no floating point, no rounding)
let key = match action {
    Action::Buy  =>   px * (1_000_000 + fee_ppm),
    Action::Sell => -(px * (1_000_000 - fee_ppm)),   // higher proceeds = smaller key
};
```
- **Cost:** O(levels consumed × venues) with only fixed-size arrays.
- **Ties:** go to the lower venue ID, so plans are deterministic.
- **A level can be partly used:** the last level only gives what's still needed.

### Fees: exact, in parts per million
- `VenueConfig::taker_fee_ppm`: Binance USDⓈ-M VIP 0 is 0.0500% = **500**, Bybit
  derivatives non-VIP is 0.0550% = **550**. These should be set from the account's actual
  tier; `set_taker_fee` changes them at run time.
- **Fees can change where to trade.** Tested: venue A asks 100.00 with a 0.06% fee,
  venue B asks 100.05 with no fee. Effective prices are 100.06 against 100.05, so the
  fee-aware plan buys from **B**, while the price-only plan would pick A.
- **Rounding always goes against us:** fees round **up**, a buy's average price rounds
  **up**, a sell's rounds **down**. A cost estimate should never look better than reality.
  The test `fees_are_exact_and_rounded_against_us` checks each by hand: 3 @ 1000 at 550 ppm
  is a fee of 1.65, which rounds up to 2.

### The output: one child order per venue
```rust
pub struct ExecutionPlan {
    pub action, pub requested, pub filled, pub gross: Notional, pub fees: Notional,
    pub partial: bool,             // visible liquidity couldn't fill `requested`
    pub may_have_more_depth: bool, // ...but some venue shows only part of its book
    children: [ChildOrder; 8],     // one per venue used
}
pub struct ChildOrder { venue, qty, limit_px /* worst level used there */, gross, fee, levels }
// methods: children(), worst_px(), vwap(), vwap_including_fees()
```
A router doesn't send one order per level; it sends one **IOC limit order per venue**
(immediate-or-cancel: fill what you can now, cancel the rest), priced at the worst level
it plans to take there. So that's the output. The Java skeleton's
`ExecutionPlan(List<Quote> quotes, BigDecimal totalQuantity)` maps onto `children()` and
`filled`.

**The Java skeleton's `Side` parameter is ambiguous.** Does
`calculateOptimalExecution(Side.BID, q)` mean "I want to buy" or "execute against the
bids"? Here it's `Action::Buy` / `Action::Sell`: no ambiguity.

### Mapping to the Java skeleton

| Java skeleton | Rust | Notes |
|---|---|---|
| `getAskPriceForVolume(v)` | `ask_price_for_volume(v, now)` | Buy `v` by price alone; `worst_px()` is "the price at which we can execute" |
| `getBidPriceForVolume(v)` | `bid_price_for_volume(v, now)` | Sell `v` by price alone |
| `calculateOptimalExecution(side, q)` | `optimal_execution(Action, q, now)` | Includes fees; one child order per venue |

### What greedy-optimal does *not* cover (stated in the code too)
- **Fixed per-order costs or minimum order sizes:** these break pure greedy. Both venues
  have small minimum notionals (around 5 USDT); a production router would drop tiny child
  orders.
- **The book changing while orders travel:** the plan is optimal for the book we see.
  Other traders race for the same levels during the milliseconds before our orders
  arrive. That's why the output uses IOC limits: we never pay worse than planned, but we
  may fill less.
- **Hidden liquidity and market impact beyond the visible book:** `may_have_more_depth`
  flags when the plan ran into the edge of what a venue shows.

## 2. Cross-venue arbitrage

### What it finds
On one venue, a crossed book is an error (step 3). **Across** venues it can be real: buy
where the ask is low, sell where the bid is high. `scan_arbitrage` checks every ordered
pair of healthy venues. For each pair it walks the sell venue's bids (highest first)
against the buy venue's asks (lowest first) **while each unit is still profitable after
both taker fees**:
```rust
// per-unit net edge × 10^6, exact
let edge = bid_px * (1_000_000 - sell_fee) - ask_px * (1_000_000 + buy_fee);
if edge <= 0 { break; }
```
It returns the quantity, both best prices, both limit prices, gross and net profit (net
rounded down), and the timestamp skew.

### The timing guard (the step 9 lesson)
Two venues' books are only compared if their exchange timestamps are within
`max_skew_ms` (default **150 ms**: Binance conflates updates to 100 ms, plus jitter).
Pairs outside that come back in `rejected_for_skew`, which is for logging and never for
acting on.

### What the real data showed: two separate layers of "not real"

| Layer | Result on the recordings |
|---|---|
| Raw prices | Bybit bid 58,656.2 > Binance ask 58,647.5: **$8.70 per BTC** |
| **Real taker fees** (0.05% + 0.055% ≈ **$61.60 per BTC**) | **Not profitable at all.** A cross must exceed ~0.105% just to pay a taker's fees on both sides |
| Fees switched off (test only) | "$5.57 on 0.952 BTC", but the books are **12,608 ms apart**, so it's **rejected** |
| Fees off **and** guard off | Reported as an opportunity: the mistake both layers exist to prevent |

The test `the_recorded_cross_is_neither_profitable_nor_simultaneous` asserts all three
layers. **I first wrote that test assuming the cross would survive fees, and it failed.**
Working out why gave the most useful number in this step: a cross between these two
venues needs about **0.105%** (roughly $62 at $58.6k) before a taker earns anything. Real
cross-venue arbitrage at these fee levels needs maker rebates, VIP fee tiers, or very
fast reactions to brief dislocations.

## 3. Results on the real books
```text
buy 1 BTC:  filled 1 of 1;  vwap 58647.5, with fees 58676.9; fees 29.3238 USDT; [binance 1 up to 58647.5 (1 levels)]
buy 10 BTC: filled 10 of 10; vwap 58648.2, with fees 58677.6; fees 293.241 USDT; [binance 10 up to 58650 (19 levels)]
```
Both route entirely to Binance, because its recorded book is cheaper. **But that's partly
the same timing artefact:** the Binance book was recorded ~12 s before the Bybit book,
when the price was lower. Live, with simultaneous books, the split would differ. The test
also asserts that the fee-aware plan's all-in cost is never worse than the price-only
plan's.

## 4. Testing summary

| Test | Covers |
|---|---|
| `execution.rs` (14) | Cheapest levels across venues; sell side with rounding down; partly used levels; partial plans, and the "may have more depth" flag; zero quantity / no venues; unhealthy venues skipped; ties; fees changing the venue; exact fee and average-price rounding; arbitrage walked level by level; fees shrinking and removing an arbitrage (with exact per-unit arithmetic); skew rejection at 151 ms; no cross → nothing, never within one venue |
| `optimality.rs`: brute force (3,000 cases) | No split of the order is cheaper than the plan, after fees, buys and sells |
| `optimality.rs`: reference (3,000 cases) | Up to 4 venues × 40 levels, with and without fees: per-venue child orders equal a flatten-sort-take reference; `partial` flag correct |
| `real_execution.rs` (2) | Routing 1 and 10 BTC on the real books; the three-layer arbitrage check |
| Planted bug | `optimal_execution` ignoring fees → the brute force finds a cheaper split |

## 5. Performance

Criterion, real books (Binance 20+20, Bybit 50+50), real fees, two runs:

| Operation | Time |
|---|---|
| `optimal_execution` buy 1 BTC (1 level) | **72 ns** |
| `optimal_execution` buy 10 BTC (19 levels) | 262–264 ns |
| `optimal_execution` buy 60 BTC (~all visible levels) | 723–731 ns |
| `ask_price_for_volume` 1 BTC (no fees) | 68–70 ns |
| `scan_arbitrage`, 2 venues | **25–27 ns** |

That's about **10 ns per level consumed**: every merge step recomputes each venue's
effective price with 128-bit multiplies. Caching each head's key and recomputing only the
one that advanced would cut this; that's noted for step 20. Even 60 BTC deep plans in
under a microsecond with no allocation.

## 6. Java counterpart
The skeleton:
```java
public record ExecutionPlan(List<Quote> quotes, BigDecimal totalQuantity) {}
public ExecutionPlan calculateOptimalExecution(Side side, BigDecimal targetQuantity) {
    throw new UnsupportedOperationException("To be implemented");
}
```
A typical Java answer: collect every level from every book into a `List`, sort it by
price with a `Comparator`, then loop with `BigDecimal` arithmetic. Compared with this step:
- **Allocation:** a list of all levels plus a sort, per call. Here it's a merge over
  already-sorted books, with nothing allocated.
- **Fees:** usually ignored, which gives the wrong venue in the "A 100.00 with fee vs
  B 100.05 without" case.
- **`Side`:** the ambiguity above.
- **Rounding:** `BigDecimal` division needs an explicit `RoundingMode` (and throws on
  non-terminating division without one). "Round against us" has to be chosen
  deliberately: `CEILING` for buy costs, `FLOOR` for sell proceeds.
- **Consistency:** the books are being mutated during the sort and loop. Here the plan
  runs on the aggregator's stable cached copies (step 9).

## How to verify it yourself
```bash
cd rust
cargo test -p ob-agg --release --test execution
cargo test -p ob-agg --release --test optimality          # brute-force optimality
cargo test -p ob-agg --release --test real_execution -- --nocapture   # prints the plans and the rejected cross
cargo bench -p ob-agg --bench agg -- execution
```
Things to try:
- In `execution.rs`, change `self.plan(action, qty, now_ns, true)` in
  `optimal_execution` to `false`. The brute-force test reports
  `greedy is not optimal` with a minimal counter-example.
- In `arbitrage.rs`, drop the sell-side fee from the edge (replace `(PPM - sell_fee)` with
  `PPM`). Then `fees_shrink_or_remove_the_opportunity` fails: an opportunity that fees
  should have removed is reported. (I first wrote a different suggestion here, changing
  `<= 0` to `< 0`. Checking it showed no test has an edge of exactly zero, so it wouldn't
  fail. I replaced it with this one, which I ran.)

## Likely interview questions
1. **How do you route an order optimally across venues?**
   Merge all venues' levels by effective price (price ± taker fee) and take the cheapest
   units first. That's optimal because the problem is a linear programme with one
   constraint (fractional knapsack). I verified it by brute force over every possible
   split on random books.
2. **When is greedy *not* optimal?**
   With fixed per-order costs, minimum order sizes, or quantity-dependent fee tiers. Also,
   in reality, when the book changes before your orders arrive. Hence IOC limit orders at
   the planned worst price.
3. **How do fees change routing?**
   Effective price is what matters: 100.00 at 0.06% loses to 100.05 at 0%. Tested.
4. **How do you round money?**
   Exactly in integers (`i128`) until the last step, then against yourself: costs and fees
   up, proceeds down.
5. **You see Bybit's bid above Binance's ask. Is it arbitrage?**
   Check three things:
   - Is it **after fees**? This one wasn't: an $8.70 edge against about $61.60 of taker
     fees.
   - Were the books **simultaneous**? They were 12.6 s apart.
   - **How much** quantity is profitable, walking the levels?

   The scan does all three and reports skew-rejected crosses separately.
6. **What's the break-even cross between Binance and Bybit for a taker?**
   About 0.105%: Binance taker 0.05% plus Bybit taker 0.055%, around $62 per BTC at
   $58.6k.
7. **Why one child order per venue instead of per level?**
   That's what you actually send: an IOC limit at the worst planned level takes all the
   planned levels on that venue in one order.
