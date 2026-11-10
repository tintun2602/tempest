//! Meme-coin paper test (`MODE=memepaper`). No money, no wallet, no model in
//! the loop: every 5 minutes it reads DexScreener's free API (new profiles,
//! latest and top boosts on Solana and Base) and RugCheck for Solana mints,
//! applies fixed entry rules, and paper-trades what passes with fixed exits
//! and realistic costs. Every token that fails a rule is followed for 24 h
//! too, so the report can say whether the filter kept the rug pulls out.
//!
//! The rules are fixed for the 14-day run (playbook
//! `agents/trading/meme-paper-plan.md`); change them only between runs.
//!
//! Files, in `MEME_DIR` (default `/var/lib/tempest/meme`):
//! `state.json` (open paper positions and followed tokens, rewritten each
//! round), `trades.csv` and `filtered.csv` (appended when a trade or a
//! follow-up ends), `summary.txt` (one line, totals since the start).

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tracing::{error, info, warn};

const DEX: &str = "https://api.dexscreener.com";
const RUGCHECK: &str = "https://api.rugcheck.xyz/v1/tokens";
const CHAINS: [&str; 2] = ["solana", "base"];
/// DexScreener's `tokens/v1` takes at most 30 addresses per call.
const BATCH: usize = 30;

// ----- the rules -----

const MIN_AGE_MS: u64 = 30 * 60_000;
const MAX_AGE_MS: u64 = 72 * 3_600_000;
const MIN_LIQUIDITY: f64 = 20_000.0;
/// Last hour's volume against the hourly mean of the last six hours.
const MIN_VOLUME_RISE: f64 = 2.0;
const MIN_BUY_SELL: f64 = 1.2;
const MAX_H1_CHANGE_PCT: f64 = 100.0;
const MIN_LP_LOCKED_PCT: f64 = 80.0;
const MAX_TOP10_PCT: f64 = 30.0;

const TAKE_PROFIT: f64 = 0.50;
const STOP: f64 = -0.30;
const MAX_HOLD_MS: u64 = 24 * 3_600_000;
/// Fee and slippage each way, before the size-dependent part.
const COST_EACH_WAY: f64 = 0.02;
const PAPER_SIZE_USD: f64 = 100.0;
/// Liquidity down this much, or price down this much, within 24 h: a rug.
const RUG_LIQUIDITY_DROP: f64 = 0.80;
const RUG_PRICE_DROP: f64 = 0.90;
const FOLLOW_UPS_MS: [u64; 3] = [3_600_000, 6 * 3_600_000, 24 * 3_600_000];

/// What DexScreener reports for a token's most liquid pair.
#[derive(Debug, Clone, PartialEq)]
pub struct Pair {
    pub chain: String,
    pub token: String,
    pub symbol: String,
    pub price: f64,
    pub liquidity: f64,
    pub volume_h1: f64,
    pub volume_h6: f64,
    pub buys_h1: f64,
    pub sells_h1: f64,
    pub change_h1: f64,
    pub created_ms: u64,
}

/// The parts of a RugCheck report the rules read.
#[derive(Debug, Clone, PartialEq)]
pub struct Rug {
    pub danger: bool,
    pub mint_authority: bool,
    pub freeze_authority: bool,
    pub lp_locked_pct: f64,
    /// Top ten holders' share, AMM pools left out.
    pub top10_pct: f64,
    /// Creator wallet's share of supply, percent.
    pub creator_pct: f64,
    /// Wallets found to be funded together (an insider network).
    pub insiders: bool,
}

/// The first rule a pair fails, or `None` when it passes. Solana needs a
/// RugCheck report; without one it fails.
pub fn entry_check(pair: &Pair, rug: Option<&Rug>, now_ms: u64) -> Option<&'static str> {
    let age = now_ms.saturating_sub(pair.created_ms);
    if pair.created_ms == 0 || age < MIN_AGE_MS {
        return Some("too new");
    }
    if age > MAX_AGE_MS {
        return Some("too old");
    }
    if pair.price.is_nan() || pair.price <= 0.0 {
        return Some("no price");
    }
    if pair.liquidity < MIN_LIQUIDITY {
        return Some("liquidity");
    }
    let hourly = pair.volume_h6 / 6.0;
    if !(hourly > 0.0 && pair.volume_h1 >= MIN_VOLUME_RISE * hourly) {
        return Some("volume not rising");
    }
    let buys_ok = if pair.sells_h1 > 0.0 {
        pair.buys_h1 / pair.sells_h1 >= MIN_BUY_SELL
    } else {
        pair.buys_h1 > 0.0
    };
    if !buys_ok {
        return Some("buy/sell");
    }
    if !(pair.change_h1 >= 0.0 && pair.change_h1 <= MAX_H1_CHANGE_PCT) {
        return Some("price change");
    }
    if pair.chain == "solana" {
        let Some(rug) = rug else {
            return Some("no rugcheck");
        };
        if rug.danger {
            return Some("rugcheck danger");
        }
        if rug.mint_authority || rug.freeze_authority {
            return Some("authority");
        }
        if rug.lp_locked_pct < MIN_LP_LOCKED_PCT {
            return Some("lp not locked");
        }
        if rug.top10_pct > MAX_TOP10_PCT {
            return Some("holders");
        }
    }
    None
}

