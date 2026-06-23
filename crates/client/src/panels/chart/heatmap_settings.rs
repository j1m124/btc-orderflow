//! Floating settings panel for the orderbook-heatmap overlay. Sibling of
//! [`super::footprint_settings`], scoped to the chart's heatmap render layer.
//! Opened by the gear glyph next to the header "Heatmap" toggle; resolves the
//! focused chart through [`crate::panels::LastFocusedChart`] in the workspace
//! handler.
//!
//! The colour range is a two-handle [`SliderState`] (low + peak) held here —
//! the declarative `SettingsForm` framework is stateless, so the stateful slider
//! lives on the view and writes through `apply_heatmap_settings` on change. Max
//! opacity stays a plain form number field.

use gpui::{
    Action, App, AppContext as _, Context, Entity, FocusHandle, Focusable, Hsla,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString, Styled as _,
    Subscription, WeakEntity, Window, div, px,
};
use gpui_component::slider::{Slider, SliderEvent, SliderScale, SliderState};
use gpui_component::{ActiveTheme as _, v_flex};
use serde::Deserialize;

use super::paint::{COLOR_RANGE_MAX, COLOR_RANGE_MIN, HeatmapSettings};
use crate::panels::ContentPanel;
use crate::settings_form::{Field, NumberOpts, SettingsForm, SettingsGroup};

/// Open the heatmap settings panel. No payload — the workspace handler resolves
/// the focused chart via `LastFocusedChart`, mirroring `OpenChartRenderSettings`.
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = client, no_json)]
pub struct OpenHeatmapSettings;

pub struct HeatmapSettingsView {
    target: WeakEntity<ContentPanel>,
    focus: FocusHandle,
    /// Two-handle colour range (low, peak) in coin units, log scale.
    range: Entity<SliderState>,
    _subs: Vec<Subscription>,
}

impl HeatmapSettingsView {
    pub fn new(
        target: WeakEntity<ContentPanel>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let (lo, peak) = current_range(&target, cx);
        let range = cx.new(|_| {
            SliderState::new()
                .min(COLOR_RANGE_MIN as f32)
                .max(COLOR_RANGE_MAX as f32)
                .step(1.0)
                .scale(SliderScale::Logarithmic)
                .default_value(lo..peak)
        });
        let sub = cx.subscribe(&range, |this, _slider, event: &SliderEvent, cx| {
            let SliderEvent::Change(value) = event;
            let lo = value.start() as f64;
            let peak = value.end() as f64;
            if let Some(panel) = this.target.upgrade() {
                panel.update(cx, |p, cx| {
                    p.apply_heatmap_settings(
                        move |s| {
                            s.color_lo = lo;
                            s.color_peak = peak;
                        },
                        cx,
                    );
                });
            }
            // Repaint this settings view so the thumbs + readout track the drag
            // (the slider notifies its own state, but not us).
            cx.notify();
        });
        Self {
            target,
            focus: cx.focus_handle(),
            range,
            _subs: vec![sub],
        }
    }

    pub fn retarget(
        &mut self,
        target: WeakEntity<ContentPanel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.target = target;
        let (lo, peak) = current_range(&self.target, cx);
        // Programmatic `set_value` doesn't emit `Change`, so this won't loop back
        // into `apply_heatmap_settings`.
        self.range
            .update(cx, |s, cx| s.set_value(lo..peak, window, cx));
        cx.notify();
    }

    pub fn current_target(&self) -> &WeakEntity<ContentPanel> {
        &self.target
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
        let border = cx.theme().border;

        let Some(panel_e) = self.target.upgrade() else {
            return missing_body("Chart no longer available", muted).into_any_element();
        };
        {
            let panel = panel_e.read(cx);
            if panel.chart_state.as_ref().is_none() {
                return missing_body("Not a chart panel", muted).into_any_element();
            }
        }

        let header = div()
            .text_sm()
            .text_color(muted)
            .child(SharedString::from("Orderbook Heatmap"));

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
                div().text_xs().text_color(muted).child(SharedString::from(
                    format!("hide < {}   ·   peak {}", fmt_amt(value.start()), fmt_amt(value.end())),
                )),
            )
            .child(div().text_xs().text_color(muted).child(SharedString::from(
                "Cells below the low handle aren't drawn; the high handle is the top of the ramp.",
            )));

        let form = build_heatmap_form(self.target.clone());
        let inner = form.render(window, cx);
        v_flex()
            .id(SharedString::from("chart-heatmap-settings"))
            .size_full()
            .p_4()
            .gap_3()
            .child(header)
            .child(div().h(px(1.)).bg(border))
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

/// Current (low, peak) of the target chart's heatmap, clamped into the slider
/// domain with `low <= peak`. Defaults when the target isn't a chart.
fn current_range(target: &WeakEntity<ContentPanel>, cx: &App) -> (f32, f32) {
    let s = target
        .upgrade()
        .and_then(|p| p.read(cx).chart_state.as_ref().map(|c| c.heatmap_settings()))
        .unwrap_or_default();
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

fn build_heatmap_form(target: WeakEntity<ContentPanel>) -> SettingsForm {
    let opacity_field = Field::number(
        "Max opacity",
        NumberOpts::int(5, 100).format(|v| SharedString::from(format!("{}%", v.round() as i64))),
        getter_f64(target.clone(), |s| (s.max_opacity as f64 * 100.0).round()),
        setter(target.clone(), |s: &mut HeatmapSettings, v: f64| {
            s.max_opacity = (v / 100.0).clamp(0.05, 1.0) as f32;
        }),
    )
    .description("Opacity of the hottest cell — lower keeps candles readable.");

    let show_text_field = Field::switch(
        "Show cell values",
        getter_bool(target.clone(), |s| s.show_text),
        setter(target.clone(), |s: &mut HeatmapSettings, v: bool| {
            s.show_text = v;
        }),
    )
    .description("Draw the book size inside each cell when zoomed in enough.");

    let extend_right_field = Field::switch(
        "Extend latest to edge",
        getter_bool(target.clone(), |s| s.extend_right),
        setter(target, |s: &mut HeatmapSettings, v: bool| {
            s.extend_right = v;
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

// ─────────────────── closure helpers ───────────────────

fn read_settings<R>(
    target: &WeakEntity<ContentPanel>,
    cx: &App,
    f: impl FnOnce(&HeatmapSettings) -> R,
) -> Option<R> {
    let panel = target.upgrade()?;
    let panel = panel.read(cx);
    let chart = panel.chart_state.as_ref()?;
    let settings = chart.heatmap_settings();
    Some(f(&settings))
}

fn getter_f64<F>(target: WeakEntity<ContentPanel>, f: F) -> impl Fn(&App) -> f64 + 'static
where
    F: Fn(&HeatmapSettings) -> f64 + 'static,
{
    move |cx| read_settings(&target, cx, &f).unwrap_or(0.0)
}

fn getter_bool<F>(target: WeakEntity<ContentPanel>, f: F) -> impl Fn(&App) -> bool + 'static
where
    F: Fn(&HeatmapSettings) -> bool + 'static,
{
    move |cx| read_settings(&target, cx, &f).unwrap_or(false)
}

fn setter<T, F>(target: WeakEntity<ContentPanel>, f: F) -> impl Fn(T, &mut App) + 'static
where
    T: 'static,
    F: Fn(&mut HeatmapSettings, T) + 'static + Clone,
{
    move |value, cx| {
        let Some(panel) = target.upgrade() else {
            return;
        };
        let f = f.clone();
        panel.update(cx, |p, cx| {
            p.apply_heatmap_settings(move |s| f(s, value), cx);
        });
    }
}
