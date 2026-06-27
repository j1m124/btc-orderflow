//! Liquidation heatmap as an overlay indicator — a thin façade over a dedicated
//! render layer, mirroring [`crate::indicators::ob_heatmap`] exactly.
//!
//! Unlike most kinds it computes no per-bar series: its real render is a
//! GPU-texture blit drawn *behind* the candles, owned by
//! [`crate::panels::chart::paint`]'s `LiqHeatmapLayer` and driven by a forward
//! simulation over the candle / open-interest / mark-price data `ChartState`
//! already gathers (see [`sim`]). A **pure-client** indicator — zero
//! protocol/server change, all inputs are already on the wire.
//!
//! The wiring mirrors the orderbook heatmap:
//! - **State** lives here in [`LiqHeatmapParams`] (the single source of truth,
//!   persisted via `IndicatorPrefs`). `ChartState` reads it each frame and
//!   feeds the sim + the layer's mirror fields before the texture refresh.
//! - **`compute`** returns the no-op [`IndicatorOutput::Heatmap`] marker.
//! - **Settings** come from the bespoke stateful [`LiqHeatmapSettingsView`]
//!   (the two-handle log colour slider the declarative form can't express).
//! - **Singleton** — one sim / one texture cache per chart.

pub mod sim;

use std::any::Any;

use gpui::{AnyView, App, AppContext as _, SharedString, WeakEntity, Window};
use serde::{Deserialize, Serialize};

use super::instance::InstanceId;
use super::kind::{ComputeCtx, CustomSettingsBuilder, IndicatorKind, PaneKind};
use super::output::{IndicatorOutput, ValueReadout};
use crate::panels::ContentPanel;
use crate::panels::chart::{HeatmapSettings, LiqHeatmapSettingsView};
use crate::services::market_data::Candle;

/// Default maintenance-margin rate (0.4%). Flat across tiers in v1; tiered
/// Binance brackets are deferred.
pub const DEFAULT_MMR: f64 = 0.004;

/// Default warm-up lookback (24h). The sim runs this far left of the visible
/// window so positions opened earlier still magnetize the visible range.
pub const DEFAULT_LOOKBACK_MS: i64 = 24 * 60 * 60 * 1000;

/// BTCUSDT-perp price increment — one **tick** = $0.10. The bucket width is
/// stored in dollars (the sim/render do price math in dollars) but presented to
/// the user as a count of ticks.
pub const TICK_SIZE: f64 = 0.1;

/// Default price-bucket width in dollars ($5 = 50 ticks).
pub const DEFAULT_BUCKET: f64 = sim::DEFAULT_PRICE_BUCKET;

/// Min / max price-bucket width the user may enter, in **ticks**. Floor is one
/// exchange tick; the ceiling ($10000) is a coarse cap.
pub const MIN_BUCKET_TICKS: i64 = 1;
pub const MAX_BUCKET_TICKS: i64 = 100_000;

/// Liquidation-heatmap colour-range defaults (coin/contract units — the sim
/// stores `ΔOI × split`, see [`sim`]). Larger than the orderbook heatmap's
/// because a magnet bucket accumulates contributions across the lookback.
pub const DEFAULT_LIQ_COLOR_LO: f64 = 5.0;
pub const DEFAULT_LIQ_COLOR_PEAK: f64 = 500.0;

/// The settings struct defaults to liquidation-specific colour bounds (the
/// derived `HeatmapSettings::default()` is tuned for the orderbook heatmap's
/// much smaller coin sizes). Used both for a fresh instance and for the
/// `#[serde(default)]` fill when an older persisted blob omits `settings`.
fn default_liq_settings() -> HeatmapSettings {
    HeatmapSettings {
        color_lo: DEFAULT_LIQ_COLOR_LO,
        color_peak: DEFAULT_LIQ_COLOR_PEAK,
        ..HeatmapSettings::default()
    }
}

/// Per-instance params. Wraps the shared render-layer [`HeatmapSettings`]
/// (colour range / opacity / text / extend-right) plus the two sim knobs.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct LiqHeatmapParams {
    /// Maintenance-margin rate (fraction).
    #[serde(default = "default_mmr")]
    pub mmr: f64,
    /// Warm-up lookback in ms.
    #[serde(default = "default_lookback_ms")]
    pub lookback_ms: i64,
    /// Price-bucket width ("tick size") in dollars.
    #[serde(default = "default_bucket")]
    pub bucket: f64,
    #[serde(default = "default_liq_settings")]
    pub settings: HeatmapSettings,
}

fn default_mmr() -> f64 {
    DEFAULT_MMR
}
fn default_lookback_ms() -> i64 {
    DEFAULT_LOOKBACK_MS
}
fn default_bucket() -> f64 {
    DEFAULT_BUCKET
}

impl Default for LiqHeatmapParams {
    fn default() -> Self {
        Self {
            mmr: DEFAULT_MMR,
            lookback_ms: DEFAULT_LOOKBACK_MS,
            bucket: DEFAULT_BUCKET,
            settings: default_liq_settings(),
        }
    }
}

impl LiqHeatmapParams {
    /// The sim inputs derived from these params.
    pub fn sim_params(&self) -> sim::SimParams {
        sim::SimParams {
            mmr: self.mmr,
            lookback_ms: self.lookback_ms,
            bucket: self.bucket,
        }
    }
}

impl IndicatorKind for LiqHeatmapParams {
    fn kind_id(&self) -> &'static str {
        "liq_heatmap"
    }

    fn pane_kind(&self) -> PaneKind {
        PaneKind::OverlayOnly
    }

    fn label(&self) -> SharedString {
        SharedString::from("Liquidation Heatmap")
    }

    fn compute(&self, _candles: &[Candle], _ctx: ComputeCtx<'_>) -> IndicatorOutput {
        // Façade marker — the real render runs behind the candles via the
        // `LiqHeatmapLayer`, fed from these params by `ChartState`.
        IndicatorOutput::Heatmap
    }

    fn value_at(&self, _output: &IndicatorOutput, _index: usize) -> ValueReadout {
        ValueReadout::Empty
    }

    fn y_range(
        &self,
        _output: &IndicatorOutput,
        _range: std::ops::Range<usize>,
    ) -> Option<(f64, f64)> {
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
        Some(Box::new(
            |panel: WeakEntity<ContentPanel>, id: InstanceId, window: &mut Window, cx: &mut App| {
                let view = cx.new(|cx| LiqHeatmapSettingsView::new(panel, id, window, cx));
                AnyView::from(view)
            },
        ))
    }
}