/// Price paid on entry and received on exit: the quoted price moved against
/// us by the flat cost plus the paper size's share of the pool.
pub fn fill(price: f64, liquidity: f64, buy: bool) -> f64 {
    let impact = if liquidity > 0.0 {
        PAPER_SIZE_USD / liquidity
    } else {
        1.0
    };
    let cost = (COST_EACH_WAY + impact).min(0.99);
    if buy {
        price * (1.0 + cost)
    } else {
        price * (1.0 - cost)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Position {
    pub chain: String,
    pub token: String,
    pub symbol: String,
    pub entry_ms: u64,
    /// Quoted price at entry, before costs: exits are measured against it.
    pub quote: f64,
    /// What the paper trade paid, costs included.
    pub paid: f64,
    pub liquidity0: f64,
    pub min_liquidity: f64,
    pub min_price: f64,
}

/// Why a paper position closes now, if it does.
pub fn exit_reason(pos: &Position, price: f64, now_ms: u64) -> Option<&'static str> {
    let change = price / pos.quote - 1.0;
    if change >= TAKE_PROFIT {
        Some("target")
    } else if change <= STOP {
        Some("stop")
    } else if now_ms.saturating_sub(pos.entry_ms) >= MAX_HOLD_MS {
        Some("time")
    } else {
        None
    }
}

pub fn is_rug(price0: f64, liquidity0: f64, min_price: f64, min_liquidity: f64) -> bool {
    (liquidity0 > 0.0 && min_liquidity <= liquidity0 * (1.0 - RUG_LIQUIDITY_DROP))
        || (price0 > 0.0 && min_price <= price0 * (1.0 - RUG_PRICE_DROP))
}

/// A token that failed a rule, followed for 24 h.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Followed {
    pub chain: String,
    pub token: String,
    pub symbol: String,
    pub reason: String,
    pub seen_ms: u64,
    pub price0: f64,
    pub liquidity0: f64,
    pub min_price: f64,
    pub min_liquidity: f64,
    /// Price at 1 h, 6 h and 24 h; `None` until reached (or when the token
    /// vanished from DexScreener).
    pub after: [Option<f64>; 3],
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct State {
    pub started_ms: u64,
    pub open: Vec<Position>,
    pub followed: Vec<Followed>,
    /// Tokens already traded or filtered: each is judged once per run.
    pub judged: HashSet<String>,
    pub closed: u32,
    pub wins: u32,
    pub net_pct_sum: f64,
    pub worst_pct: f64,
    pub rugs_traded: u32,
    pub filtered_done: u32,
    pub rugs_filtered: u32,
}

/// One closed paper trade as a CSV row.
#[derive(Debug, Clone, PartialEq)]
pub struct Closed {
    pub pos: Position,
    pub exit_ms: u64,
    pub exit_quote: f64,
    pub received: f64,
    pub reason: &'static str,
    pub net_pct: f64,
    pub rug: bool,
}

impl State {
    fn key(chain: &str, token: &str) -> String {
        format!("{chain}:{token}")
    }

    /// One round on fresh pair data. `pairs` is keyed like `key`; `rugs` holds
    /// RugCheck reports for Solana tokens. Returns trades closed and
    /// follow-ups finished this round.
    pub fn step(
        &mut self,
        candidates: &[(String, String)],
        pairs: &HashMap<String, Pair>,
        rugs: &HashMap<String, Rug>,
        now_ms: u64,
    ) -> (Vec<Closed>, Vec<Followed>) {
        if self.started_ms == 0 {
            self.started_ms = now_ms;
        }
        let mut closed = Vec::new();
        let mut still_open = Vec::new();
        for mut pos in std::mem::take(&mut self.open) {
            let key = Self::key(&pos.chain, &pos.token);
            // A token that vanished from DexScreener is marked at zero: the
            // pool is gone.
            let (price, liquidity) = pairs
                .get(&key)
                .map_or((0.0, 0.0), |p| (p.price, p.liquidity));
            pos.min_price = pos.min_price.min(price);
            pos.min_liquidity = pos.min_liquidity.min(liquidity);
            let gone = !pairs.contains_key(&key);
            let reason = if gone {
                Some("vanished")
            } else {
                exit_reason(&pos, price, now_ms)
            };
            if let Some(reason) = reason {
                let received = if gone {
                    0.0
                } else {
                    fill(price, liquidity, false)
                };
                let net_pct = (received / pos.paid - 1.0) * 100.0;
                let rug = is_rug(pos.quote, pos.liquidity0, pos.min_price, pos.min_liquidity);
                self.closed += 1;
                if net_pct > 0.0 {
                    self.wins += 1;
                }
                self.net_pct_sum += net_pct;
                self.worst_pct = self.worst_pct.min(net_pct);
                if rug {
                    self.rugs_traded += 1;
                }
                closed.push(Closed {
                    pos,
                    exit_ms: now_ms,
                    exit_quote: price,
                    received,
                    reason,
                    net_pct,
                    rug,
                });
            } else {
                still_open.push(pos);
            }
        }
        self.open = still_open;

        let (done, rugs_done) = advance_followed(&mut self.followed, pairs, now_ms);
        self.filtered_done += done.len() as u32;
        self.rugs_filtered += rugs_done;

        for (chain, token) in candidates {
            let key = Self::key(chain, token);
            if self.judged.contains(&key) {
                continue;
            }
            let Some(pair) = pairs.get(&key) else {
                continue;
            };
            let rug = rugs.get(&key);
            // A Solana token whose report is not in yet waits for a later
            // round rather than failing on a network hiccup.
            if pair.chain == "solana" && rug.is_none() {
                continue;
            }
            match entry_check(pair, rug, now_ms) {
                // Too new: judged again later, once it is old enough.
                Some("too new") => {}
                Some(reason) => {
                    self.judged.insert(key);
                    self.followed.push(Followed {
                        chain: pair.chain.clone(),
                        token: pair.token.clone(),
                        symbol: pair.symbol.clone(),
                        reason: reason.to_string(),
                        seen_ms: now_ms,
                        price0: pair.price,
                        liquidity0: pair.liquidity,
                        min_price: pair.price,
                        min_liquidity: pair.liquidity,
                        after: [None; 3],
                    });
                }
                None => {
                    self.judged.insert(key);
                    self.open.push(Position {
                        chain: pair.chain.clone(),
                        token: pair.token.clone(),
                        symbol: pair.symbol.clone(),
                        entry_ms: now_ms,
                        quote: pair.price,
                        paid: fill(pair.price, pair.liquidity, true),
                        liquidity0: pair.liquidity,
                        min_liquidity: pair.liquidity,
                        min_price: pair.price,
                    });
                }
            }
        }
        (closed, done)
    }

    pub fn summary(&self, now_ms: u64) -> String {
        let days = now_ms.saturating_sub(self.started_ms) as f64 / 86_400_000.0;
        let hit = if self.closed > 0 {
            100.0 * f64::from(self.wins) / f64::from(self.closed)
        } else {
            0.0
        };
        let mean = if self.closed > 0 {
            self.net_pct_sum / f64::from(self.closed)
        } else {
            0.0
        };
        format!(
            "v1: day {days:.1} | {} open | {} closed, hit {hit:.0} %, mean {mean:+.1} %/trade after costs, \
             worst {:.1} %, rugs traded {} | filtered followed 24 h: {}, of which rugs {} | following {}",
            self.open.len(),
            self.closed,
            self.worst_pct,
            self.rugs_traded,
            self.filtered_done,
            self.rugs_filtered,
            self.followed.len()
        )
    }
}

// ----- parsing -----

fn num(v: &Value) -> f64 {
    match v {
        Value::Number(n) => n.as_f64().unwrap_or(0.0),
        Value::String(s) => s.parse().unwrap_or(0.0),
        _ => 0.0,
    }
}

/// The most liquid pair per base token from a `tokens/v1` response.
pub fn parse_pairs(body: &Value) -> Vec<Pair> {
    let mut best: HashMap<String, Pair> = HashMap::new();
    for p in body.as_array().into_iter().flatten() {
        let pair = Pair {
            chain: p["chainId"].as_str().unwrap_or_default().to_string(),
            token: p["baseToken"]["address"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
            symbol: p["baseToken"]["symbol"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
            price: num(&p["priceUsd"]),
            liquidity: num(&p["liquidity"]["usd"]),
            volume_h1: num(&p["volume"]["h1"]),
            volume_h6: num(&p["volume"]["h6"]),
            buys_h1: num(&p["txns"]["h1"]["buys"]),
            sells_h1: num(&p["txns"]["h1"]["sells"]),
            change_h1: num(&p["priceChange"]["h1"]),
            created_ms: p["pairCreatedAt"].as_u64().unwrap_or(0),
        };
        if pair.token.is_empty() {
            continue;
        }
        let key = State::key(&pair.chain, &pair.token);
        if best.get(&key).is_none_or(|b| pair.liquidity > b.liquidity) {
            best.insert(key, pair);
        }
    }
    best.into_values().collect()
}

pub fn parse_rug(body: &Value) -> Option<Rug> {
    body.get("risks")?;
    let amm: HashSet<&str> = body["knownAccounts"]
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(_, v)| v["type"].as_str() == Some("AMM"))
        .map(|(k, _)| k.as_str())
        .collect();
    let top10_pct = body["topHolders"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|h| {
            !amm.contains(h["address"].as_str().unwrap_or_default())
                && !amm.contains(h["owner"].as_str().unwrap_or_default())
        })
        .take(10)
        .map(|h| num(&h["pct"]))
        .sum();
    let lp_locked_pct = body["markets"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|m| num(&m["lp"]["lpLockedPct"]))
        .fold(0.0, f64::max);
    Some(Rug {
        danger: body["risks"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|r| r["level"].as_str() == Some("danger")),
        mint_authority: !body["mintAuthority"].is_null(),
        freeze_authority: !body["freezeAuthority"].is_null(),
        lp_locked_pct,
        top10_pct,
        creator_pct: {
            let supply = num(&body["token"]["supply"]);
            if supply > 0.0 {
                100.0 * num(&body["creatorBalance"]) / supply
            } else {
                0.0
            }
        },
        insiders: num(&body["graphInsidersDetected"]) > 0.0
            || body["insiderNetworks"]
                .as_array()
                .is_some_and(|n| !n.is_empty()),
    })
}

// ----- the loop -----

fn dir() -> PathBuf {
    PathBuf::from(std::env::var("MEME_DIR").unwrap_or_else(|_| "/var/lib/tempest/meme".into()))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

async fn get(http: &reqwest::Client, url: &str) -> Result<Value, String> {
    let resp = http.get(url).send().await.map_err(|e| e.to_string())?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("{url}: HTTP {status}"));
    }
    resp.json().await.map_err(|e| format!("{url}: {e}"))
}

/// Solana and Base tokens from the profile and boost lists.
async fn candidates(http: &reqwest::Client) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for path in [
        "/token-profiles/latest/v1",
        "/token-boosts/latest/v1",
        "/token-boosts/top/v1",
    ] {
        match get(http, &format!("{DEX}{path}")).await {
            Ok(list) => {
                for t in list.as_array().into_iter().flatten() {
                    let chain = t["chainId"].as_str().unwrap_or_default();
                    let token = t["tokenAddress"].as_str().unwrap_or_default();
                    if CHAINS.contains(&chain)
                        && !token.is_empty()
                        && seen.insert(State::key(chain, token))
                    {
                        out.push((chain.to_string(), token.to_string()));
                    }
                }
            }
            Err(e) => warn!("meme: {e}"),
        }
    }
    out
}

