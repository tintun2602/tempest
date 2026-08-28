//! Exchange-agnostic models and provider traits.
//!
//! Everything above this layer — strategy, risk, executor, backtest — speaks
//! only in these types. Venue wire formats (Binance's stringly-typed JSON) are
//! normalised by the adapters in this module's children, so no `serde_json`
//! parsing leaks into trading logic.
//!
//! Split into three narrow traits rather than one fat `Exchange` so a component
//! advertises exactly what it touches: the backtester needs market data and can
//! be handed something that cannot place an order at all.

pub mod binance;

use std::future::Future;

/// A single OHLCV bar. `open_time`/`close_time` are Unix epoch milliseconds.
#[derive(Debug, Clone, PartialEq)]
pub struct Candle {
    pub open_time: u64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: f64,
    pub close_time: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Buy,
    Sell,
}

impl Side {
    /// Binance wire representation.
    pub fn as_str(self) -> &'static str {
        match self {
            Side::Buy => "BUY",
            Side::Sell => "SELL",
        }
    }
}

/// One partial fill of an order, at a single price.
#[derive(Debug, Clone, PartialEq)]
pub struct Fill {
    pub price: f64,
    pub quantity: f64,
    /// Fee charged for this fill, denominated in `commission_asset`.
    pub commission: f64,
    pub commission_asset: String,
}

/// Normalised result of a market order that has already executed.
///
/// `average_price` is volume-weighted across `fills`. Callers use it as the
/// real entry/exit price rather than the pre-trade quote, so PnL and the
/// derived take-profit reflect what the venue actually gave us.
#[derive(Debug, Clone, PartialEq)]
pub struct OrderOutcome {
    pub symbol: String,
    pub side: Side,
    pub filled_quantity: f64,
    pub average_price: f64,
    pub fills: Vec<Fill>,
}

impl OrderOutcome {
    /// Total fee charged in `asset` across all fills.
    ///
    /// This matters for spot BUYs: Binance takes the fee out of the *base*
    /// asset, so the amount actually available to sell afterwards is
    /// `filled_quantity` minus this. Bracketing the gross quantity is rejected
    /// for insufficient balance, which would leave the position unprotected.
    pub fn commission_paid_in(&self, asset: &str) -> f64 {
        self.fills
            .iter()
            .filter(|f| f.commission_asset == asset)
            .map(|f| f.commission)
            .sum()
    }
}

/// The order types tempest cares about. Anything else the venue reports is
/// preserved verbatim under `Other` rather than being silently dropped —
/// protective-level recovery must not mistake an unknown type for "no stop".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrderKind {
    Market,
    Limit,
    LimitMaker,
    StopLoss,
    StopLossLimit,
    TakeProfit,
    TakeProfitLimit,
    Other(String),
}

impl OrderKind {
    pub fn from_wire(s: &str) -> Self {
        match s {
            "MARKET" => OrderKind::Market,
            "LIMIT" => OrderKind::Limit,
            "LIMIT_MAKER" => OrderKind::LimitMaker,
            "STOP_LOSS" => OrderKind::StopLoss,
            "STOP_LOSS_LIMIT" => OrderKind::StopLossLimit,
            "TAKE_PROFIT" => OrderKind::TakeProfit,
            "TAKE_PROFIT_LIMIT" => OrderKind::TakeProfitLimit,
            other => OrderKind::Other(other.to_string()),
        }
    }

    /// Whether this kind carries a stop trigger (its `stop_price` is the level).
    pub fn is_stop(&self) -> bool {
        matches!(self, OrderKind::StopLoss | OrderKind::StopLossLimit)
    }

    /// Whether this kind rests at a target price (its `price` is the level).
    pub fn is_target(&self) -> bool {
        matches!(
            self,
            OrderKind::LimitMaker
                | OrderKind::Limit
                | OrderKind::TakeProfit
                | OrderKind::TakeProfitLimit
        )
    }
}

/// An order resting on the exchange.
#[derive(Debug, Clone, PartialEq)]
pub struct OpenOrder {
    pub order_id: u64,
    pub symbol: String,
    pub side: Side,
    pub kind: OrderKind,
    /// Resting limit price. `0.0` when the kind has none.
    pub price: f64,
    /// Trigger price for stop kinds. `0.0` when the kind has none.
    pub stop_price: f64,
    pub quantity: f64,
}

