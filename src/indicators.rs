/// Compute an Exponential Moving Average over `values` with the given `period`.
/// Returns a Vec aligned with the input. The first `period - 1` entries are `NAN`
/// (insufficient data), and entry at index `period - 1` is seeded with the SMA.
pub fn ema(values: &[f64], period: usize) -> Vec<f64> {
    if values.len() < period || period == 0 {
        return vec![f64::NAN; values.len()];
    }

    let mut result = vec![f64::NAN; values.len()];

    // Seed: simple moving average of the first `period` values
    let sma: f64 = values[..period].iter().sum::<f64>() / period as f64;
    result[period - 1] = sma;

    let mult = 2.0 / (period as f64 + 1.0);
    for i in period..values.len() {
        result[i] = values[i] * mult + result[i - 1] * (1.0 - mult);
    }

    result
}

/// Average True Range over `period`, using Wilder's smoothing.
///
/// True Range is the widest of: this bar's range, the gap up from the previous
/// close, or the gap down from it — so it counts overnight gaps that a simple
/// high-minus-low ignores.
///
/// ATR expresses "normal movement" in price units, which is what makes it the
/// right yardstick for a signal buffer: a fixed percentage threshold is too
/// tight for a volatile asset and too loose for a calm one.
///
/// Returns a Vec aligned with the input; the first `period` entries are `NAN`.
pub fn atr(highs: &[f64], lows: &[f64], closes: &[f64], period: usize) -> Vec<f64> {
    let len = closes.len();
    if period == 0 || len <= period || highs.len() != len || lows.len() != len {
        return vec![f64::NAN; len];
    }

    let mut true_range = vec![f64::NAN; len];
    for i in 1..len {
        let previous_close = closes[i - 1];
        true_range[i] = (highs[i] - lows[i])
            .max((highs[i] - previous_close).abs())
            .max((lows[i] - previous_close).abs());
    }

    let mut result = vec![f64::NAN; len];
    // Seed with the simple mean of the first `period` true ranges, which start
    // at index 1 because the first bar has no previous close.
    let seed: f64 = true_range[1..=period].iter().sum::<f64>() / period as f64;
    result[period] = seed;

    for i in period + 1..len {
        result[i] = (result[i - 1] * (period as f64 - 1.0) + true_range[i]) / period as f64;
    }

    result
}

/// Compute RSI (Relative Strength Index) from close prices with the given `period`.
/// Uses the smoothed (Wilder) method. Returns NAN for indices with insufficient data.
pub fn rsi(closes: &[f64], period: usize) -> Vec<f64> {
    let len = closes.len();
    if len <= period {
        return vec![f64::NAN; len];
    }

    let mut result = vec![f64::NAN; len];

    // Separate gains and losses
    let mut gains = vec![0.0; len];
    let mut losses = vec![0.0; len];
    for i in 1..len {
        let change = closes[i] - closes[i - 1];
        if change > 0.0 {
            gains[i] = change;
        } else {
            losses[i] = -change;
        }
    }

    // First average: simple mean over the first `period` changes (indices 1..=period)
    let mut avg_gain: f64 = gains[1..=period].iter().sum::<f64>() / period as f64;
    let mut avg_loss: f64 = losses[1..=period].iter().sum::<f64>() / period as f64;

    result[period] = if avg_loss == 0.0 {
        100.0
    } else {
        100.0 - 100.0 / (1.0 + avg_gain / avg_loss)
    };

    // Smoothed averages for remaining values
    for i in (period + 1)..len {
        avg_gain = (avg_gain * (period as f64 - 1.0) + gains[i]) / period as f64;
        avg_loss = (avg_loss * (period as f64 - 1.0) + losses[i]) / period as f64;

        result[i] = if avg_loss == 0.0 {
            100.0
        } else {
            100.0 - 100.0 / (1.0 + avg_gain / avg_loss)
        };
    }

    result
}

