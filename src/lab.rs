//! Strategy lab: replay every strategy on the live timeframes and pairs, with
//! costs, and say whether anything beats what is live.
//!
//! Runs as `MODE=lab`. Each signal bar is evaluated with the inputs the live
//! loop fetches: the last 250 trend candles including the one still forming
//! (rebuilt from signal bars, so nothing later is read) and the last 100
//! signal candles. Stops, targets, the trailing stop and costs follow the live
//! executor.
//!
//! One difference remains: the lab decides at each signal bar's close, while
//! live polls mid-bar on the ticker price. Before a strategy other than
//! `trend_pullback` goes live, live must evaluate it on closed bars too.
//!
//! History is split by time into an earlier `IN_SAMPLE_SHARE` and a later
//! out-of-sample period. A recommendation needs both, and is ranked on the
//! later one.

use crate::backtest;
use crate::config::Config;
use crate::costs::CostModel;
use crate::exchange::{Candle, MarketDataProvider};
use crate::strategies::{StrategyKind, REWARD_RISK};
use crate::strategy::{self, Signal, StrategyParams};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use serde_json::json;
use tracing::{error, info};

const DEFAULT_LAB_DAYS: u64 = 120;
/// Binance pagination makes very long windows slow; two years is plenty.
const MAX_LAB_DAYS: u64 = 730;
/// Fixed so the same candles always give the same report.
const MONTE_CARLO_SEED: u64 = 0x7e3_9e57;
const IN_SAMPLE_SHARE: f64 = 0.7;
/// Fewer out-of-sample trades than this is noise, not evidence.
const MIN_OOS_TRADES: usize = 30;
/// Required out-of-sample profit factor before anything is recommended.
const MIN_OOS_PROFIT_FACTOR: f64 = 1.1;
const MONTE_CARLO_RUNS: usize = 2_000;
const MONTE_CARLO_TRADES: usize = 100;
/// Same windows the live loop requests.
const TREND_WINDOW: usize = 250;
const SIGNAL_WINDOW: usize = 100;
/// Largest share of equity one entry may use, as in `risk.rs`.
const MAX_NOTIONAL_FRACTION: f64 = 0.95;

#[derive(Debug, Clone)]
struct LabTrade {
    strategy: StrategyKind,
    exit_time: u64,
    /// Net return on account equity, after fees and slippage.
    ret: f64,
    out_of_sample: bool,
}

#[derive(Debug, Clone, Copy)]
struct OpenTrade {
    entry_time: u64,
    entry_price: f64,
    stop: f64,
    target: f64,
    /// Share of equity committed.
    notional: f64,
    /// Highest high since entry, for the trailing stop.
    highest_high: f64,
    /// Trend ATR at entry, held fixed as the live executor does.
    atr_at_entry: f64,
}

