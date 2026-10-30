//! Live trading on Binance USDⓈ-M perpetuals: long and short, one-way mode.
//!
//! Runs as `MARKET=futures`. The exchange is the source of truth: every cycle
//! reads the open positions and conditional orders and acts on what is there,
//! so a restart loses nothing but the original target of an open trade.
//!
//! Decisions follow the lab: the strategy is evaluated on the last *closed*
//! signal candle, the short side is the strategy's mirror (`evaluate_short`),
//! and positions are sized from the stop, capped at `MAX_NOTIONAL_FRACTION` x
//! `LEVERAGE` of available margin. Each entry gets a mark-price stop and a 2R
//! target, both closing the whole position. A position found without a stop
//! gets one at once.
//!
//! Not supported here, by design for a first version: the trailing stop,
//! hedge mode (the bot refuses to start), and more than one position per pair.

use crate::config::Config;
use crate::exchange::binance_futures::{BinanceFuturesClient, PRODUCTION_URL};
use crate::exchange::{Candle, FuturesExchange, FuturesPosition, Side, SymbolFilters, TriggerKind};
use crate::notify::Notifier;
use crate::risk::{CashFlow, RiskManager};
use crate::strategies::{StrategyKind, REWARD_RISK};
use crate::strategy::{self, IndicatorSnapshot, ShortDecision, Signal, StrategyParams};
use std::time::Duration;
use tokio::time::sleep;
use tracing::{error, info, warn};

const DEFAULT_LEVERAGE: u32 = 3;
const MAX_LEVERAGE: u32 = 3;
/// Largest share of available margin x leverage one entry may use.
const MAX_NOTIONAL_FRACTION: f64 = 0.95;
/// Maintenance margin rate for the pre-trade liquidation estimate. Binance's
/// first tier is 0.4%; rounded up to err toward liquidating early.
const MAINT_MARGIN: f64 = 0.005;
/// A position found without a stop gets one this many trend ATRs from mark.
const EMERGENCY_STOP_ATR: f64 = 1.5;
/// ...or this far from mark when no ATR is available.
const EMERGENCY_STOP_FRACTION: f64 = 0.02;
/// Same windows the spot loop requests; one extra signal bar because the
/// forming one is dropped.
const TREND_LIMIT: u32 = 250;
const SIGNAL_LIMIT: u32 = 101;
/// Margin assets valued at one dollar when a transfer moves them.
const DOLLAR_ASSETS: [&str; 4] = ["USDC", "USDT", "FDUSD", "BNFCR"];

const HEDGE_MODE_REFUSAL: &str =
    "the account is in hedge mode; switch USDⓈ-M futures to one-way mode";

pub struct FuturesSettings {
    pub base_url: String,
    pub leverage: u32,
}

impl FuturesSettings {
    /// `FUTURES_BASE_URL` (default production) and `LEVERAGE` (1-3, default 3).
    pub fn from_env() -> Self {
        let leverage = std::env::var("LEVERAGE")
            .ok()
            .and_then(|v| v.trim().parse::<u32>().ok())
            .unwrap_or(DEFAULT_LEVERAGE)
            .clamp(1, MAX_LEVERAGE);
        Self {
            base_url: std::env::var("FUTURES_BASE_URL")
                .ok()
                .filter(|v| !v.trim().is_empty())
                .unwrap_or_else(|| PRODUCTION_URL.to_string()),
            leverage,
        }
    }
}

pub async fn run(config: &Config) {
    let settings = FuturesSettings::from_env();
    let client = BinanceFuturesClient::new(&settings.base_url, &config.api_key, &config.api_secret);
    let notifier = Notifier::from_env();
    let params = StrategyParams::from_env();
    let kind = config.strategy;

    info!(
        "Tempest FUTURES starting | {} | {}x | pairs {:?} | strategy {} | trend {} | signal {}",
        settings.base_url,
        settings.leverage,
        config.trading_pairs,
        kind.name(),
        config.trend_interval,
        config.signal_interval
    );
    if params.trailing_stop_atr > 0.0 {
        warn!("TRAILING_STOP_ATR is ignored on futures: entries use a fixed 2R target");
    }

    let poll = Duration::from_secs(config.poll_interval_secs);
    let mut last_error = String::new();
    // Retried until it works: a network blip or a key still missing its
    // futures permission must not end the process. Hedge mode is the one
    // refusal that stands until someone changes the account.
    let equity = loop {
        match startup(&client, config, &settings).await {
            Ok((equity, report)) => {
                info!("{report}");
                notifier.send(&report).await;
                break equity;
            }
            Err(e) => {
                error!("Futures startup failed: {e}");
                if e != last_error {
                    notifier
                        .send(&format!("\u{26d4} *Futures bot not started*\n`{e}`"))
                        .await;
                    last_error = e.clone();
                }
                if e == HEDGE_MODE_REFUSAL {
                    return;
                }
                sleep(poll).await;
            }
        }
    };
    last_error.clear();

    let mut risk = RiskManager::with_risk_per_trade(equity, config.risk_per_trade)
        .with_limits(config.max_open_positions, config.daily_drawdown_limit);
    loop {
        match run_cycle(
            &client,
            config,
            settings.leverage,
            &mut risk,
            &notifier,
            &params,
        )
        .await
        {
            Ok(()) => last_error.clear(),
            Err(e) => {
                error!("Futures cycle error: {e}");
                // Once per distinct error, not once per poll.
                if e != last_error {
                    notifier.notify_error("Futures cycle", &e).await;
                    last_error = e;
                }
            }
        }
        sleep(poll).await;
    }
}

