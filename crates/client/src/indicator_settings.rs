//! Floating settings panel for an attached indicator. One workspace-owned
//! singleton (per the locked design): clicking the gear on a chip retargets
//! the existing window to the new instance and the form re-renders. The
//! window itself is the reusable `FloatingWindow` wrapper (drag title bar,
//! corner resize, X to close).
//!
//! Form widgets are intentionally button-based (no `InputState`) so the
//! view can rebuild on every render without losing keyboard focus or
//! cursor position. Each input is a small stepper / dropdown / popover
//! click that mutates the instance's typed params via
//! `ChartState::update_indicator` + a downcast on `kind.as_any_mut()`.
//!
//! The color slot uses gpui-component's `ColorPicker` (popover trigger →
//! palette + sliders + hex). State entities are owned by this view and
//! resync to the target instance's current colors on retarget.

use gpui::{
    Action, App, AppContext as _, Context, Entity, FocusHandle, Focusable, Hsla,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, WeakEntity, Window, div, px,
};
use gpui_component::{
    ActiveTheme as _, Sizable as _,
    button::{Button, ButtonVariants as _},
    color_picker::{ColorPicker, ColorPickerEvent, ColorPickerState},
    h_flex, v_flex,
};
use serde::Deserialize;

use crate::indicators::{
    BarStatGrade, BarStatParams, BbParams, COLOR_PALETTE_SIZE, InstanceId, Placement, Source,
    VolumeDeltaMode, VolumeDeltaParams, VrvpParams, palette_color_for,
};
use crate::panels::ContentPanel;
use crate::volume_profile::{AnchorEdge, VpDeltaScale, VpRenderMode};
use crate::volume_profile::params::{
    BTCUSDT_TICK_SIZE, BUCKET_TICKS_MAX, BUCKET_TICKS_MIN, VA_PERCENT_MAX, VA_PERCENT_MIN,
    WIDTH_PCT_MAX, WIDTH_PCT_MIN,
};

/// Open the settings panel for an indicator on the currently-focused chart.
/// Carries the instance id; the workspace resolves the target chart via
/// `LastFocusedChart` (the chip body click on the chip already sets it).
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = client, no_json)]
pub struct OpenIndicatorSettings(pub u64);

/// The hosted view inside the floating settings window. Holds a weak handle
/// to the chart panel + the instance id; rebuilds the form on every render
/// by looking up the live state through the weak ref.
pub struct IndicatorSettingsView {
    target: WeakEntity<ContentPanel>,
    instance_id: InstanceId,
    focus: FocusHandle,
    /// One color-picker state per slot the current kind declares, indexed
    /// parallel to `kind.color_slots()`. Built fresh on construction and
    /// on `retarget` (so retargeting from a 2-slot MACD to a 1-slot SMA
    /// drops the extra state). Owned here so each picker's popover state
    /// (open/closed, hex input, sliders) survives across renders.
    color_states: Vec<Entity<ColorPickerState>>,
    /// Subscriptions parallel to `color_states`. Each closure dispatches a
    /// `set_indicator_color(id, slot, color)` against the *current* target
    /// (read at event-time, so retarget-after-create still routes correctly).
    /// Cleared and rebuilt whenever `color_states` is rebuilt.
    _subscriptions: Vec<Subscription>,
}

impl IndicatorSettingsView {
    pub fn new(
        target: WeakEntity<ContentPanel>,
        instance_id: InstanceId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self {
            target,
            instance_id,
            focus: cx.focus_handle(),
            color_states: Vec::new(),
            _subscriptions: Vec::new(),
        };
        this.rebuild_color_states(window, cx);
        this
    }

    /// Retarget when the user clicks a different chip while the window is
    /// already open. View re-renders against the new instance, and color
    /// picker states are reconstructed for the new kind's slot count.
    pub fn retarget(
        &mut self,
        target: WeakEntity<ContentPanel>,
        instance_id: InstanceId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.target = target;
        self.instance_id = instance_id;
        self.rebuild_color_states(window, cx);
        cx.notify();
    }

    pub fn current_target(&self) -> &WeakEntity<ContentPanel> {
        &self.target
    }

    pub fn current_instance_id(&self) -> InstanceId {
        self.instance_id
    }

    /// (Re)build `color_states` + matching subscriptions to match the
    /// current target instance's color-slot count. Each subscription
    /// captures only the slot index (a plain usize) — the rest is read
    /// at event time from `self.instance_id` / `self.target`, so the same
    /// state entities keep working after a later retarget.
    fn rebuild_color_states(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let initial = lookup_slot_colors(&self.target, self.instance_id, cx);
        self.color_states.clear();
        self._subscriptions.clear();
        for (slot, color) in initial.into_iter().enumerate() {
            let state = cx.new(|cx| ColorPickerState::new(window, cx).default_value(color));
            let sub = cx.subscribe(&state, move |this, _state, ev: &ColorPickerEvent, cx| {
                if let ColorPickerEvent::Change(Some(color)) = ev {
                    apply_slot_color(this, slot, *color, cx);
                }
            });
            self.color_states.push(state);
            self._subscriptions.push(sub);
        }
    }
}

