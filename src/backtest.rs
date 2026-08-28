use crate::costs::CostModel;
use crate::exchange::{Candle, MarketDataProvider};
use crate::risk::RiskManager;
use crate::strategy::{self, EntryConditions, Signal, StrategyParams};
use rand::Rng;
use std::collections::HashMap;
use tracing::info;

const STARTING_BALANCE: f64 = 10_000.0;
const MONTE_CARLO_RUNS: usize = 10_000;
/// 4H bars in a day — the evaluation cadence, and the live poll interval.
const BARS_PER_DAY: f64 = 6.0;

#[derive(Debug)]
struct SimTrade {
    entry_price: f64,
    exit_price: f64,
    quantity: f64,
    /// Net of both fees.
    pnl: f64,
    /// Return on the position's own notional.
    pnl_pct: f64,
    /// Return on *portfolio equity* at entry. This — not `pnl_pct` — is what
    /// compounds, because sizing risks a fixed fraction of equity rather than
    /// deploying all of it.
    equity_return: f64,
    fees: f64,
    exit_reason: ExitReason,
}

/// Why a simulated position was closed. Recorded at the exit site rather than
/// inferred afterwards from the price, which cannot distinguish a stop fill
/// from a signal exit that happened to land near the stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExitReason {
    StopLoss,
    TakeProfit,
    /// Strategy produced a SELL before either level was touched.
    Signal,
    /// Still open when history ran out; marked to the final close.
    EndOfData,
}

impl ExitReason {
    fn label(self) -> &'static str {
        match self {
            ExitReason::StopLoss => "SL",
            ExitReason::TakeProfit => "TP",
            ExitReason::Signal => "signal",
            ExitReason::EndOfData => "end",
        }
    }
}

#[derive(Debug)]
struct SimPosition {
    symbol: String,
    /// The price actually paid, i.e. quote plus slippage.
    entry_price: f64,
    quantity: f64,
    stop_loss: f64,
    take_profit: f64,
    entry_fee: f64,
    /// Portfolio equity when this position was opened.
    equity_at_entry: f64,
}

/// Close `pos` at `exit_quote` and price the fill according to how the exit
/// happens.
///
/// A take-profit is a resting limit order: it fills at its price and pays the
/// maker fee. Everything else — a triggered stop, a signal exit, the final
/// mark — crosses the book and pays taker plus slippage.
fn settle(pos: &SimPosition, exit_quote: f64, reason: ExitReason, costs: &CostModel) -> SimTrade {
    let resting = reason == ExitReason::TakeProfit;
    let exit_price = if resting {
        costs.limit_fill(exit_quote)
    } else {
        costs.sell_fill(exit_quote)
    };

    let exit_notional = exit_price * pos.quantity;
    let exit_fee = if resting {
        costs.maker_cost(exit_notional)
    } else {
        costs.taker_cost(exit_notional)
    };

    let fees = pos.entry_fee + exit_fee;
    let gross = (exit_price - pos.entry_price) * pos.quantity;
    let pnl = gross - fees;
    let entry_notional = pos.entry_price * pos.quantity;

    SimTrade {
        entry_price: pos.entry_price,
        exit_price,
        quantity: pos.quantity,
        pnl,
        pnl_pct: if entry_notional > 0.0 {
            pnl / entry_notional * 100.0
        } else {
            0.0
        },
        equity_return: if pos.equity_at_entry > 0.0 {
            pnl / pos.equity_at_entry
        } else {
            0.0
        },
        fees,
        exit_reason: reason,
    }
}

/// Run a backtest for a single symbol over its historical data.
pub async fn run<M: MarketDataProvider>(
    client: &M,
    symbols: &[String],
    risk_per_trade: f64,
) {
    let params = StrategyParams::from_env();
    let params = &params;
    let costs = CostModel::from_env();
    info!(
        "Costs: taker {:.3}% | maker {:.3}% | slippage {:.3}%",
        costs.taker_fee * 100.0,
        costs.maker_fee * 100.0,
        costs.slippage * 100.0
    );
    info!(
        "Signal buffers: entry {:.2} ATR | exit {:.2} ATR",
        params.entry_buffer_atr, params.exit_buffer_atr
    );
    info!("=== BACKTEST MODE ===");
    info!("Starting balance: {STARTING_BALANCE:.2} USDT");

    for symbol in symbols {
        info!("\n--- Backtesting {symbol} ---");
        match backtest_symbol(client, symbol, &costs, risk_per_trade, params).await {
            Ok(()) => {}
            Err(e) => {
                tracing::error!("{symbol}: backtest failed: {e}");
            }
        }
    }

    if symbols.len() > 1 {
        if let Err(e) = portfolio_backtest(client, symbols, &costs, risk_per_trade, params).await {
            tracing::error!("portfolio backtest failed: {e}");
        }
    }
}