/// Refuse to trade an account the loop cannot reason about, set leverage, and
/// say which pairs this balance can actually trade. Returns starting equity
/// and the startup report.
async fn startup<E: FuturesExchange>(
    ex: &E,
    config: &Config,
    settings: &FuturesSettings,
) -> Result<(f64, String), String> {
    if ex.hedge_mode().await? {
        return Err(HEDGE_MODE_REFUSAL.into());
    }
    let account = ex.account().await?;
    let max_notional = MAX_NOTIONAL_FRACTION * f64::from(settings.leverage) * account.available;
    let mut lines = vec![format!(
        "\u{1f680} *Tempest futures started*\nEquity `{:.2}` (`{:.2}` available) \u{b7} {}x \u{b7} \
         most per position `{max_notional:.2}`",
        account.equity, account.available, settings.leverage
    )];
    for symbol in &config.trading_pairs {
        let filters = ex.filters(symbol).await?;
        if let Err(e) = ex.set_leverage(symbol, settings.leverage).await {
            warn!(
                "{symbol}: could not set leverage to {}x: {e}",
                settings.leverage
            );
        }
        let price = ex
            .klines(symbol, &config.signal_interval, 1)
            .await?
            .last()
            .map_or(0.0, |c| c.close);
        let smallest = filters.min_tradable_quantity(price) * price;
        lines.push(if smallest > max_notional {
            format!(
                "\u{26a0}\u{fe0f} *{symbol}*: smallest order `{smallest:.2}` is more than this \
                 account can hold at {}x; it will not trade",
                settings.leverage
            )
        } else {
            format!("*{symbol}*: smallest order `{smallest:.2}`")
        });
    }
    Ok((account.equity, lines.join("\n")))
}

async fn run_cycle<E: FuturesExchange>(
    ex: &E,
    config: &Config,
    leverage: u32,
    risk: &mut RiskManager,
    notifier: &Notifier,
    params: &StrategyParams,
) -> Result<(), String> {
    let positions = ex.positions().await?;
    if std::env::var("FORCE_CLOSE").unwrap_or_default() == "true" {
        warn!("FORCE_CLOSE is set: closing every futures position");
        for pos in positions
            .iter()
            .filter(|p| config.trading_pairs.contains(&p.symbol))
        {
            close_position(ex, pos, "FORCE_CLOSE", notifier).await?;
        }
        return Ok(());
    }

    let account = ex.account().await?;
    risk.check_day_reset(account.equity);
    if let Err(e) = record_transfers(ex, risk).await {
        warn!("Futures transfers not read this cycle: {e}");
    }
    if !risk.halted && risk.check_drawdown(account.equity) {
        notifier
            .notify_halt(risk.drawdown_pct(account.equity), account.equity)
            .await;
    }

    let mut open = positions
        .iter()
        .filter(|p| config.trading_pairs.contains(&p.symbol))
        .count();
    let mut traded = false;
    for symbol in &config.trading_pairs {
        let pos = positions.iter().find(|p| &p.symbol == symbol);
        let entries_allowed = !risk.halted && open < config.max_open_positions;
        match manage_symbol(
            ex,
            config,
            leverage,
            symbol,
            pos,
            entries_allowed,
            notifier,
            params,
        )
        .await
        {
            Ok(Traded::Opened) => {
                traded = true;
                open += 1;
            }
            Ok(Traded::Closed) => traded = true,
            Ok(Traded::Nothing) => {}
            Err(e) => {
                error!("{symbol}: {e}");
                notifier.notify_error(symbol, &e).await;
            }
        }
    }

    if traded {
        let after = ex.account().await?;
        notifier
            .send(&format!(
                "\u{1f4ca} *Futures summary*\nEquity `{:.2}` (`{:.2}` available)\nToday `{:+.2}`",
                after.equity,
                after.available,
                risk.day_pnl(after.equity)
            ))
            .await;
    }
    Ok(())
}

#[derive(Debug, PartialEq)]
enum Traded {
    Nothing,
    Opened,
    Closed,
}