impl Focusable for IndicatorSettingsView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for IndicatorSettingsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let target = self.target.clone();
        let id = self.instance_id;

        // Snapshot the instance's display-only fields out of the borrow so
        // the per-kind render fns can take `&mut cx` freely (otherwise the
        // immutable `panel.read(cx)` borrow conflicts with `cx.listener`
        // inside the render fns).
        let Some(panel_e) = target.upgrade() else {
            return missing_body("Indicator no longer available", muted).into_any_element();
        };
        let snapshot = {
            let panel = panel_e.read(cx);
            let Some(chart) = panel.chart_state.as_ref() else {
                return missing_body("Not a chart panel", muted).into_any_element();
            };
            let Some(inst) = chart.indicators().iter().find(|i| i.id == id) else {
                return missing_body("Indicator was removed", muted).into_any_element();
            };
            InstanceSnapshot {
                kind_id: inst.kind_id,
                label: inst.kind.label(),
                placement: inst.placement,
                params: inst.kind.params_json(),
                color_slot_labels: inst.kind.color_slots(),
            }
        };
        // Defensive: if the picker-state Vec is out of sync with the
        // current kind's slot count (e.g., the kind's params were mutated
        // in a way that changed slot count — not a thing today, but
        // future-proofing) rebuild now so the renderer doesn't index past
        // the end. Skip the work in the common case where counts already
        // match.
        if self.color_states.len() != snapshot.color_slot_labels.len() {
            self.rebuild_color_states(window, cx);
        }
        let kind_body = match snapshot.kind_id {
            "bb" => render_bb(&snapshot, target.clone(), id, cx),
            "volume" => render_volume(&snapshot, target.clone(), id, cx),
            "volume_delta" => render_volume_delta(&snapshot, target.clone(), id, cx),
            "bar_stat" => render_bar_stat(&snapshot, target.clone(), id, cx),
            "vrvp" => render_vrvp(&snapshot, target.clone(), id, cx),
            "liq_bars" => render_liq_bars(&snapshot, target.clone(), id, cx),
            _ => div()
                .text_color(muted)
                .child("Unknown indicator kind")
                .into_any_element(),
        };
        let label = snapshot.label.clone();

        // Generic color section: one row per slot the kind declares. Skipped
        // for kinds with no slots (Volume).
        let color_rows: Vec<gpui::AnyElement> = snapshot
            .color_slot_labels
            .iter()
            .zip(self.color_states.iter())
            .map(|(slot_label, state)| color_row(slot_label.clone(), state, cx))
            .collect();
        let has_color_section = !color_rows.is_empty();

        // Form body lives inside a scrollable container so windows whose
        // content exceeds the FloatingWindow's height stay usable. Per the
        // CLAUDE.md gotcha, the inner content uses `.w_full()` (not
        // `.size_full()`) so the outer `overflow_y_scroll` actually has
        // something to scroll against.
        let mut body = v_flex()
            .w_full()
            .p_4()
            .gap_3()
            .child(
                div()
                    .text_sm()
                    .text_color(muted)
                    .child(SharedString::from(format!("{}", label))),
            )
            .child(div().h(px(1.)).bg(cx.theme().border))
            .child(kind_body);
        if has_color_section {
            body = body
                .child(div().h(px(1.)).bg(cx.theme().border))
                .child(v_flex().gap_2().children(color_rows));
        }
        v_flex()
            .id(SharedString::from(format!("indicator-settings-{}", id)))
            .size_full()
            .child(
                div()
                    .id(SharedString::from(format!("indicator-settings-scroll-{}", id)))
                    .flex_1()
                    .w_full()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(body),
            )
            .into_any_element()
    }
}

/// Snapshot of an instance's read-only fields used to render the form
/// without holding the chart-state borrow open. Per-kind renders pull
/// concrete params from `params` via `serde_json::Value::pointer`.
struct InstanceSnapshot {
    kind_id: &'static str,
    label: SharedString,
    placement: Placement,
    params: serde_json::Value,
    /// Names of the kind's color slots, snapshot at render time so we can
    /// pair them with `IndicatorSettingsView.color_states` without keeping
    /// the chart borrow open.
    color_slot_labels: Vec<SharedString>,
}

fn missing_body(msg: &'static str, muted: Hsla) -> impl IntoElement {
    div()
        .p_4()
        .text_sm()
        .text_color(muted)
        .child(SharedString::from(msg))
}

// ────────────────────────────── per-kind forms ──────────────────────────────

/// MA Suite form: one row per entry, plus a footer "+ Add MA" button.
/// Each row exposes the entry's color (inline ColorPicker), flavor
/// (SMA / EMA toggle), period (double-click value to edit; stepper
/// buttons for ±1), source pickers, and a per-row remove button.
/// Mutations route through `update_indicator` so the recompute + color-
/// slot resync happen in one place.
#[allow(clippy::too_many_arguments)]

fn render_bb(
    snap: &InstanceSnapshot,
    target: WeakEntity<ContentPanel>,
    id: InstanceId,
    cx: &mut Context<IndicatorSettingsView>,
) -> gpui::AnyElement {
    let p = snap
        .params
        .pointer("/period")
        .and_then(|v| v.as_u64())
        .unwrap_or(20) as usize;
    let sd = snap
        .params
        .pointer("/stddev")
        .and_then(|v| v.as_f64())
        .unwrap_or(2.0);
    let s = read_source(&snap.params).unwrap_or(Source::Close);
    v_flex()
        .gap_2()
        .child(period_row(
            "Period",
            p,
            target.clone(),
            id,
            |kind, delta| {
                mutate::<BbParams>(kind, |x| {
                    x.period = step_period(x.period, delta);
                });
            },
            cx,
        ))
        .child(float_row(
            "StdDev",
            format!("{:.1}", sd),
            target.clone(),
            id,
            |kind, delta| {
                mutate::<BbParams>(kind, |x| {
                    x.stddev = (x.stddev + delta as f64 * 0.5).clamp(0.5, 5.0);
                });
            },
            cx,
        ))
        .child(source_row(
            "Source",
            s,
            target,
            id,
            |kind, src| {
                mutate::<BbParams>(kind, |x| x.source = src);
            },
            cx,
        ))
        .into_any_element()
}

