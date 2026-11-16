//! Profit withdrawal and top-up of the futures wallet.
//!
//! Futures margin sits at high leverage, so profit left there is exposed
//! again. With `SKIM_ABOVE_USDC` set, a flat account above that equity moves
//! everything over `SKIM_KEEP_USDC` to the spot wallet; `SKIM_BUY` optionally
//! turns it into coins there. With `TOPUP_BELOW_USDC` set, a futures account
//! under that equity is filled from spot USDC that is already there (never by
//! selling anything). All of it is off until the owner sets the numbers.
//!
//! A failed or refused move never stops the bot: it is logged, the owner is
//! told, and the next try waits. A key without the needed permission switches
//! the feature off until the next UTC day, with one message.

use crate::exchange::{
    FuturesAccount, FuturesExchange, FuturesPosition, TransferDirection, WalletExchange,
};
use crate::notify::Notifier;
use crate::risk::RiskManager;
use tracing::{error, info, warn};

/// The margin and spot asset moved.
pub const ASSET: &str = "USDC";
/// Smallest amount worth moving: no pocket change every cycle.
pub const MIN_MOVE: f64 = 5.0;
const DAY_MS: u64 = 86_400_000;
/// Wait after a rate limit, and after any other failure.
const RATE_LIMIT_WAIT_MS: u64 = 5 * 60_000;
const FAILURE_WAIT_MS: u64 = 60 * 60_000;

const PERMIT_TRANSFER: &str = "Permit Universal Transfer";
const PERMIT_SPOT: &str = "Enable Spot & Margin Trading";

#[derive(Debug, Clone, PartialEq)]
pub struct WalletConfig {
    /// Skim when futures equity is above this; `None` is off.
    pub skim_above: Option<f64>,
    /// What stays in futures after a skim.
    pub skim_keep: f64,
    /// Coins (base asset, weight in percent) bought with a skim.
    pub skim_buy: Vec<(String, f64)>,
    /// Top up when futures equity is below this; `None` is off.
    pub topup_below: Option<f64>,
    /// Equity to fill up to.
    pub topup_to: f64,
}

impl WalletConfig {
    pub fn from_env() -> Self {
        let num = |key: &str| {
            std::env::var(key)
                .ok()
                .and_then(|v| v.trim().parse::<f64>().ok())
                .filter(|v| v.is_finite() && *v > 0.0)
        };
        let buy = std::env::var("SKIM_BUY").unwrap_or_default();
        Self::new(
            num("SKIM_ABOVE_USDC"),
            std::env::var("SKIM_KEEP_USDC")
                .ok()
                .and_then(|v| v.trim().parse::<f64>().ok())
                .filter(|v| v.is_finite() && *v >= 0.0)
                .unwrap_or(0.0),
            &buy,
            num("TOPUP_BELOW_USDC"),
            num("TOPUP_TO_USDC").unwrap_or(0.0),
        )
    }

    /// Settings that contradict each other switch the whole feature off, with
    /// a warning, rather than guess what the owner meant.
    pub fn new(
        skim_above: Option<f64>,
        skim_keep: f64,
        skim_buy: &str,
        topup_below: Option<f64>,
        topup_to: f64,
    ) -> Self {
        let mut cfg = Self {
            skim_above,
            skim_keep,
            skim_buy: parse_skim_buy(skim_buy),
            topup_below,
            topup_to,
        };
        if let Some(above) = cfg.skim_above {
            if cfg.skim_keep >= above {
                warn!("SKIM_KEEP_USDC {skim_keep} is not below SKIM_ABOVE_USDC {above}: skim off");
                cfg.skim_above = None;
            }
        }
        if let Some(below) = cfg.topup_below {
            if cfg.topup_to <= below {
                warn!("TOPUP_TO_USDC {topup_to} is not above TOPUP_BELOW_USDC {below}: top-up off");
                cfg.topup_below = None;
            }
        }
        // A top-up that lands above where a skim starts, or a skim that
        // leaves less than where a top-up starts, would move the same money
        // back and forth.
        if let (Some(above), Some(below)) = (cfg.skim_above, cfg.topup_below) {
            if cfg.topup_to >= above || cfg.skim_keep < below {
                warn!(
                    "SKIM_* and TOPUP_* overlap (top-up to {} vs skim above {above}, keep {} vs \
                     top-up below {below}): both off",
                    cfg.topup_to, cfg.skim_keep
                );
                cfg.skim_above = None;
                cfg.topup_below = None;
            }
        }
        cfg
    }

