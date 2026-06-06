//! Core trait + supporting enums. v1 model is intentionally minimal: the
//! trait is stateless (`compute` is pure), per-kind impls own typed params,
//! and identity is carried by a stable `kind_id` string for persistence /
//! picker routing. When scripting lands, a `ScriptedIndicator` impl satisfies
//! the same trait — the scripting boundary is the trait, not a new variant.
//!
//! Paint isn't on the trait yet: the chart's paint pipeline matches on
//! `kind_id` (or on the output shape) to draw the right primitive, so each
//! impl can focus on the pure math. We'll revisit if a kind ever needs a
//! truly bespoke render (e.g., custom Ichimoku cloud shading).

use std::any::Any;

use gpui::SharedString;
use serde::{Deserialize, Serialize};

use super::output::{IndicatorOutput, ValueReadout};
use crate::persistence::VolumeUnit;
use crate::services::market_data::Candle;

/// Cross-cutting per-compute context, threaded from `ChartState` into each
/// `IndicatorKind::compute` call. Lets per-chart settings (e.g., the volume
/// unit toggle that sits in the chart header) flow into indicator math
/// without going through a global. Add fields here as more per-chart
/// scaling/normalisation knobs surface.
#[derive(Clone, Copy, Debug, Default)]
pub struct ComputeCtx {
    pub volume_unit: VolumeUnit,
}

/// Where an indicator can render. Drives picker entry placement, default
/// `Placement` on add, and which chip strip the chip lives in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaneKind {
    /// Lives on top of the candles (SMA, EMA, BB).
    OverlayOnly,
    /// Lives in its own pane below the candles (MACD, RSI).
    PaneOnly,
    /// Can render as either. Volume is the canonical example — defaults to
    /// overlay; user toggles to pane in settings.
    Both,
}

/// Per-instance placement. Only meaningful when `kind.pane_kind() == Both`;
/// for `OverlayOnly` / `PaneOnly` kinds this field is fixed to the matching
/// variant and the settings UI hides the toggle.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Placement {
    Overlay,
    Pane,
}

/// Price source for line indicators. `Volume` reads `Candle.volume`
/// directly and ignores this enum.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Source {
    Close,
    Open,
    High,
    Low,
    Hl2,
    Ohlc4,
}

impl Source {
    pub const ALL: &'static [Source] = &[
        Source::Close,
        Source::Open,
        Source::High,
        Source::Low,
        Source::Hl2,
        Source::Ohlc4,
    ];

    /// User-facing label ("close", "hl2", …). Lowercase to match TV
    /// conventions and the settings dropdown.
    pub fn label(&self) -> &'static str {
        match self {
            Source::Close => "close",
            Source::Open => "open",
            Source::High => "high",
            Source::Low => "low",
            Source::Hl2 => "hl2",
            Source::Ohlc4 => "ohlc4",
        }
    }
}

/// The contract every indicator (built-in or future scripted) implements.
///
/// Stateless: the caller (the chart) owns the cached output. The trait is
/// `Send + Sync` so future work could move compute off the render thread,
/// though v1 runs compute synchronously on `apply_tick`.
pub trait IndicatorKind: Any + Send + Sync {
    /// Stable identifier used for persistence and picker routing
    /// ("sma", "ema", "bb", "volume", "macd", "rsi"). Must be unique across
    /// all registered kinds.
    fn kind_id(&self) -> &'static str;

    /// Where this kind can render.
    fn pane_kind(&self) -> PaneKind;

    /// User-facing label generated from current params — updates live when
    /// params change. Example: `SMA(period=20)` → "SMA 20",
    /// `MACD(12, 26, 9)` → "MACD(12, 26, 9)".
    fn label(&self) -> SharedString;

    /// Pure compute: full recompute over the candle array. Output length
    /// matches `candles.len()`; positions where there isn't enough history
    /// (e.g., the first `period - 1` bars) are `None`. `ctx` carries any
    /// chart-scoped knobs that affect indicator math (e.g., volume unit).
    fn compute(&self, candles: &[Candle], ctx: ComputeCtx) -> IndicatorOutput;

    /// Crosshair-active chip readout at a specific bar index. Each kind
    /// formats this differently — single-line kinds return `One(...)`,
    /// MACD returns `Three(...)`, BB returns `Two(...)`.
    fn value_at(&self, output: &IndicatorOutput, index: usize) -> ValueReadout;

    /// Visible-data y-range for this output across the visible bar range,
    /// used by per-pane y-axis auto-fit. `None` means no data in range.
    /// Overlay indicators contribute to the main pane's range; pane
    /// indicators own their pane's range exclusively.
    fn y_range(&self, output: &IndicatorOutput, range: std::ops::Range<usize>) -> Option<(f64, f64)>;

    /// Serialize current params to a `serde_json::Value`. Paired with a
    /// per-kind `from_params` constructor in the registry on load.
    fn params_json(&self) -> serde_json::Value;

    /// Downcast hook for the settings UI: each impl returns `self` and the
    /// settings panel does `as_any_mut().downcast_mut::<SmaParams>()` to
    /// edit typed fields in place. Avoids round-tripping through
    /// `params_json()` on every keystroke.
    fn as_any_mut(&mut self) -> &mut dyn Any;

    /// Names of the color slots this kind exposes. The settings panel
    /// renders one color picker per slot, and `IndicatorInstance.colors`
    /// is sized to match (slot 0 = primary line, slot 1+ = additional
    /// series). Empty Vec means "no color controls" — Volume's bullish
    /// and bearish bars are theme-driven and don't carry a configurable
    /// color. Default: one "Color" slot, which matches every single-line
    /// indicator (BB, RSI).
    ///
    /// Owned `Vec<SharedString>` (rather than `&'static`) so kinds whose
    /// slot count is data-driven (MA Suite — one slot per user-added
    /// MA entry) can return labels derived from `self`.
    fn color_slots(&self) -> Vec<SharedString> {
        vec![SharedString::from("Color")]
    }
}