async fn backtest_symbol<M: MarketDataProvider>(
    client: &M,
    symbol: &str,
    costs: &CostModel,
    risk_per_trade: f64,
    params: &StrategyParams,
) -> Result<(), String> {
    // Fetch maximum historical data from production API (testnet has limited history)
    // Daily: 1000 candles = ~2.7 years. 4H: paginate to cover the same period (~6000 candles).
    let daily = client.klines_extended(symbol, "1d", 1000).await?;
    let four_hour = client.klines_extended(symbol, "4h", 6000).await?;

    if daily.len() < 210 {
        return Err(format!(
            "Only {} daily candles available, need 210+",
            daily.len()
        ));
    }
    if four_hour.len() < 50 {
        return Err(format!(
            "Only {} 4H candles available, need 50+",
            four_hour.len()
        ));
    }

    info!(
        "Loaded {} daily candles, {} 4H candles",
        daily.len(),
        four_hour.len()
    );

    // The live bot polls every 4 hours, and each poll reads `klines(1d, ..)`,
    // which includes the *in-progress* daily candle. Walking one step per
    // completed daily candle therefore measured a different strategy from the
    // one that actually runs: it saw only a 12-hour MACD window out of every
    // 24, and never saw a partial day at all.
    //
    // So the walk follows the 4H grid, and the daily series is reconstructed at
    // each step exactly as the venue would report it. Nothing later than
    // `four_hour[j].close_time` is ever read, so there is still no look-ahead.
    // Run the same history twice. The frictionless pass isolates what the
    // strategy found; the costed pass is what a real account would have kept.
    let gross = simulate(
        symbol,
        &daily,
        &four_hour,
        &CostModel::frictionless(),
        risk_per_trade,
        params,
    );
    let net = simulate(symbol, &daily, &four_hour, costs, risk_per_trade, params);

    print_diagnostics(&net.diagnostics, 1);
    print_cost_impact(&gross, &net);
    if let Some(first) = net.first_bar {
        print_benchmark(&net, &buy_and_hold(&four_hour[first..], costs));
    }
    print_report(symbol, &net.trades, net.final_balance, net.max_drawdown);
    print_monte_carlo_report(&net.trades);
    print_period_split(
        symbol,
        &daily,
        &four_hour,
        costs,
        risk_per_trade,
        net.first_bar,
        params,
    );
    print_risk_sweep(&daily, &four_hour, costs, symbol, params);

    Ok(())
}

/// Everything the walk produced.
struct SimResult {
    trades: Vec<SimTrade>,
    final_balance: f64,
    /// Peak-to-trough on *mark-to-market equity*, not realised cash.
    max_drawdown: f64,
    /// Index of the first 4H bar that cleared warmup, so a benchmark can be
    /// measured over exactly the same window.
    first_bar: Option<usize>,
    diagnostics: Diagnostics,
}

/// How often each entry condition held, and how often they held together.
#[derive(Default)]
struct Diagnostics {
    evaluated: u32,
    trend: u32,
    rsi: u32,
    macd: u32,
    rr: u32,
    all_aligned: u32,
    blocked_in_position: u32,
}

impl Diagnostics {
    fn record(&mut self, conditions: &EntryConditions, already_long: bool) {
        self.evaluated += 1;
        self.trend += u32::from(conditions.trend_bullish);
        self.rsi += u32::from(conditions.rsi_ok);
        self.macd += u32::from(conditions.macd_crossed);
        self.rr += u32::from(conditions.stop_valid);
        if conditions.all_met() {
            self.all_aligned += 1;
            self.blocked_in_position += u32::from(already_long);
        }
    }
}

/// Walk the history once under a given cost model. Pure: no I/O, so it can be
/// run repeatedly over the same candles.
fn simulate(
    symbol: &str,
    daily: &[Candle],
    four_hour: &[Candle],
    costs: &CostModel,
    risk_per_trade: f64,
    params: &StrategyParams,
) -> SimResult {
    let mut balance = STARTING_BALANCE;
    let mut risk_manager = RiskManager::with_risk_per_trade(balance, risk_per_trade);
    let mut trades: Vec<SimTrade> = Vec::new();
    let mut position: Option<SimPosition> = None;
    let mut peak_balance = balance;
    let mut max_drawdown = 0.0_f64;

    // Diagnostics: count how often each condition is true
    let mut diagnostics = Diagnostics::default();
    // Days where every entry condition held at once — the true signal rate, as
    // opposed to how often each condition fires on its own.

    // Daily candles that have closed. The in-progress one is appended only for
    // the duration of each indicator call.
    let mut daily_view: Vec<Candle> = Vec::new();
    let mut next_daily = 0usize;
    let mut first_bar: Option<usize> = None;

    for j in 0..four_hour.len() {
        let bar = &four_hour[j];

        // Absorb every daily candle that has closed by this bar.
        while next_daily < daily.len() && daily[next_daily].close_time <= bar.close_time {
            daily_view.push(daily[next_daily].clone());
            next_daily += 1;
        }

        // EMA200 warmup on the daily series, MACD warmup on the 4H series.
        if daily_view.len() < 200 || j + 1 < 35 {
            continue;
        }

        first_bar.get_or_insert(j);
        let current_price = bar.close;

        // --- Check exits on existing position ---
        // Tested against 4H extremes rather than daily ones, matching the
        // cadence the live bot actually polls at. A bar that touches both
        // levels resolves as the stop, which is the conservative reading.
        if let Some(pos) = &position {
            let hit_sl = bar.low <= pos.stop_loss;
            let hit_tp = bar.high >= pos.take_profit;

            if hit_sl || hit_tp {
                // One bar touching both levels resolves as the stop.
                let (level, reason) = if hit_sl {
                    (pos.stop_loss, ExitReason::StopLoss)
                } else {
                    (pos.take_profit, ExitReason::TakeProfit)
                };
                let trade = settle(pos, level, reason, costs);
                balance += trade.pnl;
                trades.push(trade);

                risk_manager.close_position(&pos.symbol);
                position = None;
            }
        }

        // --- Evaluate strategy ---
        // Append the in-progress daily candle so indicator input matches what
        // the venue reports mid-day, then restore the view.
        let partial = daily
            .get(next_daily)
            .and_then(|next| partial_daily(&four_hour[..=j], next.open_time));
        let pushed = partial.is_some();
        if let Some(candle) = partial {
            daily_view.push(candle);
        }
        let snap = strategy::compute_indicators(&daily_view, &four_hour[..=j], current_price);
        if pushed {
            daily_view.pop();
        }

        let Some(snap) = snap else { continue };

        diagnostics.record(
            &EntryConditions::evaluate(&snap, params),
            position.is_some(),
        );

        let signal = strategy::evaluate(symbol, &snap, params);

        match signal.signal {
            Signal::Buy if position.is_none() => {
                // Simulated cash and equity are the same number here: the sim holds
                // at most one position and books PnL on exit.
                let entry_fill = costs.buy_fill(signal.entry_price);
                let (qty, _) = risk_manager.calculate_position_size(
                    balance,
                    balance,
                    entry_fill,
                    signal.stop_loss,
                );
                if qty <= 0.0 {
                    continue;
                }

                // The live executor re-derives the target from the realised
                // fill, so the sim must too or it books a different trade.
                let take_profit = entry_fill + 2.0 * (entry_fill - signal.stop_loss);

                position = Some(SimPosition {
                    symbol: symbol.to_string(),
                    entry_price: entry_fill,
                    quantity: qty,
                    stop_loss: signal.stop_loss,
                    take_profit,
                    entry_fee: costs.taker_cost(entry_fill * qty),
                    equity_at_entry: balance,
                });

                risk_manager.open_position(crate::risk::Position {
                    symbol: symbol.to_string(),
                    entry_price: entry_fill,
                    quantity: qty,
                    stop_loss: signal.stop_loss,
                    take_profit,
                    entry_time: bar.open_time,
                    // The sim fills SL/TP off candle extremes, i.e. it assumes
                    // the bracket is resting on the exchange.
                    protected: true,
                });
            }
            Signal::Sell if position.is_some() => {
                let pos = position.take().unwrap();
                let trade = settle(&pos, current_price, ExitReason::Signal, costs);
                balance += trade.pnl;
                trades.push(trade);

                risk_manager.close_position(&pos.symbol);
            }
            _ => {}
        }

        // Track drawdown on mark-to-market equity. Measuring realised cash
        // alone hides every loss that is still open — the exact stretch a
        // drawdown limit is supposed to catch.
        let open_pnl = position
            .as_ref()
            .map_or(0.0, |p| (bar.close - p.entry_price) * p.quantity);
        let equity = balance + open_pnl;
        peak_balance = peak_balance.max(equity);
        if peak_balance > 0.0 {
            max_drawdown = max_drawdown.max((peak_balance - equity) / peak_balance * 100.0);
        }
    }

    // --- Close any remaining open position at last price ---
    if let Some(pos) = position.take() {
        let last_close = four_hour.last().map_or(pos.entry_price, |c| c.close);
        let trade = settle(&pos, last_close, ExitReason::EndOfData, costs);
        balance += trade.pnl;
        trades.push(trade);
    }

    SimResult {
        trades,
        final_balance: balance,
        max_drawdown,
        first_bar,
        diagnostics,
    }
}

