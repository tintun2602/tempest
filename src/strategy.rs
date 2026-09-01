use crate::exchange::Candle;
use crate::indicators;
use serde::Serialize;
use tracing::debug;

/// The RSI band the strategy has always used.
const DEFAULT_RSI_MIN: f64 = 35.0;
const DEFAULT_RSI_MAX: f64 = 55.0;
/// How recent a bullish MACD cross must be, in four-hour bars.
const DEFAULT_MACD_LOOKBACK_BARS: usize = 3;
/// How far back `compute_indicators` looks for the last cross. The snapshot
/// records the distance so the *policy* (how recent is recent enough) lives in
/// `StrategyParams` rather than being baked into the measurement.
const MAX_MACD_LOOKBACK_BARS: usize = 12;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum Signal {
    Buy,
    Sell,
    Hold,
    /// Reserved. `main` already handles this arm, but `evaluate` never returns
    /// one: the drawdown halt lives in `RiskManager::halted`, which stops
    /// trading without going through the signal path. Producing it here would
    /// change signal semantics, so it stays unconstructed until that is a
    /// deliberate decision rather than a side effect.
    #[allow(dead_code)]
    Halt,
}

#[derive(Debug, Clone, Serialize)]
pub struct TradeSignal {
    pub asset: String,
    pub signal: Signal,
    pub confidence: String,
    pub entry_price: f64,
    pub stop_loss: f64,
    pub take_profit: f64,
    pub risk_reward_ratio: f64,
    pub reasoning: String,
    pub warnings: Vec<String>,
    /// Daily ATR(14) at signal time, used to size the trailing stop.
    pub atr: f64,
}

/// Snapshot of all indicator values at the latest candle.
pub struct IndicatorSnapshot {
    pub ema_50: f64,
    pub ema_200: f64,
    pub rsi_14: f64,
    pub macd_histogram: f64,
    /// MACD line currently above its signal line.
    pub macd_above_signal: bool,
    /// Bars since the MACD line was last at or below its signal line, capped at
    /// [`MAX_MACD_LOOKBACK_BARS`]. `None` means it has been above for at least
    /// that long — a mature move rather than a fresh cross.
    pub macd_bars_since_cross: Option<usize>,
    pub current_price: f64,
    pub swing_low: f64,
    /// Daily ATR(14) — "normal" movement in price units, used to size the
    /// signal buffer so a threshold is neither too tight for a volatile asset
    /// nor too loose for a calm one.
    pub atr_14: f64,
}

/// Tunables that change how far price must travel past EMA50 before a signal
/// counts, without changing which indicators are consulted.
///
/// Both default to zero, which reproduces the original behaviour exactly:
/// entry and exit share the EMA50 line. That shared threshold is what makes
/// price hovering near the line produce a buy, a noise stop-out, and a buy
/// again — each round trip paying roughly 0.2-0.3% in fees and slippage.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StrategyParams {
    /// ATR multiples price must sit *above* EMA50 to enter.
    pub entry_buffer_atr: f64,
    /// ATR multiples price must fall *below* EMA50 to exit.
    pub exit_buffer_atr: f64,
    /// Chandelier trailing stop, in ATR multiples below the highest high since
    /// entry. Zero disables it and keeps the fixed 2R take-profit.
    ///
    /// The fixed target caps every winner at twice the stop distance, which
    /// deletes the fat right tail trend-following depends on — measured here,
    /// *every* winning trade exited at the target and none ran further.
    pub trailing_stop_atr: f64,

    /// Lower bound of the daily RSI(14) entry band.
    pub rsi_min: f64,
    /// Upper bound of the daily RSI(14) entry band.
    ///
    /// The default of 55 fights the trend filter: an established uptrend keeps
    /// daily RSI in roughly 55-70, so demanding price > EMA50 > EMA200 *and*
    /// RSI <= 55 asks for strength and weakness at the same time. Raising this
    /// is the single largest lever on how often the strategy trades.
    pub rsi_max: f64,

    /// Whether MACD must have *crossed* bullish recently, rather than merely
    /// sitting above its signal line.
    ///
    /// The cross is an event and holds for only a handful of bars; the state
    /// holds for as long as the move does. Requiring the event is the sharpest
    /// single constraint on entry frequency.
    pub macd_require_cross: bool,
    /// How many four-hour bars back the bullish cross may have happened.
    /// Ignored when `macd_require_cross` is false.
    pub macd_lookback_bars: usize,
}

