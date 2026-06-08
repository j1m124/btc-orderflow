//! Adaptive controls rendered inside the floating settings strip when a
//! drawing is selected. Owns the per-control listeners and reads the
//! currently-selected drawing from [`super::service::DrawingService`].
//!
//! Layout: `[<shape label>] [color | width | label] [gear] [visible | lock | trash]`
//!
//! Phase 5 wires color (1 swatch, or 2 for position shapes), width
//! popover (1/2/3/4 px), label dialog button, and a gear placeholder.
//! The per-shape gear settings window itself ships in Phase 7.

use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable, Hsla,
    InteractiveElement as _, IntoElement, MouseButton, ParentElement as _, Render, SharedString,
    Styled as _, Subscription, Window, div, px,
};
use gpui_component::{
    ActiveTheme as _, Disableable as _, IconName, Sizable as _,
    button::{Button, ButtonVariants as _},
    color_picker::{ColorPicker, ColorPickerEvent, ColorPickerState},
    h_flex,
    popover::Popover,
    v_flex,
};

use super::service::{ColorRole, DrawingId, DrawingServiceHandle};
use super::shapes::{DrawingColor, DrawingShape};

const WIDTH_CHOICES: &[f32] = &[1.0, 2.0, 3.0, 4.0];

#[derive(Clone, Debug)]
pub enum StripContentEvent {
    /// User clicked the gear button — workspace opens the per-shape
    /// settings window. Wired in Phase 7; for now the workspace can
    /// ignore the event and the button is a placeholder.
    GearClicked { symbol: SharedString, id: DrawingId },
}

/// Identifies the selection the cached `color_states` were built for.
type SelectionKey = (SharedString, DrawingId);

pub struct DrawingStripContent {
    focus: FocusHandle,
    /// Selection the cached `color_states` were built for. When the
    /// rendered selection differs, the picker states + subscriptions are
    /// rebuilt from scratch.
    current_key: Option<SelectionKey>,
    /// One picker entity per active color role for the current shape.
    /// Position shapes have two entries (Profit, Loss); every other
    /// shape has one entry (Primary). Empty when nothing is selected.
    color_states: Vec<(ColorRole, Entity<ColorPickerState>)>,
    _color_subs: Vec<Subscription>,
}

impl DrawingStripContent {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            focus: cx.focus_handle(),
            current_key: None,
            color_states: Vec::new(),
            _color_subs: Vec::new(),
        }
    }

    /// (Re)build cached picker states for a new selection. Idempotent —
    /// `key` mismatch is the only trigger so re-renders driven by the
    /// service's own `Changed` events (after a picker write) don't churn.
    fn ensure_state_for(
        &mut self,
        key: SelectionKey,
        shape: &DrawingShape,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.current_key.as_ref() == Some(&key) {
            return;
        }
        self.current_key = Some(key);
        self.color_states.clear();
        self._color_subs.clear();
        for role in roles_for_shape(shape) {
            let initial = effective_color_for(shape, role, cx);
            let state = cx.new(|cx| ColorPickerState::new(window, cx).default_value(initial));
            let sub = cx.subscribe(&state, move |this, _state, ev: &ColorPickerEvent, cx| {
                let ColorPickerEvent::Change(color) = ev;
                let Some((sym, id)) = this.current_key.clone() else {
                    return;
                };
                let Some(handle) = cx.try_global::<DrawingServiceHandle>().cloned() else {
                    return;
                };
                let color_dc = color.map(DrawingColor::from_hsla);
                handle.0.update(cx, |s, cx| {
                    s.set_color(sym.as_ref(), id, role, color_dc, cx);
                });
            });
            self.color_states.push((role, state));
            self._color_subs.push(sub);
        }
    }
}

impl Focusable for DrawingStripContent {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl EventEmitter<StripContentEvent> for DrawingStripContent {}

impl Render for DrawingStripContent {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(handle) = cx.try_global::<DrawingServiceHandle>().cloned() else {
            self.current_key = None;
            self.color_states.clear();
            self._color_subs.clear();
            return placeholder();
        };
        // Snapshot everything we need from the service in one borrow so
        // we can release it before issuing the `ensure_state_for` mut call
        // (which itself reads the global through closures).
        let snapshot = {
            let svc = handle.0.read(cx);
            svc.selected_drawing().map(|(sym, d)| (sym.clone(), d.clone()))
        };
        let Some((symbol, drawing)) = snapshot else {
            self.current_key = None;
            self.color_states.clear();
            self._color_subs.clear();
            return placeholder();
        };
        let key: SelectionKey = (symbol.clone(), drawing.id);
        self.ensure_state_for(key.clone(), &drawing.shape, window, cx);

