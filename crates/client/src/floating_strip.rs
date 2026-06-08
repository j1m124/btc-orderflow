//! Compact floating strip — borrows the [`crate::floating_window`] drag
//! + clamp pattern, but renders no title bar, close X, or resize handle.
//! Drag is anchored to a dedicated `⋮⋮` grip on the left edge so clicks on
//! the strip's actual controls (color swatch, gear button, …) never start a
//! drag. The caller supplies an [`AnyView`] for the controls row.
//!
//! Workspace mounts a single instance and toggles visibility based on
//! `DrawingService::selected`. Persistence (origin) lives in the workspace —
//! the strip itself just owns the in-memory bounds and the drag mechanic.

use gpui::{
    AnyView, App, AppContext as _, Bounds, Context, DragMoveEvent, Empty, EntityId, EventEmitter,
    FocusHandle, Focusable, InteractiveElement as _, IntoElement, MouseButton, MouseDownEvent,
    MouseUpEvent, ParentElement as _, Pixels, Point, Render, StatefulInteractiveElement as _,
    Styled as _, Window, div, point, px, size,
};
use gpui_component::{ActiveTheme as _, ElementExt as _, h_flex, v_flex};

const GRIP_WIDTH: Pixels = px(14.);
const STRIP_HEIGHT: Pixels = px(36.);
const DEFAULT_WIDTH: Pixels = px(280.);
const TOP_MARGIN: Pixels = px(8.);

/// Emitted whenever the user finishes dragging the strip — workspace
/// subscribes so it can persist the new origin to local storage. Not
/// emitted during the in-flight drag (60 Hz) to avoid hammering storage.
#[derive(Clone, Debug)]
pub enum FloatingStripEvent {
    Moved(Point<Pixels>),
}

#[derive(Clone)]
struct StripDrag(EntityId);
impl Render for StripDrag {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

struct DragOrigin {
    initial_mouse_local: Point<Pixels>,
    initial_origin: Point<Pixels>,
}

pub struct FloatingStrip {
    focus_handle: FocusHandle,
    content: Option<AnyView>,
    /// Strip origin in container-local coords. `None` until first
    /// placement — the caller sets it via [`Self::set_origin`] once the
    /// container has been measured (or after loading from persistence).
    origin: Option<Point<Pixels>>,
    /// Latest container bounds captured during `on_prepaint`. Used to
    /// clamp the strip inside the visible area.
    container_bounds: Bounds<Pixels>,
    dragging: Option<DragOrigin>,
    visible: bool,
    /// Fallback default position the strip falls back to when no persisted
    /// origin is available (or when the persisted one is far off-screen).
    /// Centers the strip near the top of the container; chart workspace
    /// overrides this if needed.
    default_origin: Option<Point<Pixels>>,
}

impl FloatingStrip {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            content: None,
            origin: None,
            container_bounds: Bounds::default(),
            dragging: None,
            visible: false,
            default_origin: None,
        }
    }

    pub fn set_content(&mut self, content: AnyView, cx: &mut Context<Self>) {
        self.content = Some(content);
        cx.notify();
    }

    pub fn clear_content(&mut self, cx: &mut Context<Self>) {
        self.content = None;
        cx.notify();
    }

    pub fn show(&mut self, cx: &mut Context<Self>) {
        if !self.visible {
            self.visible = true;
            cx.notify();
        }
    }

    pub fn hide(&mut self, cx: &mut Context<Self>) {
        if self.visible {
            self.visible = false;
            cx.notify();
        }
    }

    pub fn is_visible(&self) -> bool {
        self.visible
    }

    pub fn origin(&self) -> Option<Point<Pixels>> {
        self.origin
    }

    pub fn set_origin(&mut self, origin: Point<Pixels>, cx: &mut Context<Self>) {
        self.origin = Some(self.clamp_origin(origin));
        cx.notify();
    }

    /// Set the fallback default origin used when no persisted position is
    /// available. Workspace calls this once the chart panel has been laid
    /// out so the strip lands above the price chart on first show.
    pub fn set_default_origin(&mut self, origin: Point<Pixels>, cx: &mut Context<Self>) {
        self.default_origin = Some(origin);
        if self.origin.is_none() {
            self.origin = Some(self.clamp_origin(origin));
            cx.notify();
        }
    }

    fn clamp_origin(&self, origin: Point<Pixels>) -> Point<Pixels> {
        let strip_w = self.measured_width();
        let max_x = (self.container_bounds.size.width - strip_w).max(px(0.));
        let max_y = (self.container_bounds.size.height - STRIP_HEIGHT).max(px(0.));
        point(origin.x.clamp(px(0.), max_x), origin.y.clamp(px(0.), max_y))
    }

    fn measured_width(&self) -> Pixels {
        // The strip auto-sizes to its content but we don't know that until
        // paint. Use DEFAULT_WIDTH as a generous floor for clamp purposes —
        // overshoot is preferable to under-clamping (clipped content beats
        // an off-screen strip).
        DEFAULT_WIDTH
    }

    fn on_container_layout(&mut self, container: Bounds<Pixels>, cx: &mut Context<Self>) {
        let prev = self.container_bounds;
        self.container_bounds = container;
        let has_area = container.size.width > px(0.) && container.size.height > px(0.);
        if self.origin.is_none() && has_area {
            let fallback = self.default_origin.unwrap_or_else(|| {
                let cx_pos =
                    ((container.size.width - self.measured_width()) / 2.).max(px(0.));
                point(cx_pos, TOP_MARGIN)
            });
            self.origin = Some(self.clamp_origin(fallback));
            cx.notify();
        } else if container.size != prev.size {
            if let Some(o) = self.origin {
                let clamped = self.clamp_origin(o);
                if clamped != o {
                    self.origin = Some(clamped);
                    cx.notify();
                }
            }
        }
    }

    fn on_grip_down(&mut self, ev: &MouseDownEvent, _: &mut Window, _: &mut Context<Self>) {
        let Some(o) = self.origin else { return };
        self.dragging = Some(DragOrigin {
            initial_mouse_local: ev.position - self.container_bounds.origin,
            initial_origin: o,
        });
    }

    fn on_grip_move(&mut self, mouse_window: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(d) = &self.dragging else { return };
        let local = mouse_window - self.container_bounds.origin;
        let delta = local - d.initial_mouse_local;
        let new_origin = self.clamp_origin(d.initial_origin + delta);
        if Some(new_origin) != self.origin {
            self.origin = Some(new_origin);
            cx.notify();
        }
    }

    fn on_mouse_up(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        if self.dragging.take().is_some() {
            if let Some(o) = self.origin {
                cx.emit(FloatingStripEvent::Moved(o));
            }
        }
    }
}

