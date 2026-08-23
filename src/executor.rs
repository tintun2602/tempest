use crate::market::BinanceClient;
use crate::notify::Notifier;
use crate::risk::{Position, RiskManager};
use crate::strategy::TradeSignal;
use tracing::{error, info, warn};

pub struct Executor<'a> {
    client: &'a BinanceClient,
    notifier: &'a Notifier,
}

impl<'a> Executor<'a> {
    pub fn new(client: &'a BinanceClient, notifier: &'a Notifier) -> Self {
        Self { client, notifier }
    }

    /// Execute a BUY: market-buy, then place an OCO sell for SL + TP.
    pub async fn execute_buy(
        &self,
        signal: &TradeSignal,
        quantity: f64,
        risk_manager: &mut RiskManager,
    ) -> Result<(), String> {
        info!(
            "BUY {} | qty {:.6} | entry ~{:.2} | SL {:.2} | TP {:.2}",
            signal.asset, quantity, signal.entry_price, signal.stop_loss, signal.take_profit
        );

        // 1. Market buy
        let order = self
            .client
            .market_order(&signal.asset, "BUY", quantity)
            .await?;

        let fill_price = weighted_fill_price(&order).unwrap_or(signal.entry_price);
        let executed_qty = order["executedQty"]
            .as_str()
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(quantity);

        info!(
            "BUY filled: {} @ {:.2} qty {:.6}",
            signal.asset, fill_price, executed_qty
        );

        // Recalculate SL / TP from actual fill
        let stop_distance = fill_price - signal.stop_loss;
        let actual_sl = signal.stop_loss;
        let actual_tp = fill_price + 2.0 * stop_distance;
        let stop_limit = actual_sl * 0.998; // slightly below trigger to ensure fill

        // 2. OCO sell (take-profit + stop-loss)
        match self
            .client
            .oco_sell(&signal.asset, executed_qty, actual_tp, actual_sl, stop_limit)
            .await
        {
            Ok(oco) => {
                info!(
                    "OCO placed for {}: id={:?}",
                    signal.asset,
                    oco.get("orderListId")
                );
            }
            Err(e) => {
                error!(
                    "OCO FAILED for {} — position is UNPROTECTED: {}",
                    signal.asset, e
                );
            }
        }

        // 3. Track the position locally
        risk_manager.open_position(Position {
            symbol: signal.asset.clone(),
            entry_price: fill_price,
            quantity: executed_qty,
            stop_loss: actual_sl,
            take_profit: actual_tp,
            entry_time: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64,
        });

        // 4. Notify
        self.notifier
            .notify_buy(
                &signal.asset,
                fill_price,
                executed_qty,
                actual_sl,
                actual_tp,
                &signal.confidence,
                &signal.reasoning,
            )
            .await;

        Ok(())
    }

    /// Close an existing position: cancel open orders, then market-sell.
    pub async fn execute_sell(
        &self,
        symbol: &str,
        risk_manager: &mut RiskManager,
    ) -> Result<(), String> {
        let position = risk_manager
            .close_position(symbol)
            .ok_or_else(|| format!("No open position for {symbol}"))?;

        info!("SELL {} | qty {:.6}", symbol, position.quantity);

        // Cancel any standing OCO / limit orders first
        if let Err(e) = self.client.cancel_open_orders(symbol).await {
            warn!("Failed to cancel open orders for {}: {}", symbol, e);
        }

        // Market sell
        match self
            .client
            .market_order(symbol, "SELL", position.quantity)
            .await
        {
            Ok(result) => {
                let sell_price = weighted_fill_price(&result).unwrap_or_else(|| {
                    warn!("Could not parse fill price from response: {}", result);
                    0.0
                });
                let pnl = (sell_price - position.entry_price) * position.quantity;
                let pnl_pct =
                    (sell_price - position.entry_price) / position.entry_price * 100.0;
                info!(
                    "SELL filled: {} @ {:.2} | PnL {:.2} {} ({:+.2}%)",
                    symbol, sell_price, pnl, self.notifier.quote_asset(), pnl_pct
                );
                self.notifier
                    .notify_sell(symbol, sell_price, pnl, pnl_pct)
                    .await;
                Ok(())
            }
            Err(e) => Err(format!("Market sell failed for {symbol}: {e}")),
        }
    }

    /// Force-close every open position (used by FORCE_CLOSE and HALT).
    pub async fn close_all_positions(&self, risk_manager: &mut RiskManager) {
        let symbols: Vec<String> = risk_manager
            .positions
            .iter()
            .map(|p| p.symbol.clone())
            .collect();

        for symbol in symbols {
            if let Err(e) = self.execute_sell(&symbol, risk_manager).await {
                error!("Force-close {} failed: {}", symbol, e);
            }
        }
    }
}

/// Compute volume-weighted average fill price from Binance order response.
/// Falls back to cummulativeQuoteQty / executedQty if fills array is missing.
fn weighted_fill_price(order: &serde_json::Value) -> Option<f64> {
    // Primary: VWAP from fills array
    if let Some(fills) = order["fills"].as_array() {
        let mut total_cost = 0.0_f64;
        let mut total_qty = 0.0_f64;
        for fill in fills {
            if let (Some(p), Some(q)) = (
                fill["price"].as_str().and_then(|s| s.parse::<f64>().ok()),
                fill["qty"].as_str().and_then(|s| s.parse::<f64>().ok()),
            ) {
                total_cost += p * q;
                total_qty += q;
            }
        }
        if total_qty > 0.0 {
            return Some(total_cost / total_qty);
        }
    }

    // Fallback: cummulativeQuoteQty / executedQty
    let quote_qty: f64 = order["cummulativeQuoteQty"].as_str()?.parse().ok()?;
    let exec_qty: f64 = order["executedQty"].as_str()?.parse().ok()?;
    if exec_qty > 0.0 {
        Some(quote_qty / exec_qty)
    } else {
        None
    }
}