    pub fn is_on(&self) -> bool {
        self.skim_above.is_some() || self.topup_below.is_some()
    }
}

/// `BNB:50,ETH:50` into `[("BNB", 50.0), ("ETH", 50.0)]`; bad entries are
/// left out with a warning.
pub fn parse_skim_buy(text: &str) -> Vec<(String, f64)> {
    text.split(',')
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .filter_map(|entry| {
            let parsed = entry.split_once(':').and_then(|(coin, weight)| {
                let coin = coin.trim().to_uppercase();
                let weight = weight.trim().parse::<f64>().ok()?;
                (!coin.is_empty() && weight.is_finite() && weight > 0.0).then_some((coin, weight))
            });
            if parsed.is_none() {
                warn!("SKIM_BUY entry {entry:?} ignored (want COIN:PERCENT)");
            }
            parsed
        })
        .collect()
}

/// Everything the futures loop needs to move money: the client, the settings
/// and what has gone wrong today.
pub struct Wallet<W> {
    pub client: W,
    pub cfg: WalletConfig,
    pub state: WalletState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Feature {
    Transfer,
    SpotTrade,
}

/// What `run` did this cycle.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Moved {
    Nothing,
    /// Moved this much to spot.
    Skimmed(f64),
    /// Moved this much into futures.
    ToppedUp(f64),
}

#[derive(Default)]
pub struct WalletState {
    /// UTC day number a feature is off until the end of.
    off_on_day: Vec<(Feature, u64)>,
    /// No attempt before this time.
    retry_after_ms: u64,
    /// The last failure told to the owner, so it is said once.
    last_error: String,
}

impl WalletState {
    fn is_off(&self, feature: Feature, now_ms: u64) -> bool {
        self.off_on_day
            .iter()
            .any(|(f, day)| *f == feature && *day == now_ms / DAY_MS)
    }

    fn switch_off_today(&mut self, feature: Feature, now_ms: u64) {
        self.off_on_day.retain(|(f, _)| *f != feature);
        self.off_on_day.push((feature, now_ms / DAY_MS));
    }
}

/// Round an amount down to whole cents.
fn floor_cents(x: f64) -> f64 {
    (x * 100.0).floor() / 100.0
}

fn is_permission_error(error: &str) -> bool {
    error.contains("-2015")
        || error.contains("-2014")
        || error.to_lowercase().contains("permission")
}

/// Handle a failed call: a missing permission switches the feature off for the
/// day with one message; a rate limit waits briefly and quietly; anything else
/// waits an hour and is told once per distinct error.
async fn failed(
    state: &mut WalletState,
    feature: Feature,
    permission: &str,
    what: &str,
    error: &str,
    now_ms: u64,
    notifier: &Notifier,
) {
    if is_permission_error(error) {
        state.switch_off_today(feature, now_ms);
        let msg = format!(
            "\u{1f511} Key lacks permission *{permission}* (nøkkelen mangler rettighet): \
             {what} is off for the rest of the UTC day. Turn it on for the key on Binance.\n`{error}`"
        );
        error!("{msg}");
        notifier.send(&msg).await;
    } else if crate::futures::is_rate_limited(error) {
        state.retry_after_ms = now_ms + RATE_LIMIT_WAIT_MS;
        warn!("{what}: rate limited, waiting: {error}");
    } else {
        state.retry_after_ms = now_ms + FAILURE_WAIT_MS;
        error!("{what} failed: {error}");
        if state.last_error != error {
            state.last_error = error.to_string();
            notifier
                .send(&format!(
                    "\u{26a0}\u{fe0f} {what} failed, trying again in an hour.\n`{error}`"
                ))
                .await;
        }
    }
}

