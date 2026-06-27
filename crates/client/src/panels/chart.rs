//! Chart panel — a thin facade over focused submodules. See each submodule's
//! own header for detail:
//! - [`actions`] — gpui action types dispatched at the panel.
//! - [`coords`] — pure coordinate <-> screen math + label formatting.
//! - [`drawing`] — the drawing model, edit/creation state, and hit-testing.
//! - [`state`] — [`ChartState`], the panel's model + all mutation methods.
//! - [`view`] — the gpui [`render`] tree and its sub-builders.
//!
//! The pre-existing [`footprint`], [`footprint_settings`], [`drawings_view`],
//! and [`paint`] submodules are unchanged. The re-exports below cover (a) the
//! surface `panels.rs` consumes and (b) the few items the `paint/` submodules
//! reach via `super::super::` — kept here so `paint/` needs no edits.

mod actions;
mod coords;
mod drawing;
mod drawings_view;
mod footprint;
mod footprint_settings;
mod heatmap_settings;
mod liq_heatmap_settings;
mod paint;
mod state;
mod view;

pub use actions::{
    ChangeChartRender, ChangeChartTimeframe, ChangeChartVolumeUnit, GoToLatest,
    MoveIndicatorPaneDown, MoveIndicatorPaneUp, RemoveIndicator, ResetChartScale,
    ToggleChartRenderVisible, ToggleIndicatorHidden,
};
pub use footprint::{
    ColorScope, FootprintParams, RenderKind, RenderMetric, TextMetric, WireframeVariant,
};
pub use footprint_settings::{ChartRenderSettingsView, OpenChartRenderSettings};
pub use heatmap_settings::HeatmapSettingsView;
pub use liq_heatmap_settings::LiqHeatmapSettingsView;
pub use paint::{
    COLOR_RANGE_MAX, COLOR_RANGE_MIN, HEATMAP_DEPTH, HeatmapSettings, LIQ_COLOR_RANGE_MAX,
    LIQ_COLOR_RANGE_MIN,
};
pub use state::ChartState;
pub use view::render;

// Consumed by the `paint/` submodules via `super::super::{Drawing, DrawingId,
// index_to_screen, price_to_screen, time_to_idx}`. Private re-exports: visible
// to the chart module and all its descendants (including `paint/`), so those
// files compile unchanged without widening the items' visibility to the crate.
//
// `time_to_idx` (in `drawings_view`) is the time→fractional-index mapping the
// heatmap paint needs to place 1s book columns on the candle x-axis. It is a
// pure coordinate helper that happens to live next to the drawing code; the
// heatmap reuses it rather than duplicating its gap-safe binary-search.
use coords::{index_to_screen, price_to_screen};
use drawing::Drawing;
use drawings_view::time_to_idx;

use crate::drawings::service::DrawingId;
