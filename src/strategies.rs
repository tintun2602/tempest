//! The strategies the bot can trade, behind one switch.
//!
//! Every strategy decides from the same inputs the live loop already fetches:
//! the trend-interval snapshot (EMA50/EMA200, RSI and ATR on `TREND_INTERVAL`
//! candles) plus the raw `SIGNAL_INTERVAL` candles. The lab replays exactly
//! these calls, so a lab result describes the code that would run live.

use crate::exchange::Candle;
use crate::indicators;
use crate::strategy::{
    self, IndicatorSnapshot, ShortDecision, Signal, StrategyParams, TradeSignal,
};

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
    /// Close above the highest high of the last `DONCHIAN_ENTRY` signal bars
    /// with the trend EMA50 above EMA200; out below the `DONCHIAN_EXIT` low.
    Donchian,
    /// Signal-interval EMA20 crossing up through EMA50 while price is above
    /// the trend EMA200; out when EMA20 falls back under EMA50.
    EmaCross,
    /// Close above the upper Bollinger band right after a volatility squeeze,
    /// price above the trend EMA200; out below the middle line.
    Squeeze,
}

/// One day of bars on the live 15m signal interval. A five-hour channel fired
/// so often that trading costs, not the signal, decided the result.
const BREAKOUT_LOOKBACK: usize = 96;
// The channel plus the bar being evaluated must fit in the lab's signal
// window (the 100 bars `main.rs` also requests), or the strategy never trades.
const _: () = assert!(BREAKOUT_LOOKBACK < crate::lab::SIGNAL_WINDOW);
/// Breakout exit: a close below the lowest low of this many signal bars.
const BREAKOUT_EXIT_LOOKBACK: usize = 10;
const DONCHIAN_ENTRY: usize = 20;
const DONCHIAN_EXIT: usize = 10;
const EMA_FAST: usize = 20;
const EMA_SLOW: usize = 50;
const BB_PERIOD: usize = 20;
const BB_MULT: f64 = 2.0;
/// Squeeze: the previous bar's bandwidth must rank among the lowest
/// `SQUEEZE_PCT` percent of the last `SQUEEZE_WINDOW` bandwidths.
const SQUEEZE_WINDOW: usize = 80;
const SQUEEZE_PCT: usize = 20;
const _: () = assert!(SQUEEZE_WINDOW + BB_PERIOD <= crate::lab::SIGNAL_WINDOW);
const OVERSOLD: f64 = 30.0;
const RECOVERED: f64 = 60.0;
/// Mean-reversion short: signal RSI above `OVERBOUGHT` below the trend EMA200,
/// covered once it falls back under `FADED`. Mirrors of the long thresholds.
const OVERBOUGHT: f64 = 70.0;
const FADED: f64 = 40.0;
/// Stops for the new strategies sit this many *trend-interval* ATRs below
/// entry. A signal-interval ATR is so tight on 15m bars that fees and noise
/// would decide most trades.
const STOP_ATR: f64 = 1.5;

impl StrategyKind {
    pub const ALL: [StrategyKind; 6] = [
        StrategyKind::TrendPullback,
        StrategyKind::Breakout,
        StrategyKind::MeanReversion,
        StrategyKind::Donchian,
        StrategyKind::EmaCross,
        StrategyKind::Squeeze,
    ];

    pub fn name(self) -> &'static str {
        match self {
            StrategyKind::TrendPullback => "trend_pullback",
            StrategyKind::Breakout => "breakout",
            StrategyKind::MeanReversion => "mean_reversion",
            StrategyKind::Donchian => "donchian",
            StrategyKind::EmaCross => "ema_cross",
            StrategyKind::Squeeze => "squeeze",
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
            StrategyKind::Donchian => donchian(symbol, snap, signal_candles),
            StrategyKind::EmaCross => ema_cross(symbol, snap, signal_candles),
            StrategyKind::Squeeze => squeeze(symbol, snap, signal_candles),
        }
    }
}

