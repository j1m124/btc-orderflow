//! Orderbook Profile — overlay indicator drawing the **live** order book as
//! right-anchored horizontal bars on the chart's price axis (the orderbook
//! panel ladder rotated onto candle Y). A thin **façade**, like
//! [`crate::indicators::ob_heatmap`]: it computes no per-bar series — its real
//! render reads the live book snapshot fresh each frame in
//! [`crate::panels::chart::paint`]'s `paint_ob_profile`, fed by these params.
//! **Pure-client** — zero protocol/server change; it reuses the chart's
//! existing book subscription (the same one the heatmap / OB-imbalance ride).
//!
//! Unlike the heatmap façades it needs **no** render layer / texture cache: the
//! live snapshot is cheap to bin over just the visible price band, so each frame
//! re-buckets straight into PaintQuads. Its settings are a plain declarative
//! [`SettingsForm`] (no stateful widget), so no `custom_settings_view`.
//!
//! Locked v1 semantics (grilled 2026-06-28):
//! - Scope = the current book snapshot (live), anchored to the right edge (=now),
//!   independent of the visible *time* range.
//! - Vertical coverage = every bucket within the visible price range; zoom /
//!   scroll Y reveals more book. No depth knob.
//! - Each side (bids below mid, asks above) scales to **its own** largest visible
//!   bucket and shares the right-edge anchor; the two never overlap in price.
//! - **Singleton** — one book / one chart; overlapping bars would be unreadable.

use std::any::Any;

use gpui::{Hsla, SharedString, WeakEntity};
use gpui_component::ActiveTheme as _;
use serde::{Deserialize, Serialize};

use super::instance::InstanceId;
use super::kind::{ComputeCtx, IndicatorKind, PaneKind};
use super::output::{IndicatorOutput, ValueReadout};
use crate::panels::ContentPanel;
use crate::services::market_data::Candle;
use crate::settings_form::{Field, IndicatorTarget, NumberOpts, SettingsForm, SettingsGroup};
use crate::volume_profile::params::ColorBlob;

/// BTCUSDT-perp tick size ($0.10). The bucket width is presented to the user as
/// a count of ticks but stored / rendered in dollars.
pub const TICK_SIZE: f64 = 0.1;

/// Bucket-width bounds, in **ticks**. Floor is one exchange tick; the ceiling
/// matches the heatmap / VRVP conventions.
pub const MIN_BUCKET_TICKS: i64 = 1;
pub const MAX_BUCKET_TICKS: i64 = 10_000;
/// Default bucket width: 50 ticks = $5, matching the heatmap render grain.
pub const DEFAULT_BUCKET_TICKS: u32 = 50;

/// Profile-width bounds (% of the chart plot width).
pub const MIN_WIDTH_PCT: i64 = 5;
pub const MAX_WIDTH_PCT: i64 = 80;
pub const DEFAULT_WIDTH_PCT: u8 = 20;

fn default_bucket_ticks() -> u32 {
    DEFAULT_BUCKET_TICKS
}
fn default_width_pct() -> u8 {
    DEFAULT_WIDTH_PCT
}

/// Per-instance params. All fields carry `#[serde(default)]` so older persisted
/// blobs (and future field additions) round-trip cleanly.
///
/// `color_bid` / `color_ask` are `None` by default, meaning **follow the
/// theme's bullish/bearish chart colours at full alpha** — resolved at paint
/// time (and in the settings colour picker), since the theme isn't available at
/// `Default` construction. Once the user picks a colour it's pinned to `Some`.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct OrderbookProfileParams {
    #[serde(default = "default_bucket_ticks")]
    pub bucket_ticks: u32,
    #[serde(default = "default_width_pct")]
    pub width_pct: u8,
    #[serde(default)]
    pub color_bid: Option<ColorBlob>,
    #[serde(default)]
    pub color_ask: Option<ColorBlob>,
}

impl Default for OrderbookProfileParams {
    fn default() -> Self {
        Self {
            bucket_ticks: DEFAULT_BUCKET_TICKS,
            width_pct: DEFAULT_WIDTH_PCT,
            color_bid: None,
            color_ask: None,
        }
    }
}

impl OrderbookProfileParams {
    /// Bucket width in dollars (`ticks × $0.10`), clamped to ≥ one tick.
    pub fn bucket_dollars(&self) -> f64 {
        (self.bucket_ticks.max(1) as f64) * TICK_SIZE
    }

    /// Profile width as a fraction of the plot width.
    pub fn width_frac(&self) -> f32 {
        (self.width_pct as f32 / 100.0).clamp(0.05, 0.80)
    }

    /// Resolved bid-bar colour: the explicit override, else the theme's bullish
    /// chart colour (`theme_bid`) at full alpha.
    pub fn bid_color(&self, theme_bid: Hsla) -> Hsla {
        self.color_bid.map(|c| c.into_hsla()).unwrap_or(theme_bid)
    }

