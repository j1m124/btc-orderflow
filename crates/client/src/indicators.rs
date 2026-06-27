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
pub mod funding;
pub mod instance;
pub mod kind;
pub mod liq_bars;
pub mod liq_heatmap;
pub mod math;
pub mod net_ls;
pub mod ob_heatmap;
pub mod ob_imbalance;
pub mod open_interest;
pub mod output;
pub mod trades;
pub mod volume;
pub mod volume_delta;
pub mod vrvp;

pub use bar_stat::{BarStatGrade, BarStatParams};
pub use bb::BbParams;
pub use funding::{FundingParams, FundingRenderMode};
pub use liq_bars::{LiqBarsScale, LiquidationBarsParams};
pub use liq_heatmap::LiqHeatmapParams;
pub use net_ls::NetLsParams;
pub use ob_heatmap::OrderbookHeatmapParams;
pub use ob_imbalance::ObImbalanceParams;
pub use open_interest::{OiRenderMode, OpenInterestParams};
pub use instance::{
    COLOR_PALETTE_SIZE, IndicatorInstance, InstanceId, bump_next_id_past, default_pane_height,
    new_instance_id, palette_color_for,
};
pub use kind::{
    ComputeCtx, CustomSettingsBuilder, IndicatorKind, PaneKind, Placement, Source,
};
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
            name: "VRVP".into(),
            description: "Visible Range Volume Profile — per-price-bucket volume/delta histogram across the visible bar range".into(),
            category: Category::Volume,
            spawn: || Box::new(VrvpParams::default()),
        },
        KindEntry {
            kind_id: "liq_bars",
            name: "Liq Bars".into(),
            description: "Per-bar liquidation histogram — long-liq below 0, short-liq above; axis unit follows the chart's Coin/USD toggle".into(),
            category: Category::Volume,
            spawn: || Box::new(LiquidationBarsParams::default()),
        },
        KindEntry {
            kind_id: "open_interest",
            name: "Open Interest".into(),
            description: "Total open interest as a direction-colored line (or OHLC candles); axis unit follows the chart's Coin/USD toggle".into(),
            category: Category::Volume,
            spawn: || Box::new(OpenInterestParams::default()),
        },
        KindEntry {
            kind_id: "funding",
            name: "Funding".into(),
            description: "Perpetual funding rate as a signed histogram (or line); positive = longs pay, shown in percent".into(),
            category: Category::Volume,
            spawn: || Box::new(FundingParams::default()),
        },
        KindEntry {
            kind_id: "net_ls",
            name: "Net L/S".into(),
            description: "Net long/short flow from taker delta + OI change (signed histogram); long buildup / short covering up, short buildup / long exit down".into(),
            category: Category::Volume,
            spawn: || Box::new(NetLsParams::default()),
        },
        KindEntry {
            kind_id: "ob_imbalance",
            name: "OB Imbalance".into(),
            description: "Order-book bid/ask imbalance within a depth band (signed histogram in [-1,+1]); bid-heavy up, ask-heavy down".into(),
            category: Category::Volume,
            spawn: || Box::new(ObImbalanceParams::default()),
        },
        KindEntry {
            kind_id: "ob_heatmap",
            name: "Orderbook Heatmap".into(),
            description: "Resting order-book liquidity as a colour heatmap behind the candles; brighter = larger resting size. Singleton.".into(),
            category: Category::Volume,
            spawn: || Box::new(OrderbookHeatmapParams::default()),
        },
        KindEntry {
            kind_id: "liq_heatmap",
            name: "Liquidation Heatmap".into(),
            description: "Predictive liquidation magnets behind the candles: estimated un-liquidated leverage from OI + taker delta, brighter = denser cluster. Singleton.".into(),
            category: Category::Volume,
            spawn: || Box::new(LiqHeatmapParams::default()),
        },
    ]
}

/// Kinds that may exist at most once per chart. The picker hides the entry while
/// one is present and [`crate::panels::ContentPanel::add_indicator_from_picker`]
/// refuses a duplicate. The heatmap is singleton because there is one order book
/// / one texture cache per chart — a second instance would be redundant work.
pub fn is_singleton_kind(kind_id: &str) -> bool {
    matches!(kind_id, "ob_heatmap" | "liq_heatmap")
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
        "liq_bars" => serde_json::from_value::<LiquidationBarsParams>(params.clone())
            .ok()
            .map(|p| Box::new(p) as Box<dyn IndicatorKind>),
        "open_interest" => serde_json::from_value::<OpenInterestParams>(params.clone())
            .ok()
            .map(|p| Box::new(p) as Box<dyn IndicatorKind>),
        "funding" => serde_json::from_value::<FundingParams>(params.clone())
            .ok()
            .map(|p| Box::new(p) as Box<dyn IndicatorKind>),
        "net_ls" => serde_json::from_value::<NetLsParams>(params.clone())
            .ok()
            .map(|p| Box::new(p) as Box<dyn IndicatorKind>),
        "ob_imbalance" => serde_json::from_value::<ObImbalanceParams>(params.clone())
            .ok()
            .map(|p| Box::new(p) as Box<dyn IndicatorKind>),
        "ob_heatmap" => serde_json::from_value::<OrderbookHeatmapParams>(params.clone())
            .ok()
            .map(|p| Box::new(p) as Box<dyn IndicatorKind>),
        "liq_heatmap" => serde_json::from_value::<LiqHeatmapParams>(params.clone())
            .ok()
            .map(|p| Box::new(p) as Box<dyn IndicatorKind>),
        // Bollinger Bands is retained in-tree but isn't user-spawnable. Legacy
        // persisted state referencing removed kinds (ma_suite, macd, rsi) or
        // bb is dropped on load.
        _ => None,
    }
}
