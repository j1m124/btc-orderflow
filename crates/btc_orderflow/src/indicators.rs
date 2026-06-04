//! Chart indicators. v1 design (locked in via /grill-me):
//!
//! - Per-chart `Vec<IndicatorInstance>` on `ChartState`; stateless trait with
//!   per-instance output cache; recompute on each `apply_tick` / `tick_clock`
//!   / params edit.
//! - Six built-ins: SMA, EMA, Bollinger Bands, Volume, MACD, RSI.
//! - Overlay indicators paint inside the main candle pane; pane indicators
//!   live in their own sub-canvas below it.
//! - Settings UI is hand-written per kind and hosted in a singleton floating
//!   window. Picker is a `SymbolPicker`-style modal.
//! - Persistence keyed by chart-id under `terminal_demo.indicators.v1`.

pub mod bb;
pub mod instance;
pub mod kind;
pub mod ma_suite;
pub mod macd;
pub mod math;
pub mod output;
pub mod rsi;
pub mod session_vwap;
pub mod trades;
pub mod volume;

pub use bb::BbParams;
pub use instance::{
    COLOR_PALETTE_SIZE, IndicatorInstance, InstanceId, default_pane_height, new_instance_id,
    palette_color_for,
};
pub use kind::{IndicatorKind, PaneKind, Placement, Source};
pub use ma_suite::{MaEntry, MaFlavor, MaSuiteParams};
pub use macd::MacdParams;
pub use output::{IndicatorOutput, Series, ValueReadout};
pub use rsi::RsiParams;
pub use session_vwap::SessionVwapParams;
pub use trades::TradesParams;
pub use volume::VolumeParams;

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

/// Available kinds in the picker. Stripped to Volume-only for the BTC
/// orderflow fork; the other built-ins (MA Suite, Bollinger Bands, Session
/// VWAP, Trades, MACD, RSI) remain in the tree but aren't user-spawnable.
pub fn kind_entries() -> Vec<KindEntry> {
    vec![
        KindEntry {
            kind_id: "volume",
            name: "Volume".into(),
            description: "Per-bar volume histogram".into(),
            category: Category::Volume,
            spawn: || Box::new(VolumeParams::default()),
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
        // The other built-ins (ma_suite, bb, session_vwap, trades, macd, rsi)
        // are no longer user-spawnable. If a legacy persisted state references
        // them they're dropped on load.
        _ => None,
    }
}