impl Default for StrategyParams {
    fn default() -> Self {
        Self {
            entry_buffer_atr: 0.0,
            exit_buffer_atr: 0.0,
            trailing_stop_atr: 0.0,
            rsi_min: DEFAULT_RSI_MIN,
            rsi_max: DEFAULT_RSI_MAX,
            macd_require_cross: true,
            macd_lookback_bars: DEFAULT_MACD_LOOKBACK_BARS,
        }
    }
}

impl StrategyParams {
    pub fn from_env() -> Self {
        let read = |key: &str| {
            std::env::var(key)
                .ok()
                .and_then(|v| v.parse::<f64>().ok())
                .filter(|v| v.is_finite() && *v >= 0.0)
                .unwrap_or(0.0)
        };
        // Buffers default to zero (feature off). The entry gates instead
        // default to the values the strategy has always used, so an unset
        // environment reproduces current behaviour exactly.
        let or_default = |key: &str, fallback: f64| {
            std::env::var(key)
                .ok()
                .and_then(|v| v.parse::<f64>().ok())
                .filter(|v| v.is_finite())
                .unwrap_or(fallback)
        };

        let rsi_min = or_default("RSI_MIN", DEFAULT_RSI_MIN);
        let rsi_max = or_default("RSI_MAX", DEFAULT_RSI_MAX);
        // An inverted band silently blocks every trade. Fall back rather than
        // run a bot that can never buy.
        let (rsi_min, rsi_max) = if rsi_min <= rsi_max {
            (rsi_min, rsi_max)
        } else {
            tracing::warn!(
                "RSI_MIN {rsi_min} is above RSI_MAX {rsi_max} — ignoring both and using {}-{}",
                DEFAULT_RSI_MIN,
                DEFAULT_RSI_MAX
            );
            (DEFAULT_RSI_MIN, DEFAULT_RSI_MAX)
        };

        Self {
            entry_buffer_atr: read("ENTRY_BUFFER_ATR"),
            exit_buffer_atr: read("EXIT_BUFFER_ATR"),
            trailing_stop_atr: read("TRAILING_STOP_ATR"),
            rsi_min,
            rsi_max,
            // Only an explicit falsey value switches to state mode; anything
            // unrecognised keeps the stricter rule rather than silently
            // loosening entry.
            macd_require_cross: std::env::var("MACD_REQUIRE_CROSS")
                .map(|v| !matches!(v.trim().to_ascii_lowercase().as_str(), "false" | "0" | "no"))
                .unwrap_or(true),
            macd_lookback_bars: std::env::var("MACD_LOOKBACK_BARS")
                .ok()
                .and_then(|v| v.parse::<usize>().ok())
                .filter(|v| *v >= 1 && *v <= MAX_MACD_LOOKBACK_BARS)
                .unwrap_or(DEFAULT_MACD_LOOKBACK_BARS),
        }
    }

    /// Price must clear this to enter.
    fn entry_threshold(&self, snap: &IndicatorSnapshot) -> f64 {
        snap.ema_50 + self.entry_buffer_atr * usable_atr(snap)
    }

    /// Price must fall under this to exit.
    fn exit_threshold(&self, snap: &IndicatorSnapshot) -> f64 {
        snap.ema_50 - self.exit_buffer_atr * usable_atr(snap)
    }

    /// Whether MACD momentum satisfies the entry gate.
    ///
    /// In cross mode the line must be above its signal *and* have been below it
    /// within `macd_lookback_bars` — a fresh turn. In state mode being above is
    /// enough, which fires far more often and drifts toward trend-following.
    fn macd_bullish(&self, snap: &IndicatorSnapshot) -> bool {
        if !snap.macd_above_signal {
            return false;
        }
        if !self.macd_require_cross {
            return true;
        }
        snap.macd_bars_since_cross
            .is_some_and(|bars| bars <= self.macd_lookback_bars)
    }
}

