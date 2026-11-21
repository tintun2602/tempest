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
//! gets one at once; one found with its stop but no target gets the 2R target
//! back, as long as the stop still shows the original risk.
//!
//! With `TRAILING_STOP_ATR` set, entries get no target: the stop trails the
//! mark price at that many ATR (the strategy's trend ATR) and only ever
//! tightens, as in the lab. Not supported: hedge mode (the bot refuses to
//! start) and more than one position per pair.

use crate::config::Config;
use crate::exchange::binance_futures::{BinanceFuturesClient, PRODUCTION_URL};
use crate::exchange::binance_wallet::BinanceWalletClient;
use crate::exchange::WalletExchange;
use crate::exchange::{
    Candle, FuturesExchange, FuturesPosition, Side, SymbolFilters, TriggerKind, TriggerOrder,
};
use crate::notify::Notifier;
use crate::risk::{CashFlow, RiskManager};
use crate::strategies::{StrategyKind, REWARD_RISK};
use crate::strategy::{self, IndicatorSnapshot, ShortDecision, Signal, StrategyParams};
use crate::wallet::{self, Moved, Wallet};
use std::collections::BTreeSet;
use std::sync::Mutex;
use std::time::Duration;
use tokio::time::sleep;
use tracing::{error, info, warn};

const DEFAULT_LEVERAGE: u32 = 3;
/// The owner's ceiling. Positions are still sized from RISK_PER_TRADE_PCT at
/// the stop, so leverage only matters when that sizing asks for more, and an
/// entry whose stop lies beyond the liquidation price is still skipped.
/// Binance caps some pairs lower; set_leverage then fails with a warning.
const MAX_LEVERAGE: u32 = 50;
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

/// Waits between attempts at a call that must not be given up on: the stop
/// for a position just opened, and closing it again if that stop never goes
/// in. Binance's IP limit resets each minute, so the last retry lands after
/// it; the IP is shared with other Railway services, so their bursts count.
#[cfg(not(test))]
const RETRY_DELAYS: [Duration; 3] = [
    Duration::from_secs(2),
    Duration::from_secs(10),
    Duration::from_secs(50),
];
/// The same number of attempts with no waiting, so tests of a whole cycle
/// do not sleep.
#[cfg(test)]
const RETRY_DELAYS: [Duration; 3] = [Duration::ZERO; 3];
/// Starts the error for an open position that is left without a stop. The
/// cycle alerts the owner on it and never treats it as a rate limit, whatever
/// the venue said underneath: that pause is only safe while every stop holds.
const NO_STOP: &str = "NO STOP";
/// Rate-limited cycles in a row before Telegram hears about it. One is
/// someone else on the shared IP; an hour of them is worth knowing.
const RATE_LIMIT_ALERT_AFTER: u32 = 4;

/// Pairs whose open position has no target and cannot be given one, already
/// reported: said once, not every cycle. Cleared when the pair has a target
/// again or is flat.
static TARGET_GAP_REPORTED: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());

/// Transfers in a non-dollar asset already warned about.
static WARNED_TRANSFERS: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());

const HEDGE_MODE_REFUSAL: &str =
    "the account is in hedge mode; switch USDⓈ-M futures to one-way mode";

pub struct FuturesSettings {
    pub base_url: String,
    pub leverage: u32,
}

impl FuturesSettings {
    /// `FUTURES_BASE_URL` (default production) and `LEVERAGE` (1-50, default 3).
    pub fn from_env() -> Self {
        let leverage = parse_leverage(std::env::var("LEVERAGE").ok().as_deref());
        Self {
            base_url: std::env::var("FUTURES_BASE_URL")
                .ok()
                .filter(|v| !v.trim().is_empty())
                .unwrap_or_else(|| PRODUCTION_URL.to_string()),
            leverage,
        }
    }
}

/// `LEVERAGE` as set, held to 1-`MAX_LEVERAGE`; the default when unset or
/// unreadable.
fn parse_leverage(value: Option<&str>) -> u32 {
    value
        .and_then(|v| v.trim().parse::<u32>().ok())
        .unwrap_or(DEFAULT_LEVERAGE)
        .clamp(1, MAX_LEVERAGE)
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
        info!(
            "Trailing stop {} x ATR; new entries get no fixed target",
            params.trailing_stop_atr
        );
    }

    if entries_paused() {
        warn!("PAUSE_ENTRIES=true: no new positions; open ones keep their stops and exits");
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
                // During an IP ban every request can extend it: wait until it
                // lifts rather than retrying each poll.
                let wait = rate_limit_wait_ms(&e, now_ms(), poll.as_millis() as u64)
                    .map_or(poll, Duration::from_millis);
                sleep(wait).await;
            }
        }
    };
    last_error.clear();

    // The day's baseline and halt come back from disk: a deploy restarts the
    // bot, and must not re-enable entries on a day already halted.
    let mut risk = RiskManager::with_risk_per_trade(equity, config.risk_per_trade)
        .with_limits(config.max_open_positions, config.daily_drawdown_limit)
        .with_state_file(crate::risk::state_file_from_env());
    if risk.halted {
        let msg = format!(
            "\u{1f6d1} *Still halted* after the restart: today's drawdown limit was hit earlier \
             (day open `{:.2}`). No new entries until the next UTC day.",
            risk.day_open_equity
        );
        warn!("{msg}");
        notifier.send(&msg).await;
    }
    let wallet_cfg = wallet::WalletConfig::from_env();
    let mut half_out_line = None;
    let mut wallet = wallet_cfg.is_on().then(|| {
        info!("Wallet moves on: {wallet_cfg:?}");
        let mut state = wallet::WalletState::with_file(wallet::state_file_from_env());
        if let Some(line) = state.half_out_line(&wallet_cfg) {
            info!("{line}");
            half_out_line = Some(line);
        }
        Wallet {
            client: BinanceWalletClient::new(
                &BinanceWalletClient::base_url_from_env(),
                &config.api_key,
                &config.api_secret,
            ),
            cfg: wallet_cfg,
            state,
        }
    });
    if let Some(line) = half_out_line {
        notifier.send(&format!("\u{1f4b0} {line}")).await;
    }
    let mut rate_limited_cycles = 0u32;
    loop {
        let mut wait = poll;
        match run_cycle(
            &client,
            config,
            settings.leverage,
            &mut risk,
            &notifier,
            &params,
            wallet.as_mut(),
        )
        .await
        {
            Ok(()) => {
                last_error.clear();
                rate_limited_cycles = 0;
            }
            Err(e) => {
                if let Some(ms) = rate_limit_wait_ms(&e, now_ms(), poll.as_millis() as u64) {
                    // The exchange still holds every stop (a position left
                    // without one is reported by the cycle as NO_STOP, never
                    // as this error); skipping a cycle costs nothing, and
                    // hammering turns a limit into a ban.
                    rate_limited_cycles += 1;
                    wait = Duration::from_millis(ms);
                    warn!(
                        "Binance rate limit ({rate_limited_cycles} cycles in a row); \
                         pausing {}s: {e}",
                        ms / 1000
                    );
                    if rate_limited_cycles == RATE_LIMIT_ALERT_AFTER {
                        notifier
                            .send(&format!(
                                "\u{26d4} *Binance rate limit* for {rate_limited_cycles} cycles in a \
                                 row. Stops on the exchange stay in place.\n`{e}`"
                            ))
                            .await;
                    }
                } else {
                    rate_limited_cycles = 0;
                    error!("Futures cycle error: {e}");
                    // Once per distinct error, not once per poll.
                    if e != last_error {
                        notifier.notify_error("Futures cycle", &e).await;
                        last_error = e;
                    }
                }
            }
        }
        sleep(wait).await;
    }
}

/// How long to pause after `error`, if it is Binance rate-limiting this IP
/// (`-1003`, or HTTP 418/429 bans): until a reported ban ends, otherwise one
/// poll. `None` for any other error.
fn rate_limit_wait_ms(error: &str, now: u64, poll_ms: u64) -> Option<u64> {
    if !is_rate_limited(error) {
        return None;
    }
    Some(match crate::banned_until_ms(error) {
        Some(until) if until > now => crate::ban_wait_ms(until, now, poll_ms),
        _ => poll_ms,
    })
}

/// Binance is rate-limiting or has banned this IP.
pub(crate) fn is_rate_limited(error: &str) -> bool {
    error.contains("error -1003")
        || error.contains("\"code\":-1003")
        || error.contains("banned until")
}

