//! MACD: `macd = EMA(fast) − EMA(slow)`, `signal = EMA(macd, signal)`,
//! `histogram = macd − signal`. Lives in its own pane below the candles.

use gpui::SharedString;
use serde::{Deserialize, Serialize};

use super::kind::{IndicatorKind, PaneKind, Source};
use super::math::{extract_source, rolling_macd};
use super::output::{IndicatorOutput, ValueReadout};
use crate::services::market_data::Candle;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MacdParams {
    pub fast: usize,
    pub slow: usize,
    pub signal: usize,
    pub source: Source,
}

impl Default for MacdParams {
    fn default() -> Self {
        Self {
            fast: 12,
            slow: 26,
            signal: 9,
            source: Source::Close,
        }
    }
}

impl IndicatorKind for MacdParams {
    fn kind_id(&self) -> &'static str {
        "macd"
    }
    fn pane_kind(&self) -> PaneKind {
        PaneKind::PaneOnly
    }
    fn label(&self) -> SharedString {
        format!("MACD({}, {}, {})", self.fast, self.slow, self.signal).into()
    }
    fn compute(&self, candles: &[Candle]) -> IndicatorOutput {
        let src = extract_source(candles, self.source);
        let (macd, signal, histogram) = rolling_macd(&src, self.fast, self.slow, self.signal);
        IndicatorOutput::Macd {
            macd,
            signal,
            histogram,
        }
    }
    fn value_at(&self, output: &IndicatorOutput, index: usize) -> ValueReadout {
        match output {
            IndicatorOutput::Macd {
                macd,
                signal,
                histogram,
            } => ValueReadout::Three(
                macd.get(index).copied().flatten(),
                signal.get(index).copied().flatten(),
                histogram.get(index).copied().flatten(),
            ),
            _ => ValueReadout::Three(None, None, None),
        }
    }
    fn y_range(&self, output: &IndicatorOutput, range: std::ops::Range<usize>) -> Option<(f64, f64)> {
        let IndicatorOutput::Macd {
            macd,
            signal,
            histogram,
        } = output
        else {
            return None;
        };
        let lo_i = range.start.min(macd.len());
        let hi_i = range.end.min(macd.len());
        let mut min = f64::INFINITY;
        let mut max = f64::NEG_INFINITY;
        let mut any = false;
        for series in [macd, signal, histogram] {
            for v in series[lo_i..hi_i].iter().filter_map(|v| *v) {
                if v < min {
                    min = v;
                }
                if v > max {
                    max = v;
                }
                any = true;
            }
        }
        // Ensure zero is in range so the histogram has a visible zero-line.
        if any {
            if min > 0.0 {
                min = 0.0;
            }
            if max < 0.0 {
                max = 0.0;
            }
            Some((min, max))
        } else {
            None
        }
    }
    fn params_json(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or(serde_json::Value::Null)
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
    fn color_slots(&self) -> Vec<SharedString> {
        // Slot 0: macd line + histogram tint. Slot 1: signal line.
        vec![SharedString::from("Color"), SharedString::from("Signal color")]
    }
}
