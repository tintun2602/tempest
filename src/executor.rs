//! Order execution: turns a `TradeSignal` into venue orders and keeps the
//! `RiskManager`'s view of open positions in step with what actually filled.
//!
//! Generic over [`ExecutionProvider`], so the same code drives the live Binance
//! REST client, a paper-trading simulator, or a test double.

use crate::exchange::{ExecutionProvider, InstrumentProvider, Side, SymbolFilters};
use crate::notify::Notifier;
use crate::risk::{Position, RiskManager};
use crate::strategy::{StrategyParams, TradeSignal};
use tracing::{error, info, warn};

/// Stop-limit price is placed just under the trigger so the resting limit still
/// crosses the book when the stop fires.
const STOP_LIMIT_SLIP: f64 = 0.998;

/// Reward-to-risk applied to the realised fill when setting the target.
const REWARD_RISK_RATIO: f64 = 2.0;

/// The trailing stop is only moved once it has advanced this many ATR, so a
/// position is not cancel-and-replaced on every poll for a few ticks of gain.
/// Each move opens a window where nothing protects the position.
const RATCHET_MIN_ATR: f64 = 0.25;

pub struct Executor<'a, E> {
    client: &'a E,
    notifier: &'a Notifier,
}

impl<'a, E: ExecutionProvider + InstrumentProvider> Executor<'a, E> {
    pub fn new(client: &'a E, notifier: &'a Notifier) -> Self {
        Self { client, notifier }
    }

    /// Execute a BUY: market-buy, then bracket it with a protective OCO sell.
    ///
    /// The position is registered either way — if the OCO is rejected the
    /// bot's own `check_exits` is the only stop it has, so it must still be
    /// tracked — but it is only flagged `protected` once the venue confirms.
    pub async fn execute_buy(
        &self,
        signal: &TradeSignal,
        quantity: f64,
        risk_manager: &mut RiskManager,
        params: &StrategyParams,
    ) -> Result<(), String> {
        info!(
            "BUY {} | qty {:.6} | entry ~{:.2} | SL {:.2} | TP {:.2}",
            signal.asset, quantity, signal.entry_price, signal.stop_loss, signal.take_profit
        );

        // Venue rules first: an order that violates a lot or notional filter is
        // rejected outright, so it must be corrected before it is sent rather
        // than discovered from a -1013 error.
        let filters = self.client.filters(&signal.asset).await?;
        let quantity = filters.round_quantity(quantity);
        if let Err(rejection) = filters.check_order(quantity, signal.entry_price) {
            let floor = filters.min_tradable_quantity(signal.entry_price);
            return Err(format!(
                "{}: {rejection}; the smallest tradable size here is {floor} {}                  (~{:.2} {})",
                signal.asset,
                filters.base_asset,
                floor * signal.entry_price,
                filters.quote_asset,
            ));
        }

        let outcome = self
            .client
            .market_order(&signal.asset, Side::Buy, quantity)
            .await?;

        // A venue that reports no usable fill price leaves the pre-trade quote
        // as the only reference we have.
        let fill_price = if outcome.average_price > 0.0 {
            outcome.average_price
        } else {
            warn!(
                "{}: no fill price in order response, using signal entry {:.2}",
                signal.asset, signal.entry_price
            );
            signal.entry_price
        };
        let executed_qty = outcome.filled_quantity;

        info!(
            "BUY filled: {} @ {:.2} qty {:.6}",
            signal.asset, fill_price, executed_qty
        );

        // Re-derive the target from the realised fill, keeping the stop where
        // the strategy put it (the swing low is a structural level, not an
        // offset from our entry).
        let stop_loss = signal.stop_loss;
        let take_profit = fill_price + REWARD_RISK_RATIO * (fill_price - stop_loss);

        // Binance charges the spot BUY fee in the base asset, so less was
        // received than was filled. Bracketing the gross amount is rejected for
        // insufficient balance — which would leave the position unprotected.
        let commission = outcome.commission_paid_in(&filters.base_asset);
        let sellable = filters.round_quantity(executed_qty - commission);
        if commission > 0.0 {
            info!(
                "{}: {commission} {} taken as fee; bracketing {sellable} of {executed_qty}",
                signal.asset, filters.base_asset
            );
        }

        let stop_loss = filters.round_price(stop_loss);
        let stop_limit = filters.round_price(stop_loss * STOP_LIMIT_SLIP);
        let trailing = params.trailing_stop_atr > 0.0 && signal.atr > 0.0;

        // A trailing stop has no target: capping the upside is exactly what it
        // exists to avoid. `0.0` is the established "unset" sentinel that
        // `check_exits` ignores.
        let take_profit = if trailing {
            0.0
        } else {
            filters.round_price(take_profit)
        };

        let placement = if trailing {
            self.client
                .place_stop_loss(&signal.asset, sellable, stop_loss, stop_limit)
                .await
                .map(|stop| format!("stop {}", stop.order_id))
        } else {
            self.oco_or_reason(
                &signal.asset,
                &filters,
                sellable,
                take_profit,
                stop_loss,
                stop_limit,
            )
            .await
            .map(|oco| format!("OCO list {}", oco.order_list_id))
        };

        let protected = match placement {
            Ok(reference) => {
                info!(
                    "{} protected: {reference} (SL {stop_loss:.2})",
                    signal.asset
                );
                true
            }
            Err(e) => {
                error!(
                    "{}: protective order FAILED — position is UNPROTECTED: {e}",
                    signal.asset
                );
                self.notifier
                    .notify_error(
                        &format!("UNPROTECTED {}", signal.asset),
                        &format!("Protective order rejected: {e}"),
                    )
                    .await;
                false
            }
        };

        risk_manager.open_position(Position {
            symbol: signal.asset.clone(),
            entry_price: fill_price,
            quantity: executed_qty,
            stop_loss,
            take_profit,
            entry_time: now_ms(),
            protected,
            highest_high: fill_price,
            atr_at_entry: if trailing { signal.atr } else { 0.0 },
        });

        self.notifier
            .notify_buy(
                &signal.asset,
                fill_price,
                executed_qty,
                stop_loss,
                take_profit,
                &signal.confidence,
                &signal.reasoning,
            )
            .await;

        Ok(())
    }

