use crate::market::{BinanceClient, Candle};
use crate::risk::RiskManager;
use crate::strategy::{self, Signal};
use rand::Rng;
use tracing::info;

const STARTING_BALANCE: f64 = 10_000.0;
const MONTE_CARLO_RUNS: usize = 10_000;

#[derive(Debug)]
struct SimTrade {
    symbol: String,
    entry_price: f64,
    exit_price: f64,
    quantity: f64,
    stop_loss: f64,
    take_profit: f64,
    pnl: f64,
    pnl_pct: f64,
}

#[derive(Debug)]
struct SimPosition {
    symbol: String,
    entry_price: f64,
    quantity: f64,
    stop_loss: f64,
    take_profit: f64,
}

/// Run a backtest for a single symbol over its historical data.
pub async fn run(client: &BinanceClient, symbols: &[String]) {
    info!("=== BACKTEST MODE ===");
    info!("Starting balance: {STARTING_BALANCE:.2} USDT");

    for symbol in symbols {
        info!("\n--- Backtesting {symbol} ---");
        match backtest_symbol(client, symbol).await {
            Ok(()) => {}
            Err(e) => {
                tracing::error!("{symbol}: backtest failed: {e}");
            }
        }
    }
}

async fn backtest_symbol(client: &BinanceClient, symbol: &str) -> Result<(), String> {
    // Fetch maximum historical data from production API (testnet has limited history)
    // Daily: 1000 candles = ~2.7 years. 4H: paginate to cover the same period (~6000 candles).
    let daily = client.get_klines_public(symbol, "1d", 1000).await?;
    let four_hour = client.get_klines_extended(symbol, "4h", 6000).await?;

    if daily.len() < 210 {
        return Err(format!("Only {} daily candles available, need 210+", daily.len()));
    }
    if four_hour.len() < 50 {
        return Err(format!("Only {} 4H candles available, need 50+", four_hour.len()));
    }

    info!(
        "Loaded {} daily candles, {} 4H candles",
        daily.len(),
        four_hour.len()
    );

    // Build a mapping from daily candle timestamps to the 4H candles available at that point.
    // For each daily candle at index i, we use all 4H candles whose close_time <= daily[i].close_time.
    let mut balance = STARTING_BALANCE;
    let mut risk_manager = RiskManager::new(balance);
    let mut trades: Vec<SimTrade> = Vec::new();
    let mut position: Option<SimPosition> = None;
    let mut peak_balance = balance;
    let mut max_drawdown = 0.0_f64;

    // Diagnostics: count how often each condition is true
    let mut diag_evaluated = 0u32;
    let mut diag_trend = 0u32;
    let mut diag_rsi = 0u32;
    let mut diag_macd = 0u32;
    let mut diag_rr = 0u32;

    // Walk through daily candles from index 200 onward (need 200 for EMA200 warmup)
    for i in 200..daily.len() {
        let daily_slice = &daily[..=i];
        let current_price = daily[i].close;

        // Find 4H candles up to this daily candle's close time
        let daily_close_time = daily[i].close_time;
        let four_hour_slice: Vec<&Candle> = four_hour
            .iter()
            .filter(|c| c.close_time <= daily_close_time)
            .collect();

        if four_hour_slice.len() < 35 {
            continue;
        }

        // Convert to owned slices for the strategy
        let fh_owned: Vec<Candle> = four_hour_slice.into_iter().cloned().collect();

        // --- Check exits on existing position ---
        if let Some(pos) = &position {
            let hit_sl = daily[i].low <= pos.stop_loss;
            let hit_tp = daily[i].high >= pos.take_profit;

            if hit_sl || hit_tp {
                let exit_price = if hit_sl { pos.stop_loss } else { pos.take_profit };
                let pnl = (exit_price - pos.entry_price) * pos.quantity;
                let pnl_pct = (exit_price - pos.entry_price) / pos.entry_price * 100.0;
                balance += pnl;

                trades.push(SimTrade {
                    symbol: symbol.to_string(),
                    entry_price: pos.entry_price,
                    exit_price,
                    quantity: pos.quantity,
                    stop_loss: pos.stop_loss,
                    take_profit: pos.take_profit,
                    pnl,
                    pnl_pct,
                });

                risk_manager.close_position(&pos.symbol);
                position = None;
            }
        }

        // --- Evaluate strategy ---
        let snap = match strategy::compute_indicators(daily_slice, &fh_owned, current_price) {
            Some(s) => s,
            None => continue,
        };

        // Track condition hits for diagnostics
        diag_evaluated += 1;
        let trend_ok = snap.current_price > snap.ema_50 && snap.ema_50 > snap.ema_200;
        let rsi_ok = snap.rsi_14 >= 35.0 && snap.rsi_14 <= 55.0;
        let macd_ok = snap.macd_crossed_bullish_recently;
        let stop_dist = snap.current_price - snap.swing_low;
        let rr_ok = stop_dist > 0.0; // RR is always 2.0 by construction
        if trend_ok { diag_trend += 1; }
        if rsi_ok { diag_rsi += 1; }
        if macd_ok { diag_macd += 1; }
        if rr_ok { diag_rr += 1; }

        let signal = strategy::evaluate(symbol, &snap);

        match signal.signal {
            Signal::Buy if position.is_none() => {
                // Simulated cash and equity are the same number here: the sim holds
                // at most one position and books PnL on exit.
                let (qty, _) = risk_manager.calculate_position_size(
                    balance,
                    balance,
                    signal.entry_price,
                    signal.stop_loss,
                );
                if qty <= 0.0 {
                    continue;
                }

                position = Some(SimPosition {
                    symbol: symbol.to_string(),
                    entry_price: signal.entry_price,
                    quantity: qty,
                    stop_loss: signal.stop_loss,
                    take_profit: signal.take_profit,
                });

                risk_manager.open_position(crate::risk::Position {
                    symbol: symbol.to_string(),
                    entry_price: signal.entry_price,
                    quantity: qty,
                    stop_loss: signal.stop_loss,
                    take_profit: signal.take_profit,
                    entry_time: daily[i].open_time,
                });
            }
            Signal::Sell if position.is_some() => {
                let pos = position.take().unwrap();
                let pnl = (current_price - pos.entry_price) * pos.quantity;
                let pnl_pct = (current_price - pos.entry_price) / pos.entry_price * 100.0;
                balance += pnl;

                trades.push(SimTrade {
                    symbol: symbol.to_string(),
                    entry_price: pos.entry_price,
                    exit_price: current_price,
                    quantity: pos.quantity,
                    stop_loss: pos.stop_loss,
                    take_profit: pos.take_profit,
                    pnl,
                    pnl_pct,
                });

                risk_manager.close_position(&pos.symbol);
            }
            _ => {}
        }

        // Track drawdown
        if balance > peak_balance {
            peak_balance = balance;
        }
        let dd = (peak_balance - balance) / peak_balance * 100.0;
        if dd > max_drawdown {
            max_drawdown = dd;
        }
    }

    // --- Close any remaining open position at last price ---
    if let Some(pos) = position.take() {
        let exit_price = daily.last().unwrap().close;
        let pnl = (exit_price - pos.entry_price) * pos.quantity;
        let pnl_pct = (exit_price - pos.entry_price) / pos.entry_price * 100.0;
        balance += pnl;

        trades.push(SimTrade {
            symbol: symbol.to_string(),
            entry_price: pos.entry_price,
            exit_price,
            quantity: pos.quantity,
            stop_loss: pos.stop_loss,
            take_profit: pos.take_profit,
            pnl,
            pnl_pct,
        });
    }

    // --- Print diagnostics & report ---
    println!();
    println!("  Signal Diagnostics ({diag_evaluated} days evaluated):");
    println!("    Trend  (price > EMA50 > EMA200): {diag_trend:>4} days ({:.1}%)", diag_trend as f64 / diag_evaluated.max(1) as f64 * 100.0);
    println!("    RSI    (35-55):                   {diag_rsi:>4} days ({:.1}%)", diag_rsi as f64 / diag_evaluated.max(1) as f64 * 100.0);
    println!("    MACD   (bullish cross last 3):    {diag_macd:>4} days ({:.1}%)", diag_macd as f64 / diag_evaluated.max(1) as f64 * 100.0);
    println!("    R:R    (stop > 0):                {diag_rr:>4} days ({:.1}%)", diag_rr as f64 / diag_evaluated.max(1) as f64 * 100.0);

    print_report(symbol, &trades, balance, max_drawdown);
    print_monte_carlo_report(&trades);

    Ok(())
}

