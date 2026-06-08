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
use crate::volume_profile::{VolumeProfileOutput, VolumeProfileParams};

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

    fn compute(&self, _candles: &[Candle], _ctx: ComputeCtx<'_>) -> IndicatorOutput {
        // Phase 5 stub. `compute_volume_profile` exists (Phase 6) but
        // wiring it here needs a `tf_ms` value plumbed through ComputeCtx
        // — that lands as part of Phase 7 alongside the paint arm so we
        // don't introduce another ctx field with no consumer.
        IndicatorOutput::VolumeProfile(VolumeProfileOutput::default())
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