impl Focusable for FloatingStrip {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<FloatingStripEvent> for FloatingStrip {}

impl Render for FloatingStrip {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let entity_id = cx.entity_id();
        let theme = cx.theme();
        let theme_bg = theme.background;
        let theme_border = theme.border;
        let theme_muted = theme.muted_foreground;
        let view = cx.entity().clone();

        let mut layer = div()
            .id("floating-strip-layer")
            .absolute()
            .inset_0()
            .on_prepaint(move |bounds, _, cx| {
                _ = view.update(cx, |this, cx| this.on_container_layout(bounds, cx));
            })
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, w, cx| this.on_mouse_up(w, cx)),
            );

        if !self.visible {
            return layer;
        }
        let Some(origin) = self.origin else {
            return layer;
        };
        let Some(content) = self.content.clone() else {
            return layer;
        };

        let grip = div()
            .id("floating-strip-grip")
            .flex()
            .items_center()
            .justify_center()
            .w(GRIP_WIDTH)
            .h_full()
            .cursor_grab()
            .text_color(theme_muted)
            .text_size(px(12.))
            .child("⋮⋮")
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, ev: &MouseDownEvent, w, cx| this.on_grip_down(ev, w, cx)),
            )
            .on_drag(StripDrag(entity_id), |drag, _, _, cx| {
                cx.stop_propagation();
                cx.new(|_| drag.clone())
            })
            .on_drag_move(cx.listener(
                move |this, e: &DragMoveEvent<StripDrag>, _w, cx| {
                    let StripDrag(id) = e.drag(cx);
                    if *id != entity_id {
                        return;
                    }
                    this.on_grip_move(e.event.position, cx);
                },
            ));

        let card = h_flex()
            .id("floating-strip-card")
            .absolute()
            .left(origin.x)
            .top(origin.y)
            .h(STRIP_HEIGHT)
            .items_stretch()
            .bg(theme_bg)
            .border_1()
            .border_color(theme_border)
            .rounded(px(6.))
            .shadow_lg()
            .overflow_hidden()
            .occlude()
            // Clicks on the strip's interior shouldn't bubble to deselect /
            // pan handlers in the chart canvas.
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(grip)
            .child(
                v_flex()
                    .flex_1()
                    .justify_center()
                    .child(content),
            );

        layer = layer.child(card);
        layer
    }
}

/// Tiny helper used by content views that want to lay out a uniform
/// vertical separator between strip control groups.
pub fn strip_separator(cx: &App) -> impl IntoElement {
    div()
        .w(px(1.))
        .h(px(20.))
        .mx_2()
        .bg(cx.theme().border)
}

/// Default opening size convenience (callers may ignore — the strip auto
/// sizes width to its content). Exported so the workspace can compute its
/// initial centered position.
pub fn default_size() -> gpui::Size<Pixels> {
    size(DEFAULT_WIDTH, STRIP_HEIGHT)
}
