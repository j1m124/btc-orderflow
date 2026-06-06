//! Per-bar trade count histogram. Pane-only (Volume already owns the
//! overlay bottom-of-pane spot). Each bar's value = `candle.trades` cast to
//! f64; `None` when the server hasn't yet populated trade count for that
//! bar (e.g., a developing minute on a feed without `n`/`z`).
//!
//! Polarity (`up: close >= open`) mirrors the Volume histogram so the up/down
//! tint reads consistently across the two panes.

use gpui::SharedString;
use serde::{Deserialize, Serialize};

use super::kind::{ComputeCtx, IndicatorKind, PaneKind};
use super::output::{IndicatorOutput, ValueReadout};
use crate::services::market_data::Candle;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TradesParams {}

impl IndicatorKind for TradesParams {
    fn kind_id(&self) -> &'static str {
        "trades"
    }
    fn pane_kind(&self) -> PaneKind {
        PaneKind::PaneOnly
    }
    fn label(&self) -> SharedString {
        "Trades".into()
    }
    fn compute(&self, candles: &[Candle], _ctx: ComputeCtx) -> IndicatorOutput {
        let values: Vec<Option<f64>> = candles
            .iter()
            .map(|c| c.trades.map(|n| n as f64))
            .collect();
        let up: Vec<bool> = candles.iter().map(|c| c.close >= c.open).collect();
        IndicatorOutput::Histogram { values, up }
    }
    fn value_at(&self, output: &IndicatorOutput, index: usize) -> ValueReadout {
        match output {
            IndicatorOutput::Histogram { values, .. } => {
                ValueReadout::One(values.get(index).copied().flatten())
            }
            _ => ValueReadout::One(None),
        }
    }
    fn y_range(&self, output: &IndicatorOutput, range: std::ops::Range<usize>) -> Option<(f64, f64)> {
        let IndicatorOutput::Histogram { values, .. } = output else {
            return None;
        };
        let lo_i = range.start.min(values.len());
        let hi_i = range.end.min(values.len());
        let mut max = f64::NEG_INFINITY;
        let mut any = false;
        for v in values[lo_i..hi_i].iter().filter_map(|v| *v) {
            if v > max {
                max = v;
            }
            any = true;
        }
        any.then_some((0.0, max))
    }
    fn params_json(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or(serde_json::Value::Null)
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
    fn color_slots(&self) -> Vec<SharedString> {
        Vec::new()
    }
}
