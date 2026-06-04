//! Rolling-window math helpers shared by the built-in indicator kinds. All
//! functions are pure — they take a slice and return owned output buffers,
//! no incremental state. Matches the v1 compute model: full recompute on
//! each tick, output cached on the chart.
//!
//! Output buffers are `Vec<Option<f64>>` so leading bars without enough
//! history surface as `None` and the paint pipeline skips them, without
//! dragging a sentinel like NaN through downstream code.

use super::kind::Source;
use crate::services::market_data::Candle;

/// Extract a per-bar price-source series from candles. Length matches input.
pub fn extract_source(candles: &[Candle], source: Source) -> Vec<f64> {
    candles
        .iter()
        .map(|c| match source {
            Source::Close => c.close,
            Source::Open => c.open,
            Source::High => c.high,
            Source::Low => c.low,
            Source::Hl2 => (c.high + c.low) * 0.5,
            Source::Ohlc4 => (c.open + c.high + c.low + c.close) * 0.25,
        })
        .collect()
}

/// Min/max of the `Some` values in a series window. Used by single-line
/// indicators (RSI) and by the MA Suite's combined y-range.
pub fn line_range(s: &[Option<f64>], range: std::ops::Range<usize>) -> Option<(f64, f64)> {
    let lo_i = range.start.min(s.len());
    let hi_i = range.end.min(s.len());
    let mut min = f64::INFINITY;
    let mut max = f64::NEG_INFINITY;
    let mut any = false;
    for v in s[lo_i..hi_i].iter().filter_map(|v| *v) {
        if v < min {
            min = v;
        }
        if v > max {
            max = v;
        }
        any = true;
    }
    any.then_some((min, max))
}

/// Rolling simple moving average. The first `period - 1` outputs are
/// `None`. Implementation uses a sliding sum so each bar costs O(1).
pub fn rolling_sma(values: &[f64], period: usize) -> Vec<Option<f64>> {
    let n = values.len();
    let mut out = vec![None; n];
    if period == 0 || period > n {
        return out;
    }
    let mut sum: f64 = values[..period].iter().sum();
    out[period - 1] = Some(sum / period as f64);
    for i in period..n {
        sum += values[i] - values[i - period];
        out[i] = Some(sum / period as f64);
    }
    out
}

/// Rolling exponential moving average. Seeded with the SMA over the first
/// `period` values (standard TA convention), then the EMA recurrence
/// `e_i = α·v_i + (1 - α)·e_{i-1}` with `α = 2 / (period + 1)`.
/// First `period - 1` outputs are `None`.
pub fn rolling_ema(values: &[f64], period: usize) -> Vec<Option<f64>> {
    let n = values.len();
    let mut out = vec![None; n];
    if period == 0 || period > n {
        return out;
    }
    let alpha = 2.0 / (period as f64 + 1.0);
    let seed: f64 = values[..period].iter().sum::<f64>() / period as f64;
    let mut ema = seed;
    out[period - 1] = Some(ema);
    for i in period..n {
        ema = (values[i] - ema) * alpha + ema;
        out[i] = Some(ema);
    }
    out
}

/// Rolling population standard deviation (divides by N, not N-1) over a
/// window of `period`. Used by Bollinger Bands to compute the ±N⋅σ
/// envelopes. First `period - 1` outputs are `None`.
///
/// Two-pass per window for numerical stability — N is small (typically 20)
/// so the O(period) per-bar cost is fine.
pub fn rolling_stddev(values: &[f64], period: usize) -> Vec<Option<f64>> {
    let n = values.len();
    let mut out = vec![None; n];
    if period == 0 || period > n {
        return out;
    }
    for i in (period - 1)..n {
        let window = &values[i + 1 - period..=i];
        let mean: f64 = window.iter().sum::<f64>() / period as f64;
        let var: f64 =
            window.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / period as f64;
        out[i] = Some(var.sqrt());
    }
    out
}