    /// Resolved ask-bar colour: the explicit override, else the theme's bearish
    /// chart colour (`theme_ask`) at full alpha.
    pub fn ask_color(&self, theme_ask: Hsla) -> Hsla {
        self.color_ask.map(|c| c.into_hsla()).unwrap_or(theme_ask)
    }
}

impl IndicatorKind for OrderbookProfileParams {
    fn kind_id(&self) -> &'static str {
        "ob_profile"
    }

    fn pane_kind(&self) -> PaneKind {
        PaneKind::OverlayOnly
    }

    fn label(&self) -> SharedString {
        // Bucket size is the most useful single-glance summary — mirrors the
        // VRVP "{N}t" convention.
        format!("OB Profile {}t", self.bucket_ticks).into()
    }

    fn compute(&self, _candles: &[Candle], _ctx: ComputeCtx<'_>) -> IndicatorOutput {
        // Façade marker — the real render runs in `paint_ob_profile`, which reads
        // the live book snapshot fresh each frame and bins it per these params.
        IndicatorOutput::ObProfile
    }

    fn value_at(&self, _output: &IndicatorOutput, _index: usize) -> ValueReadout {
        // No per-bar / crosshair readout (the façade gives it up). The chip
        // shows just the name.
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

    fn settings_form(
        &self,
        panel: WeakEntity<ContentPanel>,
        id: InstanceId,
    ) -> Option<SettingsForm> {
        let target: IndicatorTarget<OrderbookProfileParams> = IndicatorTarget::new(panel, id);
        let form_id = SharedString::from(format!("ob-profile-{}", id));

        let bucket_field = Field::number(
            "Bucket",
            NumberOpts::int(MIN_BUCKET_TICKS, MAX_BUCKET_TICKS).with_step(10.0),
            target.getter(DEFAULT_BUCKET_TICKS as f64, |p: &OrderbookProfileParams| {
                p.bucket_ticks as f64
            }),
            target.setter(|p: &mut OrderbookProfileParams, v: f64| {
                let cur = v
                    .round()
                    .clamp(MIN_BUCKET_TICKS as f64, MAX_BUCKET_TICKS as f64);
                p.bucket_ticks = cur as u32;
            }),
        )
        .description("Price bucket size in ticks ($0.10 each).");

        let width_field = Field::slider(
            "Width",
            NumberOpts::int(MIN_WIDTH_PCT, MAX_WIDTH_PCT)
                .with_step(5.0)
                .suffix("%"),
            target.getter(DEFAULT_WIDTH_PCT as f64, |p: &OrderbookProfileParams| {
                p.width_pct as f64
            }),
            target.setter(|p: &mut OrderbookProfileParams, v: f64| {
                let cur = v.round().clamp(MIN_WIDTH_PCT as f64, MAX_WIDTH_PCT as f64);
                p.width_pct = cur as u8;
            }),
        )
        .description("Longest bar's width as a percentage of the chart pane.");

        // Unset colours follow the theme; the picker shows the live theme colour
        // so the swatch matches what's drawn until the user overrides it.
        let bid_color = make_color_field(
            "Bid color",
            target.clone(),
            |p| &mut p.color_bid,
            |p| p.color_bid,
            |cx| cx.theme().chart_bullish,
        );
        let ask_color = make_color_field(
            "Ask color",
            target.clone(),
            |p| &mut p.color_ask,
            |p| p.color_ask,
            |cx| cx.theme().chart_bearish,
        );

        Some(
            SettingsForm::new(form_id).group(
                SettingsGroup::new("General")
                    .item(bucket_field)
                    .item(width_field)
                    .item(bid_color)
                    .item(ask_color),
            ),
        )
    }
}

fn make_color_field<F, G, T>(
    label: &'static str,
    target: IndicatorTarget<OrderbookProfileParams>,
    set_field: F,
    get_field: G,
    theme_color: T,
) -> Field
where
    F: Fn(&mut OrderbookProfileParams) -> &mut Option<ColorBlob> + 'static + Clone,
    G: Fn(&OrderbookProfileParams) -> Option<ColorBlob> + 'static + Clone,
    T: Fn(&gpui::App) -> Hsla + 'static + Clone,
{
    let target_for_get = target.clone();
    let target_for_set = target;
    let theme_for_get = theme_color;
    Field::color(
        label,
        move |cx: &gpui::App| -> Hsla {
            // `None` (follow theme) shows the live theme colour; an override
            // shows the stored colour.
            match target_for_get.read(cx, |p| get_field(p)).flatten() {
                Some(c) => c.into_hsla(),
                None => theme_for_get(cx),
            }
        },
        move |color: Hsla, cx: &mut gpui::App| {
            let set_field = set_field.clone();
            target_for_set.write(cx, move |p| {
                // An explicit pick pins the colour (no longer follows theme).
                *set_field(p) = Some(ColorBlob::from_hsla(color));
            });
        },
    )
}
