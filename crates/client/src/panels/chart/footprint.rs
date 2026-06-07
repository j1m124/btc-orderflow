//! Footprint render-mode types: render kind discriminator + per-mode params.
//!
//! Three render kinds live behind one chart panel — `Candlestick` (the
//! existing path, default seed) and two footprint variants (`Cluster`,
//! `Profile`). Switching is driven by a header dropdown next to TF; the
//! active render also surfaces as the topmost chip in the chart's vertical
//! indicator list (eye + gear + disabled trash). The chip is special-cased
//! rather than a real `IndicatorInstance` so it doesn't pollute pane reorder
//! / palette-slot logic.
//!
//! Cluster and Profile each persist their **own** `FootprintParams` instance
//! — switching modes preserves what the user last picked in each. The
//! Candlestick render kind has no params.
//!
//! All types here are `Serialize`/`Deserialize` so a chart panel's render
//! choice + per-mode params round-trip through the workspace's persisted
//! layout (`ChartPrefs`, bumped in Phase 7).

use serde::{Deserialize, Serialize};

/// Which render fills the chart panel's main pane. Mutually exclusive — the
/// chart always has exactly one. `Candlestick` is the default for fresh
/// panels and the fallback the chart "trash" affordance is disabled around
/// (the only way to leave a footprint mode is to pick a different render
/// from the header dropdown).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RenderKind {
    Candlestick,
    Cluster,
    Profile,
}

impl Default for RenderKind {
    fn default() -> Self {
        RenderKind::Candlestick
    }
}

impl RenderKind {
    /// Stable wire string used in `ChartPrefs` serialization. Mirrors
    /// `serde(rename_all = "snake_case")` so manual lookups (from_str)
    /// don't drift from the persisted form.
    pub fn as_id(self) -> &'static str {
        match self {
            RenderKind::Candlestick => "candlestick",
            RenderKind::Cluster => "cluster",
            RenderKind::Profile => "profile",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        match id {
            "candlestick" => Some(RenderKind::Candlestick),
            "cluster" => Some(RenderKind::Cluster),
            "profile" => Some(RenderKind::Profile),
            _ => None,
        }
    }

    /// Label shown in the header dropdown and on the render chip.
    pub fn display_name(self) -> &'static str {
        match self {
            RenderKind::Candlestick => "Candlestick",
            RenderKind::Cluster => "Footprint Cluster",
            RenderKind::Profile => "Footprint Profile",
        }
    }

    /// True iff this kind needs a live `Channel::Footprint` subscription.
    /// Candlestick reads from the candles channel only; the footprint kinds
    /// add a per-(symbol, tf, bucket) sub allocated lazily on enter and
    /// released on exit (see Phase 6).
    pub fn needs_footprint_sub(self) -> bool {
        matches!(self, RenderKind::Cluster | RenderKind::Profile)
    }

    /// True iff the gear icon on this render's chip should open the settings
    /// popover. Candlestick has no params, so its gear stays disabled.
    pub fn has_settings(self) -> bool {
        self.needs_footprint_sub()
    }
}

/// Whether the OHLC bar wireframe paints behind cells, beside cells as a
/// dedicated narrow candle, or not at all. Independent of render kind —
/// applies to both Cluster and Profile.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireframeVariant {
    /// Open/close edges + high/low wicks render behind the cell grid.
    /// Industry standard (Sierra, ATAS). Default.
    Behind,
    /// A narrow real candle paints on one side of the slot; cells fill the
    /// rest. Decouples OHLC reading from cell density.
    SideOhlc,
    /// No OHLC context — pure cell grid.
    None,
}

impl Default for WireframeVariant {
    fn default() -> Self {
        WireframeVariant::Behind
    }
}

/// What drives the cell color (Cluster) or the bar length (Profile).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RenderMetric {
    /// Total volume per cell (bid_vol + ask_vol). Single-hue heat.
    Volume,
    /// ask_vol − bid_vol per cell. Diverging color around zero.
    Delta,
    /// Bid and ask sides drawn independently; bid tinted bearish, ask
    /// bullish, intensity by side-local volume.
    BidAsk,
}

