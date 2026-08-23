mod backtest;
mod config;
mod executor;
mod indicators;
mod market;
mod notify;
mod risk;
mod strategy;

use config::Config;
use executor::Executor;
use market::BinanceClient;
use notify::Notifier;
use risk::{Position, RiskManager};
use strategy::Signal;
use std::env;
use tokio::time::{sleep, Duration};
use tracing::{error, info, warn};

#[tokio::main]
async fn main() {
    let config = Config::from_env();

    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let client = BinanceClient::new(&config);

    // --- Backtest mode ---
    if config.backtest_mode {
        backtest::run(&client, &config.trading_pairs).await;
        return;
    }

    // --- Live mode ---
    info!("Binance Swing Trading Bot starting");
    info!("Trading pairs: {:?}", config.trading_pairs);
    info!("Poll interval: {}s", config.poll_interval_secs);

    let notifier = Notifier::from_env();

    let equity = match fetch_equity(&client, &config).await {
        Ok(eq) => {
            info!(
                "Starting equity: {:.2} USDT ({:.2} free)",
                eq.total, eq.free_usdt
            );
            eq
        }
        Err(e) => {
            error!("Failed to fetch initial equity: {e}. Starting with 0.");
            Equity {
                free_usdt: 0.0,
                total: 0.0,
            }
        }
    };

    notifier
        .notify_startup(equity.total, &config.trading_pairs)
        .await;

    let mut risk_manager = RiskManager::new(equity.total);

    // Reconcile: detect positions held from a prior crash that lack OCO protection
    reconcile_positions(&client, &config, &mut risk_manager, &notifier).await;

    let poll_interval = Duration::from_secs(config.poll_interval_secs);

    loop {
        if let Err(e) = run_cycle(&client, &config, &mut risk_manager, &notifier).await {
            error!("Cycle error: {e}");
            notifier.notify_error("Cycle", &e).await;
        }
        sleep(poll_interval).await;
    }
}

/// One full evaluation cycle: fetch data -> compute indicators -> evaluate -> execute.
async fn run_cycle(
    client: &BinanceClient,
    config: &Config,
    risk_manager: &mut RiskManager,
    notifier: &Notifier,
) -> Result<(), String> {
    // ---- FORCE_CLOSE override ----
    if env::var("FORCE_CLOSE").unwrap_or_default() == "true" {
        warn!("FORCE_CLOSE is set — closing all positions");
        let executor = Executor::new(client, notifier);
        executor.close_all_positions(risk_manager).await;
        return Ok(());
    }

    // ---- Equity & drawdown ----
    // Drawdown is measured against total equity — free USDT plus the
    // mark-to-market value of open positions. Measuring free USDT alone counts
    // every entry as a loss the size of the position notional, which trips the
    // 5% halt after a single trade.
    let equity = fetch_equity(client, config).await?;
    risk_manager.check_day_reset(equity.total);

    if risk_manager.check_drawdown(equity.total) {
        warn!("HALTED — daily drawdown limit exceeded. No new trades until next UTC day.");
        notifier
            .notify_halt(risk_manager.drawdown_pct(equity.total), equity.total)
            .await;
        return Ok(());
    }
    if risk_manager.halted {
        info!("Still halted from earlier drawdown breach. Skipping cycle.");
        return Ok(());
    }

    let executor = Executor::new(client, notifier);

    // ---- Evaluate each trading pair ----
    for symbol in &config.trading_pairs {
        info!("--- Evaluating {symbol} ---");

        let daily = match client.get_klines(symbol, "1d", 250).await {
            Ok(c) => c,
            Err(e) => {
                error!("{symbol}: daily klines failed: {e}");
                continue;
            }
        };

        let four_hour = match client.get_klines(symbol, "4h", 100).await {
            Ok(c) => c,
            Err(e) => {
                error!("{symbol}: 4h klines failed: {e}");
                continue;
            }
        };

        let price = match client.get_price(symbol).await {
            Ok(p) => p,
            Err(e) => {
                error!("{symbol}: price fetch failed: {e}");
                continue;
            }
        };

        let snap = match strategy::compute_indicators(&daily, &four_hour, price) {
            Some(s) => s,
            None => {
                warn!("{symbol}: insufficient candle data for indicators");
                continue;
            }
        };

        let signal = strategy::evaluate(symbol, &snap);

        info!(
            "{symbol}: signal={:?} confidence={} RSI={:.1} EMA50={:.2} EMA200={:.2}",
            signal.signal, signal.confidence, snap.rsi_14, snap.ema_50, snap.ema_200
        );
        if !signal.warnings.is_empty() {
            warn!("{symbol}: {}", signal.warnings.join("; "));
        }

        match signal.signal {
            Signal::Buy => {
                if risk_manager.has_position(symbol) {
                    info!("{symbol}: already in position, skipping BUY");
                    continue;
                }
                if !risk_manager.can_open_position() {
                    info!("{symbol}: max positions reached or halted, skipping BUY");
                    continue;
                }
                let (qty, _) = risk_manager.calculate_position_size(
                    equity.total,
                    equity.free_usdt,
                    signal.entry_price,
                    signal.stop_loss,
                );
                if qty <= 0.0 {
                    warn!("{symbol}: calculated position size is zero, skipping");
                    continue;
                }
                if let Err(e) = executor.execute_buy(&signal, qty, risk_manager).await {
                    error!("{symbol}: BUY failed: {e}");
                    notifier.notify_error(&format!("BUY {symbol}"), &e).await;
                }
            }
            Signal::Sell => {
                if !risk_manager.has_position(symbol) {
                    info!("{symbol}: SELL signal but no open position");
                    continue;
                }
                if let Err(e) = executor.execute_sell(symbol, risk_manager).await {
                    error!("{symbol}: SELL failed: {e}");
                    notifier.notify_error(&format!("SELL {symbol}"), &e).await;
                }
            }
            Signal::Hold => {
                // Check if an existing position should be exited on price breach
                if risk_manager.check_exits(symbol, price).is_some() {
                    info!("{symbol}: price hit SL/TP level, closing position");
                    if let Err(e) = executor.execute_sell(symbol, risk_manager).await {
                        error!("{symbol}: exit failed: {e}");
                        notifier.notify_error(&format!("Exit {symbol}"), &e).await;
                    }
                }
            }
            Signal::Halt => {
                warn!("{symbol}: HALT signal");
            }
        }
    }

    info!(
        "Cycle complete | open positions: {} | equity: {:.2} USDT ({:.2} free)",
        risk_manager.positions.len(),
        equity.total,
        equity.free_usdt
    );
    Ok(())
}

