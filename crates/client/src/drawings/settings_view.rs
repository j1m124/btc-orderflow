//! Floating per-drawing settings window. Mounted by the workspace when
//! the strip's gear button fires. Exposes the dials that wouldn't fit
//! comfortably on the strip itself — primarily the per-timeframe
//! visibility filter, which today only surfaces through the chart's
//! right-click submenu.
//!
//! Singleton: a second dispatch with a different `(symbol, id)`
//! retargets the existing view (no second window). View re-reads the
//! service on every render so external mutations (toggling via the
//! right-click menu, deleting from the strip, etc.) reflect live.

use std::collections::BTreeSet;

use gpui::{
    App, Context, FocusHandle, Focusable, Hsla, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, Styled as _, Window, div, px,
};
use gpui_component::{
    ActiveTheme as _, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex, v_flex,
};

use crate::services::market_data::Timeframe;

use super::actions::{ResetDrawingTfFilter, ToggleDrawingHidden, ToggleDrawingTfFilter};
use super::service::{DrawingId, DrawingServiceHandle};
use super::shapes::{Drawing, DrawingOrigin};

/// Per-drawing settings window content. Owns nothing the strip already
/// owns — colour / width / label / lock / delete live there. This view
/// is for the lower-frequency settings that benefit from a roomier UI.
pub struct DrawingSettingsView {
    target: (SharedString, DrawingId),
    focus: FocusHandle,
}

impl DrawingSettingsView {
    pub fn new(
        symbol: SharedString,
        id: DrawingId,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            target: (symbol, id),
            focus: cx.focus_handle(),
        }
    }

    /// Re-point the window at a different drawing. Used when the user
    /// selects another drawing and clicks its gear — keeps a single
    /// window instead of a stack.
    pub fn retarget(
        &mut self,
        symbol: SharedString,
        id: DrawingId,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.target = (symbol, id);
        cx.notify();
    }

    pub fn current_target(&self) -> &(SharedString, DrawingId) {
        &self.target
    }
}

impl Focusable for DrawingSettingsView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for DrawingSettingsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let border = cx.theme().border;
        let (symbol, id) = self.target.clone();
        let Some(handle) = cx.try_global::<DrawingServiceHandle>().cloned() else {
            return missing_body("Drawing service unavailable", muted).into_any_element();
        };
        // Snapshot what the view actually needs out of the borrow.
        let snapshot = {
            let svc = handle.0.read(cx);
            svc.for_symbol(symbol.as_ref())
                .iter()
                .find(|d| d.id == id)
                .map(|d| DrawingSnapshot::from(d.as_ref_proxy()))
        };
        let Some(snap) = snapshot else {
            return missing_body("Drawing was removed", muted).into_any_element();
        };

        let header_label: SharedString = SharedString::from(snap.label.clone());
        let origin_text = match snap.origin {
            DrawingOrigin::User => "User-drawn",
            DrawingOrigin::Ai => "AI-drawn",
        };

        let mut root = v_flex()
            .id(SharedString::from(format!("drawing-settings-{}", id)))
            .size_full()
            .p_4()
            .gap_4()
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().foreground)
                            .child(header_label),
                    )
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(muted)
                            .child(SharedString::from(origin_text)),
                    ),
            )
            .child(div().h(px(1.)).bg(border));

        // ── Visibility row: master hide/show toggle ─────────────────────
        let symbol_for_hide = symbol.clone();
        let hide_label: SharedString = if snap.hidden { "Show drawing" } else { "Hide drawing" }.into();
        root = root.child(
            v_flex()
                .gap_2()
                .child(section_label("Visibility", muted))
                .child(
                    Button::new(("drawing-settings-toggle-hidden", id as usize))
                        .small()
                        .ghost()
                        .label(hide_label)
                        .on_click(move |_, window, cx| {
                            window.dispatch_action(
                                Box::new(ToggleDrawingHidden {
                                    symbol: symbol_for_hide.clone(),
                                    id,
                                }),
                                cx,
                            );
                        }),
                ),
        );

        // ── Visible-on row: one chip per timeframe ──────────────────────
        let tf_filter = snap.tf_filter.clone();
        let symbol_for_tf = symbol.clone();
        let mut tf_chips = h_flex().gap_1().flex_wrap();
        for tf in Timeframe::ALL {
            let tf_str = tf.as_str();
            let active = match &tf_filter {
                None => true,
                Some(set) => set.contains(tf_str),
            };
            let sym_for_chip = symbol_for_tf.clone();
            let mut chip = Button::new(SharedString::from(format!(
                "drawing-settings-tf-{}-{}",
                id, tf_str
            )))
            .xsmall()
            .label(SharedString::from(tf_str));
            chip = if active { chip.primary() } else { chip.ghost() };
            chip = chip.on_click(move |_, window, cx| {
                window.dispatch_action(
                    Box::new(ToggleDrawingTfFilter {
                        symbol: sym_for_chip.clone(),
                        id,
                        tf: SharedString::from(tf_str),
                    }),
                    cx,
                );
            });
            tf_chips = tf_chips.child(chip);
        }
        let mut tf_section = v_flex()
            .gap_2()
            .child(section_label("Visible on", muted))
            .child(tf_chips);
        if tf_filter.is_some() {
            let sym_for_reset = symbol.clone();
            tf_section = tf_section.child(
                Button::new(("drawing-settings-tf-reset", id as usize))
                    .xsmall()
                    .ghost()
                    .label("Reset to all timeframes")
                    .on_click(move |_, window, cx| {
                        window.dispatch_action(
                            Box::new(ResetDrawingTfFilter {
                                symbol: sym_for_reset.clone(),
                                id,
                            }),
                            cx,
                        );
                    }),
            );
        }
        root = root.child(tf_section);

        root.into_any_element()
    }
}

fn section_label(text: &'static str, color: Hsla) -> impl IntoElement {
    div()
        .text_size(px(11.))
        .text_color(color)
        .child(SharedString::from(text))
}

fn missing_body(msg: &'static str, color: Hsla) -> impl IntoElement {
    div()
        .p_4()
        .text_sm()
        .text_color(color)
        .child(SharedString::from(msg))
}

/// View-side snapshot of the bits of a [`Drawing`] this window reads.
/// Keeping it Clone-only avoids holding the service borrow across the
/// rest of render.
struct DrawingSnapshot {
    label: String,
    hidden: bool,
    tf_filter: Option<BTreeSet<String>>,
    origin: DrawingOrigin,
}

trait DrawingRefProxy {
    fn as_ref_proxy(&self) -> &Drawing;
}

impl DrawingRefProxy for &Drawing {
    fn as_ref_proxy(&self) -> &Drawing {
        self
    }
}

impl From<&Drawing> for DrawingSnapshot {
    fn from(d: &Drawing) -> Self {
        Self {
            label: d.label(),
            hidden: d.hidden,
            tf_filter: d.tf_filter.clone(),
            origin: d.created_by,
        }
    }
}