/// Confirmation that an OCO bracket is live on the venue.
///
/// Only construct this from a venue response that actually acknowledged the
/// order list. A position must never be treated as protected without one.
#[derive(Debug, Clone, PartialEq)]
pub struct OcoPlacement {
    pub order_list_id: i64,
    pub stop_price: f64,
    pub take_profit_price: f64,
}

/// Confirmation that a standalone protective stop is live on the venue.
///
/// A trailing stop has no target leg, so it cannot be an OCO — Binance's order
/// list always pairs a stop with a take-profit. This is a lone
/// `STOP_LOSS_LIMIT` that ratchets upward as price advances.
#[derive(Debug, Clone, PartialEq)]
pub struct StopPlacement {
    pub order_id: u64,
    pub stop_price: f64,
}

/// A non-quote asset holding, `free + locked` so assets reserved by a resting
/// OCO still count toward equity.
#[derive(Debug, Clone, PartialEq)]
pub struct Balance {
    pub asset: String,
    pub quantity: f64,
}

/// Spot account snapshot: spendable quote currency plus every other non-zero
/// holding.
#[derive(Debug, Clone, PartialEq)]
pub struct AccountSnapshot {
    pub free_quote: f64,
    pub assets: Vec<Balance>,
}

/// The venue's trading rules for one symbol.
///
/// Orders that violate any of these are rejected outright (Binance `-1013`),
/// so they must be applied *before* an order is sent rather than discovered
/// from the error.
#[derive(Debug, Clone, PartialEq)]
pub struct SymbolFilters {
    pub symbol: String,
    pub base_asset: String,
    pub quote_asset: String,
    /// Quantity must be a whole multiple of this.
    pub step_size: f64,
    pub min_qty: f64,
    pub max_qty: f64,
    /// Price must be a whole multiple of this.
    pub tick_size: f64,
    /// Order value (price x quantity) must reach this.
    pub min_notional: f64,
}

/// Why an order cannot legally be sent.
#[derive(Debug, Clone, PartialEq)]
pub enum OrderRejection {
    /// Quantity rounds to zero, or is under the venue minimum.
    BelowMinQuantity {
        quantity: f64,
        min_qty: f64,
    },
    AboveMaxQuantity {
        quantity: f64,
        max_qty: f64,
    },
    /// Order value is under the venue minimum.
    BelowMinNotional {
        notional: f64,
        min_notional: f64,
    },
}

impl std::fmt::Display for OrderRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OrderRejection::BelowMinQuantity { quantity, min_qty } => write!(
                f,
                "quantity {quantity} is below the venue minimum {min_qty}"
            ),
            OrderRejection::AboveMaxQuantity { quantity, max_qty } => write!(
                f,
                "quantity {quantity} is above the venue maximum {max_qty}"
            ),
            OrderRejection::BelowMinNotional {
                notional,
                min_notional,
            } => write!(
                f,
                "order value {notional:.2} is below the venue minimum {min_notional:.2}"
            ),
        }
    }
}

impl SymbolFilters {
    /// Largest legal quantity not exceeding `quantity`.
    ///
    /// Always rounds **down**: rounding up could exceed the free balance, or
    /// try to sell more of the base asset than the fill actually delivered.
    pub fn round_quantity(&self, quantity: f64) -> f64 {
        round_down_to_increment(quantity, self.step_size)
    }

    /// Nearest legal price at or below `price`.
    pub fn round_price(&self, price: f64) -> f64 {
        round_down_to_increment(price, self.tick_size)
    }

    /// Whether an order of `quantity` at `price` satisfies every filter.
    /// `quantity` is expected to have been through [`Self::round_quantity`].
    pub fn check_order(&self, quantity: f64, price: f64) -> Result<(), OrderRejection> {
        if quantity < self.min_qty || quantity <= 0.0 {
            return Err(OrderRejection::BelowMinQuantity {
                quantity,
                min_qty: self.min_qty,
            });
        }
        if self.max_qty > 0.0 && quantity > self.max_qty {
            return Err(OrderRejection::AboveMaxQuantity {
                quantity,
                max_qty: self.max_qty,
            });
        }
        let notional = quantity * price;
        if notional < self.min_notional {
            return Err(OrderRejection::BelowMinNotional {
                notional,
                min_notional: self.min_notional,
            });
        }
        Ok(())
    }

    /// Smallest quantity that clears both the lot and notional minimums at
    /// `price`. Useful for reporting how far short an intended order fell.
    pub fn min_tradable_quantity(&self, price: f64) -> f64 {
        let by_notional = if price > 0.0 {
            round_up_to_increment(self.min_notional / price, self.step_size)
        } else {
            0.0
        };
        by_notional.max(self.min_qty)
    }
}