    /// Close an existing position: cancel the resting OCO, then market-sell.
    pub async fn execute_sell(
        &self,
        symbol: &str,
        risk_manager: &mut RiskManager,
    ) -> Result<(), String> {
        let position = risk_manager
            .close_position(symbol)
            .ok_or_else(|| format!("No open position for {symbol}"))?;

        info!("SELL {} | qty {:.6}", symbol, position.quantity);

        // The OCO reserves the base asset; it must go before the market sell
        // can spend it.
        if let Err(e) = self.client.cancel_open_orders(symbol).await {
            warn!("Failed to cancel open orders for {symbol}: {e}");
        }

        // The tracked quantity can carry dust from fees or a reconciled
        // balance; the venue only accepts whole steps.
        let filters = self.client.filters(symbol).await?;
        let sell_qty = filters.round_quantity(position.quantity);
        if let Err(rejection) = filters.check_order(sell_qty, position.entry_price) {
            warn!("{symbol}: position may be unsellable — {rejection}");
        }

        match self.client.market_order(symbol, Side::Sell, sell_qty).await {
            Ok(outcome) => {
                let sell_price = outcome.average_price;
                if sell_price <= 0.0 {
                    warn!("{symbol}: could not determine sell fill price; PnL will read as 0");
                }
                let pnl = (sell_price - position.entry_price) * position.quantity;
                let pnl_pct = (sell_price - position.entry_price) / position.entry_price * 100.0;
                info!(
                    "SELL filled: {symbol} @ {sell_price:.2} | PnL {pnl:.2} {}                      ({pnl_pct:+.2}%) | held {}",
                    self.notifier.quote_asset(),
                    format_holding_period(position.entry_time, now_ms())
                );
                self.notifier
                    .notify_sell(symbol, sell_price, pnl, pnl_pct)
                    .await;
                Ok(())
            }
            // The position is already out of the risk manager at this point.
            // Put it back so the next cycle retries rather than losing track of
            // base asset the bot still holds.
            Err(e) => {
                error!("{symbol}: market sell failed, restoring tracked position");
                risk_manager.open_position(position);
                Err(format!("Market sell failed for {symbol}: {e}"))
            }
        }
    }

    /// Place the protective bracket, refusing to send one the venue is certain
    /// to reject.
    ///
    /// An OCO is two orders, and *both* must clear the notional minimum. The
    /// stop leg is the binding one, since it sits at the lower price.
    async fn oco_or_reason(
        &self,
        symbol: &str,
        filters: &SymbolFilters,
        quantity: f64,
        take_profit: f64,
        stop_loss: f64,
        stop_limit: f64,
    ) -> Result<crate::exchange::OcoPlacement, String> {
        filters
            .check_order(quantity, stop_loss)
            .map_err(|rejection| format!("stop leg would be rejected: {rejection}"))?;
        filters
            .check_order(quantity, take_profit)
            .map_err(|rejection| format!("target leg would be rejected: {rejection}"))?;

        self.client
            .place_oco_sell(symbol, quantity, take_profit, stop_loss, stop_limit)
            .await
    }

