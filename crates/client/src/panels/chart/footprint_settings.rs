//! Floating settings panel for the chart's active footprint render
//! (Cluster or Profile). Opened by the gear glyph on the synthetic render
//! chip; sibling concept to [`crate::indicator_settings`] but scoped to the
//! render-mode-as-indicator pattern rather than an `IndicatorInstance`.
//!
//! Form is button-based for the discrete fields (wireframe / metric /
//! scope) so the view can rebuild every render without keyboard focus
//! loss. The bucket is the one exception — a free-form `InputState`
//! committed on Enter / Blur so a multi-keystroke change doesn't fire a
//! Subscribe/Unsubscribe round-trip per keystroke (per locked design
//! `project_footprint_v1_design`).
//!
//! Workspace owns one singleton instance via the same pattern as
//! `IndicatorSettingsView` — see [`crate::workspace`]. When the user
//! flips the active render kind via the header dropdown, the view's
//! `render()` notices the drift and rebuilds the bucket input so the
//! displayed value tracks the now-active mode's params.

use gpui::{
    Action, App, AppContext as _, Context, Entity, FocusHandle, Focusable, Hsla,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
    Styled as _, Subscription, WeakEntity, Window, div, px,
};
use gpui_component::{
    ActiveTheme as _, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputEvent, InputState},
    v_flex,
};
use serde::Deserialize;

use super::footprint::{
    ColorScope, FootprintParams, RenderKind, RenderMetric, TextMetric, WireframeVariant,
};
use crate::panels::ContentPanel;

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
    /// Bucket numeric input. Recreated whenever the active render kind
    /// drifts away from [`Self::bucket_kind`] so the displayed value
    /// tracks the now-active mode's bucket without a per-render write.
    bucket_input: Entity<InputState>,
    /// Render kind the bucket input was built against. When the user
    /// switches render via the header dropdown, `render()` detects the
    /// mismatch and rebuilds `bucket_input` from scratch.
    bucket_kind: RenderKind,
    /// Subscription powering Enter / Blur → commit-bucket. Kept alive on
    /// the view so it isn't dropped between renders. Rebuilt alongside
    /// `bucket_input`.
    _bucket_sub: Option<Subscription>,
}

impl ChartRenderSettingsView {
    pub fn new(
        target: WeakEntity<ContentPanel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let kind = active_render_kind(&target, cx);
        let (input, sub) = build_bucket_input(&target, kind, window, cx);
        Self {
            target,
            focus: cx.focus_handle(),
            bucket_input: input,
            bucket_kind: kind,
            _bucket_sub: sub,
        }
    }

    pub fn retarget(
        &mut self,
        target: WeakEntity<ContentPanel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.target = target;
        self.rebuild_bucket_input(window, cx);
        cx.notify();
    }

    pub fn current_target(&self) -> &WeakEntity<ContentPanel> {
        &self.target
    }

    fn rebuild_bucket_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let kind = active_render_kind(&self.target, cx);
        let (input, sub) = build_bucket_input(&self.target, kind, window, cx);
        self.bucket_input = input;
        self.bucket_kind = kind;
        self._bucket_sub = sub;
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

        let snap = {
            let panel = panel_e.read(cx);
            let Some(chart) = panel.chart_state.as_ref() else {
                return missing_body("Not a chart panel", muted).into_any_element();
            };
            let kind = chart.render_kind();
            let params = chart.active_footprint_params().copied();
            FormSnapshot { kind, params }
        };

        // Render kind drifted (user picked a different render via the
        // header dropdown while the panel was open) — rebuild the bucket
        // input so the displayed value reflects the new mode's params.
        if snap.kind != self.bucket_kind {
            self.rebuild_bucket_input(window, cx);
        }

        let header = div()
            .text_sm()
            .text_color(muted)
            .child(SharedString::from(snap.kind.display_name()));

        // Candlestick has no params — show a hint rather than an empty
        // form. The user can switch to Cluster / Profile via the header
        // dropdown without closing this window; the next render picks
        // up the new kind.
        let Some(params) = snap.params else {
            return v_flex()
                .id(SharedString::from("chart-render-settings"))
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
                .into_any_element();
        };

        let target = self.target.clone();
        let bucket_input = self.bucket_input.clone();

        v_flex()
            .id(SharedString::from("chart-render-settings"))
            .size_full()
            .p_4()
            .gap_3()
            .child(header)
            .child(div().h(px(1.)).bg(border))
            .child(bucket_row(bucket_input, muted))
            .child(wireframe_row(params.wireframe, target.clone(), cx))
            .child(render_metric_row(params.render_metric, target.clone(), cx))
            .child(text_metric_row(params.text_metric, target.clone(), cx))
            .child(color_scope_row(params.color_scope, target, cx))
            .into_any_element()
    }
}

