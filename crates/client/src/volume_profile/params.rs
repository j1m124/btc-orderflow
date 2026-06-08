//! Knobs shared by VRVP and FRVP. One serde struct backs both — the only
//! per-consumer difference (which time range to compute over) is supplied
//! by the caller, not stored here.
//!
//! Bucket size is user-facing in **ticks** (= integer multiples of the
//! symbol's price tick); we store + serialize the tick count and resolve
//! to dollars via [`VolumeProfileParams::bucket_dollars`] when keying the
//! footprint subscription. This matches how orderflow traders think
//! ("100-tick bucket" rather than "$10 bucket") and decouples persisted
//! params from future per-symbol tick-size changes.

use gpui::{Hsla, hsla};
use serde::{Deserialize, Serialize};

/// BTCUSDT-perp price tick ($0.10). Mirrors
/// [`crate::panels::chart::footprint::BTCUSDT_TICK_SIZE`] — re-declared
/// here to avoid a paint-side import in pure params code; both constants
/// must stay in lockstep. When multi-symbol lands, this becomes a lookup
/// keyed on the chart's active symbol (same migration the chart's existing
/// footprint settings will need).
pub const BTCUSDT_TICK_SIZE: f64 = 0.10;

/// How the per-bucket bars are drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VpRenderMode {
    /// One filled bar per bucket; length = total volume (bid+ask).
    Volume,
    /// One filled bar per bucket; length = `|delta|`, colored bull/bear by
    /// the sign of `ask_vol − bid_vol`.
    Delta,
    /// Outlined volume bar (stroke only, no fill) with a filled colored
    /// inner bar = `|delta|`, both anchored to the same edge. Inner length
    /// is always per-row-scaled so it fits inside the outer frame.
    VolDeltaOutline,
}

impl Default for VpRenderMode {
    fn default() -> Self {
        VpRenderMode::Volume
    }
}

impl VpRenderMode {
    pub const ALL: &'static [VpRenderMode] = &[
        VpRenderMode::Volume,
        VpRenderMode::Delta,
        VpRenderMode::VolDeltaOutline,
    ];

    pub fn label(self) -> &'static str {
        match self {
            VpRenderMode::Volume => "Volume",
            VpRenderMode::Delta => "Delta",
            VpRenderMode::VolDeltaOutline => "Volume + Delta",
        }
    }
}

/// How delta-mode bars normalize their lengths.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VpDeltaScale {
    /// Each bucket's bar length = `|delta| / bucket_total`. Lets thin rows
    /// with strong one-sided pressure pop visually.
    PerRow,
    /// All bars share a global denominator = `max(|delta|)` across the
    /// profile. Tall rows dominate; thin high-delta rows look short.
    WholeProfile,
}

impl Default for VpDeltaScale {
    fn default() -> Self {
        VpDeltaScale::PerRow
    }
}

impl VpDeltaScale {
    pub const ALL: &'static [VpDeltaScale] =
        &[VpDeltaScale::PerRow, VpDeltaScale::WholeProfile];

    pub fn label(self) -> &'static str {
        match self {
            VpDeltaScale::PerRow => "Per row",
            VpDeltaScale::WholeProfile => "Whole profile",
        }
    }
}

/// Which edge of the host region the bars are anchored against.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnchorEdge {
    Right,
    Left,
}

impl AnchorEdge {
    pub const ALL: &'static [AnchorEdge] = &[AnchorEdge::Right, AnchorEdge::Left];

    pub fn label(self) -> &'static str {
        match self {
            AnchorEdge::Right => "Right",
            AnchorEdge::Left => "Left",
        }
    }
}

/// Soft-warn threshold on bucket size — below this the profile becomes a
/// pixel-mush. Doesn't reject input; the settings UI surfaces a hint.
pub const BUCKET_TICKS_SOFT_MIN: u32 = 10;

/// Hard validation clamps. The settings UI rejects values outside these
/// before committing, so [`VolumeProfileParams::bucket_dollars`] and the
/// downstream footprint sub never see a garbage bucket.
pub const BUCKET_TICKS_MIN: u32 = 1;
pub const BUCKET_TICKS_MAX: u32 = 10_000;
pub const VA_PERCENT_MIN: u8 = 50;
pub const VA_PERCENT_MAX: u8 = 95;
pub const WIDTH_PCT_MIN: u8 = 5;
pub const WIDTH_PCT_MAX: u8 = 80;

