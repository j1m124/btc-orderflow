//! Drawing subsystem: shapes, global service, active-tool state, actions.
//!
//! Replaces the per-chart drawing state that lived on `ChartState`. Drawings
//! are now shared across every chart of the same symbol — see
//! [`service::DrawingService`] for the storage model.

use std::cell::RefCell;
use std::rc::Rc;

use gpui::{App, Global, SharedString};

pub mod actions;
pub mod service;
pub mod settings_view;
pub mod shapes;
pub mod strip_content;
pub mod tool;

/// Captured at right-mouse-down on a chart canvas; read by that chart's
/// `ContextMenu` builder so right-clicking on a drawing surfaces per-drawing
/// menu items (Hide / Visible-on / Delete) and right-clicking on empty area
/// falls back to canvas-wide actions (Clear / Reset scale).
///
/// Single global because only one context menu can be open at once.
#[derive(Clone, Debug)]
pub struct RightClickTarget {
    pub symbol: SharedString,
    pub drawing_id: Option<service::DrawingId>,
}

#[derive(Default)]
pub struct LastChartRightClick(pub Rc<RefCell<Option<RightClickTarget>>>);
impl Global for LastChartRightClick {}

/// Wire the global drawing services. Called once from `lib.rs::run` before any
/// panel can subscribe.
pub fn init(cx: &mut App) {
    service::init(cx);
    tool::init(cx);
    cx.set_global(LastChartRightClick::default());
}