#[allow(clippy::too_many_arguments)]
async fn manage_symbol<E: FuturesExchange>(
    ex: &E,
    config: &Config,
    leverage: u32,
    symbol: &str,
    pos: Option<&FuturesPosition>,
    entries_allowed: bool,
    notifier: &Notifier,
    params: &StrategyParams,
) -> Result<Traded, String> {
    let filters = ex.filters(symbol).await?;
    let candles = closed_candles(ex, config, symbol).await;
    let snap = candles.as_ref().ok().and_then(|(trend, signal)| {
        let price = signal.last()?.close;
        strategy::compute_indicators(trend, signal, price)
    });

    if let Some(pos) = pos {
        // Protection first: it must not depend on candles being available.
        ensure_stop(ex, &filters, pos, snap.as_ref().map(|s| s.atr_14), notifier).await?;
        let (Some(snap), Ok((_, signal))) = (snap.as_ref(), candles.as_ref()) else {
            return Ok(Traded::Nothing);
        };
        if decide(config.strategy, Some(pos), snap, signal, params) == Action::Close {
            close_position(ex, pos, "signal exit", notifier).await?;
            return Ok(Traded::Closed);
        }
        return Ok(Traded::Nothing);
    }

    // Flat: whatever is still resting belongs to a position that is gone.
    if !ex.trigger_orders(symbol).await?.is_empty() {
        info!("{symbol}: flat with leftover orders; cancelling them");
        ex.cancel_all(symbol).await?;
    }
    let (trend, signal) = candles?;
    let snap = snap.ok_or_else(|| {
        format!(
            "not enough history ({} trend, {} signal candles)",
            trend.len(),
            signal.len()
        )
    })?;
    if !entries_allowed {
        return Ok(Traded::Nothing);
    }
    let (short, stop) = match decide(config.strategy, None, &snap, &signal, params) {
        Action::OpenLong { stop } => (false, stop),
        Action::OpenShort { stop } => (true, stop),
        _ => return Ok(Traded::Nothing),
    };
    let account = ex.account().await?;
    let plan = match plan_entry(
        short,
        snap.current_price,
        stop,
        account.equity,
        account.available,
        config.risk_per_trade,
        leverage,
        &filters,
    ) {
        Ok(plan) => plan,
        Err(reason) => {
            info!(
                "{symbol}: {} signal skipped: {reason}",
                if short { "SHORT" } else { "LONG" }
            );
            return Ok(Traded::Nothing);
        }
    };
    open_position(ex, &filters, short, plan, stop, notifier).await?;
    Ok(Traded::Opened)
}