fn render_volume(
    snap: &InstanceSnapshot,
    target: WeakEntity<ContentPanel>,
    id: InstanceId,
    cx: &mut Context<IndicatorSettingsView>,
) -> gpui::AnyElement {
    let current = snap.placement;
    v_flex()
        .gap_2()
        .child(label_row("Placement"))
        .child(
            h_flex()
                .gap_2()
                .child(placement_btn(
                    "Overlay",
                    current == Placement::Overlay,
                    target.clone(),
                    id,
                    Placement::Overlay,
                    cx,
                ))
                .child(placement_btn(
                    "Pane",
                    current == Placement::Pane,
                    target,
                    id,
                    Placement::Pane,
                    cx,
                )),
        )
        .into_any_element()
}

/// VRVP form — three sections (Layout / Reference levels / Reset). All
/// mutations route through `chart.update_indicator(id, |k| mutate::<VrvpParams>(k, ...))`,
/// which re-runs compute + recompute → repaint pipeline in one place.
///
/// Color editing isn't surfaced here in v1: `VolumeProfileParams` exposes
/// 5 color slots (volume / bull / bear / poc / va) that the painter reads
/// directly, but the per-color picker UI is deferred to Phase 15 polish
/// alongside theme-derived default re-tinting. Sensible defaults from
/// `VolumeProfileParams::default()` ship in the meantime.
fn render_vrvp(
    snap: &InstanceSnapshot,
    target: WeakEntity<ContentPanel>,
    id: InstanceId,
    cx: &mut Context<IndicatorSettingsView>,
) -> gpui::AnyElement {
    let muted = cx.theme().muted_foreground;
    let ticks = snap
        .params
        .pointer("/params/bucket_ticks")
        .and_then(|v| v.as_u64())
        .unwrap_or(100) as u32;
    let render_mode = read_vp_render_mode(&snap.params).unwrap_or_default();
    let delta_scale = read_vp_delta_scale(&snap.params).unwrap_or_default();
    let width_pct = snap
        .params
        .pointer("/params/width_pct")
        .and_then(|v| v.as_u64())
        .unwrap_or(30) as u8;
    let anchor = read_vp_anchor(&snap.params).unwrap_or(AnchorEdge::Right);
    let va_percent = snap
        .params
        .pointer("/params/va_percent")
        .and_then(|v| v.as_u64())
        .unwrap_or(70) as u8;
    let show_poc = snap.params.pointer("/params/show_poc").and_then(|v| v.as_bool()).unwrap_or(true);
    let show_va = snap.params.pointer("/params/show_va").and_then(|v| v.as_bool()).unwrap_or(true);
    let show_va_highlight = snap
        .params
        .pointer("/params/show_va_highlight")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let show_labels = snap.params.pointer("/params/show_labels").and_then(|v| v.as_bool()).unwrap_or(true);

    let bucket_dollars = ticks as f64 * BTCUSDT_TICK_SIZE;

    // ── Layout ──
    let mut layout = v_flex().gap_2();
    layout = layout.child(label_row("Layout"));
    layout = layout.child(stepper_row(
        "Bucket",
        format!("{} ticks (${:.2})", ticks, bucket_dollars),
        target.clone(),
        id,
        step_vrvp_bucket_ticks,
        cx,
    ));
    layout = layout.child(vrvp_mode_row(render_mode, target.clone(), id, cx));
    if matches!(render_mode, VpRenderMode::Delta) {
        layout = layout.child(vrvp_delta_scale_row(delta_scale, target.clone(), id, cx));
    }
    layout = layout.child(stepper_row(
        "Width",
        format!("{}%", width_pct),
        target.clone(),
        id,
        step_vrvp_width_pct,
        cx,
    ));
    layout = layout.child(vrvp_anchor_row(anchor, target.clone(), id, cx));

    // ── Reference levels ──
    let mut levels = v_flex().gap_2();
    levels = levels.child(label_row("Reference levels"));
    levels = levels.child(vrvp_toggle_row(
        "POC line",
        show_poc,
        target.clone(),
        id,
        |k, v| mutate::<VrvpParams>(k, |x| x.params.show_poc = v),
        cx,
    ));
    levels = levels.child(vrvp_toggle_row(
        "VA lines",
        show_va,
        target.clone(),
        id,
        |k, v| mutate::<VrvpParams>(k, |x| x.params.show_va = v),
        cx,
    ));
    levels = levels.child(vrvp_toggle_row(
        "VA highlight",
        show_va_highlight,
        target.clone(),
        id,
        |k, v| mutate::<VrvpParams>(k, |x| x.params.show_va_highlight = v),
        cx,
    ));
    levels = levels.child(vrvp_toggle_row(
        "Labels",
        show_labels,
        target.clone(),
        id,
        |k, v| mutate::<VrvpParams>(k, |x| x.params.show_labels = v),
        cx,
    ));
    levels = levels.child(stepper_row(
        "VA %",
        format!("{}%", va_percent),
        target.clone(),
        id,
        step_vrvp_va_percent,
        cx,
    ));

    // ── Reset ──
    let target_reset = target.clone();
    let reset_btn = Button::new(SharedString::from(format!("vrvp-reset-{}", id)))
        .label(SharedString::from("Reset style"))
        .small()
        .ghost()
        .on_click(cx.listener(move |_this, _ev, _w, cx| {
            let Some(panel) = target_reset.upgrade() else {
                return;
            };
            panel.update(cx, |p, cx| {
                if let Some(chart) = p.chart_state.as_mut() {
                    chart.update_indicator(id, |kind| {
                        mutate::<VrvpParams>(kind, |x| x.params.reset_styles());
                    });
                }
                p.refresh_chart_footprint_sub(cx);
                cx.notify();
                crate::panels::request_layout_save(cx);
            });
        }));

    v_flex()
        .gap_3()
        .child(layout)
        .child(div().h(px(1.)).bg(cx.theme().border))
        .child(levels)
        .child(div().h(px(1.)).bg(cx.theme().border))
        .child(
            div()
                .text_xs()
                .text_color(muted)
                .child("Color editing arrives in a follow-up polish pass."),
        )
        .child(reset_btn)
        .into_any_element()
}

