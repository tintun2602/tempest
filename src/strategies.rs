//! The strategies the bot can trade, behind one switch.
//!
//! Every strategy decides from the same inputs the live loop already fetches:
//! the trend-interval snapshot (EMA50/EMA200, RSI and ATR on `TREND_INTERVAL`
//! candles) plus the raw `SIGNAL_INTERVAL` candles. The lab replays exactly
//! these calls, so a lab result describes the code that would run live.

use crate::exchange::Candle;
use crate::indicators;
use crate::strategy::{self, IndicatorSnapshot, Signal, StrategyParams, TradeSignal};

/// The strategy traded when `STRATEGY` is unset. The Trading department
/// proposes changes here by pull request; merging one is what switches live.
pub const LIVE_STRATEGY: StrategyKind = StrategyKind::TrendPullback;

/// Target distance as a multiple of the stop distance, for every strategy.
pub const REWARD_RISK: f64 = 2.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StrategyKind {
    /// Pullback inside an uptrend, timed by a MACD cross. The original rules.
    TrendPullback,
    /// Close above the highest high of the last `BREAKOUT_LOOKBACK` signal
    /// bars while price is above the trend EMA200.
    Breakout,
    /// Signal-interval RSI washed out below `OVERSOLD` while price holds above
    /// the trend EMA200; out once RSI recovers past `RECOVERED`.
    MeanReversion,
}

const BREAKOUT_LOOKBACK: usize = 20;
/// Breakout exit: a close below the lowest low of this many signal bars.
const BREAKOUT_EXIT_LOOKBACK: usize = 10;
const OVERSOLD: f64 = 30.0;
const RECOVERED: f64 = 60.0;
/// Stops for the new strategies sit this many *trend-interval* ATRs below
/// entry. A signal-interval ATR is so tight on 15m bars that fees and noise
/// would decide most trades.
const STOP_ATR: f64 = 1.5;

impl StrategyKind {
    pub const ALL: [StrategyKind; 3] = [
        StrategyKind::TrendPullback,
        StrategyKind::Breakout,
        StrategyKind::MeanReversion,
    ];

    pub fn name(self) -> &'static str {
        match self {
            StrategyKind::TrendPullback => "trend_pullback",
            StrategyKind::Breakout => "breakout",
            StrategyKind::MeanReversion => "mean_reversion",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.name() == name)
    }

    /// `STRATEGY` if set, otherwise [`LIVE_STRATEGY`]. An unknown name stops
    /// the bot at startup rather than quietly trading the default.
    pub fn live_from_env() -> Self {
        match std::env::var("STRATEGY") {
            Ok(name) if !name.trim().is_empty() => {
                Self::from_name(name.trim()).unwrap_or_else(|| {
                    let known: Vec<&str> = Self::ALL.iter().map(|k| k.name()).collect();
                    panic!("Invalid configuration: STRATEGY={name} is not one of {known:?}")
                })
            }
            _ => LIVE_STRATEGY,
        }
    }

    /// Decide on the latest bar. `signal_candles` ends with the bar being
    /// evaluated; nothing later may be passed in.
    pub fn evaluate(
        self,
        symbol: &str,
        snap: &IndicatorSnapshot,
        signal_candles: &[Candle],
        params: &StrategyParams,
    ) -> TradeSignal {
        match self {
            StrategyKind::TrendPullback => strategy::evaluate(symbol, snap, params),
            StrategyKind::Breakout => breakout(symbol, snap, signal_candles),
            StrategyKind::MeanReversion => mean_reversion(symbol, snap, signal_candles),
        }
    }
}

