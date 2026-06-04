//! Reusable floating-window wrapper. Renders an absolutely-positioned card
//! over a `relative()` parent container with a drag bar, close X, and a
//! bottom-right resize handle. Content is supplied as an `AnyView` so the
//! wrapper is agnostic to what's inside.
//!
//! Drag/resize use the same `on_drag` / `on_drag_move` flow as
//! `gpui-component`'s `Tiles`: mouse-down captures the starting state,
//! `on_drag` returns a no-op render payload, and `on_drag_move` applies the
//! delta with clamping. Mouse-up on the overlay layer clears the drag state
//! so a release outside the drag bar still ends the gesture.
//!
//! First-frame placement: the wrapper measures its container via
//! `on_prepaint`, then centers the card on the next frame. The card is
//! hidden during the unmeasured first frame so it doesn't flash at (0, 0).

use gpui::{
    AnyView, App, AppContext as _, Bounds, Context, DismissEvent, DragMoveEvent, Empty, EntityId,
    EventEmitter, FocusHandle, Focusable, InteractiveElement as _, IntoElement, MouseButton,
    MouseDownEvent, MouseUpEvent, ParentElement as _, Pixels, Point, Render, SharedString, Size,
    StatefulInteractiveElement as _, Styled as _, Window, div, point, px, size,
};
use gpui_component::{ActiveTheme as _, ElementExt as _, StyledExt as _, h_flex, v_flex};

/// Default opening size for a newly-spawned floating window. Tuned to match
/// the "~720×480" target from the spec.
const DEFAULT_SIZE: Size<Pixels> = Size {
    width: px(720.),
    height: px(480.),
};

/// Minimum size enforced by drag-resize. Below this the window starts to
/// hide its own controls.
const MIN_SIZE: Size<Pixels> = Size {
    width: px(320.),
    height: px(200.),
};

const TITLE_BAR_HEIGHT: Pixels = px(32.);
const RESIZE_HANDLE_SIZE: Pixels = px(14.);

/// Drag payload for the title-bar move gesture. The `EntityId` filters out
/// drag events that started on a different floating window.
#[derive(Clone)]
struct DragMoving(EntityId);
impl Render for DragMoving {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

/// Drag payload for the corner resize gesture.
#[derive(Clone)]
struct DragResizing(EntityId);
impl Render for DragResizing {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

struct DragOrigin {
    initial_mouse_local: Point<Pixels>,
    initial_origin: Point<Pixels>,
}

struct ResizeOrigin {
    initial_mouse: Point<Pixels>,
    initial_size: Size<Pixels>,
}

pub struct FloatingWindow {
    focus_handle: FocusHandle,
    title: SharedString,
    content: AnyView,
    /// Card bounds *within the container* (relative to container origin).
    bounds: Bounds<Pixels>,
    /// Latest window-space bounds of the overlay layer, captured during
    /// `on_prepaint`. Used to translate mouse positions into container-local
    /// coordinates.
    container_bounds: Bounds<Pixels>,
    /// `false` until the first prepaint measures the container, at which
    /// point the card is centered and `placed` flips true. The card is not
    /// rendered while `false` to avoid a flash at (0, 0).
    placed: bool,
    dragging: Option<DragOrigin>,
    resizing: Option<ResizeOrigin>,
}

impl FloatingWindow {
    pub fn new(
        title: impl Into<SharedString>,
        content: AnyView,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            title: title.into(),
            content,
            bounds: Bounds {
                origin: point(px(0.), px(0.)),
                size: DEFAULT_SIZE,
            },
            container_bounds: Bounds::default(),
            placed: false,
            dragging: None,
            resizing: None,
        }
    }

    fn clamp_origin(&self, origin: Point<Pixels>) -> Point<Pixels> {
        let max_x = (self.container_bounds.size.width - self.bounds.size.width).max(px(0.));
        let max_y = (self.container_bounds.size.height - self.bounds.size.height).max(px(0.));
        point(origin.x.clamp(px(0.), max_x), origin.y.clamp(px(0.), max_y))
    }

    fn on_container_layout(&mut self, container: Bounds<Pixels>, cx: &mut Context<Self>) {
        let prev = self.container_bounds;
        self.container_bounds = container;

        let has_area = container.size.width > px(0.) && container.size.height > px(0.);
        if !self.placed && has_area {
            // Center on first measurement. Half-sizes can't fit in the const
            // arithmetic, so do it once here.
            let cx_pos = ((container.size.width - self.bounds.size.width) / 2.).max(px(0.));
            let cy_pos = ((container.size.height - self.bounds.size.height) / 2.).max(px(0.));
            self.bounds.origin = point(cx_pos, cy_pos);
            self.placed = true;
            cx.notify();
        } else if container.size != prev.size {
            // Container resized (e.g. browser window). Re-clamp so the card
            // doesn't end up parked off the visible area.
            let new_origin = self.clamp_origin(self.bounds.origin);
            if new_origin != self.bounds.origin {
                self.bounds.origin = new_origin;
                cx.notify();
            }
        }
    }

    fn on_drag_bar_down(&mut self, ev: &MouseDownEvent, _: &mut Window, _: &mut Context<Self>) {
        self.dragging = Some(DragOrigin {
            initial_mouse_local: ev.position - self.container_bounds.origin,
            initial_origin: self.bounds.origin,
        });
    }

    fn on_drag_bar_move(&mut self, mouse_window: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(d) = &self.dragging else {
            return;
        };
        let local = mouse_window - self.container_bounds.origin;
        let delta = local - d.initial_mouse_local;
        let new_origin = self.clamp_origin(d.initial_origin + delta);
        if new_origin != self.bounds.origin {
            self.bounds.origin = new_origin;
            cx.notify();
        }
    }