struct FormSnapshot {
    kind: RenderKind,
    params: Option<FootprintParams>,
}

fn missing_body(msg: &'static str, muted: Hsla) -> impl IntoElement {
    div()
        .p_4()
        .text_sm()
        .text_color(muted)
        .child(SharedString::from(msg))
}

// ────────────────────────── form rows ──────────────────────────

fn bucket_row(input: Entity<InputState>, muted: Hsla) -> gpui::AnyElement {
    h_flex()
        .gap_3()
        .items_center()
        .child(
            div()
                .w(px(90.))
                .text_sm()
                .text_color(muted)
                .child("Bucket (ticks)"),
        )
        .child(div().w(px(120.)).child(Input::new(&input).small()))
        .child(
            div()
                .text_xs()
                .text_color(muted)
                .child(SharedString::from("1 tick = $0.10. Enter to commit")),
        )
        .into_any_element()
}

fn wireframe_row(
    current: WireframeVariant,
    target: WeakEntity<ContentPanel>,
    cx: &mut Context<ChartRenderSettingsView>,
) -> gpui::AnyElement {
    let muted = cx.theme().muted_foreground;
    let mut buttons = h_flex().gap_1();
    for (value, label) in [
        (WireframeVariant::Behind, "Behind"),
        (WireframeVariant::SideOhlc, "Side OHLC"),
        (WireframeVariant::None, "None"),
    ] {
        let target = target.clone();
        let is_active = value == current;
        let btn_id = SharedString::from(format!("chart-render-wireframe-{}", label));
        let mut btn = Button::new(btn_id).label(SharedString::from(label)).xsmall();
        btn = if is_active { btn.primary() } else { btn.ghost() };
        btn = btn.on_click(cx.listener(move |_this, _ev, _w, cx| {
            apply_mutation(&target, move |p: &mut FootprintParams| p.wireframe = value, cx);
        }));
        buttons = buttons.child(btn);
    }
    h_flex()
        .gap_3()
        .items_center()
        .child(div().w(px(90.)).text_sm().text_color(muted).child("Wireframe"))
        .child(buttons)
        .into_any_element()
}

fn render_metric_row(
    current: RenderMetric,
    target: WeakEntity<ContentPanel>,
    cx: &mut Context<ChartRenderSettingsView>,
) -> gpui::AnyElement {
    let muted = cx.theme().muted_foreground;
    let mut buttons = h_flex().gap_1();
    for (value, label) in [
        (RenderMetric::Volume, "Volume"),
        (RenderMetric::Delta, "Delta"),
        (RenderMetric::BidAsk, "Sell | Buy"),
    ] {
        let target = target.clone();
        let is_active = value == current;
        let btn_id = SharedString::from(format!("chart-render-metric-{}", label));
        let mut btn = Button::new(btn_id).label(SharedString::from(label)).xsmall();
        btn = if is_active { btn.primary() } else { btn.ghost() };
        btn = btn.on_click(cx.listener(move |_this, _ev, _w, cx| {
            apply_mutation(
                &target,
                move |p: &mut FootprintParams| p.render_metric = value,
                cx,
            );
        }));
        buttons = buttons.child(btn);
    }
    h_flex()
        .gap_3()
        .items_center()
        .child(div().w(px(90.)).text_sm().text_color(muted).child("Render"))
        .child(buttons)
        .into_any_element()
}

fn text_metric_row(
    current: TextMetric,
    target: WeakEntity<ContentPanel>,
    cx: &mut Context<ChartRenderSettingsView>,
) -> gpui::AnyElement {
    let muted = cx.theme().muted_foreground;
    let mut buttons = h_flex().gap_1();
    for (value, label) in [
        (TextMetric::Volume, "Volume"),
        (TextMetric::Delta, "Delta"),
        (TextMetric::BidAsk, "Sell | Buy"),
        (TextMetric::None, "None"),
    ] {
        let target = target.clone();
        let is_active = value == current;
        let btn_id = SharedString::from(format!("chart-render-text-{}", label));
        let mut btn = Button::new(btn_id).label(SharedString::from(label)).xsmall();
        btn = if is_active { btn.primary() } else { btn.ghost() };
        btn = btn.on_click(cx.listener(move |_this, _ev, _w, cx| {
            apply_mutation(
                &target,
                move |p: &mut FootprintParams| p.text_metric = value,
                cx,
            );
        }));
        buttons = buttons.child(btn);
    }
    h_flex()
        .gap_3()
        .items_center()
        .child(div().w(px(90.)).text_sm().text_color(muted).child("Text"))
        .child(buttons)
        .into_any_element()
}