fn breakout(symbol: &str, snap: &IndicatorSnapshot, bars: &[Candle]) -> TradeSignal {
    let price = snap.current_price;
    let n = bars.len();
    if n < BREAKOUT_LOOKBACK + 1 {
        return hold(symbol, snap, "not enough signal bars");
    }
    // The channel excludes the bar being evaluated, or a breakout could never
    // clear its own high.
    let prior = &bars[n - 1 - BREAKOUT_LOOKBACK..n - 1];
    let channel_high = prior.iter().map(|c| c.high).fold(f64::MIN, f64::max);
    let exit_low = bars[n.saturating_sub(1 + BREAKOUT_EXIT_LOOKBACK)..n - 1]
        .iter()
        .map(|c| c.low)
        .fold(f64::MAX, f64::min);

    if price < exit_low {
        return sell(
            symbol,
            snap,
            format!("Close {price:.2} below {BREAKOUT_EXIT_LOOKBACK}-bar low {exit_low:.2}"),
        );
    }
    if price > channel_high && price > snap.ema_200 {
        return buy_with_atr_stop(
            symbol,
            snap,
            format!(
                "Breakout: {price:.2} above {BREAKOUT_LOOKBACK}-bar high {channel_high:.2}, \
                 above EMA200 {:.2}",
                snap.ema_200
            ),
        );
    }
    hold(symbol, snap, "no breakout")
}

fn mean_reversion(symbol: &str, snap: &IndicatorSnapshot, bars: &[Candle]) -> TradeSignal {
    let closes: Vec<f64> = bars.iter().map(|c| c.close).collect();
    let Some(rsi) = indicators::rsi(&closes, 14)
        .last()
        .copied()
        .filter(|v| v.is_finite())
    else {
        return hold(symbol, snap, "signal RSI not computable");
    };

    if rsi > RECOVERED {
        return sell(
            symbol,
            snap,
            format!("Signal RSI {rsi:.1} recovered above {RECOVERED}"),
        );
    }
    if rsi < OVERSOLD && snap.current_price > snap.ema_200 {
        return buy_with_atr_stop(
            symbol,
            snap,
            format!(
                "Oversold dip: signal RSI {rsi:.1} < {OVERSOLD}, price above EMA200 {:.2}",
                snap.ema_200
            ),
        );
    }
    hold(symbol, snap, &format!("signal RSI {rsi:.1}"))
}

/// A BUY with its stop `STOP_ATR` trend ATRs below price and a 2R target. No
/// usable ATR means no measurable risk, so no trade.
fn buy_with_atr_stop(symbol: &str, snap: &IndicatorSnapshot, reasoning: String) -> TradeSignal {
    let price = snap.current_price;
    if !(snap.atr_14.is_finite() && snap.atr_14 > 0.0) {
        return hold(symbol, snap, "ATR not computable");
    }
    let stop = price - STOP_ATR * snap.atr_14;
    if stop <= 0.0 {
        return hold(symbol, snap, "stop below zero");
    }
    TradeSignal {
        asset: symbol.into(),
        signal: Signal::Buy,
        confidence: "MEDIUM".into(),
        entry_price: price,
        stop_loss: stop,
        take_profit: price + REWARD_RISK * (price - stop),
        risk_reward_ratio: REWARD_RISK,
        reasoning,
        warnings: Vec::new(),
        atr: snap.atr_14,
    }
}

fn sell(symbol: &str, snap: &IndicatorSnapshot, reasoning: String) -> TradeSignal {
    TradeSignal {
        asset: symbol.into(),
        signal: Signal::Sell,
        confidence: "MEDIUM".into(),
        entry_price: snap.current_price,
        stop_loss: 0.0,
        take_profit: 0.0,
        risk_reward_ratio: 0.0,
        reasoning,
        warnings: Vec::new(),
        atr: snap.atr_14,
    }
}