/// ATR treated as zero when it is not computable, so a missing value collapses
/// to the original un-buffered rule rather than disabling the strategy.
fn usable_atr(snap: &IndicatorSnapshot) -> f64 {
    if snap.atr_14.is_finite() && snap.atr_14 > 0.0 {
        snap.atr_14
    } else {
        0.0
    }
}

/// Compute all indicators from daily and 4-hour candles.
/// Returns `None` if there is insufficient data (need >= 200 daily, >= 35 four-hour).
pub fn compute_indicators(
    daily_candles: &[Candle],
    four_hour_candles: &[Candle],
    current_price: f64,
) -> Option<IndicatorSnapshot> {
    if daily_candles.len() < 200 || four_hour_candles.len() < 35 {
        return None;
    }

    let daily_closes: Vec<f64> = daily_candles.iter().map(|c| c.close).collect();
    let daily_lows: Vec<f64> = daily_candles.iter().map(|c| c.low).collect();
    let daily_highs: Vec<f64> = daily_candles.iter().map(|c| c.high).collect();
    let four_hour_closes: Vec<f64> = four_hour_candles.iter().map(|c| c.close).collect();

    // --- Daily EMAs ---
    let ema_50_series = indicators::ema(&daily_closes, 50);
    let ema_200_series = indicators::ema(&daily_closes, 200);
    let ema_50 = *ema_50_series.last()?;
    let ema_200 = *ema_200_series.last()?;
    if ema_50.is_nan() || ema_200.is_nan() {
        return None;
    }

    // --- Daily RSI(14) ---
    let rsi_series = indicators::rsi(&daily_closes, 14);
    let rsi_14 = *rsi_series.last()?;
    if rsi_14.is_nan() {
        return None;
    }

    // --- 4H MACD(12, 26, 9) ---
    let macd_result = indicators::macd(&four_hour_closes, 12, 26, 9);
    let n = macd_result.macd_line.len();
    let macd_line = macd_result.macd_line[n - 1];
    let macd_signal = macd_result.signal_line[n - 1];
    let macd_histogram = macd_result.histogram[n - 1];
    if macd_line.is_nan() || macd_signal.is_nan() {
        return None;
    }

    // How long has MACD been above its signal line? The first offset at which
    // it was at or below is the age of the current bullish leg.
    let macd_above_signal = macd_line > macd_signal;
    let macd_bars_since_cross = (1..=MAX_MACD_LOOKBACK_BARS.min(n - 1)).find(|offset| {
        let i = n - 1 - offset;
        let prev_m = macd_result.macd_line[i];
        let prev_s = macd_result.signal_line[i];
        !prev_m.is_nan() && !prev_s.is_nan() && prev_m <= prev_s
    });

    // --- Swing low for stop-loss ---
    let swing_low = indicators::find_nearest_swing_low(&daily_lows, 3).unwrap_or_else(|| {
        daily_lows
            .iter()
            .rev()
            .take(20)
            .copied()
            .fold(f64::MAX, f64::min)
    });

    debug!(
        ema_50,
        ema_200,
        rsi_14,
        macd_line,
        macd_signal,
        macd_histogram,
        macd_above_signal,
        ?macd_bars_since_cross,
        swing_low,
        current_price,
        "indicators computed"
    );

    // A missing ATR is not fatal: buffers simply fall back to the bare EMA50.
    let atr_14 = indicators::atr(&daily_highs, &daily_lows, &daily_closes, 14)
        .last()
        .copied()
        .unwrap_or(f64::NAN);

    Some(IndicatorSnapshot {
        atr_14,
        ema_50,
        ema_200,
        rsi_14,
        macd_histogram,
        macd_above_signal,
        macd_bars_since_cross,
        current_price,
        swing_low,
    })
}

