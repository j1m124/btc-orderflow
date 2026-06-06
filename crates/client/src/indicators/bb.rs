//! Bollinger Bands. Middle = SMA(period, source); upper = middle + N⋅σ;
//! lower = middle − N⋅σ. Population stddev. Paint draws three lines and
//! optionally a low-alpha fill between upper and lower.

use gpui::SharedString;
use serde::{Deserialize, Serialize};

use super::kind::{ComputeCtx, IndicatorKind, PaneKind, Source};
use super::math::{extract_source, rolling_sma, rolling_stddev};
use super::output::{IndicatorOutput, ValueReadout};
use crate::services::market_data::Candle;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BbParams {
    pub period: usize,
    pub stddev: f64,
    pub source: Source,
}

impl Default for BbParams {
    fn default() -> Self {
        Self {
            period: 20,
            stddev: 2.0,
            source: Source::Close,
        }
    }
}

impl IndicatorKind for BbParams {
    fn kind_id(&self) -> &'static str {
        "bb"
    }
    fn pane_kind(&self) -> PaneKind {
        PaneKind::OverlayOnly
    }
    fn label(&self) -> SharedString {
        // 0 → "BB(20, 2)"; non-integer → "BB(20, 2.5)". Matches TV's compact
        // form rather than always printing a trailing `.0`.
        let sd = if self.stddev.fract().abs() < f64::EPSILON {
            format!("{}", self.stddev as i64)
        } else {
            format!("{}", self.stddev)
        };
        format!("BB({}, {})", self.period, sd).into()
    }
    fn compute(&self, candles: &[Candle], _ctx: ComputeCtx) -> IndicatorOutput {
        let src = extract_source(candles, self.source);
        let middle = rolling_sma(&src, self.period);
        let sigma = rolling_stddev(&src, self.period);
        let n = src.len();
        let mut upper = vec![None; n];
        let mut lower = vec![None; n];
        for i in 0..n {
            if let (Some(m), Some(s)) = (middle[i], sigma[i]) {
                upper[i] = Some(m + self.stddev * s);
                lower[i] = Some(m - self.stddev * s);
            }
        }
        IndicatorOutput::Bands {
            upper,
            middle,
            lower,
        }
    }
    fn value_at(&self, output: &IndicatorOutput, index: usize) -> ValueReadout {
        match output {
            IndicatorOutput::Bands { upper, lower, .. } => ValueReadout::Two(
                upper.get(index).copied().flatten(),
                lower.get(index).copied().flatten(),
            ),
            _ => ValueReadout::Two(None, None),
        }
    }
    fn y_range(&self, output: &IndicatorOutput, range: std::ops::Range<usize>) -> Option<(f64, f64)> {
        let IndicatorOutput::Bands { upper, lower, .. } = output else {
            return None;
        };
        let lo_i = range.start.min(upper.len());
        let hi_i = range.end.min(upper.len());
        let mut min = f64::INFINITY;
        let mut max = f64::NEG_INFINITY;
        let mut any = false;
        for i in lo_i..hi_i {
            if let Some(u) = upper[i] {
                if u > max {
                    max = u;
                }
                any = true;
            }
            if let Some(l) = lower[i] {
                if l < min {
                    min = l;
                }
                any = true;
            }
        }
        any.then_some((min, max))
    }
    fn params_json(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or(serde_json::Value::Null)
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}
