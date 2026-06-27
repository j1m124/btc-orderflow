//! Bespoke settings view for the liquidation-heatmap indicator. Hosted inside
//! the standard indicator settings panel (`indicator_settings.rs`) via
//! [`crate::indicators::IndicatorKind::custom_settings_view`] — a sibling of
//! [`super::heatmap_settings::HeatmapSettingsView`], differing only in the
//! slider domain (liquidation magnets accumulate to larger coin magnitudes) and
//! two extra sim knobs (maintenance margin + warm-up lookback).
//!
//! Why bespoke rather than a declarative `SettingsForm`: the colour range is a
//! two-handle [`SliderState`] on a logarithmic scale, which the stateless form
//! framework can't express. The slider lives on the view and writes through the
//! instance's [`IndicatorTarget`] on change; everything else stays form fields.

use gpui::{
    App, AppContext as _, Context, Entity, FocusHandle, Focusable, Hsla, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, SharedString, Styled as _, Subscription, WeakEntity,
    Window, div,
};
use gpui_component::slider::{Slider, SliderEvent, SliderScale, SliderState};
use gpui_component::{ActiveTheme as _, v_flex};

use super::paint::{Colormap, LIQ_COLOR_RANGE_MAX, LIQ_COLOR_RANGE_MIN};
use crate::indicators::liq_heatmap::sim::{AVAILABLE_LEVERAGE, DEFAULT_LEVERAGE_SELECTED};
use crate::indicators::liq_heatmap::{
    DEFAULT_BUCKET, DEFAULT_PROFILE_WIDTH_PCT, MAX_BUCKET_TICKS, MAX_PROFILE_WIDTH_PCT,
    MIN_BUCKET_TICKS, MIN_PROFILE_WIDTH_PCT, TICK_SIZE,
};
use crate::indicators::{InstanceId, LiqHeatmapParams};
use crate::panels::ContentPanel;
use crate::settings_form::{
    DropdownOption, Field, IndicatorTarget, MultiCheckItem, NumberOpts, SettingsForm, SettingsGroup,
};

pub struct LiqHeatmapSettingsView {
    /// Routes reads/writes to the liq-heatmap instance's params, going through
    /// `chart.update_indicator` like every other settings surface.
    tgt: IndicatorTarget<LiqHeatmapParams>,
    focus: FocusHandle,
    /// Two-handle colour range (low, peak) in coin units, log scale.
    range: Entity<SliderState>,
    _subs: Vec<Subscription>,
}

impl LiqHeatmapSettingsView {
    pub fn new(
        panel: WeakEntity<ContentPanel>,
        id: InstanceId,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let tgt = IndicatorTarget::<LiqHeatmapParams>::new(panel, id);
        let (lo, peak) = current_range(&tgt, cx);
        let range = cx.new(|_| {
            SliderState::new()
                .min(LIQ_COLOR_RANGE_MIN as f32)
                .max(LIQ_COLOR_RANGE_MAX as f32)
                .step(1.0)
                .scale(SliderScale::Logarithmic)
                .default_value(lo..peak)
        });
        let tgt_for_sub = tgt.clone();
        let sub = cx.subscribe(&range, move |_this, _slider, event: &SliderEvent, cx| {
            let SliderEvent::Change(value) = event;
            let lo = value.start() as f64;
            let peak = value.end() as f64;
            tgt_for_sub.write(cx, move |p| {
                p.settings.color_lo = lo;
                p.settings.color_peak = peak;
            });
            cx.notify();
        });
        Self {
            tgt,
            focus: cx.focus_handle(),
            range,
            _subs: vec![sub],
        }
    }
}

impl Focusable for LiqHeatmapSettingsView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for LiqHeatmapSettingsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;

        if self.tgt.read(cx, |_p| ()).is_none() {
            return missing_body("Indicator no longer available", muted).into_any_element();
        }

        let value = self.range.read(cx).value();
        let range_section = v_flex()
            .gap_1()
            .child(
                div()
                    .text_sm()
                    .text_color(muted)
                    .child(SharedString::from("Colour range")),
            )
            .child(Slider::new(&self.range))
            .child(
                div().text_xs().text_color(muted).child(SharedString::from(format!(
                    "hide < {}   ·   peak {}",
                    fmt_amt(value.start()),
                    fmt_amt(value.end())
                ))),
            )
            .child(div().text_xs().text_color(muted).child(SharedString::from(
                "Cells below the low handle aren't drawn; the high handle is the top of the ramp.",
            )));

        let form = build_liq_form(&self.tgt);
        let inner = form.render(window, cx);
        v_flex()
            .id(SharedString::from("chart-liq-heatmap-settings"))
            .w_full()
            .gap_3()
            .child(range_section)
            .child(inner)
            .into_any_element()
    }
}