/// The four conditions a BUY requires, evaluated independently.
///
/// Single source of truth: `evaluate` decides from this, the backtest counts
/// from it, and notifications render it. Restating the thresholds anywhere else
/// lets a status message claim a setup the strategy would not actually take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EntryConditions {
    /// Price above EMA50, and EMA50 above EMA200.
    pub trend_bullish: bool,
    /// Daily RSI(14) inside the configured accumulation band.
    pub rsi_ok: bool,
    /// MACD momentum is bullish, per [`StrategyParams::macd_require_cross`]:
    /// either a recent bullish cross, or simply the line above its signal.
    pub macd_crossed: bool,
    /// A swing low exists below price, so risk is measurable.
    ///
    /// Reward-to-risk is 2.0 by construction whenever this holds, since the
    /// target is set at twice the stop distance.
    pub stop_valid: bool,
}

impl EntryConditions {
    pub fn evaluate(snap: &IndicatorSnapshot, params: &StrategyParams) -> Self {
        Self {
            trend_bullish: snap.current_price > params.entry_threshold(snap)
                && snap.ema_50 > snap.ema_200,
            rsi_ok: snap.rsi_14 >= params.rsi_min && snap.rsi_14 <= params.rsi_max,
            macd_crossed: params.macd_bullish(snap),
            stop_valid: snap.current_price - snap.swing_low > 0.0,
        }
    }

    /// Every condition holds, so a BUY is taken.
    pub fn all_met(&self) -> bool {
        self.trend_bullish && self.rsi_ok && self.macd_crossed && self.stop_valid
    }

    pub fn met_count(&self) -> usize {
        [
            self.trend_bullish,
            self.rsi_ok,
            self.macd_crossed,
            self.stop_valid,
        ]
        .iter()
        .filter(|met| **met)
        .count()
    }