fn read_vp_render_mode(params: &serde_json::Value) -> Option<VpRenderMode> {
    let s = params.pointer("/params/render_mode")?.as_str()?;
    Some(match s {
        "volume" => VpRenderMode::Volume,
        "delta" => VpRenderMode::Delta,
        "vol_delta_outline" => VpRenderMode::VolDeltaOutline,
        _ => return None,
    })
}

fn read_vp_delta_scale(params: &serde_json::Value) -> Option<VpDeltaScale> {
    let s = params.pointer("/params/delta_scale")?.as_str()?;
    Some(match s {
        "per_row" => VpDeltaScale::PerRow,
        "whole_profile" => VpDeltaScale::WholeProfile,
        _ => return None,
    })
}

fn read_vp_anchor(params: &serde_json::Value) -> Option<AnchorEdge> {
    let s = params.pointer("/params/anchor")?.as_str()?;
    Some(match s {
        "right" => AnchorEdge::Right,
        "left" => AnchorEdge::Left,
        _ => return None,
    })
}

/// Render-mode picker row — three buttons (Volume / Delta / Volume+Delta).
/// Mode change re-runs compute, so the new render-mode bars (or empty
/// output, if cells aren't loaded yet) show up next paint.
fn vrvp_mode_row(
    current: VpRenderMode,
    target: WeakEntity<ContentPanel>,
    id: InstanceId,
    cx: &mut Context<IndicatorSettingsView>,
) -> gpui::AnyElement {
    let muted = cx.theme().muted_foreground;
    let mut buttons = h_flex().gap_1();
    for m in VpRenderMode::ALL {
        let target = target.clone();
        let mode = *m;
        let is_active = mode == current;
        let btn_id = SharedString::from(format!("vrvp-mode-{}-{}", id, mode.label()));
        let mut btn = Button::new(btn_id)
            .label(SharedString::from(mode.label()))
            .xsmall();
        btn = if is_active { btn.primary() } else { btn.ghost() };
        btn = btn.on_click(cx.listener(move |_this, _ev, _w, cx| {
            let Some(panel) = target.upgrade() else {
                return;
            };
            panel.update(cx, |p, cx| {
                if let Some(chart) = p.chart_state.as_mut() {
                    chart.update_indicator(id, |kind| {
                        mutate::<VrvpParams>(kind, |x| x.params.render_mode = mode);
                    });
                }
                p.refresh_chart_footprint_sub(cx);
                cx.notify();
                crate::panels::request_layout_save(cx);
            });
        }));
        buttons = buttons.child(btn);
    }
    h_flex()
        .gap_3()
        .items_center()
        .child(div().w(px(90.)).text_sm().text_color(muted).child("Mode"))
        .child(buttons)
        .into_any_element()
}

fn vrvp_delta_scale_row(
    current: VpDeltaScale,
    target: WeakEntity<ContentPanel>,
    id: InstanceId,
    cx: &mut Context<IndicatorSettingsView>,
) -> gpui::AnyElement {
    let muted = cx.theme().muted_foreground;
    let mut buttons = h_flex().gap_1();
    for s in VpDeltaScale::ALL {
        let target = target.clone();
        let scale = *s;
        let is_active = scale == current;
        let btn_id = SharedString::from(format!("vrvp-scale-{}-{}", id, scale.label()));
        let mut btn = Button::new(btn_id)
            .label(SharedString::from(scale.label()))
            .xsmall();
        btn = if is_active { btn.primary() } else { btn.ghost() };
        btn = btn.on_click(cx.listener(move |_this, _ev, _w, cx| {
            let Some(panel) = target.upgrade() else {
                return;
            };
            panel.update(cx, |p, cx| {
                if let Some(chart) = p.chart_state.as_mut() {
                    chart.update_indicator(id, |kind| {
                        mutate::<VrvpParams>(kind, |x| x.params.delta_scale = scale);
                    });
                }
                p.refresh_chart_footprint_sub(cx);
                cx.notify();
                crate::panels::request_layout_save(cx);
            });
        }));
        buttons = buttons.child(btn);
    }
    h_flex()
        .gap_3()
        .items_center()
        .child(div().w(px(90.)).text_sm().text_color(muted).child("Scaling"))
        .child(buttons)
        .into_any_element()
}

fn vrvp_anchor_row(
    current: AnchorEdge,
    target: WeakEntity<ContentPanel>,
    id: InstanceId,
    cx: &mut Context<IndicatorSettingsView>,
) -> gpui::AnyElement {
    let muted = cx.theme().muted_foreground;
    let mut buttons = h_flex().gap_1();
    for a in AnchorEdge::ALL {
        let target = target.clone();
        let anchor = *a;
        let is_active = anchor == current;
        let btn_id = SharedString::from(format!("vrvp-anchor-{}-{}", id, anchor.label()));
        let mut btn = Button::new(btn_id)
            .label(SharedString::from(anchor.label()))
            .xsmall();
        btn = if is_active { btn.primary() } else { btn.ghost() };
        btn = btn.on_click(cx.listener(move |_this, _ev, _w, cx| {
            let Some(panel) = target.upgrade() else {
                return;
            };
            panel.update(cx, |p, cx| {
                if let Some(chart) = p.chart_state.as_mut() {
                    chart.update_indicator(id, |kind| {
                        mutate::<VrvpParams>(kind, |x| x.params.anchor = anchor);
                    });
                }
                p.refresh_chart_footprint_sub(cx);
                cx.notify();
                crate::panels::request_layout_save(cx);
            });
        }));
        buttons = buttons.child(btn);
    }
    h_flex()
        .gap_3()
        .items_center()
        .child(div().w(px(90.)).text_sm().text_color(muted).child("Anchor"))
        .child(buttons)
        .into_any_element()
}

