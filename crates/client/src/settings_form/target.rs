//! Target adapter for indicator forms. Wraps a `WeakEntity<ContentPanel>` +
//! an `InstanceId` and emits getter / setter closures keyed to a specific
//! indicator kind type `P`. The setter closure runs the canonical
//! post-mutation pipeline — `chart.update_indicator(id, |kind| f(typed))`
//! plus `cx.notify()` + `request_layout_save(cx)` plus per-form
//! `refresh_chart_footprint_sub` / `refresh_chart_liq_bars_sub` hooks.
//!
//! Per-kind `settings_form()` code uses
//! `target.typed(get, set)` to convert typed `(&P) -> T` / `(&mut P, T)`
//! closures into the App-keyed pairs the `Field` constructors expect.

use std::marker::PhantomData;

use gpui::{App, Entity, WeakEntity};

use crate::indicators::{IndicatorKind, InstanceId, Placement, palette_color_for};
use crate::panels::ContentPanel;

use super::field::{DropdownOption, Field};

/// Per-form refresh flags. Tells the target which `ContentPanel` re-sub
/// helpers to call after every mutation. Picking the right set per kind
/// avoids the previous "refresh footprint sub on every edit" overshoot.
#[derive(Clone, Copy, Default)]
pub struct AfterChange {
    pub refresh_footprint: bool,
    pub refresh_liq_bars: bool,
    pub refresh_oi_bars: bool,
}

impl AfterChange {
    pub const fn none() -> Self {
        Self {
            refresh_footprint: false,
            refresh_liq_bars: false,
            refresh_oi_bars: false,
        }
    }

    pub const fn footprint() -> Self {
        Self {
            refresh_footprint: true,
            refresh_liq_bars: false,
            refresh_oi_bars: false,
        }
    }

    pub const fn liq_bars() -> Self {
        Self {
            refresh_footprint: false,
            refresh_liq_bars: true,
            refresh_oi_bars: false,
        }
    }

    pub const fn oi_bars() -> Self {
        Self {
            refresh_footprint: false,
            refresh_liq_bars: false,
            refresh_oi_bars: true,
        }
    }

    /// Bar Stats toggles both liquidation rows and the OI-Δ row, each of
    /// which gates a different shared subscription — refresh both.
    pub const fn liq_and_oi_bars() -> Self {
        Self {
            refresh_footprint: false,
            refresh_liq_bars: true,
            refresh_oi_bars: true,
        }
    }
}

/// Indicator target. `P` is the concrete params type the kind exposes
/// (e.g., `BbParams`); the helpers downcast via
/// `kind.as_any_mut().downcast_mut::<P>()` and surface typed access.
pub struct IndicatorTarget<P> {
    panel: WeakEntity<ContentPanel>,
    id: InstanceId,
    after: AfterChange,
    _phantom: PhantomData<fn() -> P>,
}

impl<P> Clone for IndicatorTarget<P> {
    fn clone(&self) -> Self {
        Self {
            panel: self.panel.clone(),
            id: self.id,
            after: self.after,
            _phantom: PhantomData,
        }
    }
}

impl<P> IndicatorTarget<P>
where
    P: IndicatorKind + 'static,
{
    pub fn new(panel: WeakEntity<ContentPanel>, id: InstanceId) -> Self {
        Self {
            panel,
            id,
            after: AfterChange::default(),
            _phantom: PhantomData,
        }
    }

    pub fn with_after_change(mut self, after: AfterChange) -> Self {
        self.after = after;
        self
    }

    pub fn panel(&self) -> &WeakEntity<ContentPanel> {
        &self.panel
    }

    pub fn instance_id(&self) -> InstanceId {
        self.id
    }

    /// Read the typed params snapshot via the `get` closure. Returns `None`
    /// if the panel/chart/instance has gone away — callers (the form
    /// renderer) treat that as "skip the row".
    pub fn read<T, F>(&self, cx: &App, f: F) -> Option<T>
    where
        F: FnOnce(&P) -> T,
    {
        let panel = self.panel.upgrade()?;
        let panel = panel.read(cx);
        let chart = panel.chart_state.as_ref()?;
        let inst = chart.indicators().iter().find(|i| i.id == self.id)?;
        let typed = inst.kind.as_any().downcast_ref::<P>()?;
        Some(f(typed))
    }

    /// Apply a typed mutation to the instance's params, then run the
    /// after-change pipeline. No-op if the panel/chart/instance is gone.
    pub fn write<F>(&self, cx: &mut App, f: F)
    where
        F: FnOnce(&mut P) + 'static,
    {
        let Some(panel) = self.panel.upgrade() else {
            return;
        };
        let id = self.id;
        let after = self.after;
        let mut f_holder: Option<F> = Some(f);
        panel.update(cx, |p, cx| {
            if let Some(chart) = p.chart_state.as_mut() {
                let f = f_holder.take().expect("called once");
                chart.update_indicator(id, |kind| {
                    if let Some(typed) = kind.as_any_mut().downcast_mut::<P>() {
                        f(typed);
                    }
                });
            }
            if after.refresh_footprint {
                p.refresh_chart_footprint_sub(cx);
            }
            if after.refresh_liq_bars {
                p.refresh_chart_liq_bars_sub(cx);
            }
            if after.refresh_oi_bars {
                p.refresh_chart_oi_bars_sub(cx);
            }
            cx.notify();
            crate::panels::request_layout_save(cx);
        });
    }

    /// Build a getter closure suitable for `Field::*`. The returned closure
    /// reads the params via `read` and falls back to `default` when the
    /// target is gone (so the form keeps rendering during teardown).
    pub fn getter<T, G>(&self, default: T, get: G) -> impl Fn(&App) -> T + 'static
    where
        T: Clone + 'static,
        G: Fn(&P) -> T + 'static,
    {
        let tgt = self.clone();
        move |cx| tgt.read(cx, |p| get(p)).unwrap_or_else(|| default.clone())
    }

    /// Build a setter closure suitable for `Field::*`.
    pub fn setter<T, S>(&self, set: S) -> impl Fn(T, &mut App) + 'static
    where
        T: 'static,
        S: Fn(&mut P, T) + 'static + Clone,
    {
        let tgt = self.clone();
        move |value, cx| {
            let set = set.clone();
            tgt.write(cx, move |p| set(p, value));
        }
    }
}

