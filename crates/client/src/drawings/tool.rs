//! Global active-drawing-tool state. The top-bar "Draw" popover writes it; the
//! chart canvas reads it on mouse-down to decide what to create.

use gpui::{App, AppContext as _, Context, Entity, EventEmitter, Global, SharedString};
use gpui_component::IconName;

/// Active chart-canvas tool. `Select` is also "pan / hand" — drawing tools
/// switch back to Select after committing one drawing so the user can
/// immediately move it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tool {
    Select,
    Line,
    HorizontalRay,
    /// Full-width horizontal line. Same underlying shape as
    /// [`Tool::HorizontalRay`], but commits with `extend_left = true` so
    /// the stroke spans the entire chart width.
    HorizontalLine,
    Arrow,
    Rectangle,
    Fibonacci,
    AnchoredVwap,
    /// Fixed Range Volume Profile — click-drag two time anchors; the
    /// profile renders inside the bracket. Backed by the same shared
    /// `volume_profile` module the VRVP indicator uses.
    FixedRangeVolumeProfile,
    Text,
    Long,
    Short,
}

impl Tool {
    /// Full list in display order; drives the Draw popover's button grid.
    pub const ALL: &'static [Tool] = &[
        Tool::Select,
        Tool::Line,
        Tool::HorizontalRay,
        Tool::HorizontalLine,
        Tool::Arrow,
        Tool::Rectangle,
        Tool::Fibonacci,
        Tool::AnchoredVwap,
        Tool::FixedRangeVolumeProfile,
        Tool::Text,
        Tool::Long,
        Tool::Short,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Tool::Select => "Select",
            Tool::Line => "Line",
            Tool::HorizontalRay => "Horizontal Ray",
            Tool::HorizontalLine => "Horizontal Line",
            Tool::Arrow => "Arrow",
            Tool::Rectangle => "Rectangle",
            Tool::Fibonacci => "Fibonacci",
            Tool::AnchoredVwap => "Anchored VWAP",
            Tool::FixedRangeVolumeProfile => "FRVP",
            Tool::Text => "Text",
            Tool::Long => "Long",
            Tool::Short => "Short",
        }
    }

    /// Wire id used by the [`SetActiveTool`](super::actions::SetActiveTool)
    /// action so the icon button can dispatch without carrying the enum across
    /// the action boundary.
    pub fn id(self) -> &'static str {
        match self {
            Tool::Select => "select",
            Tool::Line => "line",
            Tool::HorizontalRay => "horizontal_ray",
            Tool::HorizontalLine => "horizontal_line",
            Tool::Arrow => "arrow",
            Tool::Rectangle => "rectangle",
            Tool::Fibonacci => "fibonacci",
            Tool::AnchoredVwap => "anchored_vwap",
            Tool::FixedRangeVolumeProfile => "frvp",
            Tool::Text => "text",
            Tool::Long => "long",
            Tool::Short => "short",
        }
    }

    pub fn from_id(id: &str) -> Option<Tool> {
        Self::ALL.iter().copied().find(|t| t.id() == id)
    }

    /// Icon shown on the popover row. The gpui-component asset set is small,
    /// so we map to close visual matches rather than the canonical Lucide
    /// names — the label next to each icon carries the meaning anyway.
    pub fn icon(self) -> IconName {
        match self {
            Tool::Select => IconName::Inspector,
            Tool::Line => IconName::Minus,
            Tool::HorizontalRay => IconName::ArrowRight,
            Tool::HorizontalLine => IconName::Minus,
            Tool::Arrow => IconName::ArrowRight,
            Tool::Rectangle => IconName::Frame,
            Tool::Fibonacci => IconName::ChartPie,
            // No dedicated VWAP icon; reuse the chart-line-ish glyph.
            Tool::AnchoredVwap => IconName::ChartPie,
            // Reuse the pie-ish chart icon for FRVP — closest visual
            // match in the gpui-component asset set.
            Tool::FixedRangeVolumeProfile => IconName::ChartPie,
            Tool::Text => IconName::CaseSensitive,
            Tool::Long => IconName::ArrowUp,
            Tool::Short => IconName::ArrowDown,
        }
    }

    pub fn is_drawing_tool(self) -> bool {
        !matches!(self, Tool::Select)
    }
}

#[derive(Clone, Debug)]
pub enum DrawingToolEvent {
    Changed,
}

pub struct DrawingToolState {
    tool: Tool,
}

impl Default for DrawingToolState {
    fn default() -> Self {
        Self {
            tool: Tool::Select,
        }
    }
}

impl EventEmitter<DrawingToolEvent> for DrawingToolState {}

impl DrawingToolState {
    pub fn new(_cx: &mut Context<Self>) -> Self {
        Self::default()
    }

    pub fn tool(&self) -> Tool {
        self.tool
    }

    pub fn set(&mut self, tool: Tool, cx: &mut Context<Self>) {
        if self.tool == tool {
            return;
        }
        self.tool = tool;
        cx.emit(DrawingToolEvent::Changed);
        cx.notify();
    }

    /// Switch back to Select. Used after a one-shot drawing tool commits, and
    /// from Escape / outside-click in the chart canvas.
    pub fn reset(&mut self, cx: &mut Context<Self>) {
        self.set(Tool::Select, cx);
    }
}

#[derive(Clone)]
pub struct DrawingToolStateHandle(pub Entity<DrawingToolState>);
impl Global for DrawingToolStateHandle {}

pub fn init(cx: &mut App) {
    let entity = cx.new(DrawingToolState::new);
    cx.set_global(DrawingToolStateHandle(entity));
}

/// Convenience: read the current tool from the global handle.
pub fn current_tool(cx: &App) -> Tool {
    cx.try_global::<DrawingToolStateHandle>()
        .map(|h| h.0.read(cx).tool())
        .unwrap_or(Tool::Select)
}

/// Convenience: set the current tool. No-op if the handle isn't published yet
/// (shouldn't happen at runtime; just keeps tests + early-init safe).
pub fn set_current_tool(tool: Tool, cx: &mut App) {
    let Some(handle) = cx.try_global::<DrawingToolStateHandle>().cloned() else {
        return;
    };
    handle.0.update(cx, |state, cx| state.set(tool, cx));
}

/// Tool-aware shared string used as a chip on the Draw popover button.
pub fn tool_chip_label(tool: Tool) -> SharedString {
    SharedString::from(match tool {
        Tool::Select => "Select",
        Tool::Line => "Line",
        Tool::HorizontalRay => "Ray",
        Tool::HorizontalLine => "HLine",
        Tool::Arrow => "Arrow",
        Tool::Rectangle => "Rect",
        Tool::Fibonacci => "Fib",
        Tool::AnchoredVwap => "AVWAP",
        Tool::FixedRangeVolumeProfile => "FRVP",
        Tool::Text => "Text",
        Tool::Long => "Long",
        Tool::Short => "Short",
    })
}
