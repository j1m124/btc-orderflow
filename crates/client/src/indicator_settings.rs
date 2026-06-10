//! Floating settings panel for an attached indicator. Workspace owns one
//! singleton; clicking the gear on a chip retargets the existing window to
//! the new instance and the form re-renders. The window itself is the
//! reusable `FloatingWindow` wrapper (drag title bar, corner resize, X to
//! close).
//!
//! The form body comes from `IndicatorKind::settings_form(panel, id)` —
//! each kind owns its own declarative form, rendered by the standardized
//! `settings_form` framework. This view is a thin dispatcher: look up the
//! live instance, ask the kind for its form, render.

use gpui::{
    Action, App, Context, FocusHandle, Focusable, Hsla, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, StatefulInteractiveElement as _, Styled as _,
    WeakEntity, Window, div, px,
};
use gpui_component::{ActiveTheme as _, v_flex};
use serde::Deserialize;

use crate::indicators::InstanceId;
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
}

impl IndicatorSettingsView {
    pub fn new(
        target: WeakEntity<ContentPanel>,
        instance_id: InstanceId,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            target,
            instance_id,
            focus: cx.focus_handle(),
        }
    }

    /// Retarget when the user clicks a different chip while the window is
    /// already open.
    pub fn retarget(
        &mut self,
        target: WeakEntity<ContentPanel>,
        instance_id: InstanceId,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.target = target;
        self.instance_id = instance_id;
        cx.notify();
    }

    pub fn current_target(&self) -> &WeakEntity<ContentPanel> {
        &self.target
    }

    pub fn current_instance_id(&self) -> InstanceId {
        self.instance_id
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

        let Some(panel_e) = target.upgrade() else {
            return missing_body("Indicator no longer available", muted).into_any_element();
        };
        let (label, form_opt) = {
            let panel = panel_e.read(cx);
            let Some(chart) = panel.chart_state.as_ref() else {
                return missing_body("Not a chart panel", muted).into_any_element();
            };
            let Some(inst) = chart.indicators().iter().find(|i| i.id == id) else {
                return missing_body("Indicator was removed", muted).into_any_element();
            };
            (inst.kind.label(), inst.kind.settings_form(target.clone(), id))
        };

        let Some(form) = form_opt else {
            return missing_body("This indicator has no settings", muted).into_any_element();
        };
        let kind_body = form.render(window, cx);

        let body = v_flex()
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

fn missing_body(msg: &'static str, muted: Hsla) -> impl IntoElement {
    div()
        .p_4()
        .text_sm()
        .text_color(muted)
        .child(SharedString::from(msg))
}
