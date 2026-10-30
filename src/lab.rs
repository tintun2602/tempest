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
//!
//! `LAB_MARKET=futures` replays Binance USDⓈ-M perpetuals instead, from the
//! public data archive: every strategy also trades its short-side mirror, with
//! leverage up to `LEVERAGE` (default 3, maximum 5), funding every 8 hours,
//! futures fees, and no entry whose stop lies beyond the liquidation price.
//! `MARKET=futures` trades the same rules live (see `futures.rs`).
//!
//! The lab also sweeps `trend_pullback`'s EMA50 buffers (`EXIT_BUFFER_ATR` x
//! `ENTRY_BUFFER_ATR`) and recommends a pair under the same bars, reported as
//! `buffer_grid` in the `LAB_REPORT` line.

use crate::archive;
use crate::backtest;
use crate::config::Config;
use crate::costs::CostModel;
use crate::exchange::{Candle, MarketDataProvider};
use crate::strategies::{StrategyKind, REWARD_RISK};
use crate::strategy::{self, ShortDecision, Signal, StrategyParams};
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
pub(crate) const SIGNAL_WINDOW: usize = 100;
/// Largest share of equity one entry may use, as in `risk.rs`. On futures
/// this is per unit of leverage.
const MAX_NOTIONAL_FRACTION: f64 = 0.95;
/// Highest leverage the futures lab accepts, as live does.
const MAX_LEVERAGE: f64 = 5.0;
const DEFAULT_LEVERAGE: f64 = 3.0;
/// Maintenance margin rate assumed for the liquidation price. Binance's lowest
/// BTC tier is 0.4%; rounded up so the estimate errs toward liquidating early.
const MAINT_MARGIN: f64 = 0.005;
/// Perpetuals fund every 8 hours.
const FUNDING_INTERVAL_MS: u64 = 8 * 3_600_000;
/// Funding events averaged to estimate the stretch the archive has not
/// published yet: the last ten days.
const FUNDING_ESTIMATE_EVENTS: usize = 30;

/// Where the lab trades.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Venue {
    /// USDⓈ-M perpetuals: shorts, leverage, funding and a liquidation price.
    /// Otherwise spot: long only, no leverage.
    futures: bool,
    /// Leverage set on the exchange, isolated margin. Caps notional at
    /// `MAX_NOTIONAL_FRACTION` x leverage and sets the liquidation price.
    leverage: f64,
}

impl Venue {
    const SPOT: Venue = Venue {
        futures: false,
        leverage: 1.0,
    };

    /// `LAB_MARKET=futures` with `LEVERAGE` (1-5, default 3); spot otherwise.
    fn from_env() -> Self {
        if std::env::var("LAB_MARKET").unwrap_or_default().trim() != "futures" {
            return Self::SPOT;
        }
        let leverage = std::env::var("LEVERAGE")
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|v| v.is_finite())
            .unwrap_or(DEFAULT_LEVERAGE)
            .clamp(1.0, MAX_LEVERAGE);
        Venue {
            futures: true,
            leverage,
        }
    }

    fn name(&self) -> &'static str {
        if self.futures {
            "futures"
        } else {
            "spot"
        }
    }

    /// Price at which an isolated position at this leverage is liquidated, or
    /// `None` on spot.
    fn liquidation_price(&self, entry: f64, short: bool) -> Option<f64> {
        if !self.futures {
            return None;
        }
        let distance = 1.0 / self.leverage - MAINT_MARGIN;
        Some(if short {
            entry * (1.0 + distance)
        } else {
            entry * (1.0 - distance)
        })
    }
}

/// One pair's history: trend and signal candles, and funding events
/// `(time_ms, rate)` (empty on spot).
struct Feed {
    trend: Vec<Candle>,
    signal: Vec<Candle>,
    funding: Vec<(u64, f64)>,
}

#[derive(Debug, Clone)]
struct LabTrade {
    strategy: StrategyKind,
    short: bool,
    exit_time: u64,
    /// Net return on account equity, after fees, slippage and funding.
    ret: f64,
    out_of_sample: bool,
}

#[derive(Debug, Clone, Copy)]
struct OpenTrade {
    short: bool,
    entry_time: u64,
    entry_price: f64,
    stop: f64,
    target: f64,
    /// Share of equity committed.
    notional: f64,
    /// Best price since entry, for the trailing stop: the highest high on a
    /// long, the lowest low on a short.
    extreme: f64,
    /// Trend ATR at entry, held fixed as the live executor does.
    atr_at_entry: f64,
    /// Funding paid so far as a share of equity; negative when received.
    funding: f64,
    /// Where the venue would liquidate, if it can.
    liquidation: Option<f64>,
}

