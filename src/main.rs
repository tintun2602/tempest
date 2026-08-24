mod backtest;
mod config;
mod costs;
mod exchange;
mod executor;
mod indicators;
mod notify;
mod risk;
mod strategy;

use config::Config;
use exchange::binance::BinanceClient;
use exchange::{
    protective_levels, AccountProvider, ExecutionProvider, InstrumentProvider, MarketDataProvider,
};
use executor::Executor;
use notify::Notifier;
use risk::{Position, RiskManager};
use std::env;
use strategy::Signal;
use tokio::time::{sleep, Duration};
use tracing::{error, info, warn};

/// Positions worth less than this are leftover fractions, not real holdings.
const DUST_NOTIONAL: f64 = 5.0;

/// Conservative bracket applied when reconciling an unprotected holding whose
/// original levels are unknown.
const EMERGENCY_STOP_PCT: f64 = 0.97;
const EMERGENCY_TARGET_PCT: f64 = 1.06;

#[tokio::main]
async fn main() {
    let config = Config::from_env();

    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let client = BinanceClient::new(&config);

    // --- Backtest mode ---
    if config.backtest_mode {
        backtest::run(&client, &config.trading_pairs, config.risk_per_trade).await;
        return;
    }

    // --- Live mode ---
    info!("Tempest swing trading bot starting");
    info!("Trading pairs: {:?}", config.trading_pairs);
    info!("Poll interval: {}s", config.poll_interval_secs);

    let notifier = Notifier::from_env();

    let equity = match fetch_equity(&client, &config).await {
        Ok(eq) => {
            info!(
                "Starting equity: {:.2} {} ({:.2} free)",
                eq.total, config.quote_asset, eq.free_quote
            );
            eq
        }
        Err(e) => {
            error!("Failed to fetch initial equity: {e}. Starting with 0.");
            Equity {
                free_quote: 0.0,
                total: 0.0,
            }
        }
    };

    notifier
        .notify_startup(equity.total, &config.trading_pairs)
        .await;

    info!(
        "Risk per trade: {:.2}% of equity",
        config.risk_per_trade * 100.0
    );
    let mut risk_manager = RiskManager::with_risk_per_trade(equity.total, config.risk_per_trade);

    // Detect positions held from a prior crash that lack OCO protection.
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
async fn run_cycle<C>(
    client: &C,
    config: &Config,
    risk_manager: &mut RiskManager,
    notifier: &Notifier,
) -> Result<(), String>
where
    C: MarketDataProvider + AccountProvider + ExecutionProvider + InstrumentProvider,
{
    // ---- FORCE_CLOSE override ----
    if env::var("FORCE_CLOSE").unwrap_or_default() == "true" {
        warn!("FORCE_CLOSE is set — closing all positions");
        Executor::new(client, notifier)
            .close_all_positions(risk_manager)
            .await;
        return Ok(());
    }

    // ---- Equity & drawdown ----
    // Drawdown is measured against total equity — free quote plus the
    // mark-to-market value of open positions. Measuring free quote alone counts
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

        let daily = match client.klines(symbol, "1d", 250).await {
            Ok(c) => c,
            Err(e) => {
                error!("{symbol}: daily klines failed: {e}");
                continue;
            }
        };

        let four_hour = match client.klines(symbol, "4h", 100).await {
            Ok(c) => c,
            Err(e) => {
                error!("{symbol}: 4h klines failed: {e}");
                continue;
            }
        };

        let price = match client.price(symbol).await {
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
                    equity.free_quote,
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
                // An existing position may still have breached a level.
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
        "Cycle complete | open positions: {} | equity: {:.2} {} ({:.2} free)",
        risk_manager.positions.len(),
        equity.total,
        config.quote_asset,
        equity.free_quote
    );
    Ok(())
}

/// Portfolio value split into the cash that can fund new entries and the total
/// that risk limits are measured against.
struct Equity {
    free_quote: f64,
    total: f64,
}

/// Total equity: free quote asset plus the mark-to-market value of every held
/// asset that maps to a configured trading pair.
///
/// A pricing failure is an error rather than a skipped asset: silently omitting
/// a position understates equity and would trip the drawdown halt.
async fn fetch_equity<C>(client: &C, config: &Config) -> Result<Equity, String>
where
    C: MarketDataProvider + AccountProvider,
{
    let account = client.account(&config.quote_asset).await?;
    let mut total = account.free_quote;

    for balance in &account.assets {
        let symbol = format!("{}{}", balance.asset, config.quote_asset);
        if !config.trading_pairs.contains(&symbol) {
            continue;
        }
        let price = client
            .price(&symbol)
            .await
            .map_err(|e| format!("cannot price {symbol} for equity: {e}"))?;
        total += balance.quantity * price;
    }

    Ok(Equity {
        free_quote: account.free_quote,
        total,
    })
}

/// On startup, check the account for holdings that correspond to configured
/// trading pairs. Any found without a confirmed protective stop is bracketed
/// with an emergency OCO so no position is left unprotected after a crash.
async fn reconcile_positions<C>(
    client: &C,
    config: &Config,
    risk_manager: &mut RiskManager,
    notifier: &Notifier,
) where
    C: MarketDataProvider + AccountProvider + ExecutionProvider + InstrumentProvider,
{
    info!("[RECONCILE] Scanning exchange for existing positions...");

    let held = match client.account(&config.quote_asset).await {
        Ok(snapshot) => snapshot.assets,
        Err(e) => {
            error!("[RECONCILE] Failed to fetch balances: {e}");
            return;
        }
    };

    let mut restored = 0u32;
    let mut emergency = 0u32;
    let mut failed = 0u32;

    for balance in &held {
        // Match asset to a configured trading pair (e.g. "BTC" -> "BTCUSDC").
        let symbol = format!("{}{}", balance.asset, config.quote_asset);
        if !config.trading_pairs.contains(&symbol) {
            continue;
        }
        let asset = &balance.asset;
        let qty = balance.quantity;

        let orders = match client.open_orders(&symbol).await {
            Ok(o) => o,
            Err(e) => {
                error!("[RECONCILE] {asset}: failed to check open orders: {e}");
                failed += 1;
                continue;
            }
        };

        if !orders.is_empty() {
            let price = client.price(&symbol).await.unwrap_or(0.0);
            // Recover the real protective levels from the live orders.
            // Registering 0.0 would make `check_exits` read `price >=
            // take_profit` as a hit and liquidate on the very next cycle.
            let (stop_loss, take_profit) = protective_levels(&orders);

            // Only a stop we can positively identify counts as protection.
            let protected = stop_loss > 0.0;
            if protected {
                info!(
                    "[RECONCILE] {asset}: found existing OCO — registered at {qty:.6} {asset} \
                     (~{price:.2}, SL {stop_loss:.2}, TP {take_profit:.2})"
                );
            } else {
                warn!(
                    "[RECONCILE] {asset}: {} open order(s) but no stop could be identified — \
                     leaving the exchange orders to manage this position",
                    orders.len()
                );
            }

            risk_manager.open_position(Position {
                symbol,
                // Estimated: the true fill price is not persisted anywhere, so
                // PnL reported when this position closes is measured from
                // today's mark.
                entry_price: price,
                quantity: qty,
                stop_loss,
                take_profit,
                entry_time: 0,
                protected,
            });
            restored += 1;
            continue;
        }

        // Non-zero balance but NO open orders -> unprotected position.
        let price = match client.price(&symbol).await {
            Ok(p) => p,
            Err(e) => {
                error!("[RECONCILE] {asset}: cannot get price: {e}");
                failed += 1;
                continue;
            }
        };

        let notional = qty * price;
        if notional < DUST_NOTIONAL {
            info!("[RECONCILE] {asset}: skipping dust ({qty:.8} {asset} ~ {notional:.2})");
            continue;
        }

        // Round to the venue's tick and step, or the rescue order is rejected
        // and the position stays naked.
        let (qty, stop, take_profit, stop_limit) = match client.filters(&symbol).await {
            Ok(f) => (
                f.round_quantity(qty),
                f.round_price(price * EMERGENCY_STOP_PCT),
                f.round_price(price * EMERGENCY_TARGET_PCT),
                f.round_price(price * EMERGENCY_STOP_PCT * 0.998),
            ),
            Err(e) => {
                error!("[RECONCILE] {asset}: cannot read venue filters: {e}");
                failed += 1;
                continue;
            }
        };

        warn!(
            "[RECONCILE] {asset}: no OCO found — placing emergency OCO \
             (stop: {stop:.2}, tp: {take_profit:.2})"
        );

        let protected = match client
            .place_oco_sell(&symbol, qty, take_profit, stop, stop_limit)
            .await
        {
            Ok(oco) => {
                info!(
                    "[RECONCILE] {asset}: emergency OCO placed, list {}",
                    oco.order_list_id
                );
                emergency += 1;
                true
            }
            Err(e) => {
                // Happens if the bot crashed mid-OCO leaving a partial order,
                // or if the quantity is below the minimum notional. Either way
                // the position is unprotected and needs manual attention.
                error!(
                    "[RECONCILE] {asset}: EMERGENCY OCO FAILED — position is UNPROTECTED. \
                     Manual intervention required. Error: {e}"
                );
                failed += 1;
                false
            }
        };

        // Track the levels either way: when the OCO failed the exchange is
        // holding nothing, so the bot's own `check_exits` is the only stop.
        risk_manager.open_position(Position {
            symbol,
            entry_price: price,
            quantity: qty,
            stop_loss: stop,
            take_profit,
            entry_time: 0,
            protected,
        });
    }

    if restored + emergency + failed == 0 {
        info!("[RECONCILE] Done. No existing positions found — clean start.");
    } else {
        info!(
            "[RECONCILE] Done. {restored} position(s) restored, \
             {emergency} emergency order(s) placed, {failed} failed."
        );
    }
    if failed > 0 {
        error!(
            "[RECONCILE] {failed} position(s) could not be protected — check exchange manually!"
        );
    }

    notifier.notify_reconcile(restored, emergency, failed).await;
}
