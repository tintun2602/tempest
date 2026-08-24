//! Trading costs: exchange fees and market-order slippage.
//!
//! Shared by the backtester and (later) paper trading, so a simulated fill is
//! priced the same way wherever it happens.

use std::env;

/// One basis point.
const BPS: f64 = 0.0001;

/// Binance spot standard tier: 0.1% maker and taker.
const DEFAULT_FEE_BPS: f64 = 10.0;
/// Market orders cross the spread. Small relative to fees on a liquid pair,
/// but it compounds with frequency.
const DEFAULT_SLIPPAGE_BPS: f64 = 5.0;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CostModel {
    /// Fee on orders that take liquidity (market, triggered stop), as a fraction.
    pub taker_fee: f64,
    /// Fee on orders that rest (the OCO take-profit leg), as a fraction.
    pub maker_fee: f64,
    /// Adverse price movement on a market order, as a fraction.
    pub slippage: f64,
}

impl Default for CostModel {
    fn default() -> Self {
        Self {
            taker_fee: DEFAULT_FEE_BPS * BPS,
            maker_fee: DEFAULT_FEE_BPS * BPS,
            slippage: DEFAULT_SLIPPAGE_BPS * BPS,
        }
    }
}

impl CostModel {
    pub fn from_env() -> Self {
        let bps = |key: &str, fallback: f64| {
            env::var(key)
                .ok()
                .and_then(|v| v.parse::<f64>().ok())
                .unwrap_or(fallback)
                * BPS
        };
        Self {
            taker_fee: bps("TAKER_FEE_BPS", DEFAULT_FEE_BPS),
            maker_fee: bps("MAKER_FEE_BPS", DEFAULT_FEE_BPS),
            slippage: bps("SLIPPAGE_BPS", DEFAULT_SLIPPAGE_BPS),
        }
    }

    /// Zero costs — for isolating strategy behaviour from execution drag.
    pub fn frictionless() -> Self {
        Self {
            taker_fee: 0.0,
            maker_fee: 0.0,
            slippage: 0.0,
        }
    }

    /// Price actually paid by a market BUY quoted at `price`.
    pub fn buy_fill(&self, price: f64) -> f64 {
        price * (1.0 + self.slippage)
    }

    /// Price actually received by a market SELL quoted at `price`.
    ///
    /// Also the right model for a triggered stop-loss: the stop becomes a
    /// marketable order and crosses the book.
    pub fn sell_fill(&self, price: f64) -> f64 {
        price * (1.0 - self.slippage)
    }

    /// A resting take-profit fills at its limit price, by definition — it is
    /// only touched when the market comes to it.
    pub fn limit_fill(&self, price: f64) -> f64 {
        price
    }

    pub fn taker_cost(&self, notional: f64) -> f64 {
        notional.abs() * self.taker_fee
    }

    pub fn maker_cost(&self, notional: f64) -> f64 {
        notional.abs() * self.maker_fee
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model() -> CostModel {
        // 0.1% fees, 5bps slippage.
        CostModel::default()
    }

    #[test]
    fn slippage_always_moves_against_the_trader() {
        let m = model();
        assert!(m.buy_fill(100.0) > 100.0, "a buy pays up");
        assert!(m.sell_fill(100.0) < 100.0, "a sell receives less");
        assert_eq!(m.buy_fill(100.0), 100.05);
        assert_eq!(m.sell_fill(100.0), 99.95);
    }

    #[test]
    fn resting_limit_orders_do_not_slip() {
        // The take-profit leg is only filled when price reaches it.
        assert_eq!(model().limit_fill(52_000.0), 52_000.0);
    }

    #[test]
    fn fees_scale_with_notional() {
        let m = model();
        assert!((m.taker_cost(10_000.0) - 10.0).abs() < 1e-9);
        assert!((m.maker_cost(10_000.0) - 10.0).abs() < 1e-9);
    }

    #[test]
    fn fees_are_never_negative_for_a_short_notional() {
        assert!(model().taker_cost(-10_000.0) > 0.0);
    }

    #[test]
    fn frictionless_model_is_free() {
        let m = CostModel::frictionless();
        assert_eq!(m.buy_fill(100.0), 100.0);
        assert_eq!(m.sell_fill(100.0), 100.0);
        assert_eq!(m.taker_cost(10_000.0), 0.0);
    }

    #[test]
    fn round_trip_drag_is_the_sum_of_both_sides() {
        // 0.1% in, 0.1% out, plus 5bps slippage each way = 30bps to overcome
        // before a trade is profitable.
        let m = model();
        let quoted = 100.0;
        let entry = m.buy_fill(quoted);
        let exit = m.sell_fill(quoted);
        let qty = 1.0;
        let drag = (entry - exit) * qty + m.taker_cost(entry * qty) + m.taker_cost(exit * qty);
        assert!((drag / quoted - 0.0030).abs() < 1e-4, "drag was {drag}");
    }
}
