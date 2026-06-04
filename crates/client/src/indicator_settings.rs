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
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString, Styled as _,
    Subscription, WeakEntity, Window, div, px,
};
use gpui_component::{
    ActiveTheme as _, Sizable as _,
    button::{Button, ButtonVariants as _},
    color_picker::{ColorPicker, ColorPickerEvent, ColorPickerState},
    h_flex, v_flex,
};
use serde::Deserialize;

use crate::indicators::{
    BbParams, COLOR_PALETTE_SIZE, InstanceId, Placement, Source, palette_color_for,
};
use crate::panels::ContentPanel;

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

        let mut root = v_flex()
            .id(SharedString::from(format!("indicator-settings-{}", id)))
            .size_full()
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
            root = root
                .child(div().h(px(1.)).bg(cx.theme().border))
                .child(v_flex().gap_2().children(color_rows));
        }
        root.into_any_element()
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
    let p = snap.params.pointer("/period").and_then(|v| v.as_u64()).unwrap_or(20) as usize;
    let sd = snap.params.pointer("/stddev").and_then(|v| v.as_f64()).unwrap_or(2.0);
    let s = read_source(&snap.params).unwrap_or(Source::Close);
    v_flex()
        .gap_2()
        .child(period_row("Period", p, target.clone(), id, |kind, delta| {
            mutate::<BbParams>(kind, |x| {
                x.period = step_period(x.period, delta);
            });
        }, cx))
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
        let mut btn = Button::new(btn_id).label(SharedString::from(s.label())).xsmall();
        btn = if is_active { btn.primary() } else { btn.ghost() };
        btn = btn.on_click(cx.listener(move |_this, _ev, _w, cx| {
            let Some(panel) = target.upgrade() else {
                return;
            };
            panel.update(cx, |p, cx| {
                if let Some(chart) = p.chart_state.as_mut() {
                    chart.update_indicator(id, |kind| mutate_fn(kind, src));
                    cx.notify();
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
fn lookup_slot_colors(
    target: &WeakEntity<ContentPanel>,
    id: InstanceId,
    cx: &App,
) -> Vec<Hsla> {
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
/// mutation. No-op if the downcast fails (kind id and concrete type don't
/// match — should never happen unless persistence loaded the wrong type).
fn mutate<T: 'static>(kind: &mut Box<dyn crate::indicators::IndicatorKind>, f: impl FnOnce(&mut T)) {
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
            cx.notify();
        }
    });
}