/// Hold the asset over the same window, for comparison.
///
/// Costed symmetrically — one market buy in, one market sell out — so the
/// comparison is not rigged in either direction. Returns
/// `(final_balance, max_drawdown_pct)`.
fn buy_and_hold(bars: &[Candle], costs: &CostModel) -> (f64, f64) {
    let Some(first) = bars.first() else {
        return (STARTING_BALANCE, 0.0);
    };

    let entry = costs.buy_fill(first.close);
    if entry <= 0.0 {
        return (STARTING_BALANCE, 0.0);
    }
    let quantity = (STARTING_BALANCE - costs.taker_cost(STARTING_BALANCE)) / entry;

    let mut peak = STARTING_BALANCE;
    let mut max_drawdown: f64 = 0.0;
    for bar in bars {
        // Mark against the bar's low: the drawdown a holder actually lived
        // through, not the one visible only at closes.
        let equity = bar.low * quantity;
        peak = peak.max(bar.close * quantity);
        if peak > 0.0 {
            max_drawdown = max_drawdown.max((peak - equity) / peak * 100.0);
        }
    }

    let exit = costs.sell_fill(bars.last().map_or(entry, |b| b.close));
    let proceeds = exit * quantity;
    (proceeds - costs.taker_cost(proceeds), max_drawdown)
}

/// Aggregate the trailing 4H bars belonging to the unfinished day starting at
/// `day_open` into the partial daily candle a venue would report.
///
/// Returns `None` when the last bar predates `day_open` — i.e. the day has no
/// bars yet, so there is nothing in progress. Walks back at most six bars.
fn partial_daily(bars: &[Candle], day_open: u64) -> Option<Candle> {
    let end = bars.len().checked_sub(1)?;
    if bars[end].open_time < day_open {
        return None;
    }

    let mut start = end;
    while start > 0 && bars[start - 1].open_time >= day_open {
        start -= 1;
    }
    let window = &bars[start..=end];
    let first = window.first()?;
    let last = window.last()?;

    Some(Candle {
        open_time: first.open_time,
        open: first.open,
        high: window.iter().map(|c| c.high).fold(f64::MIN, f64::max),
        low: window.iter().map(|c| c.low).fold(f64::MAX, f64::min),
        close: last.close,
        volume: window.iter().map(|c| c.volume).sum(),
        close_time: last.close_time,
    })
}