impl RenderMetric {
    /// Cluster's default — most-detailed orderflow read.
    pub fn cluster_default() -> Self {
        RenderMetric::BidAsk
    }
    /// Profile's default — volume-by-price distribution.
    pub fn profile_default() -> Self {
        RenderMetric::Volume
    }
}

/// What numbers appear on the cell (Cluster) or beside the bar (Profile).
/// Independent of [`RenderMetric`] — color and text can encode different
/// things, e.g. cells colored by Volume but labeled with Delta.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextMetric {
    Volume,
    Delta,
    BidAsk,
    /// No numbers at all — just color. Also the auto-applied state when
    /// cells fall below the text-legibility threshold (number auto-hide).
    None,
}

impl TextMetric {
    pub fn cluster_default() -> Self {
        TextMetric::BidAsk
    }
    pub fn profile_default() -> Self {
        TextMetric::Volume
    }
}

/// Normalization basis for the render-metric color intensity. Per-bar feels
/// most "structured" (every bar shows its own peak); visible-range lets you
/// compare across bars but quiet bars look near-black; daily anchors color
/// to the day's max for absolute-magnitude reading.
///
/// Note: `Daily` requires day-bucket caching of the max value (see Phase 3
/// paint pipeline) — implementation may fall back to `Visible` when the
/// day cache hasn't yet been built.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ColorScope {
    /// Per-bar normalization. Default — most readable.
    Individual,
    /// Viewport-wide normalization. Best for cross-bar comparison.
    Visible,
    /// Whole-day normalization. Absolute magnitude reading.
    Daily,
}

impl Default for ColorScope {
    fn default() -> Self {
        ColorScope::Individual
    }
}

/// Persisted settings for one footprint render kind. The chart panel stores
/// one of these per kind (Cluster + Profile), independently — switching
/// modes preserves what the user last configured in each.
///
/// `bucket` is in the same units as the symbol's quote currency (USD for
/// BTCUSDT). Free-form numeric on the settings UI; commits on Enter/blur
/// rather than per-keystroke so we don't spam the WS with `Subscribe` /
/// `Unsubscribe` frames while the user is mid-edit.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct FootprintParams {
    pub bucket: f64,
    pub wireframe: WireframeVariant,
    pub render_metric: RenderMetric,
    pub text_metric: TextMetric,
    pub color_scope: ColorScope,
}

/// Default bucket size for BTCUSDT — round number, ~$2k typical 1m bar
/// range / $10 = ~20 cells per bar, readable at default zoom. The settings
/// UI is free-form so the user can override per panel.
const DEFAULT_BUCKET_BTCUSDT: f64 = 10.0;

/// Binance BTCUSDT-perp price tick size ($0.10). Storage stays in raw
/// quote currency (the WS sub plumbing keys on the f64 price-bucket), but
/// the settings UI expresses bucket as a multiple of this tick to match
/// how orderflow traders think about cell sizing.
pub const BTCUSDT_TICK_SIZE: f64 = 0.10;

impl FootprintParams {
    /// Cluster defaults: bid/ask render, bid/ask text, behind-cells wireframe,
    /// per-bar color normalization, $10 bucket.
    pub fn cluster_default() -> Self {
        Self {
            bucket: DEFAULT_BUCKET_BTCUSDT,
            wireframe: WireframeVariant::default(),
            render_metric: RenderMetric::cluster_default(),
            text_metric: TextMetric::cluster_default(),
            color_scope: ColorScope::default(),
        }
    }

    /// Profile defaults: total-volume bars, volume text, behind-cells
    /// wireframe (still surfaces OHLC), per-bar normalization, $10 bucket.
    pub fn profile_default() -> Self {
        Self {
            bucket: DEFAULT_BUCKET_BTCUSDT,
            wireframe: WireframeVariant::default(),
            render_metric: RenderMetric::profile_default(),
            text_metric: TextMetric::profile_default(),
            color_scope: ColorScope::default(),
        }
    }