pub async fn run<M: MarketDataProvider>(client: &M, config: &Config) {
    let days = std::env::var("LAB_DAYS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|d| (1..=MAX_LAB_DAYS).contains(d))
        .unwrap_or(DEFAULT_LAB_DAYS);
    let venue = Venue::from_env();
    let costs = if venue.futures {
        CostModel::futures_from_env()
    } else {
        CostModel::from_env()
    };
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
        "=== LAB === {days} days | {} | trend {} | signal {} | {} pairs | live: {}",
        if venue.futures {
            format!("futures {}x", venue.leverage)
        } else {
            "spot".to_string()
        },
        config.trend_interval,
        config.signal_interval,
        config.trading_pairs.len(),
        live.name()
    );

    let signal_bars = (days * 86_400_000 / signal_ms) as usize;
    let trend_bars = (signal_bars as u64 * signal_ms / trend_ms) as usize + TREND_WINDOW + 10;
    let http = reqwest::Client::new();

    let mut trades: Vec<LabTrade> = Vec::new();
    let mut feeds: Vec<Feed> = Vec::new();
    for symbol in &config.trading_pairs {
        let loaded = if venue.futures {
            load_futures(
                &http,
                symbol,
                config,
                trend_bars,
                trend_ms,
                signal_bars,
                signal_ms,
            )
            .await
        } else {
            load_spot(client, symbol, config, trend_bars, signal_bars).await
        };
        let feed = match loaded {
            Ok(feed) => feed,
            Err(e) => {
                error!("lab {symbol}: {e}");
                continue;
            }
        };
        let found = simulate(
            &feed,
            config.risk_per_trade,
            &costs,
            &params,
            venue,
            &StrategyKind::ALL,
        );
        info!("lab {symbol}: {} trades across strategies", found.len());
        trades.extend(found);
        feeds.push(feed);
    }

    let mut report = build_report(&trades, live, days, config, venue);
    let grid = buffer_grid(&feeds, config.risk_per_trade, &costs, &params, venue);
    report["buffer_grid"] = buffer_grid_json(&grid, &params);
    print_table(&report);
    print_buffer_grid(&report["buffer_grid"]);
    // One line the Trading department parses; keep the prefix stable.
    println!("LAB_REPORT {report}");
}

async fn load_spot<M: MarketDataProvider>(
    client: &M,
    symbol: &str,
    config: &Config,
    trend_bars: usize,
    signal_bars: usize,
) -> Result<Feed, String> {
    let trend = client
        .klines_extended(symbol, &config.trend_interval, trend_bars)
        .await
        .map_err(|e| format!("trend klines failed: {e}"))?;
    let signal = client
        .klines_extended(symbol, &config.signal_interval, signal_bars)
        .await
        .map_err(|e| format!("signal klines failed: {e}"))?;
    Ok(closed_only(Feed {
        trend,
        signal,
        funding: Vec::new(),
    }))
}

/// Perpetual candles and funding from Binance's public archive, which answers
/// even where the futures API refuses service.
async fn load_futures(
    http: &reqwest::Client,
    symbol: &str,
    config: &Config,
    trend_bars: usize,
    trend_ms: u64,
    signal_bars: usize,
    signal_ms: u64,
) -> Result<Feed, String> {
    let end = now_ms();
    let trend_start = end.saturating_sub(trend_bars as u64 * trend_ms);
    let signal_start = end.saturating_sub(signal_bars as u64 * signal_ms);
    let trend =
        archive::futures_klines(http, symbol, &config.trend_interval, trend_start, end).await?;
    let signal =
        archive::futures_klines(http, symbol, &config.signal_interval, signal_start, end).await?;
    let mut funding = archive::funding_rates(http, symbol, signal_start, end).await?;
    if signal.is_empty() {
        return Err("no futures candles in the archive".into());
    }
    let last_bar = signal.last().map_or(end, |c| c.close_time);
    let archived = funding.len();
    extend_funding(&mut funding, last_bar);
    if funding.len() > archived {
        info!(
            "lab {symbol}: {} recent funding events not archived yet; estimated at the \
             last ten days' mean",
            funding.len() - archived
        );
    }
    Ok(closed_only(Feed {
        trend,
        signal,
        funding,
    }))
}

/// Drop candles still forming: the lab only trades bars that have closed.
fn closed_only(mut feed: Feed) -> Feed {
    let now = now_ms();
    feed.trend.retain(|c| c.close_time <= now);
    feed.signal.retain(|c| c.close_time <= now);
    feed
}

/// The archive publishes funding a month at a time, so the running month has
/// none. Fill it every 8 hours up to `until_ms` with the mean of the latest
/// archived rates, rather than pretend those positions paid nothing.
fn extend_funding(funding: &mut Vec<(u64, f64)>, until_ms: u64) {
    let Some(&(last, _)) = funding.last() else {
        return;
    };
    let recent = &funding[funding.len().saturating_sub(FUNDING_ESTIMATE_EVENTS)..];
    let mean = recent.iter().map(|(_, r)| r).sum::<f64>() / recent.len() as f64;
    let mut t = last + FUNDING_INTERVAL_MS;
    while t <= until_ms {
        funding.push((t, mean));
        t += FUNDING_INTERVAL_MS;
    }
}

