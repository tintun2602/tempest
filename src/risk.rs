use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{info, warn};

/// Default fraction of equity risked per trade. Overridable: this is the main
/// lever on return *and* drawdown, and the right setting depends entirely on
/// the operator's tolerance.
pub const DEFAULT_RISK_PER_TRADE: f64 = 0.015; // 1.5% of portfolio
pub const DEFAULT_MAX_OPEN_POSITIONS: usize = 4;
pub const DEFAULT_DAILY_DRAWDOWN_LIMIT: f64 = 0.05; // 5%
/// Largest share of free USDT a single entry may spend, leaving headroom for
/// fees and market-order slippage.
const MAX_NOTIONAL_FRACTION: f64 = 0.95;

#[derive(Debug, Clone)]
pub struct Position {
    pub symbol: String,
    pub entry_price: f64,
    pub quantity: f64,
    pub stop_loss: f64,
    pub take_profit: f64,
    pub entry_time: u64,
    /// Whether the exchange has *confirmed* a protective OCO for this position.
    ///
    /// Only ever set from a venue acknowledgement. When false, the exchange is
    /// holding nothing and `check_exits` is the only stop this position has —
    /// which requires the bot to be alive and polling.
    pub protected: bool,
    /// Highest price seen since entry, for the chandelier trailing stop.
    pub highest_high: f64,
    /// ATR at entry, held fixed so the stop ratchets on price alone and never
    /// loosens when volatility expands.
    pub atr_at_entry: f64,
}

pub struct RiskManager {
    pub positions: Vec<Position>,
    /// Fraction of equity risked on each entry.
    risk_per_trade: f64,
    max_open_positions: usize,
    /// Fraction of day-open equity that halts trading for the rest of the day.
    daily_drawdown_limit: f64,
    /// Total portfolio equity at the start of the current UTC day — the baseline
    /// the daily drawdown limit is measured against.
    pub day_open_equity: f64,
    /// When `day_open_equity` was measured. Deposits and withdrawals from then
    /// on are cash flows; anything earlier is already in the baseline.
    day_open_at_ms: u64,
    /// Today's deposits (positive) and withdrawals (negative) by transfer id,
    /// in quote currency, valued when first seen.
    cash_flows: HashMap<String, f64>,
    current_utc_day: u32,
    pub halted: bool,
}

/// A deposit or withdrawal, valued in the quote currency.
#[derive(Debug, Clone, PartialEq)]
pub struct CashFlow {
    pub id: String,
    pub time_ms: u64,
    /// Positive for money in, negative for money out.
    pub value: f64,
}

impl RiskManager {
    /// Risk-per-trade at the default fraction. Production paths read the
    /// configured value via [`Self::with_risk_per_trade`].
    #[cfg(test)]
    pub fn new(starting_equity: f64) -> Self {
        Self::with_risk_per_trade(starting_equity, DEFAULT_RISK_PER_TRADE)
    }

    pub fn with_risk_per_trade(starting_equity: f64, risk_per_trade: f64) -> Self {
        Self {
            positions: Vec::new(),
            risk_per_trade,
            max_open_positions: DEFAULT_MAX_OPEN_POSITIONS,
            daily_drawdown_limit: DEFAULT_DAILY_DRAWDOWN_LIMIT,
            day_open_equity: starting_equity,
            day_open_at_ms: now_ms(),
            cash_flows: HashMap::new(),
            current_utc_day: utc_day_number(),
            halted: false,
        }
    }

    /// Override the position cap and daily drawdown halt.
    pub fn with_limits(mut self, max_open_positions: usize, daily_drawdown_limit: f64) -> Self {
        self.max_open_positions = max_open_positions;
        self.daily_drawdown_limit = daily_drawdown_limit;
        self
    }

    /// Reset daily state when a new UTC day begins.
    pub fn check_day_reset(&mut self, current_equity: f64) {
        let today = utc_day_number();
        if today != self.current_utc_day {
            info!(
                "New UTC day {}. Resetting drawdown. Equity: {:.2}",
                today, current_equity
            );
            self.day_open_equity = current_equity;
            self.day_open_at_ms = now_ms();
            self.cash_flows.clear();
            self.current_utc_day = today;
            self.halted = false;
        }
    }