// ─────────────────────────── chart-only helpers ───────────────────────────

/// Locate the `Entity<ContentPanel>` referenced by a weak target. Helper
/// for the form renderer when it needs more than read/write — e.g., to
/// drive non-params fields like the `Placement` toggle that lives on the
/// `IndicatorInstance` itself.
pub fn upgrade_panel(weak: &WeakEntity<ContentPanel>) -> Option<Entity<ContentPanel>> {
    weak.upgrade()
}

/// Build a `Placement` dropdown field that toggles between Overlay and Pane
/// for hybrid (`PaneKind::Both`) kinds. The placement lives on
/// `IndicatorInstance`, not the typed params — so we read/write via
/// `chart.set_indicator_placement` directly rather than `update_indicator`.
pub fn placement_field(panel: WeakEntity<ContentPanel>, id: InstanceId) -> Field {
    let panel_for_get = panel.clone();
    let panel_for_set = panel.clone();
    Field::dropdown(
        "Placement",
        vec![
            DropdownOption::new("overlay", "Overlay"),
            DropdownOption::new("pane", "Pane"),
        ],
        move |cx| {
            let Some(p) = panel_for_get.upgrade() else {
                return SharedString::from("overlay");
            };
            let p = p.read(cx);
            let Some(chart) = p.chart_state.as_ref() else {
                return SharedString::from("overlay");
            };
            let placement = chart
                .indicators()
                .iter()
                .find(|i| i.id == id)
                .map(|i| i.placement)
                .unwrap_or(Placement::Overlay);
            match placement {
                Placement::Overlay => SharedString::from("overlay"),
                Placement::Pane => SharedString::from("pane"),
            }
        },
        move |value, cx| {
            let Some(panel) = panel_for_set.upgrade() else {
                return;
            };
            let placement = match value.as_ref() {
                "pane" => Placement::Pane,
                _ => Placement::Overlay,
            };
            panel.update(cx, |p, cx| {
                if let Some(chart) = p.chart_state.as_mut() {
                    chart.set_indicator_placement(id, placement);
                }
                cx.notify();
                crate::panels::request_layout_save(cx);
            });
        },
    )
}

use gpui::{Hsla, SharedString};

/// Color field targeting `IndicatorInstance.colors[slot]` directly. Phase 7+
/// declares each kind's color slots inline in its own `settings_form()`
/// declaration; Phase 10 then deletes the generic color section + the
/// `color_slots()` trait method.
pub fn inst_color_field(
    label: impl Into<SharedString>,
    panel: WeakEntity<ContentPanel>,
    id: InstanceId,
    slot: usize,
) -> Field {
    let panel_for_get = panel.clone();
    let panel_for_set = panel.clone();
    Field::color(
        label,
        move |cx| {
            let Some(p) = panel_for_get.upgrade() else {
                return palette_color_for(slot);
            };
            let p = p.read(cx);
            let Some(chart) = p.chart_state.as_ref() else {
                return palette_color_for(slot);
            };
            chart
                .indicators()
                .iter()
                .find(|i| i.id == id)
                .and_then(|i| i.colors.get(slot).copied())
                .unwrap_or_else(|| palette_color_for(slot))
        },
        move |color: Hsla, cx| {
            let Some(panel) = panel_for_set.upgrade() else {
                return;
            };
            panel.update(cx, |p, cx| {
                if let Some(chart) = p.chart_state.as_mut() {
                    chart.set_indicator_color(id, slot, color);
                }
                cx.notify();
                crate::panels::request_layout_save(cx);
            });
        },
    )
}