impl StrategyKind {
    /// The short-side mirror of [`Self::evaluate`], for the lab's futures
    /// mode. Same inputs, same no-look-ahead contract.
    pub fn evaluate_short(
        self,
        snap: &IndicatorSnapshot,
        signal_candles: &[Candle],
        params: &StrategyParams,
    ) -> ShortDecision {
        match self {
            StrategyKind::TrendPullback => strategy::evaluate_short(snap, params),
            StrategyKind::Breakout => breakout_short(snap, signal_candles),
            StrategyKind::MeanReversion => mean_reversion_short(snap, signal_candles),
            StrategyKind::Donchian => donchian_short(snap, signal_candles),
            StrategyKind::EmaCross => ema_cross_short(snap, signal_candles),
            StrategyKind::Squeeze => squeeze_short(snap, signal_candles),
        }
    }
}

/// Close below the lowest low of the channel while under EMA200; covered on a
/// close above the recent high.
fn breakout_short(snap: &IndicatorSnapshot, bars: &[Candle]) -> ShortDecision {
    let price = snap.current_price;
    let n = bars.len();
    if n < BREAKOUT_LOOKBACK + 1 {
        return ShortDecision::Hold;
    }
    let channel_low = bars[n - 1 - BREAKOUT_LOOKBACK..n - 1]
        .iter()
        .map(|c| c.low)
        .fold(f64::MAX, f64::min);
    let exit_high = bars[n.saturating_sub(1 + BREAKOUT_EXIT_LOOKBACK)..n - 1]
        .iter()
        .map(|c| c.high)
        .fold(f64::MIN, f64::max);
    if price > exit_high {
        return ShortDecision::Cover;
    }
    if price < channel_low && price < snap.ema_200 {
        return short_with_atr_stop(snap);
    }
    ShortDecision::Hold
}

fn mean_reversion_short(snap: &IndicatorSnapshot, bars: &[Candle]) -> ShortDecision {
    let closes: Vec<f64> = bars.iter().map(|c| c.close).collect();
    let Some(rsi) = indicators::rsi(&closes, 14)
        .last()
        .copied()
        .filter(|v| v.is_finite())
    else {
        return ShortDecision::Hold;
    };
    if rsi < FADED {
        return ShortDecision::Cover;
    }
    if rsi > OVERBOUGHT && snap.current_price < snap.ema_200 {
        return short_with_atr_stop(snap);
    }
    ShortDecision::Hold
}