fn append(path: &Path, header: &str, row: &str) {
    let new = !path.exists();
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        Ok(mut f) => {
            if new {
                let _ = writeln!(f, "{header}");
            }
            let _ = writeln!(f, "{row}");
        }
        Err(e) => error!("meme: cannot write {}: {e}", path.display()),
    }
}

fn csv_safe(s: &str) -> String {
    s.replace([',', '\n', '"'], " ")
}

// ----- v2: safety checks, size by liquidity, staged exits -----

/// v2 polls every minute; v1 keeps its 5-minute rounds as the reference.
const V2_ROUND: Duration = Duration::from_secs(60);
const V1_EVERY: u64 = 5;
const V2_MAX_TAX: f64 = 0.05;
/// Buy-then-sell quote losing more than this: too costly or not sellable.
const V2_MAX_ROUND_TRIP: f64 = 0.10;
const V2_MAX_CREATOR_PCT: f64 = 5.0;
/// Priority fee and Jito tip (Solana) or gas (Base), per side.
const V2_EXEC_COST: f64 = 0.005;
const V2_MIN_COST_EACH_WAY: f64 = 0.01;
const V2_TP1: f64 = 0.50;
const V2_TP2: f64 = 2.00;
const V2_TRAIL: f64 = 0.25;

/// What the pre-trade safety checks found for a token.
#[derive(Debug, Clone, PartialEq)]
pub struct Safety {
    /// A sell route exists and the sell simulation succeeded.
    pub sellable: bool,
    /// Share lost buying the paper size and selling it straight back.
    pub round_trip_loss: f64,
    pub buy_tax: f64,
    pub sell_tax: f64,
    /// Creator (dev) wallet's share of supply, percent; 0 when unknown.
    pub creator_pct: f64,
    /// RugCheck found an insider network (wallets funded together).
    pub insiders: bool,
}

/// Paper size: half a percent of the pool, between 20 and 100 USD.
pub fn v2_size(liquidity: f64) -> f64 {
    (liquidity * 0.005).clamp(20.0, PAPER_SIZE_USD)
}

/// v1's rules, then v2's safety checks.
pub fn v2_check(
    pair: &Pair,
    rug: Option<&Rug>,
    safety: Option<&Safety>,
    now_ms: u64,
) -> Option<&'static str> {
    if let Some(reason) = entry_check(pair, rug, now_ms) {
        return Some(reason);
    }
    let Some(s) = safety else {
        return Some("no safety check");
    };
    if !s.sellable {
        return Some("honeypot");
    }
    if s.buy_tax > V2_MAX_TAX || s.sell_tax > V2_MAX_TAX {
        return Some("tax");
    }
    if s.round_trip_loss > V2_MAX_ROUND_TRIP {
        return Some("round trip cost");
    }
    if s.creator_pct > V2_MAX_CREATOR_PCT {
        return Some("dev holds too much");
    }
    if s.insiders {
        return Some("insider cluster");
    }
    None
}

