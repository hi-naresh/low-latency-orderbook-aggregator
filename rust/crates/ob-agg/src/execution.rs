//! Execution planning: how to buy or sell a quantity across venues at the best total cost.
//!
//! ## The rule
//! Every visible level on every healthy venue is a block of quantity at a fixed marginal
//! cost per unit: its price, plus (when fees are counted) that venue's taker fee. Buying
//! `Q` means choosing amounts `x_l` from each level, `0 ≤ x_l ≤ qty_l`, `Σ x_l = Q`,
//! minimising `Σ cost_l · x_l`.
//!
//! **Taking the cheapest units first is optimal.** That's the fractional knapsack /
//! single-constraint linear programme: if an optimal plan bought a unit at cost `c` while
//! a cheaper unit `c' < c` was left untaken, swapping them would lower the total. So the
//! plan is a greedy merge of all venues' levels in order of effective price. Levels on one
//! venue are already sorted and its fee is constant, so the merge only compares the
//! current level of each venue (at most 8): O(levels taken × venues), no sorting, no
//! allocation.
//!
//! ## What the rule assumes (and the real world may not give)
//! * Fees are proportional to notional (true for taker fees; fixed per-order costs or
//!   minimum order sizes would break pure greedy).
//! * The visible book is what you'll get: no hidden liquidity, and nothing changes while
//!   orders travel to the venues (a few ms at best; others are racing for the same levels).
//! * Quantities requested are multiples of the instrument's lot; levels already are.
//!
//! ## Output
//! One **child order per venue**: how much to send there and the limit price (the worst
//! level used on that venue). That's what a router actually sends: an IOC limit order per
//! venue, not one order per level.

use ob_core::{Level, Notional, Px, Qty};

use crate::aggregator::Aggregator;
use crate::quotes::{MAX_VENUES, VenueId};

/// Buy (take liquidity from asks) or sell (take from bids).
///
/// The Java skeleton's `calculateOptimalExecution(Side side, ...)` is ambiguous: does
/// `BID` mean "I want to buy" or "execute against the bids"? An explicit action removes
/// the question.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Buy: consume asks, cheapest first.
    Buy,
    /// Sell: consume bids, highest first.
    Sell,
}

/// Fees are expressed in parts per million of notional.
pub const PPM: i128 = 1_000_000;

/// One venue's share of a plan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChildOrder {
    /// Where to send it.
    pub venue: VenueId,
    /// How much.
    pub qty: Qty,
    /// Limit price: the worst level used on this venue (an IOC at this price takes
    /// exactly the planned levels, if they're still there).
    pub limit_px: Px,
    /// Σ price × quantity over the levels used (before fees).
    pub gross: Notional,
    /// Taker fee on `gross`, rounded **up** (estimates of cost err on the safe side).
    pub fee: Notional,
    /// Number of levels used.
    pub levels: usize,
}

const NO_CHILD: ChildOrder = ChildOrder {
    venue: VenueId(0),
    qty: Qty::ZERO,
    limit_px: Px::ZERO,
    gross: Notional::from_raw(0),
    fee: Notional::from_raw(0),
    levels: 0,
};

/// The result of planning an order. Fixed size: nothing allocated.
#[derive(Clone, Copy, Debug)]
pub struct ExecutionPlan {
    /// Buy or sell.
    pub action: Action,
    /// What was asked for.
    pub requested: Qty,
    /// What the visible books can fill (≤ `requested`).
    pub filled: Qty,
    /// Σ gross over children.
    pub gross: Notional,
    /// Σ fees over children (0 if fees weren't counted).
    pub fees: Notional,
    /// `true` if the visible liquidity on healthy venues couldn't fill `requested`.
    pub partial: bool,
    /// `true` if the plan used *every* visible level on some venue whose stream shows
    /// only part of its book: there may be more liquidity than we can see there.
    pub may_have_more_depth: bool,
    children: [ChildOrder; MAX_VENUES],
    len: usize,
}

impl ExecutionPlan {
    fn empty(action: Action, requested: Qty) -> Self {
        ExecutionPlan {
            action,
            requested,
            filled: Qty::ZERO,
            gross: Notional::from_raw(0),
            fees: Notional::from_raw(0),
            partial: false,
            may_have_more_depth: false,
            children: [NO_CHILD; MAX_VENUES],
            len: 0,
        }
    }

    /// One child order per venue used, in venue order.
    pub fn children(&self) -> &[ChildOrder] {
        &self.children[..self.len]
    }

    /// The worst price reached across all venues (the Java `get*PriceForVolume` answer:
    /// the price you have to accept to fill this much).
    pub fn worst_px(&self) -> Option<Px> {
        let prices = self.children().iter().map(|c| c.limit_px);
        match self.action {
            Action::Buy => prices.max(),
            Action::Sell => prices.min(),
        }
    }

    /// Volume-weighted average price before fees, rounded against us (up for a buy, down
    /// for a sell). `None` if nothing fills.
    pub fn vwap(&self) -> Option<Px> {
        self.average(self.gross.raw())
    }

    /// Average price including fees: what a buy really costs per unit, or a sell really
    /// earns. Rounded against us.
    pub fn vwap_including_fees(&self) -> Option<Px> {
        let total = match self.action {
            Action::Buy => self.gross.raw() + self.fees.raw(),
            Action::Sell => self.gross.raw() - self.fees.raw(),
        };
        self.average(total)
    }