    /// Keep exchange-side protection correct for one open position.
    ///
    /// Three jobs, in priority order:
    /// 1. Re-protect a position that has none — which is how a failed replace
    ///    from an earlier cycle gets repaired.
    /// 2. Ratchet the trailing stop upward once it has moved far enough to be
    ///    worth the replace.
    /// 3. Otherwise leave the resting order alone.
    ///
    /// Binance reserves the base asset against a resting order, so a new stop
    /// cannot be placed before the old one is cancelled. That ordering is
    /// forced, and it means every ratchet has a brief window with nothing on
    /// the exchange. `RATCHET_MIN_ATR` keeps those windows rare, and a failure
    /// leaves `protected` false so the next cycle repairs it.
    pub async fn maintain_protection(
        &self,
        symbol: &str,
        current_price: f64,
        params: &StrategyParams,
        risk_manager: &mut RiskManager,
    ) -> Result<(), String> {
        let Some(position) = risk_manager.positions.iter_mut().find(|p| p.symbol == symbol)
        else {
            return Ok(());
        };

        // The high-water mark only ever rises.
        position.highest_high = position.highest_high.max(current_price);

        let trailing = params.trailing_stop_atr > 0.0 && position.atr_at_entry > 0.0;
        let desired = if trailing {
            let trail = position.highest_high - params.trailing_stop_atr * position.atr_at_entry;
            // Never below the structural stop the entry was sized against.
            position.stop_loss.max(trail)
        } else {
            position.stop_loss
        };

        let ratchet = trailing
            && desired > position.stop_loss + RATCHET_MIN_ATR * position.atr_at_entry;

        if position.protected && !ratchet {
            return Ok(());
        }

        let (quantity, was_protected) = (position.quantity, position.protected);
        let filters = self.client.filters(symbol).await?;
        let stop_price = filters.round_price(desired);
        let stop_limit = filters.round_price(desired * STOP_LIMIT_SLIP);
        let sell_qty = filters.round_quantity(quantity);

        if let Err(rejection) = filters.check_order(sell_qty, stop_price) {
            warn!("{symbol}: cannot place protective stop — {rejection}");
            return Ok(());
        }

        // Only cancel when something is actually resting.
        if was_protected {
            self.client.cancel_open_orders(symbol).await.map_err(|e| {
                format!("{symbol}: could not cancel old stop, leaving it in place: {e}")
            })?;
        }

        match self
            .client
            .place_stop_loss(symbol, sell_qty, stop_price, stop_limit)
            .await
        {
            Ok(stop) => {
                if let Some(p) = risk_manager.positions.iter_mut().find(|p| p.symbol == symbol) {
                    p.stop_loss = stop.stop_price;
                    p.protected = true;
                }
                info!(
                    "{symbol}: protective stop at {:.2} (order {})",
                    stop.stop_price, stop.order_id
                );
                Ok(())
            }
            Err(e) => {
                // The old order is already gone. Say so loudly and let the next
                // cycle retry via job 1.
                if let Some(p) = risk_manager.positions.iter_mut().find(|p| p.symbol == symbol) {
                    p.protected = false;
                }
                error!("{symbol}: stop placement FAILED — position is UNPROTECTED: {e}");
                self.notifier
                    .notify_error(
                        &format!("UNPROTECTED {symbol}"),
                        &format!("Stop could not be placed: {e}. Retrying next cycle."),
                    )
                    .await;
                Err(e)
            }
        }
    }

