//! Adaptive controls rendered inside the floating settings strip when a
//! drawing is selected. Owns the per-control listeners and reads the
//! currently-selected drawing from [`super::service::DrawingService`].
//!
//! Layout: `[<shape label>] [color | width | label] [gear] [visible | lock | trash]`
//!
//! Phase 4 wires the universal cluster (visible / lock / trash). Quick
//! controls (color / width / label) + gear land in Phase 5.

use gpui::{
    AnyElement, AppContext as _, Context, Entity, EventEmitter, InteractiveElement as _,
    IntoElement, MouseButton, ParentElement as _, Render, SharedString, Styled as _, Window, div,
    px,
};
use gpui_component::{
    ActiveTheme as _, Disableable as _, IconName, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
};

use super::service::{DrawingId, DrawingServiceHandle};
use super::shapes::DrawingShape;

#[derive(Clone, Debug)]
pub enum StripContentEvent {
    /// User clicked the gear button — workspace opens the per-shape
    /// settings window.
    GearClicked,
}

pub struct DrawingStripContent;

impl DrawingStripContent {
    pub fn new(_cx: &mut Context<Self>) -> Self {
        Self
    }
}

impl EventEmitter<StripContentEvent> for DrawingStripContent {}

impl Render for DrawingStripContent {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(handle) = cx.try_global::<DrawingServiceHandle>().cloned() else {
            return placeholder();
        };
        let svc = handle.0.read(cx);
        let Some((symbol, drawing)) = svc.selected_drawing() else {
            return placeholder();
        };
        let symbol = symbol.clone();
        let id: DrawingId = drawing.id;
        let hidden = drawing.hidden;
        let locked = drawing.locked;
        let shape_label: SharedString = shape_label_for(&drawing.shape).into();
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let fg = theme.foreground;
        let separator_color = theme.border;

        let visible_btn = Button::new(("strip-visible", id as usize))
            .ghost()
            .small()
            .icon(if hidden {
                IconName::EyeOff
            } else {
                IconName::Eye
            })
            .tooltip(if hidden { "Show" } else { "Hide" })
            .on_click({
                let symbol = symbol.clone();
                move |_, _, cx| {
                    let Some(handle) = cx.try_global::<DrawingServiceHandle>().cloned() else {
                        return;
                    };
                    handle.0.update(cx, |s, cx| {
                        s.toggle_hidden(symbol.as_ref(), id, cx);
                    });
                }
            });

        // No lock icon in the asset set — use a short text label that flips
        // between "Lock" and "Locked" so the state is unambiguous at a glance.
        let lock_btn = Button::new(("strip-lock", id as usize))
            .ghost()
            .small()
            .label(if locked { "Locked" } else { "Lock" })
            .tooltip(if locked { "Unlock" } else { "Lock" })
            .on_click({
                let symbol = symbol.clone();
                move |_, _, cx| {
                    let Some(handle) = cx.try_global::<DrawingServiceHandle>().cloned() else {
                        return;
                    };
                    handle.0.update(cx, |s, cx| {
                        s.set_locked(symbol.as_ref(), id, !locked, cx);
                    });
                }
            });

        let mut trash_btn = Button::new(("strip-trash", id as usize))
            .ghost()
            .small()
            .icon(IconName::Delete)
            .tooltip(if locked {
                "Unlock to delete"
            } else {
                "Delete"
            });
        if locked {
            trash_btn = trash_btn.disabled(true);
        } else {
            let symbol_for_trash = symbol.clone();
            trash_btn = trash_btn.on_click(move |_, _, cx| {
                let Some(handle) = cx.try_global::<DrawingServiceHandle>().cloned() else {
                    return;
                };
                handle.0.update(cx, |s, cx| {
                    s.delete(symbol_for_trash.as_ref(), id, cx);
                });
            });
        }

        let row = h_flex()
            .px_2()
            .h_full()
            .items_center()
            .gap_1()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .text_color(fg)
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(muted)
                    .mr_2()
                    .child(shape_label),
            )
            .child(div().w(px(1.)).h(px(20.)).mx_1().bg(separator_color))
            .child(visible_btn)
            .child(lock_btn)
            .child(trash_btn);

        row.into_any_element()
    }
}

/// Render an empty placeholder when no selection — keeps the strip's
/// content view a valid `AnyElement` without flickering visibility.
fn placeholder() -> AnyElement {
    div().h(px(0.)).w(px(0.)).into_any_element()
}

fn shape_label_for(shape: &DrawingShape) -> &'static str {
    match shape {
        DrawingShape::Line(_) => "Line",
        DrawingShape::Rect(_) => "Rect",
        DrawingShape::Arrow(_) => "Arrow",
        DrawingShape::Fibonacci(_) => "Fib",
        DrawingShape::HorizontalRay(_) => "Ray",
        DrawingShape::AnchoredVwap(_) => "AVWAP",
        DrawingShape::Text(_) => "Text",
        DrawingShape::Long(_) => "Long",
        DrawingShape::Short(_) => "Short",
    }
}

/// Workspace constructor — spawn a fresh content entity.
pub fn build(cx: &mut gpui::App) -> Entity<DrawingStripContent> {
    cx.new(DrawingStripContent::new)
}

/// Convenience re-export so callers can refer to the drawing-id by name.
pub type SelectionId = DrawingId;
