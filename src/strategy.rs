use crate::indicators;
use crate::exchange::Candle;
use serde::Serialize;
use tracing::debug;

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
}

/// Snapshot of all indicator values at the latest candle.
pub struct IndicatorSnapshot {
    pub ema_50: f64,
    pub ema_200: f64,
    pub rsi_14: f64,
    pub macd_line: f64,
    pub macd_signal: f64,
    pub macd_histogram: f64,
    pub macd_crossed_bullish_recently: bool,
    pub current_price: f64,
    pub swing_low: f64,
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

    // Did MACD cross bullish within the last 3 four-hour candles?
    let macd_crossed_bullish_recently = (1..=3.min(n - 1)).any(|offset| {
        let i = n - 1 - offset;
        let prev_m = macd_result.macd_line[i];
        let prev_s = macd_result.signal_line[i];
        if prev_m.is_nan() || prev_s.is_nan() {
            return false;
        }
        prev_m <= prev_s && macd_line > macd_signal
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
        ema_50, ema_200, rsi_14, macd_line, macd_signal, macd_histogram,
        macd_crossed_bullish_recently, swing_low, current_price,
        "indicators computed"
    );

    Some(IndicatorSnapshot {
        ema_50,
        ema_200,
        rsi_14,
        macd_line,
        macd_signal,
        macd_histogram,
        macd_crossed_bullish_recently,
        current_price,
        swing_low,
    })
}

/// Evaluate the indicator snapshot against entry/exit rules and return a signal.
pub fn evaluate(symbol: &str, snap: &IndicatorSnapshot) -> TradeSignal {
    let mut warnings: Vec<String> = Vec::new();
    let mut reasons: Vec<String> = Vec::new();

    // -----------------------------------------------------------------------
    // BUY conditions (ALL must be true)
    // -----------------------------------------------------------------------

    // 1. Trend: price > EMA50 > EMA200
    let trend_bullish = snap.current_price > snap.ema_50 && snap.ema_50 > snap.ema_200;

    // 2. RSI(14) daily between 35 and 55
    let rsi_ok = snap.rsi_14 >= 35.0 && snap.rsi_14 <= 55.0;

    // 3. MACD crossed bullish within last 3 four-hour candles
    let macd_crossed = snap.macd_crossed_bullish_recently;

    // 4. Reward-to-risk >= 2.0
    let stop_distance = snap.current_price - snap.swing_low;
    let take_profit = snap.current_price + 2.0 * stop_distance;
    let rr_ratio = if stop_distance > 0.0 {
        (take_profit - snap.current_price) / stop_distance
    } else {
        0.0
    };
    let rr_ok = rr_ratio >= 2.0;

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
        format!("RSI(14) = {:.1} (in buy zone 35–55)", snap.rsi_14)
    } else {
        format!("RSI(14) = {:.1} (outside buy zone)", snap.rsi_14)
    });

    reasons.push(if macd_crossed {
        "MACD crossed bullish within last 3 candles".into()
    } else if snap.macd_line > snap.macd_signal {
        "MACD above signal but no recent crossover".into()
    } else {
        "MACD bearish".into()
    });

    // ----- BUY -----
    let all_buy = trend_bullish && rsi_ok && macd_crossed && rr_ok && stop_distance > 0.0;

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
        };
    }

    // -----------------------------------------------------------------------
    // SELL conditions (ANY is sufficient)
    // -----------------------------------------------------------------------
    let sell_trend_break = snap.current_price < snap.ema_50;
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
    }
}
