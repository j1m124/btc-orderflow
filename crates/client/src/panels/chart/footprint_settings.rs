//! Floating settings panel for the chart's active footprint render
//! (Cluster or Profile). Sibling of `crate::indicator_settings` but scoped
//! to the chart-level render-mode-as-indicator pattern. Backed by the
//! shared `SettingsForm` framework; the per-render-kind form is rebuilt
//! every render so a header-dropdown switch between Cluster ↔ Profile
//! takes effect immediately.

use gpui::{
    Action, App, Context, FocusHandle, Focusable, Hsla, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, Styled as _, WeakEntity, Window, div, px,
};
use gpui_component::{ActiveTheme as _, v_flex};
use serde::Deserialize;

use super::footprint::{
    BTCUSDT_TICK_SIZE, ColorScope, FootprintParams, RenderKind, RenderMetric, TextMetric,
    WireframeVariant,
};
use crate::panels::ContentPanel;
use crate::settings_form::{DropdownOption, Field, NumberOpts, SettingsForm, SettingsGroup};

/// Open the chart-render settings panel. Carries no payload — the
/// workspace handler resolves the currently-focused chart through
/// [`crate::panels::LastFocusedChart`] and reads its active render
/// from there. Dispatched by the gear glyph on the synthetic render
/// chip pinned at the top of the indicator list.
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = client, no_json)]
pub struct OpenChartRenderSettings;

pub struct ChartRenderSettingsView {
    target: WeakEntity<ContentPanel>,
    focus: FocusHandle,
}

impl ChartRenderSettingsView {
    pub fn new(
        target: WeakEntity<ContentPanel>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            target,
            focus: cx.focus_handle(),
        }
    }

    pub fn retarget(
        &mut self,
        target: WeakEntity<ContentPanel>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.target = target;
        cx.notify();
    }

    pub fn current_target(&self) -> &WeakEntity<ContentPanel> {
        &self.target
    }
}

impl Focusable for ChartRenderSettingsView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for ChartRenderSettingsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let border = cx.theme().border;

        let Some(panel_e) = self.target.upgrade() else {
            return missing_body("Chart no longer available", muted).into_any_element();
        };

        let kind = {
            let panel = panel_e.read(cx);
            let Some(chart) = panel.chart_state.as_ref() else {
                return missing_body("Not a chart panel", muted).into_any_element();
            };
            chart.render_kind()
        };

        let header = div()
            .text_sm()
            .text_color(muted)
            .child(SharedString::from(kind.display_name()));

        let body = match kind {
            RenderKind::Candlestick => v_flex()
                .id(SharedString::from("chart-render-settings-empty"))
                .size_full()
                .p_4()
                .gap_3()
                .child(header)
                .child(div().h(px(1.)).bg(border))
                .child(
                    div()
                        .text_sm()
                        .text_color(muted)
                        .child(SharedString::from(
                            "Switch to a footprint render to configure cells.",
                        )),
                )
                .into_any_element(),
            RenderKind::Cluster | RenderKind::Profile => {
                let form = build_footprint_form(self.target.clone(), kind);
                let inner = form.render(window, cx);
                v_flex()
                    .id(SharedString::from(format!("chart-render-settings-{}", kind.as_id())))
                    .size_full()
                    .p_4()
                    .gap_3()
                    .child(header)
                    .child(div().h(px(1.)).bg(border))
                    .child(inner)
                    .into_any_element()
            }
        };
        body
    }
}

fn missing_body(msg: &'static str, muted: Hsla) -> impl IntoElement {
    div()
        .p_4()
        .text_sm()
        .text_color(muted)
        .child(SharedString::from(msg))
}

fn build_footprint_form(target: WeakEntity<ContentPanel>, kind: RenderKind) -> SettingsForm {
    let form_id = SharedString::from(format!("footprint-{}", kind.as_id()));
    let bucket_field = Field::number(
        "Bucket",
        NumberOpts::float(0.1, 100_000.0, BTCUSDT_TICK_SIZE)
            .format(|v| SharedString::from(format!("{} ticks", (v / BTCUSDT_TICK_SIZE).round() as i64))),
        getter_f64(target.clone(), |p| p.bucket / BTCUSDT_TICK_SIZE),
        setter(target.clone(), |p: &mut FootprintParams, v: f64| {
            let ticks = v.max(1.0).round();
            let parsed = ticks * BTCUSDT_TICK_SIZE;
            if FootprintParams::bucket_is_valid(parsed) {
                p.bucket = parsed;
            }
        }),
    )
    .description("Cell size in ticks ($0.10 each on BTCUSDT-perp).");

    let wireframe_field = Field::dropdown(
        "Wireframe",
        vec![
            DropdownOption::new("behind", "Behind cells"),
            DropdownOption::new("side_ohlc", "Side OHLC"),
            DropdownOption::new("none", "None"),
        ],
        getter_str(target.clone(), |p| wireframe_value(p.wireframe)),
        setter(target.clone(), |p: &mut FootprintParams, v: SharedString| {
            if let Some(w) = wireframe_from_value(v.as_ref()) {
                p.wireframe = w;
            }
        }),
    );

    let render_metric_field = Field::dropdown(
        "Render metric",
        vec![
            DropdownOption::new("volume", "Volume"),
            DropdownOption::new("delta", "Delta"),
            DropdownOption::new("bid_ask", "Sell | Buy"),
        ],
        getter_str(target.clone(), |p| render_metric_value(p.render_metric)),
        setter(target.clone(), |p: &mut FootprintParams, v: SharedString| {
            if let Some(m) = render_metric_from_value(v.as_ref()) {
                p.render_metric = m;
            }
        }),
    );

    let text_metric_field = Field::dropdown(
        "Text metric",
        vec![
            DropdownOption::new("volume", "Volume"),
            DropdownOption::new("delta", "Delta"),
            DropdownOption::new("bid_ask", "Sell | Buy"),
            DropdownOption::new("none", "None"),
        ],
        getter_str(target.clone(), |p| text_metric_value(p.text_metric)),
        setter(target.clone(), |p: &mut FootprintParams, v: SharedString| {
            if let Some(t) = text_metric_from_value(v.as_ref()) {
                p.text_metric = t;
            }
        }),
    );

    let color_scope_field = Field::dropdown(
        "Color scope",
        vec![
            DropdownOption::new("individual", "Per bar"),
            DropdownOption::new("visible", "Visible range"),
            DropdownOption::new("daily", "Daily"),
        ],
        getter_str(target.clone(), |p| color_scope_value(p.color_scope)),
        setter(target, |p: &mut FootprintParams, v: SharedString| {
            if let Some(s) = color_scope_from_value(v.as_ref()) {
                p.color_scope = s;
            }
        }),
    )
    .description("Normalization basis for cell color intensity.");

    SettingsForm::new(form_id).group(
        SettingsGroup::new("General")
            .item(bucket_field)
            .item(wireframe_field)
            .item(render_metric_field)
            .item(text_metric_field)
            .item(color_scope_field),
    )
}