/// Cost per side: half the quoted round trip plus execution, never under 1 %,
/// plus that side's token tax.
pub fn v2_costs(s: &Safety) -> (f64, f64) {
    let base = (s.round_trip_loss / 2.0 + V2_EXEC_COST).max(V2_MIN_COST_EACH_WAY);
    (base + s.buy_tax, base + s.sell_tax)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Position2 {
    pub chain: String,
    pub token: String,
    pub symbol: String,
    pub entry_ms: u64,
    pub quote: f64,
    pub size_usd: f64,
    pub tokens: f64,
    pub cost_out: f64,
    /// Share of the tokens still held.
    pub left: f64,
    pub proceeds: f64,
    pub peak: f64,
    pub liquidity0: f64,
    pub min_liquidity: f64,
    pub min_price: f64,
}

impl Position2 {
    fn sell(&mut self, share: f64, price: f64) {
        let share = share.min(self.left);
        self.proceeds += self.tokens * share * price * (1.0 - self.cost_out);
        self.left -= share;
    }

    /// Staged exits: half at +50 %, then the rest at +200 %, on a 25 % drop
    /// from the peak, or back at the entry; the whole at -30 % or after 24 h.
    /// Returns the reason when the position is fully closed.
    pub fn update(&mut self, price: f64, now_ms: u64) -> Option<&'static str> {
        self.peak = self.peak.max(price);
        let change = price / self.quote - 1.0;
        let full = self.left >= 1.0;
        let reason = if change <= STOP {
            Some("stop")
        } else if full && change >= V2_TP1 {
            self.sell(0.5, price);
            None
        } else if !full && change >= V2_TP2 {
            Some("tp2")
        } else if !full && price <= self.peak * (1.0 - V2_TRAIL) {
            Some("trail")
        } else if !full && change <= 0.0 {
            Some("breakeven")
        } else {
            None
        };
        let reason = reason
            .or_else(|| (now_ms.saturating_sub(self.entry_ms) >= MAX_HOLD_MS).then_some("time"));
        if reason.is_some() {
            let left = self.left;
            self.sell(left, price);
        }
        reason
    }

    pub fn net_pct(&self) -> f64 {
        (self.proceeds / self.size_usd - 1.0) * 100.0
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Closed2 {
    pub pos: Position2,
    pub exit_ms: u64,
    pub reason: &'static str,
    pub rug: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Book2 {
    pub started_ms: u64,
    pub open: Vec<Position2>,
    pub followed: Vec<Followed>,
    pub judged: HashSet<String>,
    pub closed: u32,
    pub wins: u32,
    pub pnl_usd: f64,
    pub staked_usd: f64,
    pub worst_pct: f64,
    pub rugs_traded: u32,
    pub filtered_done: u32,
    pub rugs_filtered: u32,
}

/// Move followed tokens on by one round; returns those whose 24 h are up and
/// the number of rugs among them.
fn advance_followed(
    followed: &mut Vec<Followed>,
    pairs: &HashMap<String, Pair>,
    now_ms: u64,
) -> (Vec<Followed>, u32) {
    let mut done = Vec::new();
    let mut rugs = 0;
    let mut following = Vec::new();
    for mut f in std::mem::take(followed) {
        let key = State::key(&f.chain, &f.token);
        let (price, liquidity) = pairs
            .get(&key)
            .map_or((0.0, 0.0), |p| (p.price, p.liquidity));
        f.min_price = f.min_price.min(price);
        f.min_liquidity = f.min_liquidity.min(liquidity);
        for (slot, after) in FOLLOW_UPS_MS.iter().zip(f.after.iter_mut()) {
            if after.is_none() && now_ms.saturating_sub(f.seen_ms) >= *slot {
                *after = Some(price);
            }
        }
        if f.after[2].is_some() {
            if is_rug(f.price0, f.liquidity0, f.min_price, f.min_liquidity) {
                rugs += 1;
            }
            done.push(f);
        } else {
            following.push(f);
        }
    }
    *followed = following;
    (done, rugs)
}

fn followed_from(pair: &Pair, reason: &str, now_ms: u64) -> Followed {
    Followed {
        chain: pair.chain.clone(),
        token: pair.token.clone(),
        symbol: pair.symbol.clone(),
        reason: reason.to_string(),
        seen_ms: now_ms,
        price0: pair.price,
        liquidity0: pair.liquidity,
        min_price: pair.price,
        min_liquidity: pair.liquidity,
        after: [None; 3],
    }
}

impl Book2 {
    pub fn step(
        &mut self,
        candidates: &[(String, String)],
        pairs: &HashMap<String, Pair>,
        rugs: &HashMap<String, Rug>,
        safety: &HashMap<String, Safety>,
        now_ms: u64,
    ) -> (Vec<Closed2>, Vec<Followed>) {
        if self.started_ms == 0 {
            self.started_ms = now_ms;
        }
        let mut closed = Vec::new();
        let mut still_open = Vec::new();
        for mut pos in std::mem::take(&mut self.open) {
            let key = State::key(&pos.chain, &pos.token);
            let (price, liquidity) = pairs
                .get(&key)
                .map_or((0.0, 0.0), |p| (p.price, p.liquidity));
            pos.min_price = pos.min_price.min(price);
            pos.min_liquidity = pos.min_liquidity.min(liquidity);
            let reason = if pairs.contains_key(&key) {
                pos.update(price, now_ms)
            } else {
                pos.left = 0.0;
                Some("vanished")
            };
            match reason {
                Some(reason) => {
                    let rug = is_rug(pos.quote, pos.liquidity0, pos.min_price, pos.min_liquidity);
                    let pct = pos.net_pct();
                    self.closed += 1;
                    if pct > 0.0 {
                        self.wins += 1;
                    }
                    self.pnl_usd += pos.proceeds - pos.size_usd;
                    self.staked_usd += pos.size_usd;
                    self.worst_pct = self.worst_pct.min(pct);
                    if rug {
                        self.rugs_traded += 1;
                    }
                    closed.push(Closed2 {
                        pos,
                        exit_ms: now_ms,
                        reason,
                        rug,
                    });
                }
                None => still_open.push(pos),
            }
        }
        self.open = still_open;

        let (done, rugs_done) = advance_followed(&mut self.followed, pairs, now_ms);
        self.filtered_done += done.len() as u32;
        self.rugs_filtered += rugs_done;

        for (chain, token) in candidates {
            let key = State::key(chain, token);
            if self.judged.contains(&key) {
                continue;
            }
            let Some(pair) = pairs.get(&key) else {
                continue;
            };
            let rug = rugs.get(&key);
            if pair.chain == "solana" && rug.is_none() {
                continue;
            }
            let base = entry_check(pair, rug, now_ms);
            let s = safety.get(&key);
            // A pair that passes v1's rules waits for its safety check.
            if base.is_none() && s.is_none() {
                continue;
            }
            match v2_check(pair, rug, s, now_ms) {
                Some("too new") => {}
                Some(reason) => {
                    self.judged.insert(key);
                    self.followed.push(followed_from(pair, reason, now_ms));
                }
                None => {
                    let s = s.expect("checked above");
                    let (cost_in, cost_out) = v2_costs(s);
                    let size = v2_size(pair.liquidity);
                    self.judged.insert(key);
                    self.open.push(Position2 {
                        chain: pair.chain.clone(),
                        token: pair.token.clone(),
                        symbol: pair.symbol.clone(),
                        entry_ms: now_ms,
                        quote: pair.price,
                        size_usd: size,
                        tokens: size * (1.0 - cost_in) / pair.price,
                        cost_out,
                        left: 1.0,
                        proceeds: 0.0,
                        peak: pair.price,
                        liquidity0: pair.liquidity,
                        min_liquidity: pair.liquidity,
                        min_price: pair.price,
                    });
                }
            }
        }
        (closed, done)
    }

    pub fn summary(&self, now_ms: u64) -> String {
        let days = now_ms.saturating_sub(self.started_ms) as f64 / 86_400_000.0;
        let hit = if self.closed > 0 {
            100.0 * f64::from(self.wins) / f64::from(self.closed)
        } else {
            0.0
        };
        let ret = if self.staked_usd > 0.0 {
            100.0 * self.pnl_usd / self.staked_usd
        } else {
            0.0
        };
        format!(
            "v2: day {days:.1} | {} open | {} closed, hit {hit:.0} %, P&L {:+.2} USD ({ret:+.1} % of stakes) after costs, \
             worst {:.1} %, rugs traded {} | filtered followed 24 h: {}, of which rugs {} | following {}",
            self.open.len(),
            self.closed,
            self.pnl_usd,
            self.worst_pct,
            self.rugs_traded,
            self.filtered_done,
            self.rugs_filtered,
            self.followed.len()
        )
    }
}

const JUPITER: &str = "https://lite-api.jup.ag/swap/v1/quote";
const USDC_SOL: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
const HONEYPOT: &str = "https://api.honeypot.is/v2/IsHoneypot";

/// Solana: quote the paper size in through Jupiter and straight back out.
/// No sell route is a honeypot.
async fn solana_safety(http: &reqwest::Client, mint: &str, size: f64, rug: &Rug) -> Option<Safety> {
    let amount = (size * 1e6) as u64;
    let buy = get(
        http,
        &format!(
            "{JUPITER}?inputMint={USDC_SOL}&outputMint={mint}&amount={amount}&slippageBps=300"
        ),
    )
    .await
    .ok()?;
    let got = buy["outAmount"].as_str()?.to_string();
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    let sell = get(
        http,
        &format!("{JUPITER}?inputMint={mint}&outputMint={USDC_SOL}&amount={got}&slippageBps=300"),
    )
    .await;
    let (sellable, back) = match sell {
        Ok(q) => (
            q["routePlan"].as_array().is_some_and(|r| !r.is_empty()),
            num(&q["outAmount"]) / 1e6,
        ),
        // Jupiter answers 400 when no route sells the token.
        Err(e) if e.contains("HTTP 400") => (false, 0.0),
        Err(_) => return None,
    };
    Some(Safety {
        sellable,
        round_trip_loss: (1.0 - back / size).clamp(0.0, 1.0),
        buy_tax: 0.0,
        sell_tax: 0.0,
        creator_pct: rug.creator_pct,
        insiders: rug.insiders,
    })
}

/// Base: honeypot.is simulates a buy and a sell and reports the taxes.
async fn base_safety(
    http: &reqwest::Client,
    token: &str,
    size: f64,
    liquidity: f64,
) -> Option<Safety> {
    let r = get(http, &format!("{HONEYPOT}?address={token}&chainID=8453"))
        .await
        .ok()?;
    let simulated = r["simulationSuccess"].as_bool().unwrap_or(false);
    let honeypot = r["honeypotResult"]["isHoneypot"].as_bool().unwrap_or(true);
    let impact = if liquidity > 0.0 {
        size / liquidity
    } else {
        1.0
    };
    Some(Safety {
        sellable: simulated && !honeypot,
        // A 0.3 % pool fee and the size's share of the pool, both ways.
        round_trip_loss: 2.0 * (0.003 + impact),
        buy_tax: num(&r["simulationResult"]["buyTax"]) / 100.0,
        sell_tax: num(&r["simulationResult"]["sellTax"]) / 100.0,
        creator_pct: 0.0,
        insiders: false,
    })
}

#[derive(Default, Serialize, Deserialize)]
struct Saved {
    v1: State,
    v2: Book2,
}

pub async fn run() {
    let dir = dir();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        error!("meme: cannot create {}: {e}", dir.display());
        return;
    }
    let state_path = dir.join("state.json");
    let mut saved: Saved = std::fs::read(&state_path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .user_agent("tempest-meme-paper")
        .build()
        .expect("http client");
    let mut rug_cache: HashMap<String, (u64, Rug)> = HashMap::new();
    let mut safety_cache: HashMap<String, (u64, Safety)> = HashMap::new();
    info!(
        "meme paper test (v1 + v2) starting, files in {}",
        dir.display()
    );

    let mut round: u64 = 0;
    loop {
        let started = std::time::Instant::now();
        let now = now_ms();
        let v1_round = round.is_multiple_of(V1_EVERY);
        round += 1;
        let fresh = candidates(&http).await;
        let mut wanted: Vec<(String, String)> = fresh.clone();
        let tracked = |c: &String, t: &String| (c.clone(), t.clone());
        wanted.extend(saved.v1.open.iter().map(|p| tracked(&p.chain, &p.token)));
        wanted.extend(
            saved
                .v1
                .followed
                .iter()
                .map(|f| tracked(&f.chain, &f.token)),
        );
        wanted.extend(saved.v2.open.iter().map(|p| tracked(&p.chain, &p.token)));
        wanted.extend(
            saved
                .v2
                .followed
                .iter()
                .map(|f| tracked(&f.chain, &f.token)),
        );
        let mut uniq = HashSet::new();
        wanted.retain(|(c, t)| uniq.insert(State::key(c, t)));

        let mut pairs = HashMap::new();
        let mut failed_fetch = false;
        for chain in CHAINS {
            let tokens: Vec<&str> = wanted
                .iter()
                .filter(|(c, _)| c == chain)
                .map(|(_, t)| t.as_str())
                .collect();
            for chunk in tokens.chunks(BATCH) {
                match get(
                    &http,
                    &format!("{DEX}/tokens/v1/{chain}/{}", chunk.join(",")),
                )
                .await
                {
                    Ok(body) => {
                        for p in parse_pairs(&body) {
                            if p.chain == chain {
                                pairs.insert(State::key(&p.chain, &p.token), p);
                            }
                        }
                    }
                    Err(e) => {
                        warn!("meme: {e}");
                        failed_fetch = true;
                    }
                }
                // Well inside 300 calls a minute.
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
        }
        // A failed fetch would read as every token vanishing: skip the round.
        if failed_fetch {
            tokio::time::sleep(V2_ROUND).await;
            continue;
        }

        let mut rugs = HashMap::new();
        for (chain, token) in &fresh {
            let key = State::key(chain, token);
            let judged = saved.v1.judged.contains(&key) && saved.v2.judged.contains(&key);
            if chain != "solana" || judged || !pairs.contains_key(&key) {
                continue;
            }
            if let Some((at, rug)) = rug_cache.get(&key) {
                if now.saturating_sub(*at) < 30 * 60_000 {
                    rugs.insert(key, rug.clone());
                    continue;
                }
            }
            match get(&http, &format!("{RUGCHECK}/{token}/report")).await {
                Ok(body) => {
                    if let Some(rug) = parse_rug(&body) {
                        rug_cache.insert(key.clone(), (now, rug.clone()));
                        rugs.insert(key, rug);
                    }
                }
                Err(e) => warn!("meme: rugcheck {e}"),
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        rug_cache.retain(|_, (at, _)| now.saturating_sub(*at) < 3_600_000);

        // v2 safety checks, only for pairs that pass v1's rules.
        let mut safety = HashMap::new();
        for (chain, token) in &fresh {
            let key = State::key(chain, token);
            if saved.v2.judged.contains(&key) {
                continue;
            }
            let Some(pair) = pairs.get(&key) else {
                continue;
            };
            if entry_check(pair, rugs.get(&key), now).is_some() {
                continue;
            }
            if let Some((at, s)) = safety_cache.get(&key) {
                if now.saturating_sub(*at) < 10 * 60_000 {
                    safety.insert(key, s.clone());
                    continue;
                }
            }
            let size = v2_size(pair.liquidity);
            let found = match (chain.as_str(), rugs.get(&key)) {
                ("solana", Some(rug)) => solana_safety(&http, token, size, rug).await,
                ("base", _) => base_safety(&http, token, size, pair.liquidity).await,
                _ => None,
            };
            if let Some(s) = found {
                safety_cache.insert(key.clone(), (now, s.clone()));
                safety.insert(key, s);
            }
            tokio::time::sleep(Duration::from_millis(1_100)).await;
        }
        safety_cache.retain(|_, (at, _)| now.saturating_sub(*at) < 3_600_000);

        if v1_round {
            let (closed, done) = saved.v1.step(&fresh, &pairs, &rugs, now);
            write_v1(&dir, &closed, &done);
        }
        let (closed2, done2) = saved.v2.step(&fresh, &pairs, &rugs, &safety, now);
        write_v2(&dir, &closed2, &done2);

        match serde_json::to_vec(&saved) {
            Ok(bytes) => {
                let tmp = dir.join("state.json.tmp");
                if std::fs::write(&tmp, bytes)
                    .and_then(|_| std::fs::rename(&tmp, &state_path))
                    .is_err()
                {
                    error!("meme: state not saved");
                }
            }
            Err(e) => error!("meme: {e}"),
        }
        let summary = format!("{}\n{}", saved.v1.summary(now), saved.v2.summary(now));
        let _ = std::fs::write(dir.join("summary.txt"), format!("{summary}\n"));
        let took = started.elapsed();
        // How far behind the stream the bot runs: a round that takes longer
        // than its interval is lagging.
        let lag = if took > V2_ROUND { " LAGGING" } else { "" };
        let _ = std::fs::write(
            dir.join("round.txt"),
            format!(
                "last round {:.1} s of {} s{lag}\n",
                took.as_secs_f64(),
                V2_ROUND.as_secs()
            ),
        );
        info!(
            "{summary} | round: {} candidates, {} pairs, {:.1} s{lag}",
            fresh.len(),
            pairs.len(),
            took.as_secs_f64()
        );
        tokio::time::sleep(V2_ROUND.saturating_sub(took)).await;
    }
}

fn write_v1(dir: &Path, closed: &[Closed], done: &[Followed]) {
    for c in closed {
        info!(
            "meme v1: closed {} {} ({}) {:+.1} %{}",
            c.pos.chain,
            c.pos.symbol,
            c.reason,
            c.net_pct,
            if c.rug { " RUG" } else { "" }
        );
        append(
            &dir.join("trades.csv"),
            "chain,token,symbol,entry_ms,exit_ms,entry_quote,exit_quote,paid,received,reason,net_pct,rug,liquidity0",
            &format!(
                "{},{},{},{},{},{},{},{},{},{},{:.2},{},{:.0}",
                c.pos.chain,
                c.pos.token,
                csv_safe(&c.pos.symbol),
                c.pos.entry_ms,
                c.exit_ms,
                c.pos.quote,
                c.exit_quote,
                c.pos.paid,
                c.received,
                c.reason,
                c.net_pct,
                c.rug,
                c.pos.liquidity0
            ),
        );
    }
    write_followed(&dir.join("filtered.csv"), done);
}

fn write_v2(dir: &Path, closed: &[Closed2], done: &[Followed]) {
    for c in closed {
        let p = &c.pos;
        info!(
            "meme v2: closed {} {} ({}) {:+.1} %{}",
            p.chain,
            p.symbol,
            c.reason,
            p.net_pct(),
            if c.rug { " RUG" } else { "" }
        );
        append(
            &dir.join("trades_v2.csv"),
            "chain,token,symbol,entry_ms,exit_ms,entry_quote,peak,size_usd,proceeds_usd,reason,net_pct,rug,liquidity0",
            &format!(
                "{},{},{},{},{},{},{},{:.2},{:.2},{},{:.2},{},{:.0}",
                p.chain,
                p.token,
                csv_safe(&p.symbol),
                p.entry_ms,
                c.exit_ms,
                p.quote,
                p.peak,
                p.size_usd,
                p.proceeds,
                c.reason,
                p.net_pct(),
                c.rug,
                p.liquidity0
            ),
        );
    }
    write_followed(&dir.join("filtered_v2.csv"), done);
}

fn write_followed(path: &Path, done: &[Followed]) {
    for f in done {
        let pct = |p: Option<f64>| {
            p.filter(|_| f.price0 > 0.0).map_or(String::new(), |p| {
                format!("{:.1}", (p / f.price0 - 1.0) * 100.0)
            })
        };
        append(
            path,
            "chain,token,symbol,seen_ms,reason,price0,liquidity0,chg_1h_pct,chg_6h_pct,chg_24h_pct,rug",
            &format!(
                "{},{},{},{},{},{},{:.0},{},{},{},{}",
                f.chain,
                f.token,
                csv_safe(&f.symbol),
                f.seen_ms,
                f.reason,
                f.price0,
                f.liquidity0,
                pct(f.after[0]),
                pct(f.after[1]),
                pct(f.after[2]),
                is_rug(f.price0, f.liquidity0, f.min_price, f.min_liquidity)
            ),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: u64 = 3_600_000;

    fn good(chain: &str) -> Pair {
        Pair {
            chain: chain.into(),
            token: "T".into(),
            symbol: "MEME".into(),
            price: 1.0,
            liquidity: 50_000.0,
            volume_h1: 30_000.0,
            volume_h6: 60_000.0,
            buys_h1: 150.0,
            sells_h1: 100.0,
            change_h1: 20.0,
            created_ms: 0,
        }
    }

    fn safe() -> Rug {
        Rug {
            danger: false,
            mint_authority: false,
            freeze_authority: false,
            lp_locked_pct: 100.0,
            top10_pct: 20.0,
            creator_pct: 0.0,
            insiders: false,
        }
    }

    fn at(p: Pair, created: u64) -> Pair {
        Pair {
            created_ms: created,
            ..p
        }
    }

    #[test]
    fn a_pair_passing_every_rule_enters() {
        let now = 10 * HOUR;
        assert_eq!(
            entry_check(&at(good("base"), now - 2 * HOUR), None, now),
            None
        );
        assert_eq!(
            entry_check(&at(good("solana"), now - 2 * HOUR), Some(&safe()), now),
            None
        );
    }

    #[test]
    fn each_rule_names_itself() {
        let now = 100 * HOUR;
        let born = now - 2 * HOUR;
        let check = |p: Pair| entry_check(&at(p, born), Some(&safe()), now);
        assert_eq!(
            entry_check(&at(good("base"), now - 60_000), None, now),
            Some("too new")
        );
        assert_eq!(
            entry_check(&at(good("base"), now - 80 * HOUR), None, now),
            Some("too old")
        );
        assert_eq!(
            check(Pair {
                liquidity: 5_000.0,
                ..good("base")
            }),
            Some("liquidity")
        );
        assert_eq!(
            check(Pair {
                volume_h1: 15_000.0,
                ..good("base")
            }),
            Some("volume not rising")
        );
        assert_eq!(
            check(Pair {
                buys_h1: 100.0,
                ..good("base")
            }),
            Some("buy/sell")
        );
        assert_eq!(
            check(Pair {
                change_h1: 150.0,
                ..good("base")
            }),
            Some("price change")
        );
        assert_eq!(
            check(Pair {
                change_h1: -5.0,
                ..good("base")
            }),
            Some("price change")
        );
        let sol = at(good("solana"), born);
        assert_eq!(entry_check(&sol, None, now), Some("no rugcheck"));
        let r = |rug: Rug| entry_check(&sol, Some(&rug), now);
        assert_eq!(
            r(Rug {
                danger: true,
                ..safe()
            }),
            Some("rugcheck danger")
        );
        assert_eq!(
            r(Rug {
                mint_authority: true,
                ..safe()
            }),
            Some("authority")
        );
        assert_eq!(
            r(Rug {
                lp_locked_pct: 50.0,
                ..safe()
            }),
            Some("lp not locked")
        );
        assert_eq!(
            r(Rug {
                top10_pct: 45.0,
                ..safe()
            }),
            Some("holders")
        );
    }

    #[test]
    fn costs_go_against_us_both_ways() {
        // 2 % plus 100 / 50 000 = 0.2 %.
        assert!((fill(1.0, 50_000.0, true) - 1.022).abs() < 1e-12);
        assert!((fill(1.0, 50_000.0, false) - 0.978).abs() < 1e-12);
    }

    fn feed(price: f64, liquidity: f64) -> HashMap<String, Pair> {
        let p = Pair {
            price,
            liquidity,
            ..good("base")
        };
        HashMap::from([(State::key("base", "T"), p)])
    }

    #[test]
    fn a_paper_trade_opens_once_and_closes_at_the_target_after_costs() {
        let mut s = State::default();
        let cands = vec![("base".to_string(), "T".to_string())];
        let now = 10 * HOUR;
        let mut pairs = feed(1.0, 50_000.0);
        for p in pairs.values_mut() {
            p.created_ms = now - 2 * HOUR;
        }
        let (c, _) = s.step(&cands, &pairs, &HashMap::new(), now);
        assert!(c.is_empty());
        assert_eq!(s.open.len(), 1);
        // Seen again: not entered twice.
        s.step(&cands, &pairs, &HashMap::new(), now + 300_000);
        assert_eq!(s.open.len(), 1);
        let (c, _) = s.step(&cands, &feed(1.6, 50_000.0), &HashMap::new(), now + HOUR);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].reason, "target");
        // 1.6 x 0.978 / 1.022 - 1 = +53.1 %.
        assert!((c[0].net_pct - 53.11).abs() < 0.01, "{}", c[0].net_pct);
        assert!(!c[0].rug);
        assert_eq!((s.closed, s.wins), (1, 1));
    }

    #[test]
    fn a_pulled_pool_is_a_rug_and_a_vanished_token_is_a_total_loss() {
        let mut s = State::default();
        let now = 10 * HOUR;
        s.open.push(Position {
            chain: "base".into(),
            token: "T".into(),
            symbol: "MEME".into(),
            entry_ms: now,
            quote: 1.0,
            paid: 1.022,
            liquidity0: 50_000.0,
            min_liquidity: 50_000.0,
            min_price: 1.0,
        });
        let (c, _) = s.step(&[], &feed(0.05, 2_000.0), &HashMap::new(), now + HOUR);
        assert_eq!(c[0].reason, "stop");
        assert!(c[0].rug);
        let mut s2 = State::default();
        s2.open.push(c[0].pos.clone());
        let (c, _) = s2.step(&[], &HashMap::new(), &HashMap::new(), now + HOUR);
        assert_eq!(c[0].reason, "vanished");
        assert!((c[0].net_pct + 100.0).abs() < 1e-9);
    }

    #[test]
    fn filtered_tokens_are_followed_for_a_day() {
        let mut s = State::default();
        let cands = vec![("base".to_string(), "T".to_string())];
        let now = 100 * HOUR;
        let mut pairs = feed(1.0, 5_000.0); // fails on liquidity
        for p in pairs.values_mut() {
            p.created_ms = now - 2 * HOUR;
        }
        s.step(&cands, &pairs, &HashMap::new(), now);
        assert_eq!(s.followed.len(), 1);
        assert_eq!(s.followed[0].reason, "liquidity");
        s.step(&cands, &feed(2.0, 5_000.0), &HashMap::new(), now + HOUR);
        s.step(&cands, &feed(0.5, 5_000.0), &HashMap::new(), now + 6 * HOUR);
        let (_, done) = s.step(&cands, &feed(0.05, 300.0), &HashMap::new(), now + 24 * HOUR);
        assert_eq!(done.len(), 1);
        assert_eq!(done[0].after, [Some(2.0), Some(0.5), Some(0.05)]);
        assert_eq!((s.filtered_done, s.rugs_filtered), (1, 1));
        assert!(s.followed.is_empty() && s.open.is_empty());
    }

    #[test]
    fn a_too_new_token_is_judged_again_later() {
        let mut s = State::default();
        let cands = vec![("base".to_string(), "T".to_string())];
        let now = 100 * HOUR;
        let mut pairs = feed(1.0, 50_000.0);
        for p in pairs.values_mut() {
            p.created_ms = now - 10 * 60_000;
        }
        s.step(&cands, &pairs, &HashMap::new(), now);
        assert!(s.open.is_empty() && s.followed.is_empty());
        s.step(&cands, &pairs, &HashMap::new(), now + HOUR);
        assert_eq!(s.open.len(), 1);
    }

    #[test]
    fn parses_dexscreener_and_rugcheck() {
        let pairs: Value = serde_json::json!([
            {"chainId": "solana", "baseToken": {"address": "M", "symbol": "X"}, "priceUsd": "0.5",
             "liquidity": {"usd": 1000.0}, "volume": {"h1": 1, "h6": 2},
             "txns": {"h1": {"buys": 3, "sells": 4}}, "priceChange": {"h1": 5}, "pairCreatedAt": 7},
            {"chainId": "solana", "baseToken": {"address": "M", "symbol": "X"}, "priceUsd": "0.6",
             "liquidity": {"usd": 9000.0}, "pairCreatedAt": 8}
        ]);
        let p = parse_pairs(&pairs);
        assert_eq!(p.len(), 1);
        assert_eq!(
            (p[0].price, p[0].liquidity, p[0].created_ms),
            (0.6, 9000.0, 8)
        );
        let rug: Value = serde_json::json!({
            "risks": [{"name": "x", "level": "warn"}],
            "mintAuthority": null, "freezeAuthority": "F",
            "knownAccounts": {"POOL": {"type": "AMM"}},
            "topHolders": [{"address": "A", "owner": "POOL", "pct": 40.0},
                           {"address": "B", "owner": "C", "pct": 5.0},
                           {"address": "D", "owner": "E", "pct": 3.0}],
            "markets": [{"lp": {"lpLockedPct": 0}}, {"lp": {"lpLockedPct": 100}}]
        });
        let r = parse_rug(&rug).unwrap();
        assert!(!r.danger && !r.mint_authority && r.freeze_authority);
        assert!((r.top10_pct - 8.0).abs() < 1e-12);
        assert_eq!(r.lp_locked_pct, 100.0);
        assert!(parse_rug(&serde_json::json!({"error": "not found"})).is_none());
    }

    fn safe2() -> Safety {
        Safety {
            sellable: true,
            round_trip_loss: 0.02,
            buy_tax: 0.0,
            sell_tax: 0.0,
            creator_pct: 1.0,
            insiders: false,
        }
    }

    #[test]
    fn v2_adds_safety_checks_on_top_of_v1() {
        let now = 10 * HOUR;
        let p = at(good("base"), now - 2 * HOUR);
        assert_eq!(v2_check(&p, None, Some(&safe2()), now), None);
        assert_eq!(v2_check(&p, None, None, now), Some("no safety check"));
        let c = |s: Safety| v2_check(&p, None, Some(&s), now);
        assert_eq!(
            c(Safety {
                sellable: false,
                ..safe2()
            }),
            Some("honeypot")
        );
        assert_eq!(
            c(Safety {
                sell_tax: 0.10,
                ..safe2()
            }),
            Some("tax")
        );
        assert_eq!(
            c(Safety {
                round_trip_loss: 0.2,
                ..safe2()
            }),
            Some("round trip cost")
        );
        assert_eq!(
            c(Safety {
                creator_pct: 9.0,
                ..safe2()
            }),
            Some("dev holds too much")
        );
        assert_eq!(
            c(Safety {
                insiders: true,
                ..safe2()
            }),
            Some("insider cluster")
        );
        // v1's own rules still come first.
        let thin = Pair {
            liquidity: 1_000.0,
            ..p
        };
        assert_eq!(
            v2_check(&thin, None, Some(&safe2()), now),
            Some("liquidity")
        );
    }

    #[test]
    fn v2_costs_and_size_follow_the_quote_and_the_pool() {
        // 2 % round trip: 1 % + 0.5 % per side, above the 1 % floor.
        let (i, o) = v2_costs(&safe2());
        assert!((i - 0.015).abs() < 1e-12 && (o - 0.015).abs() < 1e-12);
        let (i, o) = v2_costs(&Safety {
            round_trip_loss: 0.0,
            sell_tax: 0.03,
            ..safe2()
        });
        assert!((i - 0.01).abs() < 1e-12 && (o - 0.04).abs() < 1e-12);
        assert_eq!(v2_size(1_000_000.0), 100.0);
        assert_eq!(v2_size(10_000.0), 50.0);
        assert_eq!(v2_size(1_000.0), 20.0);
    }

    fn pos2() -> Position2 {
        Position2 {
            chain: "base".into(),
            token: "T".into(),
            symbol: "MEME".into(),
            entry_ms: 0,
            quote: 1.0,
            size_usd: 100.0,
            tokens: 100.0,
            cost_out: 0.0,
            left: 1.0,
            proceeds: 0.0,
            peak: 1.0,
            liquidity0: 50_000.0,
            min_liquidity: 50_000.0,
            min_price: 1.0,
        }
    }

    #[test]
    fn v2_takes_half_at_50_then_trails_the_rest() {
        let mut p = pos2();
        assert_eq!(p.update(1.6, HOUR), None);
        assert!((p.left - 0.5).abs() < 1e-12 && (p.proceeds - 80.0).abs() < 1e-9);
        assert_eq!(p.update(2.4, 2 * HOUR), None);
        // 25 % off the 2.4 peak.
        assert_eq!(p.update(1.75, 3 * HOUR), Some("trail"));
        assert!((p.proceeds - 167.5).abs() < 1e-9);
        assert!((p.net_pct() - 67.5).abs() < 1e-9);
        let mut p = pos2();
        p.update(1.6, HOUR);
        assert_eq!(p.update(3.1, 2 * HOUR), Some("tp2"));
        let mut p = pos2();
        p.update(1.6, HOUR);
        assert_eq!(p.update(1.0, 2 * HOUR), Some("trail"));
        let mut p = pos2();
        assert_eq!(p.update(0.69, HOUR), Some("stop"));
        assert_eq!(p.left, 0.0);
        let mut p = pos2();
        assert_eq!(p.update(1.1, 25 * HOUR), Some("time"));
    }

    #[test]
    fn v2_book_waits_for_the_safety_check_then_enters() {
        let mut b = Book2::default();
        let cands = vec![("base".to_string(), "T".to_string())];
        let now = 10 * HOUR;
        let mut pairs = feed(1.0, 50_000.0);
        for p in pairs.values_mut() {
            p.created_ms = now - 2 * HOUR;
        }
        b.step(&cands, &pairs, &HashMap::new(), &HashMap::new(), now);
        assert!(b.open.is_empty() && b.followed.is_empty());
        let safety = HashMap::from([(State::key("base", "T"), safe2())]);
        b.step(&cands, &pairs, &HashMap::new(), &safety, now + 60_000);
        assert_eq!(b.open.len(), 1);
        assert_eq!(b.open[0].size_usd, 100.0);
        let (c, _) = b.step(
            &cands,
            &HashMap::new(),
            &HashMap::new(),
            &safety,
            now + HOUR,
        );
        assert_eq!(c[0].reason, "vanished");
        assert!((b.pnl_usd + 100.0).abs() < 1e-9);
    }

    #[test]
    fn rugcheck_creator_share_and_insiders() {
        let rug: Value = serde_json::json!({
            "risks": [], "mintAuthority": null, "freezeAuthority": null,
            "token": {"supply": 1000.0}, "creatorBalance": 80.0,
            "graphInsidersDetected": 2, "topHolders": [], "markets": []
        });
        let r = parse_rug(&rug).unwrap();
        assert!((r.creator_pct - 8.0).abs() < 1e-12);
        assert!(r.insiders);
    }
}