/// Defaults: 100 ticks ($10), Volume mode, 30%-wide, all reference levels
/// on at 70% VA. Anchor edge defaults are set by the *consumer*
/// (VRVP → Right; FRVP → Left), not here — [`VolumeProfileParams::default`]
/// picks `Right` because that matches the more common case (visible-range).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VolumeProfileParams {
    pub bucket_ticks: u32,
    pub render_mode: VpRenderMode,
    pub delta_scale: VpDeltaScale,
    pub width_pct: u8,
    pub anchor: AnchorEdge,

    pub show_poc: bool,
    pub show_va: bool,
    pub show_va_highlight: bool,
    pub show_labels: bool,
    pub va_percent: u8,

    pub color_volume: ColorBlob,
    pub color_bull: ColorBlob,
    pub color_bear: ColorBlob,
    pub color_poc: ColorBlob,
    pub color_va: ColorBlob,
}

impl Default for VolumeProfileParams {
    fn default() -> Self {
        Self {
            bucket_ticks: 100,
            render_mode: VpRenderMode::Volume,
            delta_scale: VpDeltaScale::PerRow,
            width_pct: 30,
            anchor: AnchorEdge::Right,
            show_poc: true,
            show_va: true,
            show_va_highlight: true,
            show_labels: true,
            va_percent: 70,
            color_volume: ColorBlob::from_hsla(hsla(0.0, 0.0, 0.55, 0.50)),
            color_bull: ColorBlob::from_hsla(hsla(0.36, 0.55, 0.50, 0.85)),
            color_bear: ColorBlob::from_hsla(hsla(0.00, 0.65, 0.55, 0.85)),
            color_poc: ColorBlob::from_hsla(hsla(0.13, 0.85, 0.55, 1.0)),
            color_va: ColorBlob::from_hsla(hsla(0.13, 0.65, 0.55, 0.60)),
        }
    }
}

impl VolumeProfileParams {
    /// Resolve the bucket size to quote-currency dollars — this is what the
    /// footprint WS subscription is keyed on (`Channel::Footprint.price_bucket`).
    pub fn bucket_dollars(&self) -> f64 {
        self.bucket_ticks as f64 * BTCUSDT_TICK_SIZE
    }

    /// Bit pattern of the f64 bucket — used as the `HashMap` key for the
    /// ContentPanel's per-bucket footprint sub map. Matches the keying the
    /// existing `MarketDataService::FootprintSubKey` uses.
    pub fn bucket_bits(&self) -> u64 {
        self.bucket_dollars().to_bits()
    }

    /// True iff every numeric is inside its hard-clamp window.
    pub fn is_valid(&self) -> bool {
        (BUCKET_TICKS_MIN..=BUCKET_TICKS_MAX).contains(&self.bucket_ticks)
            && (VA_PERCENT_MIN..=VA_PERCENT_MAX).contains(&self.va_percent)
            && (WIDTH_PCT_MIN..=WIDTH_PCT_MAX).contains(&self.width_pct)
    }

    /// Reset *style* fields only — bucket / mode / anchor are user intent
    /// and survive. Mirrors the "Reset to defaults" button semantics from
    /// the design grilling.
    pub fn reset_styles(&mut self) {
        let d = VolumeProfileParams::default();
        self.show_poc = d.show_poc;
        self.show_va = d.show_va;
        self.show_va_highlight = d.show_va_highlight;
        self.show_labels = d.show_labels;
        self.va_percent = d.va_percent;
        self.color_volume = d.color_volume;
        self.color_bull = d.color_bull;
        self.color_bear = d.color_bear;
        self.color_poc = d.color_poc;
        self.color_va = d.color_va;
    }
}

/// Lossless serde mirror of [`gpui::Hsla`]. Same trick the drawings layer
/// uses ([`crate::drawings::shapes::DrawingColor`]) — gpui's `Hsla` doesn't
/// derive serde, so we round-trip through this.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ColorBlob {
    pub h: f32,
    pub s: f32,
    pub l: f32,
    pub a: f32,
}

impl ColorBlob {
    pub fn from_hsla(c: Hsla) -> Self {
        Self {
            h: c.h,
            s: c.s,
            l: c.l,
            a: c.a,
        }
    }

    pub fn into_hsla(self) -> Hsla {
        hsla(self.h, self.s, self.l, self.a)
    }
}