fn print_diagnostics(d: &Diagnostics, symbols: usize) {
    let evaluated = d.evaluated.max(1) as f64;
    // Each bar produces one evaluation per symbol.
    let bars_per_day = BARS_PER_DAY * symbols.max(1) as f64;
    let pct = |n: u32| n as f64 / evaluated * 100.0;

    println!();
    println!(
        "  Signal Diagnostics ({} evaluations over ~{:.0} days):",
        d.evaluated,
        evaluated / bars_per_day
    );
    println!(
        "    Trend  (price > EMA50 > EMA200): {:>5} bars ({:.1}%)",
        d.trend,
        pct(d.trend)
    );
    println!(
        "    RSI    (35-55):                  {:>5} bars ({:.1}%)",
        d.rsi,
        pct(d.rsi)
    );
    println!(
        "    MACD   (bullish cross last 3):   {:>5} bars ({:.1}%)",
        d.macd,
        pct(d.macd)
    );
    println!(
        "    R:R    (stop > 0):               {:>5} bars ({:.1}%)",
        d.rr,
        pct(d.rr)
    );
    println!("    {}", "-".repeat(46));
    println!(
        "    ALL four aligned:                {:>5} bars ({:.2}%)",
        d.all_aligned,
        pct(d.all_aligned)
    );
    println!(
        "      of which blocked (already long):{:>5}",
        d.blocked_in_position
    );
    if d.all_aligned > 0 {
        println!(
            "    Average gap between signals:     {:>5.0} days",
            evaluated / bars_per_day / d.all_aligned as f64
        );
    }
}

/// Contrast the same history with and without execution costs.
///
/// The trade counts can differ: slippage shifts the entry, which shifts the
/// derived target, which changes which bar an exit lands on.
fn print_cost_impact(gross: &SimResult, net: &SimResult) {
    let ret = |b: f64| (b - STARTING_BALANCE) / STARTING_BALANCE * 100.0;
    let fees: f64 = net.trades.iter().map(|t| t.fees).sum();

    println!();
    println!("  Cost Impact:");
    println!(
        "    Frictionless:  {:>10.2}  ({:>+7.2}%)  {:>3} trades",
        gross.final_balance,
        ret(gross.final_balance),
        gross.trades.len()
    );
    println!(
        "    After costs:   {:>10.2}  ({:>+7.2}%)  {:>3} trades",
        net.final_balance,
        ret(net.final_balance),
        net.trades.len()
    );
    println!("    Fees paid:     {fees:>10.2}");
    println!(
        "    Total drag:    {:>10.2}  ({:>+7.2} pts of return)",
        gross.final_balance - net.final_balance,
        ret(net.final_balance) - ret(gross.final_balance)
    );
    if net.final_balance <= STARTING_BALANCE && gross.final_balance > STARTING_BALANCE {
        println!("    >> The edge does not survive execution costs.");
    }
}

/// Put the strategy next to simply holding the asset over the same window.
fn print_benchmark(net: &SimResult, (hold_balance, hold_drawdown): &(f64, f64)) {
    let ret = |b: f64| (b - STARTING_BALANCE) / STARTING_BALANCE * 100.0;

    println!();
    println!("  vs Buy & Hold (same window, same costs):");
    println!(
        "    Strategy:      {:>10.2}  ({:>+7.2}%)   max DD {:>6.2}%",
        net.final_balance,
        ret(net.final_balance),
        net.max_drawdown
    );
    println!(
        "    Buy & hold:    {:>10.2}  ({:>+7.2}%)   max DD {:>6.2}%",
        hold_balance,
        ret(*hold_balance),
        hold_drawdown
    );

    let edge = ret(net.final_balance) - ret(*hold_balance);
    println!("    Difference:    {edge:>+10.2} pts");
    if edge < 0.0 {
        println!(
            "    >> Holding beat the strategy on return. It is only worth \n\
             \x20      trading if the smaller drawdown is worth {:.2} pts.",
            -edge
        );
    }
}

/// Re-run the same signals at a range of position sizes.
///
/// This is not a search for a "best" parameter — the signals are identical in
/// every row, only the stake changes. It shows the return/drawdown trade-off
/// the operator is actually choosing between, and where the balance cap stops
/// buying more exposure.
fn print_risk_sweep(
    daily: &[Candle],
    four_hour: &[Candle],
    costs: &CostModel,
    symbol: &str,
    params: &StrategyParams,
) {
    println!();
    println!("  Position-size sweep (identical signals, different stake):");
    println!(
        "    {:>6}  {:>12}  {:>9}  {:>8}  {:>8}",
        "risk%", "final", "return", "max DD", "ret/DD"
    );
    println!("    {}", "-".repeat(52));

    for pct in [1.5, 3.0, 5.0, 10.0, 15.0, 20.0, 30.0] {
        let r = simulate(symbol, daily, four_hour, costs, pct / 100.0, params);
        let ret = (r.final_balance - STARTING_BALANCE) / STARTING_BALANCE * 100.0;
        let ratio = if r.max_drawdown > 0.0 {
            ret / r.max_drawdown
        } else {
            f64::INFINITY
        };
        println!(
            "    {pct:>5.1}%  {:>12.2}  {ret:>+8.2}%  {:>7.2}%  {ratio:>8.2}",
            r.final_balance, r.max_drawdown
        );
    }
    println!("    Sizing is capped at 95% of free balance, so rows converge");
    println!("    once most entries are already fully deployed.");
}