/// Single-click toggle for a bool field. Button label flips between
/// `On` / `Off` based on current state; primary variant when on, ghost
/// when off, so the user gets clear feedback without a checkbox widget.
fn vrvp_toggle_row(
    label: &'static str,
    current: bool,
    target: WeakEntity<ContentPanel>,
    id: InstanceId,
    mutate_fn: fn(&mut Box<dyn crate::indicators::IndicatorKind>, bool),
    cx: &mut Context<IndicatorSettingsView>,
) -> gpui::AnyElement {
    let muted = cx.theme().muted_foreground;
    let btn_id = SharedString::from(format!("vrvp-tog-{}-{}", id, label));
    let target_click = target.clone();
    let mut btn = Button::new(btn_id)
        .label(SharedString::from(if current { "On" } else { "Off" }))
        .xsmall();
    btn = if current { btn.primary() } else { btn.ghost() };
    let btn = btn.on_click(cx.listener(move |_this, _ev, _w, cx| {
        let next = !current;
        let Some(panel) = target_click.upgrade() else {
            return;
        };
        panel.update(cx, |p, cx| {
            if let Some(chart) = p.chart_state.as_mut() {
                chart.update_indicator(id, |kind| mutate_fn(kind, next));
            }
            p.refresh_chart_footprint_sub(cx);
            cx.notify();
            crate::panels::request_layout_save(cx);
        });
    }));
    h_flex()
        .gap_3()
        .items_center()
        .child(div().w(px(90.)).text_sm().text_color(muted).child(label))
        .child(btn)
        .into_any_element()
}

/// Stepper handler — clamps to [BUCKET_TICKS_MIN, BUCKET_TICKS_MAX]. Steps
/// by 10 ticks per click (= $1 at the BTCUSDT-perp tick) so the user
/// reaches sensible bucket sizes (50, 100, 200 ticks) in a few clicks.
///
/// Bucket changes are the one VRVP edit that needs a footprint-sub refresh —
/// the desired bucket set shifts. `apply_mutation` is invoked through
/// `chart.update_indicator` which doesn't see ContentPanel, so the refresh
/// happens via the panel-update closure inside `stepper_row`'s listener.
/// We accept one redundant refresh on width/va-percent edits — `refresh_chart_footprint_sub`
/// short-circuits when the desired set matches.
fn step_vrvp_bucket_ticks(kind: &mut Box<dyn crate::indicators::IndicatorKind>, delta: i32) {
    mutate::<VrvpParams>(kind, |x| {
        let cur = x.params.bucket_ticks as i64;
        let nxt = (cur + delta as i64 * 10)
            .clamp(BUCKET_TICKS_MIN as i64, BUCKET_TICKS_MAX as i64);
        x.params.bucket_ticks = nxt as u32;
    });
}

fn step_vrvp_width_pct(kind: &mut Box<dyn crate::indicators::IndicatorKind>, delta: i32) {
    mutate::<VrvpParams>(kind, |x| {
        let cur = x.params.width_pct as i32;
        let nxt = (cur + delta * 5).clamp(WIDTH_PCT_MIN as i32, WIDTH_PCT_MAX as i32);
        x.params.width_pct = nxt as u8;
    });
}

fn step_vrvp_va_percent(kind: &mut Box<dyn crate::indicators::IndicatorKind>, delta: i32) {
    mutate::<VrvpParams>(kind, |x| {
        let cur = x.params.va_percent as i32;
        let nxt = (cur + delta * 5).clamp(VA_PERCENT_MIN as i32, VA_PERCENT_MAX as i32);
        x.params.va_percent = nxt as u8;
    });
}

/// Volume Delta form: a single Mode selector (Histogram / CVD / Both). No
/// placement toggle — kind is PaneOnly. Mode change routes through
/// `update_indicator`, which re-runs `compute` AND `sync_colors` so the
/// CVD color slot appears/disappears with the mode.
fn render_volume_delta(
    snap: &InstanceSnapshot,
    target: WeakEntity<ContentPanel>,
    id: InstanceId,
    cx: &mut Context<IndicatorSettingsView>,
) -> gpui::AnyElement {
    let current = read_volume_delta_mode(&snap.params).unwrap_or_default();
    let muted = cx.theme().muted_foreground;
    let mut buttons = h_flex().gap_2();
    for m in VolumeDeltaMode::ALL {
        let target = target.clone();
        let mode = *m;
        let is_active = mode == current;
        let btn_id = SharedString::from(format!("vd-mode-{}-{}", id, mode.label()));
        let mut btn = Button::new(btn_id)
            .label(SharedString::from(mode.label()))
            .small();
        btn = if is_active {
            btn.primary()
        } else {
            btn.ghost()
        };
        btn = btn.on_click(cx.listener(move |_this, _ev, _w, cx| {
            let Some(panel) = target.upgrade() else {
                return;
            };
            panel.update(cx, |p, cx| {
                if let Some(chart) = p.chart_state.as_mut() {
                    chart.update_indicator(id, |kind| {
                        mutate::<VolumeDeltaParams>(kind, |x| x.mode = mode);
                    });
                    cx.notify();
                    crate::panels::request_layout_save(cx);
                }
            });
        }));
        buttons = buttons.child(btn);
    }
    v_flex()
        .gap_2()
        .child(label_row("Mode"))
        .child(buttons)
        .child(
            div()
                .text_xs()
                .text_color(muted)
                .child("Delta = 2 \u{00d7} taker_buy_vol \u{2212} volume"),
        )
        .into_any_element()
}