// ─────────────────── closure helpers ───────────────────

fn read_active<R>(
    target: &WeakEntity<ContentPanel>,
    cx: &App,
    f: impl FnOnce(&FootprintParams) -> R,
) -> Option<R> {
    let panel = target.upgrade()?;
    let panel = panel.read(cx);
    let chart = panel.chart_state.as_ref()?;
    let params = chart.active_footprint_params()?;
    Some(f(params))
}

fn getter_f64<F>(
    target: WeakEntity<ContentPanel>,
    f: F,
) -> impl Fn(&App) -> f64 + 'static
where
    F: Fn(&FootprintParams) -> f64 + 'static,
{
    move |cx| read_active(&target, cx, &f).unwrap_or(0.0)
}

fn getter_str<F>(
    target: WeakEntity<ContentPanel>,
    f: F,
) -> impl Fn(&App) -> SharedString + 'static
where
    F: Fn(&FootprintParams) -> SharedString + 'static,
{
    move |cx| read_active(&target, cx, &f).unwrap_or_else(|| SharedString::from(""))
}

fn setter<T, F>(
    target: WeakEntity<ContentPanel>,
    f: F,
) -> impl Fn(T, &mut App) + 'static
where
    T: 'static,
    F: Fn(&mut FootprintParams, T) + 'static + Clone,
{
    move |value, cx| {
        let Some(panel) = target.upgrade() else {
            return;
        };
        let f = f.clone();
        panel.update(cx, |p, cx| {
            p.apply_active_footprint_params(move |params| f(params, value), cx);
        });
    }
}

// ─────────────────── enum encoding ───────────────────

fn wireframe_value(w: WireframeVariant) -> SharedString {
    SharedString::from(match w {
        WireframeVariant::Behind => "behind",
        WireframeVariant::SideOhlc => "side_ohlc",
        WireframeVariant::None => "none",
    })
}

fn wireframe_from_value(s: &str) -> Option<WireframeVariant> {
    Some(match s {
        "behind" => WireframeVariant::Behind,
        "side_ohlc" => WireframeVariant::SideOhlc,
        "none" => WireframeVariant::None,
        _ => return None,
    })
}

fn render_metric_value(m: RenderMetric) -> SharedString {
    SharedString::from(match m {
        RenderMetric::Volume => "volume",
        RenderMetric::Delta => "delta",
        RenderMetric::BidAsk => "bid_ask",
    })
}

fn render_metric_from_value(s: &str) -> Option<RenderMetric> {
    Some(match s {
        "volume" => RenderMetric::Volume,
        "delta" => RenderMetric::Delta,
        "bid_ask" => RenderMetric::BidAsk,
        _ => return None,
    })
}

fn text_metric_value(t: TextMetric) -> SharedString {
    SharedString::from(match t {
        TextMetric::Volume => "volume",
        TextMetric::Delta => "delta",
        TextMetric::BidAsk => "bid_ask",
        TextMetric::None => "none",
    })
}

fn text_metric_from_value(s: &str) -> Option<TextMetric> {
    Some(match s {
        "volume" => TextMetric::Volume,
        "delta" => TextMetric::Delta,
        "bid_ask" => TextMetric::BidAsk,
        "none" => TextMetric::None,
        _ => return None,
    })
}

fn color_scope_value(s: ColorScope) -> SharedString {
    SharedString::from(match s {
        ColorScope::Individual => "individual",
        ColorScope::Visible => "visible",
        ColorScope::Daily => "daily",
    })
}

fn color_scope_from_value(s: &str) -> Option<ColorScope> {
    Some(match s {
        "individual" => ColorScope::Individual,
        "visible" => ColorScope::Visible,
        "daily" => ColorScope::Daily,
        _ => return None,
    })
}