    /// When the day-open equity was measured: transfers from then on are
    /// today's cash flows.
    pub fn day_open_at_ms(&self) -> u64 {
        self.day_open_at_ms
    }

    /// Replace today's cash flows with every transfer the venue now reports.
    ///
    /// Idempotent, so the same deposit seen on every poll counts once, and a
    /// withdrawal that is later cancelled drops out. A transfer keeps the value
    /// it had when first seen: revaluing a base-asset deposit at each poll
    /// would hide that asset's price move since it arrived, which is P&L.
    /// Transfers before the day-open measurement are already in it.
    pub fn set_cash_flows(&mut self, flows: &[CashFlow]) {
        let previous = std::mem::take(&mut self.cash_flows);
        for flow in flows {
            if flow.time_ms < self.day_open_at_ms || !flow.value.is_finite() {
                continue;
            }
            let value = previous.get(&flow.id).copied().unwrap_or(flow.value);
            self.cash_flows.insert(flow.id.clone(), value);
        }
    }

    /// Net deposits minus withdrawals since the day opened.
    pub fn net_cash_flow(&self) -> f64 {
        self.cash_flows.values().sum()
    }

    /// What equity would be now had nothing been made or lost: the day-open
    /// equity plus whatever was deposited or withdrawn since. Today's P&L and
    /// drawdown are measured against this, so moving money in or out is
    /// neither a gain nor a loss.
    pub fn day_baseline(&self) -> f64 {
        self.day_open_equity + self.net_cash_flow()
    }

    /// Today's trading P&L in quote currency, cash flows excluded.
    pub fn day_pnl(&self, current_equity: f64) -> f64 {
        current_equity - self.day_baseline()
    }

    /// Returns `true` if the daily drawdown limit has been breached (sets `halted`).
    ///
    /// `current_equity` must be *total* portfolio equity — free USDT plus the
    /// mark-to-market value of open positions. Passing free USDT alone makes
    /// every entry look like a loss the size of the position notional.
    ///
    /// Deposits and withdrawals recorded with [`Self::set_cash_flows`] move
    /// the baseline, not the drawdown.
    pub fn check_drawdown(&mut self, current_equity: f64) -> bool {
        let baseline = self.day_baseline();
        if baseline <= 0.0 {
            return false;
        }
        let drawdown = (baseline - current_equity) / baseline;
        if drawdown >= self.daily_drawdown_limit {
            warn!(
                "HALT: drawdown {:.2}% >= {:.1}% limit (day open: {:.2}, net deposits: {:.2}, \
                 now: {:.2})",
                drawdown * 100.0,
                self.daily_drawdown_limit * 100.0,
                self.day_open_equity,
                self.net_cash_flow(),
                current_equity
            );
            self.halted = true;
            return true;
        }
        false
    }

    /// Current drawdown from the day's baseline, as a percentage.
    pub fn drawdown_pct(&self, current_equity: f64) -> f64 {
        let baseline = self.day_baseline();
        if baseline <= 0.0 {
            return 0.0;
        }
        -self.day_pnl(current_equity) / baseline * 100.0
    }

    /// Whether a new position is allowed right now.
    pub fn can_open_position(&self) -> bool {
        !self.halted && self.positions.len() < self.max_open_positions
    }

    /// Whether the bot already holds a position in `symbol`.
    pub fn has_position(&self, symbol: &str) -> bool {
        self.positions.iter().any(|p| p.symbol == symbol)
    }

    /// Calculate position size (in base-asset units).
    ///
    /// Risk is taken as a fraction of total `equity`, then capped by what
    /// `available_usdt` can actually pay for — a tight stop otherwise sizes a
    /// position worth several times the free balance and the order is rejected.
    /// Returns `(quantity, stop_distance)`.
    pub fn calculate_position_size(
        &self,
        equity: f64,
        available_usdt: f64,
        entry_price: f64,
        stop_loss: f64,
    ) -> (f64, f64) {
        let stop_distance = entry_price - stop_loss;
        if stop_distance <= 0.0 || entry_price <= 0.0 {
            return (0.0, 0.0);
        }

        let risk_amount = equity * self.risk_per_trade;
        let mut quantity = risk_amount / stop_distance;

        let max_affordable = available_usdt * MAX_NOTIONAL_FRACTION / entry_price;
        if quantity > max_affordable {
            warn!(
                "Position size capped by balance: risk-based {:.6} -> affordable {:.6} \
                 (entry {:.2}, free {:.2} USDT)",
                quantity, max_affordable, entry_price, available_usdt
            );
            quantity = max_affordable;
        }

        if quantity <= 0.0 {
            return (0.0, 0.0);
        }
        (quantity, stop_distance)
    }

