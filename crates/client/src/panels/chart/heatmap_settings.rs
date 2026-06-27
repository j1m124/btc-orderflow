//! Bespoke settings view for the orderbook-heatmap indicator. Hosted inside the
//! standard indicator settings panel (`indicator_settings.rs`) via
//! [`crate::indicators::IndicatorKind::custom_settings_view`] — the heatmap is a
//! singleton overlay indicator, and this view edits its instance params.
//!
//! Why bespoke rather than a declarative `SettingsForm`: the colour range is a
//! two-handle [`SliderState`] (low + peak) on a logarithmic scale, which the
//! stateless form framework can't express. The stateful slider lives on the
//! view and writes through the instance's [`IndicatorTarget`] on change; max
//! opacity / show-text / extend-right stay plain form fields below it.

use gpui::{
    App, AppContext as _, Context, Entity, FocusHandle, Focusable, Hsla, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, SharedString, Styled as _, Subscription, WeakEntity,
    Window, div,
};
use gpui_component::slider::{Slider, SliderEvent, SliderScale, SliderState};
use gpui_component::{ActiveTheme as _, v_flex};

use super::paint::{COLOR_RANGE_MAX, COLOR_RANGE_MIN};
use crate::indicators::{InstanceId, OrderbookHeatmapParams};
use crate::panels::ContentPanel;
use crate::settings_form::{Field, IndicatorTarget, NumberOpts, SettingsForm, SettingsGroup};

pub struct HeatmapSettingsView {
    /// Routes reads/writes to the heatmap instance's params, going through
    /// `chart.update_indicator` like every other settings surface.
    tgt: IndicatorTarget<OrderbookHeatmapParams>,
    focus: FocusHandle,
    /// Two-handle colour range (low, peak) in coin units, log scale.
    range: Entity<SliderState>,
    _subs: Vec<Subscription>,
}

impl HeatmapSettingsView {
    pub fn new(
        panel: WeakEntity<ContentPanel>,
        id: InstanceId,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let tgt = IndicatorTarget::<OrderbookHeatmapParams>::new(panel, id);
        let (lo, peak) = current_range(&tgt, cx);
        let range = cx.new(|_| {
            SliderState::new()
                .min(COLOR_RANGE_MIN as f32)
                .max(COLOR_RANGE_MAX as f32)
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
            // Repaint this settings view so the thumbs + readout track the drag
            // (the slider notifies its own state, but not us).
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

impl Focusable for HeatmapSettingsView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for HeatmapSettingsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;

        // The host panel renders the "Orderbook Heatmap" label + a divider, so
        // this view starts straight at the colour range. If the instance is gone
        // the host already shows "Indicator was removed"; guard anyway.
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

        let form = build_heatmap_form(&self.tgt);
        let inner = form.render(window, cx);
        v_flex()
            .id(SharedString::from("chart-heatmap-settings"))
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
fn current_range(tgt: &IndicatorTarget<OrderbookHeatmapParams>, cx: &App) -> (f32, f32) {
    let s = tgt.read(cx, |p| p.settings).unwrap_or_default();
    let min = COLOR_RANGE_MIN as f32;
    let max = COLOR_RANGE_MAX as f32;
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

fn build_heatmap_form(tgt: &IndicatorTarget<OrderbookHeatmapParams>) -> SettingsForm {
    let opacity_field = Field::number(
        "Max opacity",
        NumberOpts::int(5, 100).format(|v| SharedString::from(format!("{}%", v.round() as i64))),
        tgt.getter(85.0, |p: &OrderbookHeatmapParams| {
            (p.settings.max_opacity as f64 * 100.0).round()
        }),
        tgt.setter(|p: &mut OrderbookHeatmapParams, v: f64| {
            p.settings.max_opacity = (v / 100.0).clamp(0.05, 1.0) as f32;
        }),
    )
    .description("Opacity of the hottest cell — lower keeps candles readable.");

    let show_text_field = Field::switch(
        "Show cell values",
        tgt.getter(true, |p: &OrderbookHeatmapParams| p.settings.show_text),
        tgt.setter(|p: &mut OrderbookHeatmapParams, v: bool| {
            p.settings.show_text = v;
        }),
    )
    .description("Draw the book size inside each cell when zoomed in enough.");

    let extend_right_field = Field::switch(
        "Extend latest to edge",
        tgt.getter(true, |p: &OrderbookHeatmapParams| p.settings.extend_right),
        tgt.setter(|p: &mut OrderbookHeatmapParams, v: bool| {
            p.settings.extend_right = v;
        }),
    )
    .description("Stretch the live candle's column to the right edge of the chart.");

    SettingsForm::new(SharedString::from("heatmap")).group(
        SettingsGroup::new("General")
            .item(opacity_field)
            .item(show_text_field)
            .item(extend_right_field),
    )
}