/// Replay one pair. Pure, so it is testable on synthetic candles.
fn simulate(
    feed: &Feed,
    risk_per_trade: f64,
    costs: &CostModel,
    params: &StrategyParams,
    venue: Venue,
    kinds: &[StrategyKind],
) -> Vec<LabTrade> {
    let (trend, signal) = (&feed.trend, &feed.signal);
    let mut trades = Vec::new();
    let Some(first) = signal.first() else {
        return trades;
    };
    let last = signal.last().map_or(first.close_time, |c| c.close_time);
    let split_time = first.open_time + ((last - first.open_time) as f64 * IN_SAMPLE_SHARE) as u64;

    let mut open: Vec<Option<OpenTrade>> = vec![None; kinds.len()];
    let mut closed_trend = 0usize;
    let mut next_funding = 0usize;
    let mut trend_view: Vec<Candle> = Vec::with_capacity(TREND_WINDOW);

    for j in 0..signal.len() {
        let bar = &signal[j];
        while closed_trend < trend.len() && trend[closed_trend].close_time <= bar.close_time {
            closed_trend += 1;
        }

        // Exits on resting levels first, against this bar's range, for
        // positions opened on an earlier bar. A bar that touches both resolves
        // as the stop; a bar that opens through the stop fills at its open.
        for (k, slot) in open.iter_mut().enumerate() {
            let Some(pos) = slot.as_mut() else { continue };
            if let Some((price, resting)) = exit_on_bar(pos, bar, params, costs) {
                trades.push(close(kinds[k], pos, price, resting, bar, split_time, costs));
                *slot = None;
            } else if pos.short {
                // Ratchet after the exit test, never before it.
                pos.extreme = pos.extreme.min(bar.low);
            } else {
                pos.extreme = pos.extreme.max(bar.high);
            }
        }

        // Funding settled during this bar, on what is still open, at the
        // bar's close as the mark. Longs pay a positive rate, shorts receive.
        while next_funding < feed.funding.len() && feed.funding[next_funding].0 <= bar.close_time {
            accrue_funding(&mut open, feed.funding[next_funding].1, bar.close);
            next_funding += 1;
        }

        if closed_trend < 200 || j + 1 < 35 {
            continue;
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

        for (k, &kind) in kinds.iter().enumerate() {
            match open[k] {
                Some(pos) if pos.short => {
                    if kind.evaluate_short(&snap, signal_view, params) == ShortDecision::Cover {
                        let price = costs.buy_fill(bar.close);
                        trades.push(close(kind, &pos, price, false, bar, split_time, costs));
                        open[k] = None;
                    }
                }
                Some(pos) => {
                    let decision = kind.evaluate("LAB", &snap, signal_view, params);
                    if decision.signal == Signal::Sell {
                        let price = costs.sell_fill(bar.close);
                        trades.push(close(kind, &pos, price, false, bar, split_time, costs));
                        open[k] = None;
                    }
                }
                None => {
                    let decision = kind.evaluate("LAB", &snap, signal_view, params);
                    open[k] = if decision.signal == Signal::Buy {
                        let entry_price = costs.buy_fill(decision.entry_price);
                        open_position(
                            false,
                            entry_price,
                            decision.stop_loss,
                            decision.atr,
                            bar,
                            risk_per_trade,
                            venue,
                        )
                    } else if venue.futures {
                        match kind.evaluate_short(&snap, signal_view, params) {
                            ShortDecision::Enter { stop_loss } => open_position(
                                true,
                                costs.sell_fill(bar.close),
                                stop_loss,
                                snap.atr_14,
                                bar,
                                risk_per_trade,
                                venue,
                            ),
                            _ => None,
                        }
                    } else {
                        None
                    };
                }
            }
        }
    }

    // Anything still open is marked to the last close.
    if let Some(bar) = signal.last() {
        for (k, slot) in open.iter().enumerate() {
            if let Some(pos) = slot {
                let price = if pos.short {
                    costs.buy_fill(bar.close)
                } else {
                    costs.sell_fill(bar.close)
                };
                trades.push(close(kinds[k], pos, price, false, bar, split_time, costs));
            }
        }
    }
    trades
}

/// One funding settlement at `rate` on every open position, marked at
/// `mark`: a long pays a positive rate, a short receives it.
fn accrue_funding(open: &mut [Option<OpenTrade>], rate: f64, mark: f64) {
    for pos in open.iter_mut().flatten() {
        let notional = pos.notional * mark / pos.entry_price;
        pos.funding += if pos.short { -rate } else { rate } * notional;
    }
}

/// Size and open a position at `entry_price`, or `None` when the stop is on
/// the wrong side or no closer than the liquidation price: a trade the venue
/// would close before its own stop is not taken.
fn open_position(
    short: bool,
    entry_price: f64,
    stop: f64,
    atr: f64,
    bar: &Candle,
    risk_per_trade: f64,
    venue: Venue,
) -> Option<OpenTrade> {
    let risk = if short {
        stop - entry_price
    } else {
        entry_price - stop
    };
    let stop_pct = risk / entry_price;
    if !(stop_pct > 0.0 && stop_pct.is_finite()) {
        return None;
    }
    let liquidation = venue.liquidation_price(entry_price, short);
    if let Some(liq) = liquidation {
        if (short && stop >= liq) || (!short && stop <= liq) {
            return None;
        }
    }
    // The target is re-derived from the fill, as the live executor does.
    let target = if short {
        entry_price - REWARD_RISK * risk
    } else {
        entry_price + REWARD_RISK * risk
    };
    Some(OpenTrade {
        short,
        entry_time: bar.close_time,
        entry_price,
        stop,
        target,
        notional: (risk_per_trade / stop_pct).min(MAX_NOTIONAL_FRACTION * venue.leverage),
        extreme: entry_price,
        atr_at_entry: atr,
        funding: 0.0,
        liquidation,
    })
}

/// Whether a resting stop or target fills inside `bar`, and at what price.
///
/// A bar that touches both resolves as the stop, the conservative reading. A
/// bar that opens through a level fills at the open: a stop then fills worse
/// than its trigger, a target better than its limit. A gap past the
/// liquidation price fills there instead: the venue closes the position, and
/// the margin is all that is lost.
fn exit_on_bar(
    pos: &OpenTrade,
    bar: &Candle,
    params: &StrategyParams,
    costs: &CostModel,
) -> Option<(f64, bool)> {
    let stop = effective_stop(pos, params);
    let trailing = params.trailing_stop_atr > 0.0;
    if pos.short {
        if bar.high >= stop {
            let fill = costs.buy_fill(stop.max(bar.open));
            Some((pos.liquidation.map_or(fill, |liq| fill.min(liq)), false))
        } else if !trailing && bar.low <= pos.target {
            Some((costs.limit_fill(pos.target.min(bar.open)), true))
        } else {
            None
        }
    } else if bar.low <= stop {
        let fill = costs.sell_fill(stop.min(bar.open));
        Some((pos.liquidation.map_or(fill, |liq| fill.max(liq)), false))
    } else if !trailing && bar.high >= pos.target {
        Some((costs.limit_fill(pos.target.max(bar.open)), true))
    } else {
        None
    }
}

/// The stop as the live executor holds it: the chandelier trail when
/// `TRAILING_STOP_ATR` is set, never looser than the initial stop.
fn effective_stop(pos: &OpenTrade, params: &StrategyParams) -> f64 {
    if params.trailing_stop_atr <= 0.0 || !pos.atr_at_entry.is_finite() || pos.atr_at_entry <= 0.0 {
        return pos.stop;
    }
    let trail = params.trailing_stop_atr * pos.atr_at_entry;
    if pos.short {
        pos.stop.min(pos.extreme + trail)
    } else {
        pos.stop.max(pos.extreme - trail)
    }
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
    let moved = exit_price / pos.entry_price - 1.0;
    let gross = pos.notional * if pos.short { -moved } else { moved };
    let exit_notional = pos.notional * exit_price / pos.entry_price;
    let fees = costs.taker_cost(pos.notional)
        + if resting {
            costs.maker_cost(exit_notional)
        } else {
            costs.taker_cost(exit_notional)
        };
    LabTrade {
        strategy,
        short: pos.short,
        exit_time: bar.close_time,
        ret: gross - fees - pos.funding,
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

/// On futures there is no live strategy to beat: anything that clears every
/// bar, longs and shorts together, is a candidate, best first.
fn recommend_futures(rows: &[(StrategyKind, Stats, Stats)]) -> Option<StrategyKind> {
    rows.iter()
        .filter(|(_, is, oos)| {
            oos.trades >= MIN_OOS_TRADES
                && oos.profit_factor >= MIN_OOS_PROFIT_FACTOR
                && oos.expectancy > 0.0
                && is.trades >= MIN_OOS_TRADES
                && is.expectancy > 0.0
        })
        .max_by(|a, b| a.2.expectancy.total_cmp(&b.2.expectancy))
        .map(|r| r.0)
}

fn build_report(
    trades: &[LabTrade],
    live: StrategyKind,
    days: u64,
    config: &Config,
    venue: Venue,
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

    let recommendation = if venue.futures {
        recommend_futures(&rows).map(|kind| format!("futures:{}", kind.name()))
    } else {
        recommend(&rows, live).map(|kind| format!("switch:{}", kind.name()))
    };
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
            let mut row = json!({
                "name": kind.name(),
                "live": *kind == live,
                "in_sample": as_json(is),
                "out_of_sample": as_json(oos),
                "mc_p5_100_trades_pct": monte_carlo_p5(&oos_returns).map(|v| round(v * 100.0)),
            });
            if venue.futures {
                let side = |short: bool| -> Vec<&LabTrade> {
                    trades
                        .iter()
                        .filter(|t| t.strategy == *kind && t.out_of_sample && t.short == short)
                        .collect()
                };
                row["out_of_sample_long"] = as_json(&stats(&side(false)));
                row["out_of_sample_short"] = as_json(&stats(&side(true)));
            }
            row
        })
        .collect();

    json!({
        "days": days,
        "trend_interval": config.trend_interval,
        "signal_interval": config.signal_interval,
        "pairs": config.trading_pairs,
        "risk_per_trade_pct": round(config.risk_per_trade * 100.0),
        "market": venue.name(),
        "leverage": venue.leverage,
        "live": live.name(),
        "strategies": strategies,
        "recommendation": recommendation.unwrap_or_else(|| "keep".to_string()),
    })
}

/// ATR buffers swept by the lab: how far price must fall below EMA50 to exit,
/// and rise above it to enter. Only `trend_pullback` reads them.
const EXIT_BUFFERS_ATR: [f64; 4] = [0.0, 0.5, 1.0, 1.5];
const ENTRY_BUFFERS_ATR: [f64; 3] = [0.0, 0.25, 0.5];

#[derive(Debug, Clone, Copy, PartialEq)]
struct GridRow {
    entry_buffer_atr: f64,
    exit_buffer_atr: f64,
    in_sample: Stats,
    out_of_sample: Stats,
}

/// Replay `trend_pullback` on every pair under each buffer pair, everything
/// else (venue, trailing stop, costs, risk) as configured. On futures that is
/// both sides together.
fn buffer_grid(
    feeds: &[Feed],
    risk_per_trade: f64,
    costs: &CostModel,
    params: &StrategyParams,
    venue: Venue,
) -> Vec<GridRow> {
    let mut rows = Vec::new();
    for exit in EXIT_BUFFERS_ATR {
        for entry in ENTRY_BUFFERS_ATR {
            let p = StrategyParams {
                entry_buffer_atr: entry,
                exit_buffer_atr: exit,
                ..*params
            };
            let trades: Vec<LabTrade> = feeds
                .iter()
                .flat_map(|feed| {
                    simulate(
                        feed,
                        risk_per_trade,
                        costs,
                        &p,
                        venue,
                        &[StrategyKind::TrendPullback],
                    )
                })
                .collect();
            let of = |oos: bool| -> Vec<&LabTrade> {
                trades.iter().filter(|t| t.out_of_sample == oos).collect()
            };
            rows.push(GridRow {
                entry_buffer_atr: entry,
                exit_buffer_atr: exit,
                in_sample: stats(&of(false)),
                out_of_sample: stats(&of(true)),
            });
        }
    }
    rows
}

/// The buffer pair to make the default, if any: the best out-of-sample
/// expectancy among rows that clear the same bars a strategy switch must, and
/// only if it beats the buffers running now. `None` means keep them.
fn pick_buffers(rows: &[GridRow], current: &StrategyParams) -> Option<GridRow> {
    let same = |a: f64, b: f64| (a - b).abs() < 1e-9;
    let current_oos = rows
        .iter()
        .find(|r| {
            same(r.entry_buffer_atr, current.entry_buffer_atr)
                && same(r.exit_buffer_atr, current.exit_buffer_atr)
        })
        .map(|r| r.out_of_sample.expectancy)
        .unwrap_or(0.0);
    rows.iter()
        .filter(|r| {
            r.out_of_sample.trades >= MIN_OOS_TRADES
                && r.out_of_sample.profit_factor >= MIN_OOS_PROFIT_FACTOR
                && r.out_of_sample.expectancy > 0.0
                && r.in_sample.trades >= MIN_OOS_TRADES
                && r.in_sample.expectancy > 0.0
                && r.out_of_sample.expectancy > current_oos
        })
        .max_by(|a, b| {
            a.out_of_sample
                .expectancy
                .total_cmp(&b.out_of_sample.expectancy)
        })
        .copied()
}

fn buffer_grid_json(rows: &[GridRow], current: &StrategyParams) -> serde_json::Value {
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
    let rows_json: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            json!({
                "entry_buffer_atr": r.entry_buffer_atr,
                "exit_buffer_atr": r.exit_buffer_atr,
                "in_sample": as_json(&r.in_sample),
                "out_of_sample": as_json(&r.out_of_sample),
            })
        })
        .collect();
    json!({
        "current": {
            "entry_buffer_atr": current.entry_buffer_atr,
            "exit_buffer_atr": current.exit_buffer_atr,
        },
        "rows": rows_json,
        "recommendation": match pick_buffers(rows, current) {
            Some(r) => json!({
                "entry_buffer_atr": r.entry_buffer_atr,
                "exit_buffer_atr": r.exit_buffer_atr,
            }),
            None => json!("keep"),
        },
    })
}

