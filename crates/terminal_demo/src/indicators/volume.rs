//! Per-bar volume histogram. Hybrid kind — defaults to overlay (small
//! histogram pinned to the bottom of the price pane), can be toggled to
//! pane mode in settings. Per-bar polarity uses `close >= open` so
//! up-bars and down-bars get distinct tinting (paint pass owns the
//! actual colors; we just emit the boolean).

use gpui::SharedString;
use serde::{Deserialize, Serialize};

use super::kind::{IndicatorKind, PaneKind};
use super::output::{IndicatorOutput, ValueReadout};
use crate::services::market_data::Candle;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct VolumeParams {
    // Empty for v1. The "Placement" toggle lives on the IndicatorInstance,
    // not on the params struct, so it travels uniformly with other
    // hybrid kinds (if/when we add more).
}

impl IndicatorKind for VolumeParams {
    fn kind_id(&self) -> &'static str {
        "volume"
    }
    fn pane_kind(&self) -> PaneKind {
        PaneKind::Both
    }
    fn label(&self) -> SharedString {
        "Volume".into()
    }
    fn compute(&self, candles: &[Candle]) -> IndicatorOutput {
        let values: Vec<Option<f64>> = candles.iter().map(|c| Some(c.volume)).collect();
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
        // Volume floor is always 0 — histograms anchor at zero, never showing
        // a clipped range that hides bar height.
        any.then_some((0.0, max))
    }
    fn params_json(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or(serde_json::Value::Null)
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
    fn color_slots(&self) -> Vec<SharedString> {
        // Volume bars use the theme's bullish/bearish colors per-bar; the
        // settings panel exposes no configurable color.
        Vec::new()
    }
}
