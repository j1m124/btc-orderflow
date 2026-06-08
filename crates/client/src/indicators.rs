//! Chart indicators. Surviving built-ins after the BTC-orderflow strip:
//! Volume, Trades (both pane-only histograms) and Bollinger Bands (overlay,
//! retained in-tree but not user-spawnable). Per-chart `Vec<IndicatorInstance>`
//! on `ChartState`; stateless trait with per-instance output cache; recompute
//! on each `apply_tick` / `tick_clock` / params edit.
//!
//! Settings UI is hand-written per kind and hosted in a singleton floating
//! window. Picker is a `SymbolPicker`-style modal.
//! Persistence keyed by chart-id under `terminal_demo.indicators.v1`.

pub mod bar_stat;
pub mod bb;
pub mod instance;
pub mod kind;
pub mod math;
pub mod output;
pub mod trades;
pub mod volume;
pub mod volume_delta;
pub mod vrvp;

pub use bar_stat::{BarStatGrade, BarStatParams};
pub use bb::BbParams;
pub use instance::{
    COLOR_PALETTE_SIZE, IndicatorInstance, InstanceId, bump_next_id_past, default_pane_height,
    new_instance_id, palette_color_for,
};
pub use kind::{ComputeCtx, IndicatorKind, PaneKind, Placement, Source};
pub use output::{IndicatorOutput, Series, ValueReadout};
pub use trades::TradesParams;
pub use volume::VolumeParams;
pub use volume_delta::{VolumeDeltaMode, VolumeDeltaParams};
pub use vrvp::VrvpParams;

use gpui::SharedString;

/// Category buckets surfaced as section headers in the picker modal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Category {
    Overlay,
    Volume,
    Oscillator,
    // `Custom` — added when scripting lands.
}

impl Category {
    pub fn label(&self) -> &'static str {
        match self {
            Category::Overlay => "Overlays",
            Category::Volume => "Volume",
            Category::Oscillator => "Oscillators",
        }
    }
}

/// One entry in the picker modal. Spawning is via the boxed factory so the
/// caller doesn't need to know per-kind types — Enter on the highlighted
/// row produces a fresh defaulted instance.
pub struct KindEntry {
    pub kind_id: &'static str,
    pub name: SharedString,
    pub description: SharedString,
    pub category: Category,
    pub spawn: fn() -> Box<dyn IndicatorKind>,
}

/// Available kinds in the picker. Stripped to Volume + Trades for the BTC
/// orderflow fork; the other built-ins (MA Suite, Bollinger Bands, MACD, RSI)
/// remain in the tree but aren't user-spawnable.
pub fn kind_entries() -> Vec<KindEntry> {
    vec![
        KindEntry {
            kind_id: "volume",
            name: "Volume".into(),
            description: "Per-bar volume histogram".into(),
            category: Category::Volume,
            spawn: || Box::new(VolumeParams::default()),
        },
        KindEntry {
            kind_id: "trades",
            name: "Trades".into(),
            description: "Per-bar trade count histogram".into(),
            category: Category::Volume,
            spawn: || Box::new(TradesParams::default()),
        },
        KindEntry {
            kind_id: "volume_delta",
            name: "Volume Delta".into(),
            description: "Per-bar buy vs sell taker volume (histogram, CVD, or both)".into(),
            category: Category::Volume,
            spawn: || Box::new(VolumeDeltaParams::default()),
        },
        KindEntry {
            kind_id: "bar_stat",
            name: "Bar Stats".into(),
            description: "Per-bar volume + delta row, with optional heatmap grading".into(),
            category: Category::Volume,
            spawn: || Box::new(BarStatParams::default()),
        },
        KindEntry {
            kind_id: "vrvp",
            name: "Visible Range Volume Profile".into(),
            description: "Per-price-bucket volume/delta histogram across the visible bar range".into(),
            category: Category::Volume,
            spawn: || Box::new(VrvpParams::default()),
        },
    ]
}

/// Rebuild a boxed `dyn IndicatorKind` from a `kind_id` + serialized params.
/// Used by the persistence loader to reconstruct instances from
/// `local_storage`. Returns `None` for unknown kind_ids (typically when a
/// future version's persisted state is read by an older build).
pub fn build_kind(
    kind_id: &str,
    params: &serde_json::Value,
) -> Option<Box<dyn IndicatorKind>> {
    match kind_id {
        "volume" => serde_json::from_value::<VolumeParams>(params.clone())
            .ok()
            .map(|p| Box::new(p) as Box<dyn IndicatorKind>),
        "trades" => serde_json::from_value::<TradesParams>(params.clone())
            .ok()
            .map(|p| Box::new(p) as Box<dyn IndicatorKind>),
        "volume_delta" => serde_json::from_value::<VolumeDeltaParams>(params.clone())
            .ok()
            .map(|p| Box::new(p) as Box<dyn IndicatorKind>),
        "bar_stat" => serde_json::from_value::<BarStatParams>(params.clone())
            .ok()
            .map(|p| Box::new(p) as Box<dyn IndicatorKind>),
        "vrvp" => serde_json::from_value::<VrvpParams>(params.clone())
            .ok()
            .map(|p| Box::new(p) as Box<dyn IndicatorKind>),
        // Bollinger Bands is retained in-tree but isn't user-spawnable. Legacy
        // persisted state referencing removed kinds (ma_suite, macd, rsi) or
        // bb is dropped on load.
        _ => None,
    }
}