/// Split the history in half and run each period independently.
///
/// A strategy whose edge lives entirely in one half is fitted to that half, not
/// to the market. The second period gets its full daily warmup from the same
/// `daily` array, so it is a genuine out-of-sample run rather than a truncated
/// one.
fn print_period_split(
    symbol: &str,
    daily: &[Candle],
    four_hour: &[Candle],
    costs: &CostModel,
    risk_per_trade: f64,
    first_bar: Option<usize>,
    params: &StrategyParams,
) {
    let Some(first) = first_bar else { return };
    let mid = first + (four_hour.len() - first) / 2;
    if mid <= first || mid >= four_hour.len() {
        return;
    }

    let in_sample = simulate(symbol, daily, &four_hour[first..mid], costs, risk_per_trade, params);
    let out_sample = simulate(symbol, daily, &four_hour[mid..], costs, risk_per_trade, params);
    let ret = |b: f64| (b - STARTING_BALANCE) / STARTING_BALANCE * 100.0;

    println!();
    println!("  In-sample vs out-of-sample (history split in half):");
    println!(
        "    {:>12}  {:>10}  {:>9}  {:>7}  {:>8}",
        "period", "final", "return", "trades", "max DD"
    );
    println!("    {}", "-".repeat(54));
    for (label, r) in [("first half", &in_sample), ("second half", &out_sample)] {
        println!(
            "    {label:>12}  {:>10.2}  {:>+8.2}%  {:>7}  {:>7.2}%",
            r.final_balance,
            ret(r.final_balance),
            r.trades.len(),
            r.max_drawdown
        );
    }

    let (a, b) = (ret(in_sample.final_balance), ret(out_sample.final_balance));
    if a > 0.0 && b <= 0.0 {
        println!("    >> The edge is present in the first half only — that is a");
        println!("       warning sign, not a result.");
    } else if a <= 0.0 && b > 0.0 {
        println!("    >> Profit comes entirely from the later period.");
    }
}