/// MACD output: three parallel vectors all aligned with the input length.
pub struct MacdResult {
    pub macd_line: Vec<f64>,
    pub signal_line: Vec<f64>,
    pub histogram: Vec<f64>,
}

/// Compute MACD(fast, slow, signal_period) from close prices.
pub fn macd(closes: &[f64], fast: usize, slow: usize, signal_period: usize) -> MacdResult {
    let ema_fast = ema(closes, fast);
    let ema_slow = ema(closes, slow);

    // MACD line = EMA(fast) - EMA(slow)
    let macd_line: Vec<f64> = ema_fast
        .iter()
        .zip(ema_slow.iter())
        .map(|(f, s)| {
            if f.is_nan() || s.is_nan() {
                f64::NAN
            } else {
                f - s
            }
        })
        .collect();

    // Signal line = EMA of the valid (non-NAN) portion of the MACD line
    let valid_start = slow - 1; // first index where both EMAs are valid
    let valid_macd: Vec<f64> = macd_line[valid_start..].to_vec();
    let signal_ema = ema(&valid_macd, signal_period);

    let mut signal_line = vec![f64::NAN; valid_start];
    signal_line.extend_from_slice(&signal_ema);

    // Histogram = MACD line - signal line
    let histogram: Vec<f64> = macd_line
        .iter()
        .zip(signal_line.iter())
        .map(|(m, s)| {
            if m.is_nan() || s.is_nan() {
                f64::NAN
            } else {
                m - s
            }
        })
        .collect();

    MacdResult {
        macd_line,
        signal_line,
        histogram,
    }
}