fn print_monte_carlo_report(trades: &[SimTrade]) {
    if trades.is_empty() {
        println!("\nMonte Carlo: skipped (no completed trades)");
        return;
    }

    let returns: Vec<f64> = trades.iter().map(|trade| trade.pnl_pct / 100.0).collect();
    let mut final_balances = Vec::with_capacity(MONTE_CARLO_RUNS);
    let mut max_drawdowns = Vec::with_capacity(MONTE_CARLO_RUNS);
    let mut worst_losing_streak = Vec::with_capacity(MONTE_CARLO_RUNS);
    let mut rng = rand::thread_rng();

    for _ in 0..MONTE_CARLO_RUNS {
        let mut balance = STARTING_BALANCE;
        let mut peak = balance;
        let mut max_drawdown: f64 = 0.0;
        let mut losing_streak = 0usize;
        let mut longest_losing_streak = 0usize;

        for _ in 0..returns.len() {
            let trade_return = returns[rng.gen_range(0..returns.len())];
            balance *= 1.0 + trade_return;
            peak = peak.max(balance);
            if peak > 0.0 {
                max_drawdown = max_drawdown.max((peak - balance) / peak);
            }

            if trade_return < 0.0 {
                losing_streak += 1;
                longest_losing_streak = longest_losing_streak.max(losing_streak);
            } else {
                losing_streak = 0;
            }
        }

        final_balances.push(balance);
        max_drawdowns.push(max_drawdown * 100.0);
        worst_losing_streak.push(longest_losing_streak as f64);
    }

    final_balances.sort_by(f64::total_cmp);
    max_drawdowns.sort_by(f64::total_cmp);
    worst_losing_streak.sort_by(f64::total_cmp);

    println!("\n  Monte Carlo ({} runs, trade returns resampled):", MONTE_CARLO_RUNS);
    println!(
        "    Final balance p5 / p50 / p95: {:.2} / {:.2} / {:.2}",
        percentile(&final_balances, 0.05),
        percentile(&final_balances, 0.50),
        percentile(&final_balances, 0.95)
    );
    println!(
        "    Max drawdown p5 / p50 / p95:   {:.2}% / {:.2}% / {:.2}%",
        percentile(&max_drawdowns, 0.05),
        percentile(&max_drawdowns, 0.50),
        percentile(&max_drawdowns, 0.95)
    );
    println!(
        "    Losing streak p50 / p95:        {:.0} / {:.0} trades",
        percentile(&worst_losing_streak, 0.50),
        percentile(&worst_losing_streak, 0.95)
    );
    println!("    Note: ignores fees, slippage, and changing market conditions.");
}

