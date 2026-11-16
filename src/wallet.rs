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
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tracing::{error, info, warn};

/// The margin and spot asset moved.
pub const ASSET: &str = "USDC";
/// What can be transferred between the futures wallet and others in credits
/// mode (Binance FAQ: BTC, ETH, BNB and USDC).
const TRANSFERABLE: [&str; 4] = ["USDC", "BNB", "BTC", "ETH"];
/// A skim of collateral takes only this share of the excess, so rounding,
/// price movement and the collateral haircut never leave the account under
/// `SKIM_KEEP_USDC`.
const COLLATERAL_SAFETY: f64 = 0.98;
/// A transfer measured after the fact must be within this band of the nominal
/// value to be believed; outside it the nominal value is used.
const MEASURED_BAND: (f64, f64) = (0.5, 1.2);
/// Smallest amount worth moving: no pocket change every cycle.
pub const MIN_MOVE: f64 = 5.0;
const DAY_MS: u64 = 86_400_000;
/// Wait after a rate limit, and after any other failure.
const RATE_LIMIT_WAIT_MS: u64 = 5 * 60_000;
const FAILURE_WAIT_MS: u64 = 60 * 60_000;
/// After a move, wait this long before the next: a late balance update must
/// not look like money still to move.
const COOLDOWN_AFTER_MOVE_MS: u64 = 60 * 60_000;

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
    /// The asset that leaves futures: `USDC` (default) or the collateral the
    /// account holds, `BNB`, `BTC` or `ETH`.
    pub skim_asset: String,
    /// Sell a non-USDC skim to USDC in spot (`SKIM_TO=USDC`).
    pub skim_to_usdc: bool,
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
        let text = |key: &str| std::env::var(key).unwrap_or_default();
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
        .with_skim_asset(&text("SKIM_ASSET"), &text("SKIM_TO"))
    }

    /// `SKIM_ASSET` (blank is USDC) and `SKIM_TO`. Only the assets Binance
    /// lets out of a credits-mode futures wallet are accepted; anything else
    /// switches the skim off. `SKIM_TO=USDC` sells a non-USDC skim in spot.
    pub fn with_skim_asset(mut self, asset: &str, to: &str) -> Self {
        let asset = asset.trim().to_uppercase();
        if !asset.is_empty() {
            if TRANSFERABLE.contains(&asset.as_str()) {
                self.skim_asset = asset;
            } else {
                warn!("SKIM_ASSET {asset:?} is not one of {TRANSFERABLE:?}: skim off");
                self.skim_above = None;
            }
        }
        match to.trim().to_uppercase().as_str() {
            "" => {}
            "USDC" => self.skim_to_usdc = true,
            other => {
                warn!("SKIM_TO {other:?} is not supported (only USDC): ignored");
            }
        }
        if self.skim_asset == ASSET {
            self.skim_to_usdc = false;
        } else if !self.skim_to_usdc && !self.skim_buy.is_empty() {
            warn!(
                "SKIM_BUY needs USDC: with SKIM_ASSET not USDC it requires SKIM_TO=USDC; ignored"
            );
            self.skim_buy.clear();
        }
        self
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
            skim_asset: ASSET.to_string(),
            skim_to_usdc: false,
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
    /// Moved this much (USD) to spot.
    Skimmed(f64),
    /// Moved this much into futures.
    ToppedUp(f64),
}

/// Collateral the bot moved to spot and has not sold yet. Kept on disk so a
/// restart or a failed sale does not leave it forgotten.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct PendingSale {
    asset: String,
    quantity: f64,
}

#[derive(Serialize, Deserialize, Default)]
struct StoredWallet {
    pending: Vec<PendingSale>,
}

const STATE_FILE: &str = "wallet-state.json";

/// The wallet state file: `wallet-state.json` next to `risk-state.json`.
pub fn state_file_from_env() -> PathBuf {
    crate::risk::state_dir_from_env().join(STATE_FILE)
}