/// Find the most recent swing low using a pivot-point method.
/// A swing low at index `i` means `lows[i]` is the minimum within `i-window..=i+window`.
/// Returns the price of the nearest (most recent) confirmed swing low, or falls back
/// to the minimum of the last 20 candles if no pivot is found.
pub fn find_nearest_swing_low(lows: &[f64], window: usize) -> Option<f64> {
    if lows.is_empty() {
        return None;
    }
    if lows.len() < 2 * window + 1 {
        return lows.iter().copied().reduce(f64::min);
    }

    let mut swing_lows: Vec<(usize, f64)> = Vec::new();

    for i in window..(lows.len() - window) {
        let current = lows[i];
        let is_swing = (1..=window).all(|j| current <= lows[i - j] && current <= lows[i + j]);
        if is_swing {
            swing_lows.push((i, current));
        }
    }

    swing_lows.last().map(|(_, price)| *price).or_else(|| {
        // Fallback: minimum of last 20 candles
        lows.iter().rev().take(20).copied().reduce(f64::min)
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_atr_basic_range() {
        // Every bar spans exactly 2.0 with no gaps, so ATR must converge on 2.0.
        let highs = vec![11.0, 12.0, 13.0, 14.0, 15.0, 16.0];
        let lows = vec![9.0, 10.0, 11.0, 12.0, 13.0, 14.0];
        let closes = vec![10.0, 11.0, 12.0, 13.0, 14.0, 15.0];
        let out = atr(&highs, &lows, &closes, 3);
        assert!(out[0].is_nan() && out[2].is_nan(), "warmup must be NAN");
        assert!(out[3].is_finite());
        assert!((out[5] - 2.0).abs() < 0.5, "got {}", out[5]);
    }

    #[test]
    fn test_atr_counts_gaps() {
        // A bar that gaps far above the previous close has a true range much
        // larger than its own high-minus-low.
        let highs = vec![11.0, 12.0, 13.0, 40.0];
        let lows = vec![9.0, 10.0, 11.0, 38.0];
        let closes = vec![10.0, 11.0, 12.0, 39.0];
        let out = atr(&highs, &lows, &closes, 3);
        // Final TR is 40 - 12 = 28, far beyond the 2.0 intrabar range.
        assert!(out[3] > 5.0, "gap ignored: {}", out[3]);
    }

    #[test]
    fn test_atr_insufficient_data() {
        let out = atr(&[1.0, 2.0], &[0.5, 1.5], &[1.0, 2.0], 14);
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|v| v.is_nan()));
    }

    #[test]
    fn test_atr_is_never_negative() {
        let highs = vec![11.0, 9.0, 13.0, 8.0, 15.0, 7.0];
        let lows = vec![9.0, 7.0, 11.0, 6.0, 13.0, 5.0];
        let closes = vec![10.0, 8.0, 12.0, 7.0, 14.0, 6.0];
        for v in atr(&highs, &lows, &closes, 3)
            .into_iter()
            .filter(|v| v.is_finite())
        {
            assert!(v >= 0.0, "ATR went negative: {v}");
        }
    }

    #[test]
    fn test_atr_mismatched_series_is_rejected() {
        assert!(atr(&[1.0, 2.0, 3.0], &[1.0], &[1.0, 2.0, 3.0], 2)
            .iter()
            .all(|v| v.is_nan()));
    }

    #[test]
    fn test_ema_basic() {
        let data = vec![10.0, 11.0, 12.0, 13.0, 14.0, 15.0, 16.0, 17.0, 18.0, 19.0];
        let result = ema(&data, 3);
        assert_eq!(result.len(), data.len());
        assert!(result[0].is_nan());
        assert!(result[1].is_nan());
        // SMA of first 3 = (10+11+12)/3 = 11.0
        assert!((result[2] - 11.0).abs() < 0.001);
        // Subsequent values should trend upward
        for i in 3..result.len() {
            assert!(result[i] > result[i - 1]);
        }
    }

    #[test]
    fn test_ema_insufficient_data() {
        let data = vec![1.0, 2.0];
        let result = ema(&data, 5);
        assert!(result.iter().all(|v| v.is_nan()));
    }

    #[test]
    fn test_rsi_range() {
        // Steadily rising prices should produce RSI near 100
        let data: Vec<f64> = (0..50).map(|i| 100.0 + i as f64).collect();
        let result = rsi(&data, 14);
        for &v in &result[14..] {
            assert!((0.0..=100.0).contains(&v), "RSI out of range: {v}");
        }
    }

    #[test]
    fn test_rsi_all_gains() {
        let data: Vec<f64> = (0..30).map(|i| 100.0 + i as f64).collect();
        let result = rsi(&data, 14);
        let last = *result.last().unwrap();
        assert!(
            last > 95.0,
            "Pure gains should yield RSI near 100, got {last}"
        );
    }

    #[test]
    fn test_rsi_all_losses() {
        let data: Vec<f64> = (0..30).map(|i| 100.0 - i as f64).collect();
        let result = rsi(&data, 14);
        let last = *result.last().unwrap();
        assert!(
            last < 5.0,
            "Pure losses should yield RSI near 0, got {last}"
        );
    }

    #[test]
    fn test_macd_lengths() {
        let data: Vec<f64> = (0..100).map(|i| 50.0 + (i as f64).sin() * 5.0).collect();
        let result = macd(&data, 12, 26, 9);
        assert_eq!(result.macd_line.len(), 100);
        assert_eq!(result.signal_line.len(), 100);
        assert_eq!(result.histogram.len(), 100);
    }

    #[test]
    fn test_swing_low_basic() {
        //                     0     1     2     3     4     5     6     7     8     9
        let lows = vec![10.0, 8.0, 5.0, 7.0, 9.0, 11.0, 6.0, 4.0, 7.0, 10.0];
        // window=2: index 2 (5.0) is a pivot, index 7 (4.0) is a pivot
        let result = find_nearest_swing_low(&lows, 2);
        assert_eq!(result, Some(4.0));
    }

    #[test]
    fn test_swing_low_fallback() {
        // Too few candles for any pivot with window=5
        let lows = vec![10.0, 9.0, 8.0, 7.0, 6.0];
        let result = find_nearest_swing_low(&lows, 5);
        assert_eq!(result, Some(6.0)); // falls back to min
    }
}