        let id = drawing.id;
        let hidden = drawing.hidden;
        let locked = drawing.locked;
        let shape_label: SharedString = shape_label_for(&drawing.shape).into();
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let fg = theme.foreground;
        let separator_color = theme.border;

        let mut row = h_flex()
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
            .child(separator(separator_color));

        // ── Quick controls: color(s) → width → label ────────────────────
        let role_count = self.color_states.len();
        for idx in 0..role_count {
            let (_role, state) = &self.color_states[idx];
            row = row.child(
                div()
                    .id(("strip-color-wrap", idx))
                    .child(ColorPicker::new(state).small().featured_colors(featured_palette(cx))),
            );
        }

        if supports_width(&drawing.shape) {
            let width = current_width(&drawing.shape);
            let symbol_for_pop = symbol.clone();
            let popover = Popover::new(("strip-width", id as usize))
                .trigger(
                    Button::new(("strip-width-btn", id as usize))
                        .ghost()
                        .small()
                        .label(format!("{}px", width as u32))
                        .tooltip("Stroke width"),
                )
                .content(move |_, _, _cx| {
                    let symbol_for_choice = symbol_for_pop.clone();
                    v_flex()
                        .p_1()
                        .gap_0p5()
                        .children(WIDTH_CHOICES.iter().map(|w| {
                            let w = *w;
                            let is_active = (w - width).abs() < f32::EPSILON;
                            let symbol_for_click = symbol_for_choice.clone();
                            let mut btn = Button::new(SharedString::from(format!(
                                "strip-width-choice-{}-{}",
                                id, w as u32
                            )))
                            .small()
                            .label(format!("{}px", w as u32));
                            btn = if is_active { btn.primary() } else { btn.ghost() };
                            btn.on_click(move |_, _, cx| {
                                let Some(handle) = cx
                                    .try_global::<DrawingServiceHandle>()
                                    .cloned()
                                else {
                                    return;
                                };
                                handle.0.update(cx, |s, cx| {
                                    s.set_width(symbol_for_click.as_ref(), id, w, cx);
                                });
                            })
                        }))
                });
            row = row.child(div().id(("strip-width-wrap", id as usize)).child(popover));
        }

        if supports_label(&drawing.shape) {
            let symbol_for_label = symbol.clone();
            let has_label = current_label(&drawing.shape).is_some();
            let btn_label: SharedString = if has_label { "Label •".into() } else { "Label".into() };
            row = row.child(
                Button::new(("strip-label", id as usize))
                    .ghost()
                    .small()
                    .label(btn_label)
                    .tooltip("Edit label")
                    .on_click(move |_, window, cx| {
                        window.dispatch_action(
                            Box::new(super::actions::EditDrawingLabel {
                                symbol: symbol_for_label.clone(),
                                id,
                            }),
                            cx,
                        );
                    }),
            );
        }

        row = row.child(separator(separator_color));

        // ── Gear button — opens a per-shape settings window (Phase 7).
        // For now the strip still surfaces it so the layout stabilizes;
        // the workspace can wire `StripContentEvent::GearClicked` later.
        let symbol_for_gear = symbol.clone();
        let gear_btn = Button::new(("strip-gear", id as usize))
            .ghost()
            .small()
            .icon(IconName::Settings2)
            .tooltip("Settings")
            .on_click(cx.listener(move |_this, _ev, _w, cx| {
                cx.emit(StripContentEvent::GearClicked {
                    symbol: symbol_for_gear.clone(),
                    id,
                });
            }));
        row = row.child(gear_btn).child(separator(separator_color));

        // ── Universal cluster: visible / lock / trash ──────────────────
        let visible_btn = Button::new(("strip-visible", id as usize))
            .ghost()
            .small()
            .icon(if hidden { IconName::EyeOff } else { IconName::Eye })
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
            .tooltip(if locked { "Unlock to delete" } else { "Delete" });
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

        row = row.child(visible_btn).child(lock_btn).child(trash_btn);
        row.into_any_element()
    }
}

/// Render an empty placeholder when no selection — keeps the strip's
/// content view a valid `AnyElement` without flickering visibility.
fn placeholder() -> AnyElement {
    div().h(px(0.)).w(px(0.)).into_any_element()
}