    /// Returns true if `bucket` is a usable subscription parameter — must be
    /// finite and strictly positive. The settings UI calls this before
    /// committing a new bucket value so a malformed entry (e.g. "0" or "")
    /// doesn't trigger a footprint sub re-allocation with bad params.
    pub fn bucket_is_valid(bucket: f64) -> bool {
        bucket.is_finite() && bucket > 0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wasm_bindgen_test::wasm_bindgen_test;

    #[wasm_bindgen_test]
    fn render_kind_roundtrip() {
        for kind in [
            RenderKind::Candlestick,
            RenderKind::Cluster,
            RenderKind::Profile,
        ] {
            assert_eq!(RenderKind::from_id(kind.as_id()), Some(kind));
            let s = serde_json::to_string(&kind).unwrap();
            let back: RenderKind = serde_json::from_str(&s).unwrap();
            assert_eq!(kind, back);
        }
    }

    #[wasm_bindgen_test]
    fn render_kind_default_is_candlestick() {
        assert_eq!(RenderKind::default(), RenderKind::Candlestick);
    }

    #[wasm_bindgen_test]
    fn needs_footprint_sub_only_footprint_kinds() {
        assert!(!RenderKind::Candlestick.needs_footprint_sub());
        assert!(RenderKind::Cluster.needs_footprint_sub());
        assert!(RenderKind::Profile.needs_footprint_sub());
    }

    #[wasm_bindgen_test]
    fn has_settings_matches_needs_footprint_sub() {
        // Candlestick gear is disabled; footprint gears open the popover.
        assert_eq!(
            RenderKind::Candlestick.has_settings(),
            RenderKind::Candlestick.needs_footprint_sub()
        );
        assert!(RenderKind::Cluster.has_settings());
        assert!(RenderKind::Profile.has_settings());
    }

    #[wasm_bindgen_test]
    fn cluster_default_uses_bid_ask() {
        let p = FootprintParams::cluster_default();
        assert_eq!(p.render_metric, RenderMetric::BidAsk);
        assert_eq!(p.text_metric, TextMetric::BidAsk);
        assert_eq!(p.wireframe, WireframeVariant::Behind);
        assert_eq!(p.color_scope, ColorScope::Individual);
        assert!(FootprintParams::bucket_is_valid(p.bucket));
    }

    #[wasm_bindgen_test]
    fn profile_default_uses_volume() {
        let p = FootprintParams::profile_default();
        assert_eq!(p.render_metric, RenderMetric::Volume);
        assert_eq!(p.text_metric, TextMetric::Volume);
    }

    #[wasm_bindgen_test]
    fn bucket_validation_rejects_zero_negative_nonfinite() {
        assert!(!FootprintParams::bucket_is_valid(0.0));
        assert!(!FootprintParams::bucket_is_valid(-1.0));
        assert!(!FootprintParams::bucket_is_valid(f64::NAN));
        assert!(!FootprintParams::bucket_is_valid(f64::INFINITY));
        assert!(FootprintParams::bucket_is_valid(0.5));
        assert!(FootprintParams::bucket_is_valid(1000.0));
    }

    #[wasm_bindgen_test]
    fn params_roundtrip_json() {
        let p = FootprintParams {
            bucket: 12.5,
            wireframe: WireframeVariant::SideOhlc,
            render_metric: RenderMetric::Delta,
            text_metric: TextMetric::None,
            color_scope: ColorScope::Visible,
        };
        let s = serde_json::to_string(&p).unwrap();
        let back: FootprintParams = serde_json::from_str(&s).unwrap();
        assert_eq!(p, back);
        // Snake-case wire form for enum tags.
        assert!(s.contains("\"wireframe\":\"side_ohlc\""));
        assert!(s.contains("\"render_metric\":\"delta\""));
        assert!(s.contains("\"text_metric\":\"none\""));
        assert!(s.contains("\"color_scope\":\"visible\""));
    }
}