/// Portfolio value split into the cash that can fund new entries and the total
/// that risk limits are measured against.
struct Equity {
    free_usdt: f64,
    total: f64,
}

/// Total equity: free USDT plus the mark-to-market value of every held asset
/// that maps to a configured trading pair.
///
/// A pricing failure is an error rather than a skipped asset: silently omitting
/// a position understates equity and would trip the drawdown halt.
async fn fetch_equity(client: &BinanceClient, config: &Config) -> Result<Equity, String> {
    let account = client.get_account().await?;
    let mut total = account.free_usdt;

    for (asset, qty) in &account.assets {
        let symbol = format!("{asset}USDT");
        if !config.trading_pairs.contains(&symbol) {
            continue;
        }
        let price = client
            .get_price(&symbol)
            .await
            .map_err(|e| format!("cannot price {symbol} for equity: {e}"))?;
        total += qty * price;
    }

    Ok(Equity {
        free_usdt: account.free_usdt,
        total,
    })
}

/// On startup, check the Binance account for non-zero asset balances that correspond
/// to configured trading pairs. If any are found without open OCO orders, register them
/// in the risk manager and place protective OCO orders so no position is left unprotected
/// after a crash/restart.
async fn reconcile_positions(
    client: &BinanceClient,
    config: &Config,
    risk_manager: &mut RiskManager,
    notifier: &Notifier,
) {
    info!("[RECONCILE] Scanning exchange for existing positions...");

    let held = match client.get_nonzero_balances().await {
        Ok(b) => b,
        Err(e) => {
            error!("[RECONCILE] Failed to fetch balances: {e}");
            return;
        }
    };

    let mut restored = 0u32;
    let mut emergency = 0u32;
    let mut failed = 0u32;

    for (asset, qty) in &held {
        // Match asset to a configured trading pair (e.g. "BTC" -> "BTCUSDT")
        let symbol = format!("{asset}USDT");
        if !config.trading_pairs.contains(&symbol) {
            continue;
        }

        // Check if there are already open orders protecting this position
        match client.get_open_orders(&symbol).await {
            Ok(orders) if !orders.is_empty() => {
                let price = client.get_price(&symbol).await.unwrap_or(0.0);
                // Recover the real protective levels from the live orders. Registering
                // 0.0 here would make `check_exits` read `price >= take_profit` as a
                // hit and liquidate the position on the very next cycle.
                let (stop_loss, take_profit) = parse_protective_levels(&orders);

                if stop_loss == 0.0 && take_profit == 0.0 {
                    warn!(
                        "[RECONCILE] {asset}: {} open order(s) but no SL/TP could be parsed — \
                         leaving the exchange orders to manage this position",
                        orders.len()
                    );
                } else {
                    info!(
                        "[RECONCILE] {asset}: found existing OCO — registered at {qty:.6} {asset} \
                         (~${price:.2}, SL {stop_loss:.2}, TP {take_profit:.2})"
                    );
                }

                risk_manager.open_position(Position {
                    symbol: symbol.clone(),
                    // Estimated: the true fill price is not persisted anywhere, so PnL
                    // reported when this position closes is measured from today's mark.
                    entry_price: price,
                    quantity: *qty,
                    stop_loss,
                    take_profit,
                    entry_time: 0,
                });
                restored += 1;
            }
            Ok(_) => {
                // Non-zero balance but NO open orders -> unprotected position
                let price = match client.get_price(&symbol).await {
                    Ok(p) => p,
                    Err(e) => {
                        error!("[RECONCILE] {asset}: cannot get price: {e}");
                        failed += 1;
                        continue;
                    }
                };

                // Skip dust balances (< $5 notional) — these are leftover
                // fractional amounts, not real positions.
                let notional = qty * price;
                if notional < 5.0 {
                    info!(
                        "[RECONCILE] {asset}: skipping dust ({qty:.8} {asset} ≈ ${notional:.2})"
                    );
                    continue;
                }

                // Conservative 3% stop-loss and 6% take-profit from current price
                let stop = price * 0.97;
                let tp = price * 1.06;
                let stop_limit = stop * 0.998;

                warn!(
                    "[RECONCILE] {asset}: no OCO found — placing emergency OCO (stop: {stop:.2}, tp: {tp:.2})"
                );

                let oco_ok = match client.oco_sell(&symbol, *qty, tp, stop, stop_limit).await {
                    Ok(oco) => {
                        info!(
                            "[RECONCILE] {asset}: emergency OCO placed, id={:?}",
                            oco.get("orderListId")
                        );
                        true
                    }
                    Err(e) => {
                        // This can happen if the bot crashed mid-OCO and a partial order
                        // exists, or if the quantity is below the minimum notional. Either
                        // way, the position remains unprotected and needs manual attention.
                        error!(
                            "[RECONCILE] {asset}: EMERGENCY OCO FAILED — position is UNPROTECTED. \
                             Manual intervention required. Error: {e}"
                        );
                        false
                    }
                };

                // Track the levels either way: when the OCO failed the exchange is
                // holding nothing, so the bot's own `check_exits` is the only stop
                // this position has.
                risk_manager.open_position(Position {
                    symbol,
                    entry_price: price,
                    quantity: *qty,
                    stop_loss: stop,
                    take_profit: tp,
                    entry_time: 0,
                });

                if oco_ok {
                    emergency += 1;
                } else {
                    failed += 1;
                }
            }
            Err(e) => {
                error!("[RECONCILE] {asset}: failed to check open orders: {e}");
                failed += 1;
            }
        }
    }

    let total = restored + emergency + failed;
    if total == 0 {
        info!("[RECONCILE] Done. No existing positions found — clean start.");
    } else {
        info!(
            "[RECONCILE] Done. {} position(s) restored, {} emergency order(s) placed, {} failed.",
            restored, emergency, failed
        );
    }
    if failed > 0 {
        error!(
            "[RECONCILE] {failed} position(s) could not be protected — check exchange manually!"
        );
    }

    notifier.notify_reconcile(restored, emergency, failed).await;
}