    fn on_resize_down(&mut self, ev: &MouseDownEvent, _: &mut Window, _: &mut Context<Self>) {
        self.resizing = Some(ResizeOrigin {
            initial_mouse: ev.position,
            initial_size: self.bounds.size,
        });
    }

    fn on_resize_move(&mut self, mouse_window: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(r) = &self.resizing else {
            return;
        };
        let delta = mouse_window - r.initial_mouse;
        let mut new_w = (r.initial_size.width + delta.x).max(MIN_SIZE.width);
        let mut new_h = (r.initial_size.height + delta.y).max(MIN_SIZE.height);
        // Cap at the container edge so resize can't push the card past the
        // right/bottom and lose the resize handle.
        let max_w = (self.container_bounds.size.width - self.bounds.origin.x).max(MIN_SIZE.width);
        let max_h = (self.container_bounds.size.height - self.bounds.origin.y).max(MIN_SIZE.height);
        new_w = new_w.min(max_w);
        new_h = new_h.min(max_h);
        if new_w != self.bounds.size.width || new_h != self.bounds.size.height {
            self.bounds.size = size(new_w, new_h);
            cx.notify();
        }
    }

    fn on_mouse_up(&mut self, _: &mut Window, _: &mut Context<Self>) {
        self.dragging = None;
        self.resizing = None;
    }

    fn close(&mut self, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }
}

impl Focusable for FloatingWindow {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<DismissEvent> for FloatingWindow {}

impl Render for FloatingWindow {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme_bg = cx.theme().background;
        let theme_border = cx.theme().border;
        let theme_muted = cx.theme().muted_foreground;
        let theme_fg = cx.theme().foreground;

        let entity_id = cx.entity_id();
        let bounds = self.bounds;
        let placed = self.placed;
        let view = cx.entity().clone();

        let title_bar = h_flex()
            .id("floating-titlebar")
            .h(TITLE_BAR_HEIGHT)
            .w_full()
            .px_3()
            .items_center()
            .justify_between()
            .border_b_1()
            .border_color(theme_border)
            .bg(theme_bg)
            .cursor_grab()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, ev: &MouseDownEvent, w, cx| this.on_drag_bar_down(ev, w, cx)),
            )
            .on_drag(DragMoving(entity_id), |drag, _, _, cx| {
                cx.stop_propagation();
                cx.new(|_| drag.clone())
            })
            .on_drag_move(cx.listener(
                move |this, e: &DragMoveEvent<DragMoving>, _w, cx| {
                    let DragMoving(id) = e.drag(cx);
                    if *id != entity_id {
                        return;
                    }
                    this.on_drag_bar_move(e.event.position, cx);
                },
            ))
            .child(
                div()
                    .text_sm()
                    .font_semibold()
                    .text_color(theme_fg)
                    .child(self.title.clone()),
            )
            .child(
                div()
                    .id("floating-close")
                    .cursor_pointer()
                    .px_2()
                    .text_sm()
                    .text_color(theme_muted)
                    .child("\u{2715}")
                    // Stop the mouse_down before it reaches the drag bar so
                    // clicking the X doesn't start a drag.
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(cx.listener(|this, _, _, cx| this.close(cx))),
            );

        let resize_handle = div()
            .id("floating-resize")
            .absolute()
            .bottom_0()
            .right_0()
            .w(RESIZE_HANDLE_SIZE)
            .h(RESIZE_HANDLE_SIZE)
            // gpui's cursor helpers are inverted relative to CSS: the
            // `cursor_nwse_resize` macro routes through `ResizeUpLeftDownRight`,
            // which the web backend emits as CSS `nesw-resize` (the ↗↙ cursor
            // for top-right / bottom-left corners). For a true bottom-right
            // resize cursor (↘↖ / CSS `nwse-resize`) we have to call the
            // oppositely-named helper.
            .cursor_nesw_resize()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, ev: &MouseDownEvent, w, cx| this.on_resize_down(ev, w, cx)),
            )
            .on_drag(DragResizing(entity_id), |drag, _, _, cx| {
                cx.stop_propagation();
                cx.new(|_| drag.clone())
            })
            .on_drag_move(cx.listener(
                move |this, e: &DragMoveEvent<DragResizing>, _w, cx| {
                    let DragResizing(id) = e.drag(cx);
                    if *id != entity_id {
                        return;
                    }
                    this.on_resize_move(e.event.position, cx);
                },
            ));

        let content = self.content.clone();

        // The overlay layer fills its `relative()` parent so we can measure
        // the container with on_prepaint and catch mouse_up anywhere over
        // it (so drag/resize release even if the cursor left the small
        // handle area).
        let mut layer = div()
            .id("floating-window-layer")
            .absolute()
            .inset_0()
            .on_prepaint(move |bounds, _, cx| {
                _ = view.update(cx, |this, cx| this.on_container_layout(bounds, cx));
            })
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, w, cx| this.on_mouse_up(w, cx)),
            );

        if placed {
            let card = v_flex()
                .id("floating-card")
                .absolute()
                .left(bounds.origin.x)
                .top(bounds.origin.y)
                .w(bounds.size.width)
                .h(bounds.size.height)
                .bg(theme_bg)
                .border_1()
                .border_color(theme_border)
                .rounded(px(6.))
                .shadow_lg()
                .overflow_hidden()
                .occlude()
                // Clicks inside the card shouldn't bubble to the layer's
                // mouse handlers (which are only for drag release).
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(title_bar)
                .child(
                    div()
                        .id("floating-content")
                        .flex_1()
                        .min_h_0()
                        .overflow_hidden()
                        .child(content),
                )
                .child(resize_handle);
            layer = layer.child(card);
        }
        layer
    }
}