/// Round `value` down to a whole multiple of `increment`.
///
/// The epsilon matters: `0.00007 / 0.00001` evaluates to `6.999...` in binary
/// floating point, and a bare `floor` would silently drop a whole step.
fn round_down_to_increment(value: f64, increment: f64) -> f64 {
    if increment <= 0.0 || !value.is_finite() {
        return value;
    }
    let steps = value / increment;
    let steps = if (steps.round() - steps).abs() < 1e-9 {
        steps.round()
    } else {
        steps.floor()
    };
    snap(steps * increment, increment)
}

fn round_up_to_increment(value: f64, increment: f64) -> f64 {
    if increment <= 0.0 || !value.is_finite() {
        return value;
    }
    let steps = value / increment;
    let steps = if (steps.round() - steps).abs() < 1e-9 {
        steps.round()
    } else {
        steps.ceil()
    };
    snap(steps * increment, increment)
}

/// Clear binary-float dust so the value prints exactly at the increment's
/// precision (`0.00011999999999` -> `0.00012`).
fn snap(value: f64, increment: f64) -> f64 {
    let decimals = decimals_for(increment);
    let factor = 10f64.powi(decimals as i32);
    (value * factor).round() / factor
}

/// Decimal places implied by an increment: `0.00001` -> 5, `1` -> 0.
pub fn decimals_for(increment: f64) -> usize {
    if increment <= 0.0 {
        return 8;
    }
    let text = format!("{increment:.8}");
    let text = text.trim_end_matches('0');
    match text.find('.') {
        Some(dot) => text.len() - dot - 1,
        None => 0,
    }
}

/// Venue trading rules per symbol.
pub trait InstrumentProvider {
    fn filters(&self, symbol: &str) -> impl Future<Output = Result<SymbolFilters, String>> + Send;
}

/// Read-only price and candle history.
pub trait MarketDataProvider {
    /// Recent candles for `symbol`, oldest first. `interval` is venue notation
    /// ("1d", "4h").
    fn klines(
        &self,
        symbol: &str,
        interval: &str,
        limit: u32,
    ) -> impl Future<Output = Result<Vec<Candle>, String>> + Send;

    /// Extended history, paginating backwards until `total` candles are
    /// collected or the venue runs out. Oldest first.
    fn klines_extended(
        &self,
        symbol: &str,
        interval: &str,
        total: usize,
    ) -> impl Future<Output = Result<Vec<Candle>, String>> + Send;

    /// Last traded price.
    fn price(&self, symbol: &str) -> impl Future<Output = Result<f64, String>> + Send;
}

/// Read-only account state.
pub trait AccountProvider {
    fn account(
        &self,
        quote_asset: &str,
    ) -> impl Future<Output = Result<AccountSnapshot, String>> + Send;
}

/// Order placement and cancellation.
///
/// Deliberately models `place_oco_sell` as its own operation rather than a
/// generic "open order" call: Binance OCO is a single atomic order-list
/// request, and splitting it into two independent orders would leave a window
/// where a position carries a target but no stop.
pub trait ExecutionProvider {
    fn market_order(
        &self,
        symbol: &str,
        side: Side,
        quantity: f64,
    ) -> impl Future<Output = Result<OrderOutcome, String>> + Send;

    /// Place a protective bracket over an existing long: take-profit limit
    /// above, stop-loss limit below. Resolves only once the venue confirms.
    fn place_oco_sell(
        &self,
        symbol: &str,
        quantity: f64,
        take_profit_price: f64,
        stop_price: f64,
        stop_limit_price: f64,
    ) -> impl Future<Output = Result<OcoPlacement, String>> + Send;

    /// Place a standalone protective stop, with no target leg.
    ///
    /// Used by the trailing stop, where capping the upside is exactly what we
    /// are trying to avoid.
    fn place_stop_loss(
        &self,
        symbol: &str,
        quantity: f64,
        stop_price: f64,
        stop_limit_price: f64,
    ) -> impl Future<Output = Result<StopPlacement, String>> + Send;

    fn cancel_open_orders(&self, symbol: &str) -> impl Future<Output = Result<(), String>> + Send;

    fn open_orders(
        &self,
        symbol: &str,
    ) -> impl Future<Output = Result<Vec<OpenOrder>, String>> + Send;
}