/// Extract `(stop_loss, take_profit)` from a symbol's open sell orders.
///
/// An OCO sell is two orders: a LIMIT_MAKER holding the target and a
/// STOP_LOSS_LIMIT whose `stopPrice` is the trigger. A level that cannot be
/// found is returned as `0.0`, which `check_exits` treats as unset.
fn parse_protective_levels(orders: &[serde_json::Value]) -> (f64, f64) {
    let mut stop_loss = 0.0;
    let mut take_profit = 0.0;

    for order in orders {
        if order["side"].as_str() != Some("SELL") {
            continue;
        }
        let price = order["price"]
            .as_str()
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(0.0);
        let stop_price = order["stopPrice"]
            .as_str()
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(0.0);

        match order["type"].as_str().unwrap_or("") {
            "STOP_LOSS" | "STOP_LOSS_LIMIT" if stop_price > 0.0 => stop_loss = stop_price,
            "LIMIT_MAKER" | "LIMIT" | "TAKE_PROFIT" | "TAKE_PROFIT_LIMIT" if price > 0.0 => {
                take_profit = price
            }
            _ => {}
        }
    }

    (stop_loss, take_profit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_parse_protective_levels_from_oco() {
        let orders = vec![
            json!({
                "side": "SELL",
                "type": "LIMIT_MAKER",
                "price": "62000.00",
                "stopPrice": "0.00"
            }),
            json!({
                "side": "SELL",
                "type": "STOP_LOSS_LIMIT",
                "price": "56888.00",
                "stopPrice": "57000.00"
            }),
        ];
        let (sl, tp) = parse_protective_levels(&orders);
        assert!((sl - 57_000.0).abs() < 1e-9);
        assert!((tp - 62_000.0).abs() < 1e-9);
    }

    #[test]
    fn test_parse_protective_levels_unrecognised_orders() {
        // A stray buy order carries no protection — both levels stay unset so
        // `check_exits` leaves the position alone.
        let orders = vec![json!({ "side": "BUY", "type": "LIMIT", "price": "50000.00" })];
        let (sl, tp) = parse_protective_levels(&orders);
        assert_eq!(sl, 0.0);
        assert_eq!(tp, 0.0);
    }

    #[test]
    fn test_parse_protective_levels_empty() {
        let (sl, tp) = parse_protective_levels(&[]);
        assert_eq!(sl, 0.0);
        assert_eq!(tp, 0.0);
    }
}