/// Skim or top up if the settings and the account call for it. Never errors:
/// the cycle goes on whatever happens here. After a move the caller should end
/// the cycle, since the account it read is out of date.
#[allow(clippy::too_many_arguments)]
pub async fn run<W: WalletExchange, E: FuturesExchange>(
    wallet: &W,
    ex: &E,
    cfg: &WalletConfig,
    state: &mut WalletState,
    account: FuturesAccount,
    positions: &[FuturesPosition],
    pairs: &[String],
    risk: &mut RiskManager,
    notifier: &Notifier,
    now_ms: u64,
) -> Moved {
    if !cfg.is_on() || now_ms < state.retry_after_ms || state.is_off(Feature::Transfer, now_ms) {
        return Moved::Nothing;
    }

    if let Some(above) = cfg.skim_above {
        if account.equity > above {
            return skim(
                wallet, ex, cfg, state, account, positions, pairs, risk, notifier, now_ms,
            )
            .await;
        }
    }

    if let Some(below) = cfg.topup_below {
        if account.equity < below {
            return top_up(wallet, cfg, state, account, below, risk, notifier, now_ms).await;
        }
    }
    Moved::Nothing
}

#[allow(clippy::too_many_arguments)]
async fn skim<W: WalletExchange, E: FuturesExchange>(
    wallet: &W,
    ex: &E,
    cfg: &WalletConfig,
    state: &mut WalletState,
    account: FuturesAccount,
    positions: &[FuturesPosition],
    pairs: &[String],
    risk: &mut RiskManager,
    notifier: &Notifier,
    now_ms: u64,
) -> Moved {
    // Margin is never weakened under an open position.
    if !positions.is_empty() {
        return Moved::Nothing;
    }
    for symbol in pairs {
        match ex.trigger_orders(symbol).await {
            Ok(orders) if orders.is_empty() => {}
            Ok(_) => {
                info!("Skim waits: {symbol} still has resting orders");
                return Moved::Nothing;
            }
            Err(e) => {
                warn!("Skim waits: could not read {symbol} orders: {e}");
                return Moved::Nothing;
            }
        }
    }
    // Never more than what is free, whatever the equity says.
    let amount = floor_cents(account.available.min(account.equity) - cfg.skim_keep);
    if amount < MIN_MOVE || amount > account.available {
        return Moved::Nothing;
    }
    match wallet
        .universal_transfer(TransferDirection::FuturesToSpot, ASSET, amount)
        .await
    {
        Ok(id) => {
            risk.record_own_transfer(-amount, now_ms);
            let left = account.equity - amount;
            info!("Skimmed {amount:.2} {ASSET} to spot (transfer {id}); futures equity {left:.2}");
            let mut msg = format!(
                "\u{1f4b0} *Profit moved to spot*\n`{amount:.2}` {ASSET} out of futures; futures \
                 equity now about `{left:.2}`"
            );
            if !cfg.skim_buy.is_empty() {
                for line in buy_coins(wallet, cfg, state, amount, now_ms, notifier).await {
                    msg.push('\n');
                    msg.push_str(&line);
                }
            }
            notifier.send(&msg).await;
            Moved::Skimmed(amount)
        }
        Err(e) => {
            failed(
                state,
                Feature::Transfer,
                PERMIT_TRANSFER,
                "Profit withdrawal",
                &e,
                now_ms,
                notifier,
            )
            .await;
            Moved::Nothing
        }
    }
}