#[derive(Default)]
pub struct WalletState {
    /// UTC day number a feature is off until the end of.
    off_on_day: Vec<(Feature, u64)>,
    /// No attempt before this time.
    retry_after_ms: u64,
    /// The last failure told to the owner, so it is said once.
    last_error: String,
    /// Moved to spot by the bot, not sold. Never more is sold than this.
    pending: Vec<PendingSale>,
    /// The owner has been told something stands unsold.
    unsold_told: bool,
    path: Option<PathBuf>,
}

impl WalletState {
    /// State kept in `path`; whatever stood unsold when the bot last ran is
    /// read back. An unreadable file starts empty, with a warning.
    pub fn with_file(path: PathBuf) -> Self {
        let mut state = Self::default();
        match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<StoredWallet>(&bytes) {
                Ok(stored) => {
                    state.pending = stored
                        .pending
                        .into_iter()
                        .filter(|p| p.quantity.is_finite() && p.quantity > 0.0)
                        .collect();
                    if !state.pending.is_empty() {
                        warn!(
                            "Unsold collateral from before the restart: {:?}",
                            state.pending
                        );
                    }
                }
                Err(e) => warn!(
                    "Wallet state {} is corrupt ({e}); starting empty",
                    path.display()
                ),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => warn!(
                "Wallet state {} not readable ({e}); starting empty",
                path.display()
            ),
        }
        state.path = Some(path);
        state
    }

    fn persist(&self) {
        let Some(path) = &self.path else {
            return;
        };
        let stored = StoredWallet {
            pending: self.pending.clone(),
        };
        let tmp = path.with_extension("json.tmp");
        let written = serde_json::to_vec(&stored)
            .map_err(std::io::Error::other)
            .and_then(|bytes| {
                if let Some(dir) = path.parent() {
                    std::fs::create_dir_all(dir)?;
                }
                std::fs::write(&tmp, bytes)?;
                std::fs::rename(&tmp, path)
            });
        if let Err(e) = written {
            warn!("Wallet state not saved to {}: {e}", path.display());
        }
    }

    fn add_pending(&mut self, asset: &str, quantity: f64) {
        match self.pending.iter_mut().find(|p| p.asset == asset) {
            Some(p) => p.quantity += quantity,
            None => self.pending.push(PendingSale {
                asset: asset.to_string(),
                quantity,
            }),
        }
        self.persist();
    }

    fn clear_pending(&mut self, asset: &str) {
        self.pending.retain(|p| p.asset != asset);
        self.unsold_told = false;
        self.persist();
    }