fn missing_body(msg: &'static str, muted: Hsla) -> impl IntoElement {
    div()
        .p_4()
        .text_sm()
        .text_color(muted)
        .child(SharedString::from(msg))
}

/// Current (low, peak) of the instance's heatmap params, clamped into the slider
/// domain with `low <= peak`. Defaults when the instance is gone.
fn current_range(tgt: &IndicatorTarget<LiqHeatmapParams>, cx: &App) -> (f32, f32) {
    let s = tgt.read(cx, |p| p.settings).unwrap_or_default();
    let min = LIQ_COLOR_RANGE_MIN as f32;
    let max = LIQ_COLOR_RANGE_MAX as f32;
    let lo = (s.color_lo as f32).clamp(min, max);
    let peak = (s.color_peak as f32).clamp(min, max).max(lo);
    (lo, peak)
}

/// Compact coin-amount label for the range readout: `2.4k` / `340` / `4.5`.
fn fmt_amt(v: f32) -> String {
    if v >= 1000.0 {
        format!("{:.1}k", v / 1000.0)
    } else if v >= 10.0 {
        format!("{:.0}", v)
    } else {
        format!("{:.1}", v)
    }
}

/// Bucket width in **ticks**, rounded to a whole tick. Stored in dollars, so
/// divide by [`TICK_SIZE`].
fn bucket_ticks(bucket: f64) -> f64 {
    (bucket / TICK_SIZE).round().max(MIN_BUCKET_TICKS as f64)
}