fn read_volume_delta_mode(params: &serde_json::Value) -> Option<VolumeDeltaMode> {
    let s = params.pointer("/mode")?.as_str()?;
    Some(match s {
        "Histogram" => VolumeDeltaMode::Histogram,
        "Cvd" => VolumeDeltaMode::Cvd,
        _ => return None,
    })
}

/// Bar Stats form: a single Grading selector (Off / Per-bar / Visible range
/// / Daily). No placement toggle — kind is PaneOnly. No color slot —
/// bull/bear come from the theme.
fn render_bar_stat(
    snap: &InstanceSnapshot,
    target: WeakEntity<ContentPanel>,
    id: InstanceId,
    cx: &mut Context<IndicatorSettingsView>,
) -> gpui::AnyElement {
    let current = read_bar_stat_grade(&snap.params).unwrap_or_default();
    let muted = cx.theme().muted_foreground;
    let mut buttons = h_flex().gap_2();
    for g in BarStatGrade::ALL {
        let target = target.clone();
        let grade = *g;
        let is_active = grade == current;
        let btn_id = SharedString::from(format!("bs-grade-{}-{}", id, grade.label()));
        let mut btn = Button::new(btn_id)
            .label(SharedString::from(grade.label()))
            .small();
        btn = if is_active {
            btn.primary()
        } else {
            btn.ghost()
        };
        btn = btn.on_click(cx.listener(move |_this, _ev, _w, cx| {
            let Some(panel) = target.upgrade() else {
                return;
            };
            panel.update(cx, |p, cx| {
                if let Some(chart) = p.chart_state.as_mut() {
                    chart.update_indicator(id, |kind| {
                        mutate::<BarStatParams>(kind, |x| x.grade = grade);
                    });
                    cx.notify();
                    crate::panels::request_layout_save(cx);
                }
            });
        }));
        buttons = buttons.child(btn);
    }
    v_flex()
        .gap_2()
        .child(label_row("Color grading"))
        .child(buttons)
        .child(div().text_xs().text_color(muted).child(
            "Top row: bar volume. Bottom row: signed delta. Grading scales the cell tint by \
                    visible-range or trailing-24h max.",
        ))
        .into_any_element()
}

fn read_bar_stat_grade(params: &serde_json::Value) -> Option<BarStatGrade> {
    let s = params.pointer("/grade")?.as_str()?;
    Some(match s {
        "Off" => BarStatGrade::Off,
        "Bar" => BarStatGrade::Bar,
        "VisibleRange" => BarStatGrade::VisibleRange,
        "Daily" => BarStatGrade::Daily,
        _ => return None,
    })
}

// ────────────────────────────── form widgets ──────────────────────────────

fn label_row(text: &'static str) -> impl IntoElement {
    div().text_sm().child(SharedString::from(text))
}

/// One color slot: `[label]   [ColorPicker swatch trigger]`. The picker is
/// the full gpui-component widget — clicking the trigger opens a popover
/// with the featured palette + HSLA sliders + hex input.
fn color_row(
    label: SharedString,
    state: &Entity<ColorPickerState>,
    cx: &mut Context<IndicatorSettingsView>,
) -> gpui::AnyElement {
    let muted = cx.theme().muted_foreground;
    let featured = featured_palette();
    h_flex()
        .gap_3()
        .items_center()
        .child(div().w(px(90.)).text_sm().text_color(muted).child(label))
        .child(ColorPicker::new(state).small().featured_colors(featured))
        .into_any_element()
}

/// `[label]    [-] [value] [+]` row for an integer field.
fn period_row(
    label: &'static str,
    value: usize,
    target: WeakEntity<ContentPanel>,
    id: InstanceId,
    mutate_fn: fn(&mut Box<dyn crate::indicators::IndicatorKind>, i32),
    cx: &mut Context<IndicatorSettingsView>,
) -> gpui::AnyElement {
    stepper_row(label, format!("{}", value), target, id, mutate_fn, cx)
}

/// `[label]    [-] [value] [+]` row for a float field, with caller-formatted
/// readout.
fn float_row(
    label: &'static str,
    readout: String,
    target: WeakEntity<ContentPanel>,
    id: InstanceId,
    mutate_fn: fn(&mut Box<dyn crate::indicators::IndicatorKind>, i32),
    cx: &mut Context<IndicatorSettingsView>,
) -> gpui::AnyElement {
    stepper_row(label, readout, target, id, mutate_fn, cx)
}

fn stepper_row(
    label: &'static str,
    readout: String,
    target: WeakEntity<ContentPanel>,
    id: InstanceId,
    mutate_fn: fn(&mut Box<dyn crate::indicators::IndicatorKind>, i32),
    cx: &mut Context<IndicatorSettingsView>,
) -> gpui::AnyElement {
    let muted = cx.theme().muted_foreground;
    let row_id = SharedString::from(format!("stepper-{}-{}", id, label));
    let target_dec = target.clone();
    let target_inc = target.clone();
    h_flex()
        .id(row_id)
        .gap_3()
        .items_center()
        .child(div().w(px(90.)).text_sm().text_color(muted).child(label))
        .child(
            Button::new(SharedString::from(format!("dec-{}-{}", id, label)))
                .label(SharedString::from("\u{2212}"))
                .xsmall()
                .ghost()
                .on_click(cx.listener(move |_this, _ev, _w, cx| {
                    apply_mutation(&target_dec, id, -1, mutate_fn, cx);
                })),
        )
        .child(
            div()
                .min_w(px(48.))
                .text_sm()
                .child(SharedString::from(readout)),
        )
        .child(
            Button::new(SharedString::from(format!("inc-{}-{}", id, label)))
                .label(SharedString::from("+"))
                .xsmall()
                .ghost()
                .on_click(cx.listener(move |_this, _ev, _w, cx| {
                    apply_mutation(&target_inc, id, 1, mutate_fn, cx);
                })),
        )
        .into_any_element()
}