fn percentile(sorted_values: &[f64], percentile: f64) -> f64 {
    let index = ((sorted_values.len() - 1) as f64 * percentile).round() as usize;
    sorted_values[index]
}

fn print_report(symbol: &str, trades: &[SimTrade], final_balance: f64, max_drawdown: f64) {
    let total_trades = trades.len();
    let winners: Vec<&SimTrade> = trades.iter().filter(|t| t.pnl > 0.0).collect();
    let losers: Vec<&SimTrade> = trades.iter().filter(|t| t.pnl <= 0.0).collect();
    let win_count = winners.len();
    let loss_count = losers.len();

    let total_pnl: f64 = trades.iter().map(|t| t.pnl).sum();
    let total_return = (final_balance - STARTING_BALANCE) / STARTING_BALANCE * 100.0;

    let avg_win = if win_count > 0 {
        winners.iter().map(|t| t.pnl).sum::<f64>() / win_count as f64
    } else {
        0.0
    };
    let avg_loss = if loss_count > 0 {
        losers.iter().map(|t| t.pnl).sum::<f64>() / loss_count as f64
    } else {
        0.0
    };
    let win_rate = if total_trades > 0 {
        win_count as f64 / total_trades as f64 * 100.0
    } else {
        0.0
    };
    let profit_factor = if avg_loss != 0.0 {
        (avg_win * win_count as f64) / (avg_loss.abs() * loss_count as f64)
    } else {
        f64::INFINITY
    };

    println!();
    println!("╔══════════════════════════════════════════════════╗");
    println!("║  BACKTEST REPORT: {:<31}║", symbol);
    println!("╠══════════════════════════════════════════════════╣");
    println!("║  Starting Balance:  {:>10.2} USDT             ║", STARTING_BALANCE);
    println!("║  Final Balance:     {:>10.2} USDT             ║", final_balance);
    println!("║  Total Return:      {:>+10.2}%                 ║", total_return);
    println!("║  Total PnL:         {:>+10.2} USDT             ║", total_pnl);
    println!("╠══════════════════════════════════════════════════╣");
    println!("║  Total Trades:      {:>10}                   ║", total_trades);
    println!("║  Winners:           {:>10}                   ║", win_count);
    println!("║  Losers:            {:>10}                   ║", loss_count);
    println!("║  Win Rate:          {:>10.1}%                 ║", win_rate);
    println!("╠══════════════════════════════════════════════════╣");
    println!("║  Avg Win:           {:>+10.2} USDT             ║", avg_win);
    println!("║  Avg Loss:          {:>+10.2} USDT             ║", avg_loss);
    println!("║  Profit Factor:     {:>10.2}                   ║", profit_factor);
    println!("║  Max Drawdown:      {:>10.2}%                 ║", max_drawdown);
    println!("╚══════════════════════════════════════════════════╝");

    if !trades.is_empty() {
        println!();
        println!("  Trade Log:");
        println!("  {:>4}  {:>10}  {:>10}  {:>10}  {:>8}", "#", "Entry", "Exit", "PnL", "PnL %");
        println!("  {}", "-".repeat(52));
        for (i, t) in trades.iter().enumerate() {
            println!(
                "  {:>4}  {:>10.2}  {:>10.2}  {:>+10.2}  {:>+7.2}%",
                i + 1,
                t.entry_price,
                t.exit_price,
                t.pnl,
                t.pnl_pct
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::percentile;

    #[test]
    fn percentile_returns_nearest_sorted_value() {
        let values = [10.0, 20.0, 30.0, 40.0, 50.0];

        assert_eq!(percentile(&values, 0.0), 10.0);
        assert_eq!(percentile(&values, 0.5), 30.0);
        assert_eq!(percentile(&values, 1.0), 50.0);
    }
}
