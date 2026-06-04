//! Relative Strength Index. Single line bounded 0..=100 with conventional
//! 30/70 over/oversold guides. Wilder's smoothing (see `math::rolling_rsi`).

use gpui::SharedString;
use serde::{Deserialize, Serialize};

use super::kind::{IndicatorKind, PaneKind, Source};
use super::math::{extract_source, rolling_rsi};
use super::output::{IndicatorOutput, ValueReadout};
use crate::services::market_data::Candle;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RsiParams {
    pub period: usize,
    pub source: Source,
    pub overbought: f64,
    pub oversold: f64,
}

impl Default for RsiParams {
    fn default() -> Self {
        Self {
            period: 14,
            source: Source::Close,
            overbought: 70.0,
            oversold: 30.0,
        }
    }
}

impl IndicatorKind for RsiParams {
    fn kind_id(&self) -> &'static str {
        "rsi"
    }
    fn pane_kind(&self) -> PaneKind {
        PaneKind::PaneOnly
    }
    fn label(&self) -> SharedString {
        format!("RSI {}", self.period).into()
    }
    fn compute(&self, candles: &[Candle]) -> IndicatorOutput {
        let src = extract_source(candles, self.source);
        IndicatorOutput::Line(rolling_rsi(&src, self.period))
    }
    fn value_at(&self, output: &IndicatorOutput, index: usize) -> ValueReadout {
        match output {
            IndicatorOutput::Line(s) => ValueReadout::One(s.get(index).copied().flatten()),
            _ => ValueReadout::One(None),
        }
    }
    fn y_range(&self, _output: &IndicatorOutput, _range: std::ops::Range<usize>) -> Option<(f64, f64)> {
        // RSI is bounded — always show the full 0..100 band so the
        // overbought/oversold guides stay in fixed positions.
        Some((0.0, 100.0))
    }
    fn params_json(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or(serde_json::Value::Null)
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}