    /// `(label, met)` per condition, in evaluation order.
    pub fn checklist(&self) -> [(&'static str, bool); 4] {
        [
            ("Trend  price > EMA50 > EMA200", self.trend_bullish),
            ("RSI    within band", self.rsi_ok),
            ("MACD   bullish momentum", self.macd_crossed),
            ("Stop   swing low below price", self.stop_valid),
        ]
    }
}

/// Evaluate the indicator snapshot against entry/exit rules and return a signal.
pub fn evaluate(symbol: &str, snap: &IndicatorSnapshot, params: &StrategyParams) -> TradeSignal {
    let mut warnings: Vec<String> = Vec::new();
    let mut reasons: Vec<String> = Vec::new();

    // -----------------------------------------------------------------------
    // BUY conditions (ALL must be true)
    // -----------------------------------------------------------------------

    let conditions = EntryConditions::evaluate(snap, params);
    let EntryConditions {
        trend_bullish,
        rsi_ok,
        macd_crossed,
        ..
    } = conditions;

    // Reward-to-risk is 2.0 by construction: the target sits at twice the stop
    // distance, so `rr_ok` holds exactly when a stop distance exists.
    let stop_distance = snap.current_price - snap.swing_low;
    let take_profit = snap.current_price + 2.0 * stop_distance;
    let rr_ratio = if stop_distance > 0.0 {
        (take_profit - snap.current_price) / stop_distance
    } else {
        0.0
    };

    // Warn on extreme stop distances
    let stop_pct = if snap.current_price > 0.0 {
        stop_distance / snap.current_price * 100.0
    } else {
        0.0
    };
    if stop_pct > 0.0 && stop_pct < 0.5 {
        warnings.push("Stop-loss very tight (<0.5%), risk of noise stop-out".into());
    }
    if stop_pct > 10.0 {
        warnings.push("Stop-loss very wide (>10%), large risk per trade".into());
    }

    // Build reasoning
    reasons.push(if trend_bullish {
        format!(
            "Trend bullish: price {:.2} > EMA50 {:.2} > EMA200 {:.2}",
            snap.current_price, snap.ema_50, snap.ema_200
        )
    } else {
        format!(
            "Trend not bullish: price {:.2}, EMA50 {:.2}, EMA200 {:.2}",
            snap.current_price, snap.ema_50, snap.ema_200
        )
    });

    reasons.push(if rsi_ok {
        format!(
            "RSI(14) = {:.1} (in buy zone {:.0}–{:.0})",
            snap.rsi_14, params.rsi_min, params.rsi_max
        )
    } else {
        format!(
            "RSI(14) = {:.1} (outside buy zone {:.0}–{:.0})",
            snap.rsi_14, params.rsi_min, params.rsi_max
        )
    });

    reasons.push(if macd_crossed {
        match snap.macd_bars_since_cross {
            Some(bars) => format!("MACD crossed bullish {bars} candle(s) ago"),
            None => "MACD above signal (established leg)".into(),
        }
    } else if snap.macd_above_signal {
        "MACD above signal but no recent crossover".into()
    } else {
        "MACD bearish".into()
    });

    // ----- BUY -----
    let all_buy = conditions.all_met();

    if all_buy {
        let confidence = if snap.rsi_14 < 45.0 && stop_pct < 5.0 {
            "HIGH"
        } else {
            "MEDIUM"
        };
        return TradeSignal {
            asset: symbol.into(),
            signal: Signal::Buy,
            confidence: confidence.into(),
            entry_price: snap.current_price,
            stop_loss: snap.swing_low,
            take_profit,
            risk_reward_ratio: rr_ratio,
            reasoning: reasons.join(". "),
            warnings,
            atr: snap.atr_14,
        };
    }

    // -----------------------------------------------------------------------
    // SELL conditions (ANY is sufficient)
    // -----------------------------------------------------------------------
    let sell_trend_break = snap.current_price < params.exit_threshold(snap);
    let sell_overbought_reversal = snap.rsi_14 > 70.0 && snap.macd_histogram < 0.0;

    if sell_trend_break || sell_overbought_reversal {
        let mut sell_reasons = Vec::new();
        if sell_trend_break {
            sell_reasons.push(format!(
                "Price {:.2} below EMA50 {:.2} — downtrend",
                snap.current_price, snap.ema_50
            ));
        }
        if sell_overbought_reversal {
            sell_reasons.push(format!(
                "RSI {:.1} > 70 and MACD histogram negative — overbought reversal",
                snap.rsi_14
            ));
        }
        return TradeSignal {
            asset: symbol.into(),
            signal: Signal::Sell,
            confidence: "MEDIUM".into(),
            entry_price: snap.current_price,
            stop_loss: 0.0,
            take_profit: 0.0,
            risk_reward_ratio: 0.0,
            reasoning: sell_reasons.join(". "),
            warnings,
            atr: snap.atr_14,
        };
    }

    // -----------------------------------------------------------------------
    // HOLD — no clear edge
    // -----------------------------------------------------------------------
    TradeSignal {
        asset: symbol.into(),
        signal: Signal::Hold,
        confidence: "LOW".into(),
        entry_price: snap.current_price,
        stop_loss: snap.swing_low,
        take_profit,
        risk_reward_ratio: rr_ratio,
        reasoning: reasons.join(". "),
        warnings,
        atr: snap.atr_14,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A snapshot that satisfies every entry condition.
    fn qualifying() -> IndicatorSnapshot {
        IndicatorSnapshot {
            ema_50: 60_000.0,
            ema_200: 55_000.0,
            rsi_14: 45.0,
            macd_histogram: 5.0,
            macd_above_signal: true,
            macd_bars_since_cross: Some(1),
            current_price: 62_000.0,
            swing_low: 59_000.0,
            atr_14: 1_000.0,
        }
    }

    /// The original behaviour: entry and exit both sit exactly on EMA50.
    fn bare() -> StrategyParams {
        StrategyParams::default()
    }

    #[test]
    fn a_qualifying_setup_meets_every_condition() {
        let c = EntryConditions::evaluate(&qualifying(), &bare());
        assert!(c.all_met());
        assert_eq!(c.met_count(), 4);
    }

    #[test]
    fn buy_is_returned_exactly_when_all_conditions_hold() {
        // The invariant the status message depends on: if the checklist shows
        // 4/4, the strategy must actually take the trade. Otherwise a
        // notification could promise a setup that never fires.
        let snap = qualifying();
        assert!(EntryConditions::evaluate(&snap, &bare()).all_met());
        assert_eq!(evaluate("BTCUSDC", &snap, &bare()).signal, Signal::Buy);
    }

    #[test]
    fn no_buy_while_any_condition_fails() {
        let mut snap = qualifying();
        snap.rsi_14 = 80.0; // outside the 35-55 band
        let c = EntryConditions::evaluate(&snap, &bare());
        assert!(!c.all_met());
        assert_eq!(c.met_count(), 3);
        assert_ne!(evaluate("BTCUSDC", &snap, &bare()).signal, Signal::Buy);
    }

    #[test]
    fn an_inverted_ema_stack_fails_the_trend_check() {
        // The live case today: price is above EMA50, but EMA50 sits below
        // EMA200, so the trend condition must still fail.
        let mut snap = qualifying();
        snap.ema_50 = 66_756.0;
        snap.ema_200 = 71_203.0;
        snap.current_price = 77_214.0;
        let c = EntryConditions::evaluate(&snap, &bare());
        assert!(!c.trend_bullish);
    }

    #[test]
    fn a_swing_low_above_price_invalidates_the_stop() {
        let mut snap = qualifying();
        snap.swing_low = snap.current_price + 1.0;
        assert!(!EntryConditions::evaluate(&snap, &bare()).stop_valid);
    }

    #[test]
    fn checklist_agrees_with_the_individual_flags() {
        let mut snap = qualifying();
        snap.macd_bars_since_cross = None;
        let c = EntryConditions::evaluate(&snap, &bare());
        let checklist = c.checklist();
        assert_eq!(checklist.len(), 4);
        assert_eq!(
            checklist.iter().filter(|(_, met)| *met).count(),
            c.met_count()
        );
        assert!(!checklist[2].1, "MACD row must reflect the failed cross");
    }

    // ----- ATR signal buffers -----

    #[test]
    fn default_params_reproduce_the_bare_ema_rule() {
        // Zero buffers must behave exactly as before, so enabling the feature
        // is opt-in and the old backtests stay comparable.
        let mut snap = qualifying();
        snap.current_price = snap.ema_50 + 0.01;
        assert!(EntryConditions::evaluate(&snap, &bare()).trend_bullish);
    }

    #[test]
    fn an_entry_buffer_rejects_a_marginal_cross() {
        // Price a hair above EMA50 is exactly the setup that buys, gets
        // stopped out by noise, and buys again.
        let mut snap = qualifying();
        snap.current_price = snap.ema_50 + 0.01;
        let buffered = StrategyParams {
            entry_buffer_atr: 0.5,
            exit_buffer_atr: 0.0,
            ..Default::default()
        };
        assert!(!EntryConditions::evaluate(&snap, &buffered).trend_bullish);

        // A decisive move still qualifies: 0.5 ATR is 500 here.
        snap.current_price = snap.ema_50 + 600.0;
        assert!(EntryConditions::evaluate(&snap, &buffered).trend_bullish);
    }

    #[test]
    fn an_exit_buffer_holds_through_a_marginal_dip() {
        let mut snap = qualifying();
        snap.current_price = snap.ema_50 - 0.01;
        // Without a buffer this dip is a SELL.
        assert_eq!(evaluate("BTCUSDC", &snap, &bare()).signal, Signal::Sell);

        // With one, it is noise and the position is held.
        let buffered = StrategyParams {
            entry_buffer_atr: 0.0,
            exit_buffer_atr: 0.5,
            ..Default::default()
        };
        assert_ne!(evaluate("BTCUSDC", &snap, &buffered).signal, Signal::Sell);

        // A real breakdown still exits.
        snap.current_price = snap.ema_50 - 600.0;
        assert_eq!(evaluate("BTCUSDC", &snap, &buffered).signal, Signal::Sell);
    }

    #[test]
    fn buffers_create_a_dead_zone_between_entry_and_exit() {
        // The whole point: a price that neither buys nor sells, instead of one
        // threshold doing both jobs.
        let mut snap = qualifying();
        snap.current_price = snap.ema_50 + 100.0;
        let buffered = StrategyParams {
            entry_buffer_atr: 0.5,
            exit_buffer_atr: 0.5,
            ..Default::default()
        };
        assert!(!EntryConditions::evaluate(&snap, &buffered).all_met());
        assert_ne!(evaluate("BTCUSDC", &snap, &buffered).signal, Signal::Sell);
    }

    // ----- tunable entry gates -----

    #[test]
    fn default_params_are_the_historical_rules() {
        let p = StrategyParams::default();
        assert_eq!(p.rsi_min, 35.0);
        assert_eq!(p.rsi_max, 55.0);
        assert!(p.macd_require_cross);
        assert_eq!(p.macd_lookback_bars, 3);
    }

    #[test]
    fn a_raised_rsi_ceiling_admits_a_strong_trend() {
        // RSI 62 is typical of an established uptrend and is exactly what the
        // default 55 ceiling excludes.
        let mut snap = qualifying();
        snap.rsi_14 = 62.0;
        assert!(!EntryConditions::evaluate(&snap, &bare()).rsi_ok);

        let wide = StrategyParams {
            rsi_max: 70.0,
            ..Default::default()
        };
        assert!(EntryConditions::evaluate(&snap, &wide).rsi_ok);
        assert_eq!(evaluate("BTCUSDC", &snap, &wide).signal, Signal::Buy);
    }

    #[test]
    fn the_rsi_floor_still_rejects_a_collapse() {
        let mut snap = qualifying();
        snap.rsi_14 = 20.0;
        let wide = StrategyParams {
            rsi_max: 70.0,
            ..Default::default()
        };
        assert!(!EntryConditions::evaluate(&snap, &wide).rsi_ok);
    }

    #[test]
    fn a_stale_cross_fails_in_cross_mode_but_passes_in_state_mode() {
        // Above signal for 8 bars: a mature leg, not a fresh turn.
        let mut snap = qualifying();
        snap.macd_bars_since_cross = Some(8);
        assert!(!EntryConditions::evaluate(&snap, &bare()).macd_crossed);

        let state = StrategyParams {
            macd_require_cross: false,
            ..Default::default()
        };
        assert!(EntryConditions::evaluate(&snap, &state).macd_crossed);
    }

    #[test]
    fn a_widened_lookback_admits_a_slightly_older_cross() {
        let mut snap = qualifying();
        snap.macd_bars_since_cross = Some(5);
        assert!(!EntryConditions::evaluate(&snap, &bare()).macd_crossed);

        let wider = StrategyParams {
            macd_lookback_bars: 6,
            ..Default::default()
        };
        assert!(EntryConditions::evaluate(&snap, &wider).macd_crossed);
    }

    #[test]
    fn state_mode_still_requires_macd_above_its_signal() {
        // Loosening the recency rule must not turn a bearish MACD into a buy.
        let mut snap = qualifying();
        snap.macd_above_signal = false;
        snap.macd_bars_since_cross = None;
        let state = StrategyParams {
            macd_require_cross: false,
            ..Default::default()
        };
        assert!(!EntryConditions::evaluate(&snap, &state).macd_crossed);
        assert_ne!(evaluate("BTCUSDC", &snap, &state).signal, Signal::Buy);
    }

    #[test]
    fn a_missing_atr_falls_back_to_the_bare_threshold() {
        // A NaN ATR must not disable trading or produce a NaN threshold.
        let mut snap = qualifying();
        snap.atr_14 = f64::NAN;
        snap.current_price = snap.ema_50 + 0.01;
        let buffered = StrategyParams {
            entry_buffer_atr: 2.0,
            exit_buffer_atr: 2.0,
            ..Default::default()
        };
        assert!(EntryConditions::evaluate(&snap, &buffered).trend_bullish);
    }
}