/// A short with its stop `STOP_ATR` trend ATRs above price. No usable ATR, no
/// trade.
fn short_with_atr_stop(snap: &IndicatorSnapshot) -> ShortDecision {
    if !(snap.atr_14.is_finite() && snap.atr_14 > 0.0) {
        return ShortDecision::Hold;
    }
    ShortDecision::Enter {
        stop_loss: snap.current_price + STOP_ATR * snap.atr_14,
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

/// Highest high and lowest low of the `n` bars before the last one.
fn prior_range(bars: &[Candle], n: usize) -> (f64, f64) {
    let len = bars.len();
    let prior = &bars[len.saturating_sub(1 + n)..len - 1];
    let high = prior.iter().map(|c| c.high).fold(f64::MIN, f64::max);
    let low = prior.iter().map(|c| c.low).fold(f64::MAX, f64::min);
    (high, low)
}

fn donchian(symbol: &str, snap: &IndicatorSnapshot, bars: &[Candle]) -> TradeSignal {
    let price = snap.current_price;
    if bars.len() < DONCHIAN_ENTRY + 1 {
        return hold(symbol, snap, "not enough signal bars");
    }
    let (channel_high, _) = prior_range(bars, DONCHIAN_ENTRY);
    let (_, exit_low) = prior_range(bars, DONCHIAN_EXIT);
    if price < exit_low {
        return sell(
            symbol,
            snap,
            format!("Close {price:.2} below {DONCHIAN_EXIT}-bar low {exit_low:.2}"),
        );
    }
    if price > channel_high && snap.ema_50 > snap.ema_200 {
        return buy_with_atr_stop(
            symbol,
            snap,
            format!(
                "Donchian: {price:.2} above {DONCHIAN_ENTRY}-bar high {channel_high:.2}, \
                 EMA50 {:.2} above EMA200 {:.2}",
                snap.ema_50, snap.ema_200
            ),
        );
    }
    hold(symbol, snap, "no donchian breakout")
}

fn donchian_short(snap: &IndicatorSnapshot, bars: &[Candle]) -> ShortDecision {
    let price = snap.current_price;
    if bars.len() < DONCHIAN_ENTRY + 1 {
        return ShortDecision::Hold;
    }
    let (_, channel_low) = prior_range(bars, DONCHIAN_ENTRY);
    let (exit_high, _) = prior_range(bars, DONCHIAN_EXIT);
    if price > exit_high {
        return ShortDecision::Cover;
    }
    if price < channel_low && snap.ema_50 < snap.ema_200 {
        return short_with_atr_stop(snap);
    }
    ShortDecision::Hold
}

/// (previous, current) of EMA20 and EMA50 on the signal closes, if both exist.
fn ema_pair(bars: &[Candle]) -> Option<((f64, f64), (f64, f64))> {
    let closes: Vec<f64> = bars.iter().map(|c| c.close).collect();
    let n = closes.len();
    if n < EMA_SLOW + 1 {
        return None;
    }
    let fast = indicators::ema(&closes, EMA_FAST);
    let slow = indicators::ema(&closes, EMA_SLOW);
    let v = [fast[n - 2], slow[n - 2], fast[n - 1], slow[n - 1]];
    if v.iter().any(|x| !x.is_finite()) {
        return None;
    }
    Some(((v[0], v[1]), (v[2], v[3])))
}

fn ema_cross(symbol: &str, snap: &IndicatorSnapshot, bars: &[Candle]) -> TradeSignal {
    let Some(((pf, ps), (f, s))) = ema_pair(bars) else {
        return hold(symbol, snap, "not enough signal bars");
    };
    if f < s {
        return sell(
            symbol,
            snap,
            format!("EMA{EMA_FAST} {f:.2} below EMA{EMA_SLOW} {s:.2}"),
        );
    }
    if pf <= ps && f > s && snap.current_price > snap.ema_200 {
        return buy_with_atr_stop(
            symbol,
            snap,
            format!(
                "EMA{EMA_FAST} crossed above EMA{EMA_SLOW}, above EMA200 {:.2}",
                snap.ema_200
            ),
        );
    }
    hold(symbol, snap, "no ema cross")
}

fn ema_cross_short(snap: &IndicatorSnapshot, bars: &[Candle]) -> ShortDecision {
    let Some(((pf, ps), (f, s))) = ema_pair(bars) else {
        return ShortDecision::Hold;
    };
    if f > s {
        return ShortDecision::Cover;
    }
    if pf >= ps && f < s && snap.current_price < snap.ema_200 {
        return short_with_atr_stop(snap);
    }
    ShortDecision::Hold
}

/// Bollinger(20, 2) at bar `i` of `closes`: (upper, middle, lower).
fn bollinger_at(closes: &[f64], i: usize) -> (f64, f64, f64) {
    let w = &closes[i + 1 - BB_PERIOD..=i];
    let mid = w.iter().sum::<f64>() / BB_PERIOD as f64;
    let var = w.iter().map(|c| (c - mid).powi(2)).sum::<f64>() / BB_PERIOD as f64;
    let sd = var.sqrt();
    (mid + BB_MULT * sd, mid, mid - BB_MULT * sd)
}

fn bandwidth_at(closes: &[f64], i: usize) -> f64 {
    let (up, mid, lo) = bollinger_at(closes, i);
    (up - lo) / mid
}

/// Bands of the last bar plus whether the bar before it was in a squeeze.
/// `None` until `SQUEEZE_WINDOW` earlier bandwidths exist.
fn squeeze_state(bars: &[Candle]) -> Option<(bool, (f64, f64, f64))> {
    let closes: Vec<f64> = bars.iter().map(|c| c.close).collect();
    let n = closes.len();
    if n < SQUEEZE_WINDOW + BB_PERIOD {
        return None;
    }
    let prev = bandwidth_at(&closes, n - 2);
    if !prev.is_finite() {
        return None;
    }
    // Bandwidths of the SQUEEZE_WINDOW bars ending with the previous bar.
    let lower = (n - 1 - SQUEEZE_WINDOW..n - 1)
        .filter(|&i| bandwidth_at(&closes, i) < prev)
        .count();
    let squeezed = lower < SQUEEZE_WINDOW * SQUEEZE_PCT / 100;
    Some((squeezed, bollinger_at(&closes, n - 1)))
}

fn squeeze(symbol: &str, snap: &IndicatorSnapshot, bars: &[Candle]) -> TradeSignal {
    let price = snap.current_price;
    let Some((squeezed, (upper, mid, _))) = squeeze_state(bars) else {
        return hold(symbol, snap, "not enough signal bars");
    };
    if price < mid {
        return sell(
            symbol,
            snap,
            format!("Close {price:.2} below Bollinger middle {mid:.2}"),
        );
    }
    if squeezed && price > upper && price > snap.ema_200 {
        return buy_with_atr_stop(
            symbol,
            snap,
            format!(
                "Squeeze breakout: {price:.2} above upper band {upper:.2}, above EMA200 {:.2}",
                snap.ema_200
            ),
        );
    }
    hold(symbol, snap, "no squeeze breakout")
}

fn squeeze_short(snap: &IndicatorSnapshot, bars: &[Candle]) -> ShortDecision {
    let price = snap.current_price;
    let Some((squeezed, (_, mid, lower))) = squeeze_state(bars) else {
        return ShortDecision::Hold;
    };
    if price > mid {
        return ShortDecision::Cover;
    }
    if squeezed && price < lower && price < snap.ema_200 {
        return short_with_atr_stop(snap);
    }
    ShortDecision::Hold
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
            macd_crossed_bearish_recently: false,
            swing_high: 110.0,
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
        let n = BREAKOUT_LOOKBACK as u64;
        let mut bars: Vec<Candle> = (0..n).map(|i| bar(i, 99.0, 101.0, 100.0)).collect();
        bars.push(bar(n, last - 0.5, last + 0.5, last));
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
    fn breakout_holds_without_a_full_channel() {
        let mut bars = flat_then(103.0);
        bars.remove(0);
        let s = StrategyKind::Breakout.evaluate("X", &snap(103.0), &bars, &Default::default());
        assert_eq!(s.signal, Signal::Hold);
        assert_eq!(s.reasoning, "not enough signal bars");
    }

    #[test]
    fn breakout_channel_reaches_back_the_full_lookback() {
        // A high in the oldest channel bar still caps the breakout.
        let mut bars = flat_then(103.0);
        bars[0] = bar(0, 99.0, 105.0, 100.0);
        let s = StrategyKind::Breakout.evaluate("X", &snap(103.0), &bars, &Default::default());
        assert_ne!(s.signal, Signal::Buy);
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
    fn breakout_shorts_a_close_below_the_channel_in_a_downtrend() {
        let bars = flat_then(97.0);
        let mut below = snap(97.0);
        below.ema_200 = 105.0;
        let d = StrategyKind::Breakout.evaluate_short(&below, &bars, &Default::default());
        // 1.5 trend ATRs above entry.
        assert_eq!(d, ShortDecision::Enter { stop_loss: 100.0 });
        // Above EMA200 it stays out.
        let d = StrategyKind::Breakout.evaluate_short(&snap(97.0), &bars, &Default::default());
        assert_ne!(d, ShortDecision::Enter { stop_loss: 100.0 });
    }

    #[test]
    fn breakout_covers_above_the_recent_high() {
        let bars = flat_then(103.0);
        let d = StrategyKind::Breakout.evaluate_short(&snap(103.0), &bars, &Default::default());
        assert_eq!(d, ShortDecision::Cover);
    }

    #[test]
    fn mean_reversion_shorts_a_spike_below_the_trend_ema_and_covers_on_a_fade() {
        let rising: Vec<Candle> = (0..40)
            .map(|i| {
                let c = 80.0 + i as f64 * 0.5;
                bar(i, c - 0.1, c + 0.1, c)
            })
            .collect();
        let price = rising.last().unwrap().close;
        let mut below = snap(price);
        below.ema_200 = 120.0;
        let d = StrategyKind::MeanReversion.evaluate_short(&below, &rising, &Default::default());
        assert!(matches!(d, ShortDecision::Enter { .. }), "{d:?}");

        let falling: Vec<Candle> = (0..40)
            .map(|i| {
                let c = 120.0 - i as f64 * 0.5;
                bar(i, c - 0.1, c + 0.1, c)
            })
            .collect();
        let price = falling.last().unwrap().close;
        let d =
            StrategyKind::MeanReversion.evaluate_short(&snap(price), &falling, &Default::default());
        assert_eq!(d, ShortDecision::Cover);
    }

    #[test]
    fn a_missing_atr_never_produces_a_buy() {
        let bars = flat_then(103.0);
        let mut no_atr = snap(103.0);
        no_atr.atr_14 = f64::NAN;
        let s = StrategyKind::Breakout.evaluate("X", &no_atr, &bars, &Default::default());
        assert_ne!(s.signal, Signal::Buy);
    }

    fn closes_to_bars(closes: &[f64]) -> Vec<Candle> {
        closes
            .iter()
            .enumerate()
            .map(|(i, &c)| bar(i as u64, c - 0.1, c + 0.1, c))
            .collect()
    }

    fn eval_long(kind: StrategyKind, s: &IndicatorSnapshot, bars: &[Candle]) -> TradeSignal {
        kind.evaluate("X", s, bars, &Default::default())
    }

    fn eval_short(kind: StrategyKind, s: &IndicatorSnapshot, bars: &[Candle]) -> ShortDecision {
        kind.evaluate_short(s, bars, &Default::default())
    }

    fn enter(price: f64) -> ShortDecision {
        ShortDecision::Enter { stop_loss: price + 3.0 }
    }

    // Donchian: 25 flat bars (99-101) then a final bar.
    fn donchian_bars(last: f64) -> Vec<Candle> {
        let mut bars: Vec<Candle> = (0..25).map(|i| bar(i, 99.0, 101.0, 100.0)).collect();
        bars.push(bar(25, last - 0.5, last + 0.5, last));
        bars
    }

    #[test]
    fn donchian_long_short_and_exits() {
        let k = StrategyKind::Donchian;
        let s = eval_long(k, &snap(103.0), &donchian_bars(103.0));
        assert_eq!(s.signal, Signal::Buy, "{}", s.reasoning);
        // EMA50 below EMA200 blocks the long.
        let mut down = snap(103.0);
        down.ema_50 = 85.0;
        assert_eq!(eval_long(k, &down, &donchian_bars(103.0)).signal, Signal::Hold);
        // Exit below the 10-bar low.
        assert_eq!(eval_long(k, &snap(97.0), &donchian_bars(97.0)).signal, Signal::Sell);
        // Short: close under the 20-bar low with EMA50 < EMA200.
        let mut down97 = snap(97.0);
        down97.ema_50 = 85.0;
        assert_eq!(eval_short(k, &down97, &donchian_bars(97.0)), enter(97.0));
        // Uptrend blocks the short; cover above the 10-bar high.
        assert_eq!(eval_short(k, &snap(97.0), &donchian_bars(97.0)), ShortDecision::Hold);
        assert_eq!(eval_short(k, &snap(103.0), &donchian_bars(103.0)), ShortDecision::Cover);
        // Too few bars.
        assert_eq!(eval_short(k, &down97, &donchian_bars(97.0)[10..]), ShortDecision::Hold);
    }

    /// Flat 100s, a rally and a fall make EMA20 cross EMA50 on the last bar.
    fn cross_up_bars() -> Vec<Candle> {
        // 60 bars sliding from 110 to 100.5, then rising until EMA20 > EMA50
        // for the first time; the returned set ends exactly on that bar.
        let mut closes: Vec<f64> = (0..60).map(|i| 110.0 - i as f64 * 0.16).collect();
        let mut last = *closes.last().unwrap();
        loop {
            last += 0.8;
            closes.push(last);
            let n = closes.len();
            let f = indicators::ema(&closes, EMA_FAST);
            let s = indicators::ema(&closes, EMA_SLOW);
            if f[n - 1] > s[n - 1] {
                return closes_to_bars(&closes);
            }
            assert!(n < 200, "no cross");
        }
    }

    fn mirror(bars: &[Candle]) -> Vec<Candle> {
        let closes: Vec<f64> = bars.iter().map(|c| 200.0 - c.close).collect();
        closes_to_bars(&closes)
    }

    #[test]
    fn ema_cross_long_short_and_exits() {
        let k = StrategyKind::EmaCross;
        let up = cross_up_bars();
        let price = up.last().unwrap().close;
        let s = eval_long(k, &snap(price), &up);
        assert_eq!(s.signal, Signal::Buy, "{}", s.reasoning);
        // Below EMA200 it stays out.
        let mut under = snap(price);
        under.ema_200 = price + 10.0;
        assert_eq!(eval_long(k, &under, &up).signal, Signal::Hold);
        // Exit while EMA20 < EMA50: the slide before the cross.
        let slide: Vec<Candle> = closes_to_bars(
            &(0..60).map(|i| 110.0 - i as f64 * 0.16).collect::<Vec<_>>(),
        );
        assert_eq!(eval_long(k, &snap(100.0), &slide).signal, Signal::Sell);
        // Mirror image: cross down, price below EMA200.
        let down = mirror(&up);
        let dprice = down.last().unwrap().close;
        let mut d = snap(dprice);
        d.ema_200 = dprice + 10.0;
        assert_eq!(eval_short(k, &d, &down), enter(dprice));
        let mut above = snap(dprice);
        above.ema_200 = dprice - 10.0;
        assert_eq!(eval_short(k, &above, &down), ShortDecision::Hold);
        // Cover while EMA20 > EMA50: the mirrored slide is a rise.
        let rise = mirror(&slide);
        assert_eq!(eval_short(k, &snap(100.0), &rise), ShortDecision::Cover);
    }

    /// 80 quiet-ish bars with wide swings, then 20 very tight bars (the
    /// squeeze), then a final bar closing at `last`.
    fn squeeze_bars(last: f64) -> Vec<Candle> {
        let mut closes: Vec<f64> = (0..60)
            .map(|i| 100.0 + if i % 2 == 0 { 3.0 } else { -3.0 })
            .collect();
        closes.extend((0..39).map(|i| 100.0 + if i % 2 == 0 { 0.05 } else { -0.05 }));
        closes.push(last);
        closes_to_bars(&closes)
    }

    #[test]
    fn squeeze_long_short_and_exits() {
        let k = StrategyKind::Squeeze;
        let up = squeeze_bars(101.0);
        let s = eval_long(k, &snap(101.0), &up);
        assert_eq!(s.signal, Signal::Buy, "{}", s.reasoning);
        // Below EMA200 it stays out.
        let mut under = snap(101.0);
        under.ema_200 = 105.0;
        assert_eq!(eval_long(k, &under, &up).signal, Signal::Hold);
        // Exit below the middle line.
        assert_eq!(eval_long(k, &snap(99.5), &squeeze_bars(99.5)).signal, Signal::Sell);
        // No squeeze (wide bands throughout): no entry.
        let wide: Vec<f64> = (0..100)
            .map(|i| 100.0 + (1.0 + i as f64 * 0.05) * if i % 2 == 0 { 1.0 } else { -1.0 })
            .collect();
        let mut wide_bars = closes_to_bars(&wide[..99]);
        wide_bars.push(bar(99, 109.5, 110.5, 110.0));
        assert_ne!(eval_long(k, &snap(110.0), &wide_bars).signal, Signal::Buy);
        // Short mirror: close below the lower band, price under EMA200.
        let dn = squeeze_bars(99.0);
        let mut d = snap(99.0);
        d.ema_200 = 105.0;
        assert_eq!(eval_short(k, &d, &dn), enter(99.0));
        assert_eq!(eval_short(k, &snap(99.0), &dn), ShortDecision::Hold);
        // Cover above the middle line.
        assert_eq!(eval_short(k, &snap(100.5), &squeeze_bars(100.5)), ShortDecision::Cover);
    }
}