    /// Force-close every open position (used by FORCE_CLOSE).
    pub async fn close_all_positions(&self, risk_manager: &mut RiskManager) {
        let symbols: Vec<String> = risk_manager
            .positions
            .iter()
            .map(|p| p.symbol.clone())
            .collect();

        for symbol in symbols {
            if let Err(e) = self.execute_sell(&symbol, risk_manager).await {
                error!("Force-close {symbol} failed: {e}");
            }
        }
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// How long a position was held, for the close log. Both arguments are epoch
/// milliseconds.
///
/// Positions recovered by reconciliation carry `entry_time: 0` — their true
/// entry is not persisted anywhere — and report as unknown rather than as a
/// spurious 50-year hold.
fn format_holding_period(entry_time: u64, now: u64) -> String {
    if entry_time == 0 || now < entry_time {
        return "unknown".to_string();
    }
    let minutes = (now - entry_time) / 60_000;
    match minutes {
        0..=59 => format!("{minutes}m"),
        60..=2879 => format!("{}h{}m", minutes / 60, minutes % 60),
        _ => format!("{}d{}h", minutes / 1440, (minutes % 1440) / 60),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exchange::{Fill, OcoPlacement, OpenOrder, OrderOutcome, StopPlacement};
    use crate::strategy::Signal;
    use std::sync::Mutex;

    /// Scriptable stand-in for the venue. Records what was asked of it so tests
    /// can assert on call order as well as on resulting state.
    #[derive(Default)]
    struct FakeExchange {
        /// `(symbol, side, quantity)` per market order, in call order.
        market_orders: Mutex<Vec<(String, Side, f64)>>,
        /// `(symbol, quantity, take_profit, stop)` per OCO attempt.
        oco_requests: Mutex<Vec<(String, f64, f64, f64)>>,
        /// Every operation in the order it happened, for sequencing assertions.
        call_log: Mutex<Vec<&'static str>>,

        /// `(average_price, filled_quantity)` the venue reports for a fill.
        fill: Option<(f64, f64)>,
        /// Fee charged in the base asset, as Binance does for spot BUYs.
        base_fee: f64,
        oco_rejects: bool,
        market_order_rejects: bool,
        stop_rejects: bool,
        /// `(symbol, quantity, stop_price)` per standalone stop attempt.
        stop_requests: Mutex<Vec<(String, f64, f64)>>,
    }

    /// The real BTCUSDC rules: 1e-5 lot step, 0.01 tick, 5.00 USDC minimum.
    fn btcusdc_filters() -> SymbolFilters {
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

    impl InstrumentProvider for FakeExchange {
        async fn filters(&self, _symbol: &str) -> Result<SymbolFilters, String> {
            Ok(btcusdc_filters())
        }
    }

    impl FakeExchange {
        fn filling_at(price: f64, qty: f64) -> Self {
            Self {
                fill: Some((price, qty)),
                ..Default::default()
            }
        }

        fn log(&self, what: &'static str) {
            self.call_log.lock().unwrap().push(what);
        }

        fn calls(&self) -> Vec<&'static str> {
            self.call_log.lock().unwrap().clone()
        }
    }

    impl ExecutionProvider for FakeExchange {
        async fn market_order(
            &self,
            symbol: &str,
            side: Side,
            quantity: f64,
        ) -> Result<OrderOutcome, String> {
            self.log("market_order");
            self.market_orders
                .lock()
                .unwrap()
                .push((symbol.to_string(), side, quantity));

            if self.market_order_rejects {
                return Err("insufficient balance".into());
            }

            let (average_price, filled_quantity) = self.fill.unwrap_or((0.0, quantity));
            Ok(OrderOutcome {
                symbol: symbol.to_string(),
                side,
                filled_quantity,
                average_price,
                fills: vec![Fill {
                    price: average_price,
                    quantity: filled_quantity,
                    commission: self.base_fee,
                    commission_asset: "BTC".into(),
                }],
            })
        }

        async fn place_oco_sell(
            &self,
            symbol: &str,
            quantity: f64,
            take_profit_price: f64,
            stop_price: f64,
            _stop_limit_price: f64,
        ) -> Result<OcoPlacement, String> {
            self.log("place_oco_sell");
            self.oco_requests.lock().unwrap().push((
                symbol.to_string(),
                quantity,
                take_profit_price,
                stop_price,
            ));

            if self.oco_rejects {
                return Err("Binance API error -1013: Filter failure: MIN_NOTIONAL".into());
            }
            Ok(OcoPlacement {
                order_list_id: 4242,
                stop_price,
                take_profit_price,
            })
        }

        async fn place_stop_loss(
            &self,
            symbol: &str,
            quantity: f64,
            stop_price: f64,
            _stop_limit_price: f64,
        ) -> Result<StopPlacement, String> {
            self.log("place_stop_loss");
            self.stop_requests
                .lock()
                .unwrap()
                .push((symbol.to_string(), quantity, stop_price));
            if self.stop_rejects {
                return Err("Binance API error -2010: insufficient balance".into());
            }
            Ok(StopPlacement {
                order_id: 77,
                stop_price,
            })
        }

        async fn cancel_open_orders(&self, _symbol: &str) -> Result<(), String> {
            self.log("cancel_open_orders");
            Ok(())
        }

        async fn open_orders(
            &self,
            _symbol: &str,
        ) -> Result<Vec<crate::exchange::OpenOrder>, String> {
            self.log("open_orders");
            Ok(Vec::new())
        }
    }

    fn buy_signal() -> TradeSignal {
        TradeSignal {
            asset: "BTCUSDC".into(),
            signal: Signal::Buy,
            confidence: "HIGH".into(),
            entry_price: 50_000.0,
            stop_loss: 49_000.0,
            take_profit: 52_000.0,
            risk_reward_ratio: 2.0,
            reasoning: "test".into(),
            warnings: Vec::new(),
            atr: 1_000.0,
        }
    }

    fn open_position(rm: &mut RiskManager, symbol: &str, qty: f64) {
        rm.open_position(Position {
            symbol: symbol.into(),
            entry_price: 50_000.0,
            quantity: qty,
            stop_loss: 49_000.0,
            take_profit: 52_000.0,
            entry_time: 0,
            protected: true,
            highest_high: 50_000.0,
            atr_at_entry: 1_000.0,
        });
    }

    /// A 3-ATR chandelier.
    fn trailing() -> StrategyParams {
        StrategyParams {
            trailing_stop_atr: 3.0,
            ..Default::default()
        }
    }

    // ----- BUY -----

    #[tokio::test]
    async fn buy_marks_position_protected_only_after_oco_confirms() {
        let venue = FakeExchange::filling_at(50_100.0, 0.003);
        let notifier = Notifier::disabled("USDC");
        let mut rm = RiskManager::new(10_000.0);

        Executor::new(&venue, &notifier)
            .execute_buy(&buy_signal(), 0.003, &mut rm, &StrategyParams::default())
            .await
            .unwrap();

        let pos = &rm.positions[0];
        assert!(
            pos.protected,
            "confirmed OCO must mark the position protected"
        );
        assert_eq!(pos.quantity, 0.003);
        // Entry is the realised fill, not the pre-trade quote.
        assert_eq!(pos.entry_price, 50_100.0);
        assert_eq!(venue.calls(), vec!["market_order", "place_oco_sell"]);
    }

    #[tokio::test]
    async fn buy_leaves_position_tracked_but_unprotected_when_oco_rejected() {
        let venue = FakeExchange {
            oco_rejects: true,
            ..FakeExchange::filling_at(50_100.0, 0.003)
        };
        let notifier = Notifier::disabled("USDC");
        let mut rm = RiskManager::new(10_000.0);

        // The buy itself still succeeded — the caller must not treat this as a
        // failed entry, or it would re-buy on the next cycle.
        Executor::new(&venue, &notifier)
            .execute_buy(&buy_signal(), 0.003, &mut rm, &StrategyParams::default())
            .await
            .unwrap();

        let pos = &rm.positions[0];
        assert!(!pos.protected, "rejected OCO must never read as protected");
        // Still tracked, and with live levels, so `check_exits` can act as the
        // only remaining stop.
        assert!(rm.has_position("BTCUSDC"));
        assert_eq!(pos.stop_loss, 49_000.0);
        assert!(pos.take_profit > 0.0);
    }

    #[tokio::test]
    async fn take_profit_is_two_to_one_against_the_realised_fill() {
        // Filled 100 above the quote: risk is 50_100 - 49_000 = 1_100, so the
        // target must move to 50_100 + 2_200, not stay at the signal's 52_000.
        let venue = FakeExchange::filling_at(50_100.0, 0.003);
        let notifier = Notifier::disabled("USDC");
        let mut rm = RiskManager::new(10_000.0);

        Executor::new(&venue, &notifier)
            .execute_buy(&buy_signal(), 0.003, &mut rm, &StrategyParams::default())
            .await
            .unwrap();

        assert!((rm.positions[0].take_profit - 52_300.0).abs() < 1e-6);
        // The OCO is placed at exactly the levels the position records.
        let (_, qty, tp, stop) = venue.oco_requests.lock().unwrap()[0].clone();
        assert_eq!(qty, 0.003);
        assert!((tp - 52_300.0).abs() < 1e-6);
        assert_eq!(stop, 49_000.0);
    }

    #[tokio::test]
    async fn buy_falls_back_to_signal_entry_when_venue_reports_no_fill_price() {
        let venue = FakeExchange::default(); // average_price 0.0
        let notifier = Notifier::disabled("USDC");
        let mut rm = RiskManager::new(10_000.0);

        Executor::new(&venue, &notifier)
            .execute_buy(&buy_signal(), 0.003, &mut rm, &StrategyParams::default())
            .await
            .unwrap();

        assert_eq!(rm.positions[0].entry_price, 50_000.0);
    }

    #[tokio::test]
    async fn failed_buy_registers_no_position() {
        let venue = FakeExchange {
            market_order_rejects: true,
            ..Default::default()
        };
        let notifier = Notifier::disabled("USDC");
        let mut rm = RiskManager::new(10_000.0);

        let result = Executor::new(&venue, &notifier)
            .execute_buy(&buy_signal(), 0.003, &mut rm, &StrategyParams::default())
            .await;

        assert!(result.is_err());
        assert!(rm.positions.is_empty());
        // No OCO should be attempted over a position that does not exist.
        assert_eq!(venue.calls(), vec!["market_order"]);
    }

    #[tokio::test]
    async fn buy_quantity_is_rounded_down_to_the_venue_step() {
        // 0.000123 is what 1.5%-of-equity sizing produces; it is not a legal
        // multiple of the 1e-5 step and Binance would reject it with -1013.
        let venue = FakeExchange::filling_at(50_000.0, 0.00012);
        let notifier = Notifier::disabled("USDC");
        let mut rm = RiskManager::new(19.0);

        Executor::new(&venue, &notifier)
            .execute_buy(&buy_signal(), 0.000123, &mut rm, &StrategyParams::default())
            .await
            .unwrap();

        let (_, _, sent_qty) = venue.market_orders.lock().unwrap()[0].clone();
        assert_eq!(sent_qty, 0.00012, "quantity must be a whole step");
    }

    #[tokio::test]
    async fn buy_below_min_notional_is_refused_before_reaching_the_venue() {
        // 0.00003 BTC at 50,000 is 1.50 USDC, under the 5.00 minimum.
        let venue = FakeExchange::filling_at(50_000.0, 0.00003);
        let notifier = Notifier::disabled("USDC");
        let mut rm = RiskManager::new(19.0);

        let err = Executor::new(&venue, &notifier)
            .execute_buy(&buy_signal(), 0.00003, &mut rm, &StrategyParams::default())
            .await
            .unwrap_err();

        assert!(err.contains("below the venue minimum"), "got: {err}");
        // The point of pre-checking is that no doomed order is sent.
        assert!(venue.calls().is_empty());
        assert!(rm.positions.is_empty());
        // And the operator is told what size would actually work.
        assert!(err.contains("smallest tradable size"), "got: {err}");
    }

    #[tokio::test]
    async fn oco_brackets_only_what_the_fee_left_behind() {
        // Binance takes the spot BUY fee in BTC, so bracketing the gross fill
        // would be rejected for insufficient balance.
        let venue = FakeExchange {
            base_fee: 0.00000012,
            ..FakeExchange::filling_at(50_000.0, 0.00012)
        };
        let notifier = Notifier::disabled("USDC");
        let mut rm = RiskManager::new(19.0);

        Executor::new(&venue, &notifier)
            .execute_buy(&buy_signal(), 0.00012, &mut rm, &StrategyParams::default())
            .await
            .unwrap();

        // 0.00012 - 0.00000012 = 0.00011988, rounded down to a whole step.
        let (_, oco_qty, _, _) = venue.oco_requests.lock().unwrap()[0].clone();
        assert_eq!(oco_qty, 0.00011);
        assert!(oco_qty < 0.00012);
    }

    #[tokio::test]
    async fn oco_prices_are_snapped_to_the_tick() {
        let venue = FakeExchange::filling_at(50_000.33, 0.00012);
        let notifier = Notifier::disabled("USDC");
        let mut rm = RiskManager::new(19.0);

        Executor::new(&venue, &notifier)
            .execute_buy(&buy_signal(), 0.00012, &mut rm, &StrategyParams::default())
            .await
            .unwrap();

        let (_, _, tp, stop) = venue.oco_requests.lock().unwrap()[0].clone();
        // 50_000.33 + 2*(50_000.33 - 49_000) = 52_001.  Both legs must land on
        // a whole cent.
        assert_eq!((tp * 100.0).round() / 100.0, tp);
        assert_eq!((stop * 100.0).round() / 100.0, stop);
    }

    #[tokio::test]
    async fn oco_is_not_attempted_when_a_leg_cannot_clear_the_minimum() {
        // A fill so small that the stop leg falls under 5.00 USDC. Sending it
        // would be rejected, so the position must be reported unprotected
        // rather than appearing to have a bracket.
        let venue = FakeExchange::filling_at(50_000.0, 0.00002);
        let notifier = Notifier::disabled("USDC");
        let mut rm = RiskManager::new(19.0);

        Executor::new(&venue, &notifier)
            .execute_buy(&buy_signal(), 0.00012, &mut rm, &StrategyParams::default())
            .await
            .unwrap();

        assert!(!rm.positions[0].protected);
        assert_eq!(venue.calls(), vec!["market_order"], "no doomed OCO sent");
    }

    // ----- trailing stop -----

    #[tokio::test]
    async fn a_trailing_entry_places_a_stop_and_no_target() {
        // Capping the winner is the thing the trail exists to avoid, so the
        // take-profit must be left unset rather than placed at 2R.
        let venue = FakeExchange::filling_at(50_000.0, 0.00012);
        let notifier = Notifier::disabled("USDC");
        let mut rm = RiskManager::new(19.0);

        Executor::new(&venue, &notifier)
            .execute_buy(&buy_signal(), 0.00012, &mut rm, &trailing())
            .await
            .unwrap();

        assert_eq!(venue.calls(), vec!["market_order", "place_stop_loss"]);
        let pos = &rm.positions[0];
        assert!(pos.protected);
        assert_eq!(pos.take_profit, 0.0, "no target under a trailing stop");
        assert_eq!(pos.atr_at_entry, 1_000.0);
    }

    #[tokio::test]
    async fn the_stop_ratchets_up_as_price_advances() {
        let venue = FakeExchange::default();
        let notifier = Notifier::disabled("USDC");
        let mut rm = RiskManager::new(10_000.0);
        open_position(&mut rm, "BTCUSDC", 0.001);

        // Price runs to 56,000: trail = 56,000 - 3*1,000 = 53,000.
        Executor::new(&venue, &notifier)
            .maintain_protection("BTCUSDC", 56_000.0, &trailing(), &mut rm)
            .await
            .unwrap();

        assert_eq!(rm.positions[0].stop_loss, 53_000.0);
        assert_eq!(rm.positions[0].highest_high, 56_000.0);
        // The old order must go before the new one, since it reserves the asset.
        assert_eq!(
            venue.calls(),
            vec!["cancel_open_orders", "place_stop_loss"]
        );
    }

    #[tokio::test]
    async fn the_stop_never_moves_down() {
        let venue = FakeExchange::default();
        let notifier = Notifier::disabled("USDC");
        let mut rm = RiskManager::new(10_000.0);
        open_position(&mut rm, "BTCUSDC", 0.001);
        let executor = Executor::new(&venue, &notifier);

        executor
            .maintain_protection("BTCUSDC", 56_000.0, &trailing(), &mut rm)
            .await
            .unwrap();
        // Price falls back; the ratchet must hold, not retreat.
        executor
            .maintain_protection("BTCUSDC", 51_000.0, &trailing(), &mut rm)
            .await
            .unwrap();

        assert_eq!(rm.positions[0].stop_loss, 53_000.0);
        assert_eq!(rm.positions[0].highest_high, 56_000.0);
    }

    #[tokio::test]
    async fn a_small_advance_does_not_churn_the_order() {
        // Every replace opens a window with nothing protecting the position,
        // so a few ticks of gain must not trigger one.
        let venue = FakeExchange::default();
        let notifier = Notifier::disabled("USDC");
        let mut rm = RiskManager::new(10_000.0);
        open_position(&mut rm, "BTCUSDC", 0.001);

        // Trail would be 52,050 — only 50 above the existing 49,000... but the
        // ratchet threshold is 0.25 ATR = 250, and 52,050 clears it, so use a
        // move that does not: price 52,100 gives trail 49,100, under 49,250.
        Executor::new(&venue, &notifier)
            .maintain_protection("BTCUSDC", 52_100.0, &trailing(), &mut rm)
            .await
            .unwrap();

        assert_eq!(rm.positions[0].stop_loss, 49_000.0, "stop should not move");
        assert!(venue.calls().is_empty(), "no orders should be touched");
    }

    #[tokio::test]
    async fn a_failed_replace_marks_the_position_unprotected() {
        // The old order is already cancelled at this point, so the position is
        // genuinely naked and must not be recorded as safe.
        let venue = FakeExchange {
            stop_rejects: true,
            ..Default::default()
        };
        let notifier = Notifier::disabled("USDC");
        let mut rm = RiskManager::new(10_000.0);
        open_position(&mut rm, "BTCUSDC", 0.001);

        let result = Executor::new(&venue, &notifier)
            .maintain_protection("BTCUSDC", 56_000.0, &trailing(), &mut rm)
            .await;

        assert!(result.is_err());
        assert!(!rm.positions[0].protected);
        assert!(rm.has_position("BTCUSDC"), "position must stay tracked");
    }

    #[tokio::test]
    async fn an_unprotected_position_is_repaired_without_cancelling() {
        // Repairing after a failed replace: nothing is resting, so issuing a
        // cancel would be pointless and could mask a real error.
        let venue = FakeExchange::default();
        let notifier = Notifier::disabled("USDC");
        let mut rm = RiskManager::new(10_000.0);
        open_position(&mut rm, "BTCUSDC", 0.001);
        rm.positions[0].protected = false;

        Executor::new(&venue, &notifier)
            .maintain_protection("BTCUSDC", 50_100.0, &trailing(), &mut rm)
            .await
            .unwrap();

        assert!(rm.positions[0].protected);
        assert_eq!(venue.calls(), vec!["place_stop_loss"]);
    }

    #[tokio::test]
    async fn maintenance_is_inert_without_a_position() {
        let venue = FakeExchange::default();
        let notifier = Notifier::disabled("USDC");
        let mut rm = RiskManager::new(10_000.0);

        Executor::new(&venue, &notifier)
            .maintain_protection("BTCUSDC", 56_000.0, &trailing(), &mut rm)
            .await
            .unwrap();

        assert!(venue.calls().is_empty());
    }

    #[tokio::test]
    async fn a_protected_position_is_left_alone_when_trailing_is_off() {
        let venue = FakeExchange::default();
        let notifier = Notifier::disabled("USDC");
        let mut rm = RiskManager::new(10_000.0);
        open_position(&mut rm, "BTCUSDC", 0.001);

        Executor::new(&venue, &notifier)
            .maintain_protection("BTCUSDC", 90_000.0, &StrategyParams::default(), &mut rm)
            .await
            .unwrap();

        assert_eq!(rm.positions[0].stop_loss, 49_000.0);
        assert!(venue.calls().is_empty());
    }

    // ----- SELL -----

    #[tokio::test]
    async fn sell_cancels_resting_oco_before_market_selling() {
        // The resting OCO reserves the base asset; selling first would be
        // rejected for insufficient balance.
        let venue = FakeExchange::filling_at(51_000.0, 0.003);
        let notifier = Notifier::disabled("USDC");
        let mut rm = RiskManager::new(10_000.0);
        open_position(&mut rm, "BTCUSDC", 0.003);

        Executor::new(&venue, &notifier)
            .execute_sell("BTCUSDC", &mut rm)
            .await
            .unwrap();

        assert_eq!(venue.calls(), vec!["cancel_open_orders", "market_order"]);
        assert!(!rm.has_position("BTCUSDC"));
        let (_, side, qty) = venue.market_orders.lock().unwrap()[0].clone();
        assert_eq!(side, Side::Sell);
        assert_eq!(qty, 0.003);
    }

    #[tokio::test]
    async fn failed_sell_keeps_the_position_tracked() {
        // The bot still holds the base asset. Dropping it from the risk manager
        // would leave real exposure invisible to every later cycle.
        let venue = FakeExchange {
            market_order_rejects: true,
            ..Default::default()
        };
        let notifier = Notifier::disabled("USDC");
        let mut rm = RiskManager::new(10_000.0);
        open_position(&mut rm, "BTCUSDC", 0.003);

        let result = Executor::new(&venue, &notifier)
            .execute_sell("BTCUSDC", &mut rm)
            .await;

        assert!(result.is_err());
        assert!(
            rm.has_position("BTCUSDC"),
            "position must survive a failed sell"
        );
        assert_eq!(rm.positions[0].quantity, 0.003);
    }

    #[tokio::test]
    async fn selling_an_untracked_symbol_is_an_error_and_places_no_order() {
        let venue = FakeExchange::default();
        let notifier = Notifier::disabled("USDC");
        let mut rm = RiskManager::new(10_000.0);

        assert!(Executor::new(&venue, &notifier)
            .execute_sell("BTCUSDC", &mut rm)
            .await
            .is_err());
        assert!(venue.calls().is_empty());
    }

    #[test]
    fn holding_period_is_unknown_for_reconciled_positions() {
        // Reconciliation cannot recover the original entry time.
        assert_eq!(format_holding_period(0, 1_700_000_000_000), "unknown");
    }

    #[test]
    fn holding_period_never_reports_a_negative_span() {
        // Clock skew must not produce a wrapped, enormous duration.
        assert_eq!(
            format_holding_period(1_700_000_000_000, 1_699_000_000_000),
            "unknown"
        );
    }

    #[test]
    fn holding_period_scales_its_unit() {
        let base = 1_700_000_000_000u64;
        assert_eq!(format_holding_period(base, base + 45 * 60_000), "45m");
        assert_eq!(format_holding_period(base, base + 150 * 60_000), "2h30m");
        // A swing trade held four and a half days.
        assert_eq!(format_holding_period(base, base + 6_480 * 60_000), "4d12h");
    }

    #[tokio::test]
    async fn close_all_positions_empties_the_book() {
        let venue = FakeExchange::filling_at(51_000.0, 1.0);
        let notifier = Notifier::disabled("USDC");
        let mut rm = RiskManager::new(10_000.0);
        open_position(&mut rm, "BTCUSDC", 0.003);
        open_position(&mut rm, "ETHUSDC", 0.5);

        Executor::new(&venue, &notifier)
            .close_all_positions(&mut rm)
            .await;

        assert!(rm.positions.is_empty());
        assert_eq!(venue.market_orders.lock().unwrap().len(), 2);
    }
}