    /// Record a newly opened position.
    pub fn open_position(&mut self, pos: Position) {
        info!(
            "Opened {} | entry {:.2} | qty {:.6} | SL {:.2} | TP {:.2} | {}",
            pos.symbol,
            pos.entry_price,
            pos.quantity,
            pos.stop_loss,
            pos.take_profit,
            if pos.protected {
                "OCO confirmed"
            } else {
                "UNPROTECTED"
            }
        );
        self.positions.push(pos);
    }

    /// Remove and return the position for `symbol`, if any.
    pub fn close_position(&mut self, symbol: &str) -> Option<Position> {
        if let Some(idx) = self.positions.iter().position(|p| p.symbol == symbol) {
            let pos = self.positions.remove(idx);
            info!(
                "Closed {} | entry {:.2} | qty {:.6}",
                pos.symbol, pos.entry_price, pos.quantity
            );
            Some(pos)
        } else {
            None
        }
    }

    /// Check whether an existing position in `symbol` should be exited because
    /// the current price has breached its stop-loss or take-profit.
    ///
    /// A level of `0.0` means "not set" and is ignored. Without that guard a
    /// position registered with `take_profit: 0.0` matches `price >= take_profit`
    /// on every call and is liquidated on the next cycle.
    pub fn check_exits(&self, symbol: &str, current_price: f64) -> Option<&Position> {
        self.positions.iter().find(|p| {
            if p.symbol != symbol {
                return false;
            }
            let hit_stop = p.stop_loss > 0.0 && current_price <= p.stop_loss;
            let hit_target = p.take_profit > 0.0 && current_price >= p.take_profit;
            hit_stop || hit_target
        })
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// Number of days since the Unix epoch in UTC.
fn utc_day_number() -> u32 {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    (secs / 86_400) as u32
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_risk_per_trade_scales_size_linearly() {
        // Sizing is the operator's main lever: doubling the risk fraction must
        // double the position, up to what the balance can fund.
        let base = RiskManager::with_risk_per_trade(10_000.0, 0.015);
        let doubled = RiskManager::with_risk_per_trade(10_000.0, 0.030);
        let (q1, _) = base.calculate_position_size(10_000.0, 1_000_000.0, 50_000.0, 49_000.0);
        let (q2, _) = doubled.calculate_position_size(10_000.0, 1_000_000.0, 50_000.0, 49_000.0);
        assert!((q2 - 2.0 * q1).abs() < 1e-9);
    }

    #[test]
    fn test_risk_per_trade_still_capped_by_balance() {
        // A large risk fraction must not size beyond what the account holds.
        let rm = RiskManager::with_risk_per_trade(10_000.0, 0.50);
        let (qty, _) = rm.calculate_position_size(10_000.0, 10_000.0, 50_000.0, 49_000.0);
        assert!(qty * 50_000.0 <= 10_000.0);
    }

    #[test]
    fn test_position_sizing() {
        let rm = RiskManager::new(10_000.0);
        // equity=10k, entry=50000, stop=49000 → distance=1000
        // risk = 10000 * 0.015 = 150 → qty = 150/1000 = 0.15
        let (qty, dist) = rm.calculate_position_size(10_000.0, 10_000.0, 50_000.0, 49_000.0);
        assert!((qty - 0.15).abs() < 1e-6);
        assert!((dist - 1000.0).abs() < 1e-6);
    }

    #[test]
    fn test_position_sizing_invalid_stop() {
        let rm = RiskManager::new(10_000.0);
        // stop above entry → zero
        let (qty, _) = rm.calculate_position_size(10_000.0, 10_000.0, 50_000.0, 51_000.0);
        assert_eq!(qty, 0.0);
    }

    #[test]
    fn test_position_size_capped_by_available_balance() {
        let rm = RiskManager::new(10_000.0);
        // A 0.2% stop sizes 150/100 = 1.5 BTC ≈ 75,000 USDT of notional on a
        // 10,000 USDT balance. It must be capped to what the balance can buy.
        let (qty, _) = rm.calculate_position_size(10_000.0, 10_000.0, 50_000.0, 49_900.0);
        let notional = qty * 50_000.0;
        assert!(
            notional <= 10_000.0,
            "notional {notional:.2} exceeds available balance"
        );
        assert!((qty - 10_000.0 * 0.95 / 50_000.0).abs() < 1e-9);
    }

    #[test]
    fn test_position_size_uses_equity_not_free_balance() {
        let rm = RiskManager::new(10_000.0);
        // Equity 10k but only 3k free: risk is still 1.5% of equity (qty 0.15,
        // notional 7.5k) — but 3k free can only afford 0.057 BTC.
        let (qty, _) = rm.calculate_position_size(10_000.0, 3_000.0, 50_000.0, 49_000.0);
        assert!((qty - 3_000.0 * 0.95 / 50_000.0).abs() < 1e-9);
    }

    #[test]
    fn test_max_positions_enforced() {
        let mut rm = RiskManager::new(10_000.0);
        assert!(rm.can_open_position());

        for i in 0..4 {
            rm.open_position(Position {
                symbol: format!("PAIR{i}"),
                entry_price: 100.0,
                quantity: 1.0,
                stop_loss: 90.0,
                take_profit: 120.0,
                entry_time: 0,
                protected: false,
            highest_high: 0.0,
            atr_at_entry: 0.0,
            });
        }
        assert!(!rm.can_open_position());
    }

    #[test]
    fn test_drawdown_triggers_halt() {
        let mut rm = RiskManager::new(10_000.0);
        assert!(!rm.check_drawdown(9_600.0)); // 4% — OK
        assert!(rm.check_drawdown(9_400.0)); // 6% — halted
        assert!(rm.halted);
        assert!(!rm.can_open_position());
    }

    #[test]
    fn test_close_position() {
        let mut rm = RiskManager::new(10_000.0);
        rm.open_position(Position {
            symbol: "BTCUSDT".into(),
            entry_price: 50_000.0,
            quantity: 0.1,
            stop_loss: 49_000.0,
            take_profit: 52_000.0,
            entry_time: 0,
            protected: false,
            highest_high: 0.0,
            atr_at_entry: 0.0,
        });
        assert!(rm.has_position("BTCUSDT"));
        let closed = rm.close_position("BTCUSDT");
        assert!(closed.is_some());
        assert!(!rm.has_position("BTCUSDT"));
    }

    #[test]
    fn test_check_exits() {
        let mut rm = RiskManager::new(10_000.0);
        rm.open_position(Position {
            symbol: "ETHUSDT".into(),
            entry_price: 3_000.0,
            quantity: 1.0,
            stop_loss: 2_800.0,
            take_profit: 3_400.0,
            entry_time: 0,
            protected: false,
            highest_high: 0.0,
            atr_at_entry: 0.0,
        });
        // Price between SL and TP → no exit
        assert!(rm.check_exits("ETHUSDT", 3_100.0).is_none());
        // Price hits TP
        assert!(rm.check_exits("ETHUSDT", 3_500.0).is_some());
        // Price hits SL
        assert!(rm.check_exits("ETHUSDT", 2_700.0).is_some());
    }

    #[test]
    fn test_check_exits_ignores_unset_levels() {
        let mut rm = RiskManager::new(10_000.0);
        // A reconciled position whose levels are unknown: the exchange's own OCO
        // protects it, and the bot must not treat 0.0 as a breached target.
        rm.open_position(Position {
            symbol: "BTCUSDT".into(),
            entry_price: 50_000.0,
            quantity: 0.1,
            stop_loss: 0.0,
            take_profit: 0.0,
            entry_time: 0,
            protected: false,
            highest_high: 0.0,
            atr_at_entry: 0.0,
        });
        assert!(rm.check_exits("BTCUSDT", 50_000.0).is_none());
        assert!(rm.check_exits("BTCUSDT", 1.0).is_none());
    }

    #[test]
    fn test_check_exits_with_only_stop_set() {
        let mut rm = RiskManager::new(10_000.0);
        rm.open_position(Position {
            symbol: "BTCUSDT".into(),
            entry_price: 50_000.0,
            quantity: 0.1,
            stop_loss: 48_000.0,
            take_profit: 0.0,
            entry_time: 0,
            protected: false,
            highest_high: 0.0,
            atr_at_entry: 0.0,
        });
        // Unset target never fires, however high the price goes.
        assert!(rm.check_exits("BTCUSDT", 90_000.0).is_none());
        // The stop still works.
        assert!(rm.check_exits("BTCUSDT", 47_500.0).is_some());
    }

    #[test]
    fn test_drawdown_uses_day_open_equity() {
        let mut rm = RiskManager::new(10_000.0);
        // Entry converts 3,000 USDT into an asset: free balance falls to 7,000
        // but equity is unchanged, so no halt.
        assert!(!rm.check_drawdown(10_000.0));
        assert!(!rm.halted);
        assert!((rm.drawdown_pct(9_500.0) - 5.0).abs() < 1e-9);
    }

    fn flow(id: &str, rm: &RiskManager, value: f64) -> CashFlow {
        CashFlow {
            id: id.into(),
            time_ms: rm.day_open_at_ms() + 1,
            value,
        }
    }

    #[test]
    fn a_withdrawal_does_not_trigger_the_drawdown_halt() {
        let mut rm = RiskManager::new(10_000.0);
        // 2,000 withdrawn: equity falls 20%, but nothing was lost trading.
        rm.set_cash_flows(&[flow("w", &rm, -2_000.0)]);
        assert!(!rm.check_drawdown(8_000.0));
        assert!(!rm.halted);
        assert!(rm.day_pnl(8_000.0).abs() < 1e-9);
        assert!(rm.drawdown_pct(8_000.0).abs() < 1e-9);
    }

    #[test]
    fn a_deposit_is_not_todays_profit() {
        let mut rm = RiskManager::new(10_000.0);
        rm.set_cash_flows(&[flow("d", &rm, 5_000.0)]);
        assert!(rm.day_pnl(15_000.0).abs() < 1e-9);
        assert!((rm.day_pnl(15_300.0) - 300.0).abs() < 1e-9);
    }

    #[test]
    fn a_deposit_cannot_mask_a_trading_loss() {
        let mut rm = RiskManager::new(10_000.0);
        // 5,000 deposited, then 900 lost: 6% of the 15,000 baseline, halted,
        // even though equity is well above the day open.
        rm.set_cash_flows(&[flow("d", &rm, 5_000.0)]);
        assert!(rm.check_drawdown(14_100.0));
        assert!((rm.drawdown_pct(14_100.0) - 6.0).abs() < 1e-9);
    }

    #[test]
    fn drawdown_after_a_withdrawal_is_measured_on_what_is_left() {
        let mut rm = RiskManager::new(10_000.0);
        rm.set_cash_flows(&[flow("w", &rm, -5_000.0)]);
        // 300 lost on the remaining 5,000 is 6%.
        assert!(rm.check_drawdown(4_700.0));
    }

    #[test]
    fn the_same_transfer_counts_once_and_keeps_its_first_value() {
        let mut rm = RiskManager::new(10_000.0);
        rm.set_cash_flows(&[flow("d", &rm, 1_000.0)]);
        // Seen again next poll, now priced higher: still 1,000.
        rm.set_cash_flows(&[flow("d", &rm, 1_100.0)]);
        assert!((rm.net_cash_flow() - 1_000.0).abs() < 1e-9);
        // A second transfer adds; one that disappears (cancelled) drops out.
        rm.set_cash_flows(&[flow("w", &rm, -400.0)]);
        assert!((rm.net_cash_flow() + 400.0).abs() < 1e-9);
    }

    #[test]
    fn transfers_before_the_day_open_are_already_in_the_baseline() {
        let mut rm = RiskManager::new(10_000.0);
        let earlier = CashFlow {
            id: "d".into(),
            time_ms: rm.day_open_at_ms() - 1,
            value: 1_000.0,
        };
        rm.set_cash_flows(&[earlier]);
        assert_eq!(rm.net_cash_flow(), 0.0);
        assert_eq!(rm.day_baseline(), 10_000.0);
    }
}