/// Extract `(stop_loss, take_profit)` from a symbol's resting sell orders.
///
/// An OCO sell is two orders: one holding the target and one whose trigger is
/// the stop. A level that cannot be found is returned as `0.0`, which
/// `RiskManager::check_exits` treats as unset — registering a real `0.0` target
/// would read as "price >= take_profit" and liquidate the position immediately.
pub fn protective_levels(orders: &[OpenOrder]) -> (f64, f64) {
    let mut stop_loss = 0.0;
    let mut take_profit = 0.0;

    for order in orders {
        if order.side != Side::Sell {
            continue;
        }
        if order.kind.is_stop() && order.stop_price > 0.0 {
            stop_loss = order.stop_price;
        } else if order.kind.is_target() && order.price > 0.0 {
            take_profit = order.price;
        }
    }

    (stop_loss, take_profit)
}

/// Volume-weighted average price across `fills`.
pub fn weighted_average_price(fills: &[Fill]) -> Option<f64> {
    let mut cost = 0.0;
    let mut qty = 0.0;
    for fill in fills {
        cost += fill.price * fill.quantity;
        qty += fill.quantity;
    }
    (qty > 0.0).then(|| cost / qty)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sell(kind: OrderKind, price: f64, stop_price: f64) -> OpenOrder {
        OpenOrder {
            order_id: 1,
            symbol: "BTCUSDC".into(),
            side: Side::Sell,
            kind,
            price,
            stop_price,
            quantity: 0.1,
        }
    }

    #[test]
    fn protective_levels_from_oco() {
        let orders = vec![
            sell(OrderKind::LimitMaker, 62_000.0, 0.0),
            sell(OrderKind::StopLossLimit, 56_888.0, 57_000.0),
        ];
        let (sl, tp) = protective_levels(&orders);
        assert!((sl - 57_000.0).abs() < 1e-9);
        assert!((tp - 62_000.0).abs() < 1e-9);
    }

    #[test]
    fn protective_levels_ignores_buy_orders() {
        // A stray buy carries no protection — both levels stay unset so
        // `check_exits` leaves the position to the exchange.
        let orders = vec![OpenOrder {
            side: Side::Buy,
            ..sell(OrderKind::Limit, 50_000.0, 0.0)
        }];
        assert_eq!(protective_levels(&orders), (0.0, 0.0));
    }

    #[test]
    fn protective_levels_empty() {
        assert_eq!(protective_levels(&[]), (0.0, 0.0));
    }

    #[test]
    fn protective_levels_ignores_unknown_kind() {
        // An unrecognised type must not be mistaken for a stop at 0.0.
        let orders = vec![sell(OrderKind::Other("TRAILING_STOP".into()), 100.0, 90.0)];
        assert_eq!(protective_levels(&orders), (0.0, 0.0));
    }

    #[test]
    fn order_kind_round_trips_known_wire_values() {
        assert_eq!(OrderKind::from_wire("LIMIT_MAKER"), OrderKind::LimitMaker);
        assert_eq!(
            OrderKind::from_wire("STOP_LOSS_LIMIT"),
            OrderKind::StopLossLimit
        );
        assert!(OrderKind::from_wire("STOP_LOSS").is_stop());
        assert!(OrderKind::from_wire("TAKE_PROFIT").is_target());
        assert_eq!(
            OrderKind::from_wire("MYSTERY"),
            OrderKind::Other("MYSTERY".into())
        );
    }

    fn fill(price: f64, quantity: f64) -> Fill {
        Fill {
            price,
            quantity,
            commission: 0.0,
            commission_asset: "BNB".into(),
        }
    }

    #[test]
    fn weighted_average_price_is_volume_weighted() {
        // (100*1 + 200*3) / 4 = 175
        assert_eq!(
            weighted_average_price(&[fill(100.0, 1.0), fill(200.0, 3.0)]),
            Some(175.0)
        );
    }

    #[test]
    fn weighted_average_price_none_when_nothing_filled() {
        assert_eq!(weighted_average_price(&[]), None);
        assert_eq!(weighted_average_price(&[fill(100.0, 0.0)]), None);
    }

    // ----- exchange filters -----

    /// The real BTCUSDC rules, read from Binance exchangeInfo.
    fn btcusdc() -> SymbolFilters {
        SymbolFilters {
            symbol: "BTCUSDC".into(),
            base_asset: "BTC".into(),
            quote_asset: "USDC".into(),
            step_size: 0.00001,
            min_qty: 0.00001,
            max_qty: 9000.0,
            tick_size: 0.01,
            min_notional: 5.0,
        }
    }

    #[test]
    fn quantity_rounds_down_to_the_step_size() {
        let f = btcusdc();
        // Six-decimal sizing output is not a legal multiple of a 1e-5 step.
        assert_eq!(f.round_quantity(0.000123), 0.00012);
        assert_eq!(f.round_quantity(0.056103), 0.0561);
        assert_eq!(f.round_quantity(0.090946), 0.09094);
    }

    #[test]
    fn quantity_already_on_a_step_boundary_is_unchanged() {
        // 0.00007 / 0.00001 is 6.999... in binary floating point; a naive floor
        // would round a valid quantity down to 0.00006.
        let f = btcusdc();
        assert_eq!(f.round_quantity(0.00007), 0.00007);
        assert_eq!(f.round_quantity(0.00003), 0.00003);
        assert_eq!(f.round_quantity(0.29), 0.29);
    }

    #[test]
    fn quantity_below_one_step_rounds_to_zero() {
        assert_eq!(btcusdc().round_quantity(0.000004), 0.0);
    }

    #[test]
    fn price_rounds_down_to_the_tick_size() {
        let f = btcusdc();
        assert_eq!(f.round_price(77_214.249), 77_214.24);
        assert_eq!(f.round_price(77_214.24), 77_214.24);
    }

    #[test]
    fn price_rounding_handles_coarse_and_fine_ticks() {
        // A venue tick coarser than a cent: 2-decimal formatting would emit an
        // illegal price.
        let coarse = SymbolFilters {
            tick_size: 0.1,
            ..btcusdc()
        };
        assert_eq!(coarse.round_price(123.45), 123.4);
        // And a sub-cent tick, where 2 decimals would destroy the price.
        let fine = SymbolFilters {
            tick_size: 0.00000001,
            ..btcusdc()
        };
        assert_eq!(fine.round_price(0.00001234), 0.00001234);
    }

    #[test]
    fn order_below_min_notional_is_rejected() {
        let f = btcusdc();
        // 0.00003 BTC at 77,214 is ~2.32 USDC — under the 5.00 minimum.
        let err = f.check_order(0.00003, 77_214.24).unwrap_err();
        assert!(matches!(err, OrderRejection::BelowMinNotional { .. }));
        assert!(err.to_string().contains("below the venue minimum"));
    }

    #[test]
    fn order_below_min_quantity_is_rejected() {
        let f = btcusdc();
        assert!(matches!(
            f.check_order(0.0, 77_214.24).unwrap_err(),
            OrderRejection::BelowMinQuantity { .. }
        ));
    }

    #[test]
    fn order_meeting_every_filter_is_accepted() {
        // 0.00012 BTC at 77,214 is ~9.27 USDC — clears both minimums.
        assert!(btcusdc().check_order(0.00012, 77_214.24).is_ok());
    }

    #[test]
    fn min_tradable_quantity_clears_the_notional_floor() {
        let f = btcusdc();
        let price = 77_214.24;
        let min = f.min_tradable_quantity(price);
        // 5.00 / 77214.24 = 0.0000647..., which must round *up* to a step.
        assert_eq!(min, 0.00007);
        assert!(f.check_order(min, price).is_ok());
        // One step less must fail, or the floor is not tight.
        assert!(f.check_order(min - f.step_size, price).is_err());
    }

    #[test]
    fn decimals_are_derived_from_the_increment() {
        assert_eq!(decimals_for(0.00001), 5);
        assert_eq!(decimals_for(0.01), 2);
        assert_eq!(decimals_for(1.0), 0);
        assert_eq!(decimals_for(0.00000001), 8);
    }

    #[test]
    fn commission_is_summed_per_asset() {
        let outcome = OrderOutcome {
            symbol: "BTCUSDC".into(),
            side: Side::Buy,
            filled_quantity: 0.00012,
            average_price: 77_214.24,
            fills: vec![
                Fill {
                    price: 77_214.24,
                    quantity: 0.00008,
                    commission: 0.00000008,
                    commission_asset: "BTC".into(),
                },
                Fill {
                    price: 77_215.00,
                    quantity: 0.00004,
                    commission: 0.00000004,
                    commission_asset: "BTC".into(),
                },
            ],
        };
        assert!((outcome.commission_paid_in("BTC") - 0.00000012).abs() < 1e-12);
        // Fees paid in another asset do not reduce the base holding.
        assert_eq!(outcome.commission_paid_in("BNB"), 0.0);
    }
}