/// Run `call` until it succeeds, waiting `delays` between attempts; the last
/// error if every attempt fails.
async fn with_retries<T, F, Fut>(delays: &[Duration], what: &str, mut call: F) -> Result<T, String>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, String>>,
{
    let mut result = call().await;
    for delay in delays {
        let Err(e) = &result else { break };
        warn!("{what} failed ({e}); retrying in {}s", delay.as_secs());
        sleep(*delay).await;
        result = call().await;
    }
    result
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

async fn run_cycle<E: FuturesExchange, W: WalletExchange>(
    ex: &E,
    config: &Config,
    leverage: u32,
    risk: &mut RiskManager,
    notifier: &Notifier,
    params: &StrategyParams,
    wallet: Option<&mut Wallet<W>>,
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

    // Profit out or a top-up in. After a move the account read above is out
    // of date, so the cycle ends here and entries wait for the next one.
    if let Some(w) = wallet {
        let moved = wallet::run(
            &w.client,
            ex,
            &w.cfg,
            &mut w.state,
            account,
            &positions,
            risk,
            notifier,
            now_ms(),
        )
        .await;
        if moved != Moved::Nothing {
            return Ok(());
        }
    }

    let mut open = positions
        .iter()
        .filter(|p| config.trading_pairs.contains(&p.symbol))
        .count();
    let mut traded = false;
    for symbol in &config.trading_pairs {
        let pos = positions.iter().find(|p| &p.symbol == symbol);
        let entries_allowed = !risk.halted && !entries_paused() && open < config.max_open_positions;
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
            // Before the rate-limit arm: this error may carry a -1003 from
            // the refused stop, and must reach the owner, not the back-off.
            Err(e) if e.starts_with(NO_STOP) => {
                error!("{symbol}: {e}");
                notifier
                    .send(&format!(
                        "\u{1f6a8} *{symbol}* has NO STOP on the exchange. Place one by hand \
                         or close the position; the bot tries again next cycle.\n`{e}`"
                    ))
                    .await;
            }
            // A rate limit hits every pair alike: stop the cycle and let
            // the loop's back-off handle it, rather than spend more
            // requests and send one alert per pair.
            Err(e) if is_rate_limited(&e) => return Err(e),
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
        ensure_stop(
            ex,
            &filters,
            pos,
            snap.as_ref().map(|s| s.atr_14),
            notifier,
            &RETRY_DELAYS,
        )
        .await?;
        // Before the break-even rule: once that has moved the stop, the
        // original risk can no longer be read from it. A trailing stop takes
        // the target's place, so none is put back then.
        let trailing = params.trailing_stop_atr > 0.0;
        if !trailing {
            ensure_target(ex, &filters, pos, notifier).await?;
        }
        if params.breakeven_reached(
            pos.is_short(),
            pos.entry_price,
            pos.mark_price,
            f64::from(leverage),
        ) {
            move_stop_to_entry(ex, &filters, pos, notifier, &RETRY_DELAYS).await?;
        }
        let atr = snap
            .as_ref()
            .map(|s| s.atr_14)
            .filter(|a| a.is_finite() && *a > 0.0);
        if let (true, Some(atr)) = (trailing, atr) {
            let level = trailing_level(
                pos.is_short(),
                pos.mark_price,
                atr,
                params.trailing_stop_atr,
            );
            let moved = move_stop(
                ex,
                &filters,
                pos,
                level,
                TRAIL_MIN_STEP_ATR * atr,
                StopMove::Trail,
                notifier,
                &RETRY_DELAYS,
            )
            .await;
            // A position left without a stop, or a rate limit, ends the
            // cycle as before; any other refusal is logged and the signal
            // exit below still runs.
            if let Err(e) = moved {
                if e.contains(NO_STOP) || is_rate_limited(&e) {
                    return Err(e);
                }
                warn!("{symbol}: trailing stop not moved: {e}");
            }
        }
        if let Some(s) = snap.as_ref() {
            if lock_due(pos, s.rsi_14, params) {
                let file = crate::risk::state_dir_from_env().join(LOCK_FILE);
                if !lock_done(&file, pos) {
                    lock_position(ex, &filters, pos, params.lock_share, s.rsi_14, notifier).await?;
                    mark_locked(&file, pos);
                }
            }
        }
        let (Some(snap), Ok((_, signal))) = (snap.as_ref(), candles.as_ref()) else {
            return Ok(Traded::Nothing);
        };
        if decide(config.strategy, Some(pos), snap, signal, params) == Action::Close {
            close_position(ex, pos, "signal exit", notifier).await?;
            return Ok(Traded::Closed);
        }
        return Ok(Traded::Nothing);
    }

    target_gap_cleared(symbol);
    clear_locks(&crate::risk::state_dir_from_env().join(LOCK_FILE), symbol);
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
        config.max_open_positions,
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
    let with_target = params.trailing_stop_atr <= 0.0;
    open_position(
        ex,
        &filters,
        short,
        plan,
        stop,
        with_target,
        notifier,
        &RETRY_DELAYS,
    )
    .await?;
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
/// can carry at `leverage`. That margin is one slot's share: equity split
/// over `slots` (MAX_OPEN_POSITIONS), so the first signal cannot take it all
/// and leave the next ones too small to trade. A size under the venue minimum is skipped rather
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
    slots: usize,
    filters: &SymbolFilters,
) -> Result<EntryPlan, String> {
    let distance = if short { stop - price } else { price - stop };
    if !(distance > 0.0 && distance.is_finite() && price > 0.0) {
        return Err(format!("stop {stop:.2} is on the wrong side of {price:.2}"));
    }
    let by_risk = equity * risk_per_trade / distance;
    let slot = available.min(equity / slots.max(1) as f64);
    let by_margin = MAX_NOTIONAL_FRACTION * f64::from(leverage) * slot / price;
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
#[allow(clippy::too_many_arguments)]
async fn open_position<E: FuturesExchange>(
    ex: &E,
    filters: &SymbolFilters,
    short: bool,
    plan: EntryPlan,
    stop: f64,
    with_target: bool,
    notifier: &Notifier,
    retry: &[Duration],
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
    let mut price = fill.average_price;
    // The order response can leave the average price at zero, and the
    // position can take a moment to show its entry: ask twice.
    for wait in [0, 1_000] {
        if price > 0.0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(wait)).await;
        price = ex
            .positions()
            .await?
            .iter()
            .find(|p| p.symbol == symbol)
            .map_or(0.0, |p| p.entry_price);
    }
    let label = if short { "SHORT" } else { "LONG" };

    // An unknown fill price cannot show the stop is on the wrong side; the
    // stop was planned from the last close, so it goes in.
    let stop_ok = price <= 0.0 || if short { stop > price } else { stop < price };
    let stop_price = round_away(filters, stop, short);
    let placed = if stop_ok {
        with_retries(retry, &format!("{symbol} stop"), || {
            ex.place_trigger(symbol, close_side, TriggerKind::Stop, stop_price)
        })
        .await
    } else {
        Err(format!(
            "filled at {price:.2}, already past the stop {stop:.2}"
        ))
    };
    if let Err(e) = placed {
        let undo = with_retries(retry, &format!("{symbol} close"), || {
            ex.market_order(symbol, close_side, fill.filled_quantity, true)
        })
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
    if !with_target {
        let msg = format!(
            "{} *{symbol}* {label} `{}` @ `{price:.2}`\nStop `{stop_price}` \u{b7} Trailing stop, no fixed target",
            if short { "\u{1f53b}" } else { "\u{1f53a}" },
            fill.filled_quantity
        );
        info!("{msg}");
        notifier.send(&msg).await;
        return Ok(());
    }
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

/// Give `pos` a stop if it has none on its closing side. Retried like the
/// stop of a new entry; if it still fails the error starts with `NO_STOP`.
async fn ensure_stop<E: FuturesExchange>(
    ex: &E,
    filters: &SymbolFilters,
    pos: &FuturesPosition,
    atr: Option<f64>,
    notifier: &Notifier,
    retry: &[Duration],
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
    with_retries(retry, &format!("{} emergency stop", pos.symbol), || {
        ex.place_trigger(&pos.symbol, close_side, TriggerKind::Stop, stop)
    })
    .await
    .map_err(|e| format!("{NO_STOP}: position has no stop and placing one failed: {e}"))?;
    let msg = format!(
        "\u{1f6e1}\u{fe0f} *{}* had no stop; placed one at `{stop}` (mark `{mark:.2}`)",
        pos.symbol
    );
    warn!("{msg}");
    notifier.send(&msg).await;
    Ok(())
}

/// Give `pos` its 2R target back if it has a stop but no target on its
/// closing side: the placement failed at entry, or the order has gone.
///
/// The target is derived as at entry, from the entry price and the distance
/// to the stop. That only holds while the stop still shows the original
/// risk. A stop at or past the entry (the break-even rule, or the owner) says
/// nothing about it, and nothing else recorded it, so the position is then
/// left without a target rather than given a guessed one, and the owner is
/// told once. Any order on the closing side that could be a target counts as
/// one, so an order the owner placed by hand is never doubled or replaced.
///
/// Only a rate limit is returned as an error. Any other refusal is reported
/// once and the cycle goes on: the stop still protects the position.
async fn ensure_target<E: FuturesExchange>(
    ex: &E,
    filters: &SymbolFilters,
    pos: &FuturesPosition,
    notifier: &Notifier,
) -> Result<(), String> {
    let short = pos.is_short();
    let close_side = if short { Side::Buy } else { Side::Sell };
    let entry = pos.entry_price;
    let in_profit = |p: f64| p > 0.0 && if short { p < entry } else { p > entry };
    let orders: Vec<TriggerOrder> = ex
        .trigger_orders(&pos.symbol)
        .await?
        .into_iter()
        .filter(|o| o.side == close_side)
        .collect();
    let has_target = orders.iter().any(|o| match o.kind {
        Some(TriggerKind::TakeProfit) => true,
        Some(TriggerKind::Stop) => false,
        // A type the bot never places (a limit take-profit, say): the
        // owner's, and a target if it triggers in profit.
        None => in_profit(o.trigger_price),
    });
    if has_target {
        target_gap_cleared(&pos.symbol);
        return Ok(());
    }
    let stops: Vec<f64> = orders
        .iter()
        .filter(|o| o.kind == Some(TriggerKind::Stop))
        .map(|o| o.trigger_price)
        .collect();
    if stops.is_empty() {
        // No stop to measure from; `ensure_stop` comes first and reports it.
        return Ok(());
    }
    let unknown = |why: &str| {
        format!(
            "\u{26a0}\u{fe0f} *{}* has no target and none was placed: {why}. Place one by hand \
             if you want one; the stop is in place.",
            pos.symbol
        )
    };
    let risk = stops
        .iter()
        .map(|stop| if short { stop - entry } else { entry - stop })
        .fold(f64::INFINITY, f64::min);
    if !(risk > 0.0 && risk.is_finite() && entry > 0.0) {
        let why = "the stop is at or past the entry, so the original risk is not known";
        return report_target_gap(&pos.symbol, &unknown(why), notifier).await;
    }
    let target = if short {
        entry - REWARD_RISK * risk
    } else {
        entry + REWARD_RISK * risk
    };
    let target_price = round_away(filters, target, !short);
    let passed = if short {
        pos.mark_price <= target_price
    } else {
        pos.mark_price >= target_price
    };
    if target_price <= 0.0 || (pos.mark_price > 0.0 && passed) {
        let why = format!(
            "the 2R level `{target_price}` is not ahead of the mark price `{:.2}`",
            pos.mark_price
        );
        return report_target_gap(&pos.symbol, &unknown(&why), notifier).await;
    }
    match ex
        .place_trigger(
            &pos.symbol,
            close_side,
            TriggerKind::TakeProfit,
            target_price,
        )
        .await
    {
        Ok(_) => {
            target_gap_cleared(&pos.symbol);
            let msg = format!(
                "\u{1f3af} *{}* had no target; placed one at `{target_price}` (2R from entry \
                 `{entry:.2}`)",
                pos.symbol
            );
            warn!("{msg}");
            notifier.send(&msg).await;
            Ok(())
        }
        Err(e) if is_rate_limited(&e) => Err(e),
        Err(e) => {
            let why = format!("the venue refused `{target_price}` (`{e}`)");
            report_target_gap(&pos.symbol, &unknown(&why), notifier).await
        }
    }
}

/// Say that `symbol` is left without a target, the first time only.
async fn report_target_gap(symbol: &str, msg: &str, notifier: &Notifier) -> Result<(), String> {
    let first = TARGET_GAP_REPORTED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(symbol.to_string());
    if first {
        warn!("{msg}");
        notifier.send(msg).await;
    }
    Ok(())
}

fn target_gap_cleared(symbol: &str) {
    TARGET_GAP_REPORTED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(symbol);
}

/// The break-even rule: replace a stop that would still lose money with one
/// at the entry price. The old stop is cancelled first, because the venue
/// refuses a second close-position stop; `ensure_stop` covers a crash in
/// between on the next cycle. A stop already at or past the
/// entry (one the owner moved by hand, say) is left alone.
///
/// Both the new stop and, failing that, putting the old one back are retried
/// like the stop of a new entry. If neither goes in, the position has no stop
/// and the error starts with `NO_STOP`.
async fn move_stop_to_entry<E: FuturesExchange>(
    ex: &E,
    filters: &SymbolFilters,
    pos: &FuturesPosition,
    notifier: &Notifier,
    retry: &[Duration],
) -> Result<(), String> {
    move_stop(
        ex,
        filters,
        pos,
        pos.entry_price,
        0.0,
        StopMove::BreakEven,
        notifier,
        retry,
    )
    .await
}

/// Trailing stop moves smaller than this many ATR wait for a bigger one, so
/// the stop is not replaced every cycle for a tick.
const TRAIL_MIN_STEP_ATR: f64 = 0.1;

/// Where a trailing stop `k` ATR behind `mark` sits.
fn trailing_level(short: bool, mark: f64, atr: f64, k: f64) -> f64 {
    if short {
        mark + k * atr
    } else {
        mark - k * atr
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum StopMove {
    BreakEven,
    Trail,
}

/// Tighten the stop to `level` when every stop on the closing side is
/// looser than it by more than `min_step`; a stop never loosens. Binance
/// allows one close-position stop per side (-4130 otherwise), so the old one
/// goes first; if the new one is then refused, the old level is put back
/// before the error is reported.
#[allow(clippy::too_many_arguments)]
async fn move_stop<E: FuturesExchange>(
    ex: &E,
    filters: &SymbolFilters,
    pos: &FuturesPosition,
    level: f64,
    min_step: f64,
    what: StopMove,
    notifier: &Notifier,
    retry: &[Duration],
) -> Result<(), String> {
    let short = pos.is_short();
    let close_side = if short { Side::Buy } else { Side::Sell };
    if !(level.is_finite() && level > 0.0) {
        return Ok(());
    }
    // A trailing level on the wrong side of the mark would fire at once.
    if what == StopMove::Trail
        && (pos.mark_price <= 0.0
            || if short {
                level <= pos.mark_price
            } else {
                level >= pos.mark_price
            })
    {
        return Ok(());
    }
    let stops: Vec<TriggerOrder> = ex
        .trigger_orders(&pos.symbol)
        .await?
        .into_iter()
        .filter(|o| o.kind == Some(TriggerKind::Stop) && o.side == close_side)
        .collect();
    // Normally `ensure_stop` has placed one first. A trail never adds a
    // stop of its own; break-even places one at the entry, as it always has.
    if stops.is_empty() && what == StopMove::Trail {
        return Ok(());
    }
    let tight_enough = |p: f64| {
        if short {
            p <= level + min_step
        } else {
            p >= level - min_step
        }
    };
    if stops.iter().any(|o| tight_enough(o.trigger_price)) {
        return Ok(());
    }
    // Rounded toward profit: down for a short, up for a long.
    let stop = round_away(filters, level, !short);
    let label = match what {
        StopMove::BreakEven => "break-even",
        StopMove::Trail => "trailing",
    };
    for old in &stops {
        ex.cancel_trigger(&pos.symbol, old.id)
            .await
            .map_err(|e| format!("{label}: old stop {} not cancelled: {e}", old.id))?;
    }
    let placed = with_retries(retry, &format!("{} {label} stop", pos.symbol), || {
        ex.place_trigger(&pos.symbol, close_side, TriggerKind::Stop, stop)
    })
    .await;
    if let Err(e) = placed {
        let Some(old) = stops.first() else {
            return Err(format!(
                "{NO_STOP}: {label} stop not placed: {e}; there was no old stop"
            ));
        };
        let restored = with_retries(retry, &format!("{} old stop", pos.symbol), || {
            ex.place_trigger(
                &pos.symbol,
                close_side,
                TriggerKind::Stop,
                old.trigger_price,
            )
        })
        .await;
        return Err(match restored {
            Ok(_) => format!(
                "{label} stop not placed: {e}; old stop {} restored",
                old.trigger_price
            ),
            Err(r) => format!(
                "{NO_STOP}: {label} stop not placed: {e}; old stop {} NOT RESTORED: {r}",
                old.trigger_price
            ),
        });
    }
    match what {
        StopMove::BreakEven => {
            let msg = format!(
                "\u{1f512} *{}* stop moved to entry `{stop}` (mark `{:.2}`)",
                pos.symbol, pos.mark_price
            );
            info!("{msg}");
            notifier.send(&msg).await;
        }
        // Every few cycles while a trade runs: the log, not Telegram.
        StopMove::Trail => info!(
            "{}: trailing stop moved to {stop} (mark {:.2})",
            pos.symbol, pos.mark_price
        ),
    }
    Ok(())
}

/// Positions the lock rule has already sold its share of, as
/// `SYMBOL@entry` lines, so a restart does not sell a second share.
const LOCK_FILE: &str = "lock-state.txt";

/// The lock rule (`LOCK_RSI`, `LOCK_PCT`): with the daily RSI(14) below
/// `lock_rsi` on a short (above 100 minus it on a long) and the position in
/// profit at the mark, sell `lock_share` of it once. Lab, 2 years: at 15 and
/// half, a small gain on both four and ten pairs in and out of sample.
fn lock_due(pos: &FuturesPosition, rsi: f64, params: &StrategyParams) -> bool {
    if params.lock_rsi <= 0.0 || params.lock_share <= 0.0 || !rsi.is_finite() {
        return false;
    }
    let short = pos.is_short();
    let extreme = if short {
        rsi < params.lock_rsi
    } else {
        rsi > 100.0 - params.lock_rsi
    };
    let in_profit = pos.mark_price > 0.0
        && if short {
            pos.mark_price < pos.entry_price
        } else {
            pos.mark_price > pos.entry_price
        };
    extreme && in_profit
}

fn lock_key(pos: &FuturesPosition) -> String {
    format!("{}@{}", pos.symbol, pos.entry_price)
}

fn lock_done(file: &std::path::Path, pos: &FuturesPosition) -> bool {
    let key = lock_key(pos);
    std::fs::read_to_string(file)
        .map(|s| s.lines().any(|l| l == key))
        .unwrap_or(false)
}

fn mark_locked(file: &std::path::Path, pos: &FuturesPosition) {
    let mut lines: Vec<String> = std::fs::read_to_string(file)
        .map(|s| s.lines().map(String::from).collect())
        .unwrap_or_default();
    lines.push(lock_key(pos));
    if let Some(dir) = file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Err(e) = std::fs::write(file, lines.join("\n") + "\n") {
        warn!(
            "lock state not saved ({e}); a restart may lock {} again",
            pos.symbol
        );
    }
}

/// A flat pair forgets its lock, so its next position can lock again.
fn clear_locks(file: &std::path::Path, symbol: &str) {
    let Ok(text) = std::fs::read_to_string(file) else {
        return;
    };
    let prefix = format!("{symbol}@");
    let kept: Vec<&str> = text.lines().filter(|l| !l.starts_with(&prefix)).collect();
    if kept.len() != text.lines().count() {
        let _ = std::fs::write(
            file,
            kept.join("\n") + if kept.is_empty() { "" } else { "\n" },
        );
    }
}

/// Sell `share` of `pos` at market, reduce-only. A share too small to trade
/// is skipped (and counts as done): the stop and trail still manage it all.
async fn lock_position<E: FuturesExchange>(
    ex: &E,
    filters: &SymbolFilters,
    pos: &FuturesPosition,
    share: f64,
    rsi: f64,
    notifier: &Notifier,
) -> Result<(), String> {
    let quantity = filters.round_quantity(pos.amount.abs() * share.min(1.0));
    if quantity <= 0.0 || filters.check_order(quantity, pos.mark_price).is_err() {
        info!(
            "{}: lock skipped, {quantity} is below the venue minimum",
            pos.symbol
        );
        return Ok(());
    }
    let side = if pos.is_short() {
        Side::Buy
    } else {
        Side::Sell
    };
    ex.market_order(&pos.symbol, side, quantity, true).await?;
    let msg = format!(
        "\u{1f510} *{}* locked {:.0} % at RSI `{rsi:.1}` (mark `{:.2}`, entry `{:.2}`); the rest rides the trail",
        pos.symbol,
        share * 100.0,
        pos.mark_price,
        pos.entry_price
    );
    info!("{msg}");
    notifier.send(&msg).await;
    Ok(())
}

/// `PAUSE_ENTRIES=true`: the owner wants no new positions. Open ones are
/// still managed (stop, break-even, trail, signal exit) and the wallet rules
/// still run.
fn entries_paused() -> bool {
    std::env::var("PAUSE_ENTRIES")
        .map(|v| v.trim().eq_ignore_ascii_case("true"))
        .unwrap_or(false)
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
                // Said once per transfer, not once per cycle.
                let first = WARNED_TRANSFERS
                    .lock()
                    .map_or(true, |mut seen| seen.insert(t.id.clone()));
                if first {
                    warn!(
                        "Transfer of {} {} not counted as a cash flow",
                        t.amount, t.asset
                    );
                }
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

    #[derive(Default)]
    struct Fake {
        positions: Mutex<Vec<FuturesPosition>>,
        triggers: Mutex<Vec<TriggerOrder>>,
        /// Every call, in order.
        log: Mutex<Vec<String>>,
        fill_price: f64,
        stop_rejects: bool,
        /// Stop attempts rejected before one goes through.
        stop_failures: Mutex<u32>,
        /// A stop at exactly this price is always refused.
        reject_stop_at: Option<f64>,
        /// When set, every target is refused with this error.
        target_error: Option<String>,
        /// When set, every klines request fails with this error.
        klines_error: Option<String>,
        hedge: bool,
        /// Collateral held in the futures wallet, as `asset_wallet` reports.
        held: f64,
        /// Regular open orders on the account, besides the conditional ones.
        other_orders: usize,
        /// What `account` reports after a transfer, when set.
        account_after: Mutex<Option<FuturesAccount>>,
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
        async fn klines(&self, symbol: &str, _: &str, _: u32) -> Result<Vec<Candle>, String> {
            self.push(format!("klines {symbol}"));
            if let Some(e) = &self.klines_error {
                return Err(e.clone());
            }
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
            if let Some(after) = *self.account_after.lock().unwrap() {
                return Ok(after);
            }
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
            if kind == TriggerKind::Stop {
                if self.reject_stop_at == Some(price) {
                    return Err("rejected".into());
                }
                let mut left = self.stop_failures.lock().unwrap();
                if *left > 0 {
                    *left -= 1;
                    return Err("Binance API error -1003: Too many requests".into());
                }
            }
            if kind == TriggerKind::TakeProfit {
                if let Some(e) = &self.target_error {
                    return Err(e.clone());
                }
            }
            // The order now rests on the venue, as a later read will show.
            let mut triggers = self.triggers.lock().unwrap();
            let id = 100 + triggers.len() as u64;
            triggers.push(TriggerOrder {
                id,
                kind: Some(kind),
                side,
                trigger_price: price,
            });
            Ok(id)
        }
        async fn cancel_trigger(&self, _: &str, id: u64) -> Result<(), String> {
            self.push(format!("cancel {id}"));
            self.triggers.lock().unwrap().retain(|o| o.id != id);
            Ok(())
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
        async fn open_order_count(&self) -> Result<usize, String> {
            Ok(self.triggers.lock().unwrap().len() + self.other_orders)
        }
        async fn asset_wallet(&self, _: &str) -> Result<f64, String> {
            Ok(self.held)
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

    /// Three retries, no waiting.
    const NO_WAIT: [Duration; 3] = [Duration::ZERO; 3];

    fn quiet() -> Notifier {
        Notifier::disabled("USDC")
    }

    // ----- sizing -----

    #[test]
    fn size_is_risk_at_the_stop_capped_by_margin() {
        let f = eth("ETHUSDC");
        // 1% of 100 at a 40 stop: 0.025 ETH (50 notional), under the cap.
        let p = plan_entry(false, 2_000.0, 1_960.0, 100.0, 100.0, 0.01, 3, 1, &f).unwrap();
        assert!((p.quantity - 0.025).abs() < 1e-12);
        // 10% risk wants 0.25 ETH (500); 3x on 100 caps at 285 -> 0.142.
        let p = plan_entry(false, 2_000.0, 1_960.0, 100.0, 100.0, 0.10, 3, 1, &f).unwrap();
        assert!((p.quantity - 0.142).abs() < 1e-12);
        // A short sizes the same from above.
        let p = plan_entry(true, 2_000.0, 2_040.0, 100.0, 100.0, 0.01, 3, 1, &f).unwrap();
        assert!((p.quantity - 0.025).abs() < 1e-12);
    }

    #[test]
    fn margin_is_split_over_the_position_slots() {
        let f = eth("ETHUSDC");
        // 75 of equity at 20x, three slots: 25 each, 475 notional.
        let p = plan_entry(true, 2_000.0, 2_080.0, 75.0, 75.0, 0.8, 20, 3, &f).unwrap();
        assert!((p.quantity - 0.237).abs() < 1e-12);
        // With two slots in use, the last one gets what is left, never more.
        let p = plan_entry(true, 2_000.0, 2_080.0, 75.0, 20.0, 0.8, 20, 3, &f).unwrap();
        assert!((p.quantity - 0.19).abs() < 1e-12);
    }

    #[test]
    fn leverage_is_held_between_one_and_fifty() {
        assert_eq!(parse_leverage(None), 3);
        assert_eq!(parse_leverage(Some("5")), 5);
        assert_eq!(parse_leverage(Some(" 20 ")), 20);
        assert_eq!(parse_leverage(Some("50")), 50);
        assert_eq!(parse_leverage(Some("125")), 50);
        assert_eq!(parse_leverage(Some("0")), 1);
        assert_eq!(parse_leverage(Some("lots")), 3);
    }

    #[test]
    fn five_x_lifts_the_margin_cap_but_not_the_risk() {
        let f = eth("ETHUSDC");
        // 10% of 83 at a 2.5% stop wants 0.166 ETH (332 notional).
        // 3x on 83 caps at 236.55 -> 0.118; 5x allows 394 -> the full 0.166.
        let at3 = plan_entry(false, 2_000.0, 1_950.0, 83.0, 83.0, 0.10, 3, 1, &f).unwrap();
        let at5 = plan_entry(false, 2_000.0, 1_950.0, 83.0, 83.0, 0.10, 5, 1, &f).unwrap();
        assert!((at3.quantity - 0.118).abs() < 1e-12);
        assert!((at5.quantity - 0.166).abs() < 1e-12);
        // Same stop distance, so a stop-out at 5x still costs 10% of equity.
        assert!((at5.quantity * 50.0 - 8.3).abs() < 1e-9);
    }

    #[test]
    fn an_order_under_the_venue_minimum_is_skipped_not_rounded_up() {
        // The BTCUSDC case: 23 of equity at 3x cannot reach a 0.001 BTC order.
        let btc = SymbolFilters {
            min_notional: 50.0,
            ..eth("BTCUSDC")
        };
        let err =
            plan_entry(false, 78_000.0, 76_000.0, 23.32, 23.32, 0.10, 3, 1, &btc).unwrap_err();
        assert!(err.contains("smallest order 78.00"), "{err}");
    }

    #[test]
    fn a_stop_on_the_wrong_side_is_refused() {
        let f = eth("ETHUSDC");
        assert!(plan_entry(false, 2_000.0, 2_010.0, 100.0, 100.0, 0.01, 3, 1, &f).is_err());
        assert!(plan_entry(true, 2_000.0, 1_990.0, 100.0, 100.0, 0.01, 3, 1, &f).is_err());
    }

    #[test]
    fn a_stop_beyond_liquidation_is_refused() {
        // 0.142 ETH on 100 of equity liquidates about 700 away, so a stop
        // 1,000 away is never reached.
        let f = eth("ETHUSDC");
        let err = plan_entry(false, 2_000.0, 1_000.0, 100.0, 100.0, 1.0, 3, 1, &f).unwrap_err();
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
        open_position(
            &ex,
            &eth("ETHUSDC"),
            false,
            plan,
            1_960.0,
            true,
            &quiet(),
            &NO_WAIT,
        )
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
        open_position(
            &ex,
            &eth("ETHUSDC"),
            true,
            plan,
            2_040.0,
            true,
            &quiet(),
            &NO_WAIT,
        )
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
        let result = open_position(
            &ex,
            &eth("ETHUSDC"),
            false,
            plan,
            1_960.0,
            true,
            &quiet(),
            &NO_WAIT,
        )
        .await;
        assert!(result.is_err());
        assert_eq!(
            ex.log(),
            vec![
                "market BUY 0.025",
                "trigger Stop SELL 1960",
                "trigger Stop SELL 1960",
                "trigger Stop SELL 1960",
                "trigger Stop SELL 1960",
                "market SELL 0.025 reduce",
                "cancel_all",
            ]
        );
    }

    #[tokio::test]
    async fn a_rate_limited_stop_is_retried_until_it_goes_in() {
        let ex = Fake {
            fill_price: 2_000.0,
            stop_failures: Mutex::new(2),
            ..Default::default()
        };
        let plan = EntryPlan { quantity: 0.025 };
        open_position(
            &ex,
            &eth("ETHUSDC"),
            false,
            plan,
            1_960.0,
            true,
            &quiet(),
            &NO_WAIT,
        )
        .await
        .unwrap();
        assert_eq!(
            ex.log(),
            vec![
                "market BUY 0.025",
                "trigger Stop SELL 1960",
                "trigger Stop SELL 1960",
                "trigger Stop SELL 1960",
                "trigger TakeProfit SELL 2080",
            ],
            "kept, not closed: the third attempt went in"
        );
    }

    #[tokio::test]
    async fn a_rate_limit_ends_the_cycle_instead_of_trying_every_pair() {
        let ex = Fake {
            klines_error: Some("Binance API error -1003: Too many requests".into()),
            ..Default::default()
        };
        let mut config = test_config();
        config.trading_pairs = vec!["ETHUSDC".into(), "SOLUSDC".into(), "BTCUSDC".into()];
        let mut risk = RiskManager::with_risk_per_trade(100.0, 0.02);
        let err = run_cycle(
            &ex,
            &config,
            3,
            &mut risk,
            &quiet(),
            &StrategyParams::default(),
            no_wallet(),
        )
        .await
        .unwrap_err();
        assert!(is_rate_limited(&err), "{err}");
        assert!(
            ex.log().iter().all(|l| !l.contains("SOLUSDC")),
            "stopped at the first pair: {:?}",
            ex.log()
        );
    }

    #[test]
    fn rate_limits_pause_and_other_errors_do_not() {
        let poll = 900_000;
        let limited = "Binance API error -1003: Too many requests; current limit of IP(1.2.3.4) \
                       is 2400 requests per minute.";
        assert_eq!(rate_limit_wait_ms(limited, 0, poll), Some(poll));
        // A ban waits until it lifts, plus a margin.
        let banned = "Binance API error -1003: Way too many requests; IP(1.2.3.4) banned \
                      until 7200000. Please use the websocket for live updates to avoid bans.";
        let wait = rate_limit_wait_ms(banned, 0, poll).unwrap();
        assert!(wait > 7_200_000 && wait < 7_300_000, "{wait}");
        assert_eq!(
            rate_limit_wait_ms("Binance API error -2019: Margin is insufficient.", 0, poll),
            None
        );
    }

    #[tokio::test]
    async fn a_fill_past_the_stop_is_closed_without_placing_one() {
        let ex = Fake {
            fill_price: 1_955.0,
            ..Default::default()
        };
        let plan = EntryPlan { quantity: 0.025 };
        let result = open_position(
            &ex,
            &eth("ETHUSDC"),
            false,
            plan,
            1_960.0,
            true,
            &quiet(),
            &NO_WAIT,
        )
        .await;
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
        ensure_stop(&ex, &f, &position(-0.02), Some(40.0), &quiet(), &NO_WAIT)
            .await
            .unwrap();
        ensure_stop(&ex, &f, &position(0.02), None, &quiet(), &NO_WAIT)
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
        ensure_stop(
            &ex,
            &eth("ETHUSDC"),
            &position(0.02),
            Some(40.0),
            &quiet(),
            &NO_WAIT,
        )
        .await
        .unwrap();
        assert!(ex.log().is_empty());
        // A stop on the wrong side does not protect a short.
        ensure_stop(
            &ex,
            &eth("ETHUSDC"),
            &position(-0.02),
            Some(40.0),
            &quiet(),
            &NO_WAIT,
        )
        .await
        .unwrap();
        assert_eq!(ex.log(), vec!["trigger Stop BUY 2060"]);
    }

    fn stop_at(id: u64, price: f64) -> TriggerOrder {
        TriggerOrder {
            id,
            kind: Some(TriggerKind::Stop),
            side: Side::Buy,
            trigger_price: price,
        }
    }

    #[tokio::test]
    async fn the_trailing_stop_follows_a_falling_short_down() {
        let ex = Fake::default();
        ex.triggers.lock().unwrap().push(stop_at(7, 2_100.0));
        let short = FuturesPosition {
            mark_price: 1_900.0,
            ..position(-0.02)
        };
        // 3 x 20 ATR behind a 1 900 mark: 1 960.
        let level = trailing_level(true, short.mark_price, 20.0, 3.0);
        assert_eq!(level, 1_960.0);
        move_stop(
            &ex,
            &eth("ETHUSDC"),
            &short,
            level,
            2.0,
            StopMove::Trail,
            &quiet(),
            &NO_WAIT,
        )
        .await
        .unwrap();
        assert_eq!(ex.log(), vec!["cancel 7", "trigger Stop BUY 1960"]);
    }

    #[tokio::test]
    async fn the_trailing_stop_never_loosens_and_skips_tiny_steps() {
        let short = FuturesPosition {
            mark_price: 1_900.0,
            ..position(-0.02)
        };
        let f = eth("ETHUSDC");
        // Already tighter than the trailing level: left alone.
        let ex = Fake::default();
        ex.triggers.lock().unwrap().push(stop_at(7, 1_950.0));
        move_stop(
            &ex,
            &f,
            &short,
            1_960.0,
            2.0,
            StopMove::Trail,
            &quiet(),
            &NO_WAIT,
        )
        .await
        .unwrap();
        assert!(ex.log().is_empty());
        // Looser by less than the minimum step: waits.
        let ex = Fake::default();
        ex.triggers.lock().unwrap().push(stop_at(7, 1_961.0));
        move_stop(
            &ex,
            &f,
            &short,
            1_960.0,
            2.0,
            StopMove::Trail,
            &quiet(),
            &NO_WAIT,
        )
        .await
        .unwrap();
        assert!(ex.log().is_empty());
        // A level on the wrong side of the mark would fire at once: refused.
        let ex = Fake::default();
        ex.triggers.lock().unwrap().push(stop_at(7, 2_100.0));
        move_stop(
            &ex,
            &f,
            &short,
            1_890.0,
            2.0,
            StopMove::Trail,
            &quiet(),
            &NO_WAIT,
        )
        .await
        .unwrap();
        assert!(ex.log().is_empty());
        // No stop at all: ensure_stop's job, never a second one here.
        let ex = Fake::default();
        move_stop(
            &ex,
            &f,
            &short,
            1_960.0,
            2.0,
            StopMove::Trail,
            &quiet(),
            &NO_WAIT,
        )
        .await
        .unwrap();
        assert!(ex.log().is_empty());
    }

    #[tokio::test]
    async fn break_even_still_places_a_stop_when_there_is_none() {
        let ex = Fake::default();
        let short = FuturesPosition {
            mark_price: 1_900.0,
            ..position(-0.02)
        };
        move_stop_to_entry(&ex, &eth("ETHUSDC"), &short, &quiet(), &NO_WAIT)
            .await
            .unwrap();
        assert_eq!(ex.log(), vec!["trigger Stop BUY 2000"]);
    }

    #[tokio::test]
    async fn a_long_with_an_unknown_fill_price_still_gets_its_stop() {
        // The order response said 0 and the position did not show yet: the
        // long must keep its planned stop, not be closed as "past the stop".
        let ex = Fake::default();
        let plan = EntryPlan { quantity: 0.025 };
        open_position(
            &ex,
            &eth("ETHUSDC"),
            false,
            plan,
            1_960.0,
            false,
            &quiet(),
            &NO_WAIT,
        )
        .await
        .unwrap();
        assert_eq!(ex.log(), vec!["market BUY 0.025", "trigger Stop SELL 1960"]);
    }

    #[test]
    fn the_lock_rule_fires_at_an_rsi_extreme_in_profit_only() {
        let p = StrategyParams {
            lock_rsi: 15.0,
            lock_share: 0.5,
            ..StrategyParams::default()
        };
        let short = FuturesPosition {
            mark_price: 1_900.0,
            ..position(-0.02)
        };
        assert!(lock_due(&short, 10.0, &p));
        assert!(!lock_due(&short, 20.0, &p));
        let losing = FuturesPosition {
            mark_price: 2_050.0,
            ..position(-0.02)
        };
        assert!(!lock_due(&losing, 10.0, &p));
        let long = FuturesPosition {
            mark_price: 2_100.0,
            ..position(0.02)
        };
        assert!(lock_due(&long, 90.0, &p));
        assert!(!lock_due(&long, 10.0, &p));
        assert!(!lock_due(&short, 10.0, &StrategyParams::default()));
    }

    #[test]
    fn a_lock_is_remembered_until_the_pair_is_flat() {
        let dir = std::env::temp_dir().join(format!("tempest-lock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let file = dir.join(LOCK_FILE);
        let short = position(-0.02);
        assert!(!lock_done(&file, &short));
        mark_locked(&file, &short);
        assert!(lock_done(&file, &short));
        clear_locks(&file, "BTCUSDC");
        assert!(lock_done(&file, &short));
        clear_locks(&file, "ETHUSDC");
        assert!(!lock_done(&file, &short));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn locking_sells_half_reduce_only() {
        let ex = Fake::default();
        let short = FuturesPosition {
            mark_price: 1_900.0,
            ..position(-0.1)
        };
        lock_position(&ex, &eth("ETHUSDC"), &short, 0.5, 10.0, &quiet())
            .await
            .unwrap();
        assert_eq!(ex.log(), vec!["market BUY 0.05 reduce"]);
    }

    #[tokio::test]
    async fn with_a_trailing_stop_an_entry_gets_no_target() {
        let ex = Fake {
            fill_price: 2_000.0,
            ..Default::default()
        };
        let plan = EntryPlan { quantity: 0.025 };
        open_position(
            &ex,
            &eth("ETHUSDC"),
            true,
            plan,
            2_040.0,
            false,
            &quiet(),
            &NO_WAIT,
        )
        .await
        .unwrap();
        assert_eq!(ex.log(), vec!["market SELL 0.025", "trigger Stop BUY 2040"]);
    }

    #[tokio::test]
    async fn the_break_even_rule_moves_a_losing_stop_to_the_entry() {
        let ex = Fake::default();
        ex.triggers.lock().unwrap().push(TriggerOrder {
            id: 7,
            kind: Some(TriggerKind::Stop),
            side: Side::Buy,
            trigger_price: 2_100.0,
        });
        let short = FuturesPosition {
            mark_price: 1_900.0,
            ..position(-0.02)
        };
        move_stop_to_entry(&ex, &eth("ETHUSDC"), &short, &quiet(), &NO_WAIT)
            .await
            .unwrap();
        // The venue allows one close-position stop per side: old out, new in.
        assert_eq!(ex.log(), vec!["cancel 7", "trigger Stop BUY 2000"]);
    }

    /// A short from 2000 with its stop at 2100 (id 7), marked at 1900: far
    /// enough in profit for the break-even rule at 20x.
    fn short_due_for_break_even(ex: &Fake) -> FuturesPosition {
        ex.triggers.lock().unwrap().push(TriggerOrder {
            id: 7,
            kind: Some(TriggerKind::Stop),
            side: Side::Buy,
            trigger_price: 2_100.0,
        });
        FuturesPosition {
            mark_price: 1_900.0,
            ..position(-0.02)
        }
    }

    fn stops_on(ex: &Fake) -> Vec<f64> {
        ex.triggers
            .lock()
            .unwrap()
            .iter()
            .filter(|o| o.kind == Some(TriggerKind::Stop))
            .map(|o| o.trigger_price)
            .collect()
    }

    #[tokio::test]
    async fn a_rate_limited_break_even_stop_is_retried_until_it_goes_in() {
        let ex = Fake::default();
        *ex.stop_failures.lock().unwrap() = 2;
        let short = short_due_for_break_even(&ex);
        move_stop_to_entry(&ex, &eth("ETHUSDC"), &short, &quiet(), &NO_WAIT)
            .await
            .unwrap();
        assert_eq!(
            ex.log(),
            vec![
                "cancel 7",
                "trigger Stop BUY 2000",
                "trigger Stop BUY 2000",
                "trigger Stop BUY 2000",
            ]
        );
        assert_eq!(stops_on(&ex), vec![2_000.0]);
    }

    #[tokio::test]
    async fn a_refused_break_even_stop_puts_the_old_one_back() {
        let ex = Fake {
            reject_stop_at: Some(2_000.0),
            ..Default::default()
        };
        let short = short_due_for_break_even(&ex);
        let err = move_stop_to_entry(&ex, &eth("ETHUSDC"), &short, &quiet(), &NO_WAIT)
            .await
            .unwrap_err();
        assert!(err.contains("old stop 2100 restored"), "{err}");
        assert!(!err.starts_with(NO_STOP), "{err}");
        assert_eq!(
            ex.log(),
            vec![
                "cancel 7",
                "trigger Stop BUY 2000",
                "trigger Stop BUY 2000",
                "trigger Stop BUY 2000",
                "trigger Stop BUY 2000",
                "trigger Stop BUY 2100",
            ]
        );
        assert_eq!(stops_on(&ex), vec![2_100.0]);
    }

    #[tokio::test]
    async fn a_rate_limited_restore_is_retried_too() {
        // The new level is refused outright and the first two attempts at
        // putting the old one back hit the rate limit.
        let ex = Fake {
            reject_stop_at: Some(2_000.0),
            stop_failures: Mutex::new(2),
            ..Default::default()
        };
        let short = short_due_for_break_even(&ex);
        let err = move_stop_to_entry(&ex, &eth("ETHUSDC"), &short, &quiet(), &NO_WAIT)
            .await
            .unwrap_err();
        assert!(err.contains("old stop 2100 restored"), "{err}");
        assert_eq!(stops_on(&ex), vec![2_100.0]);
    }

    #[tokio::test]
    async fn a_position_left_without_a_stop_alerts_the_owner_and_gets_one_next_cycle() {
        // Every attempt at the new stop and at restoring the old one is
        // rate-limited: four of each.
        let ex = Fake::default();
        *ex.stop_failures.lock().unwrap() = 8;
        let short = short_due_for_break_even(&ex);
        ex.positions.lock().unwrap().push(short);
        ex.triggers.lock().unwrap().push(order(
            8,
            Some(TriggerKind::TakeProfit),
            Side::Buy,
            1_800.0,
        ));
        let config = test_config();
        let params = StrategyParams {
            breakeven_roi_pct: 100.0,
            partial_tp_share: 0.0,
            lock_rsi: 0.0,
            lock_share: 0.0,
            ..StrategyParams::default()
        };
        let mut risk = RiskManager::with_risk_per_trade(100.0, 0.02);
        let notifier = quiet();

        // Not an Err: a rate-limit error here would pause the loop with the
        // position unprotected.
        run_cycle(&ex, &config, 20, &mut risk, &notifier, &params, no_wallet())
            .await
            .unwrap();
        assert!(stops_on(&ex).is_empty(), "the failure under test");
        let alerts = notifier.sent();
        assert_eq!(alerts.len(), 1, "{alerts:?}");
        assert!(alerts[0].contains("*ETHUSDC* has NO STOP"), "{alerts:?}");
        assert!(alerts[0].contains("NOT RESTORED"), "{alerts:?}");

        // The next cycle finds the position bare and protects it first.
        let before = ex.log().len();
        run_cycle(&ex, &config, 20, &mut risk, &notifier, &params, no_wallet())
            .await
            .unwrap();
        // No ATR from one candle: 2% above the 1900 mark.
        assert_eq!(
            ex.log()[before..],
            ["klines ETHUSDC", "klines ETHUSDC", "trigger Stop BUY 1938"]
        );
        assert_eq!(stops_on(&ex), vec![1_938.0]);
    }

    #[tokio::test]
    async fn an_emergency_stop_that_never_goes_in_is_not_a_rate_limit() {
        let ex = Fake::default();
        *ex.stop_failures.lock().unwrap() = 4;
        let err = ensure_stop(
            &ex,
            &eth("ETHUSDC"),
            &position(0.02),
            None,
            &quiet(),
            &NO_WAIT,
        )
        .await
        .unwrap_err();
        assert!(err.starts_with(NO_STOP), "{err}");
        assert!(err.contains("-1003"), "{err}");
        assert_eq!(ex.log().len(), 4, "retried: {:?}", ex.log());
    }

    // ----- the target of an open position -----

    fn order(id: u64, kind: Option<TriggerKind>, side: Side, trigger_price: f64) -> TriggerOrder {
        TriggerOrder {
            id,
            kind,
            side,
            trigger_price,
        }
    }

    /// The Fake with these orders resting.
    fn resting(orders: Vec<TriggerOrder>) -> Fake {
        let ex = Fake::default();
        *ex.triggers.lock().unwrap() = orders;
        ex
    }

    /// A position in a pair no other test uses: what has been reported once
    /// is remembered per pair for the whole process.
    fn position_in(symbol: &str, amount: f64) -> FuturesPosition {
        FuturesPosition {
            symbol: symbol.into(),
            ..position(amount)
        }
    }

    #[tokio::test]
    async fn a_missing_target_is_placed_once_at_2r_from_the_entry() {
        let f = eth("ETHUSDC");
        // Long from 2000, stop 40 below: target 80 above.
        let ex = resting(vec![order(7, Some(TriggerKind::Stop), Side::Sell, 1_960.0)]);
        let notifier = quiet();
        ensure_target(&ex, &f, &position(0.02), &notifier)
            .await
            .unwrap();
        // The next cycle sees it resting and adds nothing.
        ensure_target(&ex, &f, &position(0.02), &notifier)
            .await
            .unwrap();
        assert_eq!(ex.log(), vec!["trigger TakeProfit SELL 2080"]);
        let sent = notifier.sent();
        assert_eq!(sent.len(), 1, "{sent:?}");
        assert!(sent[0].contains("placed one at `2080`"), "{sent:?}");

        // Short from 2000, stop 40 above: target 80 below.
        let ex = resting(vec![order(7, Some(TriggerKind::Stop), Side::Buy, 2_040.0)]);
        ensure_target(&ex, &f, &position(-0.02), &quiet())
            .await
            .unwrap();
        assert_eq!(ex.log(), vec!["trigger TakeProfit BUY 1920"]);
    }

    #[tokio::test]
    async fn the_target_is_the_one_the_entry_would_have_placed() {
        // Same fill and stop as `a_long_gets_a_stop_below_and_a_2r_target_above`,
        // but the target is refused at entry.
        let ex = Fake {
            fill_price: 2_000.0,
            target_error: Some("Binance API error -1001: Internal error".into()),
            ..Default::default()
        };
        let f = eth("ETHUSDC");
        open_position(
            &ex,
            &f,
            false,
            EntryPlan { quantity: 0.025 },
            1_960.0,
            true,
            &quiet(),
            &NO_WAIT,
        )
        .await
        .unwrap();
        let ex = Fake {
            target_error: None,
            ..ex
        };
        ensure_target(&ex, &f, &position(0.025), &quiet())
            .await
            .unwrap();
        assert_eq!(
            ex.log(),
            vec![
                "market BUY 0.025",
                "trigger Stop SELL 1960",
                "trigger TakeProfit SELL 2080",
                "trigger TakeProfit SELL 2080",
            ]
        );
    }

    #[tokio::test]
    async fn an_existing_target_is_never_touched() {
        let f = eth("ETHUSDC");
        let stop = order(7, Some(TriggerKind::Stop), Side::Sell, 1_960.0);
        // The owner's own target, nowhere near 2R.
        let ex = resting(vec![
            stop.clone(),
            order(8, Some(TriggerKind::TakeProfit), Side::Sell, 2_500.0),
        ]);
        ensure_target(&ex, &f, &position(0.02), &quiet())
            .await
            .unwrap();
        assert!(ex.log().is_empty(), "{:?}", ex.log());

        // A type the bot never places, triggering in profit: also a target.
        let ex = resting(vec![stop.clone(), order(9, None, Side::Sell, 2_300.0)]);
        ensure_target(&ex, &f, &position(0.02), &quiet())
            .await
            .unwrap();
        assert!(ex.log().is_empty(), "{:?}", ex.log());

        // A target on the other side belongs to no long: one is placed.
        let ex = resting(vec![
            stop,
            order(8, Some(TriggerKind::TakeProfit), Side::Buy, 1_900.0),
        ]);
        ensure_target(&ex, &f, &position(0.02), &quiet())
            .await
            .unwrap();
        assert_eq!(ex.log(), vec!["trigger TakeProfit SELL 2080"]);
    }

    #[tokio::test]
    async fn a_stop_at_or_past_the_entry_gets_no_guessed_target() {
        let f = eth("BEUSDC");
        let notifier = quiet();
        for stop in [2_000.0, 1_990.0] {
            // A short whose stop the break-even rule (or the owner) moved.
            let ex = resting(vec![order(7, Some(TriggerKind::Stop), Side::Buy, stop)]);
            let short = position_in("BEUSDC", -0.02);
            ensure_target(&ex, &f, &short, &notifier).await.unwrap();
            ensure_target(&ex, &f, &short, &notifier).await.unwrap();
            assert!(ex.log().is_empty(), "{:?}", ex.log());
        }
        // Said once, not on each of the four cycles.
        let sent = notifier.sent();
        assert_eq!(sent.len(), 1, "{sent:?}");
        assert!(sent[0].contains("*BEUSDC* has no target"), "{sent:?}");
        assert!(sent[0].contains("original risk is not known"), "{sent:?}");
    }

    #[tokio::test]
    async fn a_position_without_a_stop_gets_no_target_from_here() {
        let ex = Fake::default();
        ensure_target(&ex, &eth("ETHUSDC"), &position(0.02), &quiet())
            .await
            .unwrap();
        assert!(ex.log().is_empty(), "{:?}", ex.log());
    }

    #[tokio::test]
    async fn a_target_the_price_has_already_passed_is_not_placed() {
        let ex = resting(vec![order(7, Some(TriggerKind::Stop), Side::Sell, 1_960.0)]);
        let long = FuturesPosition {
            mark_price: 2_085.0,
            ..position_in("PASTUSDC", 0.02)
        };
        ensure_target(&ex, &eth("PASTUSDC"), &long, &quiet())
            .await
            .unwrap();
        assert!(ex.log().is_empty(), "{:?}", ex.log());
    }

    #[tokio::test]
    async fn a_refused_target_is_reported_once_and_does_not_stop_the_cycle() {
        let ex = Fake {
            target_error: Some("Binance API error -4130: already exists".into()),
            ..resting(vec![order(7, Some(TriggerKind::Stop), Side::Sell, 1_960.0)])
        };
        let f = eth("REFUSDC");
        let long = position_in("REFUSDC", 0.02);
        let notifier = quiet();
        ensure_target(&ex, &f, &long, &notifier).await.unwrap();
        ensure_target(&ex, &f, &long, &notifier).await.unwrap();
        assert_eq!(ex.log().len(), 2, "tried each cycle: {:?}", ex.log());
        assert_eq!(notifier.sent().len(), 1, "{:?}", notifier.sent());

        // A rate limit is the cycle's to back off from.
        let ex = Fake {
            target_error: Some("Binance API error -1003: Too many requests".into()),
            ..resting(vec![order(7, Some(TriggerKind::Stop), Side::Sell, 1_960.0)])
        };
        let err = ensure_target(&ex, &f, &long, &quiet()).await.unwrap_err();
        assert!(is_rate_limited(&err), "{err}");
    }

    #[tokio::test]
    async fn a_cycle_restores_the_target_before_the_break_even_rule_moves_the_stop() {
        // A short from 2000, stop 2100, no target, and far enough in profit
        // for break-even in this same cycle.
        let ex = Fake::default();
        let short = short_due_for_break_even(&ex);
        ex.positions.lock().unwrap().push(short);
        let params = StrategyParams {
            breakeven_roi_pct: 100.0,
            partial_tp_share: 0.0,
            lock_rsi: 0.0,
            lock_share: 0.0,
            ..StrategyParams::default()
        };
        let mut risk = RiskManager::with_risk_per_trade(100.0, 0.02);
        run_cycle(
            &ex,
            &test_config(),
            20,
            &mut risk,
            &quiet(),
            &params,
            no_wallet(),
        )
        .await
        .unwrap();
        assert_eq!(
            ex.log(),
            vec![
                "klines ETHUSDC",
                "klines ETHUSDC",
                "trigger TakeProfit BUY 1800",
                "cancel 7",
                "trigger Stop BUY 2000",
            ]
        );
    }

    #[tokio::test]
    async fn a_stop_already_at_the_entry_is_left_alone() {
        let ex = Fake::default();
        ex.triggers.lock().unwrap().push(TriggerOrder {
            id: 8,
            kind: Some(TriggerKind::Stop),
            side: Side::Buy,
            trigger_price: 1_999.9,
        });
        let short = FuturesPosition {
            mark_price: 1_900.0,
            ..position(-0.02)
        };
        move_stop_to_entry(&ex, &eth("ETHUSDC"), &short, &quiet(), &NO_WAIT)
            .await
            .unwrap();
        assert!(ex.log().is_empty());
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
        assert_eq!(ex.log(), vec!["leverage 3", "klines ETHUSDC"]);
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

    // ----- profit withdrawal and top-up -----

    #[derive(Default)]
    struct FakeWallet {
        log: Mutex<Vec<String>>,
        spot_usdc: f64,
        transfer_error: Option<String>,
        buy_error: Option<String>,
        min_notional: f64,
        price: f64,
        sell_error: Option<String>,
    }

    impl FakeWallet {
        fn log(&self) -> Vec<String> {
            self.log.lock().unwrap().clone()
        }
    }

    impl WalletExchange for FakeWallet {
        async fn spot_free(&self, _: &str) -> Result<f64, String> {
            Ok(self.spot_usdc)
        }
        async fn universal_transfer(
            &self,
            direction: crate::exchange::TransferDirection,
            asset: &str,
            amount: f64,
        ) -> Result<u64, String> {
            if let Some(e) = &self.transfer_error {
                return Err(e.clone());
            }
            self.log
                .lock()
                .unwrap()
                .push(format!("transfer {} {asset} {amount}", direction.wire()));
            Ok(7)
        }
        async fn spot_price(&self, _: &str) -> Result<f64, String> {
            Ok(self.price)
        }
        async fn sell_base(&self, symbol: &str, quantity: f64) -> Result<OrderOutcome, String> {
            if let Some(e) = &self.sell_error {
                return Err(e.clone());
            }
            self.log
                .lock()
                .unwrap()
                .push(format!("sell {symbol} {quantity}"));
            Ok(OrderOutcome {
                symbol: symbol.into(),
                side: Side::Sell,
                filled_quantity: quantity,
                average_price: self.price,
                fills: Vec::new(),
            })
        }
        async fn spot_filters(&self, symbol: &str) -> Result<SymbolFilters, String> {
            Ok(SymbolFilters {
                min_notional: self.min_notional,
                ..eth(symbol)
            })
        }
        async fn buy_with_quote(&self, symbol: &str, quote: f64) -> Result<OrderOutcome, String> {
            if let Some(e) = &self.buy_error {
                return Err(e.clone());
            }
            self.log
                .lock()
                .unwrap()
                .push(format!("buy {symbol} {quote}"));
            Ok(OrderOutcome {
                symbol: symbol.into(),
                side: Side::Buy,
                filled_quantity: 0.5,
                average_price: 100.0,
                fills: Vec::new(),
            })
        }
    }

    fn no_wallet() -> Option<&'static mut Wallet<FakeWallet>> {
        None
    }

    fn acct(equity: f64, available: f64) -> FuturesAccount {
        FuturesAccount { equity, available }
    }

    fn skim_cfg(buy: &str) -> wallet::WalletConfig {
        wallet::WalletConfig::new(Some(200.0), 100.0, buy, None, 0.0)
    }

    fn topup_cfg() -> wallet::WalletConfig {
        wallet::WalletConfig::new(None, 0.0, "", Some(50.0), 100.0)
    }

    const NOW: u64 = 1_800_000_000_000;

    #[allow(clippy::too_many_arguments)]
    async fn move_money(
        w: &FakeWallet,
        ex: &Fake,
        cfg: &wallet::WalletConfig,
        state: &mut wallet::WalletState,
        account: FuturesAccount,
        positions: &[FuturesPosition],
        risk: &mut RiskManager,
        notifier: &Notifier,
    ) -> Moved {
        move_money_at(w, ex, cfg, state, account, positions, risk, notifier, NOW).await
    }

    #[allow(clippy::too_many_arguments)]
    async fn move_money_at(
        w: &FakeWallet,
        ex: &Fake,
        cfg: &wallet::WalletConfig,
        state: &mut wallet::WalletState,
        account: FuturesAccount,
        positions: &[FuturesPosition],
        risk: &mut RiskManager,
        notifier: &Notifier,
        now_ms: u64,
    ) -> Moved {
        wallet::run(
            w, ex, cfg, state, account, positions, risk, notifier, now_ms,
        )
        .await
    }

    #[tokio::test]
    async fn profit_above_the_threshold_leaves_the_keep_amount() {
        let (w, ex) = (FakeWallet::default(), Fake::default());
        let mut risk = RiskManager::with_risk_per_trade(250.0, 0.02);
        let moved = move_money(
            &w,
            &ex,
            &skim_cfg(""),
            &mut Default::default(),
            acct(250.0, 250.0),
            &[],
            &mut risk,
            &quiet(),
        )
        .await;
        assert_eq!(moved, Moved::Skimmed(150.0));
        assert_eq!(w.log(), ["transfer UMFUTURE_MAIN USDC 150"]);
    }

    #[tokio::test]
    async fn nothing_moves_at_or_below_the_threshold() {
        let (w, ex) = (FakeWallet::default(), Fake::default());
        let mut risk = RiskManager::with_risk_per_trade(200.0, 0.02);
        let moved = move_money(
            &w,
            &ex,
            &skim_cfg(""),
            &mut Default::default(),
            acct(200.0, 200.0),
            &[],
            &mut risk,
            &quiet(),
        )
        .await;
        assert_eq!(moved, Moved::Nothing);
        assert!(w.log().is_empty());
    }

    #[tokio::test]
    async fn nothing_moves_while_a_position_is_open() {
        let (w, ex) = (FakeWallet::default(), Fake::default());
        let mut risk = RiskManager::with_risk_per_trade(500.0, 0.02);
        let moved = move_money(
            &w,
            &ex,
            &skim_cfg(""),
            &mut Default::default(),
            acct(500.0, 300.0),
            &[position(0.1)],
            &mut risk,
            &quiet(),
        )
        .await;
        assert_eq!(moved, Moved::Nothing);
        assert!(w.log().is_empty());
    }

    #[tokio::test]
    async fn nothing_moves_while_orders_still_rest() {
        let w = FakeWallet::default();
        let ex = resting(vec![order(1, Some(TriggerKind::Stop), Side::Sell, 1_900.0)]);
        let mut risk = RiskManager::with_risk_per_trade(500.0, 0.02);
        let moved = move_money(
            &w,
            &ex,
            &skim_cfg(""),
            &mut Default::default(),
            acct(500.0, 500.0),
            &[],
            &mut risk,
            &quiet(),
        )
        .await;
        assert_eq!(moved, Moved::Nothing);
        assert!(w.log().is_empty());
    }

    #[tokio::test]
    async fn the_amount_is_never_more_than_is_available() {
        let (w, ex) = (FakeWallet::default(), Fake::default());
        let mut risk = RiskManager::with_risk_per_trade(500.0, 0.02);
        // Equity is high but only 130 is free: 130 - 100 = 30, not 400.
        let moved = move_money(
            &w,
            &ex,
            &skim_cfg(""),
            &mut Default::default(),
            acct(500.0, 130.0),
            &[],
            &mut risk,
            &quiet(),
        )
        .await;
        assert_eq!(moved, Moved::Skimmed(30.0));
    }

    #[tokio::test]
    async fn small_change_is_not_moved() {
        let (w, ex) = (FakeWallet::default(), Fake::default());
        let mut risk = RiskManager::with_risk_per_trade(250.0, 0.02);
        // Equity 103 over a keep of 99 would move 4 USDC.
        let cfg = wallet::WalletConfig::new(Some(100.0), 100.0 - 1.0, "", None, 0.0);
        let moved = move_money(
            &w,
            &ex,
            &cfg,
            &mut Default::default(),
            acct(103.0, 103.0),
            &[],
            &mut risk,
            &quiet(),
        )
        .await;
        assert_eq!(moved, Moved::Nothing, "4 USDC is under the minimum");
        assert!(w.log().is_empty());
    }

    #[tokio::test]
    async fn a_skim_is_not_a_loss_for_the_daily_drawdown() {
        let (w, ex) = (FakeWallet::default(), Fake::default());
        let mut risk = RiskManager::with_risk_per_trade(250.0, 0.02).with_limits(2, 0.5);
        move_money(
            &w,
            &ex,
            &skim_cfg(""),
            &mut Default::default(),
            acct(250.0, 250.0),
            &[],
            &mut risk,
            &quiet(),
        )
        .await;
        assert_eq!(risk.day_baseline(), 100.0);
        assert_eq!(risk.day_pnl(100.0), 0.0);
        assert!(!risk.check_drawdown(100.0));
    }

    #[tokio::test]
    async fn a_top_up_is_not_a_gain_for_the_daily_pnl() {
        let (w, ex) = (
            FakeWallet {
                spot_usdc: 500.0,
                ..Default::default()
            },
            Fake::default(),
        );
        let mut risk = RiskManager::with_risk_per_trade(30.0, 0.02);
        let moved = move_money(
            &w,
            &ex,
            &topup_cfg(),
            &mut Default::default(),
            acct(30.0, 30.0),
            &[],
            &mut risk,
            &quiet(),
        )
        .await;
        assert_eq!(moved, Moved::ToppedUp(70.0));
        assert_eq!(w.log(), ["transfer MAIN_UMFUTURE USDC 70"]);
        assert_eq!(risk.day_pnl(100.0), 0.0);
    }

    #[tokio::test]
    async fn a_top_up_uses_only_the_usdc_that_is_in_spot() {
        let (w, ex) = (
            FakeWallet {
                spot_usdc: 12.34,
                ..Default::default()
            },
            Fake::default(),
        );
        let mut risk = RiskManager::with_risk_per_trade(30.0, 0.02);
        let moved = move_money(
            &w,
            &ex,
            &topup_cfg(),
            &mut Default::default(),
            acct(30.0, 30.0),
            &[],
            &mut risk,
            &quiet(),
        )
        .await;
        assert_eq!(moved, Moved::ToppedUp(12.34));
        // Only the transfer: no order of any kind to raise cash.
        assert_eq!(w.log(), ["transfer MAIN_UMFUTURE USDC 12.34"]);
    }

    #[tokio::test]
    async fn no_top_up_with_nothing_in_spot_or_above_the_threshold() {
        let ex = Fake::default();
        let mut risk = RiskManager::with_risk_per_trade(30.0, 0.02);
        let empty = FakeWallet::default();
        let none = move_money(
            &empty,
            &ex,
            &topup_cfg(),
            &mut Default::default(),
            acct(30.0, 30.0),
            &[],
            &mut risk,
            &quiet(),
        )
        .await;
        assert_eq!(none, Moved::Nothing);
        let rich = FakeWallet {
            spot_usdc: 500.0,
            ..Default::default()
        };
        let above = move_money(
            &rich,
            &ex,
            &topup_cfg(),
            &mut Default::default(),
            acct(50.0, 50.0),
            &[],
            &mut risk,
            &quiet(),
        )
        .await;
        assert_eq!(above, Moved::Nothing);
        assert!(rich.log().is_empty() && empty.log().is_empty());
    }

    #[tokio::test]
    async fn a_missing_permission_is_said_once_and_switches_the_feature_off_for_the_day() {
        let w = FakeWallet {
            transfer_error: Some(
                "Binance API error -2015: Invalid API-key, IP, or permissions for action.".into(),
            ),
            ..Default::default()
        };
        let ex = Fake::default();
        let mut risk = RiskManager::with_risk_per_trade(250.0, 0.02);
        let notifier = quiet();
        let mut state = wallet::WalletState::default();
        for _ in 0..3 {
            let moved = move_money(
                &w,
                &ex,
                &skim_cfg(""),
                &mut state,
                acct(250.0, 250.0),
                &[],
                &mut risk,
                &notifier,
            )
            .await;
            assert_eq!(moved, Moved::Nothing);
        }
        let said = notifier.sent();
        assert_eq!(said.len(), 1, "{said:?}");
        assert!(said[0].contains("Permit Universal Transfer"), "{said:?}");
        assert_eq!(
            risk.net_cash_flow(),
            0.0,
            "a refused transfer is no cash flow"
        );
    }

    #[tokio::test]
    async fn another_failure_waits_instead_of_retrying_every_cycle() {
        let w = FakeWallet {
            transfer_error: Some("Binance API error -1000: boom".into()),
            ..Default::default()
        };
        let ex = Fake::default();
        let mut risk = RiskManager::with_risk_per_trade(250.0, 0.02);
        let notifier = quiet();
        let mut state = wallet::WalletState::default();
        for _ in 0..3 {
            move_money(
                &w,
                &ex,
                &skim_cfg(""),
                &mut state,
                acct(250.0, 250.0),
                &[],
                &mut risk,
                &notifier,
            )
            .await;
        }
        assert_eq!(notifier.sent().len(), 1, "{:?}", notifier.sent());
    }

    #[tokio::test]
    async fn coins_are_bought_with_the_skimmed_amount_by_weight() {
        let (w, ex) = (
            FakeWallet {
                min_notional: 5.0,
                ..Default::default()
            },
            Fake::default(),
        );
        let mut risk = RiskManager::with_risk_per_trade(250.0, 0.02);
        let notifier = quiet();
        let moved = move_money(
            &w,
            &ex,
            &skim_cfg("BNB:50,ETH:50"),
            &mut Default::default(),
            acct(250.0, 250.0),
            &[],
            &mut risk,
            &notifier,
        )
        .await;
        assert_eq!(moved, Moved::Skimmed(150.0));
        assert_eq!(
            w.log(),
            [
                "transfer UMFUTURE_MAIN USDC 150",
                "buy BNBUSDC 75",
                "buy ETHUSDC 75"
            ]
        );
    }

    #[tokio::test]
    async fn weights_under_100_leave_the_rest_as_usdc() {
        let (w, ex) = (FakeWallet::default(), Fake::default());
        let mut risk = RiskManager::with_risk_per_trade(250.0, 0.02);
        move_money(
            &w,
            &ex,
            &skim_cfg("BNB:20"),
            &mut Default::default(),
            acct(250.0, 250.0),
            &[],
            &mut risk,
            &quiet(),
        )
        .await;
        assert_eq!(
            w.log(),
            ["transfer UMFUTURE_MAIN USDC 150", "buy BNBUSDC 30"]
        );
    }

    #[tokio::test]
    async fn a_share_under_the_spot_minimum_stays_as_usdc() {
        let (w, ex) = (
            FakeWallet {
                min_notional: 100.0,
                ..Default::default()
            },
            Fake::default(),
        );
        let mut risk = RiskManager::with_risk_per_trade(250.0, 0.02);
        let notifier = quiet();
        let moved = move_money(
            &w,
            &ex,
            &skim_cfg("BNB:50,ETH:50"),
            &mut Default::default(),
            acct(250.0, 250.0),
            &[],
            &mut risk,
            &notifier,
        )
        .await;
        // 75 each is under the 100 minimum: no order, the money is in spot.
        assert_eq!(moved, Moved::Skimmed(150.0));
        assert_eq!(w.log(), ["transfer UMFUTURE_MAIN USDC 150"]);
        assert!(
            notifier.sent()[0].contains("under the minimum"),
            "{:?}",
            notifier.sent()
        );
    }

    #[tokio::test]
    async fn a_failed_buy_keeps_the_withdrawal_and_does_not_stop_the_bot() {
        let w = FakeWallet {
            buy_error: Some(
                "Binance API error -2015: Invalid API-key, IP, or permissions for action.".into(),
            ),
            ..Default::default()
        };
        let ex = Fake::default();
        let mut risk = RiskManager::with_risk_per_trade(250.0, 0.02);
        let notifier = quiet();
        let mut state = wallet::WalletState::default();
        let moved = move_money(
            &w,
            &ex,
            &skim_cfg("BNB:50,ETH:50"),
            &mut state,
            acct(250.0, 250.0),
            &[],
            &mut risk,
            &notifier,
        )
        .await;
        assert_eq!(moved, Moved::Skimmed(150.0), "the transfer stands");
        assert_eq!(risk.net_cash_flow(), -150.0);
        let said = notifier.sent().join("\n");
        assert!(said.contains("Enable Spot & Margin Trading"), "{said}");
        assert_eq!(said.matches("Key lacks permission").count(), 1, "{said}");
    }

    /// An EEA credits account: BNB is the collateral, 210.58 of equity, keep 200.
    fn bnb_cfg(to: &str, buy: &str) -> wallet::WalletConfig {
        wallet::WalletConfig::new(Some(205.0), 200.0, buy, None, 0.0).with_skim_asset("bnb", to)
    }

    fn bnb_world() -> (FakeWallet, Fake) {
        (
            FakeWallet {
                price: 600.0,
                min_notional: 5.0,
                ..Default::default()
            },
            Fake {
                held: 0.3584,
                ..Default::default()
            },
        )
    }

    #[tokio::test]
    async fn collateral_over_the_keep_amount_is_transferred_and_sold_for_usdc() {
        let (w, ex) = bnb_world();
        let mut risk = RiskManager::with_risk_per_trade(210.58, 0.02);
        let moved = move_money(
            &w,
            &ex,
            &bnb_cfg("USDC", ""),
            &mut Default::default(),
            acct(210.58, 210.58),
            &[],
            &mut risk,
            &quiet(),
        )
        .await;
        // 10.58 * 0.98 / 600 = 0.01728 BNB; spot step 0.001 sells 0.017.
        assert_eq!(
            w.log(),
            ["transfer UMFUTURE_MAIN BNB 0.01728", "sell BNBUSDC 0.017"]
        );
        assert!(
            matches!(moved, Moved::Skimmed(v) if (v - 10.368).abs() < 1e-3),
            "{moved:?}"
        );
        // Recorded as money out, so it is not a loss.
        assert!((risk.day_baseline() - (210.58 - 10.368)).abs() < 1e-3);
    }

    #[tokio::test]
    async fn the_skim_is_booked_at_what_the_account_actually_lost() {
        let (w, ex) = bnb_world();
        // Collateral counts at a 5 % haircut: the equity fell by 9.85, not 10.37.
        *ex.account_after.lock().unwrap() = Some(acct(210.58 - 9.85, 200.0));
        let mut risk = RiskManager::with_risk_per_trade(210.58, 0.02);
        move_money(
            &w,
            &ex,
            &bnb_cfg("", ""),
            &mut Default::default(),
            acct(210.58, 210.58),
            &[],
            &mut risk,
            &quiet(),
        )
        .await;
        assert!(
            (risk.net_cash_flow() + 9.85).abs() < 1e-9,
            "{}",
            risk.net_cash_flow()
        );
    }

    #[tokio::test]
    async fn without_skim_to_the_collateral_stays_in_spot_unsold() {
        let (w, ex) = bnb_world();
        let mut risk = RiskManager::with_risk_per_trade(210.58, 0.02);
        move_money(
            &w,
            &ex,
            &bnb_cfg("", ""),
            &mut Default::default(),
            acct(210.58, 210.58),
            &[],
            &mut risk,
            &quiet(),
        )
        .await;
        assert_eq!(w.log(), ["transfer UMFUTURE_MAIN BNB 0.01728"]);
    }

    #[tokio::test]
    async fn never_more_collateral_than_the_wallet_holds() {
        let (w, mut ex) = bnb_world();
        ex.held = 0.01;
        let mut risk = RiskManager::with_risk_per_trade(500.0, 0.02);
        move_money(
            &w,
            &ex,
            &bnb_cfg("", ""),
            &mut Default::default(),
            acct(500.0, 500.0),
            &[],
            &mut risk,
            &quiet(),
        )
        .await;
        // 0.01 BNB is 6 USD: over the minimum, and all there is.
        assert_eq!(w.log(), ["transfer UMFUTURE_MAIN BNB 0.01"]);
    }

    #[tokio::test]
    async fn a_collateral_skim_waits_for_open_positions_and_small_excess() {
        let (w, ex) = bnb_world();
        let mut risk = RiskManager::with_risk_per_trade(210.58, 0.02);
        let mut state = wallet::WalletState::default();
        let open = move_money(
            &w,
            &ex,
            &bnb_cfg("USDC", ""),
            &mut state,
            acct(210.58, 150.0),
            &[position(0.1)],
            &mut risk,
            &quiet(),
        )
        .await;
        // Over the threshold, but 4.9 * 0.98 is under the 5 USD minimum.
        let cfg = wallet::WalletConfig::new(Some(201.0), 200.0, "", None, 0.0)
            .with_skim_asset("BNB", "USDC");
        let small = move_money(
            &w,
            &ex,
            &cfg,
            &mut state,
            acct(204.9, 204.9),
            &[],
            &mut risk,
            &quiet(),
        )
        .await;
        assert_eq!((open, small), (Moved::Nothing, Moved::Nothing));
        assert!(w.log().is_empty());
    }

    #[tokio::test]
    async fn a_sale_under_the_spot_minimum_leaves_the_collateral_in_spot() {
        let (mut w, ex) = bnb_world();
        w.min_notional = 50.0;
        let mut risk = RiskManager::with_risk_per_trade(210.58, 0.02);
        let notifier = quiet();
        let moved = move_money(
            &w,
            &ex,
            &bnb_cfg("USDC", ""),
            &mut Default::default(),
            acct(210.58, 210.58),
            &[],
            &mut risk,
            &notifier,
        )
        .await;
        assert!(matches!(moved, Moved::Skimmed(_)));
        assert_eq!(w.log(), ["transfer UMFUTURE_MAIN BNB 0.01728"]);
        assert!(
            notifier.sent()[0].contains("stays in spot"),
            "{:?}",
            notifier.sent()
        );
    }

    #[tokio::test]
    async fn a_failed_sale_keeps_the_transfer_and_does_not_stop_the_bot() {
        let (mut w, ex) = bnb_world();
        w.sell_error =
            Some("Binance API error -2015: Invalid API-key, IP, or permissions for action.".into());
        let mut risk = RiskManager::with_risk_per_trade(210.58, 0.02);
        let notifier = quiet();
        let moved = move_money(
            &w,
            &ex,
            &bnb_cfg("USDC", ""),
            &mut Default::default(),
            acct(210.58, 210.58),
            &[],
            &mut risk,
            &notifier,
        )
        .await;
        assert!(matches!(moved, Moved::Skimmed(_)));
        assert!(risk.net_cash_flow() < 0.0);
        assert!(notifier
            .sent()
            .join("\n")
            .contains("Enable Spot & Margin Trading"));
    }

    #[tokio::test]
    async fn coins_are_bought_with_what_the_sale_gave() {
        let (w, ex) = bnb_world();
        let mut risk = RiskManager::with_risk_per_trade(210.58, 0.02);
        move_money(
            &w,
            &ex,
            &bnb_cfg("USDC", "ETH:50"),
            &mut Default::default(),
            acct(210.58, 210.58),
            &[],
            &mut risk,
            &quiet(),
        )
        .await;
        // 0.017 BNB at 600 = 10.2 USDC; half is 5.1.
        assert_eq!(
            w.log(),
            [
                "transfer UMFUTURE_MAIN BNB 0.01728",
                "sell BNBUSDC 0.017",
                "buy ETHUSDC 5.1"
            ]
        );
    }

    #[tokio::test]
    async fn a_second_move_waits_out_the_cooldown() {
        let (w, ex) = bnb_world();
        let mut risk = RiskManager::with_risk_per_trade(210.58, 0.02);
        let mut state = wallet::WalletState::default();
        let cfg = bnb_cfg("", "");
        let first = move_money(
            &w,
            &ex,
            &cfg,
            &mut state,
            acct(210.58, 210.58),
            &[],
            &mut risk,
            &quiet(),
        )
        .await;
        // The balance has not updated yet: it still reads 210.58.
        let soon = move_money_at(
            &w,
            &ex,
            &cfg,
            &mut state,
            acct(210.58, 210.58),
            &[],
            &mut risk,
            &quiet(),
            NOW + 5 * 60_000,
        )
        .await;
        let later = move_money_at(
            &w,
            &ex,
            &cfg,
            &mut state,
            acct(210.58, 210.58),
            &[],
            &mut risk,
            &quiet(),
            NOW + 61 * 60_000,
        )
        .await;
        assert!(matches!(first, Moved::Skimmed(_)));
        assert_eq!(soon, Moved::Nothing);
        assert!(matches!(later, Moved::Skimmed(_)));
        assert_eq!(w.log().len(), 2, "{:?}", w.log());
    }

    #[tokio::test]
    async fn a_broken_reading_never_becomes_a_transfer() {
        let (w, ex) = bnb_world();
        let mut risk = RiskManager::with_risk_per_trade(210.58, 0.02);
        for account in [
            acct(210.58, f64::NAN),
            acct(f64::NAN, 210.58),
            acct(f64::INFINITY, f64::INFINITY),
            acct(210.58, -1.0),
        ] {
            let moved = move_money(
                &w,
                &ex,
                &bnb_cfg("USDC", ""),
                &mut Default::default(),
                account,
                &[],
                &mut risk,
                &quiet(),
            )
            .await;
            assert_eq!(moved, Moved::Nothing, "{account:?}");
        }
        let top = FakeWallet {
            spot_usdc: f64::NAN,
            ..Default::default()
        };
        let moved = move_money(
            &top,
            &ex,
            &topup_cfg(),
            &mut Default::default(),
            acct(30.0, 30.0),
            &[],
            &mut risk,
            &quiet(),
        )
        .await;
        assert_eq!(moved, Moved::Nothing);
        for price in [0.0, f64::NAN, -5.0] {
            let w = FakeWallet {
                price,
                ..bnb_world().0
            };
            let moved = move_money(
                &w,
                &ex,
                &bnb_cfg("USDC", ""),
                &mut Default::default(),
                acct(210.58, 210.58),
                &[],
                &mut risk,
                &quiet(),
            )
            .await;
            assert_eq!(moved, Moved::Nothing, "price {price}");
        }
        let no_bnb = Fake {
            held: f64::NAN,
            ..Default::default()
        };
        let moved = move_money(
            &bnb_world().0,
            &no_bnb,
            &bnb_cfg("USDC", ""),
            &mut Default::default(),
            acct(210.58, 210.58),
            &[],
            &mut risk,
            &quiet(),
        )
        .await;
        assert_eq!(moved, Moved::Nothing);
        assert!(w_log_empty(&w));
    }

    fn w_log_empty(w: &FakeWallet) -> bool {
        w.log().is_empty()
    }

    #[tokio::test]
    async fn a_lost_reply_is_confirmed_by_the_account_and_counted() {
        let (mut w, ex) = bnb_world();
        w.transfer_error = Some("HTTP error: operation timed out".into());
        // The transfer went through: equity fell by 9.9.
        *ex.account_after.lock().unwrap() = Some(acct(210.58 - 9.9, 200.0));
        let mut risk = RiskManager::with_risk_per_trade(210.58, 0.02);
        let moved = move_money(
            &w,
            &ex,
            &bnb_cfg("USDC", ""),
            &mut Default::default(),
            acct(210.58, 210.58),
            &[],
            &mut risk,
            &quiet(),
        )
        .await;
        assert!(matches!(moved, Moved::Skimmed(_)), "{moved:?}");
        assert!((risk.net_cash_flow() + 9.9).abs() < 1e-9);
        assert_eq!(w.log(), ["sell BNBUSDC 0.017"], "and the sale goes on");
    }

    #[tokio::test]
    async fn a_lost_reply_with_no_fall_in_equity_is_a_failure() {
        let (mut w, ex) = bnb_world();
        w.transfer_error = Some("HTTP error: operation timed out".into());
        let mut risk = RiskManager::with_risk_per_trade(210.58, 0.02);
        let notifier = quiet();
        let moved = move_money(
            &w,
            &ex,
            &bnb_cfg("USDC", ""),
            &mut Default::default(),
            acct(210.58, 210.58),
            &[],
            &mut risk,
            &notifier,
        )
        .await;
        // The fake account still reads 100, not near 210.58 - 10: no match.
        assert_eq!(moved, Moved::Nothing);
        assert_eq!(risk.net_cash_flow(), 0.0);
        assert_eq!(notifier.sent().len(), 1);
    }

    #[tokio::test]
    async fn a_lost_top_up_reply_is_confirmed_by_the_account_too() {
        let (mut w, ex) = (
            FakeWallet {
                spot_usdc: 500.0,
                ..Default::default()
            },
            Fake::default(),
        );
        w.transfer_error = Some("JSON parse error: eof".into());
        // 30 -> topped up 70: the account now reads 100.
        let mut risk = RiskManager::with_risk_per_trade(30.0, 0.02);
        let moved = move_money(
            &w,
            &ex,
            &topup_cfg(),
            &mut Default::default(),
            acct(30.0, 30.0),
            &[],
            &mut risk,
            &quiet(),
        )
        .await;
        assert_eq!(moved, Moved::ToppedUp(70.0));
        assert_eq!(risk.net_cash_flow(), 70.0);
    }

    fn temp_state(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("tempest-wallet-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("wallet-state.json")
    }

    #[tokio::test]
    async fn unsold_collateral_is_remembered_across_a_restart_and_sold_later() {
        let path = temp_state("pending");
        let (mut w, ex) = bnb_world();
        w.sell_error = Some("Binance API error -1000: boom".into());
        let mut risk = RiskManager::with_risk_per_trade(210.58, 0.02);
        let notifier = quiet();
        let mut state = wallet::WalletState::with_file(path.clone());
        let cfg = bnb_cfg("USDC", "");
        let moved = move_money(
            &w,
            &ex,
            &cfg,
            &mut state,
            acct(210.58, 210.58),
            &[],
            &mut risk,
            &notifier,
        )
        .await;
        assert!(matches!(moved, Moved::Skimmed(_)));
        assert_eq!(
            w.log(),
            ["transfer UMFUTURE_MAIN BNB 0.01728"],
            "the sale failed"
        );

        // Restart: a new state reads the file. The sale now works.
        let (w2, ex2) = bnb_world();
        let mut state = wallet::WalletState::with_file(path.clone());
        let moved = move_money(
            &w2,
            &ex2,
            &cfg,
            &mut state,
            acct(200.2, 200.2),
            &[],
            &mut risk,
            &notifier,
        )
        .await;
        assert_eq!(moved, Moved::Nothing, "nothing new to move");
        // Only what the bot moved: 0.01728 rounded to the spot step.
        assert_eq!(w2.log(), ["sell BNBUSDC 0.017"]);
        assert!(
            notifier
                .sent()
                .iter()
                .any(|m| m.contains("Earlier unsold BNB sold")),
            "{:?}",
            notifier.sent()
        );

        // Sold: it stays sold after another restart.
        let (w3, ex3) = bnb_world();
        let mut state = wallet::WalletState::with_file(path);
        move_money(
            &w3,
            &ex3,
            &cfg,
            &mut state,
            acct(200.2, 200.2),
            &[],
            &mut risk,
            &notifier,
        )
        .await;
        assert!(w3.log().is_empty());
    }

    #[tokio::test]
    async fn something_that_stands_unsold_is_told_once() {
        let (mut w, ex) = bnb_world();
        w.min_notional = 50.0; // the sale is under the minimum order
        let mut risk = RiskManager::with_risk_per_trade(210.58, 0.02);
        let notifier = quiet();
        let mut state = wallet::WalletState::default();
        let cfg = bnb_cfg("USDC", "");
        move_money(
            &w,
            &ex,
            &cfg,
            &mut state,
            acct(210.58, 210.58),
            &[],
            &mut risk,
            &notifier,
        )
        .await;
        let after_skim = notifier.sent().len();
        for hours in [2, 4, 6] {
            move_money_at(
                &w,
                &ex,
                &cfg,
                &mut state,
                acct(200.2, 200.2),
                &[],
                &mut risk,
                &notifier,
                NOW + hours * 3_600_000,
            )
            .await;
        }
        assert_eq!(
            notifier.sent().len(),
            after_skim,
            "no new message: it was already told"
        );
        assert_eq!(w.log(), ["transfer UMFUTURE_MAIN BNB 0.01728"]);
    }

    #[tokio::test]
    async fn a_manual_order_on_another_pair_blocks_the_withdrawal() {
        let (w, mut ex) = bnb_world();
        ex.other_orders = 1;
        let mut risk = RiskManager::with_risk_per_trade(210.58, 0.02);
        let moved = move_money(
            &w,
            &ex,
            &bnb_cfg("USDC", ""),
            &mut Default::default(),
            acct(210.58, 210.58),
            &[],
            &mut risk,
            &quiet(),
        )
        .await;
        assert_eq!(moved, Moved::Nothing);
        assert!(w.log().is_empty());
    }

    #[tokio::test]
    async fn the_message_says_what_happened() {
        let (w, ex) = bnb_world();
        let mut risk = RiskManager::with_risk_per_trade(210.58, 0.02);
        let notifier = quiet();
        move_money(
            &w,
            &ex,
            &bnb_cfg("USDC", ""),
            &mut Default::default(),
            acct(210.58, 210.58),
            &[],
            &mut risk,
            &notifier,
        )
        .await;
        let msg = notifier.sent().join("\n");
        assert!(
            msg.contains(
                "Moved `0.01728` BNB (about `10.4` USD) above the `200` USD stake to spot"
            ),
            "{msg}"
        );
        assert!(msg.contains("sold `0.017` for about `10.20` USDC"), "{msg}");
        assert!(!msg.to_lowercase().contains("profit"), "{msg}");
    }

    #[test]
    fn skim_asset_and_skim_to_are_checked() {
        let base = || wallet::WalletConfig::new(Some(205.0), 200.0, "ETH:50", None, 0.0);
        assert!(
            !base().with_skim_asset("DOGE", "").is_on(),
            "not transferable"
        );
        let c = base().with_skim_asset("", "");
        assert_eq!((c.skim_asset.as_str(), c.skim_to_usdc), ("USDC", false));
        // SKIM_BUY needs USDC: dropped for BNB unless SKIM_TO=USDC.
        assert!(base().with_skim_asset("BNB", "").skim_buy.is_empty());
        assert_eq!(base().with_skim_asset("BNB", "USDC").skim_buy.len(), 1);
        // SKIM_TO is moot for USDC itself.
        assert!(!base().with_skim_asset("USDC", "USDC").skim_to_usdc);
    }

    #[test]
    fn settings_parse_and_contradictions_switch_off() {
        assert_eq!(
            wallet::parse_skim_buy("BNB:50, eth:25.5,bad,X:0"),
            vec![("BNB".to_string(), 50.0), ("ETH".to_string(), 25.5)]
        );
        // Keep must be below the threshold.
        assert!(!wallet::WalletConfig::new(Some(100.0), 100.0, "", None, 0.0).is_on());
        // Top-up target must be above its threshold.
        assert!(!wallet::WalletConfig::new(None, 0.0, "", Some(50.0), 50.0).is_on());
        // A top-up that lands above the skim line would ping-pong.
        assert!(!wallet::WalletConfig::new(Some(200.0), 100.0, "", Some(50.0), 250.0).is_on());
        // Off unless set.
        assert!(!wallet::WalletConfig::new(None, 0.0, "", None, 0.0).is_on());
    }

    // ----- half out -----

    fn half_cfg(stake: &str, step: &str, share: &str) -> wallet::WalletConfig {
        wallet::WalletConfig::new(None, 0.0, "", None, 0.0).with_half_out(stake, step, share)
    }

    fn half() -> wallet::WalletConfig {
        half_cfg("210", "150", "50")
    }

    const LATER: u64 = NOW + 2 * 3_600_000;

    /// Run one half-out cycle on a flat account with this equity.
    async fn half_cycle(
        cfg: &wallet::WalletConfig,
        state: &mut wallet::WalletState,
        equity: f64,
        now_ms: u64,
    ) -> (Moved, FakeWallet) {
        let (w, ex) = (FakeWallet::default(), Fake::default());
        let mut risk = RiskManager::with_risk_per_trade(equity, 0.02);
        let moved = move_money_at(
            &w,
            &ex,
            cfg,
            state,
            acct(equity, equity),
            &[],
            &mut risk,
            &quiet(),
            now_ms,
        )
        .await;
        (moved, w)
    }

    fn base_of(cfg: &wallet::WalletConfig, state: &mut wallet::WalletState) -> f64 {
        state.base(&cfg.half_out.unwrap())
    }

    #[tokio::test]
    async fn half_out_waits_until_equity_is_stake_plus_step() {
        let cfg = half();
        let mut state = wallet::WalletState::default();
        let (moved, w) = half_cycle(&cfg, &mut state, 359.99, NOW).await;
        assert_eq!(moved, Moved::Nothing);
        assert!(w.log().is_empty());
        let (moved, w) = half_cycle(&cfg, &mut state, 360.0, NOW).await;
        assert_eq!(moved, Moved::Skimmed(75.0));
        assert_eq!(w.log(), ["transfer UMFUTURE_MAIN USDC 75"]);
        let mut fresh = wallet::WalletState::default();
        let (moved, _) = half_cycle(&cfg, &mut fresh, 360.01, NOW).await;
        assert_eq!(moved, Moved::Skimmed(75.0), "75.005 floors to whole cents");
    }

    #[tokio::test]
    async fn half_out_example_takes_76_and_raises_the_base_to_286() {
        let cfg = half();
        let mut state = wallet::WalletState::default();
        let (w, ex) = (FakeWallet::default(), Fake::default());
        let notifier = quiet();
        let mut risk = RiskManager::with_risk_per_trade(362.0, 0.02);
        let moved = move_money_at(
            &w,
            &ex,
            &cfg,
            &mut state,
            acct(362.0, 362.0),
            &[],
            &mut risk,
            &notifier,
            NOW,
        )
        .await;
        assert_eq!(moved, Moved::Skimmed(76.0));
        assert_eq!(w.log(), ["transfer UMFUTURE_MAIN USDC 76"]);
        assert_eq!(base_of(&cfg, &mut state), 286.0);
        assert_eq!(
            notifier.sent(),
            [
                "\u{1f4b0} Took out `76.00` USDC (50 % of `152.00` earned above the `210.00` \
                 stake) to spot; stake is now `286.00`, next withdrawal at `436.00`"
            ]
        );
        // The next one is at 436, not before.
        let (moved, _) = half_cycle(&cfg, &mut state, 435.99, LATER).await;
        assert_eq!(moved, Moved::Nothing);
        let (moved, w) = half_cycle(&cfg, &mut state, 436.0, LATER).await;
        assert_eq!(moved, Moved::Skimmed(75.0));
        assert_eq!(w.log(), ["transfer UMFUTURE_MAIN USDC 75"]);
        assert_eq!(base_of(&cfg, &mut state), 361.0);
    }

    #[tokio::test]
    async fn half_out_is_not_a_loss_for_the_daily_drawdown() {
        let cfg = half();
        let (w, ex) = (FakeWallet::default(), Fake::default());
        let mut risk = RiskManager::with_risk_per_trade(362.0, 0.02).with_limits(2, 0.5);
        move_money(
            &w,
            &ex,
            &cfg,
            &mut Default::default(),
            acct(362.0, 362.0),
            &[],
            &mut risk,
            &quiet(),
        )
        .await;
        assert_eq!(risk.day_baseline(), 286.0);
        assert_eq!(risk.day_pnl(286.0), 0.0);
        assert!(!risk.check_drawdown(286.0));
    }

    #[tokio::test]
    async fn the_base_survives_a_restart_and_a_new_stake_starts_over() {
        let path = temp_state("half-base");
        let cfg = half();
        let mut state = wallet::WalletState::with_file(path.clone());
        let (moved, _) = half_cycle(&cfg, &mut state, 362.0, NOW).await;
        assert_eq!(moved, Moved::Skimmed(76.0));

        // Restart: same settings, same base.
        let mut state = wallet::WalletState::with_file(path.clone());
        assert_eq!(base_of(&cfg, &mut state), 286.0);
        let (moved, _) = half_cycle(&cfg, &mut state, 400.0, LATER).await;
        assert_eq!(moved, Moved::Nothing);

        // Only step and share changed: the base stays.
        let other = half_cfg("210", "100", "25");
        let mut state = wallet::WalletState::with_file(path.clone());
        assert_eq!(base_of(&other, &mut state), 286.0);

        // A new stake starts the base over from it.
        let bigger = half_cfg("250", "150", "50");
        let mut state = wallet::WalletState::with_file(path.clone());
        assert_eq!(base_of(&bigger, &mut state), 250.0);
        let (moved, _) = half_cycle(&bigger, &mut state, 400.0, LATER).await;
        assert_eq!(moved, Moved::Skimmed(75.0));
        let mut state = wallet::WalletState::with_file(path.clone());
        assert_eq!(base_of(&bigger, &mut state), 325.0);

        // Back to the old stake: it is 210 again, not the stale 286.
        let mut state = wallet::WalletState::with_file(path);
        assert_eq!(base_of(&cfg, &mut state), 210.0);
    }

    #[tokio::test]
    async fn a_missing_or_broken_file_starts_from_the_stake() {
        let path = temp_state("half-broken");
        let cfg = half();
        let mut state = wallet::WalletState::with_file(path.clone());
        assert_eq!(base_of(&cfg, &mut state), 210.0, "no file");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"{ not json").unwrap();
        let mut state = wallet::WalletState::with_file(path.clone());
        assert_eq!(base_of(&cfg, &mut state), 210.0, "corrupt file");
        std::fs::write(
            &path,
            br#"{"pending":[],"half_out":{"base":-5.0,"stake":210.0,"step":150.0,"share_pct":50.0}}"#,
        )
        .unwrap();
        let mut state = wallet::WalletState::with_file(path.clone());
        assert_eq!(base_of(&cfg, &mut state), 210.0, "nonsense base");
        // A file from before this feature has no base.
        std::fs::write(&path, br#"{"pending":[]}"#).unwrap();
        let mut state = wallet::WalletState::with_file(path);
        assert_eq!(base_of(&cfg, &mut state), 210.0, "old file");
    }

    #[tokio::test]
    async fn losses_never_lower_the_base() {
        let cfg = half();
        let mut state = wallet::WalletState::default();
        half_cycle(&cfg, &mut state, 362.0, NOW).await;
        assert_eq!(base_of(&cfg, &mut state), 286.0);
        // The account falls to 150: the next withdrawal is still at 436.
        let (moved, _) = half_cycle(&cfg, &mut state, 150.0, LATER).await;
        assert_eq!(moved, Moved::Nothing);
        assert_eq!(base_of(&cfg, &mut state), 286.0);
        let (moved, _) = half_cycle(&cfg, &mut state, 360.0, LATER).await;
        assert_eq!(moved, Moved::Nothing);
        let (moved, _) = half_cycle(&cfg, &mut state, 436.0, LATER).await;
        assert!(matches!(moved, Moved::Skimmed(_)));
    }

    #[tokio::test]
    async fn half_out_waits_for_a_flat_account_and_the_cooldown() {
        let cfg = half();
        let mut risk = RiskManager::with_risk_per_trade(400.0, 0.02);
        let (w, ex) = (FakeWallet::default(), Fake::default());
        let moved = move_money(
            &w,
            &ex,
            &cfg,
            &mut Default::default(),
            acct(400.0, 300.0),
            &[position(0.1)],
            &mut risk,
            &quiet(),
        )
        .await;
        assert_eq!(moved, Moved::Nothing, "open position");
        let ex = resting(vec![order(1, Some(TriggerKind::Stop), Side::Sell, 1_900.0)]);
        let moved = move_money(
            &w,
            &ex,
            &cfg,
            &mut Default::default(),
            acct(400.0, 400.0),
            &[],
            &mut risk,
            &quiet(),
        )
        .await;
        assert_eq!(moved, Moved::Nothing, "resting order");
        let ex = Fake {
            other_orders: 1,
            ..Default::default()
        };
        let moved = move_money(
            &w,
            &ex,
            &cfg,
            &mut Default::default(),
            acct(400.0, 400.0),
            &[],
            &mut risk,
            &quiet(),
        )
        .await;
        assert_eq!(moved, Moved::Nothing, "regular order");
        assert!(w.log().is_empty());

        // Right after a move nothing more goes, even above the new line.
        let mut state = wallet::WalletState::default();
        half_cycle(&cfg, &mut state, 362.0, NOW).await;
        let (moved, _) = half_cycle(&cfg, &mut state, 500.0, NOW + 60_000).await;
        assert_eq!(moved, Moved::Nothing, "cooldown");
    }

    #[tokio::test]
    async fn half_out_never_takes_more_than_is_available() {
        let cfg = half();
        let (w, ex) = (FakeWallet::default(), Fake::default());
        let mut risk = RiskManager::with_risk_per_trade(362.0, 0.02);
        let mut state = wallet::WalletState::default();
        let moved = move_money(
            &w,
            &ex,
            &cfg,
            &mut state,
            acct(362.0, 30.0),
            &[],
            &mut risk,
            &quiet(),
        )
        .await;
        assert_eq!(moved, Moved::Skimmed(30.0));
        assert_eq!(base_of(&cfg, &mut state), 332.0, "what stays is the base");
    }

    #[tokio::test]
    async fn share_100_takes_all_the_gain_and_share_1_a_sliver() {
        let all = half_cfg("210", "150", "100");
        let mut state = wallet::WalletState::default();
        let (moved, w) = half_cycle(&all, &mut state, 362.0, NOW).await;
        assert_eq!(moved, Moved::Skimmed(152.0));
        assert_eq!(w.log(), ["transfer UMFUTURE_MAIN USDC 152"]);
        assert_eq!(base_of(&all, &mut state), 210.0, "never below the base");

        let one = half_cfg("210", "150", "1");
        let mut state = wallet::WalletState::default();
        let (moved, _) = half_cycle(&one, &mut state, 362.0, NOW).await;
        assert_eq!(moved, Moved::Nothing, "1.52 is under the minimum move");
        assert_eq!(base_of(&one, &mut state), 210.0);
        let (moved, w) = half_cycle(&one, &mut state, 800.0, NOW).await;
        assert_eq!(moved, Moved::Skimmed(5.9));
        assert_eq!(w.log(), ["transfer UMFUTURE_MAIN USDC 5.9"]);
        assert_eq!(base_of(&one, &mut state), 794.1);
    }

    #[test]
    fn half_out_settings_are_checked() {
        assert!(half().is_on());
        assert_eq!(
            half().half_out,
            Some(wallet::HalfOut {
                stake: 210.0,
                step: 150.0,
                share_pct: 50.0
            })
        );
        // Blank share is 50; nothing set is off.
        assert_eq!(half_cfg("210", "150", "").half_out.unwrap().share_pct, 50.0);
        assert!(!half_cfg("", "", "").is_on());
        for bad in ["0", "-1", "-0.5", "NaN", "inf", "abc"] {
            assert!(!half_cfg(bad, "150", "50").is_on(), "stake {bad}");
            assert!(!half_cfg("210", bad, "50").is_on(), "step {bad}");
        }
        for bad in ["0", "0.5", "-10", "101", "100.1", "NaN", "inf", "x"] {
            assert!(!half_cfg("210", "150", bad).is_on(), "share {bad}");
        }
        assert!(half_cfg("210", "150", "1").is_on());
        assert!(half_cfg("210", "150", "100").is_on());
        // Stake or step alone is not a rule.
        assert!(!half_cfg("210", "", "50").is_on());
        assert!(!half_cfg("", "150", "50").is_on());
        // A bad asset switches it off too.
        assert!(!half().with_skim_asset("DOGE", "").is_on());
    }

    #[test]
    fn half_out_clashing_with_above_or_keep_switches_the_skim_off() {
        let above = wallet::WalletConfig::new(Some(300.0), 0.0, "", None, 0.0)
            .with_half_out("210", "150", "50");
        assert!(!above.is_on());
        assert!(above.half_out.is_none() && above.skim_above.is_none());
        let keep =
            wallet::WalletConfig::new(None, 100.0, "", None, 0.0).with_half_out("210", "150", "50");
        assert!(!keep.is_on());
        // Top-up is not part of the clash and stays as set.
        let with_topup = wallet::WalletConfig::new(Some(300.0), 60.0, "", Some(50.0), 100.0)
            .with_half_out("210", "150", "50");
        assert!(with_topup.is_on());
        assert!(with_topup.half_out.is_none() && with_topup.skim_above.is_none());
        assert_eq!(with_topup.topup_below, Some(50.0));
    }

    #[tokio::test]
    async fn a_lost_reply_that_went_through_raises_the_base_from_the_account() {
        let cfg = half();
        let (mut w, ex) = (FakeWallet::default(), Fake::default());
        w.transfer_error = Some("HTTP error: operation timed out".into());
        // The transfer landed: equity fell by 74 (not quite 76).
        *ex.account_after.lock().unwrap() = Some(acct(288.0, 288.0));
        let mut risk = RiskManager::with_risk_per_trade(362.0, 0.02);
        let mut state = wallet::WalletState::default();
        let moved = move_money(
            &w,
            &ex,
            &cfg,
            &mut state,
            acct(362.0, 362.0),
            &[],
            &mut risk,
            &quiet(),
        )
        .await;
        assert_eq!(moved, Moved::Skimmed(76.0));
        assert_eq!(base_of(&cfg, &mut state), 288.0, "what the account holds");
        assert!((risk.net_cash_flow() + 74.0).abs() < 1e-9);
    }

    #[tokio::test]
    async fn a_lost_reply_that_did_not_go_through_leaves_the_base() {
        let cfg = half();
        let (mut w, ex) = (FakeWallet::default(), Fake::default());
        w.transfer_error = Some("HTTP error: operation timed out".into());
        // The account still reads the old equity: nothing left.
        *ex.account_after.lock().unwrap() = Some(acct(362.0, 362.0));
        let mut risk = RiskManager::with_risk_per_trade(362.0, 0.02);
        let mut state = wallet::WalletState::default();
        let notifier = quiet();
        let moved = move_money(
            &w,
            &ex,
            &cfg,
            &mut state,
            acct(362.0, 362.0),
            &[],
            &mut risk,
            &notifier,
        )
        .await;
        assert_eq!(moved, Moved::Nothing);
        assert_eq!(base_of(&cfg, &mut state), 210.0);
        assert_eq!(risk.net_cash_flow(), 0.0);
        // A refusal from Binance is definite too.
        w.transfer_error = Some("Binance API error -1000: boom".into());
        let moved = move_money_at(
            &w,
            &ex,
            &cfg,
            &mut state,
            acct(362.0, 362.0),
            &[],
            &mut risk,
            &notifier,
            LATER,
        )
        .await;
        assert_eq!(moved, Moved::Nothing);
        assert_eq!(base_of(&cfg, &mut state), 210.0);
    }

    #[tokio::test]
    async fn a_misreading_after_the_transfer_falls_back_to_the_nominal_amount() {
        // The fake account reads 100 afterwards: far from 286, so the nominal
        // 76 is used and the base is 362 - 76.
        let cfg = half();
        let mut state = wallet::WalletState::default();
        half_cycle(&cfg, &mut state, 362.0, NOW).await;
        assert_eq!(base_of(&cfg, &mut state), 286.0);
    }

    #[tokio::test]
    async fn half_out_in_bnb_takes_the_share_of_the_gain() {
        let cfg = half_cfg("200", "5", "50").with_skim_asset("bnb", "USDC");
        let (w, ex) = bnb_world();
        let mut risk = RiskManager::with_risk_per_trade(210.58, 0.02);
        let notifier = quiet();
        let mut state = wallet::WalletState::default();
        let moved = move_money(
            &w,
            &ex,
            &cfg,
            &mut state,
            acct(210.58, 210.58),
            &[],
            &mut risk,
            &notifier,
        )
        .await;
        // Half of 10.58 is 5.29; 98 % of it at the fake price.
        assert!(
            matches!(moved, Moved::Skimmed(v) if v < 5.29 && v > 5.0),
            "{moved:?}"
        );
        let msg = notifier.sent().join("\n");
        assert!(
            msg.contains("BNB") && msg.contains("earned above the `200.00` stake"),
            "{msg}"
        );
        assert!(base_of(&cfg, &mut state) > 204.0);
    }

    #[test]
    fn the_startup_line_names_base_and_next_level() {
        let cfg = half();
        let mut state = wallet::WalletState::default();
        let line = state.half_out_line(&cfg).unwrap();
        assert!(
            line.contains("210.00") && line.contains("360.00") && line.contains("50 %"),
            "{line}"
        );
        assert!(wallet::WalletState::default()
            .half_out_line(&skim_cfg(""))
            .is_none());
    }

    #[tokio::test]
    async fn a_cycle_that_moves_money_ends_before_any_entry() {
        let ex = Fake::default();
        let config = test_config();
        let mut risk = RiskManager::with_risk_per_trade(100.0, 0.02);
        let mut w = Wallet {
            client: FakeWallet::default(),
            cfg: wallet::WalletConfig::new(Some(50.0), 20.0, "", None, 0.0),
            state: Default::default(),
        };
        run_cycle(
            &ex,
            &config,
            20,
            &mut risk,
            &quiet(),
            &StrategyParams::default(),
            Some(&mut w),
        )
        .await
        .unwrap();
        assert_eq!(w.client.log(), ["transfer UMFUTURE_MAIN USDC 80"]);
        assert!(
            ex.log().is_empty(),
            "no entry work after a move: {:?}",
            ex.log()
        );
        assert_eq!(risk.day_baseline(), 20.0);
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