fn build_liq_form(tgt: &IndicatorTarget<LiqHeatmapParams>) -> SettingsForm {
    let bucket_field = Field::number(
        "Tick size",
        NumberOpts::int(MIN_BUCKET_TICKS, MAX_BUCKET_TICKS).suffix("ticks"),
        tgt.getter(bucket_ticks(DEFAULT_BUCKET), |p: &LiqHeatmapParams| {
            bucket_ticks(p.bucket)
        }),
        tgt.setter(|p: &mut LiqHeatmapParams, v: f64| {
            let ticks = v.round().clamp(MIN_BUCKET_TICKS as f64, MAX_BUCKET_TICKS as f64);
            p.bucket = ticks * TICK_SIZE;
        }),
    )
    .description("Heatmap row height in ticks — 1 tick = $0.10 (the BTCUSDT price increment). Coarser merges nearby magnets into fatter bands.");

    let opacity_field = Field::slider(
        "Max opacity",
        NumberOpts::int(5, 100).suffix("%"),
        tgt.getter(85.0, |p: &LiqHeatmapParams| {
            (p.settings.max_opacity as f64 * 100.0).round()
        }),
        tgt.setter(|p: &mut LiqHeatmapParams, v: f64| {
            p.settings.max_opacity = (v / 100.0).clamp(0.05, 1.0) as f32;
        }),
    )
    .description("Opacity of the hottest cell — lower keeps candles readable.");

    let colormap_field = Field::dropdown(
        "Colour map",
        Colormap::ALL
            .iter()
            .map(|c| DropdownOption::new(c.as_str(), c.label()))
            .collect(),
        tgt.getter(
            SharedString::from(Colormap::Plasma.as_str()),
            |p: &LiqHeatmapParams| SharedString::from(p.settings.colormap.as_str()),
        ),
        tgt.setter(|p: &mut LiqHeatmapParams, v: SharedString| {
            p.settings.colormap = Colormap::from_token(v.as_ref());
        }),
    )
    .description("Colour gradient for the heat ramp (also recolours the profile).");

    let mmr_field = Field::slider(
        "Maint. margin",
        NumberOpts::float(0.0, 5.0, 0.05)
            .format(|v| SharedString::from(format!("{:.2}", v)))
            .suffix("%"),
        tgt.getter(0.4, |p: &LiqHeatmapParams| p.mmr * 100.0),
        tgt.setter(|p: &mut LiqHeatmapParams, v: f64| {
            p.mmr = (v / 100.0).clamp(0.0, 0.1);
        }),
    )
    .description("Maintenance-margin rate — widens each band toward the entry price.");

    let lookback_field = Field::number(
        "Lookback",
        NumberOpts::int(1, 168).suffix("h"),
        tgt.getter(24.0, |p: &LiqHeatmapParams| {
            (p.lookback_ms as f64 / 3_600_000.0).round()
        }),
        tgt.setter(|p: &mut LiqHeatmapParams, v: f64| {
            p.lookback_ms = (v.max(1.0) * 3_600_000.0) as i64;
        }),
    )
    .description("Warm-up window — how far back positions are tracked before the visible range.");

    // One checkbox per modeled leverage level (`5×…100×`). Each side's notional
    // is spread equally across the checked levels; the bands sit closer to the
    // entry as leverage rises.
    let leverage_field = Field::multi_checkbox(
        "Leverage",
        AVAILABLE_LEVERAGE
            .iter()
            .enumerate()
            .map(|(ix, lev)| {
                MultiCheckItem::new(
                    SharedString::from(format!("{}×", *lev as i64)),
                    tgt.getter(DEFAULT_LEVERAGE_SELECTED[ix], move |p: &LiqHeatmapParams| {
                        p.leverage.get(ix).copied().unwrap_or(false)
                    }),
                    tgt.setter(move |p: &mut LiqHeatmapParams, v: bool| {
                        if let Some(slot) = p.leverage.get_mut(ix) {
                            *slot = v;
                        }
                    }),
                )
            })
            .collect(),
    )
    .description("Leverage levels used to estimate liquidation prices. Each side's notional is split equally across the checked levels.");

    let show_text_field = Field::switch(
        "Show cell values",
        tgt.getter(true, |p: &LiqHeatmapParams| p.settings.show_text),
        tgt.setter(|p: &mut LiqHeatmapParams, v: bool| {
            p.settings.show_text = v;
        }),
    )
    .description("Draw the estimated notional inside each cell when zoomed in enough.");

    let show_profile_field = Field::switch(
        "Show profile",
        tgt.getter(false, |p: &LiqHeatmapParams| p.show_profile),
        tgt.setter(|p: &mut LiqHeatmapParams, v: bool| {
            p.show_profile = v;
        }),
    )
    .description("Right-edge histogram of peak estimated liq notional per price across the visible range — a price-axis summary of the heatmap.");

    let tgt_vis = tgt.clone();
    let profile_width_field = Field::slider(
        "Profile width",
        NumberOpts::float(MIN_PROFILE_WIDTH_PCT, MAX_PROFILE_WIDTH_PCT, 1.0).suffix("%"),
        tgt.getter(DEFAULT_PROFILE_WIDTH_PCT as f64, |p: &LiqHeatmapParams| {
            p.profile_width_pct as f64
        }),
        tgt.setter(|p: &mut LiqHeatmapParams, v: f64| {
            p.profile_width_pct = v.clamp(MIN_PROFILE_WIDTH_PCT, MAX_PROFILE_WIDTH_PCT) as f32;
        }),
    )
    .description("Length of the longest profile bar, as a percent of the chart's plot width.")
    .visible_if(move |cx| tgt_vis.read(cx, |p| p.show_profile).unwrap_or(false));

    let extend_right_field = Field::switch(
        "Extend latest to edge",
        tgt.getter(true, |p: &LiqHeatmapParams| p.settings.extend_right),
        tgt.setter(|p: &mut LiqHeatmapParams, v: bool| {
            p.settings.extend_right = v;
        }),
    )
    .description("Stretch the live candle's column to the right edge of the chart.");

    SettingsForm::new(SharedString::from("liq-heatmap")).group(
        SettingsGroup::new("General")
            .item(bucket_field)
            .item(opacity_field)
            .item(colormap_field)
            .item(mmr_field)
            .item(lookback_field)
            .item(leverage_field)
            .item(show_text_field)
            .item(show_profile_field)
            .item(profile_width_field)
            .item(extend_right_field),
    )
}