/// Spend the skimmed `amount` on the `SKIM_BUY` coins. A coin that cannot be
/// bought stays as USDC; nothing here undoes the transfer or stops the bot.
/// Returns one line per coin for the owner's message.
async fn buy_coins<W: WalletExchange>(
    wallet: &W,
    cfg: &WalletConfig,
    state: &mut WalletState,
    amount: f64,
    now_ms: u64,
    notifier: &Notifier,
) -> Vec<String> {
    let total: f64 = cfg.skim_buy.iter().map(|(_, w)| w).sum();
    // Weights are percent of the amount; over 100 they are shares of it.
    let scale = total.max(100.0);
    let mut lines = Vec::new();
    for (coin, weight) in &cfg.skim_buy {
        let share = floor_cents(amount * weight / scale);
        let symbol = format!("{coin}{ASSET}");
        if state.is_off(Feature::SpotTrade, now_ms) {
            lines.push(format!(
                "{symbol}: not bought, `{share:.2}` {ASSET} stays in spot"
            ));
            continue;
        }
        let filters = match wallet.spot_filters(&symbol).await {
            Ok(f) => f,
            Err(e) => {
                warn!("{symbol}: spot rules not read: {e}");
                lines.push(format!(
                    "{symbol}: not bought ({e}), `{share:.2}` {ASSET} stays in spot"
                ));
                continue;
            }
        };
        if share < filters.min_notional || share <= 0.0 {
            lines.push(format!(
                "{symbol}: `{share:.2}` is under the minimum order `{:.2}`, stays as {ASSET}",
                filters.min_notional
            ));
            continue;
        }
        match wallet.buy_with_quote(&symbol, share).await {
            Ok(fill) => {
                info!(
                    "Bought {} {coin} for {share:.2} {ASSET}",
                    fill.filled_quantity
                );
                lines.push(format!(
                    "{symbol}: bought `{}` for `{share:.2}`",
                    fill.filled_quantity
                ));
            }
            Err(e) => {
                lines.push(format!(
                    "{symbol}: buy failed, `{share:.2}` {ASSET} stays in spot"
                ));
                failed(
                    state,
                    Feature::SpotTrade,
                    PERMIT_SPOT,
                    &format!("{symbol} buy"),
                    &e,
                    now_ms,
                    notifier,
                )
                .await;
            }
        }
    }
    lines
}

#[allow(clippy::too_many_arguments)]
async fn top_up<W: WalletExchange>(
    wallet: &W,
    cfg: &WalletConfig,
    state: &mut WalletState,
    account: FuturesAccount,
    below: f64,
    risk: &mut RiskManager,
    notifier: &Notifier,
    now_ms: u64,
) -> Moved {
    let free = match wallet.spot_free(ASSET).await {
        Ok(free) => free,
        Err(e) => {
            failed(
                state,
                Feature::Transfer,
                "Enable Reading",
                "Top-up (spot balance)",
                &e,
                now_ms,
                notifier,
            )
            .await;
            return Moved::Nothing;
        }
    };
    // Only USDC already in spot: nothing is sold to fill up.
    let amount = floor_cents((cfg.topup_to - account.equity).min(free));
    if amount < MIN_MOVE || amount > free {
        return Moved::Nothing;
    }
    match wallet
        .universal_transfer(TransferDirection::SpotToFutures, ASSET, amount)
        .await
    {
        Ok(id) => {
            risk.record_own_transfer(amount, now_ms);
            let now = account.equity + amount;
            info!(
                "Topped up futures with {amount:.2} {ASSET} (transfer {id}); equity was {:.2} \
                 (below {below:.2})",
                account.equity
            );
            notifier
                .send(&format!(
                    "\u{1f4b5} *Futures topped up*\n`{amount:.2}` {ASSET} in from spot; futures \
                     equity now about `{now:.2}`"
                ))
                .await;
            Moved::ToppedUp(amount)
        }
        Err(e) => {
            failed(
                state,
                Feature::Transfer,
                PERMIT_TRANSFER,
                "Top-up",
                &e,
                now_ms,
                notifier,
            )
            .await;
            Moved::Nothing
        }
    }
}
