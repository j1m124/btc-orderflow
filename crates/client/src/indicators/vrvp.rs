//! Visible-Range Volume Profile (VRVP) — overlay indicator that aggregates
//! per-price-bucket bid/ask volume across the chart's currently-visible bar
//! window. Shares all heavy lifting (params struct, compute, paint, settings
//! form) with the FRVP drawing tool via [`crate::volume_profile`]; this file
//! is just the [`IndicatorKind`] adapter.
//!
//! Phase 5 (this commit) wires the kind so it shows up in the picker, can be
//! added to the chart, and round-trips through persistence. `compute` returns
//! an empty [`VolumeProfileOutput`] until Phase 6 implements real aggregation;
//! the paint pipeline's stub arm draws nothing until Phase 7 lights it up.
//!
//! Pane semantics: `OverlayOnly` — VRVP draws bars anchored to the right
//! (default) edge of the candle pane, never as its own sub-pane. Per-bar
//! `y_range` is `None` so VRVP doesn't disturb the candle pane's auto-fit.

use std::any::Any;

use gpui::SharedString;
use serde::{Deserialize, Serialize};

use super::kind::{ComputeCtx, IndicatorKind, PaneKind};
use super::output::{IndicatorOutput, ValueReadout};
use crate::services::market_data::Candle;
use crate::volume_profile::{
    VolumeProfileOutput, VolumeProfileParams, compute_volume_profile,
};

/// One VRVP instance. Single field: the shared params struct (FRVP uses the
/// same struct inside its `DrawingShape::Frvp`). Serializing the wrapper
/// rather than `VolumeProfileParams` directly keeps the persisted JSON shape
/// extensible if the indicator ever grows VRVP-only fields.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct VrvpParams {
    pub params: VolumeProfileParams,
}

impl IndicatorKind for VrvpParams {
    fn kind_id(&self) -> &'static str {
        "vrvp"
    }

    fn pane_kind(&self) -> PaneKind {
        PaneKind::OverlayOnly
    }

    fn label(&self) -> SharedString {
        // Bucket size is the most useful single-glance summary — matches how
        // orderflow traders refer to a profile ("100-tick VRVP").
        format!("VRVP {}t", self.params.bucket_ticks).into()
    }

    fn compute(&self, candles: &[Candle], ctx: ComputeCtx<'_>) -> IndicatorOutput {
        // VRVP aggregates over the chart's visible bar window only — that's
        // the "visible range" in the name. Without a measured viewport the
        // window is undefined and we report empty (rather than over- or
        // under-counting against the full loaded buffer).
        let Some(range) = ctx.view_time_range else {
            return IndicatorOutput::VolumeProfile {
                output: VolumeProfileOutput::default(),
                params: self.params.clone(),
            };
        };
        // TF inferred from adjacent open_times (avoids plumbing the chart's
        // `Timeframe` through ComputeCtx for a one-call consumer). Falls to
        // 0 when fewer than 2 candles are loaded — `compute_volume_profile`
        // short-circuits in that case.
        let tf_ms = if candles.len() >= 2 {
            candles[1].open_time - candles[0].open_time
        } else {
            0
        };
        let cells = ctx
            .footprint
            .and_then(|lookup| lookup.cells_for_bucket(self.params.bucket_dollars()))
            .unwrap_or(&[]);
        let output = compute_volume_profile(cells, range, tf_ms, &self.params);
        IndicatorOutput::VolumeProfile {
            output,
            params: self.params.clone(),
        }
    }

    fn value_at(&self, _output: &IndicatorOutput, _index: usize) -> ValueReadout {
        // No crosshair interaction in v1 (per the design grilling). The chip
        // strip skips VP-shape outputs entirely.
        ValueReadout::One(None)
    }

    fn y_range(
        &self,
        _output: &IndicatorOutput,
        _range: std::ops::Range<usize>,
    ) -> Option<(f64, f64)> {
        // VRVP is price-bucket-keyed, not bar-keyed — it shouldn't pull the
        // candle pane's y-fit up or down. Returning `None` keeps the candle
        // auto-fit driven purely by visible candles.
        None
    }

    fn params_json(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or(serde_json::Value::Null)
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn color_slots(&self) -> Vec<SharedString> {
        // Five colors, matching the five `color_*` fields on
        // `VolumeProfileParams`. The settings form drives these through its
        // own color pickers (write-back into params directly) rather than
        // through `IndicatorInstance.colors`, but we still declare the slots
        // so the generic color-section in `IndicatorSettingsView` mirrors
        // the user's intent and the chip strip can render a representative
        // swatch.
        vec![
            "Volume".into(),
            "Bull".into(),
            "Bear".into(),
            "POC".into(),
            "VA".into(),
        ]
    }
}