/// Wilder's smoothed RSI. Output is in 0..=100. The first `period` outputs
/// are `None` (we need `period + 1` candles for the first ratio). Wilder's
/// recurrence uses α = 1/period rather than the standard EMA's
/// 2/(period+1).
pub fn rolling_rsi(values: &[f64], period: usize) -> Vec<Option<f64>> {
    let n = values.len();
    let mut out = vec![None; n];
    if period == 0 || n < period + 1 {
        return out;
    }
    let mut sum_gain = 0.0;
    let mut sum_loss = 0.0;
    for i in 1..=period {
        let diff = values[i] - values[i - 1];
        if diff >= 0.0 {
            sum_gain += diff;
        } else {
            sum_loss += -diff;
        }
    }
    let mut avg_gain = sum_gain / period as f64;
    let mut avg_loss = sum_loss / period as f64;
    out[period] = Some(rsi_from(avg_gain, avg_loss));
    let alpha = 1.0 / period as f64;
    for i in (period + 1)..n {
        let diff = values[i] - values[i - 1];
        let (g, l) = if diff >= 0.0 { (diff, 0.0) } else { (0.0, -diff) };
        avg_gain += alpha * (g - avg_gain);
        avg_loss += alpha * (l - avg_loss);
        out[i] = Some(rsi_from(avg_gain, avg_loss));
    }
    out
}

fn rsi_from(gain: f64, loss: f64) -> f64 {
    if loss == 0.0 {
        return 100.0;
    }
    let rs = gain / loss;
    100.0 - (100.0 / (1.0 + rs))
}

/// Compute MACD: `macd = EMA(fast) - EMA(slow)`, `signal = EMA(macd, signal)`,
/// `histogram = macd - signal`. Returned tuple parallels `IndicatorOutput::Macd`.
/// Leading bars without enough history surface as `None` in each series.
pub fn rolling_macd(
    values: &[f64],
    fast: usize,
    slow: usize,
    signal: usize,
) -> (Vec<Option<f64>>, Vec<Option<f64>>, Vec<Option<f64>>) {
    let ema_fast = rolling_ema(values, fast);
    let ema_slow = rolling_ema(values, slow);
    let n = values.len();
    let mut macd = vec![None; n];
    for i in 0..n {
        if let (Some(f), Some(s)) = (ema_fast[i], ema_slow[i]) {
            macd[i] = Some(f - s);
        }
    }
    // EMA(signal, signal_period) needs to skip the leading `None`s of the
    // MACD line. We collect the dense MACD values, smooth them, then re-map
    // the smoothed series back into the original index space.
    let macd_dense: Vec<(usize, f64)> = macd
        .iter()
        .enumerate()
        .filter_map(|(i, v)| v.map(|x| (i, x)))
        .collect();
    let dense_values: Vec<f64> = macd_dense.iter().map(|(_, v)| *v).collect();
    let dense_signal = rolling_ema(&dense_values, signal);
    let mut signal_out = vec![None; n];
    for (k, (i, _)) in macd_dense.iter().enumerate() {
        if let Some(s) = dense_signal[k] {
            signal_out[*i] = Some(s);
        }
    }
    let mut hist = vec![None; n];
    for i in 0..n {
        if let (Some(m), Some(s)) = (macd[i], signal_out[i]) {
            hist[i] = Some(m - s);
        }
    }
    (macd, signal_out, hist)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wasm_bindgen_test::wasm_bindgen_test;

    #[wasm_bindgen_test]
    fn sma_first_value_is_period_average() {
        let v = vec![1.0, 2.0, 3.0, 4.0, 5.0];
        let out = rolling_sma(&v, 3);
        assert_eq!(out[0], None);
        assert_eq!(out[1], None);
        assert_eq!(out[2], Some(2.0));
        assert_eq!(out[3], Some(3.0));
        assert_eq!(out[4], Some(4.0));
    }

    #[wasm_bindgen_test]
    fn ema_seed_equals_sma_at_period_minus_one() {
        let v = vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0];
        let ema = rolling_ema(&v, 3);
        // Seed = (1+2+3)/3 = 2.0
        assert_eq!(ema[2], Some(2.0));
        // Next: α = 2/4 = 0.5; ema = (4 - 2) * 0.5 + 2 = 3.0
        assert_eq!(ema[3], Some(3.0));
    }

    #[wasm_bindgen_test]
    fn rsi_all_gains_saturates_at_100() {
        let v: Vec<f64> = (1..=20).map(|x| x as f64).collect();
        let rsi = rolling_rsi(&v, 14);
        // With all gains, avg_loss = 0 → RSI = 100.
        assert_eq!(rsi[14], Some(100.0));
    }

    #[wasm_bindgen_test]
    fn stddev_of_constant_is_zero() {
        let v = vec![5.0; 10];
        let s = rolling_stddev(&v, 4);
        assert_eq!(s[3], Some(0.0));
        assert_eq!(s[9], Some(0.0));
    }
}