    fn pending_quantity(&self, asset: &str) -> f64 {
        self.pending
            .iter()
            .find(|p| p.asset == asset)
            .map_or(0.0, |p| p.quantity)
    }

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

/// Round `x` down to a multiple of `step`.
fn floor_to(x: f64, step: f64) -> f64 {
    // Eight decimals is the finest any amount here is sent at; rounding keeps
    // float noise out of the request.
    (((x / step) + 1e-9).floor() * step * 1e8).round() / 1e8
}

fn is_permission_error(error: &str) -> bool {
    error.contains("-2015")
        || error.contains("-2014")
        || error.to_lowercase().contains("permission")
}

/// A transfer whose answer never came (timeout, broken connection): it may
/// have gone through. An answer from Binance, even a refusal, is definite.
fn is_lost_reply(error: &str) -> bool {
    !error.starts_with("Binance API error")
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

/// How much the account's equity changed across a transfer of nominal `value`
/// (negative out, positive in), read again from the account; `None` when the
/// read fails or the change is not about the size of the transfer.
async fn measured_change<E: FuturesExchange>(ex: &E, before: f64, nominal: f64) -> Option<f64> {
    let after = ex.account().await.ok()?;
    let delta = after.equity - before;
    let ratio = delta / nominal;
    let (lo, hi) = MEASURED_BAND;
    (delta.is_finite() && ratio >= lo && ratio <= hi).then_some(delta)
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
    risk: &mut RiskManager,
    notifier: &Notifier,
    now_ms: u64,
) -> Moved {
    if !cfg.is_on() || now_ms < state.retry_after_ms {
        return Moved::Nothing;
    }
    // A broken reading must never turn into a transfer.
    if !account.equity.is_finite()
        || !account.available.is_finite()
        || account.equity <= 0.0
        || account.available < 0.0
    {
        warn!("Wallet moves skipped: account reads {account:?}");
        return Moved::Nothing;
    }

    // Collateral moved earlier and not yet sold.
    if !state.pending.is_empty() && cfg.skim_to_usdc && !state.is_off(Feature::SpotTrade, now_ms) {
        sell_pending(wallet, state, notifier, now_ms).await;
        if now_ms < state.retry_after_ms {
            return Moved::Nothing;
        }
    }
    if state.is_off(Feature::Transfer, now_ms) {
        return Moved::Nothing;
    }

    if let Some(above) = cfg.skim_above {
        if account.equity > above {
            return skim(
                wallet, ex, cfg, state, account, positions, risk, notifier, now_ms,
            )
            .await;
        }
    }

    if let Some(below) = cfg.topup_below {
        if account.equity < below {
            return top_up(
                wallet, ex, cfg, state, account, below, risk, notifier, now_ms,
            )
            .await;
        }
    }
    Moved::Nothing
}

/// Try again to sell what was moved to spot and is still unsold. Told to the
/// owner once while it stands; a sale cools down for an hour after a miss.
async fn sell_pending<W: WalletExchange>(
    wallet: &W,
    state: &mut WalletState,
    notifier: &Notifier,
    now_ms: u64,
) {
    for PendingSale { asset, quantity } in state.pending.clone() {
        let symbol = format!("{asset}{ASSET}");
        let price = match wallet.spot_price(&symbol).await {
            Ok(p) if p.is_finite() && p > 0.0 => p,
            other => {
                let e = other.err().unwrap_or_else(|| "no usable price".into());
                failed(
                    state,
                    Feature::SpotTrade,
                    PERMIT_SPOT,
                    &format!("{symbol} price"),
                    &e,
                    now_ms,
                    notifier,
                )
                .await;
                continue;
            }
        };
        match sell_to_usdc(wallet, &asset, quantity, price, state, now_ms, notifier).await {
            Ok((line, _)) => {
                state.clear_pending(&asset);
                notifier
                    .send(&format!("\u{2705} Earlier unsold {asset} sold\n{line}"))
                    .await;
            }
            Err(line) => {
                state.retry_after_ms = state.retry_after_ms.max(now_ms + FAILURE_WAIT_MS);
                if !state.unsold_told {
                    state.unsold_told = true;
                    notifier
                        .send(&format!("\u{26a0}\u{fe0f} Still unsold in spot\n{line}"))
                        .await;
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn skim<W: WalletExchange, E: FuturesExchange>(
    wallet: &W,
    ex: &E,
    cfg: &WalletConfig,
    state: &mut WalletState,
    account: FuturesAccount,
    positions: &[FuturesPosition],
    risk: &mut RiskManager,
    notifier: &Notifier,
    now_ms: u64,
) -> Moved {
    // Margin is never weakened under an open position.
    if !positions.is_empty() {
        return Moved::Nothing;
    }
    // Nor under any resting order on the account, ours or a manual one on a
    // pair we do not trade (both endpoints without a symbol: weight 40 each).
    match ex.open_order_count().await {
        Ok(0) => {}
        Ok(n) => {
            info!("Skim waits: {n} open order(s) on the account");
            return Moved::Nothing;
        }
        Err(e) => {
            warn!("Skim waits: could not read the open orders: {e}");
            return Moved::Nothing;
        }
    }
    let asset = cfg.skim_asset.as_str();
    // The excess over the keep amount, never more than what is free.
    let excess = account.available.min(account.equity) - cfg.skim_keep;
    if !excess.is_finite() || excess <= 0.0 {
        return Moved::Nothing;
    }
    // How much leaves (in units of `asset`), its dollar value, and its price.
    let (quantity, value, price) = if asset == ASSET {
        let amount = floor_cents(excess);
        (amount, amount, 1.0)
    } else {
        let symbol = format!("{asset}{ASSET}");
        let price = match wallet.spot_price(&symbol).await {
            Ok(p) if p.is_finite() && p > 0.0 => p,
            other => {
                let e = other.err().unwrap_or_else(|| "no usable price".into());
                failed(
                    state,
                    Feature::Transfer,
                    "Enable Reading",
                    &format!("{symbol} price"),
                    &e,
                    now_ms,
                    notifier,
                )
                .await;
                return Moved::Nothing;
            }
        };
        let held = match ex.asset_wallet(asset).await {
            Ok(h) if h.is_finite() && h > 0.0 => h,
            Ok(_) => return Moved::Nothing,
            Err(e) => {
                warn!("Skim waits: could not read the {asset} balance: {e}");
                return Moved::Nothing;
            }
        };
        // A little under the excess, and never more than the wallet holds.
        let wanted = excess * COLLATERAL_SAFETY / price;
        let quantity = floor_to(wanted.min(held), 0.000001);
        (quantity, quantity * price, price)
    };
    if !value.is_finite() || value < MIN_MOVE || value > account.available {
        return Moved::Nothing;
    }
    let nominal = -value;
    let sent = wallet
        .universal_transfer(TransferDirection::FuturesToSpot, asset, quantity)
        .await;
    let (how, change) = match sent {
        Ok(id) => (
            format!("transfer {id}"),
            measured_change(ex, account.equity, nominal)
                .await
                .unwrap_or(nominal),
        ),
        Err(e) if is_lost_reply(&e) => match measured_change(ex, account.equity, nominal).await {
            Some(delta) => {
                warn!(
                    "Skim reply lost ({e}), but equity fell by {:.2}: counted as done",
                    -delta
                );
                ("reply lost, confirmed by the account".to_string(), delta)
            }
            None => {
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
                return Moved::Nothing;
            }
        },
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
            return Moved::Nothing;
        }
    };
    // A late balance update must not bring a second transfer next cycle.
    state.retry_after_ms = now_ms + COOLDOWN_AFTER_MOVE_MS;
    risk.record_own_transfer(change, now_ms);
    let left = account.equity + change;
    info!("Skimmed {quantity} {asset} (~{value:.2} USD, {how}) to spot; futures equity about {left:.2}");
    let keep = cfg.skim_keep;
    let mut msg = if asset == ASSET {
        format!(
            "\u{1f4b0} Moved `{quantity:.2}` {ASSET} above the `{keep}` USD stake to spot; \
             futures equity now about `{left:.2}`"
        )
    } else {
        format!(
            "\u{1f4b0} Moved `{quantity}` {asset} (about `{value:.1}` USD) above the `{keep}` USD \
             stake to spot; futures equity now about `{left:.2}`"
        )
    };
    // Back to USDC, and then on to coins if asked.
    let mut usdc = (asset == ASSET).then_some(value);
    if asset != ASSET && cfg.skim_to_usdc {
        state.add_pending(asset, quantity);
        let owed = state.pending_quantity(asset);
        match sell_to_usdc(wallet, asset, owed, price, state, now_ms, notifier).await {
            Ok((line, proceeds)) => {
                state.clear_pending(asset);
                msg.push('\n');
                msg.push_str(&line);
                usdc = Some(proceeds);
            }
            Err(line) => {
                state.unsold_told = true;
                msg.push('\n');
                msg.push_str(&line);
            }
        }
    }
    if let (Some(amount), false) = (usdc, cfg.skim_buy.is_empty()) {
        for line in buy_coins(wallet, cfg, state, amount, now_ms, notifier).await {
            msg.push('\n');
            msg.push_str(&line);
        }
    }
    notifier.send(&msg).await;
    Moved::Skimmed(value)
}

/// Sell `quantity` of `asset` in spot for USDC, rounded down to the symbol's
/// step and held to its minimum order. Returns a line for the owner and the
/// USDC received, or a line saying the asset stays in spot (the transfer is
/// not undone, and the bot goes on). `quantity` is only ever what the bot
/// itself moved.
async fn sell_to_usdc<W: WalletExchange>(
    wallet: &W,
    asset: &str,
    quantity: f64,
    price: f64,
    state: &mut WalletState,
    now_ms: u64,
    notifier: &Notifier,
) -> Result<(String, f64), String> {
    let symbol = format!("{asset}{ASSET}");
    let stays =
        |why: &str| format!("{symbol}: not sold ({why}), `{quantity}` {asset} stays in spot");
    if state.is_off(Feature::SpotTrade, now_ms) {
        return Err(stays("spot trading is off today"));
    }
    let filters = wallet.spot_filters(&symbol).await.map_err(|e| stays(&e))?;
    let step = if filters.step_size > 0.0 {
        filters.step_size
    } else {
        0.000001
    };
    let sell = floor_to(quantity, step);
    if sell <= 0.0 || sell < filters.min_qty || sell * price < filters.min_notional {
        return Err(stays("under the minimum order"));
    }
    match wallet.sell_base(&symbol, sell).await {
        Ok(fill) => {
            let proceeds = if fill.average_price > 0.0 {
                fill.filled_quantity * fill.average_price
            } else {
                sell * price
            };
            info!(
                "Sold {} {asset} for about {proceeds:.2} {ASSET}",
                fill.filled_quantity
            );
            Ok((
                format!(
                    "{symbol}: sold `{}` for about `{proceeds:.2}` {ASSET}",
                    fill.filled_quantity
                ),
                proceeds,
            ))
        }
        Err(e) => {
            failed(
                state,
                Feature::SpotTrade,
                PERMIT_SPOT,
                &format!("{symbol} sell"),
                &e,
                now_ms,
                notifier,
            )
            .await;
            Err(stays("sell failed"))
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
async fn top_up<W: WalletExchange, E: FuturesExchange>(
    wallet: &W,
    ex: &E,
    cfg: &WalletConfig,
    state: &mut WalletState,
    account: FuturesAccount,
    below: f64,
    risk: &mut RiskManager,
    notifier: &Notifier,
    now_ms: u64,
) -> Moved {
    let free = match wallet.spot_free(ASSET).await {
        Ok(free) if free.is_finite() => free,
        Ok(_) => return Moved::Nothing,
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
    if !amount.is_finite() || amount < MIN_MOVE || amount > free {
        return Moved::Nothing;
    }
    let sent = wallet
        .universal_transfer(TransferDirection::SpotToFutures, ASSET, amount)
        .await;
    let change = match sent {
        Ok(_) => measured_change(ex, account.equity, amount)
            .await
            .unwrap_or(amount),
        Err(e) if is_lost_reply(&e) => match measured_change(ex, account.equity, amount).await {
            Some(delta) => {
                warn!("Top-up reply lost ({e}), but equity rose by {delta:.2}: counted as done");
                delta
            }
            None => {
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
                return Moved::Nothing;
            }
        },
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
            return Moved::Nothing;
        }
    };
    state.retry_after_ms = now_ms + COOLDOWN_AFTER_MOVE_MS;
    risk.record_own_transfer(change, now_ms);
    let now = account.equity + change;
    info!(
        "Topped up futures with {amount:.2} {ASSET}; equity was {:.2} (below {below:.2})",
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