fn separator(color: Hsla) -> gpui::Div {
    div().w(px(1.)).h(px(20.)).mx_1().bg(color)
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

/// Which color roles a given shape exposes. Position shapes carry two
/// (profit + loss); everything else has a single `Primary`.
fn roles_for_shape(shape: &DrawingShape) -> Vec<ColorRole> {
    match shape {
        DrawingShape::Long(_) | DrawingShape::Short(_) => {
            vec![ColorRole::Profit, ColorRole::Loss]
        }
        _ => vec![ColorRole::Primary],
    }
}

/// True iff the shape paints a stroke whose width the user can change.
/// Text uses pixel-based font sizing; the strip suppresses the width
/// slot for it.
fn supports_width(shape: &DrawingShape) -> bool {
    !matches!(shape, DrawingShape::Text(_))
}

/// True iff the shape carries a separate label/text field. `Text`'s
/// own text content IS the label so we don't expose a second slot.
fn supports_label(shape: &DrawingShape) -> bool {
    !matches!(shape, DrawingShape::Text(_))
}

/// Read the current stroke width from any shape that has one. Returns
/// 1.0 for shapes without a width field (defensive — caller already
/// gates on `supports_width`).
fn current_width(shape: &DrawingShape) -> f32 {
    match shape {
        DrawingShape::Line(s)
        | DrawingShape::Rect(s)
        | DrawingShape::Arrow(s)
        | DrawingShape::Fibonacci(s) => s.width,
        DrawingShape::HorizontalRay(s) => s.width,
        DrawingShape::AnchoredVwap(s) => s.width,
        DrawingShape::Long(p) | DrawingShape::Short(p) => p.width,
        DrawingShape::Text(_) => 1.0,
    }
}

/// Pull the current secondary label out of any shape that supports one.
fn current_label(shape: &DrawingShape) -> Option<&str> {
    match shape {
        DrawingShape::Line(s)
        | DrawingShape::Rect(s)
        | DrawingShape::Arrow(s)
        | DrawingShape::Fibonacci(s) => s.label.as_deref(),
        DrawingShape::HorizontalRay(s) => s.text.as_deref(),
        DrawingShape::AnchoredVwap(s) => s.label.as_deref(),
        DrawingShape::Long(p) | DrawingShape::Short(p) => p.label.as_deref(),
        DrawingShape::Text(_) => None,
    }
}

/// Effective starting color for the picker — the persisted `Some(color)`
/// when set, otherwise a sensible theme default for the role. Paint
/// integration (Phase 7) will resolve these defaults canonically; here
/// they only seed the picker's opening value.
fn effective_color_for(shape: &DrawingShape, role: ColorRole, cx: &App) -> Hsla {
    let theme = cx.theme();
    let fallback = match role {
        ColorRole::Primary => theme.foreground,
        ColorRole::Profit => theme.green,
        ColorRole::Loss => theme.red,
    };
    let stored: Option<DrawingColor> = match (shape, role) {
        (DrawingShape::Line(s), ColorRole::Primary)
        | (DrawingShape::Rect(s), ColorRole::Primary)
        | (DrawingShape::Arrow(s), ColorRole::Primary)
        | (DrawingShape::Fibonacci(s), ColorRole::Primary) => s.color,
        (DrawingShape::HorizontalRay(s), ColorRole::Primary) => s.color,
        (DrawingShape::AnchoredVwap(s), ColorRole::Primary) => s.color,
        (DrawingShape::Text(s), ColorRole::Primary) => s.color,
        (DrawingShape::Long(p), ColorRole::Profit) | (DrawingShape::Short(p), ColorRole::Profit) => {
            p.profit_color
        }
        (DrawingShape::Long(p), ColorRole::Loss) | (DrawingShape::Short(p), ColorRole::Loss) => {
            p.loss_color
        }
        _ => None,
    };
    stored.map(|c| c.into_hsla()).unwrap_or(fallback)
}

/// Featured-colors set passed to every ColorPicker — a handful of the
/// theme's named accents so the most common picks are one click away.
fn featured_palette(cx: &App) -> Vec<Hsla> {
    let theme = cx.theme();
    vec![
        theme.foreground,
        theme.red,
        theme.green,
        theme.blue,
        theme.yellow,
        theme.cyan,
        theme.magenta,
    ]
}

/// Workspace constructor — spawn a fresh content entity.
pub fn build(cx: &mut gpui::App) -> Entity<DrawingStripContent> {
    cx.new(DrawingStripContent::new)
}

/// Convenience re-export so callers can refer to the drawing-id by name.
pub type SelectionId = DrawingId;