fn color_scope_row(
    current: ColorScope,
    target: WeakEntity<ContentPanel>,
    cx: &mut Context<ChartRenderSettingsView>,
) -> gpui::AnyElement {
    let muted = cx.theme().muted_foreground;
    let mut buttons = h_flex().gap_1();
    for (value, label) in [
        (ColorScope::Individual, "Per bar"),
        (ColorScope::Visible, "Visible"),
        (ColorScope::Daily, "Daily"),
    ] {
        let target = target.clone();
        let is_active = value == current;
        let btn_id = SharedString::from(format!("chart-render-scope-{}", label));
        let mut btn = Button::new(btn_id).label(SharedString::from(label)).xsmall();
        btn = if is_active { btn.primary() } else { btn.ghost() };
        btn = btn.on_click(cx.listener(move |_this, _ev, _w, cx| {
            apply_mutation(
                &target,
                move |p: &mut FootprintParams| p.color_scope = value,
                cx,
            );
        }));
        buttons = buttons.child(btn);
    }
    h_flex()
        .gap_3()
        .items_center()
        .child(div().w(px(90.)).text_sm().text_color(muted).child("Color scope"))
        .child(buttons)
        .into_any_element()
}

// ────────────────────────── bucket input plumbing ──────────────────────────

/// Build a fresh `InputState` seeded with the current bucket, plus a
/// subscription that commits the parsed value on Enter / Blur. The
/// subscription captures only the weak `target` ref, so the input
/// works after a retarget too.
fn build_bucket_input(
    target: &WeakEntity<ContentPanel>,
    _kind: RenderKind,
    window: &mut Window,
    cx: &mut Context<ChartRenderSettingsView>,
) -> (Entity<InputState>, Option<Subscription>) {
    let current = read_active_bucket(target, cx);
    let seed: SharedString = match current {
        Some(v) => SharedString::from(format_bucket_ticks(v)),
        None => SharedString::default(),
    };
    let state = cx.new(|cx| InputState::new(window, cx).placeholder("Ticks"));
    if !seed.is_empty() {
        state.update(cx, |s, cx| s.set_value(seed, window, cx));
    }
    let target_for_sub = target.clone();
    let sub = cx.subscribe_in(&state, window, move |_this, input, ev: &InputEvent, _w, cx| {
        match ev {
            InputEvent::PressEnter { .. } | InputEvent::Blur => {
                let text = input.read(cx).value();
                commit_bucket(&target_for_sub, text.as_ref(), cx);
            }
            _ => {}
        }
    });
    (state, Some(sub))
}

fn read_active_bucket(
    target: &WeakEntity<ContentPanel>,
    cx: &App,
) -> Option<f64> {
    let panel = target.upgrade()?;
    let p = panel.read(cx);
    let chart = p.chart_state.as_ref()?;
    chart.active_footprint_params().map(|p| p.bucket)
}

fn active_render_kind(target: &WeakEntity<ContentPanel>, cx: &App) -> RenderKind {
    target
        .upgrade()
        .and_then(|panel| {
            panel
                .read(cx)
                .chart_state
                .as_ref()
                .map(|c| c.render_kind())
        })
        .unwrap_or(RenderKind::Candlestick)
}

/// Format the stored quote-currency bucket as an integer multiple of the
/// symbol's tick size. Non-integer multiples render with one decimal so a
/// user-typed `2.5` (= $0.25 on BTCUSDT) round-trips cleanly.
fn format_bucket_ticks(v: f64) -> String {
    let ticks = v / super::footprint::BTCUSDT_TICK_SIZE;
    if (ticks - ticks.round()).abs() < 1e-6 {
        format!("{}", ticks.round() as i64)
    } else {
        format!("{:.1}", ticks)
    }
}

fn commit_bucket(
    target: &WeakEntity<ContentPanel>,
    text: &str,
    cx: &mut Context<ChartRenderSettingsView>,
) {
    // User enters tick count; storage is the equivalent quote-currency
    // bucket (ticks × tick_size). Reject non-positive or non-finite ticks
    // up front so the f64 conversion can't produce a zero bucket.
    let Ok(ticks) = text.trim().parse::<f64>() else {
        return;
    };
    if !ticks.is_finite() || ticks <= 0.0 {
        return;
    }
    let parsed = ticks * super::footprint::BTCUSDT_TICK_SIZE;
    if !FootprintParams::bucket_is_valid(parsed) {
        return;
    }
    let Some(panel) = target.upgrade() else {
        return;
    };
    panel.update(cx, |p, cx| {
        p.apply_active_footprint_params(
            move |params| {
                params.bucket = parsed;
            },
            cx,
        );
    });
}

fn apply_mutation<F>(
    target: &WeakEntity<ContentPanel>,
    f: F,
    cx: &mut Context<ChartRenderSettingsView>,
) where
    F: FnOnce(&mut FootprintParams) + 'static,
{
    let Some(panel) = target.upgrade() else {
        return;
    };
    panel.update(cx, |p, cx| {
        p.apply_active_footprint_params(f, cx);
    });
}