fn print_monte_carlo_report(trades: &[SimTrade]) {
    if trades.is_empty() {
        println!("\nMonte Carlo: skipped (no completed trades)");
        return;
    }

    // Return on equity, not on the position's own notional. Sizing risks a
    // fixed fraction of equity, so a +7.7% move on a position that was 40% of
    // the book is a +3% move on the book. Compounding `pnl_pct` here overstated
    // both the upside and the drawdowns badly.
    let returns: Vec<f64> = trades.iter().map(|trade| trade.equity_return).collect();
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

    println!(
        "\n  Monte Carlo ({} runs, trade returns resampled):",
        MONTE_CARLO_RUNS
    );
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
    println!("    Note: resamples realised trades, so it assumes the edge holds.");
    println!("          Fees and slippage are included; regime change is not.");
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
    println!(
        "║  Starting Balance:  {:>10.2} USDT             ║",
        STARTING_BALANCE
    );
    println!(
        "║  Final Balance:     {:>10.2} USDT             ║",
        final_balance
    );
    println!(
        "║  Total Return:      {:>+10.2}%                 ║",
        total_return
    );
    println!(
        "║  Total PnL:         {:>+10.2} USDT             ║",
        total_pnl
    );
    println!("╠══════════════════════════════════════════════════╣");
    println!(
        "║  Total Trades:      {:>10}                   ║",
        total_trades
    );
    println!(
        "║  Winners:           {:>10}                   ║",
        win_count
    );
    println!(
        "║  Losers:            {:>10}                   ║",
        loss_count
    );
    println!(
        "║  Win Rate:          {:>10.1}%                 ║",
        win_rate
    );
    println!("╠══════════════════════════════════════════════════╣");
    println!(
        "║  Avg Win:           {:>+10.2} USDT             ║",
        avg_win
    );
    println!(
        "║  Avg Loss:          {:>+10.2} USDT             ║",
        avg_loss
    );
    println!(
        "║  Profit Factor:     {:>10.2}                   ║",
        profit_factor
    );
    println!(
        "║  Max Drawdown:      {:>10.2}%                 ║",
        max_drawdown
    );
    println!("╚══════════════════════════════════════════════════╝");

    if !trades.is_empty() {
        println!();
        println!("  Trade Log:");
        println!(
            "  {:>4}  {:>10}  {:>10}  {:>10}  {:>10}  {:>8}  {:>6}",
            "#", "Entry", "Exit", "Qty", "PnL", "PnL %", "Why"
        );
        println!("  {}", "-".repeat(70));
        for (i, t) in trades.iter().enumerate() {
            println!(
                "  {:>4}  {:>10.2}  {:>10.2}  {:>10.6}  {:>+10.2}  {:>+7.2}%  {:>6}",
                i + 1,
                t.entry_price,
                t.exit_price,
                t.quantity,
                t.pnl,
                t.pnl_pct,
                t.exit_reason.label()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{partial_daily, percentile, Candle};

    /// Four-hour bar starting at `open_time` (epoch ms).
    fn bar(open_time: u64, open: f64, high: f64, low: f64, close: f64) -> Candle {
        Candle {
            open_time,
            open,
            high,
            low,
            close,
            volume: 1.0,
            close_time: open_time + 4 * 3_600_000 - 1,
        }
    }

    const DAY: u64 = 86_400_000;
    const H4: u64 = 4 * 3_600_000;

    #[test]
    fn partial_daily_aggregates_the_current_day_only() {
        // Yesterday's final bar, then three bars of today.
        let bars = [
            bar(DAY - H4, 10.0, 11.0, 9.0, 10.5),
            bar(DAY, 100.0, 105.0, 99.0, 104.0),
            bar(DAY + H4, 104.0, 112.0, 103.0, 108.0),
            bar(DAY + 2 * H4, 108.0, 110.0, 101.0, 102.0),
        ];

        let partial = partial_daily(&bars, DAY).expect("today has bars");
        // Open from the day's first bar, close from its last.
        assert_eq!(partial.open, 100.0);
        assert_eq!(partial.close, 102.0);
        // Extremes across today only — yesterday's 9.0 low must not leak in.
        assert_eq!(partial.high, 112.0);
        assert_eq!(partial.low, 99.0);
        assert_eq!(partial.volume, 3.0);
        assert_eq!(partial.open_time, DAY);
    }

    #[test]
    fn partial_daily_handles_the_first_bar_of_a_day() {
        let bars = [
            bar(DAY - H4, 10.0, 11.0, 9.0, 10.5),
            bar(DAY, 100.0, 105.0, 99.0, 104.0),
        ];
        let partial = partial_daily(&bars, DAY).unwrap();
        assert_eq!(partial.open, 100.0);
        assert_eq!(partial.close, 104.0);
        assert_eq!(partial.high, 105.0);
        assert_eq!(partial.low, 99.0);
    }

    #[test]
    fn partial_daily_is_none_when_the_day_has_not_started() {
        // The latest bar still belongs to the previous day, so no candle is in
        // progress and the daily view must not gain a phantom entry.
        let bars = [bar(DAY - H4, 10.0, 11.0, 9.0, 10.5)];
        assert!(partial_daily(&bars, DAY).is_none());
    }

    #[test]
    fn partial_daily_is_none_for_an_empty_series() {
        assert!(partial_daily(&[], DAY).is_none());
    }

    #[test]
    fn partial_daily_never_reads_beyond_the_slice() {
        // The caller passes `&four_hour[..=j]`; the aggregate must reflect only
        // bars up to j, never a later one.
        let bars = [
            bar(DAY, 100.0, 105.0, 99.0, 104.0),
            bar(DAY + H4, 104.0, 112.0, 103.0, 108.0),
            bar(DAY + 2 * H4, 108.0, 999.0, 1.0, 102.0),
        ];
        let partial = partial_daily(&bars[..=1], DAY).unwrap();
        assert_eq!(partial.high, 112.0);
        assert_eq!(partial.low, 99.0);
        assert_eq!(partial.close, 108.0);
    }

    #[test]
    fn percentile_returns_nearest_sorted_value() {
        let values = [10.0, 20.0, 30.0, 40.0, 50.0];

        assert_eq!(percentile(&values, 0.0), 10.0);
        assert_eq!(percentile(&values, 0.5), 30.0);
        assert_eq!(percentile(&values, 1.0), 50.0);
    }
}

// ---------------------------------------------------------------------------
// Portfolio simulation
// ---------------------------------------------------------------------------

/// Candles for one symbol, plus the rolling daily view the walk maintains.
struct SymbolFeed {
    symbol: String,
    daily: Vec<Candle>,
    four_hour: Vec<Candle>,
    daily_view: Vec<Candle>,
    next_daily: usize,
}

/// Run every configured symbol against one shared balance.
///
/// This is the question the single-symbol backtest cannot answer: it discarded
/// 60% of its signals for being already-long, and a portfolio that can hold
/// several positions at once recovers exactly those.
///
/// Cash is accounted properly here rather than booking PnL on exit — with
/// concurrent positions, what limits the next entry is the cash left after the
/// previous ones.
fn simulate_portfolio(
    feeds: &mut [SymbolFeed],
    costs: &CostModel,
    risk_per_trade: f64,
    params: &StrategyParams,
) -> (SimResult, usize) {
    let mut balance = STARTING_BALANCE;
    let mut risk_manager = RiskManager::with_risk_per_trade(balance, risk_per_trade);
    let mut positions: Vec<SimPosition> = Vec::new();
    let mut trades: Vec<SimTrade> = Vec::new();
    let mut peak_equity = balance;
    let mut max_drawdown = 0.0_f64;
    let mut diagnostics = Diagnostics::default();
    let mut first_bar: Option<usize> = None;
    let mut concurrent_peak = 0usize;

    // Reset the rolling daily views so this can be called repeatedly over the
    // same feeds — a sweep re-runs identical history under different settings,
    // and one data fetch should serve all of them.
    for feed in feeds.iter_mut() {
        feed.daily_view.clear();
        feed.next_daily = 0;
    }

    let bars = feeds.iter().map(|f| f.four_hour.len()).min().unwrap_or(0);

    for j in 0..bars {
        // ---- advance every feed's daily view to this bar ----
        for feed in feeds.iter_mut() {
            let now = feed.four_hour[j].close_time;
            while feed.next_daily < feed.daily.len()
                && feed.daily[feed.next_daily].close_time <= now
            {
                let candle = feed.daily[feed.next_daily].clone();
                feed.daily_view.push(candle);
                feed.next_daily += 1;
            }
        }

        // Warmup is reached only when every symbol has enough daily history.
        if feeds.iter().any(|f| f.daily_view.len() < 200) || j + 1 < 35 {
            continue;
        }
        first_bar.get_or_insert(j);

        // Snapshot this bar per symbol, so the mutable walk below does not need
        // to borrow `feeds` a second time.
        let bar_data: HashMap<String, (f64, f64, f64)> = feeds
            .iter()
            .map(|f| {
                let c = &f.four_hour[j];
                (f.symbol.clone(), (c.low, c.high, c.close))
            })
            .collect();
        let mark = |symbol: &str, fallback: f64| {
            bar_data
                .get(symbol)
                .map_or(fallback, |(_, _, close)| *close)
        };

        // ---- exits first, so freed cash can fund entries on the same bar ----
        let mut i = 0;
        while i < positions.len() {
            let pos = &positions[i];
            let (low, high, _) = bar_data
                .get(&pos.symbol)
                .copied()
                .expect("position symbol always has a feed");

            let hit_sl = low <= pos.stop_loss;
            let hit_tp = high >= pos.take_profit;
            if hit_sl || hit_tp {
                let (level, reason) = if hit_sl {
                    (pos.stop_loss, ExitReason::StopLoss)
                } else {
                    (pos.take_profit, ExitReason::TakeProfit)
                };
                let trade = settle(pos, level, reason, costs);
                balance += trade.exit_price * pos.quantity
                    - costs.taker_cost(trade.exit_price * pos.quantity);
                trades.push(trade);
                risk_manager.close_position(&pos.symbol);
                positions.remove(i);
                continue;
            }
            i += 1;
        }

        // ---- evaluate each symbol ----
        for feed in feeds.iter_mut() {
            let bar = &feed.four_hour[j];
            let current_price = bar.close;

            let partial = feed
                .daily
                .get(feed.next_daily)
                .and_then(|next| partial_daily(&feed.four_hour[..=j], next.open_time));
            let pushed = partial.is_some();
            if let Some(candle) = partial {
                feed.daily_view.push(candle);
            }
            let snap = strategy::compute_indicators(
                &feed.daily_view,
                &feed.four_hour[..=j],
                current_price,
            );
            if pushed {
                feed.daily_view.pop();
            }

            let Some(snap) = snap else { continue };

            diagnostics.record(
                &EntryConditions::evaluate(&snap, params),
                positions.iter().any(|p| p.symbol == feed.symbol),
            );

            let signal = strategy::evaluate(&feed.symbol, &snap, params);
            let held = positions.iter().position(|p| p.symbol == feed.symbol);

            match signal.signal {
                Signal::Buy if held.is_none() && risk_manager.can_open_position() => {
                    let entry_fill = costs.buy_fill(signal.entry_price);

                    // Equity marks open positions; only cash can fund an entry.
                    let open_value: f64 = positions
                        .iter()
                        .map(|p| p.quantity * mark(&p.symbol, p.entry_price))
                        .sum();
                    let equity = balance + open_value;

                    let (qty, _) = risk_manager.calculate_position_size(
                        equity,
                        balance,
                        entry_fill,
                        signal.stop_loss,
                    );
                    if qty <= 0.0 {
                        continue;
                    }

                    let cost = entry_fill * qty;
                    let fee = costs.taker_cost(cost);
                    if cost + fee > balance {
                        continue;
                    }
                    balance -= cost + fee;

                    let take_profit = entry_fill + 2.0 * (entry_fill - signal.stop_loss);
                    positions.push(SimPosition {
                        symbol: feed.symbol.clone(),
                        entry_price: entry_fill,
                        quantity: qty,
                        stop_loss: signal.stop_loss,
                        take_profit,
                        entry_fee: fee,
                        equity_at_entry: equity,
                    });
                    risk_manager.open_position(crate::risk::Position {
                        symbol: feed.symbol.clone(),
                        entry_price: entry_fill,
                        quantity: qty,
                        stop_loss: signal.stop_loss,
                        take_profit,
                        entry_time: bar.open_time,
                        protected: true,
                    });
                    concurrent_peak = concurrent_peak.max(positions.len());
                }
                Signal::Sell => {
                    if let Some(idx) = held {
                        let pos = positions.remove(idx);
                        let trade = settle(&pos, current_price, ExitReason::Signal, costs);
                        balance += trade.exit_price * pos.quantity
                            - costs.taker_cost(trade.exit_price * pos.quantity);
                        trades.push(trade);
                        risk_manager.close_position(&pos.symbol);
                    }
                }
                _ => {}
            }
        }

        // ---- drawdown on total equity ----
        let open_value: f64 = positions
            .iter()
            .map(|p| p.quantity * mark(&p.symbol, p.entry_price))
            .sum();
        let equity = balance + open_value;
        peak_equity = peak_equity.max(equity);
        if peak_equity > 0.0 {
            max_drawdown = max_drawdown.max((peak_equity - equity) / peak_equity * 100.0);
        }
    }

    // ---- mark out anything still open ----
    let last = bars.saturating_sub(1);
    let final_marks: HashMap<String, f64> = feeds
        .iter()
        .filter_map(|f| f.four_hour.get(last).map(|c| (f.symbol.clone(), c.close)))
        .collect();
    for pos in positions.drain(..) {
        let close = final_marks
            .get(&pos.symbol)
            .copied()
            .unwrap_or(pos.entry_price);
        let trade = settle(&pos, close, ExitReason::EndOfData, costs);
        balance +=
            trade.exit_price * pos.quantity - costs.taker_cost(trade.exit_price * pos.quantity);
        trades.push(trade);
    }

    (
        SimResult {
            trades,
            final_balance: balance,
            max_drawdown,
            first_bar,
            diagnostics,
        },
        concurrent_peak,
    )
}

/// Fetch every symbol, align them onto a shared 4H timeline, and run them
/// against one balance.
async fn portfolio_backtest<M: MarketDataProvider>(
    client: &M,
    symbols: &[String],
    costs: &CostModel,
    risk_per_trade: f64,
    params: &StrategyParams,
) -> Result<(), String> {
    let mut feeds: Vec<SymbolFeed> = Vec::new();
    for symbol in symbols {
        let daily = client.klines_extended(symbol, "1d", 1000).await?;
        let four_hour = client.klines_extended(symbol, "4h", 6000).await?;
        if daily.len() < 210 || four_hour.len() < 50 {
            tracing::warn!("{symbol}: insufficient history for the portfolio run, skipping");
            continue;
        }
        feeds.push(SymbolFeed {
            symbol: symbol.clone(),
            daily,
            four_hour,
            daily_view: Vec::new(),
            next_daily: 0,
        });
    }

    if feeds.len() < 2 {
        return Ok(());
    }

    // Align on the intersection of bar timestamps. Trimming to a common start
    // is not enough: a single missing bar on one symbol would shift every later
    // index and silently compare different moments in time.
    let mut common: std::collections::HashSet<u64> =
        feeds[0].four_hour.iter().map(|c| c.open_time).collect();
    for feed in &feeds[1..] {
        let times: std::collections::HashSet<u64> =
            feed.four_hour.iter().map(|c| c.open_time).collect();
        common = common.intersection(&times).copied().collect();
    }
    for feed in feeds.iter_mut() {
        feed.four_hour.retain(|c| common.contains(&c.open_time));
    }

    let aligned = feeds[0].four_hour.len();
    if feeds.iter().any(|f| f.four_hour.len() != aligned) {
        return Err("portfolio feeds could not be aligned".to_string());
    }

    info!(
        "Portfolio: {} symbols, {} aligned 4H bars (~{:.0} days)",
        feeds.len(),
        aligned,
        aligned as f64 / BARS_PER_DAY
    );

    let (result, concurrent_peak) = simulate_portfolio(&mut feeds, costs, risk_per_trade, params);

    println!();
    println!("=============================================================");
    println!("  PORTFOLIO: {} symbols, shared balance", feeds.len());
    println!("=============================================================");
    print_diagnostics(&result.diagnostics, feeds.len());

    let hold = portfolio_buy_and_hold(&feeds, costs, result.first_bar.unwrap_or(0));
    let ret = |b: f64| (b - STARTING_BALANCE) / STARTING_BALANCE * 100.0;

    println!();
    println!("  Portfolio vs equal-weight hold:");
    println!(
        "    Strategy:      {:>10.2}  ({:>+7.2}%)   max DD {:>6.2}%   {} trades",
        result.final_balance,
        ret(result.final_balance),
        result.max_drawdown,
        result.trades.len()
    );
    println!(
        "    Equal-weight:  {:>10.2}  ({:>+7.2}%)   max DD {:>6.2}%",
        hold.0,
        ret(hold.0),
        hold.1
    );
    println!("    Peak concurrent positions: {concurrent_peak}");

    print_report(
        "PORTFOLIO",
        &result.trades,
        result.final_balance,
        result.max_drawdown,
    );
    print_monte_carlo_report(&result.trades);
    print_buffer_sweep(&mut feeds, costs, risk_per_trade);

    Ok(())
}

/// Re-run the portfolio under a range of ATR signal buffers.
///
/// Entry and exit currently share the EMA50 line, so price hovering there
/// produces a buy, a noise stop-out, and a buy again — each round trip paying
/// fees and slippage. A buffer separates the two thresholds into a dead zone.
/// Every row trades the same instruments over the same history; only the
/// threshold moves.
fn print_buffer_sweep(feeds: &mut [SymbolFeed], costs: &CostModel, risk_per_trade: f64) {
    println!();
    println!("  ATR signal-buffer sweep (same signals, different thresholds):");
    println!(
        "    {:>6} {:>6}  {:>10}  {:>9}  {:>7}  {:>7}  {:>8}",
        "entry", "exit", "final", "return", "trades", "win%", "max DD"
    );
    println!("    {}", "-".repeat(62));

    for (entry, exit) in [
        (0.0, 0.0),
        (0.0, 0.5),
        (0.0, 1.0),
        (0.5, 0.5),
        (0.5, 1.0),
        (1.0, 1.0),
        (1.0, 2.0),
    ] {
        let params = StrategyParams {
            entry_buffer_atr: entry,
            exit_buffer_atr: exit,
        };
        let (r, _) = simulate_portfolio(feeds, costs, risk_per_trade, &params);
        let wins = r.trades.iter().filter(|t| t.pnl > 0.0).count();
        let win_rate = if r.trades.is_empty() {
            0.0
        } else {
            wins as f64 / r.trades.len() as f64 * 100.0
        };
        println!(
            "    {entry:>6.1} {exit:>6.1}  {:>10.2}  {:>+8.2}%  {:>7}  {win_rate:>6.1}%  {:>7.2}%",
            r.final_balance,
            (r.final_balance - STARTING_BALANCE) / STARTING_BALANCE * 100.0,
            r.trades.len(),
            r.max_drawdown
        );
    }
}

/// Equal-weight buy and hold across the same symbols and window.
///
/// `first_bar` is where the strategy actually began trading. Holding from bar
/// zero would credit the benchmark with the entire indicator warmup — a
/// stretch the strategy sat out — and on this data that alone turned a losing
/// benchmark into a +63% one.
fn portfolio_buy_and_hold(
    feeds: &[SymbolFeed],
    costs: &CostModel,
    first_bar: usize,
) -> (f64, f64) {
    let per_symbol = STARTING_BALANCE / feeds.len() as f64;
    let bars = feeds.iter().map(|f| f.four_hour.len()).min().unwrap_or(0);
    if bars == 0 || first_bar >= bars {
        return (STARTING_BALANCE, 0.0);
    }

    let quantities: Vec<f64> = feeds
        .iter()
        .map(|f| {
            let entry = costs.buy_fill(f.four_hour[first_bar].close);
            if entry > 0.0 {
                (per_symbol - costs.taker_cost(per_symbol)) / entry
            } else {
                0.0
            }
        })
        .collect();

    let mut peak = STARTING_BALANCE;
    let mut max_drawdown: f64 = 0.0;
    for j in first_bar..bars {
        let low: f64 = feeds
            .iter()
            .zip(&quantities)
            .map(|(f, q)| f.four_hour[j].low * q)
            .sum();
        let close: f64 = feeds
            .iter()
            .zip(&quantities)
            .map(|(f, q)| f.four_hour[j].close * q)
            .sum();
        peak = peak.max(close);
        if peak > 0.0 {
            max_drawdown = max_drawdown.max((peak - low) / peak * 100.0);
        }
    }

    let final_value: f64 = feeds
        .iter()
        .zip(&quantities)
        .map(|(f, q)| costs.sell_fill(f.four_hour[bars - 1].close) * q)
        .sum();
    (final_value - costs.taker_cost(final_value), max_drawdown)
}