fn source_row(
    label: &'static str,
    current: Source,
    target: WeakEntity<ContentPanel>,
    id: InstanceId,
    mutate_fn: fn(&mut Box<dyn crate::indicators::IndicatorKind>, Source),
    cx: &mut Context<IndicatorSettingsView>,
) -> gpui::AnyElement {
    // Row of six small toggle buttons, one per Source. Simpler than a
    // popup-menu dropdown (which expects `Box<dyn Action>` items and
    // can't take a closure-per-item), and keeps the keyboard-free
    // click model consistent with the steppers above.
    let muted = cx.theme().muted_foreground;
    let mut buttons = h_flex().gap_1();
    for s in Source::ALL {
        let target = target.clone();
        let src = *s;
        let btn_id = SharedString::from(format!("source-{}-{}", id, s.label()));
        let is_active = src == current;
        let mut btn = Button::new(btn_id)
            .label(SharedString::from(s.label()))
            .xsmall();
        btn = if is_active {
            btn.primary()
        } else {
            btn.ghost()
        };
        btn = btn.on_click(cx.listener(move |_this, _ev, _w, cx| {
            let Some(panel) = target.upgrade() else {
                return;
            };
            panel.update(cx, |p, cx| {
                if let Some(chart) = p.chart_state.as_mut() {
                    chart.update_indicator(id, |kind| mutate_fn(kind, src));
                    cx.notify();
                    crate::panels::request_layout_save(cx);
                }
            });
        }));
        buttons = buttons.child(btn);
    }
    h_flex()
        .gap_3()
        .items_center()
        .child(div().w(px(90.)).text_sm().text_color(muted).child(label))
        .child(buttons)
        .into_any_element()
}

fn placement_btn(
    label: &'static str,
    active: bool,
    target: WeakEntity<ContentPanel>,
    id: InstanceId,
    placement: Placement,
    cx: &mut Context<IndicatorSettingsView>,
) -> impl IntoElement {
    let btn_id = SharedString::from(format!("placement-{}-{}", id, label));
    let target_click = target.clone();
    let mut btn = Button::new(btn_id).label(SharedString::from(label)).small();
    btn = if active { btn.primary() } else { btn.ghost() };
    btn.on_click(cx.listener(move |_this, _ev, _w, cx| {
        let Some(panel) = target_click.upgrade() else {
            return;
        };
        panel.update(cx, |p, cx| {
            if let Some(chart) = p.chart_state.as_mut() {
                chart.set_indicator_placement(id, placement);
                cx.notify();
                crate::panels::request_layout_save(cx);
            }
        });
    }))
}

// ────────────────────────────── helpers ──────────────────────────────

/// Featured-colors set passed to every ColorPicker — the 8 auto-rotation
/// palette slots, in their canonical order. These show up as a quick-pick
/// row above the picker's sliders.
fn featured_palette() -> Vec<Hsla> {
    (0..COLOR_PALETTE_SIZE).map(palette_color_for).collect()
}

/// Snapshot the per-slot colors for an instance. The returned Vec is
/// sized to the kind's `color_slots().len()`. Empty if the panel or
/// instance has gone away (the caller — `rebuild_color_states` — then
/// allocates zero picker states, which matches Volume's no-color setup).
fn lookup_slot_colors(target: &WeakEntity<ContentPanel>, id: InstanceId, cx: &App) -> Vec<Hsla> {
    let Some(panel) = target.upgrade() else {
        return Vec::new();
    };
    let p = panel.read(cx);
    let Some(chart) = p.chart_state.as_ref() else {
        return Vec::new();
    };
    let Some(inst) = chart.indicators().iter().find(|i| i.id == id) else {
        return Vec::new();
    };
    inst.colors.clone()
}

/// Apply a slot color to the currently-targeted instance. Called from
/// the per-slot ColorPicker subscription closure when the user picks a
/// new value — `slot` is captured at closure build time.
fn apply_slot_color(
    this: &mut IndicatorSettingsView,
    slot: usize,
    color: Hsla,
    cx: &mut Context<IndicatorSettingsView>,
) {
    let id = this.instance_id;
    let Some(panel) = this.target.upgrade() else {
        return;
    };
    panel.update(cx, |p, cx| {
        if let Some(chart) = p.chart_state.as_mut() {
            chart.set_indicator_color(id, slot, color);
            cx.notify();
            crate::panels::request_layout_save(cx);
        }
    });
}

fn step_period(current: usize, delta: i32) -> usize {
    ((current as i32 + delta).max(2)) as usize
}

/// Read the `source` field out of a params JSON view. Avoids a per-kind
/// downcast for the read path; we only downcast on the write path inside
/// the mutation closures.
fn read_source(params: &serde_json::Value) -> Option<Source> {
    let s = params.pointer("/source")?.as_str()?;
    Some(match s {
        "Close" => Source::Close,
        "Open" => Source::Open,
        "High" => Source::High,
        "Low" => Source::Low,
        "Hl2" => Source::Hl2,
        "Ohlc4" => Source::Ohlc4,
        _ => return None,
    })
}

