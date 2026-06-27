//! Orderbook heatmap as an overlay indicator — a thin façade over the existing
//! heatmap render layer.
//!
//! Unlike every other kind, the heatmap does **not** compute a per-bar series or
//! paint through the indicator pipeline. Its real render is a GPU-texture blit
//! drawn *behind* the candles, owned by [`crate::panels::chart::HeatmapSettings`]
//! / `HeatmapLayer` and driven by its own book subscription + 1s sampler (see
//! `panels/chart/paint/heatmap.rs`). Making it an indicator buys UX uniformity:
//! it lives in the "+ Indicator" picker and the chip row, and its settings live
//! in the standard indicator settings panel.
//!
//! The wiring:
//! - **State** lives here, in [`OrderbookHeatmapParams`] — the single source of
//!   truth, persisted via `IndicatorPrefs` like any kind. `ChartState` reads
//!   these params and syncs them into the `HeatmapLayer` before each texture
//!   refresh; the heavy texture-cache path is otherwise untouched.
//! - **`compute`** returns the no-op [`IndicatorOutput::Heatmap`] marker; the
//!   overlay/pane paint passes skip it.
//! - **Settings** come from a bespoke stateful view (the two-handle logarithmic
//!   colour slider the declarative form can't express), returned via
//!   [`IndicatorKind::custom_settings_view`].
//! - **Singleton** — one book / one cache per chart (see
//!   [`crate::indicators::is_singleton_kind`]).

use std::any::Any;

use gpui::{AnyView, App, AppContext as _, SharedString, WeakEntity, Window};
use serde::{Deserialize, Serialize};

use super::instance::InstanceId;
use super::kind::{ComputeCtx, CustomSettingsBuilder, IndicatorKind, PaneKind};
use super::output::{IndicatorOutput, ValueReadout};
use crate::panels::ContentPanel;
use crate::panels::chart::{HeatmapSettings, HeatmapSettingsView};
use crate::services::market_data::Candle;

/// Per-instance heatmap params. Wraps the render-layer [`HeatmapSettings`]
/// directly so there's a single settings struct: the settings view edits
/// `.settings`, and `ChartState` copies `.settings` into the `HeatmapLayer`.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct OrderbookHeatmapParams {
    #[serde(default)]
    pub settings: HeatmapSettings,
}

impl IndicatorKind for OrderbookHeatmapParams {
    fn kind_id(&self) -> &'static str {
        "ob_heatmap"
    }

    fn pane_kind(&self) -> PaneKind {
        PaneKind::OverlayOnly
    }

    fn label(&self) -> SharedString {
        SharedString::from("Orderbook Heatmap")
    }

    fn compute(&self, _candles: &[Candle], _ctx: ComputeCtx<'_>) -> IndicatorOutput {
        // Façade marker — the real render runs behind the candles via
        // `HeatmapLayer`, fed from these params by `ChartState`.
        IndicatorOutput::Heatmap
    }

    fn value_at(&self, _output: &IndicatorOutput, _index: usize) -> ValueReadout {
        // No per-bar readout; the chip shows just the name.
        ValueReadout::Empty
    }

    fn y_range(
        &self,
        _output: &IndicatorOutput,
        _range: std::ops::Range<usize>,
    ) -> Option<(f64, f64)> {
        // Drawn within the candle price band; contributes nothing to auto-fit.
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

    fn custom_settings_view(&self) -> Option<CustomSettingsBuilder> {
        // The two-handle log colour slider is stateful, so hand the settings
        // panel a builder for the bespoke `HeatmapSettingsView` (which reads /
        // writes this instance's params via the standard `IndicatorTarget`).
        Some(Box::new(
            |panel: WeakEntity<ContentPanel>, id: InstanceId, window: &mut Window, cx: &mut App| {
                let view = cx.new(|cx| HeatmapSettingsView::new(panel, id, window, cx));
                AnyView::from(view)
            },
        ))
    }
}