    fn average(&self, notional: i128) -> Option<Px> {
        let q = i128::from(self.filled.raw());
        if q == 0 {
            return None;
        }
        // Notional units are price units × quantity units, so dividing by the quantity
        // gives price units; round against us.
        let avg = match self.action {
            Action::Buy => (notional + q - 1).div_euclid(q),
            Action::Sell => notional.div_euclid(q),
        };
        i64::try_from(avg).ok().map(Px::from_raw)
    }
}

/// Per-venue running totals while planning.
#[derive(Clone, Copy)]
struct Acc {
    qty: i64,
    gross: i128,
    limit_px: Px,
    levels: usize,
}

impl Aggregator {
    /// The cheapest way to buy, or the best way to sell, `qty` across healthy venues,
    /// **including each venue's taker fee** (see the module docs for why greedy is
    /// optimal). The Java skeleton's `calculateOptimalExecution`.
    pub fn optimal_execution(&self, action: Action, qty: Qty, now_ns: u64) -> ExecutionPlan {
        self.plan(action, qty, now_ns, true)
    }

    /// Buying `qty` by price alone (no fees): the levels you'd take and the worst price
    /// reached. The Java skeleton's `getAskPriceForVolume`.
    pub fn ask_price_for_volume(&self, qty: Qty, now_ns: u64) -> ExecutionPlan {
        self.plan(Action::Buy, qty, now_ns, false)
    }

    /// Selling `qty` by price alone (no fees). The Java skeleton's `getBidPriceForVolume`.
    pub fn bid_price_for_volume(&self, qty: Qty, now_ns: u64) -> ExecutionPlan {
        self.plan(Action::Sell, qty, now_ns, false)
    }

    fn plan(&self, action: Action, qty: Qty, now_ns: u64, with_fees: bool) -> ExecutionPlan {
        let mut plan = ExecutionPlan::empty(action, qty);
        // One "head" per healthy venue: its levels on the side we take from, and how far
        // we've consumed them.
        let mut heads: [(VenueId, &[Level], i128, usize, bool); MAX_VENUES] =
            [(VenueId(0), &[], 0, 0, false); MAX_VENUES];
        let mut n = 0;
        for (id, snap, cfg) in self.included_with_config(now_ns) {
            let levels = match action {
                Action::Buy => snap.asks(),
                Action::Sell => snap.bids(),
            };
            let fee = if with_fees {
                i128::from(cfg.taker_fee_ppm)
            } else {
                0
            };
            let truncated = levels.len() >= cfg.depth; // the venue may have more than it shows
            if let Some(h) = heads.get_mut(n) {
                *h = (id, levels, fee, 0, truncated);
                n += 1;
            }
        }
        let heads = &mut heads[..n];

        let mut acc = [Acc {
            qty: 0,
            gross: 0,
            limit_px: Px::ZERO,
            levels: 0,
        }; MAX_VENUES];
        let mut remaining = qty.raw();
        while remaining > 0 {
            // The venue whose next level has the best effective price. Effective price ×
            // 10^6: buy = px·(10^6 + fee), sell = px·(10^6 − fee). Exact in i128. Ties go
            // to the lower venue ID (first found).
            let mut best: Option<(usize, i128)> = None;
            for (h, &(_, levels, fee, pos, _)) in heads.iter().enumerate() {
                let Some(level) = levels.get(pos) else {
                    continue;
                };
                let px = i128::from(level.px.raw());
                let key = match action {
                    Action::Buy => px * (PPM + fee),
                    Action::Sell => -(px * (PPM - fee)), // higher proceeds = smaller key
                };
                if best.is_none_or(|(_, k)| key < k) {
                    best = Some((h, key));
                }
            }
            let Some((h, _)) = best else { break }; // no liquidity left anywhere
            let (venue, levels, _, pos, _) = heads[h];
            let level = levels[pos];
            let take = remaining.min(level.qty.raw());
            let a = &mut acc[venue.index()];
            a.qty += take;
            a.gross += i128::from(level.px.raw()) * i128::from(take);
            a.limit_px = level.px;
            a.levels += 1;
            remaining -= take;
            if take == level.qty.raw() {
                heads[h].3 += 1; // level fully used: move to the venue's next level
            }
        }

        plan.partial = remaining > 0;
        plan.may_have_more_depth = plan.partial
            && heads
                .iter()
                .any(|&(_, levels, _, pos, truncated)| truncated && pos >= levels.len());
        let fee_of = |venue: VenueId| heads.iter().find(|h| h.0 == venue).map_or(0, |h| h.2);
        let (mut filled, mut gross, mut fees) = (0_i64, 0_i128, 0_i128);
        for (i, a) in acc.iter().enumerate() {
            if a.qty == 0 {
                continue;
            }
            let venue = VenueId::from_index(i);
            // Fee rounded up: never under-estimate a cost.
            let fee = (a.gross * fee_of(venue) + PPM - 1).div_euclid(PPM);
            plan.children[plan.len] = ChildOrder {
                venue,
                qty: Qty::from_raw(a.qty).unwrap_or(Qty::ZERO),
                limit_px: a.limit_px,
                gross: Notional::from_raw(a.gross),
                fee: Notional::from_raw(fee),
                levels: a.levels,
            };
            plan.len += 1;
            filled += a.qty;
            gross += a.gross;
            fees += fee;
        }
        plan.filled = Qty::from_raw(filled).unwrap_or(Qty::ZERO);
        plan.gross = Notional::from_raw(gross);
        plan.fees = Notional::from_raw(fees);
        plan
    }
}
