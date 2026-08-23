use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{info, warn};

const MAX_RISK_PER_TRADE: f64 = 0.015; // 1.5% of portfolio
const MAX_OPEN_POSITIONS: usize = 4;
const DAILY_DRAWDOWN_LIMIT: f64 = 0.05; // 5%
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
}

pub struct RiskManager {
    pub positions: Vec<Position>,
    /// Total portfolio equity at the start of the current UTC day — the baseline
    /// the daily drawdown limit is measured against.
    pub day_open_equity: f64,
    current_utc_day: u32,
    pub halted: bool,
}

impl RiskManager {
    pub fn new(starting_equity: f64) -> Self {
        Self {
            positions: Vec::new(),
            day_open_equity: starting_equity,
            current_utc_day: utc_day_number(),
            halted: false,
        }
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
            self.current_utc_day = today;
            self.halted = false;
        }
    }

    /// Returns `true` if the daily drawdown limit has been breached (sets `halted`).
    ///
    /// `current_equity` must be *total* portfolio equity — free USDT plus the
    /// mark-to-market value of open positions. Passing free USDT alone makes
    /// every entry look like a loss the size of the position notional.
    pub fn check_drawdown(&mut self, current_equity: f64) -> bool {
        if self.day_open_equity <= 0.0 {
            return false;
        }
        let drawdown = (self.day_open_equity - current_equity) / self.day_open_equity;
        if drawdown >= DAILY_DRAWDOWN_LIMIT {
            warn!(
                "HALT: drawdown {:.2}% >= {:.1}% limit (day open: {:.2}, now: {:.2})",
                drawdown * 100.0,
                DAILY_DRAWDOWN_LIMIT * 100.0,
                self.day_open_equity,
                current_equity
            );
            self.halted = true;
            return true;
        }
        false
    }

    /// Current drawdown from the day-open equity, as a percentage.
    pub fn drawdown_pct(&self, current_equity: f64) -> f64 {
        if self.day_open_equity <= 0.0 {
            return 0.0;
        }
        (self.day_open_equity - current_equity) / self.day_open_equity * 100.0
    }

    /// Whether a new position is allowed right now.
    pub fn can_open_position(&self) -> bool {
        !self.halted && self.positions.len() < MAX_OPEN_POSITIONS
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

        let risk_amount = equity * MAX_RISK_PER_TRADE;
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
            "Opened {} | entry {:.2} | qty {:.6} | SL {:.2} | TP {:.2}",
            pos.symbol, pos.entry_price, pos.quantity, pos.stop_loss, pos.take_profit
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
}