/// Downcast the dynamic `kind` to a concrete params struct and apply a
/// Liquidation-bars form: scale mode (Auto / Fixed) + cumulative-line
/// toggle. Custom long/short color pickers + the Fixed cap input land in a
/// later polish pass; for now Fixed without a positive cap silently falls
/// back to Auto via the `y_range` guard.
fn render_liq_bars(
    snap: &InstanceSnapshot,
    target: WeakEntity<ContentPanel>,
    id: InstanceId,
    cx: &mut Context<IndicatorSettingsView>,
) -> gpui::AnyElement {
    let muted = cx.theme().muted_foreground;
    let scale_is_fixed = snap
        .params
        .pointer("/scale")
        .and_then(|v| v.get("Fixed"))
        .is_some();
    let show_cum = snap
        .params
        .pointer("/show_cumulative")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let auto_btn = {
        let target = target.clone();
        let mut b = Button::new(SharedString::from(format!("liqbars-scale-auto-{}", id)))
            .label(SharedString::from("Auto"))
            .small();
        b = if !scale_is_fixed { b.primary() } else { b.ghost() };
        b.on_click(cx.listener(move |_this, _ev, _w, cx| {
            let Some(panel) = target.upgrade() else { return; };
            panel.update(cx, |p, cx| {
                if let Some(chart) = p.chart_state.as_mut() {
                    chart.update_indicator(id, |kind| {
                        mutate::<crate::indicators::LiquidationBarsParams>(kind, |x| {
                            x.scale = crate::indicators::LiqBarsScale::Auto;
                        });
                    });
                    cx.notify();
                    crate::panels::request_layout_save(cx);
                }
            });
        }))
    };
    let fixed_btn = {
        let target = target.clone();
        let mut b = Button::new(SharedString::from(format!("liqbars-scale-fixed-{}", id)))
            .label(SharedString::from("Fixed"))
            .small();
        b = if scale_is_fixed { b.primary() } else { b.ghost() };
        b.on_click(cx.listener(move |_this, _ev, _w, cx| {
            let Some(panel) = target.upgrade() else { return; };
            panel.update(cx, |p, cx| {
                if let Some(chart) = p.chart_state.as_mut() {
                    chart.update_indicator(id, |kind| {
                        mutate::<crate::indicators::LiquidationBarsParams>(kind, |x| {
                            // Seed cap from the current y-range if it's still
                            // the default 0.0; otherwise keep whatever the
                            // user already set.
                            let prev_cap = match x.scale {
                                crate::indicators::LiqBarsScale::Fixed { max } => max,
                                _ => 0.0,
                            };
                            x.scale = crate::indicators::LiqBarsScale::Fixed { max: prev_cap };
                        });
                    });
                    cx.notify();
                    crate::panels::request_layout_save(cx);
                }
            });
        }))
    };

    let cum_btn = {
        let target = target.clone();
        let label = if show_cum { "Cumulative: ON" } else { "Cumulative: OFF" };
        let mut b = Button::new(SharedString::from(format!("liqbars-cum-{}", id)))
            .label(SharedString::from(label))
            .small();
        b = if show_cum { b.primary() } else { b.ghost() };
        b.on_click(cx.listener(move |_this, _ev, _w, cx| {
            let Some(panel) = target.upgrade() else { return; };
            panel.update(cx, |p, cx| {
                if let Some(chart) = p.chart_state.as_mut() {
                    chart.update_indicator(id, |kind| {
                        mutate::<crate::indicators::LiquidationBarsParams>(kind, |x| {
                            x.show_cumulative = !x.show_cumulative;
                        });
                    });
                    cx.notify();
                    crate::panels::request_layout_save(cx);
                }
            });
        }))
    };

    v_flex()
        .gap_2()
        .child(label_row("Scale"))
        .child(h_flex().gap_2().child(auto_btn).child(fixed_btn))
        .child(label_row("Overlays"))
        .child(h_flex().gap_2().child(cum_btn))
        .child(
            div()
                .text_color(muted)
                .text_size(px(11.))
                .child(
                    "Auto fits to the visible bars; Fixed locks a symmetric \
                     y-range. Cumulative draws a running net line (short USD \
                     − long USD across visible bars). Coin/USD axis follows \
                     the chart's volume-unit toggle.",
                ),
        )
        .into_any_element()
}

/// mutation. No-op if the downcast fails (kind id and concrete type don't
/// match — should never happen unless persistence loaded the wrong type).
fn mutate<T: 'static>(
    kind: &mut Box<dyn crate::indicators::IndicatorKind>,
    f: impl FnOnce(&mut T),
) {
    if let Some(p) = kind.as_any_mut().downcast_mut::<T>() {
        f(p);
    }
}

/// Step-mutation helper used by every stepper click. Looks up the chart
/// panel, finds the instance, applies the closure, and triggers a recompute
/// via `update_indicator`.
fn apply_mutation(
    target: &WeakEntity<ContentPanel>,
    id: InstanceId,
    delta: i32,
    mutate_fn: fn(&mut Box<dyn crate::indicators::IndicatorKind>, i32),
    cx: &mut Context<IndicatorSettingsView>,
) {
    let Some(panel) = target.upgrade() else {
        return;
    };
    panel.update(cx, |p, cx| {
        if let Some(chart) = p.chart_state.as_mut() {
            chart.update_indicator(id, |kind| mutate_fn(kind, delta));
        }
        // VRVP edits can shift the bucket-sub set (bucket size stepper);
        // refresh is idempotent for non-VRVP / same-bucket edits so call
        // unconditionally rather than special-casing the kind.
        p.refresh_chart_footprint_sub(cx);
        cx.notify();
        crate::panels::request_layout_save(cx);
    });
}