/// The trend candles (newest may be forming) and the closed signal candles.
async fn closed_candles<E: FuturesExchange>(
    ex: &E,
    config: &Config,
    symbol: &str,
) -> Result<(Vec<Candle>, Vec<Candle>), String> {
    let trend = ex
        .klines(symbol, &config.trend_interval, TREND_LIMIT)
        .await?;
    let mut signal = ex
        .klines(symbol, &config.signal_interval, SIGNAL_LIMIT)
        .await?;
    let now = now_ms();
    signal.retain(|c| c.close_time <= now);
    Ok((trend, signal))
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Action {
    Hold,
    Close,
    OpenLong { stop: f64 },
    OpenShort { stop: f64 },
}

/// What the strategy wants for this pair, given the position held (if any).
/// A long can only be closed by the long rules and a short by the short ones;
/// a flat pair tries the long side first.
fn decide(
    kind: StrategyKind,
    pos: Option<&FuturesPosition>,
    snap: &IndicatorSnapshot,
    signal: &[Candle],
    params: &StrategyParams,
) -> Action {
    match pos {
        Some(p) if p.is_short() => {
            if kind.evaluate_short(snap, signal, params) == ShortDecision::Cover {
                Action::Close
            } else {
                Action::Hold
            }
        }
        Some(_) => {
            if kind.evaluate("LIVE", snap, signal, params).signal == Signal::Sell {
                Action::Close
            } else {
                Action::Hold
            }
        }
        None => {
            let long = kind.evaluate("LIVE", snap, signal, params);
            if long.signal == Signal::Buy {
                return Action::OpenLong {
                    stop: long.stop_loss,
                };
            }
            match kind.evaluate_short(snap, signal, params) {
                ShortDecision::Enter { stop_loss } => Action::OpenShort { stop: stop_loss },
                _ => Action::Hold,
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct EntryPlan {
    quantity: f64,
}

/// Size an entry at `price` with its stop at `stop`, or say why not.
///
/// Risk is `risk_per_trade` of equity at the stop, capped by what the margin
/// can carry at `leverage`. A size under the venue minimum is skipped rather
/// than rounded up: rounding up would risk more than configured. So is a
/// trade the venue would liquidate before its own stop.
#[allow(clippy::too_many_arguments)]
fn plan_entry(
    short: bool,
    price: f64,
    stop: f64,
    equity: f64,
    available: f64,
    risk_per_trade: f64,
    leverage: u32,
    filters: &SymbolFilters,
) -> Result<EntryPlan, String> {
    let distance = if short { stop - price } else { price - stop };
    if !(distance > 0.0 && distance.is_finite() && price > 0.0) {
        return Err(format!("stop {stop:.2} is on the wrong side of {price:.2}"));
    }
    let by_risk = equity * risk_per_trade / distance;
    let by_margin = MAX_NOTIONAL_FRACTION * f64::from(leverage) * available / price;
    let quantity = filters.round_quantity(by_risk.min(by_margin));
    if let Err(e) = filters.check_order(quantity, price) {
        return Err(format!(
            "{e} (risk sizes {:.2}, margin allows {:.2}, smallest order {:.2})",
            by_risk * price,
            by_margin * price,
            filters.min_tradable_quantity(price) * price
        ));
    }
    if let Some(liq) = liquidation_price(price, quantity, equity, short) {
        if (short && stop >= liq) || (!short && stop <= liq) {
            return Err(format!(
                "stop {stop:.2} is beyond the liquidation price {liq:.2}"
            ));
        }
    }
    Ok(EntryPlan { quantity })
}

/// Where a lone cross-margin position of `quantity` would be liquidated: when
/// its loss eats the equity down to the maintenance margin. `None` when it
/// never can (a long whose price would have to go below zero).
fn liquidation_price(entry: f64, quantity: f64, equity: f64, short: bool) -> Option<f64> {
    if quantity <= 0.0 {
        return None;
    }
    let cushion = (equity - MAINT_MARGIN * entry * quantity) / quantity;
    if short {
        Some(entry + cushion)
    } else {
        Some(entry - cushion).filter(|p| *p > 0.0)
    }
}

/// Open, then protect. A position that cannot get a stop is closed again at
/// once: an unprotected leveraged position is the one outcome not allowed.
async fn open_position<E: FuturesExchange>(
    ex: &E,
    filters: &SymbolFilters,
    short: bool,
    plan: EntryPlan,
    stop: f64,
    notifier: &Notifier,
) -> Result<(), String> {
    let symbol = filters.symbol.as_str();
    let (open_side, close_side) = if short {
        (Side::Sell, Side::Buy)
    } else {
        (Side::Buy, Side::Sell)
    };
    let fill = ex
        .market_order(symbol, open_side, plan.quantity, false)
        .await?;
    let price = if fill.average_price > 0.0 {
        fill.average_price
    } else {
        ex.positions()
            .await?
            .iter()
            .find(|p| p.symbol == symbol)
            .map_or(0.0, |p| p.entry_price)
    };
    let label = if short { "SHORT" } else { "LONG" };

    let stop_ok = if short { stop > price } else { stop < price };
    let stop_price = round_away(filters, stop, short);
    let placed = if stop_ok {
        match ex
            .place_trigger(symbol, close_side, TriggerKind::Stop, stop_price)
            .await
        {
            Ok(id) => Ok(id),
            Err(first) => {
                warn!("{symbol}: stop rejected ({first}); retrying once");
                ex.place_trigger(symbol, close_side, TriggerKind::Stop, stop_price)
                    .await
            }
        }
    } else {
        Err(format!(
            "filled at {price:.2}, already past the stop {stop:.2}"
        ))
    };
    if let Err(e) = placed {
        let undo = ex
            .market_order(symbol, close_side, fill.filled_quantity, true)
            .await;
        let _ = ex.cancel_all(symbol).await;
        let msg = format!(
            "\u{1f6a8} *{symbol}* {label} opened but no stop could be placed ({e}). {}",
            match &undo {
                Ok(_) => "Closed it again.".to_string(),
                Err(u) => format!("Closing it FAILED too ({u}): close it by hand now."),
            }
        );
        error!("{msg}");
        notifier.send(&msg).await;
        return Err(format!("{symbol}: entry undone, no stop: {e}"));
    }

    let risk = (price - stop).abs();
    let target = if short {
        price - REWARD_RISK * risk
    } else {
        price + REWARD_RISK * risk
    };
    let target_price = round_away(filters, target, !short);
    if let Err(e) = ex
        .place_trigger(symbol, close_side, TriggerKind::TakeProfit, target_price)
        .await
    {
        warn!("{symbol}: target not placed ({e}); the stop still protects the position");
        notifier
            .send(&format!(
                "\u{26a0}\u{fe0f} *{symbol}* target not placed: `{e}`. The stop is in place."
            ))
            .await;
    }
    let msg = format!(
        "{} *{symbol}* {label} `{}` @ `{price:.2}`\nStop `{stop_price}` \u{b7} Target `{target_price}`",
        if short { "\u{1f53b}" } else { "\u{1f53a}" },
        fill.filled_quantity
    );
    info!("{msg}");
    notifier.send(&msg).await;
    Ok(())
}

/// Close all of `pos` at market and clear its resting orders.
async fn close_position<E: FuturesExchange>(
    ex: &E,
    pos: &FuturesPosition,
    reason: &str,
    notifier: &Notifier,
) -> Result<(), String> {
    let side = if pos.is_short() {
        Side::Buy
    } else {
        Side::Sell
    };
    let fill = ex
        .market_order(&pos.symbol, side, pos.amount.abs(), true)
        .await?;
    ex.cancel_all(&pos.symbol).await?;
    let price = if fill.average_price > 0.0 {
        fill.average_price
    } else {
        pos.mark_price
    };
    // Signed amount: a short gains when price falls.
    let pnl = (price - pos.entry_price) * pos.amount;
    let msg = format!(
        "{} *{}* {} closed ({reason}) @ `{price:.2}`\nEntry `{:.2}` \u{b7} PnL `{pnl:+.2}` before fees",
        if pnl >= 0.0 { "\u{1f7e2}" } else { "\u{1f534}" },
        pos.symbol,
        if pos.is_short() { "SHORT" } else { "LONG" },
        pos.entry_price
    );
    info!("{msg}");
    notifier.send(&msg).await;
    Ok(())
}

/// Give `pos` a stop if it has none on its closing side.
async fn ensure_stop<E: FuturesExchange>(
    ex: &E,
    filters: &SymbolFilters,
    pos: &FuturesPosition,
    atr: Option<f64>,
    notifier: &Notifier,
) -> Result<(), String> {
    let close_side = if pos.is_short() {
        Side::Buy
    } else {
        Side::Sell
    };
    let orders = ex.trigger_orders(&pos.symbol).await?;
    if orders
        .iter()
        .any(|o| o.kind == Some(TriggerKind::Stop) && o.side == close_side)
    {
        return Ok(());
    }
    let mark = if pos.mark_price > 0.0 {
        pos.mark_price
    } else {
        pos.entry_price
    };
    let distance = atr
        .filter(|a| a.is_finite() && *a > 0.0)
        .map_or(mark * EMERGENCY_STOP_FRACTION, |a| EMERGENCY_STOP_ATR * a)
        .min(mark * 0.5);
    let stop = if pos.is_short() {
        mark + distance
    } else {
        mark - distance
    };
    let stop = round_away(filters, stop, pos.is_short());
    ex.place_trigger(&pos.symbol, close_side, TriggerKind::Stop, stop)
        .await
        .map_err(|e| format!("position has no stop and placing one failed: {e}"))?;
    let msg = format!(
        "\u{1f6e1}\u{fe0f} *{}* had no stop; placed one at `{stop}` (mark `{mark:.2}`)",
        pos.symbol
    );
    warn!("{msg}");
    notifier.send(&msg).await;
    Ok(())
}

/// Round `price` to the tick, up when `up` (a short's stop, a long's target:
/// never closer to the market than asked) and down otherwise.
fn round_away(filters: &SymbolFilters, price: f64, up: bool) -> f64 {
    let down = filters.round_price(price);
    if up && down < price && filters.tick_size > 0.0 {
        filters.round_price(down + filters.tick_size * 1.5)
    } else {
        down
    }
}

/// Transfers into and out of the futures wallet today are cash flows, not
/// P&L. Dollar-pegged margin assets count at face value; anything else is
/// left out with a warning.
async fn record_transfers<E: FuturesExchange>(
    ex: &E,
    risk: &mut RiskManager,
) -> Result<(), String> {
    let transfers = ex.transfers(risk.day_open_at_ms()).await?;
    let flows: Vec<CashFlow> = transfers
        .into_iter()
        .filter_map(|t| {
            if DOLLAR_ASSETS.contains(&t.asset.as_str()) {
                Some(CashFlow {
                    id: t.id,
                    time_ms: t.time_ms,
                    value: t.amount,
                })
            } else {
                warn!(
                    "Transfer of {} {} not counted as a cash flow",
                    t.amount, t.asset
                );
                None
            }
        })
        .collect();
    risk.set_cash_flows(&flows);
    Ok(())
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exchange::{FuturesAccount, OrderOutcome, Transfer, TriggerOrder};
    use std::sync::Mutex;

    #[derive(Default)]
    struct Fake {
        positions: Mutex<Vec<FuturesPosition>>,
        triggers: Mutex<Vec<TriggerOrder>>,
        /// Every call, in order.
        log: Mutex<Vec<String>>,
        fill_price: f64,
        stop_rejects: bool,
        hedge: bool,
    }

    impl Fake {
        fn log(&self) -> Vec<String> {
            self.log.lock().unwrap().clone()
        }
        fn push(&self, entry: String) {
            self.log.lock().unwrap().push(entry);
        }
    }

    impl FuturesExchange for Fake {
        async fn klines(&self, _: &str, _: &str, _: u32) -> Result<Vec<Candle>, String> {
            Ok(vec![Candle {
                open_time: 0,
                open: 2_000.0,
                high: 2_000.0,
                low: 2_000.0,
                close: 2_000.0,
                volume: 1.0,
                close_time: 1,
            }])
        }
        async fn filters(&self, symbol: &str) -> Result<SymbolFilters, String> {
            Ok(eth(symbol))
        }
        async fn account(&self) -> Result<FuturesAccount, String> {
            Ok(FuturesAccount {
                equity: 100.0,
                available: 100.0,
            })
        }
        async fn positions(&self) -> Result<Vec<FuturesPosition>, String> {
            Ok(self.positions.lock().unwrap().clone())
        }
        async fn trigger_orders(&self, _: &str) -> Result<Vec<TriggerOrder>, String> {
            Ok(self.triggers.lock().unwrap().clone())
        }
        async fn market_order(
            &self,
            symbol: &str,
            side: Side,
            quantity: f64,
            reduce_only: bool,
        ) -> Result<OrderOutcome, String> {
            self.push(format!(
                "market {} {quantity}{}",
                side.as_str(),
                if reduce_only { " reduce" } else { "" }
            ));
            Ok(OrderOutcome {
                symbol: symbol.into(),
                side,
                filled_quantity: quantity,
                average_price: self.fill_price,
                fills: Vec::new(),
            })
        }
        async fn place_trigger(
            &self,
            _: &str,
            side: Side,
            kind: TriggerKind,
            price: f64,
        ) -> Result<u64, String> {
            self.push(format!("trigger {kind:?} {} {price}", side.as_str()));
            if self.stop_rejects && kind == TriggerKind::Stop {
                return Err("rejected".into());
            }
            Ok(1)
        }
        async fn cancel_all(&self, _: &str) -> Result<(), String> {
            self.push("cancel_all".into());
            Ok(())
        }
        async fn set_leverage(&self, _: &str, leverage: u32) -> Result<(), String> {
            self.push(format!("leverage {leverage}"));
            Ok(())
        }
        async fn hedge_mode(&self) -> Result<bool, String> {
            Ok(self.hedge)
        }
        async fn transfers(&self, _: u64) -> Result<Vec<Transfer>, String> {
            Ok(Vec::new())
        }
    }

    /// ETHUSDC perpetual rules: 0.001 lot, 0.01 tick, 20 minimum.
    fn eth(symbol: &str) -> SymbolFilters {
        SymbolFilters {
            symbol: symbol.into(),
            base_asset: "ETH".into(),
            quote_asset: "USDC".into(),
            step_size: 0.001,
            min_qty: 0.001,
            max_qty: 10_000.0,
            tick_size: 0.01,
            min_notional: 20.0,
        }
    }

    fn position(amount: f64) -> FuturesPosition {
        FuturesPosition {
            symbol: "ETHUSDC".into(),
            amount,
            entry_price: 2_000.0,
            mark_price: 2_000.0,
            liquidation_price: 0.0,
        }
    }

    fn quiet() -> Notifier {
        Notifier::disabled("USDC")
    }

    // ----- sizing -----

    #[test]
    fn size_is_risk_at_the_stop_capped_by_margin() {
        let f = eth("ETHUSDC");
        // 1% of 100 at a 40 stop: 0.025 ETH (50 notional), under the cap.
        let p = plan_entry(false, 2_000.0, 1_960.0, 100.0, 100.0, 0.01, 3, &f).unwrap();
        assert!((p.quantity - 0.025).abs() < 1e-12);
        // 10% risk wants 0.25 ETH (500); 3x on 100 caps at 285 -> 0.142.
        let p = plan_entry(false, 2_000.0, 1_960.0, 100.0, 100.0, 0.10, 3, &f).unwrap();
        assert!((p.quantity - 0.142).abs() < 1e-12);
        // A short sizes the same from above.
        let p = plan_entry(true, 2_000.0, 2_040.0, 100.0, 100.0, 0.01, 3, &f).unwrap();
        assert!((p.quantity - 0.025).abs() < 1e-12);
    }

    #[test]
    fn an_order_under_the_venue_minimum_is_skipped_not_rounded_up() {
        // The BTCUSDC case: 23 of equity at 3x cannot reach a 0.001 BTC order.
        let btc = SymbolFilters {
            min_notional: 50.0,
            ..eth("BTCUSDC")
        };
        let err = plan_entry(false, 78_000.0, 76_000.0, 23.32, 23.32, 0.10, 3, &btc).unwrap_err();
        assert!(err.contains("smallest order 78.00"), "{err}");
    }

    #[test]
    fn a_stop_on_the_wrong_side_is_refused() {
        let f = eth("ETHUSDC");
        assert!(plan_entry(false, 2_000.0, 2_010.0, 100.0, 100.0, 0.01, 3, &f).is_err());
        assert!(plan_entry(true, 2_000.0, 1_990.0, 100.0, 100.0, 0.01, 3, &f).is_err());
    }

    #[test]
    fn a_stop_beyond_liquidation_is_refused() {
        // 0.142 ETH on 100 of equity liquidates about 700 away, so a stop
        // 1,000 away is never reached.
        let f = eth("ETHUSDC");
        let err = plan_entry(false, 2_000.0, 1_000.0, 100.0, 100.0, 1.0, 3, &f).unwrap_err();
        assert!(err.contains("liquidation"), "{err}");
    }

    #[test]
    fn cross_liquidation_is_where_the_loss_meets_maintenance_margin() {
        // 0.1 ETH at 2000 on 100 equity: maintenance 1.0, cushion 990 per ETH.
        let liq = liquidation_price(2_000.0, 0.1, 100.0, false).unwrap();
        assert!((liq - 1_010.0).abs() < 1e-9);
        let liq = liquidation_price(2_000.0, 0.1, 100.0, true).unwrap();
        assert!((liq - 2_990.0).abs() < 1e-9);
        assert_eq!(liquidation_price(2_000.0, 0.01, 100.0, false), None);
    }

    // ----- execution -----

    #[tokio::test]
    async fn a_long_gets_a_stop_below_and_a_2r_target_above() {
        let ex = Fake {
            fill_price: 2_000.0,
            ..Default::default()
        };
        let plan = EntryPlan { quantity: 0.025 };
        open_position(&ex, &eth("ETHUSDC"), false, plan, 1_960.0, &quiet())
            .await
            .unwrap();
        assert_eq!(
            ex.log(),
            vec![
                "market BUY 0.025",
                "trigger Stop SELL 1960",
                "trigger TakeProfit SELL 2080",
            ]
        );
    }

    #[tokio::test]
    async fn a_short_gets_a_stop_above_and_a_2r_target_below() {
        let ex = Fake {
            fill_price: 2_000.0,
            ..Default::default()
        };
        let plan = EntryPlan { quantity: 0.025 };
        open_position(&ex, &eth("ETHUSDC"), true, plan, 2_040.0, &quiet())
            .await
            .unwrap();
        assert_eq!(
            ex.log(),
            vec![
                "market SELL 0.025",
                "trigger Stop BUY 2040",
                "trigger TakeProfit BUY 1920",
            ]
        );
    }

    #[tokio::test]
    async fn an_entry_that_cannot_get_a_stop_is_closed_again() {
        let ex = Fake {
            fill_price: 2_000.0,
            stop_rejects: true,
            ..Default::default()
        };
        let plan = EntryPlan { quantity: 0.025 };
        let result = open_position(&ex, &eth("ETHUSDC"), false, plan, 1_960.0, &quiet()).await;
        assert!(result.is_err());
        assert_eq!(
            ex.log(),
            vec![
                "market BUY 0.025",
                "trigger Stop SELL 1960",
                "trigger Stop SELL 1960",
                "market SELL 0.025 reduce",
                "cancel_all",
            ]
        );
    }

    #[tokio::test]
    async fn a_fill_past_the_stop_is_closed_without_placing_one() {
        let ex = Fake {
            fill_price: 1_955.0,
            ..Default::default()
        };
        let plan = EntryPlan { quantity: 0.025 };
        let result = open_position(&ex, &eth("ETHUSDC"), false, plan, 1_960.0, &quiet()).await;
        assert!(result.is_err());
        assert_eq!(
            ex.log(),
            vec!["market BUY 0.025", "market SELL 0.025 reduce", "cancel_all"]
        );
    }

    #[tokio::test]
    async fn an_unprotected_position_gets_an_emergency_stop() {
        let ex = Fake::default();
        let f = eth("ETHUSDC");
        ensure_stop(&ex, &f, &position(-0.02), Some(40.0), &quiet())
            .await
            .unwrap();
        ensure_stop(&ex, &f, &position(0.02), None, &quiet())
            .await
            .unwrap();
        assert_eq!(
            ex.log(),
            vec!["trigger Stop BUY 2060", "trigger Stop SELL 1960"]
        );
    }

    #[tokio::test]
    async fn a_protected_position_is_left_alone() {
        let ex = Fake::default();
        ex.triggers.lock().unwrap().push(TriggerOrder {
            id: 7,
            kind: Some(TriggerKind::Stop),
            side: Side::Sell,
            trigger_price: 1_950.0,
        });
        ensure_stop(&ex, &eth("ETHUSDC"), &position(0.02), Some(40.0), &quiet())
            .await
            .unwrap();
        assert!(ex.log().is_empty());
        // A stop on the wrong side does not protect a short.
        ensure_stop(&ex, &eth("ETHUSDC"), &position(-0.02), Some(40.0), &quiet())
            .await
            .unwrap();
        assert_eq!(ex.log(), vec!["trigger Stop BUY 2060"]);
    }

    #[tokio::test]
    async fn closing_a_short_buys_back_reduce_only_and_clears_orders() {
        let ex = Fake {
            fill_price: 1_900.0,
            ..Default::default()
        };
        close_position(&ex, &position(-0.02), "test", &quiet())
            .await
            .unwrap();
        assert_eq!(ex.log(), vec!["market BUY 0.02 reduce", "cancel_all"]);
    }

    #[tokio::test]
    async fn hedge_mode_refuses_to_start() {
        let ex = Fake {
            hedge: true,
            ..Default::default()
        };
        let config = test_config();
        let settings = FuturesSettings {
            base_url: String::new(),
            leverage: 3,
        };
        let err = startup(&ex, &config, &settings).await.unwrap_err();
        assert!(err.contains("hedge mode"), "{err}");
        assert!(ex.log().is_empty(), "nothing set on a refused account");
    }

    #[tokio::test]
    async fn startup_sets_leverage_and_flags_pairs_too_big_to_trade() {
        let ex = Fake::default();
        let mut config = test_config();
        config.trading_pairs = vec!["ETHUSDC".into()];
        let settings = FuturesSettings {
            base_url: String::new(),
            leverage: 3,
        };
        let (equity, report) = startup(&ex, &config, &settings).await.unwrap();
        assert_eq!(equity, 100.0);
        assert_eq!(ex.log(), vec!["leverage 3"]);
        assert!(
            report.contains("*ETHUSDC*: smallest order `20.00`"),
            "{report}"
        );

        let settings = FuturesSettings {
            base_url: String::new(),
            leverage: 1,
        };
        let poor = Fake::default();
        let mut tiny = test_config();
        tiny.trading_pairs = vec!["ETHUSDC".into()];
        // 0.95 x 1 x 100 = 95 can hold a 20 order; it trades.
        let (_, report) = startup(&poor, &tiny, &settings).await.unwrap();
        assert!(!report.contains("will not trade"), "{report}");
    }

    // ----- decisions -----

    fn snap() -> IndicatorSnapshot {
        IndicatorSnapshot {
            ema_50: 2_000.0,
            ema_200: 2_100.0,
            rsi_14: 55.0,
            macd_line: -1.0,
            macd_signal: 0.0,
            macd_histogram: -1.0,
            macd_crossed_bullish_recently: false,
            current_price: 1_950.0,
            swing_low: 1_800.0,
            macd_crossed_bearish_recently: true,
            swing_high: 2_020.0,
            atr_14: 40.0,
        }
    }

    #[test]
    fn a_flat_pair_in_a_downtrend_opens_a_short() {
        let p = StrategyParams::default();
        assert_eq!(
            decide(StrategyKind::TrendPullback, None, &snap(), &[], &p),
            Action::OpenShort { stop: 2_020.0 }
        );
    }

    #[test]
    fn a_short_is_held_by_short_rules_and_a_long_closed_by_long_rules() {
        let p = StrategyParams::default();
        let short = position(-0.02);
        let long = position(0.02);
        // Below EMA50: the short holds, the long's trend has broken.
        assert_eq!(
            decide(StrategyKind::TrendPullback, Some(&short), &snap(), &[], &p),
            Action::Hold
        );
        assert_eq!(
            decide(StrategyKind::TrendPullback, Some(&long), &snap(), &[], &p),
            Action::Close
        );
        // Back above EMA50: the short is covered.
        let mut above = snap();
        above.current_price = 2_050.0;
        assert_eq!(
            decide(StrategyKind::TrendPullback, Some(&short), &above, &[], &p),
            Action::Close
        );
    }

    #[test]
    fn rounding_never_moves_a_level_toward_the_market() {
        let f = eth("ETHUSDC");
        assert_eq!(round_away(&f, 2_040.004, true), 2_040.01);
        assert_eq!(round_away(&f, 2_040.004, false), 2_040.0);
        assert_eq!(round_away(&f, 2_040.0, true), 2_040.0);
    }

    fn test_config() -> Config {
        Config {
            base_url: String::new(),
            api_key: String::new(),
            api_secret: String::new(),
            quote_asset: "USDC".into(),
            trading_pairs: vec!["ETHUSDC".into()],
            poll_interval_secs: 900,
            backtest_mode: false,
            risk_per_trade: 0.01,
            trend_interval: "4h".into(),
            signal_interval: "15m".into(),
            max_open_positions: 4,
            daily_drawdown_limit: 0.05,
            strategy: StrategyKind::TrendPullback,
        }
    }
}
