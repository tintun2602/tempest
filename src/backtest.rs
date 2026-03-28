use crate::market::{BinanceClient, Candle};
use crate::risk::RiskManager;
use crate::strategy::{self, Signal};
use tracing::info;

const STARTING_BALANCE: f64 = 10_000.0;

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
    // Fetch maximum historical data
    let daily = client.get_klines(symbol, "1d", 1000).await?;
    let four_hour = client.get_klines(symbol, "4h", 1000).await?;

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

        let signal = strategy::evaluate(symbol, &snap);

        match signal.signal {
            Signal::Buy if position.is_none() => {
                let (qty, _) = risk_manager.calculate_position_size(
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

    // --- Print report ---
    print_report(symbol, &trades, balance, max_drawdown);

    Ok(())
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