fn hold(symbol: &str, snap: &IndicatorSnapshot, reasoning: &str) -> TradeSignal {
    TradeSignal {
        asset: symbol.into(),
        signal: Signal::Hold,
        confidence: "LOW".into(),
        entry_price: snap.current_price,
        stop_loss: 0.0,
        take_profit: 0.0,
        risk_reward_ratio: 0.0,
        reasoning: reasoning.into(),
        warnings: Vec::new(),
        atr: snap.atr_14,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(price: f64) -> IndicatorSnapshot {
        IndicatorSnapshot {
            ema_50: 95.0,
            ema_200: 90.0,
            rsi_14: 50.0,
            macd_line: 0.0,
            macd_signal: 0.0,
            macd_histogram: 0.0,
            macd_crossed_bullish_recently: false,
            current_price: price,
            swing_low: 80.0,
            atr_14: 2.0,
        }
    }

    fn bar(i: u64, low: f64, high: f64, close: f64) -> Candle {
        Candle {
            open_time: i * 900_000,
            open: close,
            high,
            low,
            close,
            volume: 1.0,
            close_time: (i + 1) * 900_000 - 1,
        }
    }

    /// Flat bars at 100 (range 99-101), then one final bar closing at `last`.
    fn flat_then(last: f64) -> Vec<Candle> {
        let mut bars: Vec<Candle> = (0..30).map(|i| bar(i, 99.0, 101.0, 100.0)).collect();
        bars.push(bar(30, last - 0.5, last + 0.5, last));
        bars
    }

    #[test]
    fn trend_pullback_is_exactly_the_original_rules() {
        // Live behaviour must not depend on the signal candles passed in.
        let mut s = snap(103.0);
        s.macd_crossed_bullish_recently = true;
        s.rsi_14 = 45.0;
        let params = StrategyParams::default();
        let original = strategy::evaluate("X", &s, &params);
        for bars in [Vec::new(), flat_then(103.0)] {
            let via = StrategyKind::TrendPullback.evaluate("X", &s, &bars, &params);
            assert_eq!(via.signal, original.signal);
            assert_eq!(via.stop_loss, original.stop_loss);
            assert_eq!(via.take_profit, original.take_profit);
        }
    }

    #[test]
    fn names_round_trip() {
        for kind in StrategyKind::ALL {
            assert_eq!(StrategyKind::from_name(kind.name()), Some(kind));
        }
        assert_eq!(StrategyKind::from_name("martingale"), None);
    }

    #[test]
    fn breakout_buys_a_close_above_the_channel_in_an_uptrend() {
        let bars = flat_then(103.0);
        let s = StrategyKind::Breakout.evaluate("X", &snap(103.0), &bars, &Default::default());
        assert_eq!(s.signal, Signal::Buy);
        // 1.5 trend ATRs below entry, target at 2R.
        assert!((s.stop_loss - 100.0).abs() < 1e-9);
        assert!((s.take_profit - 109.0).abs() < 1e-9);
    }

    #[test]
    fn breakout_ignores_a_close_inside_the_channel() {
        let bars = flat_then(100.5);
        let s = StrategyKind::Breakout.evaluate("X", &snap(100.5), &bars, &Default::default());
        assert_eq!(s.signal, Signal::Hold);
    }

    #[test]
    fn breakout_stays_out_below_the_trend_ema() {
        let bars = flat_then(103.0);
        let mut below = snap(103.0);
        below.ema_200 = 110.0;
        let s = StrategyKind::Breakout.evaluate("X", &below, &bars, &Default::default());
        assert_ne!(s.signal, Signal::Buy);
    }

    #[test]
    fn breakout_exits_below_the_recent_low() {
        let bars = flat_then(97.0);
        let s = StrategyKind::Breakout.evaluate("X", &snap(97.0), &bars, &Default::default());
        assert_eq!(s.signal, Signal::Sell);
    }

    #[test]
    fn mean_reversion_buys_a_washout_above_the_trend_ema() {
        // A steady slide drives signal RSI toward zero.
        let bars: Vec<Candle> = (0..40)
            .map(|i| {
                let c = 120.0 - i as f64 * 0.5;
                bar(i, c - 0.1, c + 0.1, c)
            })
            .collect();
        let price = bars.last().unwrap().close;
        let s = StrategyKind::MeanReversion.evaluate("X", &snap(price), &bars, &Default::default());
        assert_eq!(s.signal, Signal::Buy, "{}", s.reasoning);
    }

    #[test]
    fn mean_reversion_exits_once_rsi_recovers() {
        let bars: Vec<Candle> = (0..40)
            .map(|i| {
                let c = 100.0 + i as f64 * 0.5;
                bar(i, c - 0.1, c + 0.1, c)
            })
            .collect();
        let price = bars.last().unwrap().close;
        let s = StrategyKind::MeanReversion.evaluate("X", &snap(price), &bars, &Default::default());
        assert_eq!(s.signal, Signal::Sell);
    }

    #[test]
    fn a_missing_atr_never_produces_a_buy() {
        let bars = flat_then(103.0);
        let mut no_atr = snap(103.0);
        no_atr.atr_14 = f64::NAN;
        let s = StrategyKind::Breakout.evaluate("X", &no_atr, &bars, &Default::default());
        assert_ne!(s.signal, Signal::Buy);
    }
}