fn print_buffer_grid(grid: &serde_json::Value) {
    info!("trend_pullback buffers | OOS trades | win%  | exp%/trade | PF    | maxDD% | IS trades | IS exp%");
    for r in grid["rows"].as_array().into_iter().flatten() {
        let oos = &r["out_of_sample"];
        info!(
            "entry {:<4} exit {:<4}  | {:>10} | {:>5} | {:>10} | {:>5} | {:>6} | {:>9} | {}",
            r["entry_buffer_atr"],
            r["exit_buffer_atr"],
            oos["trades"],
            oos["win_rate"],
            oos["expectancy_pct"],
            oos["profit_factor"],
            oos["max_drawdown_pct"],
            r["in_sample"]["trades"],
            r["in_sample"]["expectancy_pct"],
        );
    }
    info!("buffer recommendation: {}", grid["recommendation"]);
}

fn print_table(report: &serde_json::Value) {
    info!(
        "market: {} (leverage {})",
        report["market"].as_str().unwrap_or("?"),
        report["leverage"]
    );
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
        for (label, side) in [
            ("  long", &s["out_of_sample_long"]),
            ("  short", &s["out_of_sample_short"]),
        ] {
            if side.is_object() {
                info!(
                    "{label:<17} | {:>10} | {:>5} | {:>10} | {:>5} | {:>6} |",
                    side["trades"],
                    side["win_rate"],
                    side["expectancy_pct"],
                    side["profit_factor"],
                    side["max_drawdown_pct"],
                );
            }
        }
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
            short: false,
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
        let warmup_end = signal[34].close_time;
        let feed = Feed {
            trend,
            signal,
            funding: Vec::new(),
        };
        let trades = simulate(
            &feed,
            0.10,
            &CostModel::default(),
            &StrategyParams::default(),
            Venue::SPOT,
            &StrategyKind::ALL,
        );
        assert!(
            trades.iter().any(|t| t.strategy == StrategyKind::Breakout),
            "fixture must exercise the simulator"
        );
        for t in &trades {
            assert!(t.exit_time > warmup_end, "traded during warmup");
            assert!(t.ret.is_finite());
            assert!(t.ret.abs() < MAX_NOTIONAL_FRACTION);
            assert!(!t.short, "spot never shorts");
        }
    }

    /// The mirror of the fixture above: a falling trend, and signal bars that
    /// keep making new lows and bouncing.
    fn falling_feed() -> Feed {
        let trend: Vec<Candle> = (0..260)
            .map(|i| candle(i * 14_400_000, 14_400_000, 200.0 - i as f64 * 0.1))
            .collect();
        let start = 210 * 14_400_000;
        let signal: Vec<Candle> = (0..2_000)
            .map(|i| {
                let x = i as f64;
                let close = 160.0 + 5.0 * (x * 0.03).sin() - x * 0.002;
                Candle {
                    high: close + 0.01,
                    low: close - 0.01,
                    ..candle(start + i * 900_000, 900_000, close)
                }
            })
            .collect();
        let funding = (0..200)
            .map(|i| (start + i * FUNDING_INTERVAL_MS, 0.0001))
            .collect();
        Feed {
            trend,
            signal,
            funding,
        }
    }

    fn futures(leverage: f64) -> Venue {
        Venue {
            futures: true,
            leverage,
        }
    }

    #[test]
    fn futures_short_a_falling_market_and_spot_does_not() {
        let feed = falling_feed();
        let run = |venue| {
            simulate(
                &feed,
                0.10,
                &CostModel::futures_from_env(),
                &StrategyParams::default(),
                venue,
                &StrategyKind::ALL,
            )
        };
        let shorts: Vec<LabTrade> = run(futures(3.0)).into_iter().filter(|t| t.short).collect();
        assert!(
            shorts.iter().any(|t| t.strategy == StrategyKind::Breakout),
            "fixture must exercise the short side"
        );
        assert!(shorts.iter().all(|t| t.ret.is_finite()));
        assert!(run(Venue::SPOT).iter().all(|t| !t.short));
    }

    #[test]
    fn longs_pay_positive_funding_and_shorts_receive_it() {
        let mut open = vec![
            Some(open_trade()),
            Some(OpenTrade {
                short: true,
                ..open_trade()
            }),
            None,
        ];
        // 0.01% on half of equity, marked 10% higher: 0.0055% of equity.
        accrue_funding(&mut open, 0.0001, 110.0);
        let paid = |i: usize| open[i].map(|p| p.funding).unwrap();
        assert!((paid(0) - 0.000_055).abs() < 1e-12);
        assert!((paid(1) + 0.000_055).abs() < 1e-12);

        let bar = range(100.0, 99.0, 101.0);
        let free = CostModel::frictionless();
        let pos = open[0].unwrap();
        let t = close(StrategyKind::Breakout, &pos, 100.0, false, &bar, 0, &free);
        assert!(
            (t.ret + 0.000_055).abs() < 1e-12,
            "funding comes off the return"
        );
    }

    #[test]
    fn missing_recent_funding_is_estimated_from_the_latest_rates() {
        let mut funding = vec![(0, 0.0001), (FUNDING_INTERVAL_MS, 0.0003)];
        extend_funding(&mut funding, 3 * FUNDING_INTERVAL_MS + 5);
        assert_eq!(funding.len(), 4);
        for (i, (t, rate)) in funding[2..].iter().enumerate() {
            assert_eq!(*t, (i as u64 + 2) * FUNDING_INTERVAL_MS);
            assert!((rate - 0.0002).abs() < 1e-12);
        }

        let mut none: Vec<(u64, f64)> = Vec::new();
        extend_funding(&mut none, FUNDING_INTERVAL_MS * 10);
        assert!(none.is_empty(), "nothing to estimate from");
    }

    #[test]
    fn leverage_caps_notional_and_sets_the_liquidation_price() {
        let bar = range(100.0, 99.0, 101.0);
        // A 0.5% stop at 1% risk wants 2x equity: spot caps at 0.95, 3x at 2.85.
        let spot = open_position(false, 100.0, 99.5, 1.0, &bar, 0.01, Venue::SPOT).unwrap();
        assert!((spot.notional - 0.95).abs() < 1e-12);
        assert_eq!(spot.liquidation, None);
        let lev = open_position(false, 100.0, 99.5, 1.0, &bar, 0.01, futures(3.0)).unwrap();
        assert!((lev.notional - 2.0).abs() < 1e-12);
        // 1/3 away less maintenance margin.
        let liq = lev.liquidation.unwrap();
        assert!((liq - 100.0 * (1.0 - 1.0 / 3.0 + MAINT_MARGIN)).abs() < 1e-9);

        let short = open_position(true, 100.0, 102.0, 1.0, &bar, 0.01, futures(3.0)).unwrap();
        assert!((short.target - 96.0).abs() < 1e-12, "2R below a short");
        assert!(short.liquidation.unwrap() > 102.0);
    }

    #[test]
    fn a_stop_beyond_liquidation_is_not_taken() {
        let bar = range(100.0, 99.0, 101.0);
        // At 3x a long liquidates near 67: a stop at 60 would never be reached.
        assert!(open_position(false, 100.0, 60.0, 1.0, &bar, 0.01, futures(3.0)).is_none());
        assert!(open_position(true, 100.0, 140.0, 1.0, &bar, 0.01, futures(3.0)).is_none());
        // Wrong-side stops are rejected on either side.
        assert!(open_position(true, 100.0, 99.0, 1.0, &bar, 0.01, futures(3.0)).is_none());
        assert!(open_position(false, 100.0, 60.0, 1.0, &bar, 0.01, Venue::SPOT).is_some());
    }

    fn open_trade() -> OpenTrade {
        OpenTrade {
            short: false,
            entry_time: 0,
            entry_price: 100.0,
            stop: 95.0,
            target: 110.0,
            notional: 0.5,
            extreme: 100.0,
            atr_at_entry: 2.0,
            funding: 0.0,
            liquidation: None,
        }
    }

    fn open_short() -> OpenTrade {
        OpenTrade {
            short: true,
            stop: 105.0,
            target: 90.0,
            ..open_trade()
        }
    }

    #[test]
    fn a_short_stops_above_and_takes_profit_below() {
        let free = CostModel::frictionless();
        let p = StrategyParams::default();
        let short = open_short();
        // Touches both: the stop wins.
        assert_eq!(
            exit_on_bar(&short, &range(100.0, 89.0, 106.0), &p, &free),
            Some((105.0, false))
        );
        // Gaps up through the stop: fills at the open.
        assert_eq!(
            exit_on_bar(&short, &range(108.0, 107.0, 109.0), &p, &free),
            Some((108.0, false))
        );
        // Target is a resting order.
        assert_eq!(
            exit_on_bar(&short, &range(95.0, 89.5, 96.0), &p, &free),
            Some((90.0, true))
        );
        assert_eq!(
            exit_on_bar(&short, &range(100.0, 95.0, 104.0), &p, &free),
            None
        );
    }

    #[test]
    fn a_short_trails_down_from_its_lowest_low() {
        let free = CostModel::frictionless();
        let params = StrategyParams {
            trailing_stop_atr: 2.0,
            ..Default::default()
        };
        let mut short = open_short();
        short.extreme = 92.0;
        // Stop trails to 92 + 4 = 96.
        assert_eq!(
            exit_on_bar(&short, &range(95.0, 94.0, 96.5), &params, &free),
            Some((96.0, false))
        );
    }

    #[test]
    fn a_gap_past_liquidation_loses_only_the_margin() {
        let free = CostModel::frictionless();
        let p = StrategyParams::default();
        let long = OpenTrade {
            liquidation: Some(70.0),
            ..open_trade()
        };
        assert_eq!(
            exit_on_bar(&long, &range(60.0, 55.0, 61.0), &p, &free),
            Some((70.0, false))
        );
        let short = OpenTrade {
            liquidation: Some(130.0),
            ..open_short()
        };
        assert_eq!(
            exit_on_bar(&short, &range(140.0, 139.0, 141.0), &p, &free),
            Some((130.0, false))
        );
    }

    #[test]
    fn a_short_profits_when_price_falls() {
        let bar = range(90.0, 89.0, 91.0);
        let t = close(
            StrategyKind::Breakout,
            &open_short(),
            90.0,
            true,
            &bar,
            0,
            &CostModel::frictionless(),
        );
        // Half of equity short, price -10%: +5%.
        assert!((t.ret - 0.05).abs() < 1e-12);
        assert!(t.short);
    }

    #[test]
    fn futures_recommend_anything_that_clears_every_bar() {
        let good = Stats {
            trades: 40,
            expectancy: 0.003,
            profit_factor: 1.3,
            ..Default::default()
        };
        let losing = Stats {
            expectancy: -0.001,
            profit_factor: 0.8,
            ..good
        };
        let rows = [
            (StrategyKind::TrendPullback, good, good),
            (StrategyKind::Breakout, losing, losing),
        ];
        // The live strategy itself qualifies: on futures nothing runs yet.
        assert_eq!(recommend_futures(&rows), Some(StrategyKind::TrendPullback));
        assert_eq!(recommend_futures(&rows[1..]), None);
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
        pos.extreme = 112.0;
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

    fn row(entry: f64, exit: f64, oos_trades: usize, is_exp: f64, oos_exp: f64) -> GridRow {
        let s = |trades, expectancy| Stats {
            trades,
            expectancy,
            profit_factor: if expectancy > 0.0 { 1.5 } else { 0.8 },
            ..Default::default()
        };
        GridRow {
            entry_buffer_atr: entry,
            exit_buffer_atr: exit,
            in_sample: s(40, is_exp),
            out_of_sample: s(oos_trades, oos_exp),
        }
    }

    #[test]
    fn the_best_qualifying_buffers_win_out_of_sample() {
        let rows = [
            row(0.0, 0.0, 40, 0.001, 0.001),
            row(0.25, 1.0, 40, 0.002, 0.004),
            row(0.5, 1.5, 40, 0.003, 0.003),
        ];
        let pick = pick_buffers(&rows, &StrategyParams::default()).unwrap();
        assert_eq!((pick.entry_buffer_atr, pick.exit_buffer_atr), (0.25, 1.0));
    }

    #[test]
    fn buffers_are_not_changed_on_thin_or_in_sample_losing_evidence() {
        let rows = [
            row(0.0, 0.0, 40, 0.001, 0.001),
            // Best out of sample, but too few trades to mean anything.
            row(0.25, 1.0, MIN_OOS_TRADES - 1, 0.002, 0.010),
            // Lost money in sample: a good recent run is luck.
            row(0.5, 1.5, 40, -0.001, 0.008),
        ];
        assert_eq!(pick_buffers(&rows, &StrategyParams::default()), None);
    }

    #[test]
    fn buffers_that_do_not_beat_the_current_ones_are_kept() {
        let rows = [
            row(0.0, 0.0, 40, 0.002, 0.003),
            row(0.25, 1.0, 40, 0.004, 0.005),
        ];
        let current = StrategyParams {
            entry_buffer_atr: 0.25,
            exit_buffer_atr: 1.0,
            ..Default::default()
        };
        assert_eq!(pick_buffers(&rows, &current), None);
    }

    #[test]
    fn the_grid_covers_every_buffer_pair() {
        let rows = buffer_grid(
            &[],
            0.01,
            &CostModel::default(),
            &StrategyParams::default(),
            Venue::SPOT,
        );
        assert_eq!(rows.len(), EXIT_BUFFERS_ATR.len() * ENTRY_BUFFERS_ATR.len());
        assert!(rows.iter().all(|r| r.out_of_sample.trades == 0));
        let json = buffer_grid_json(&rows, &StrategyParams::default());
        assert_eq!(json["recommendation"], "keep");
        assert_eq!(json["rows"].as_array().map(Vec::len), Some(12));
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