pub async fn run<M: MarketDataProvider>(client: &M, config: &Config) {
    let days = std::env::var("LAB_DAYS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|d| (1..=MAX_LAB_DAYS).contains(d))
        .unwrap_or(DEFAULT_LAB_DAYS);
    let costs = CostModel::from_env();
    let params = StrategyParams::from_env();
    let live = config.strategy;
    let (Some(trend_ms), Some(signal_ms)) = (
        interval_ms(&config.trend_interval),
        interval_ms(&config.signal_interval),
    ) else {
        error!("lab: unsupported interval");
        return;
    };

    info!(
        "=== LAB === {days} days | trend {} | signal {} | {} pairs | live: {}",
        config.trend_interval,
        config.signal_interval,
        config.trading_pairs.len(),
        live.name()
    );

    let signal_bars = (days * 86_400_000 / signal_ms) as usize;
    let trend_bars = (signal_bars as u64 * signal_ms / trend_ms) as usize + TREND_WINDOW + 10;

    let mut trades: Vec<LabTrade> = Vec::new();
    for symbol in &config.trading_pairs {
        let trend = match client
            .klines_extended(symbol, &config.trend_interval, trend_bars)
            .await
        {
            Ok(c) => c,
            Err(e) => {
                error!("lab {symbol}: trend klines failed: {e}");
                continue;
            }
        };
        let signal = match client
            .klines_extended(symbol, &config.signal_interval, signal_bars)
            .await
        {
            Ok(c) => c,
            Err(e) => {
                error!("lab {symbol}: signal klines failed: {e}");
                continue;
            }
        };
        // The newest candle of each series may still be forming; the lab only
        // trades bars that have closed.
        let now = now_ms();
        let trend: Vec<Candle> = trend.into_iter().filter(|c| c.close_time <= now).collect();
        let signal: Vec<Candle> = signal.into_iter().filter(|c| c.close_time <= now).collect();
        let found = simulate(&trend, &signal, config.risk_per_trade, &costs, &params);
        info!("lab {symbol}: {} trades across strategies", found.len());
        trades.extend(found);
    }

    let report = build_report(&trades, live, days, config);
    print_table(&report);
    // One line the Trading department parses; keep the prefix stable.
    println!("LAB_REPORT {report}");
}

/// Replay one pair. Pure, so it is testable on synthetic candles.
fn simulate(
    trend: &[Candle],
    signal: &[Candle],
    risk_per_trade: f64,
    costs: &CostModel,
    params: &StrategyParams,
) -> Vec<LabTrade> {
    let mut trades = Vec::new();
    let Some(first) = signal.first() else {
        return trades;
    };
    let last = signal.last().map_or(first.close_time, |c| c.close_time);
    let split_time = first.open_time + ((last - first.open_time) as f64 * IN_SAMPLE_SHARE) as u64;

    let mut open: Vec<Option<OpenTrade>> = vec![None; StrategyKind::ALL.len()];
    let mut closed_trend = 0usize;
    let mut trend_view: Vec<Candle> = Vec::with_capacity(TREND_WINDOW);

    for j in 0..signal.len() {
        let bar = &signal[j];
        while closed_trend < trend.len() && trend[closed_trend].close_time <= bar.close_time {
            closed_trend += 1;
        }
        if closed_trend < 200 || j + 1 < 35 {
            continue;
        }

        // Exits on resting levels first, against this bar's range, for
        // positions opened on an earlier bar. A bar that touches both resolves
        // as the stop; a bar that opens through the stop fills at its open.
        for (k, slot) in open.iter_mut().enumerate() {
            let Some(pos) = slot.as_mut() else { continue };
            if let Some((price, resting)) = exit_on_bar(pos, bar, params, costs) {
                let kind = StrategyKind::ALL[k];
                trades.push(close(kind, pos, price, resting, bar, split_time, costs));
                *slot = None;
            } else {
                // Ratchet after the exit test, never before it.
                pos.highest_high = pos.highest_high.max(bar.high);
            }
        }

        // What the venue reports at this bar: the closed trend candles plus
        // the one still forming, rebuilt from signal bars so nothing later
        // than this bar is read.
        trend_view.clear();
        let forming = trend
            .get(closed_trend)
            .and_then(|next| backtest::partial_daily(&signal[..=j], next.open_time));
        let keep = TREND_WINDOW - usize::from(forming.is_some());
        trend_view.extend_from_slice(&trend[closed_trend.saturating_sub(keep)..closed_trend]);
        trend_view.extend(forming);

        let signal_view = &signal[(j + 1).saturating_sub(SIGNAL_WINDOW)..=j];
        let Some(snap) = strategy::compute_indicators(&trend_view, signal_view, bar.close) else {
            continue;
        };

        for (k, kind) in StrategyKind::ALL.into_iter().enumerate() {
            let decision = kind.evaluate("LAB", &snap, signal_view, params);
            match (decision.signal, open[k]) {
                (Signal::Sell, Some(pos)) => {
                    let price = costs.sell_fill(bar.close);
                    trades.push(close(kind, &pos, price, false, bar, split_time, costs));
                    open[k] = None;
                }
                (Signal::Buy, None) => {
                    let entry_price = costs.buy_fill(decision.entry_price);
                    let stop_pct = (entry_price - decision.stop_loss) / entry_price;
                    if !(stop_pct > 0.0 && stop_pct.is_finite()) {
                        continue;
                    }
                    open[k] = Some(OpenTrade {
                        entry_time: bar.close_time,
                        entry_price,
                        stop: decision.stop_loss,
                        // Re-derived from the fill, as the live executor does.
                        target: entry_price + REWARD_RISK * (entry_price - decision.stop_loss),
                        notional: (risk_per_trade / stop_pct).min(MAX_NOTIONAL_FRACTION),
                        highest_high: entry_price,
                        atr_at_entry: decision.atr,
                    });
                }
                _ => {}
            }
        }
    }

    // Anything still open is marked to the last close.
    if let Some(bar) = signal.last() {
        for (k, slot) in open.iter().enumerate() {
            if let Some(pos) = slot {
                let price = costs.sell_fill(bar.close);
                let kind = StrategyKind::ALL[k];
                trades.push(close(kind, pos, price, false, bar, split_time, costs));
            }
        }
    }
    trades
}

/// Whether a resting stop or target fills inside `bar`, and at what price.
///
/// A bar that touches both resolves as the stop, the conservative reading. A
/// bar that opens through a level fills at the open: a stop then sells lower
/// than its trigger, a target higher than its limit.
fn exit_on_bar(
    pos: &OpenTrade,
    bar: &Candle,
    params: &StrategyParams,
    costs: &CostModel,
) -> Option<(f64, bool)> {
    let stop = effective_stop(pos, params);
    let trailing = params.trailing_stop_atr > 0.0;
    if bar.low <= stop {
        Some((costs.sell_fill(stop.min(bar.open)), false))
    } else if !trailing && bar.high >= pos.target {
        Some((costs.limit_fill(pos.target.max(bar.open)), true))
    } else {
        None
    }
}

/// The stop as the live executor holds it: the chandelier trail when
/// `TRAILING_STOP_ATR` is set, never below the initial stop.
fn effective_stop(pos: &OpenTrade, params: &StrategyParams) -> f64 {
    if params.trailing_stop_atr <= 0.0 || !pos.atr_at_entry.is_finite() || pos.atr_at_entry <= 0.0 {
        return pos.stop;
    }
    pos.stop
        .max(pos.highest_high - params.trailing_stop_atr * pos.atr_at_entry)
}

fn close(
    strategy: StrategyKind,
    pos: &OpenTrade,
    exit_price: f64,
    resting: bool,
    bar: &Candle,
    split_time: u64,
    costs: &CostModel,
) -> LabTrade {
    let gross = pos.notional * (exit_price / pos.entry_price - 1.0);
    let exit_notional = pos.notional * exit_price / pos.entry_price;
    let fees = costs.taker_cost(pos.notional)
        + if resting {
            costs.maker_cost(exit_notional)
        } else {
            costs.taker_cost(exit_notional)
        };
    LabTrade {
        strategy,
        exit_time: bar.close_time,
        ret: gross - fees,
        out_of_sample: pos.entry_time >= split_time,
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq)]
struct Stats {
    trades: usize,
    win_rate: f64,
    /// Mean net return per trade, as a fraction of equity.
    expectancy: f64,
    profit_factor: f64,
    /// Trades taken one after another in exit order, compounding. Overlapping
    /// trades on different pairs make this optimistic; it ranks, it does not
    /// forecast.
    compounded: f64,
    max_drawdown: f64,
}

fn stats(trades: &[&LabTrade]) -> Stats {
    if trades.is_empty() {
        return Stats::default();
    }
    let mut ordered: Vec<&&LabTrade> = trades.iter().collect();
    ordered.sort_by_key(|t| t.exit_time);
    let wins = trades.iter().filter(|t| t.ret > 0.0).count();
    let gains: f64 = trades.iter().filter(|t| t.ret > 0.0).map(|t| t.ret).sum();
    let losses: f64 = -trades
        .iter()
        .filter(|t| t.ret < 0.0)
        .map(|t| t.ret)
        .sum::<f64>();
    let (mut equity, mut peak, mut max_dd) = (1.0_f64, 1.0_f64, 0.0_f64);
    for t in ordered {
        equity *= 1.0 + t.ret;
        peak = peak.max(equity);
        max_dd = max_dd.max((peak - equity) / peak);
    }
    Stats {
        trades: trades.len(),
        win_rate: wins as f64 / trades.len() as f64,
        expectancy: trades.iter().map(|t| t.ret).sum::<f64>() / trades.len() as f64,
        profit_factor: if losses > 0.0 {
            gains / losses
        } else {
            f64::INFINITY
        },
        compounded: equity - 1.0,
        max_drawdown: max_dd,
    }
}

/// 5th-percentile compounded return over `MONTE_CARLO_TRADES` trades drawn
/// with replacement: a bad-luck run of the same edge.
fn monte_carlo_p5(returns: &[f64]) -> Option<f64> {
    if returns.len() < MIN_OOS_TRADES {
        return None;
    }
    let mut rng = StdRng::seed_from_u64(MONTE_CARLO_SEED);
    let mut outcomes: Vec<f64> = (0..MONTE_CARLO_RUNS)
        .map(|_| {
            (0..MONTE_CARLO_TRADES)
                .map(|_| 1.0 + returns[rng.gen_range(0..returns.len())])
                .product::<f64>()
                - 1.0
        })
        .collect();
    outcomes.sort_by(f64::total_cmp);
    Some(outcomes[MONTE_CARLO_RUNS / 20])
}

/// Picks a strategy only when its out-of-sample record clears every bar *and*
/// beats live out of sample. Otherwise the answer is to keep what runs.
fn recommend(rows: &[(StrategyKind, Stats, Stats)], live: StrategyKind) -> Option<StrategyKind> {
    let live_oos = rows
        .iter()
        .find(|r| r.0 == live)
        .map(|r| r.2)
        .unwrap_or_default();
    rows.iter()
        .filter(|(kind, is, oos)| {
            *kind != live
                && oos.trades >= MIN_OOS_TRADES
                && oos.profit_factor >= MIN_OOS_PROFIT_FACTOR
                && oos.expectancy > 0.0
                && is.trades >= MIN_OOS_TRADES
                && is.expectancy > 0.0
                && oos.expectancy > live_oos.expectancy
        })
        .max_by(|a, b| a.2.expectancy.total_cmp(&b.2.expectancy))
        .map(|r| r.0)
}

fn build_report(
    trades: &[LabTrade],
    live: StrategyKind,
    days: u64,
    config: &Config,
) -> serde_json::Value {
    let rows: Vec<(StrategyKind, Stats, Stats)> = StrategyKind::ALL
        .into_iter()
        .map(|kind| {
            let of = |oos: bool| -> Vec<&LabTrade> {
                trades
                    .iter()
                    .filter(|t| t.strategy == kind && t.out_of_sample == oos)
                    .collect()
            };
            (kind, stats(&of(false)), stats(&of(true)))
        })
        .collect();

    let recommendation = recommend(&rows, live);
    let as_json = |s: &Stats| {
        json!({
            "trades": s.trades,
            "win_rate": round(s.win_rate),
            "expectancy_pct": round(s.expectancy * 100.0),
            "profit_factor": if s.profit_factor.is_finite() { round(s.profit_factor) } else { 999.0 },
            "compounded_pct": round(s.compounded * 100.0),
            "max_drawdown_pct": round(s.max_drawdown * 100.0),
        })
    };
    let strategies: Vec<serde_json::Value> = rows
        .iter()
        .map(|(kind, is, oos)| {
            let oos_returns: Vec<f64> = trades
                .iter()
                .filter(|t| t.strategy == *kind && t.out_of_sample)
                .map(|t| t.ret)
                .collect();
            json!({
                "name": kind.name(),
                "live": *kind == live,
                "in_sample": as_json(is),
                "out_of_sample": as_json(oos),
                "mc_p5_100_trades_pct": monte_carlo_p5(&oos_returns).map(|v| round(v * 100.0)),
            })
        })
        .collect();

    json!({
        "days": days,
        "trend_interval": config.trend_interval,
        "signal_interval": config.signal_interval,
        "pairs": config.trading_pairs,
        "risk_per_trade_pct": round(config.risk_per_trade * 100.0),
        "live": live.name(),
        "strategies": strategies,
        "recommendation": match recommendation {
            Some(kind) => format!("switch:{}", kind.name()),
            None => "keep".to_string(),
        },
    })
}

fn print_table(report: &serde_json::Value) {
    info!("strategy          | OOS trades | win%  | exp%/trade | PF    | maxDD% | IS exp%");
    for s in report["strategies"].as_array().into_iter().flatten() {
        let oos = &s["out_of_sample"];
        info!(
            "{:<17} | {:>10} | {:>5} | {:>10} | {:>5} | {:>6} | {}",
            format!(
                "{}{}",
                s["name"].as_str().unwrap_or("?"),
                if s["live"] == true { "*" } else { "" }
            ),
            oos["trades"],
            oos["win_rate"],
            oos["expectancy_pct"],
            oos["profit_factor"],
            oos["max_drawdown_pct"],
            s["in_sample"]["expectancy_pct"],
        );
    }
    info!("recommendation: {}", report["recommendation"]);
}

fn round(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(u64::MAX, |d| d.as_millis() as u64)
}

/// Binance interval notation to milliseconds.
fn interval_ms(interval: &str) -> Option<u64> {
    let (n, unit) = interval.split_at(interval.len().checked_sub(1)?);
    let n: u64 = n.parse().ok()?;
    let unit_ms = match unit {
        "m" => 60_000,
        "h" => 3_600_000,
        "d" => 86_400_000,
        "w" => 604_800_000,
        _ => return None,
    };
    Some(n * unit_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candle(open_time: u64, len: u64, close: f64) -> Candle {
        Candle {
            open_time,
            open: close,
            high: close * 1.002,
            low: close * 0.998,
            close,
            volume: 1.0,
            close_time: open_time + len - 1,
        }
    }

    fn trade(strategy: StrategyKind, ret: f64, oos: bool, t: u64) -> LabTrade {
        LabTrade {
            strategy,
            exit_time: t + 1,
            ret,
            out_of_sample: oos,
        }
    }

    #[test]
    fn intervals_convert_to_milliseconds() {
        assert_eq!(interval_ms("15m"), Some(900_000));
        assert_eq!(interval_ms("4h"), Some(14_400_000));
        assert_eq!(interval_ms("1d"), Some(86_400_000));
        assert_eq!(interval_ms("x"), None);
    }

    #[test]
    fn stats_measure_expectancy_profit_factor_and_drawdown() {
        let t: Vec<LabTrade> = vec![
            trade(StrategyKind::Breakout, 0.10, true, 1),
            trade(StrategyKind::Breakout, -0.05, true, 2),
            trade(StrategyKind::Breakout, 0.02, true, 3),
        ];
        let refs: Vec<&LabTrade> = t.iter().collect();
        let s = stats(&refs);
        assert_eq!(s.trades, 3);
        assert!((s.expectancy - 0.07 / 3.0).abs() < 1e-12);
        assert!((s.profit_factor - 0.12 / 0.05).abs() < 1e-12);
        // Peak 1.10, trough 1.045: a 5% drawdown.
        assert!((s.max_drawdown - 0.05).abs() < 1e-12);
    }

    #[test]
    fn nothing_is_recommended_on_too_few_trades() {
        let strong = Stats {
            trades: MIN_OOS_TRADES - 1,
            expectancy: 0.05,
            profit_factor: 3.0,
            ..Default::default()
        };
        let rows = [
            (
                StrategyKind::TrendPullback,
                Stats::default(),
                Stats::default(),
            ),
            (StrategyKind::Breakout, strong, strong),
        ];
        assert_eq!(recommend(&rows, StrategyKind::TrendPullback), None);
    }

    #[test]
    fn a_strategy_that_beats_live_out_of_sample_is_recommended() {
        let good = Stats {
            trades: 40,
            expectancy: 0.004,
            profit_factor: 1.4,
            ..Default::default()
        };
        let weak = Stats {
            trades: 40,
            expectancy: 0.001,
            profit_factor: 1.05,
            ..Default::default()
        };
        let rows = [
            (StrategyKind::TrendPullback, weak, weak),
            (StrategyKind::Breakout, good, good),
            (StrategyKind::MeanReversion, weak, weak),
        ];
        assert_eq!(
            recommend(&rows, StrategyKind::TrendPullback),
            Some(StrategyKind::Breakout)
        );
    }

    #[test]
    fn an_in_sample_loser_is_never_recommended() {
        // Good recent numbers on a strategy that lost money before are luck.
        let oos = Stats {
            trades: 40,
            expectancy: 0.01,
            profit_factor: 2.0,
            ..Default::default()
        };
        let is = Stats {
            expectancy: -0.002,
            ..oos
        };
        let rows = [
            (
                StrategyKind::TrendPullback,
                Stats::default(),
                Stats::default(),
            ),
            (StrategyKind::MeanReversion, is, oos),
        ];
        assert_eq!(recommend(&rows, StrategyKind::TrendPullback), None);
    }

    #[test]
    fn simulate_trades_after_warmup_and_books_finite_returns() {
        // A rising trend series, then signal bars that keep making new highs
        // and pulling back: breakouts enter, pullbacks exit.
        let trend: Vec<Candle> = (0..260)
            .map(|i| candle(i * 14_400_000, 14_400_000, 100.0 + i as f64 * 0.1))
            .collect();
        let start = 210 * 14_400_000;
        let signal: Vec<Candle> = (0..2_000)
            .map(|i| {
                let x = i as f64;
                let close = 140.0 + 5.0 * (x * 0.03).sin() + x * 0.002;
                // Tight bars, so a rising close clears the prior highs.
                Candle {
                    high: close + 0.01,
                    low: close - 0.01,
                    ..candle(start + i * 900_000, 900_000, close)
                }
            })
            .collect();
        let trades = simulate(
            &trend,
            &signal,
            0.10,
            &CostModel::default(),
            &StrategyParams::default(),
        );
        assert!(
            trades.iter().any(|t| t.strategy == StrategyKind::Breakout),
            "fixture must exercise the simulator"
        );
        for t in &trades {
            assert!(t.exit_time > signal[34].close_time, "traded during warmup");
            assert!(t.ret.is_finite());
            assert!(t.ret.abs() < MAX_NOTIONAL_FRACTION);
        }
    }

    fn open_trade() -> OpenTrade {
        OpenTrade {
            entry_time: 0,
            entry_price: 100.0,
            stop: 95.0,
            target: 110.0,
            notional: 0.5,
            highest_high: 100.0,
            atr_at_entry: 2.0,
        }
    }

    fn range(open: f64, low: f64, high: f64) -> Candle {
        Candle {
            open_time: 0,
            open,
            high,
            low,
            close: open,
            volume: 1.0,
            close_time: 1,
        }
    }

    #[test]
    fn a_bar_touching_both_levels_is_a_stop() {
        let free = CostModel::frictionless();
        let hit = exit_on_bar(
            &open_trade(),
            &range(100.0, 94.0, 111.0),
            &Default::default(),
            &free,
        );
        assert_eq!(hit, Some((95.0, false)));
    }

    #[test]
    fn a_gap_through_the_stop_fills_at_the_open() {
        let free = CostModel::frictionless();
        let hit = exit_on_bar(
            &open_trade(),
            &range(90.0, 89.0, 91.0),
            &Default::default(),
            &free,
        );
        assert_eq!(hit, Some((90.0, false)));
    }

    #[test]
    fn a_trailing_stop_replaces_the_target() {
        let free = CostModel::frictionless();
        let params = StrategyParams {
            trailing_stop_atr: 2.0,
            ..Default::default()
        };
        let mut pos = open_trade();
        // Past the 2R target, but trailing: no exit.
        assert_eq!(
            exit_on_bar(&pos, &range(105.0, 104.0, 112.0), &params, &free),
            None
        );
        // After the high reaches 112 the stop trails to 108.
        pos.highest_high = 112.0;
        let hit = exit_on_bar(&pos, &range(109.0, 107.0, 109.5), &params, &free);
        assert_eq!(hit, Some((108.0, false)));
    }

    #[test]
    fn fees_and_slippage_reduce_the_booked_return() {
        let pos = open_trade();
        let bar = range(110.0, 109.0, 111.0);
        let free = close(
            StrategyKind::Breakout,
            &pos,
            110.0,
            true,
            &bar,
            0,
            &CostModel::frictionless(),
        );
        // 50% of equity, +10%: +5% before costs.
        assert!((free.ret - 0.05).abs() < 1e-12);
        let costed = close(
            StrategyKind::Breakout,
            &pos,
            110.0,
            true,
            &bar,
            0,
            &CostModel::default(),
        );
        assert!(costed.ret < free.ret);
        assert!(
            costed.out_of_sample,
            "entry at the split boundary counts as out of sample"
        );
    }

    #[test]
    fn monte_carlo_is_reproducible_and_needs_enough_trades() {
        assert_eq!(monte_carlo_p5(&[0.01; MIN_OOS_TRADES - 1]), None);
        let returns: Vec<f64> = (0..60)
            .map(|i| if i % 3 == 0 { -0.02 } else { 0.015 })
            .collect();
        assert_eq!(monte_carlo_p5(&returns), monte_carlo_p5(&returns));
    }
}
