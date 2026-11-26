//! Cross-venue arbitrage: one venue's bid above another venue's ask, net of fees.
//!
//! On a single venue a crossed book means our copy is wrong (step 3). *Across* venues it
//! can be real: buy on the cheap venue, sell on the dear one. This scan finds it and
//! measures how much is actually there, walking both books level by level while the trade
//! is still profitable after both taker fees.
//!
//! **Timing guard.** Step 9's real data showed Bybit's bid $8.70 above Binance's ask, but
//! the two books were recorded 15 s apart. Comparing books from different moments makes
//! fake opportunities. So a pair is only reported as an opportunity if the books' exchange
//! timestamps are within [`crate::Config::max_skew_ms`]; otherwise it's reported
//! separately as rejected, so it can be logged but never acted on.

use ob_core::{Level, Notional, Px, Qty};
use ob_sync::Snapshot;

use crate::aggregator::Aggregator;
use crate::execution::PPM;
use crate::quotes::VenueId;

/// A cross-venue opportunity: buy on `buy_venue`, sell on `sell_venue`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Arbitrage {
    /// Where to buy (its asks are cheap).
    pub buy_venue: VenueId,
    /// Where to sell (its bids are high).
    pub sell_venue: VenueId,
    /// Best ask on the buy venue.
    pub buy_px: Px,
    /// Best bid on the sell venue.
    pub sell_px: Px,
    /// Worst ask that's still profitable to buy (the buy order's limit).
    pub buy_limit_px: Px,
    /// Worst bid that's still profitable to sell into (the sell order's limit).
    pub sell_limit_px: Px,
    /// How much can be bought and sold while every unit is profitable after fees.
    pub qty: Qty,
    /// Σ (bid − ask) × qty, before fees.
    pub gross_profit: Notional,
    /// After both taker fees, rounded down (profit estimates err on the safe side).
    pub net_profit: Notional,
    /// |exchange timestamp of one book − the other|, ms.
    pub skew_ms: u64,
}

/// The result of a scan.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ArbScan {
    /// The most profitable opportunity between books close enough in time to compare.
    pub best: Option<Arbitrage>,
    /// The most profitable apparent opportunity that was rejected because the books are
    /// too far apart in time. For logging: never act on it.
    pub rejected_for_skew: Option<Arbitrage>,
}

impl Aggregator {
    /// Scans every ordered pair of healthy venues for a net-of-fees cross.
    pub fn scan_arbitrage(&self, now_ns: u64) -> ArbScan {
        let mut scan = ArbScan::default();
        for (sell_id, sell_snap, sell_cfg) in self.included_with_config(now_ns) {
            for (buy_id, buy_snap, buy_cfg) in self.included_with_config(now_ns) {
                if sell_id == buy_id {
                    continue;
                }
                let fees = (
                    i128::from(sell_cfg.taker_fee_ppm),
                    i128::from(buy_cfg.taker_fee_ppm),
                );
                let Some(arb) = walk(sell_id, sell_snap, buy_id, buy_snap, fees) else {
                    continue;
                };
                let slot = if arb.skew_ms <= self.config().max_skew_ms {
                    &mut scan.best
                } else {
                    &mut scan.rejected_for_skew
                };
                if slot.is_none_or(|b| arb.net_profit > b.net_profit) {
                    *slot = Some(arb);
                }
            }
        }
        scan
    }
}

/// Walks the sell venue's bids (highest first) against the buy venue's asks (lowest
/// first) while each unit is profitable after both fees. `None` if not even the first
/// unit is.
fn walk(
    sell_id: VenueId,
    sell: &Snapshot,
    buy_id: VenueId,
    buy: &Snapshot,
    (sell_fee, buy_fee): (i128, i128),
) -> Option<Arbitrage> {
    let (bids, asks): (&[Level], &[Level]) = (sell.bids(), buy.asks());
    let (first_bid, first_ask) = (bids.first()?, asks.first()?);
    let (mut i, mut j) = (0, 0);
    let (mut bid_left, mut ask_left) = (first_bid.qty.raw(), first_ask.qty.raw());
    let (mut qty, mut gross, mut net_scaled) = (0_i64, 0_i128, 0_i128);
    let (mut sell_limit, mut buy_limit) = (first_bid.px, first_ask.px);
    while let (Some(bid), Some(ask)) = (bids.get(i), asks.get(j)) {
        // Net edge per unit × 10^6: what selling earns after its fee minus what buying
        // costs after its fee. Exact in i128.
        let edge = i128::from(bid.px.raw()) * (PPM - sell_fee)
            - i128::from(ask.px.raw()) * (PPM + buy_fee);
        if edge <= 0 {
            break;
        }
        let q = bid_left.min(ask_left);
        qty += q;
        gross += (i128::from(bid.px.raw()) - i128::from(ask.px.raw())) * i128::from(q);
        net_scaled += edge * i128::from(q);
        (sell_limit, buy_limit) = (bid.px, ask.px);
        bid_left -= q;
        ask_left -= q;
        if bid_left == 0 {
            i += 1;
            bid_left = bids.get(i).map_or(0, |l| l.qty.raw());
        }
        if ask_left == 0 {
            j += 1;
            ask_left = asks.get(j).map_or(0, |l| l.qty.raw());
        }
    }
    (qty > 0).then(|| Arbitrage {
        buy_venue: buy_id,
        sell_venue: sell_id,
        buy_px: first_ask.px,
        sell_px: first_bid.px,
        buy_limit_px: buy_limit,
        sell_limit_px: sell_limit,
        qty: Qty::from_raw(qty).unwrap_or(Qty::ZERO),
        gross_profit: Notional::from_raw(gross),
        net_profit: Notional::from_raw(net_scaled.div_euclid(PPM)),
        skew_ms: sell.meta.exchange_ts.abs_diff(buy.meta.exchange_ts),
    })
}
