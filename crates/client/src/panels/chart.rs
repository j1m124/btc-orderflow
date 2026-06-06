//! Chart panel: candlestick rendering, pan/zoom, drawing tools, crosshair.
//! Owned by `ContentPanel` via `Option<ChartState>` when `kind == Kind::Chart`.
//!
//! The paint pipeline (candles + grid + axis labels + drawings overlay)
//! lives in [`paint`]. State, interaction handlers, and the render-tree
//! assembly stay here.

mod drawings_view;
mod footprint;
mod paint;

pub use footprint::{
    ColorScope, FootprintParams, RenderKind, RenderMetric, TextMetric, WireframeVariant,
};

use gpui::{
    Action, AppContext as _, Bounds, ContentMask, Context, Entity, FocusHandle, Focusable as _,
    Hsla, InteractiveElement as _, IntoElement, MouseButton, MouseDownEvent, MouseMoveEvent,
    ParentElement as _, Pixels, Point, ScrollWheelEvent, SharedString,
    StatefulInteractiveElement as _, Styled as _, Window, canvas, div, prelude::FluentBuilder as _,
    px,
};
use gpui_component::{
    ActiveTheme as _, ElementExt as _, IconName, Sizable as _, StyledExt as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputState},
    menu::{ContextMenuExt as _, DropdownMenu as _},
    plot::AXIS_GAP,
    v_flex,
};
use serde::Deserialize;

use self::paint::{
    DrawingColors, MainChartColors, OverlayPaintItem, PanePaintItem, paint_main_chart,
    paint_overlay_indicators, paint_sub_pane, render_drawings_overlay,
};
use super::ContentPanel;
use crate::drawings::service::{DrawingId, DrawingServiceHandle};
use crate::drawings::tool::Tool;
use crate::indicators::{
    IndicatorInstance, IndicatorKind, IndicatorOutput, InstanceId, Placement, ValueReadout,
    VolumeParams, palette_color_for,
};
use crate::panels::LastFocusedChart;
use crate::services::market_data::{self, Candle, Timeframe};

/// Format a bar's open_time in the user's chosen timezone for crosshair /
/// OHLC pill display. `Candle::date` is frozen at ingestion time using
/// `Local`, so the pre-formatted string ignores any later Settings change —
/// reading from `open_time` at render time fixes that.
fn format_user_tz(open_time: i64, cx: &gpui::App) -> String {
    use chrono::TimeZone as _;
    let offset = crate::prefs::offset_for(cx, open_time);
    offset
        .timestamp_millis_opt(open_time)
        .single()
        .map(|dt| dt.format("%b %d %H:%M").to_string())
        .unwrap_or_default()
}

use crate::services::symbols::SymbolsServiceHandle;
use crate::symbol_picker::OpenSymbolPicker;

/// Switch the chart's timeframe. Carries the timeframe's wire string (`1m`,
/// `5m`, …); the handler parses it back to a [`Timeframe`]. Dispatched from the
/// chart's timeframe-selector dropdown, scoped to this panel's focus so it
/// dispatches up through *this* panel (not whichever element had focus when the
/// menu opened), keeping multiple Chart panels independent.
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = client, no_json)]
pub struct ChangeChartTimeframe(pub SharedString);

// Drawing-delete + clear actions now live in `crate::drawings::actions` so
// they're shared with workspace-wide key bindings and the Objects popover.

/// Right-click context-menu actions on an indicator chip. The instance id
/// is carried inline; the handler lives on `ContentPanel` so the action
/// naturally routes to the chart panel hosting the chip (no
/// `LastFocusedChart` round-trip needed — context menus bubble from the
/// element they were opened on).
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = client, no_json)]
pub struct MoveIndicatorPaneUp(pub u64);

#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = client, no_json)]
pub struct MoveIndicatorPaneDown(pub u64);

#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = client, no_json)]
pub struct ToggleIndicatorHidden(pub u64);

#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = client, no_json)]
pub struct RemoveIndicator(pub u64);

/// Reset the chart's viewport to its default trailing window (x-axis) and
/// re-enable y-auto-fit (price axis). Dispatched from the chart's right-click
/// context menu — mirrors what double-clicking either axis would do, but in
/// one shot.
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = client, no_json)]
pub struct ResetChartScale;

/// Snap the viewport so the latest bar lands at the default trailing offset
/// AND enable sticky-tail mode: subsequent new bars advance `view_start` so
/// the chart stays glued to the live edge. Sticky stays on until the user
/// pans the canvas horizontally. Dispatched from the floating bottom-right
/// button and from the right-click context menu.
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = client, no_json)]
pub struct GoToLatest;

// ============================================================================
// Chart
// ============================================================================

/// `Candle` is owned by [`crate::services::market_data`] so the live service
/// can populate it directly — no translation layer between WS events and the
/// chart's render path.

// Default candles per viewport — now user-settable via Settings → General →
// Chart. The const remains as the seed value for the atomic in `prefs.rs`;
// runtime reads go through `crate::prefs::chart_default_view()`.
const CHART_MIN_VIEW: f32 = 8.0;
/// Maximum candles visible at once — the hard zoom-out limit. Past ~1px per
/// candle the view is already aggregated (see the dense paint path), so showing
/// more adds no detail; this also keeps the whole buffer (up to 5,000 bars)
/// from being crammed on screen. Users pan to reach older history, not zoom.
/// Capped to the buffer length when fewer bars are loaded.
const CHART_MAX_VIEW: f32 = 1000.0;
// Right-edge buffer ratio — now user-settable via Settings → General → Chart.
// Reads at runtime through `crate::prefs::chart_right_buffer()`.
/// Symmetric left buffer: lets the user pan/zoom-out past bar 0 into empty
/// space. Required so wheel-zoom-out can keep its right-edge anchor invariant
/// even when nearing the historical edge.
const CHART_LEFT_BUFFER_RATIO: f32 = 0.50;
/// Minimum vertical motion in pixels before a canvas drag starts panning Y.
/// Pure horizontal drags below this threshold leave `y_auto` alone so casual
/// time-scrubbing keeps the price axis auto-fitting.
const Y_FREEZE_DEADZONE_PX: f32 = 4.0;
/// Pixels of wheel `delta_y` per one zoom unit — the divisor in the
/// exponential `factor = exp(-delta_y / SCROLL_ZOOM_RATE)` used by the
/// canvas, x-axis, and y-axis scroll handlers. 120 matches the historical
/// "one mouse-wheel notch" on Windows/Mac; lower values make the wheel
/// zoom more aggressive, higher values dampen it.
const SCROLL_ZOOM_RATE: f32 = 240.0;

/// Per-drag state for canvas 2D panning. X always pans from `start_view_start`;
/// Y panning is "lazy" — `y_freeze` stays `None` until the drag accumulates
/// `Y_FREEZE_DEADZONE_PX` of vertical motion from `start_pos`. Once it trips,
/// we snapshot the price range at that instant and translate from that
/// baseline so the chart doesn't jump at threshold cross.
#[derive(Clone, Copy)]
struct CanvasDrag {
    start_pos: Point<Pixels>,
    start_view_start: f32,
    y_freeze: Option<(Point<Pixels>, f64, f64)>,
}

// `Tool` and `DrawingId` are imported from `crate::drawings::{tool, service}` —
// the chart no longer owns the active tool (it's a workspace-global state).

/// All drawings are anchored in world coords: time-index (fractional candle
/// position) for the x axis, price for the y axis. Strokes, handles, and text
/// are rendered in pixel sizes so they don't visually scale with zoom.
#[derive(Clone, Debug)]
pub enum Drawing {
    Line {
        id: DrawingId,
        a: (f32, f64),
        b: (f32, f64),
    },
    Rect {
        id: DrawingId,
        a: (f32, f64),
        b: (f32, f64),
    },
    Text {
        id: DrawingId,
        anchor: (f32, f64),
        /// Box width in screen pixels — text wraps inside this width and the
        /// rendered div's height grows with content. World-coord width
        /// wouldn't work because text uses pixel-based font sizing.
        width: f32,
        text: String,
    },
    Long {
        id: DrawingId,
        t0: f32,
        t1: f32,
        entry: f64,
        take_profit: f64,
        stop_loss: f64,
    },
    Short {
        id: DrawingId,
        t0: f32,
        t1: f32,
        entry: f64,
        take_profit: f64,
        stop_loss: f64,
    },
    HorizontalRay {
        id: DrawingId,
        /// `(time_idx, price)`. The line extends from this point to the
        /// chart's right edge at the constant price.
        anchor: (f32, f64),
        /// Optional label rendered at the top-right of the ray. Edited via
        /// the chart's right-click menu.
        text: Option<String>,
    },
    Arrow {
        id: DrawingId,
        a: (f32, f64),
        /// Endpoint that carries the arrowhead.
        b: (f32, f64),
    },
    /// Fibonacci retracement. `a` and `b` define the price range and
    /// horizontal extent; the renderer paints horizontal levels at the
    /// standard ratios between `a.1` and `b.1`.
    Fibonacci {
        id: DrawingId,
        a: (f32, f64),
        b: (f32, f64),
    },
    /// Anchored VWAP starting at `anchor.0` (fractional bar index). Price
    /// component `anchor.1` is unused for rendering (the line is computed
    /// from candle data) but kept for symmetry with single-anchor drawings
    /// so existing handle/hit-test scaffolding doesn't special-case.
    AnchoredVwap {
        id: DrawingId,
        anchor: (f32, f64),
    },
}

impl Drawing {
    fn id(&self) -> DrawingId {
        match self {
            Drawing::Line { id, .. }
            | Drawing::Rect { id, .. }
            | Drawing::Text { id, .. }
            | Drawing::Long { id, .. }
            | Drawing::Short { id, .. }
            | Drawing::HorizontalRay { id, .. }
            | Drawing::Arrow { id, .. }
            | Drawing::Fibonacci { id, .. }
            | Drawing::AnchoredVwap { id, .. } => *id,
        }
    }

    /// Shift this drawing's time (x) anchors right by `n` candle indices. Used
    /// when older candles are prepended so drawings stay attached to their bars.
    fn shift_x(&mut self, n: f32) {
        match self {
            Drawing::Line { a, b, .. }
            | Drawing::Rect { a, b, .. }
            | Drawing::Arrow { a, b, .. }
            | Drawing::Fibonacci { a, b, .. } => {
                a.0 += n;
                b.0 += n;
            }
            Drawing::Text { anchor, .. }
            | Drawing::HorizontalRay { anchor, .. }
            | Drawing::AnchoredVwap { anchor, .. } => {
                anchor.0 += n;
            }
            Drawing::Long { t0, t1, .. } | Drawing::Short { t0, t1, .. } => {
                *t0 += n;
                *t1 += n;
            }
        }
    }
}

/// In-progress drawing being constructed via click-drag. Held in `ChartState`
/// from mouse-down on a drawing tool until mouse-up commits it into
/// `state.drawings` with a fresh `DrawingId`. Position variants store the
/// time anchor + a default horizontal width fixed at creation — vertical drag
/// changes only the TP price; width edits happen after via endpoint handles
/// (deferred to v2).
#[derive(Clone, Debug)]
enum CreatingDrawing {
    Line {
        a: (f32, f64),
        b: (f32, f64),
    },
    Arrow {
        a: (f32, f64),
        b: (f32, f64),
    },
    Rect {
        a: (f32, f64),
        b: (f32, f64),
    },
    Fibonacci {
        a: (f32, f64),
        b: (f32, f64),
    },
    Long {
        entry: (f32, f64),
        tp: (f32, f64),
    },
    Short {
        entry: (f32, f64),
        tp: (f32, f64),
    },
    /// One-click horizontal ray. The mouse-down handler commits it
    /// immediately; this variant only exists for symmetry with the other
    /// in-flight states (e.g. so a tool switch mid-creation can drop it).
    HorizontalRay {
        anchor: (f32, f64),
    },
    /// One-click Anchored VWAP. Same shape as `HorizontalRay`: the mouse-down
    /// handler commits the drawing with this anchor immediately; the line
    /// itself is computed at paint time from the chart's candle buffer.
    AnchoredVwap {
        anchor: (f32, f64),
    },
}

/// Default-R:R risk-to-reward ratio for new positions. SL is placed half the
/// distance to TP on the opposite side of entry — per spec section 9d.
const POSITION_DEFAULT_RR: f64 = 2.0;
/// Default position width as a fraction of `view_size` — per spec section 9d.
const POSITION_DEFAULT_WIDTH_RATIO: f32 = 0.30;

impl CreatingDrawing {
    fn from_tool(tool: Tool, pt: (f32, f64), default_width: f32) -> Option<Self> {
        match tool {
            Tool::Line => Some(CreatingDrawing::Line { a: pt, b: pt }),
            Tool::Arrow => Some(CreatingDrawing::Arrow { a: pt, b: pt }),
            Tool::Rectangle => Some(CreatingDrawing::Rect { a: pt, b: pt }),
            Tool::Fibonacci => Some(CreatingDrawing::Fibonacci { a: pt, b: pt }),
            Tool::HorizontalRay => Some(CreatingDrawing::HorizontalRay { anchor: pt }),
            Tool::AnchoredVwap => Some(CreatingDrawing::AnchoredVwap { anchor: pt }),
            Tool::Long => Some(CreatingDrawing::Long {
                entry: pt,
                tp: (pt.0 + default_width, pt.1),
            }),
            Tool::Short => Some(CreatingDrawing::Short {
                entry: pt,
                tp: (pt.0 + default_width, pt.1),
            }),
            _ => None,
        }
    }

    fn set_end(&mut self, pt: (f32, f64)) {
        match self {
            CreatingDrawing::Line { b, .. }
            | CreatingDrawing::Arrow { b, .. }
            | CreatingDrawing::Rect { b, .. }
            | CreatingDrawing::Fibonacci { b, .. } => *b = pt,
            // Position width is locked at creation; only the TP price (y)
            // follows the cursor. Otherwise zero-width rects would be easy to
            // accidentally create with a near-vertical drag.
            CreatingDrawing::Long { tp, .. } | CreatingDrawing::Short { tp, .. } => {
                tp.1 = pt.1;
            }
            // Horizontal ray is single-click; no trailing endpoint to track.
            CreatingDrawing::HorizontalRay { .. } => {}
            // Same single-click model.
            CreatingDrawing::AnchoredVwap { .. } => {}
        }
    }

    /// Shift the in-progress drawing's time (x) anchors right by `n` indices.
    fn shift_x(&mut self, n: f32) {
        match self {
            CreatingDrawing::Line { a, b }
            | CreatingDrawing::Arrow { a, b }
            | CreatingDrawing::Rect { a, b }
            | CreatingDrawing::Fibonacci { a, b } => {
                a.0 += n;
                b.0 += n;
            }
            CreatingDrawing::Long { entry, tp } | CreatingDrawing::Short { entry, tp } => {
                entry.0 += n;
                tp.0 += n;
            }
            CreatingDrawing::HorizontalRay { anchor } => {
                anchor.0 += n;
            }
            CreatingDrawing::AnchoredVwap { anchor } => {
                anchor.0 += n;
            }
        }
    }

    fn into_drawing(self, id: DrawingId) -> Drawing {
        match self {
            CreatingDrawing::Line { a, b } => Drawing::Line { id, a, b },
            CreatingDrawing::Arrow { a, b } => Drawing::Arrow { id, a, b },
            CreatingDrawing::Rect { a, b } => Drawing::Rect { id, a, b },
            CreatingDrawing::Fibonacci { a, b } => Drawing::Fibonacci { id, a, b },
            CreatingDrawing::HorizontalRay { anchor } => Drawing::HorizontalRay {
                id,
                anchor,
                text: None,
            },
            CreatingDrawing::AnchoredVwap { anchor } => Drawing::AnchoredVwap { id, anchor },
            CreatingDrawing::Long { entry, tp } => {
                // 1 : POSITION_DEFAULT_RR — risk = reward / RR. For long the
                // SL sits *below* entry.
                let reward = tp.1 - entry.1;
                let sl = entry.1 - reward / POSITION_DEFAULT_RR;
                Drawing::Long {
                    id,
                    t0: entry.0,
                    t1: tp.0,
                    entry: entry.1,
                    take_profit: tp.1,
                    stop_loss: sl,
                }
            }
            CreatingDrawing::Short { entry, tp } => {
                // For short, reward materialises when price *falls*, so swap
                // the sign — SL sits above entry.
                let reward = entry.1 - tp.1;
                let sl = entry.1 + reward / POSITION_DEFAULT_RR;
                Drawing::Short {
                    id,
                    t0: entry.0,
                    t1: tp.0,
                    entry: entry.1,
                    take_profit: tp.1,
                    stop_loss: sl,
                }
            }
        }
    }

    /// Synthesize a paint-only `Drawing` with a sentinel id so the same render
    /// path used for committed drawings can also draw the in-progress preview.
    fn preview(&self) -> Drawing {
        self.clone().into_drawing(0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EditHandle {
    Body,
    /// Line/Rect first anchor (the "a" endpoint).
    EndpointA,
    /// Line/Rect second anchor (the "b" endpoint).
    EndpointB,
    /// Long/Short entry price line — vertical drag moves entry only.
    PositionEntry,
    /// Long/Short TP price line — vertical drag moves TP only.
    PositionTakeProfit,
    /// Long/Short SL price line — vertical drag moves SL only.
    PositionStopLoss,
    /// Long/Short left time edge (t0) — horizontal drag moves t0 only.
    PositionStart,
    /// Long/Short right time edge (t1) — horizontal drag moves t1 only.
    PositionEnd,
}

/// Active handle drag on an existing drawing. `baseline` is the drawing's
/// world coords at mouse-down so translations apply from a stable reference.
/// `anchor_screen` is the cursor position in canvas-relative pixels at
/// mouse-down — needed for handles that operate in pixel units (e.g. the
/// text-box width handle). `moved` flips to true the first time mouse-move
/// produces a non-zero translation; mouse-up reads it to distinguish "user
/// dragged the drawing" (snap to current-TF grid) from "user just clicked it
/// to select" (leave the drawing alone).
#[derive(Clone, Debug)]
struct EditDrag {
    id: DrawingId,
    handle: EditHandle,
    baseline: Drawing,
    anchor_world: (f32, f64),
    anchor_screen: (f32, f32),
    moved: bool,
}

/// Active inline text editor. Holds the live `Input` while the user types;
/// committed on mouse-down outside the input (canvas's mouse-down handler
/// drains this first). `existing_id == Some` when re-editing an existing
/// text drawing; `None` for new-text creation.
struct TextEditing {
    existing_id: Option<DrawingId>,
    anchor: (f32, f64),
    width: f32,
    input: Entity<InputState>,
}

/// Default pixel width for new text boxes. Drag the right-edge handle to
/// resize after creation.
const TEXT_DEFAULT_WIDTH_PX: f32 = 160.0;

pub struct ChartState {
    symbol: SharedString,
    /// Selected chart timeframe. Drives backfill/subscription and the x-axis
    /// step picker.
    timeframe: Timeframe,
    candles: Vec<Candle>,
    /// Fractional left-edge index of the visible window. Fractional so pan
    /// stays smooth at sub-candle granularity even though the chart paints
    /// integer-indexed bars. May go negative (left buffer) or extend past
    /// `total` (right buffer) by the ratios above.
    view_start: f32,
    /// Number of candles visible in the viewport (fractional for the same
    /// reason as `view_start`).
    view_size: f32,
    /// Set on left-mouse-down on the canvas, cleared on up. While present,
    /// mouse-move pans the view in 2D.
    drag_anchor: Option<CanvasDrag>,
    /// Last painted bounds of the chart canvas. Captured via `on_prepaint`
    /// and consumed by drag/wheel handlers to convert pixel deltas into
    /// candle-space deltas.
    bounds: Option<Bounds<Pixels>>,
    /// When true the price (y) axis auto-fits to the visible candles each
    /// render. Flipping to false locks the axis to (`y_min`, `y_max`) so
    /// users can drag/wheel the right edge to scale price independently.
    /// Restored to true via double-click on the right axis.
    y_auto: bool,
    /// Locked price-axis range. Only consulted when `y_auto` is false.
    y_min: f64,
    y_max: f64,
    /// Drag anchor for vertical-only manipulation on the right axis:
    /// `(mouse_down_position, y_min_at_down, y_max_at_down)`.
    y_drag_anchor: Option<(Point<Pixels>, f64, f64)>,
    /// Drag anchor for horizontal-only zoom on the bottom axis:
    /// `(mouse_down_position, view_size_at_down, view_start_at_down)`.
    /// `view_start_at_down` lets the zoom keep the viewport's centre at the
    /// position it was at drag-start, instead of recomputing the centre each
    /// frame (which drifts when `clamp` adjusts `view_start`).
    x_axis_drag_anchor: Option<(Point<Pixels>, f32, f32)>,
    /// In-progress drawing being constructed (Line / Rect / position).
    /// Local to the chart that started the click-drag; broadcast to the
    /// service only on mouse-up so other charts of the same symbol don't see
    /// a half-drawn shape.
    creating: Option<CreatingDrawing>,
    /// Active edit drag on an existing drawing (handle or body translation).
    /// Baseline is captured from the service at drag start, then the chart
    /// emits `preview_shape` on every move and a final `update_shape` on up.
    edit_drag: Option<EditDrag>,
    /// Inline text editor. Single instance — only one text can be edited at
    /// a time. Committed on mouse-down outside the input.
    editing_text: Option<TextEditing>,
    /// Cursor position in canvas-relative pixel coords. `Some` while the
    /// mouse hovers the canvas, `None` after the cursor leaves. Drives the
    /// crosshair overlay (guide lines + axis labels + OHLC readout).
    cursor: Option<(f32, f32)>,
    /// Width of the y-axis gutter in px, recomputed each render to fit the
    /// widest price label produced by the current `(y_min, y_max)` range.
    /// `Cell` for interior mutation so `render(&ChartState, …)` can refresh
    /// it without taking a mutable borrow that conflicts with `cx`. Read by
    /// hit-test helpers (`screen_to_index` etc.) so clicks and drags stay
    /// aligned with paint when the gutter resizes.
    y_axis_gap_px: std::cell::Cell<f32>,
    /// Sticky-tail mode: when true, new bars arriving via `apply_tick`,
    /// `tick_clock`, or `resnap` advance `view_start` so the chart stays
    /// glued to the live edge. Enabled by `snap_to_latest` (the "Go to
    /// latest" action); disabled the moment the user pans the canvas
    /// horizontally. Ephemeral, never persisted.
    sticky_to_latest: bool,
    /// Indicators attached to this chart panel. Carry over on symbol /
    /// timeframe switch (the user's analytical setup follows the chart panel,
    /// not the data). Volume is seeded by default — see `new()`.
    indicators: Vec<IndicatorInstance>,
    /// Cached output per indicator, parallel-indexed with `indicators`.
    /// Recomputed from `candles` on `apply_tick` / `tick_clock` / `resnap`
    /// / `apply_prepend` and on add / edit / remove. Paint reads this
    /// directly — no compute happens inside the paint closure.
    indicator_outputs: Vec<IndicatorOutput>,
    /// Active sub-pane splitter drag, if any. Set on splitter mouse-down
    /// (carrying the target instance id + a baseline of starting cursor-y
    /// and starting pane_height); read by the outer panel's mouse-move
    /// handler to update `pane_height`; cleared on mouse-up. Drag survives
    /// while the cursor stays inside the chart panel — exits past the
    /// panel edge end the drag (v1 limitation, follow-up via global drag).
    splitter_drag: Option<SplitterDrag>,
    /// Canvas-relative x of the cursor in whichever pane (main or sub-pane)
    /// is currently hovered. Drives the cross-pane vertical crosshair guide
    /// — `cursor` and `sub_cursor` track per-pane state for the horizontal
    /// guide / readouts, but vertical guides paint across every pane at the
    /// same x. `None` when the cursor isn't over any chart pane.
    cross_cursor_x: Option<f32>,
    /// When the cursor sits over a sub-pane, the id + canvas-relative
    /// position within that pane. Drives the hovered sub-pane's horizontal
    /// y-line + value-readout pill. `None` when the cursor is over the
    /// main pane (`cursor` carries it then) or outside the chart entirely.
    sub_cursor: Option<(InstanceId, f32, f32)>,
    /// Last-painted bounds per sub-pane canvas, keyed by instance id.
    /// Captured via each sub-canvas's `on_prepaint` and consumed by its
    /// `on_mouse_move` to translate window-relative event coords into the
    /// canvas-relative cursor used by the cross-pane crosshair pipeline.
    pane_bounds: std::collections::HashMap<InstanceId, Bounds<Pixels>>,
    /// Collapse state for the main-pane "Indicators (N) ▼" header chip.
    /// Ephemeral — not preserved across symbol/timeframe switches (those
    /// reconstruct `ChartState`), not persisted to local_storage. Toggled by
    /// the header chip's click handler.
    pub indicators_collapsed: bool,
    /// Active render mode (Candlestick / Footprint Cluster / Footprint
    /// Profile). Drives the paint pipeline branch in [`paint`] and, in
    /// later commits, the header dropdown + the synthesized render chip
    /// pinned at the top of the indicator list. Defaults to `Candlestick`
    /// and is preserved across symbol/timeframe switches (the render
    /// choice follows the panel, mirroring how indicators do — see
    /// [`Self::adopt_render_settings`]).
    render_kind: RenderKind,
    /// Eye-toggle state on the render chip. False suppresses the candle /
    /// cell / profile paint (overlays + drawings still render). Ephemeral —
    /// not persisted, defaults true.
    render_visible: bool,
    /// Persisted per-mode params for the Cluster render. Each footprint
    /// mode remembers its own settings — switching Cluster ↔ Profile does
    /// not bleed across — see the locked design in
    /// `project_footprint_v1_design`.
    cluster_params: FootprintParams,
    /// Persisted per-mode params for the Profile render.
    profile_params: FootprintParams,
}

/// Baseline captured at splitter mouse-down. The outer mouse-move handler
/// computes `new_height = start_height + (current_y - start_y)` and pushes
/// through `set_indicator_pane_height` (which clamps the floor to 60px).
#[derive(Clone, Copy)]
struct SplitterDrag {
    instance_id: InstanceId,
    start_y: f32,
    start_height: f32,
}

/// Captured by `switch_symbol` / `switch_timeframe` before they tear down
/// `self` via `*self = Self::new(...)`. Adopted back onto the fresh
/// `ChartState` so the user's render choice + per-mode params survive
/// data-side changes.
#[derive(Clone, Copy)]
struct RenderSettingsSnapshot {
    kind: RenderKind,
    visible: bool,
    cluster: FootprintParams,
    profile: FootprintParams,
}

impl ChartState {
    /// Fallback default symbol — used by `ContentPanel::new` when no persisted
    /// chart prefs are present and the symbols service is empty. The server
    /// only supports BTCUSDT today (see `SUPPORTED_SYMBOL`).
    pub fn default_symbol() -> &'static str {
        "BTCUSDT"
    }

    /// Timeframe used for a freshly-opened chart.
    pub fn default_timeframe() -> Timeframe {
        market_data::DEFAULT_TIMEFRAME
    }

    /// Replace `self` with a fresh state for `symbol` (keeping the current
    /// timeframe), but only if `symbol` differs. Returns `true` if a switch
    /// happened (the caller can skip a redundant `cx.notify()`). Indicators
    /// carry over: the user's analytical setup follows the chart panel, not
    /// the data — see /grill-me locked design. Render kind + per-mode
    /// footprint params also carry over (the rendering mode is a user
    /// choice, not a per-symbol one).
    pub fn switch_symbol(&mut self, symbol: &str, candles: Vec<Candle>) -> bool {
        if self.symbol == symbol {
            return false;
        }
        let indicators = std::mem::take(&mut self.indicators);
        let render = self.snapshot_render_settings();
        *self = Self::new(symbol, self.timeframe, candles);
        self.adopt_indicators(indicators);
        self.adopt_render_settings(render);
        true
    }

    /// Replace `self` with a fresh state at `tf` (keeping symbol), but only
    /// if `tf` differs. Returns `true` if a switch happened. Indicators carry
    /// over (see `switch_symbol`); render kind + per-mode footprint params
    /// also carry over.
    pub fn switch_timeframe(&mut self, tf: Timeframe, candles: Vec<Candle>) -> bool {
        if self.timeframe == tf {
            return false;
        }
        let symbol = self.symbol.clone();
        let indicators = std::mem::take(&mut self.indicators);
        let render = self.snapshot_render_settings();
        *self = Self::new(symbol.as_ref(), tf, candles);
        self.adopt_indicators(indicators);
        self.adopt_render_settings(render);
        true
    }

    // ─────────────────────────── Render mode ───────────────────────────

    /// Currently-active render kind. Defaults `Candlestick`; switched via
    /// [`Self::switch_render`].
    pub fn render_kind(&self) -> RenderKind {
        self.render_kind
    }

    /// Eye-toggle state for the render chip. False suppresses the
    /// candle/cell/profile paint (overlays + drawings still render).
    pub fn render_visible(&self) -> bool {
        self.render_visible
    }

    pub fn set_render_visible(&mut self, visible: bool) {
        self.render_visible = visible;
    }

    /// Switch the active render kind. Returns `true` if it actually changed
    /// (caller can skip a redundant `cx.notify()` / sub re-allocation).
    ///
    /// Sub-lifecycle wiring (drop the old footprint sub, allocate a new one
    /// for the entered mode) happens at the [`crate::panels::ContentPanel`]
    /// layer — same pattern as `chart_sub_handles` for the candles channel.
    /// `ChartState` is purely state here.
    pub fn switch_render(&mut self, kind: RenderKind) -> bool {
        if self.render_kind == kind {
            return false;
        }
        self.render_kind = kind;
        true
    }

    pub fn cluster_params(&self) -> &FootprintParams {
        &self.cluster_params
    }

    pub fn profile_params(&self) -> &FootprintParams {
        &self.profile_params
    }

    /// Params for `kind`, or `None` for `Candlestick` (which has no params).
    pub fn params_for(&self, kind: RenderKind) -> Option<&FootprintParams> {
        match kind {
            RenderKind::Candlestick => None,
            RenderKind::Cluster => Some(&self.cluster_params),
            RenderKind::Profile => Some(&self.profile_params),
        }
    }

    /// Params for the active render, or `None` in Candlestick mode. Used by
    /// the paint pipeline branch and (later) the settings popover.
    pub fn active_footprint_params(&self) -> Option<&FootprintParams> {
        self.params_for(self.render_kind)
    }

    /// Mutate the Cluster params in place. The closure should return `true`
    /// if it changed a field that requires the caller to re-subscribe (i.e.
    /// the `bucket`); `false` for cosmetic-only edits. Caller (typically
    /// `ContentPanel`) acts on the return value to drop+reopen the
    /// footprint sub.
    pub fn update_cluster_params<F>(&mut self, f: F) -> bool
    where
        F: FnOnce(&mut FootprintParams) -> bool,
    {
        f(&mut self.cluster_params)
    }

    pub fn update_profile_params<F>(&mut self, f: F) -> bool
    where
        F: FnOnce(&mut FootprintParams) -> bool,
    {
        f(&mut self.profile_params)
    }

    /// Snapshot the render state (kind + both per-mode params + visibility)
    /// so `switch_symbol` / `switch_timeframe` can restore it onto the
    /// freshly-constructed `ChartState`. Visibility carries over too — the
    /// user's "hidden render" choice shouldn't reset on a symbol flip.
    fn snapshot_render_settings(&self) -> RenderSettingsSnapshot {
        RenderSettingsSnapshot {
            kind: self.render_kind,
            visible: self.render_visible,
            cluster: self.cluster_params,
            profile: self.profile_params,
        }
    }

    fn adopt_render_settings(&mut self, snap: RenderSettingsSnapshot) {
        self.render_kind = snap.kind;
        self.render_visible = snap.visible;
        self.cluster_params = snap.cluster;
        self.profile_params = snap.profile;
    }

    /// Direct setters used by `ContentPanel::new_restored` to seed
    /// persisted render state (`ChartPrefs.render_kind` / `cluster` /
    /// `profile`) onto a freshly-constructed `ChartState` without going
    /// through the switch_* / update_* path that may have side effects in
    /// later commits.
    pub fn seed_render(
        &mut self,
        kind: RenderKind,
        cluster: FootprintParams,
        profile: FootprintParams,
    ) {
        self.render_kind = kind;
        self.cluster_params = cluster;
        self.profile_params = profile;
    }

    pub fn timeframe(&self) -> Timeframe {
        self.timeframe
    }

    /// Reset both axes to their defaults: trailing-window viewport on x,
    /// auto-fit on y. Composition of `reset_x` + `reset_y_auto` so the
    /// context menu can do "both at once" without the user having to
    /// double-click each axis.
    pub fn reset_scale(&mut self) {
        self.reset_x();
        self.reset_y_auto();
    }

    pub fn symbol(&self) -> &SharedString {
        &self.symbol
    }

    /// Merge a WS tick into our buffer. Mirrors `MarketDataService::apply_tick`
    /// — the service is the source of truth, but each chart panel keeps its
    /// own copy so drawings stay anchored to bar indices across symbol
    /// switches. Mutates the last bar in place when `open_time` matches the
    /// tail; appends when it advances; drops out-of-order ticks.
    pub fn apply_tick(&mut self, candle: Candle, _is_closed: bool) {
        let appended = match self.candles.last_mut() {
            Some(last) if last.open_time == candle.open_time => {
                *last = candle;
                false
            }
            Some(last) if last.open_time < candle.open_time => {
                self.candles.push(candle);
                true
            }
            None => {
                self.candles.push(candle);
                true
            }
            Some(_) => {
                // Out-of-order tick (post-reconnect resync grace period). Ignore.
                false
            }
        };
        if appended && self.sticky_to_latest {
            self.view_start += 1.0;
            self.clamp();
        }
        // Every tick mutates the candle array — either the in-progress tail's
        // OHLC moves, or a new bar appends. Both shift indicator output, so we
        // recompute. Cost is sub-ms for v1 indicators × ~1000 bars.
        self.recompute_indicators();
    }

    /// Roll the chart forward to wall-clock when no live tick has arrived for
    /// the next bar yet. Each synthesized bar carries the previous close as
    /// O/H/L/C and zero volume — when a real tick lands it replaces the
    /// synthetic one through `apply_tick`'s open_time match. Returns true if
    /// any bar was appended (callers can skip a needless `cx.notify()` cost on
    /// the no-op path, though the countdown caller always notifies).
    ///
    /// Capped per call so a chart left open across off-hours doesn't fabricate
    /// thousands of empty bars — the reconnect / resnap path will fill the
    /// real gap when the user comes back.
    pub fn tick_clock(&mut self, now_ms: i64) -> bool {
        let dur = self.timeframe.duration_ms();
        if dur <= 0 {
            return false;
        }
        let Some(last) = self.candles.last().cloned() else {
            return false;
        };
        if now_ms <= last.close_time {
            return false;
        }
        const MAX_ROLL_PER_TICK: usize = 5;
        let mut prev = last;
        let mut added = 0;
        while now_ms > prev.close_time && added < MAX_ROLL_PER_TICK {
            let next_open = prev.open_time + dur;
            let next_close = next_open + dur - 1;
            let flat = Candle::new(
                next_open,
                next_close,
                prev.close,
                prev.close,
                prev.close,
                prev.close,
                0.0,
            );
            self.candles.push(flat.clone());
            prev = flat;
            added += 1;
        }
        if added > 0 && self.sticky_to_latest {
            self.view_start += added as f32;
            self.clamp();
        }
        if added > 0 {
            self.recompute_indicators();
        }
        added > 0
    }

    /// Re-seed `candles` from a fresh snapshot (initial backfill, or post-
    /// reconnect resync). Resets the viewport only if we had no prior data —
    /// otherwise the user's pan/zoom is preserved.
    pub fn resnap(&mut self, candles: Vec<Candle>) {
        let was_empty = self.candles.is_empty();
        self.candles = candles;
        if was_empty {
            let total = self.candles.len() as f32;
            self.view_size = crate::prefs::chart_default_view().min(total).max(1.0);
            self.view_start = if total > 0.0 {
                total - self.view_size * (1.0 - crate::prefs::chart_right_buffer())
            } else {
                0.0
            };
        } else if self.sticky_to_latest {
            // Sticky mode: re-anchor to the live edge of the fresh snapshot
            // (post-reconnect catch-up). User's `view_size` is preserved.
            let total = self.candles.len() as f32;
            if total > 0.0 {
                self.view_start = total - self.view_size * (1.0 - crate::prefs::chart_right_buffer());
                self.clamp();
            }
        }
        self.recompute_indicators();
    }

    /// True when the oldest loaded bar is within ~one viewport of the left edge
    /// — the cue to prefetch older history before the user hits the hard clamp.
    pub fn wants_older(&self) -> bool {
        self.view_start < self.view_size
    }

    /// Apply a prepend of `added` older bars: adopt the fresh (longer) snapshot
    /// and shift every index-anchored value right by `added` so the viewport and
    /// drawings stay on the same bars (no view reset).
    pub fn apply_prepend(&mut self, candles: Vec<Candle>, added: usize) {
        self.candles = candles;
        if added > 0 {
            self.shift_indices(added as f32);
        }
        self.recompute_indicators();
    }

    // ──────────────────────────── Indicators ────────────────────────────

    /// Read-only view of attached indicators (in render order). Chip rendering
    /// + paint pipeline iterate this; settings + picker use the mutators below.
    pub fn indicators(&self) -> &[IndicatorInstance] {
        &self.indicators
    }

    /// Integer bar index under the crosshair, or `None` when the cursor
    /// isn't over any pane (or hasn't been measured yet). Used by chip
    /// rendering to insert the indicator's value-at-cursor into the chip
    /// label. Reads from `cross_cursor_x` so sub-pane hover counts too —
    /// the bar grid is shared across panes.
    pub fn cursor_bar_index(&self) -> Option<usize> {
        let cx = self.cross_cursor_x?;
        let bounds = self.bounds?;
        let canvas_w = bounds.size.width.as_f32();
        let raw = screen_to_index(
            self.view_start,
            self.view_size,
            cx,
            canvas_w,
            self.y_axis_gap_px.get(),
        );
        if raw < 0.0 {
            return None;
        }
        let idx = raw.round() as usize;
        if idx >= self.candles.len() {
            return None;
        }
        Some(idx)
    }

    /// Cached output for instance `id`, or `None` if the id is unknown.
    pub fn indicator_output(&self, id: InstanceId) -> Option<&IndicatorOutput> {
        let idx = self.indicators.iter().position(|i| i.id == id)?;
        self.indicator_outputs.get(idx)
    }

    /// Add a freshly-spawned kind, auto-picking the next palette slot from
    /// the per-kind rotation. Returns the new instance's id.
    pub fn add_indicator(&mut self, kind: Box<dyn IndicatorKind>) -> InstanceId {
        let kind_id = kind.kind_id();
        let count = self
            .indicators
            .iter()
            .filter(|i| i.kind_id == kind_id)
            .count();
        let color = palette_color_for(count);
        let instance = IndicatorInstance::new(kind, color);
        let id = instance.id;
        let output = instance.kind.compute(&self.candles);
        self.indicators.push(instance);
        self.indicator_outputs.push(output);
        id
    }

    /// Drop the instance with the given id, if it exists. No-op otherwise.
    pub fn remove_indicator(&mut self, id: InstanceId) {
        if let Some(idx) = self.indicators.iter().position(|i| i.id == id) {
            self.indicators.remove(idx);
            self.indicator_outputs.remove(idx);
            self.pane_bounds.remove(&id);
            if let Some((sub_id, _, _)) = self.sub_cursor {
                if sub_id == id {
                    self.sub_cursor = None;
                }
            }
        }
    }

    /// Mutate an instance's `kind` in place via a closure, then recompute
    /// just that one's output. Used by the settings panel for live-apply
    /// edits — the closure typically downcasts `kind.as_any_mut()` to the
    /// concrete params type and mutates fields. Returns true if `id` was
    /// found.
    pub fn update_indicator<F>(&mut self, id: InstanceId, f: F) -> bool
    where
        F: FnOnce(&mut Box<dyn IndicatorKind>),
    {
        let Some(idx) = self.indicators.iter().position(|i| i.id == id) else {
            return false;
        };
        f(&mut self.indicators[idx].kind);
        // `kind.kind_id()` might change if the closure swaps the box, but
        // for downcast-in-place edits it's stable. Refresh the mirrored
        // copy for safety.
        self.indicators[idx].kind_id = self.indicators[idx].kind.kind_id();
        // The mutation may have changed the kind's color-slot count
        // (e.g., adding or removing an MA Suite entry). Resize the
        // instance's per-slot color Vec so paint and the settings UI
        // see a consistent shape.
        self.indicators[idx].sync_colors();
        let new_output = self.indicators[idx].kind.compute(&self.candles);
        self.indicator_outputs[idx] = new_output;
        true
    }

    /// Swap in a new kind box for an existing instance (used by the settings
    /// panel when params change). Preserves placement, pane_height, color,
    /// and hidden state; recomputes just that one's output. Returns true if
    /// the id was found.
    pub fn replace_indicator_kind(&mut self, id: InstanceId, kind: Box<dyn IndicatorKind>) -> bool {
        let Some(idx) = self.indicators.iter().position(|i| i.id == id) else {
            return false;
        };
        let inst = &mut self.indicators[idx];
        inst.kind_id = kind.kind_id();
        inst.kind = kind;
        let new_output = inst.kind.compute(&self.candles);
        self.indicator_outputs[idx] = new_output;
        true
    }

    /// Toggle the hidden flag (eye icon / context-menu Hide). Returns the
    /// new state if the id was found.
    pub fn set_indicator_hidden(&mut self, id: InstanceId, hidden: bool) -> Option<bool> {
        let inst = self.indicators.iter_mut().find(|i| i.id == id)?;
        inst.hidden = hidden;
        Some(hidden)
    }

    /// Update the pane height (called when the user drags the splitter).
    /// Clamps to the v1 spec's 60px floor.
    pub fn set_indicator_pane_height(&mut self, id: InstanceId, h: f32) {
        if let Some(inst) = self.indicators.iter_mut().find(|i| i.id == id) {
            if inst.pane_height.is_some() {
                inst.pane_height = Some(h.max(60.0));
            }
        }
    }

    /// Toggle a hybrid-kind instance between overlay and pane placement
    /// (Volume's settings toggle). Sets/clears `pane_height` to match.
    pub fn set_indicator_placement(&mut self, id: InstanceId, placement: Placement) {
        if let Some(inst) = self.indicators.iter_mut().find(|i| i.id == id) {
            inst.placement = placement;
            inst.pane_height = match placement {
                Placement::Pane => {
                    Some(crate::indicators::default_pane_height(inst.kind_id))
                }
                Placement::Overlay => None,
            };
        }
    }

    /// Set the color for a specific slot on an instance. Slot 0 is the
    /// primary line; further slots match `kind.color_slots()` order
    /// (e.g., MACD slot 1 = signal line). Out-of-bounds slots are a no-op
    /// — paint reads with the same bounds so an out-of-range index would
    /// just never be drawn anyway.
    pub fn set_indicator_color(&mut self, id: InstanceId, slot: usize, color: Hsla) {
        if let Some(inst) = self.indicators.iter_mut().find(|i| i.id == id) {
            if let Some(slot_color) = inst.colors.get_mut(slot) {
                *slot_color = color;
            }
        }
    }

    /// Reorder a sub-pane indicator by `delta` positions among the pane
    /// instances (delta = -1 → move up, +1 → move down). Overlay indicators
    /// are ignored (they don't participate in pane reorder). Clamps at edges.
    pub fn move_indicator_pane(&mut self, id: InstanceId, delta: i32) {
        let Some(idx) = self.indicators.iter().position(|i| i.id == id) else {
            return;
        };
        if self.indicators[idx].placement != Placement::Pane {
            return;
        }
        // Collect the positions of all pane instances, in order. The
        // reorder happens within that subsequence — overlay indicators
        // keep their slots in the underlying Vec.
        let pane_positions: Vec<usize> = self
            .indicators
            .iter()
            .enumerate()
            .filter(|(_, i)| i.placement == Placement::Pane)
            .map(|(p, _)| p)
            .collect();
        let Some(my_rank) = pane_positions.iter().position(|p| *p == idx) else {
            return;
        };
        let new_rank = (my_rank as i32 + delta).clamp(0, pane_positions.len() as i32 - 1) as usize;
        if new_rank == my_rank {
            return;
        }
        let target_pos = pane_positions[new_rank];
        // Swap via remove + insert so any overlay indicators between idx and
        // target_pos keep their relative slot.
        let instance = self.indicators.remove(idx);
        let output = self.indicator_outputs.remove(idx);
        let insert_at = if new_rank > my_rank {
            target_pos
        } else {
            target_pos
        };
        self.indicators.insert(insert_at, instance);
        self.indicator_outputs.insert(insert_at, output);
    }

    /// Adopt a saved indicator list (used by `switch_*` to preserve the
    /// user's setup across symbol / timeframe changes). Drops
    /// whatever the freshly-constructed state seeded (e.g., default Volume),
    /// then recomputes against the new candle buffer.
    fn adopt_indicators(&mut self, indicators: Vec<IndicatorInstance>) {
        self.indicators = indicators;
        self.indicator_outputs.clear();
        self.recompute_indicators();
    }

    /// Full recompute over the current `candles`. Cheap by v1 specs (~5
    /// indicators × ~1000 bars × sub-µs per op). Called after every tick,
    /// fabrication, snapshot, prepend, or instance edit.
    fn recompute_indicators(&mut self) {
        if self.indicators.len() != self.indicator_outputs.len() {
            self.indicator_outputs
                .resize_with(self.indicators.len(), || IndicatorOutput::Line(Vec::new()));
        }
        for (i, inst) in self.indicators.iter().enumerate() {
            self.indicator_outputs[i] = inst.kind.compute(&self.candles);
        }
    }

    /// Shift all candle-index-space state right by `n`. Committed drawings live
    /// in the workspace [`DrawingService`](crate::drawings::service) anchored
    /// to absolute ms, so prepended bars don't require shifting them; only
    /// chart-local ephemeral state (viewport + in-flight create/edit/text)
    /// needs the adjustment.
    fn shift_indices(&mut self, n: f32) {
        self.view_start += n;
        if let Some(c) = &mut self.creating {
            c.shift_x(n);
        }
        if let Some(t) = &mut self.editing_text {
            t.anchor.0 += n;
        }
        if let Some(ed) = &mut self.edit_drag {
            ed.baseline.shift_x(n);
            ed.anchor_world.0 += n;
        }
        if let Some(d) = &mut self.drag_anchor {
            d.start_view_start += n;
        }
        if let Some(a) = &mut self.x_axis_drag_anchor {
            // `.2` is `view_start_at_down` (see field doc).
            a.2 += n;
        }
    }

    /// Build a chart for `symbol` at `timeframe`. The initial bar buffer is
    /// the snapshot from `MarketDataService` (possibly empty if backfill
    /// hasn't completed yet — `Resnap` then fills it in). Display
    /// name/exchange are resolved at render time from the symbols service,
    /// so they aren't stored here.
    pub fn new(symbol: &str, timeframe: Timeframe, candles: Vec<Candle>) -> Self {
        let total = candles.len() as f32;
        let view_size = crate::prefs::chart_default_view().min(total).max(1.0);
        // Default view: latest candle anchored at the right edge of the
        // populated zone, with `crate::prefs::chart_right_buffer() * view_size` candles
        // of empty space past it. `view_start = total - view_size * (1 -
        // right_buffer_ratio)` ⇒ right_edge = total + buffer_in_candles.
        let view_start = if total > 0.0 {
            total - view_size * (1.0 - crate::prefs::chart_right_buffer())
        } else {
            0.0
        };
        let mut state = Self {
            symbol: SharedString::from(symbol.to_string()),
            timeframe,
            candles,
            view_start,
            view_size,
            drag_anchor: None,
            bounds: None,
            y_auto: true,
            y_min: 0.0,
            y_max: 0.0,
            y_drag_anchor: None,
            x_axis_drag_anchor: None,
            creating: None,
            edit_drag: None,
            editing_text: None,
            cursor: None,
            y_axis_gap_px: std::cell::Cell::new(52.0),
            sticky_to_latest: false,
            indicators: Vec::new(),
            indicator_outputs: Vec::new(),
            splitter_drag: None,
            cross_cursor_x: None,
            sub_cursor: None,
            pane_bounds: std::collections::HashMap::new(),
            indicators_collapsed: false,
            render_kind: RenderKind::default(),
            render_visible: true,
            cluster_params: FootprintParams::cluster_default(),
            profile_params: FootprintParams::profile_default(),
        };
        // Every fresh chart is born with a Volume overlay. `switch_*`
        // callers preserve the user's indicator list, so this seeding only
        // takes effect on truly-new ChartStates.
        state.add_indicator(Box::new(VolumeParams::default()));
        state
    }

    fn clamp(&mut self) {
        let total = self.candles.len() as f32;
        self.view_size = self
            .view_size
            .clamp(CHART_MIN_VIEW.min(total), CHART_MAX_VIEW.min(total));
        // Right buffer: view_start may push past `total - view_size` by
        // `view_size * RIGHT_BUFFER_RATIO`, leaving an empty zone where future
        // bars would appear. Left buffer: view_start may go negative by
        // `view_size * LEFT_BUFFER_RATIO` so wheel-zoom-out near bar 0 still
        // works with the right-edge anchor invariant.
        let max_start = total - self.view_size * (1.0 - crate::prefs::chart_right_buffer());
        let min_start = -self.view_size * CHART_LEFT_BUFFER_RATIO;
        self.view_start = self.view_start.clamp(min_start, max_start);
    }

    /// Borrowed view of the candles currently inside the viewport. Used by
    /// hot paths (`auto_y_range`, `render`) that previously cloned a `Vec`
    /// every call — at default `view_size = 60` and `y_auto = true`, the
    /// render path was hitting this 5×/frame, which under continuous
    /// repaint (the bottom-bar's animation-frame loop) added up enough to
    /// matter. Keep `Vec`-returning variants only for callers that genuinely
    /// need ownership.
    fn visible_slice(&self) -> &[Candle] {
        let start = self.view_start.max(0.0).floor() as usize;
        let take = self.view_size.ceil() as usize;
        let end = (start + take).min(self.candles.len());
        &self.candles[start..end]
    }

    /// Borrowed slice of candles that *paint* in the current viewport,
    /// together with the absolute index of the first candle. Used by the
    /// custom candle-paint pass which needs each candle's absolute index to
    /// compute its continuous center-x via `index_to_screen`. Returns one
    /// extra candle on the right so a candle whose body is partially
    /// clipped at the right edge during pan still paints — `visible_slice`
    /// is stricter and is reserved for the y-range auto-fit scan.
    fn paint_slice(&self) -> (usize, &[Candle]) {
        let total = self.candles.len();
        let start = self.view_start.floor().max(0.0) as usize;
        let end_target = (self.view_start + self.view_size).ceil().max(0.0) as usize + 1;
        let end = end_target.min(total);
        let start = start.min(end);
        (start, &self.candles[start..end])
    }

    /// Sampling interval (milliseconds) between consecutive candles, taken
    /// directly from the selected timeframe. Drives the x-axis step picker.
    fn candle_interval_ms(&self) -> i64 {
        self.timeframe.duration_ms()
    }

    /// Auto-fit price range from the visible candles. Returned `(min, max)`
    /// with a small padding so candles don't touch the chart edges. Padding
    /// is user-tunable via Settings → General → Chart → Price-axis padding;
    /// default is 5%.
    fn auto_y_range(&self) -> (f64, f64) {
        let mut lo = f64::INFINITY;
        let mut hi = f64::NEG_INFINITY;
        for c in self.visible_slice() {
            lo = lo.min(c.low);
            hi = hi.max(c.high);
        }
        if !lo.is_finite() || !hi.is_finite() || hi <= lo {
            return (0.0, 1.0);
        }
        let pad = (hi - lo) * crate::prefs::chart_y_padding() as f64;
        (lo - pad, hi + pad)
    }

    /// Lock the price axis to the current auto-fit range. Called the moment
    /// the user starts manipulating the right axis so subsequent drag/wheel
    /// moves work from a stable baseline instead of fighting auto-fit.
    fn freeze_y_if_auto(&mut self) {
        if self.y_auto {
            let (lo, hi) = self.auto_y_range();
            self.y_min = lo;
            self.y_max = hi;
            self.y_auto = false;
        }
    }

    fn reset_y_auto(&mut self) {
        self.y_auto = true;
        self.y_drag_anchor = None;
    }

    /// Reset the time axis to the default trailing window (most recent
    /// `crate::prefs::chart_default_view()` candles, with the standard right-side buffer
    /// pushing the latest candle to ~80% of width). Used by double-click on
    /// the bottom axis.
    fn reset_x(&mut self) {
        let total = self.candles.len() as f32;
        self.view_size = crate::prefs::chart_default_view().min(total);
        self.view_start = total - self.view_size * (1.0 - crate::prefs::chart_right_buffer());
        self.x_axis_drag_anchor = None;
    }

    /// Pan horizontally so the latest bar lands at the default trailing
    /// offset (~60% from left, with the standard right-buffer past it), and
    /// turn on sticky-tail mode so subsequent new bars keep the chart
    /// pinned to the live edge. Preserves `view_size` (the user's zoom);
    /// re-enables y-axis auto-fit. Sticky is cleared by any user canvas
    /// pan that moves `view_start`.
    pub fn snap_to_latest(&mut self) {
        let total = self.candles.len() as f32;
        if total > 0.0 {
            self.view_start = total - self.view_size * (1.0 - crate::prefs::chart_right_buffer());
            self.clamp();
        }
        self.reset_y_auto();
        self.sticky_to_latest = true;
    }

    /// True when the most recent candle has scrolled off the right edge of
    /// the viewport (`latest_idx >= view_start + view_size`). Drives the
    /// floating "Go to latest" overlay button.
    pub fn latest_off_right(&self) -> bool {
        let total = self.candles.len() as f32;
        total > 0.0 && (total - 1.0) >= self.view_start + self.view_size
    }

    /// The y range currently rendered. Reads from auto-fit when `y_auto`,
    /// otherwise the locked range. Drawings convert prices to pixels via this
    /// — and on this frame's auto-fit if relevant — so they sit visually next
    /// to the candles they were anchored to.
    fn y_range(&self) -> (f64, f64) {
        if self.y_auto {
            self.auto_y_range()
        } else {
            (self.y_min, self.y_max)
        }
    }
}

/// Width of the y-axis label gutter for a given price range. Labels paint
/// with `format!("{:.2}", value)` at px(10), so the widest label width
/// drives the gutter. Clamped so the gutter never collapses (small prices)
/// nor steals the whole chart (anomalous ranges).
pub(super) fn compute_y_axis_gap(y_lo: f64, y_hi: f64) -> f32 {
    let widest = y_lo.abs().max(y_hi.abs());
    if !widest.is_finite() {
        return 52.0;
    }
    let label = format!("{:.2}", widest);
    // Each character ~6.5 px at the px(10) font size used in paint, plus
    // 14 px combined left+right padding so labels don't kiss the chart.
    (label.len() as f32 * 6.5 + 14.0).clamp(44.0, 120.0)
}

/// Convert a screen-space x pixel (relative to canvas origin) to a fractional
/// candle index. The chart paints candles inside `(0, width - y_axis_gap)`,
/// so we exclude the right-side label gutter from the mapping. The `-0.5`
/// offset aligns the click to the *centre* of each candle slot, matching
/// where `paint_main_chart` places each candle's body (centred within its
/// slot of width `chart_w / view_size`).
fn screen_to_index(
    view_start: f32,
    view_size: f32,
    x_in_canvas: f32,
    canvas_width: f32,
    y_axis_gap: f32,
) -> f32 {
    let chart_w = (canvas_width - y_axis_gap).max(1.0);
    view_start + (x_in_canvas / chart_w) * view_size - 0.5
}

/// Inverse of `screen_to_index`. The `+ 0.5` mirrors the centre-of-slot
/// alignment so drawings anchored at integer candle indices render where
/// the candle body actually paints.
fn index_to_screen(
    view_start: f32,
    view_size: f32,
    index: f32,
    canvas_width: f32,
    y_axis_gap: f32,
) -> f32 {
    let chart_w = (canvas_width - y_axis_gap).max(1.0);
    (index - view_start + 0.5) / view_size * chart_w
}

/// Render a single indicator chip. Used for both the main-pane vertical
/// list (overlay indicators only) and each sub-pane's solo chip overlay
/// (the pane's lone indicator). Layout: `[label] [● eye] [⚙ gear] [× trash]`.
///
/// Body has no `cursor_pointer` and no left-click handler — settings is
/// only reachable via the gear button or the right-click "Settings…" item.
/// A subtle hover bg tint marks the chip as an interactive surface so the
/// right-click affordance is discoverable.
fn render_indicator_chip(
    inst: &IndicatorInstance,
    output: &IndicatorOutput,
    cursor_idx: Option<usize>,
    cx: &mut Context<super::ContentPanel>,
) -> gpui::AnyElement {
    let id = inst.id;
    let hidden = inst.hidden;
    let is_pane = inst.placement == Placement::Pane;
    // Chip label = "Name" when crosshair is off-canvas, or
    // "Name: v1[ / v2[ / v3]]" when the crosshair is over a bar.
    // `kind.value_at` returns a typed ValueReadout that's formatted here so
    // the chip always reads cleanly regardless of how many series the
    // indicator emits.
    let base_label = inst.kind.label();
    let label: SharedString = match cursor_idx {
        Some(i) => SharedString::from(format!(
            "{}: {}",
            base_label,
            format_readout(inst.kind.value_at(output, i))
        )),
        None => base_label,
    };
    let chip_id = SharedString::from(format!("chip-{}", id));
    let eye_id = SharedString::from(format!("chip-eye-{}", id));
    let gear_id = SharedString::from(format!("chip-gear-{}", id));
    let close_id = SharedString::from(format!("chip-close-{}", id));
    // Neutral theme colors — the indicator's series color shows in the
    // pane paint itself, so the chip doesn't need to repeat it. Hidden
    // chips dim to muted_foreground; visible chips use foreground.
    let (text_color, border_color) = {
        let theme = cx.theme();
        if hidden {
            (theme.muted_foreground, Hsla { a: 0.45, ..theme.border })
        } else {
            (theme.foreground, theme.border)
        }
    };
    // Eye glyph: filled circle visible, hollow circle hidden. Same width so
    // the chip doesn't reflow when the user toggles visibility.
    let eye_label: SharedString = if hidden {
        SharedString::from("\u{25CB}") // ○
    } else {
        SharedString::from("\u{25CF}") // ●
    };
    let hover_bg = {
        let muted = cx.theme().muted;
        Hsla { a: 0.30, ..muted }
    };
    h_flex()
        .id(chip_id)
        .gap_1()
        .px_2()
        .py(px(2.))
        .items_center()
        .rounded(px(4.))
        .border_1()
        .border_color(border_color)
        .text_xs()
        .text_color(text_color)
        // Occlude the chart canvas's hitbox underneath: right-clicking
        // the chip should open only the chip's own context menu, not
        // also the chart's. `gpui-component`'s `context_menu` primitive
        // works off `window.on_mouse_event` + `hitbox.is_hovered`, so
        // event-level `stop_propagation` doesn't help — `.occlude()` is
        // the supported mechanism for marking hitboxes behind this
        // element as not-hovered. Same trick suppresses ghost-hover
        // styling on the canvas while the cursor is over a chip.
        .occlude()
        // Subtle hover tint so the right-click affordance is discoverable.
        // No cursor_pointer — the body itself is not clickable; only the
        // three buttons (which set their own pointer) are.
        .hover(move |this| this.bg(hover_bg))
        // Right-click menu: Settings… (always), Hide/Show (always),
        // Move pane up/down (Pane-placed only — overlay indicators have no
        // pane order to reshuffle), Remove (always). Actions are scoped to
        // this ContentPanel via the chip's element tree.
        .context_menu(move |menu, _, _| {
            let mut m = menu
                .menu(
                    "Settings…",
                    Box::new(crate::indicator_settings::OpenIndicatorSettings(id)),
                )
                .separator()
                .menu("Hide / Show", Box::new(ToggleIndicatorHidden(id)));
            if is_pane {
                m = m
                    .menu("Move pane up", Box::new(MoveIndicatorPaneUp(id)))
                    .menu("Move pane down", Box::new(MoveIndicatorPaneDown(id)));
            }
            m.separator()
                .menu("Remove", Box::new(RemoveIndicator(id)))
        })
        .child(div().child(label))
        .child(
            Button::new(eye_id)
                .label(eye_label)
                .xsmall()
                .ghost()
                .on_click(cx.listener(move |this, _ev, _w, cx| {
                    if let Some(chart) = this.chart_state.as_mut() {
                        let was_hidden = chart
                            .indicators()
                            .iter()
                            .find(|i| i.id == id)
                            .map(|i| i.hidden)
                            .unwrap_or(false);
                        chart.set_indicator_hidden(id, !was_hidden);
                        cx.notify();
                    }
                })),
        )
        .child(
            Button::new(gear_id)
                .label(SharedString::from("\u{2699}")) // ⚙
                .xsmall()
                .ghost()
                .on_click(move |_ev, window, cx| {
                    window.dispatch_action(
                        Box::new(crate::indicator_settings::OpenIndicatorSettings(id)),
                        cx,
                    );
                }),
        )
        .child(
            Button::new(close_id)
                .label(SharedString::from("\u{00d7}")) // ×
                .xsmall()
                .ghost()
                .on_click(cx.listener(move |this, _ev, _w, cx| {
                    if let Some(chart) = this.chart_state.as_mut() {
                        chart.remove_indicator(id);
                        cx.notify();
                    }
                })),
        )
        .into_any_element()
}

/// Build chip elements for indicators matching `pred`, in render order.
/// Used by both the main-pane vertical list (overlay-only) and the
/// sub-pane chip overlays (each pane's lone pane-placed indicator).
fn render_indicator_chips_filtered(
    state: &ChartState,
    cx: &mut Context<super::ContentPanel>,
    pred: impl Fn(&IndicatorInstance) -> bool,
) -> Vec<gpui::AnyElement> {
    let cursor_idx = state.cursor_bar_index();
    state
        .indicators()
        .iter()
        .zip(state.indicator_outputs.iter())
        .filter(|(inst, _)| pred(inst))
        .map(|(inst, output)| render_indicator_chip(inst, output, cursor_idx, cx))
        .collect()
}

/// Render the main-pane vertical indicator list — a header chip
/// `Indicators (N) ▼/▶` that toggles `state.indicators_collapsed`, plus a
/// stack of overlay-indicator chips below it when expanded.
///
/// Pane-placed indicators are NOT included here; they each get their own
/// chip rendered at the top-left of their sub-pane (`render_sub_pane_chip`).
/// Positioned by the caller — typically absolute at the main canvas's
/// top-left in the drawings-overlay layer.
fn render_main_indicator_list(
    state: &ChartState,
    cx: &mut Context<super::ContentPanel>,
) -> gpui::AnyElement {
    let collapsed = state.indicators_collapsed;
    let overlay_count = state
        .indicators()
        .iter()
        .filter(|i| i.placement == Placement::Overlay)
        .count();
    let chevron = if collapsed { "\u{25B6}" } else { "\u{25BC}" }; // ▶ / ▼
    let header_label = SharedString::from(format!("Indicators ({}) {}", overlay_count, chevron));
    let (theme_border, theme_muted_fg, theme_bg, hover_bg) = {
        let theme = cx.theme();
        (
            theme.border,
            theme.muted_foreground,
            theme.background,
            Hsla { a: 0.30, ..theme.muted },
        )
    };
    let header = h_flex()
        .id(SharedString::from("indicator-list-header"))
        .gap_1()
        .px_2()
        .py(px(2.))
        .items_center()
        .rounded(px(4.))
        .border_1()
        .border_color(theme_border)
        .bg(theme_bg)
        .text_xs()
        .text_color(theme_muted_fg)
        .cursor_pointer()
        .hover(move |this| this.bg(hover_bg))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|this, _ev, _w, cx| {
                if let Some(chart) = this.chart_state.as_mut() {
                    chart.indicators_collapsed = !chart.indicators_collapsed;
                    cx.notify();
                }
            }),
        )
        .child(div().child(header_label));
    let chips = if collapsed {
        Vec::new()
    } else {
        render_indicator_chips_filtered(state, cx, |i| i.placement == Placement::Overlay)
    };
    // Absolute-anchored at the main canvas's top-left (mirrors the
    // pre-move OHLC pill's `top(8) left(8)`). `items_start` so each chip
    // auto-sizes to its content rather than stretching to fill the longest.
    v_flex()
        .absolute()
        .top(px(8.0))
        .left(px(8.0))
        .gap_1()
        .items_start()
        .child(header)
        .children(chips)
        .into_any_element()
}

/// Format an indicator's `ValueReadout` for the chip label. `None` slots
/// render as `—` so a no-history bar still reads cleanly. Volume values
/// use the K/M/B abbreviations standard in trading platforms; everything
/// else gets two decimals.
fn format_readout(r: ValueReadout) -> String {
    match r {
        ValueReadout::One(v) => fmt_scalar(v),
        ValueReadout::Two(a, b) => format!("{} / {}", fmt_scalar(a), fmt_scalar(b)),
        ValueReadout::Three(a, b, c) => {
            format!("{} / {} / {}", fmt_scalar(a), fmt_scalar(b), fmt_scalar(c))
        }
        ValueReadout::Many(vs) => {
            if vs.is_empty() {
                "\u{2014}".to_string()
            } else {
                vs.into_iter()
                    .map(fmt_scalar)
                    .collect::<Vec<_>>()
                    .join(" / ")
            }
        }
    }
}

fn fmt_scalar(v: Option<f64>) -> String {
    match v {
        None => "\u{2014}".to_string(),
        Some(v) if v.abs() >= 1_000_000_000.0 => format!("{:.2}B", v / 1_000_000_000.0),
        Some(v) if v.abs() >= 1_000_000.0 => format!("{:.2}M", v / 1_000_000.0),
        Some(v) if v.abs() >= 10_000.0 => format!("{:.1}K", v / 1_000.0),
        Some(v) => format!("{:.2}", v),
    }
}

/// Pixel y for a given price. `paint_main_chart` paints prices into the
/// band `[10, canvas_height - AXIS_GAP]`; drawings and overlay chrome use
/// this same function so they sit next to the candles they anchor against.
fn price_to_screen(y_lo: f64, y_hi: f64, price: f64, canvas_height: f32) -> f32 {
    let top = 10.0_f32;
    let bottom = (canvas_height - AXIS_GAP).max(top + 1.0);
    let range = y_hi - y_lo;
    if range.abs() < 1e-9 {
        return (top + bottom) / 2.0;
    }
    let t = ((y_hi - price) / range) as f32;
    top + t * (bottom - top)
}

fn screen_to_price(y_lo: f64, y_hi: f64, y_in_canvas: f32, canvas_height: f32) -> f64 {
    let top = 10.0_f32;
    let bottom = (canvas_height - AXIS_GAP).max(top + 1.0);
    let t = ((y_in_canvas - top) / (bottom - top)).clamp(-2.0, 2.0) as f64;
    y_hi - t * (y_hi - y_lo)
}

/// Snap a fractional candle index to its integer slot. Per Q10a, x-anchors
/// always snap so drawings sit on candle centres.
fn snap_t(t: f32) -> f32 {
    t.round()
}

/// Estimate the rendered pixel height of a text-drawing box of given pixel
/// `width` containing `text`. Matches the visual extent of the
/// `text_xs() / px_1p5() / py_0p5() / border_1` div used in the render path
/// so hit-test bounds line up with what the user sees.
fn estimate_text_box_height(text: &str, width: f32) -> f32 {
    // text_xs: ~12px font, ~18px line height. Padding totals ~4px vertical
    // (py_0p5 each side = 2px) + ~2px for the 1px border each side.
    const LINE_HEIGHT: f32 = 18.0;
    const VERTICAL_PADDING_AND_BORDER: f32 = 6.0;
    // Horizontal padding (px_1p5 = 6px each side = 12px) eats into the
    // content width.
    const HORIZONTAL_PADDING: f32 = 12.0;
    // Approximate proportional char width at 12px font; this is a soft
    // estimate, so erring slightly large is preferable — undershooting
    // makes the hit area smaller than the visible text.
    const AVG_CHAR_WIDTH: f32 = 6.5;

    let content_w = (width - HORIZONTAL_PADDING).max(AVG_CHAR_WIDTH);
    let chars_per_line = (content_w / AVG_CHAR_WIDTH).floor().max(1.0);
    // Count wrapped lines per paragraph (split by '\n') so multi-line text
    // doesn't under-report when the user typed explicit breaks.
    let mut total_lines = 0.0f32;
    let mut had_content = false;
    for paragraph in text.split('\n') {
        had_content = true;
        let chars = paragraph.chars().count().max(1) as f32;
        let para_lines = (chars / chars_per_line).ceil().max(1.0);
        total_lines += para_lines;
    }
    if !had_content {
        total_lines = 1.0;
    }
    total_lines * LINE_HEIGHT + VERTICAL_PADDING_AND_BORDER
}

/// Round every time-anchor in a view-coord [`Drawing`] to the nearest integer
/// candle slot. Used at edit-drag commit so a drawing dragged on TF B
/// (regardless of where its original anchors were exact) ends up flush with
/// TF B's candle grid.
fn snap_view_to_grid(view: &mut Drawing) {
    match view {
        Drawing::Line { a, b, .. }
        | Drawing::Rect { a, b, .. }
        | Drawing::Arrow { a, b, .. }
        | Drawing::Fibonacci { a, b, .. } => {
            a.0 = a.0.round();
            b.0 = b.0.round();
        }
        Drawing::Text { anchor, .. }
        | Drawing::HorizontalRay { anchor, .. }
        | Drawing::AnchoredVwap { anchor, .. } => {
            anchor.0 = anchor.0.round();
        }
        Drawing::Long { t0, t1, .. } | Drawing::Short { t0, t1, .. } => {
            *t0 = t0.round();
            *t1 = t1.round();
        }
    }
}

const DRAWING_HANDLE_HIT_PX: f32 = 8.0;
const DRAWING_STROKE_HIT_PX: f32 = 6.0;

fn point_to_segment_dist(ax: f32, ay: f32, bx: f32, by: f32, px: f32, py: f32) -> f32 {
    let abx = bx - ax;
    let aby = by - ay;
    let apx = px - ax;
    let apy = py - ay;
    let len2 = abx * abx + aby * aby;
    if len2 < 1e-6 {
        return (apx * apx + apy * apy).sqrt();
    }
    let t = ((apx * abx + apy * aby) / len2).clamp(0.0, 1.0);
    let cx = ax + t * abx;
    let cy = ay + t * aby;
    let dx = px - cx;
    let dy = py - cy;
    (dx * dx + dy * dy).sqrt()
}

impl Drawing {
    /// Translate all anchors in world coords.
    fn translate(&mut self, dt: f32, dp: f64) {
        match self {
            Drawing::Line { a, b, .. }
            | Drawing::Rect { a, b, .. }
            | Drawing::Arrow { a, b, .. }
            | Drawing::Fibonacci { a, b, .. } => {
                a.0 += dt;
                a.1 += dp;
                b.0 += dt;
                b.1 += dp;
            }
            Drawing::Text { anchor, .. } | Drawing::HorizontalRay { anchor, .. } => {
                anchor.0 += dt;
                anchor.1 += dp;
            }
            // AnchoredVwap: only the time component is meaningful; the y-axis
            // movement is intentionally ignored so vertical drags don't shift
            // an anchor away from its bar.
            Drawing::AnchoredVwap { anchor, .. } => {
                anchor.0 += dt;
            }
            Drawing::Long {
                t0,
                t1,
                entry,
                take_profit,
                stop_loss,
                ..
            }
            | Drawing::Short {
                t0,
                t1,
                entry,
                take_profit,
                stop_loss,
                ..
            } => {
                *t0 += dt;
                *t1 += dt;
                *entry += dp;
                *take_profit += dp;
                *stop_loss += dp;
            }
        }
    }

    /// Set the A or B endpoint of a Line/Rect/Arrow drawing. No-op on other
    /// variants — they have variant-specific handles wired in later steps.
    fn set_endpoint(&mut self, handle: EditHandle, pt: (f32, f64)) {
        match handle {
            EditHandle::EndpointA => match self {
                Drawing::Line { a, .. }
                | Drawing::Rect { a, .. }
                | Drawing::Arrow { a, .. }
                | Drawing::Fibonacci { a, .. } => *a = pt,
                Drawing::HorizontalRay { anchor, .. } => *anchor = pt,
                _ => {}
            },
            EditHandle::EndpointB => match self {
                Drawing::Line { b, .. }
                | Drawing::Rect { b, .. }
                | Drawing::Arrow { b, .. }
                | Drawing::Fibonacci { b, .. } => *b = pt,
                _ => {}
            },
            // Body + position handles handled outside (translate / apply_edit
            // do per-field mutations rather than going through endpoint
            // assignment).
            _ => {}
        }
    }

    /// Anchors in the order rendered, for both painting handles and hit
    /// testing. Empty for variants without endpoint handles (positions add
    /// their own set in their dedicated step).
    fn endpoint_anchors(&self) -> Vec<(EditHandle, (f32, f64))> {
        match self {
            Drawing::Line { a, b, .. }
            | Drawing::Rect { a, b, .. }
            | Drawing::Arrow { a, b, .. }
            | Drawing::Fibonacci { a, b, .. } => {
                vec![(EditHandle::EndpointA, *a), (EditHandle::EndpointB, *b)]
            }
            Drawing::HorizontalRay { anchor, .. } => {
                vec![(EditHandle::EndpointA, *anchor)]
            }
            _ => Vec::new(),
        }
    }
}

/// Apply an edit drag in-place: copy `baseline` into `out`, then move the
/// specified handle (or whole body) by the world-delta `(dt, dp)`. Position
/// handles ignore the dimension they don't own (e.g. dragging a TP price
/// only changes the price, not the time anchors).
fn apply_edit(out: &mut Drawing, baseline: &Drawing, handle: EditHandle, dt: f32, dp: f64) {
    *out = baseline.clone();
    match handle {
        EditHandle::Body => out.translate(dt, dp),
        EditHandle::EndpointA | EditHandle::EndpointB => {
            let baseline_pt = match baseline {
                Drawing::Line { a, b, .. }
                | Drawing::Rect { a, b, .. }
                | Drawing::Arrow { a, b, .. }
                | Drawing::Fibonacci { a, b, .. } => {
                    if handle == EditHandle::EndpointA {
                        *a
                    } else {
                        *b
                    }
                }
                // Horizontal ray only has the A anchor; B doesn't apply.
                Drawing::HorizontalRay { anchor, .. } if handle == EditHandle::EndpointA => *anchor,
                _ => return,
            };
            out.set_endpoint(handle, (baseline_pt.0 + dt, baseline_pt.1 + dp));
        }
        EditHandle::PositionEntry => {
            let base = match baseline {
                Drawing::Long { entry, .. } | Drawing::Short { entry, .. } => *entry,
                _ => return,
            };
            match out {
                Drawing::Long { entry, .. } | Drawing::Short { entry, .. } => {
                    *entry = base + dp;
                }
                _ => {}
            }
        }
        EditHandle::PositionTakeProfit => {
            let base = match baseline {
                Drawing::Long { take_profit, .. } | Drawing::Short { take_profit, .. } => {
                    *take_profit
                }
                _ => return,
            };
            match out {
                Drawing::Long { take_profit, .. } | Drawing::Short { take_profit, .. } => {
                    *take_profit = base + dp;
                }
                _ => {}
            }
        }
        EditHandle::PositionStopLoss => {
            let base = match baseline {
                Drawing::Long { stop_loss, .. } | Drawing::Short { stop_loss, .. } => *stop_loss,
                _ => return,
            };
            match out {
                Drawing::Long { stop_loss, .. } | Drawing::Short { stop_loss, .. } => {
                    *stop_loss = base + dp;
                }
                _ => {}
            }
        }
        EditHandle::PositionStart => {
            let base = match baseline {
                Drawing::Long { t0, .. } | Drawing::Short { t0, .. } => *t0,
                _ => return,
            };
            match out {
                Drawing::Long { t0, .. } | Drawing::Short { t0, .. } => {
                    *t0 = base + dt;
                }
                _ => {}
            }
        }
        EditHandle::PositionEnd => {
            let base = match baseline {
                Drawing::Long { t1, .. } | Drawing::Short { t1, .. } => *t1,
                _ => return,
            };
            match out {
                Drawing::Long { t1, .. } | Drawing::Short { t1, .. } => {
                    *t1 = base + dt;
                }
                _ => {}
            }
        }
    }
}

/// Hit-test all drawings against a canvas-relative screen point. Returns the
/// topmost hit (drawings later in the vec are drawn last and thus on top).
fn hit_test_drawings(
    drawings: &[Drawing],
    view_start: f32,
    view_size: f32,
    y_lo: f64,
    y_hi: f64,
    canvas_w: f32,
    canvas_h: f32,
    y_axis_gap: f32,
    pt_x: f32,
    pt_y: f32,
) -> Option<(DrawingId, EditHandle)> {
    let to_screen = |w: (f32, f64)| {
        let x = index_to_screen(view_start, view_size, w.0, canvas_w, y_axis_gap);
        let y = price_to_screen(y_lo, y_hi, w.1, canvas_h);
        (x, y)
    };
    for d in drawings.iter().rev() {
        // Endpoint handles win over body — they're smaller hit targets but
        // user intent is unambiguous when clicking near a handle.
        for (handle, world) in d.endpoint_anchors() {
            let (hx, hy) = to_screen(world);
            let dx = pt_x - hx;
            let dy = pt_y - hy;
            if (dx * dx + dy * dy).sqrt() <= DRAWING_HANDLE_HIT_PX {
                return Some((d.id(), handle));
            }
        }
        match d {
            Drawing::Line { a, b, .. } | Drawing::Arrow { a, b, .. } => {
                let (ax, ay) = to_screen(*a);
                let (bx, by) = to_screen(*b);
                if point_to_segment_dist(ax, ay, bx, by, pt_x, pt_y) <= DRAWING_STROKE_HIT_PX {
                    return Some((d.id(), EditHandle::Body));
                }
            }
            Drawing::HorizontalRay { anchor, .. } => {
                // Hit if the point sits within stroke-hit-px of the ray's
                // y-line, anywhere to the right of the anchor x (clipped to
                // canvas).
                let (ax, ay) = to_screen(*anchor);
                if (pt_y - ay).abs() <= DRAWING_STROKE_HIT_PX && pt_x >= ax - DRAWING_HANDLE_HIT_PX
                {
                    return Some((d.id(), EditHandle::Body));
                }
            }
            Drawing::Rect { a, b, .. } | Drawing::Fibonacci { a, b, .. } => {
                let (ax, ay) = to_screen(*a);
                let (bx, by) = to_screen(*b);
                let (xmin, xmax) = (ax.min(bx), ax.max(bx));
                let (ymin, ymax) = (ay.min(by), ay.max(by));
                if pt_x >= xmin && pt_x <= xmax && pt_y >= ymin && pt_y <= ymax {
                    return Some((d.id(), EditHandle::Body));
                }
            }
            Drawing::Text {
                anchor,
                width,
                text,
                ..
            } => {
                // Box height is estimated to match the rendered div, which
                // uses `text_xs` (≈12px font, ≈18px line height) inside
                // `px_1p5().py_0p5()` padding (12px horizontal, 4px vertical
                // total). Count explicit newlines AND wrap each paragraph
                // against `content_width = width - 12`, then total
                // line count drives the height. Mirrors the natural-grow
                // behaviour of the rendered div so the hit area expands with
                // visible text rather than under-estimating long content.
                let (ax, ay) = to_screen(*anchor);
                let h_est = estimate_text_box_height(text, *width);
                // Right-edge resize handle: within `DRAWING_HANDLE_HIT_PX`
                // of the right edge, anywhere within the box's vertical
                // extent.
                if (pt_x - (ax + *width)).abs() <= DRAWING_HANDLE_HIT_PX
                    && pt_y >= ay - DRAWING_HANDLE_HIT_PX
                    && pt_y <= ay + h_est + DRAWING_HANDLE_HIT_PX
                {
                    return Some((d.id(), EditHandle::EndpointB));
                }
                if pt_x >= ax && pt_x <= ax + *width && pt_y >= ay && pt_y <= ay + h_est {
                    return Some((d.id(), EditHandle::Body));
                }
            }
            Drawing::Long {
                t0,
                t1,
                entry,
                take_profit,
                stop_loss,
                ..
            }
            | Drawing::Short {
                t0,
                t1,
                entry,
                take_profit,
                stop_loss,
                ..
            } => {
                let (x0, _) = to_screen((*t0, *entry));
                let (x1, _) = to_screen((*t1, *entry));
                let (xmin, xmax) = (x0.min(x1), x0.max(x1));
                let (_, y_entry) = to_screen((*t0, *entry));
                let (_, y_tp) = to_screen((*t0, *take_profit));
                let (_, y_sl) = to_screen((*t0, *stop_loss));
                let ymin = y_entry.min(y_tp).min(y_sl);
                let ymax = y_entry.max(y_tp).max(y_sl);

                // Time-edge handles: vertical edges at xmin / xmax within
                // the vertical extent of the rect. Checked FIRST so the
                // visual handle dots at `(xmin/xmax, y_entry)` resolve to
                // a width drag — without this, the price-line check below
                // would intercept the click as `PositionEntry`.
                let in_y_extent =
                    pt_y >= ymin - DRAWING_STROKE_HIT_PX && pt_y <= ymax + DRAWING_STROKE_HIT_PX;
                if in_y_extent {
                    if (pt_x - xmin).abs() <= DRAWING_STROKE_HIT_PX {
                        return Some((d.id(), EditHandle::PositionStart));
                    }
                    if (pt_x - xmax).abs() <= DRAWING_STROKE_HIT_PX {
                        return Some((d.id(), EditHandle::PositionEnd));
                    }
                }

                // Price-line handles: any click on a price line (within the
                // horizontal extent of the rect, plus a small margin) grabs
                // that line. Test TP first → SL → entry, so when lines are
                // stacked the user gets the most "intentful" pick.
                let in_x_extent =
                    pt_x >= xmin - DRAWING_STROKE_HIT_PX && pt_x <= xmax + DRAWING_STROKE_HIT_PX;
                if in_x_extent {
                    if (pt_y - y_tp).abs() <= DRAWING_STROKE_HIT_PX {
                        return Some((d.id(), EditHandle::PositionTakeProfit));
                    }
                    if (pt_y - y_sl).abs() <= DRAWING_STROKE_HIT_PX {
                        return Some((d.id(), EditHandle::PositionStopLoss));
                    }
                    if (pt_y - y_entry).abs() <= DRAWING_STROKE_HIT_PX {
                        return Some((d.id(), EditHandle::PositionEntry));
                    }
                }

                // Body: anywhere inside the rect that didn't hit a handle.
                if pt_x >= xmin && pt_x <= xmax && pt_y >= ymin && pt_y <= ymax {
                    return Some((d.id(), EditHandle::Body));
                }
            }
            Drawing::AnchoredVwap { anchor, .. } => {
                // Rough hit test: a vertical strip at the anchor x — enough
                // for the user to right-click and Delete / hide. Proper
                // polyline hit-testing would re-walk the candle buffer; v1
                // keeps it cheap.
                let (ax, _) = to_screen(*anchor);
                if (pt_x - ax).abs() <= DRAWING_HANDLE_HIT_PX {
                    return Some((d.id(), EditHandle::Body));
                }
            }
        }
    }
    None
}

pub fn render(
    state: &ChartState,
    focus: FocusHandle,
    _window: &mut Window,
    cx: &mut Context<ContentPanel>,
) -> impl IntoElement {
    // Extract every theme colour we need as Hsla (Copy) up front so the
    // `&Theme` borrow ends before we start chaining `cx.listener` calls below.
    // Otherwise the closure constructions further down trip E0500 against any
    // later reference to `theme.*`.
    let (
        theme_background,
        theme_border,
        theme_muted_foreground,
        theme_chart_bullish,
        theme_chart_bearish,
        theme_chart_5,
        theme_foreground,
        theme_ring,
    ) = {
        let theme = cx.theme();
        (
            theme.background,
            theme.border,
            theme.muted_foreground,
            theme.chart_bullish,
            theme.chart_bearish,
            theme.chart_5,
            theme.foreground,
            theme.ring,
        )
    };

    // LIVE / Reconnecting / Disconnected badge in the header. Mirrors the
    // market-data service's connection state.
    let (badge_color, badge_label): (Hsla, &'static str) = {
        let svc = cx
            .global::<market_data::MarketDataServiceHandle>()
            .0
            .clone();
        let status = svc
            .read(cx)
            .status(state.symbol.as_ref(), state.timeframe());
        use market_data::LiveStatus::*;
        match status {
            Connected => (theme_chart_bullish, "LIVE"),
            Connecting => (theme_chart_5, "Connecting…"),
            Reconnecting { attempts } if attempts >= 4 => (theme_chart_bearish, "Disconnected"),
            Reconnecting { .. } => (theme_chart_5, "Reconnecting…"),
        }
    };

    // Single `y_range` + `visible_slice` snapshot threaded through the rest
    // of render. Previously the render path called `state.y_range()` three
    // times and `state.visible()` (a cloning variant) once; each y_range call
    // re-scanned the visible window when y_auto was on. With the bottom-bar's
    // animation-frame loop forcing continuous repaint, that redundancy
    // mattered.
    let (y_lo, y_hi) = state.y_range();
    // Resize the y-axis gutter to fit the widest price label this frame.
    // Stored on `state` so mouse handlers' hit-tests use the same gap as
    // paint — otherwise clicks drift after a y-range change.
    state.y_axis_gap_px.set(compute_y_axis_gap(y_lo, y_hi));

    // This symbol's display meta (name/exchange) for the header line comes
    // from the symbols service. Falls back to the bare ticker if the entry
    // hasn't been registered yet.
    let symbols_handle = cx.global::<SymbolsServiceHandle>().0.clone();
    let symbols_svc = symbols_handle.read(cx);
    let (header_name, header_exchange) = symbols_svc
        .meta(state.symbol.as_ref())
        .unwrap_or_else(|| (state.symbol.clone(), SharedString::from("")));

    // Header symbol button — opens the shared TradingView-style picker
    // (modal overlay) targeting *this* chart. Sets `LastFocusedChart` before
    // dispatching so the workspace's `OpenSymbolPicker` handler resolves to
    // this panel regardless of where the user last clicked.
    let symbol_button = Button::new("chart-symbol-open-picker")
        .label(state.symbol.clone())
        .icon(IconName::ChevronDown)
        .small()
        .ghost()
        .tooltip("Change symbol (Cmd-K)")
        .on_click(cx.listener(|this, _ev, window, cx| {
            let weak = cx.weak_entity();
            *cx.global::<LastFocusedChart>().0.borrow_mut() = Some(weak);
            window.dispatch_action(
                Box::new(OpenSymbolPicker {
                    kind: SharedString::from("chart"),
                }),
                cx,
            );
            let _ = this;
        }));

    // Timeframe-selector dropdown — same focus-scoping as the symbol selector
    // so `ChangeChartTimeframe` dispatches up through this panel.
    let tf_focus = focus.clone();
    let timeframe_btn = Button::new("chart-timeframe-select")
        .label(SharedString::from(state.timeframe().as_str()))
        .small()
        .ghost()
        .dropdown_menu(move |menu, _, _| {
            let mut menu = menu.action_context(tf_focus.clone());
            for tf in Timeframe::ALL {
                menu = menu.menu(
                    SharedString::from(tf.as_str()),
                    Box::new(ChangeChartTimeframe(SharedString::from(tf.as_str()))),
                );
            }
            menu
        });

    // Snapshot the candle slice the paint pass needs. Cloning is fine — at
    // default `view_size = 60` we copy ~60 `Candle`s once per render, the
    // same cost the deleted `visible_for_chart_with_y` had. The closure
    // captures these by move so the borrow doesn't escape `render`.
    let (paint_start_idx, paint_candles_slice) = state.paint_slice();
    let paint_candles: Vec<Candle> = paint_candles_slice.to_vec();
    // Captured separately so the sub-pane builders (which run after the main
    // canvas closure consumes `paint_candles`) can still read the visible-bar
    // count.
    let paint_candles_len = paint_candles.len();
    let paint_view_start = state.view_start;
    let paint_view_size = state.view_size;
    let paint_candle_interval_ms = state.candle_interval_ms();
    let paint_y_axis_gap = state.y_axis_gap_px.get();
    // Pre-filter overlay indicators for the paint closure: skip hidden /
    // pane-placed instances, snapshot color + output so the closure stays
    // 'static. Per-render clone — `Series` is a `Vec<Option<f64>>`, so the
    // cost is comparable to `paint_candles.clone()` above.
    let paint_overlay_items: Vec<OverlayPaintItem> = state
        .indicators
        .iter()
        .zip(state.indicator_outputs.iter())
        .filter(|(i, _)| !i.hidden && i.placement == Placement::Overlay)
        .map(|(i, o)| OverlayPaintItem {
            colors: i.colors.clone(),
            output: o.clone(),
        })
        .collect();

    // Pane-placed indicators: one sub-canvas each, computed in render order.
    // We capture `(instance_id, height, PanePaintItem)` triples; the sub-pane
    // emit loop below uses these to build the splitter+canvas elements. y_lo /
    // y_hi come from `IndicatorKind::y_range` over the visible bar slice — the
    // paint closure stays trait-object-free so it can be `'static`. Hidden
    // pane indicators stay in the list (keeping their slot at full height)
    // but get `hidden: true` so `paint_sub_pane` early-returns without
    // painting anything — the chip overlay at top-left remains reachable.
    let paint_pane_items: Vec<(InstanceId, f32, PanePaintItem)> = {
        let visible_end = paint_start_idx
            .saturating_add(paint_candles.len())
            .min(state.candles.len());
        let visible_range = paint_start_idx..visible_end;
        state
            .indicators
            .iter()
            .zip(state.indicator_outputs.iter())
            .filter(|(i, _)| i.placement == Placement::Pane)
            .filter_map(|(i, o)| {
                let height = i.pane_height.unwrap_or_else(|| {
                    crate::indicators::default_pane_height(i.kind_id)
                });
                if i.hidden {
                    // Placeholder y range — never read since `paint_sub_pane`
                    // early-returns on `hidden`. Keep the slot at full height
                    // so toggling visibility doesn't reflow the layout.
                    let item = PanePaintItem {
                        colors: i.colors.clone(),
                        output: o.clone(),
                        kind_id: i.kind_id,
                        y_lo: 0.0,
                        y_hi: 1.0,
                        hidden: true,
                    };
                    return Some((i.id, height, item));
                }
                // `y_range` returns `None` when no `Some(_)` data falls in the
                // visible window (early bars before the indicator has enough
                // history). For visible panes we skip the canvas paint that
                // frame, but the chip overlay is rendered against the same
                // pane element (built below), so we still keep the slot —
                // the canvas just no-ops via `hidden: true`.
                let (mut y_lo, mut y_hi) = i
                    .kind
                    .y_range(o, visible_range.clone())
                    .unwrap_or((0.0, 1.0));
                if (y_hi - y_lo).abs() < 1e-9 {
                    // Degenerate range: pad ±5% so the line/zero level sits in
                    // the middle of the pane instead of stuck at the edge.
                    let pad = y_hi.abs().max(1.0) * 0.05;
                    y_lo -= pad;
                    y_hi += pad;
                }
                let item = PanePaintItem {
                    colors: i.colors.clone(),
                    output: o.clone(),
                    kind_id: i.kind_id,
                    y_lo,
                    y_hi,
                    hidden: false,
                };
                Some((i.id, height, item))
            })
            .collect()
    };

    let main_chart_colors = MainChartColors {
        bullish: theme_chart_bullish,
        bearish: theme_chart_bearish,
        grid: Hsla {
            a: 0.30,
            ..theme_border
        },
        label: theme_muted_foreground,
        axis_bg: theme_background,
        axis_border: theme_border,
    };
    let entity = cx.entity();

    // Right (price) axis interaction zone — overlays the chart's reserved
    // y-label gutter. Vertical drag scales the locked y range; wheel zooms
    // it; double-click re-enables auto-fit.
    let y_axis_gap = state.y_axis_gap_px.get();
    let right_axis = div()
        .id("chart-right-axis")
        .absolute()
        .right_0()
        .top_0()
        .bottom(gpui::px(AXIS_GAP))
        .w(gpui::px(y_axis_gap))
        .cursor_ns_resize()
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|this, ev: &MouseDownEvent, _w, cx| {
                cx.stop_propagation();
                let Some(state) = this.chart_state.as_mut() else {
                    return;
                };
                if ev.click_count >= 2 {
                    state.reset_y_auto();
                    cx.notify();
                    return;
                }
                state.freeze_y_if_auto();
                state.y_drag_anchor = Some((ev.position, state.y_min, state.y_max));
                cx.notify();
            }),
        )
        .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _w, cx| {
            let Some(state) = this.chart_state.as_mut() else {
                return;
            };
            if !ev.dragging() {
                if state.y_drag_anchor.take().is_some() {
                    cx.notify();
                }
                return;
            }
            let Some((start_pos, start_lo, start_hi)) = state.y_drag_anchor else {
                return;
            };
            let Some(bounds) = state.bounds else {
                return;
            };
            let h = bounds.size.height.as_f32();
            if h <= 0.0 {
                return;
            }
            let dy = ev.position.y.as_f32() - start_pos.y.as_f32();
            // Drag down → range expands (zoom out); drag up → contracts.
            let factor = (dy / h).exp() as f64;
            let center = (start_lo + start_hi) / 2.0;
            state.y_min = center - (center - start_lo) * factor;
            state.y_max = center + (start_hi - center) * factor;
            cx.notify();
        }))
        .on_mouse_up(
            MouseButton::Left,
            cx.listener(|this, _ev, _w, cx| {
                let Some(state) = this.chart_state.as_mut() else {
                    return;
                };
                if state.y_drag_anchor.take().is_some() {
                    cx.notify();
                }
            }),
        )
        .on_scroll_wheel(cx.listener(|this, ev: &ScrollWheelEvent, w, cx| {
            cx.stop_propagation();
            let Some(state) = this.chart_state.as_mut() else {
                return;
            };
            let delta_y = ev.delta.pixel_delta(w.line_height()).y.as_f32();
            if delta_y == 0.0 {
                return;
            }
            state.freeze_y_if_auto();
            let factor = (-delta_y / SCROLL_ZOOM_RATE).exp() as f64;
            let center = (state.y_min + state.y_max) / 2.0;
            state.y_min = center - (center - state.y_min) * factor;
            state.y_max = center + (state.y_max - center) * factor;
            cx.notify();
        }));

    // Bottom (time) axis interaction zone. Horizontal drag scales view_size
    // around its centre; wheel zooms; double-click resets to the trailing
    // default window.
    let bottom_axis = div()
        .id("chart-bottom-axis")
        .absolute()
        .left_0()
        .bottom_0()
        .right(gpui::px(y_axis_gap))
        .h(gpui::px(AXIS_GAP))
        .cursor_ew_resize()
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|this, ev: &MouseDownEvent, _w, cx| {
                cx.stop_propagation();
                let Some(state) = this.chart_state.as_mut() else {
                    return;
                };
                if ev.click_count >= 2 {
                    state.reset_x();
                    cx.notify();
                    return;
                }
                state.x_axis_drag_anchor = Some((ev.position, state.view_size, state.view_start));
                cx.notify();
            }),
        )
        .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _w, cx| {
            let Some(state) = this.chart_state.as_mut() else {
                return;
            };
            if !ev.dragging() {
                if state.x_axis_drag_anchor.take().is_some() {
                    cx.notify();
                }
                return;
            }
            let Some((start_pos, start_size, start_view_start)) = state.x_axis_drag_anchor else {
                return;
            };
            let Some(bounds) = state.bounds else {
                return;
            };
            let w = bounds.size.width.as_f32();
            if w <= 0.0 {
                return;
            }
            let dx = ev.position.x.as_f32() - start_pos.x.as_f32();
            // Drag right → view widens (more candles), drag left → narrows.
            let factor = (dx / w).exp();
            // Centre is taken from the drag-start viewport so it doesn't
            // drift when `clamp` adjusts `view_start` between frames.
            // `view_size` is clamped BEFORE we derive `view_start`, so once
            // the candle width hits its min/max the drag stops shifting the
            // chart horizontally (mirrors the wheel-handler fix).
            let total = state.candles.len() as f32;
            let center_at_down = start_view_start + start_size / 2.0;
            let new_view_size =
                (start_size * factor).clamp(CHART_MIN_VIEW.min(total), CHART_MAX_VIEW.min(total));
            state.view_size = new_view_size;
            state.view_start = center_at_down - new_view_size / 2.0;
            state.clamp();
            this.maybe_load_older(cx);
            cx.notify();
        }))
        .on_mouse_up(
            MouseButton::Left,
            cx.listener(|this, _ev, _w, cx| {
                let Some(state) = this.chart_state.as_mut() else {
                    return;
                };
                if state.x_axis_drag_anchor.take().is_some() {
                    cx.notify();
                }
            }),
        )
        .on_scroll_wheel(cx.listener(|this, ev: &ScrollWheelEvent, w, cx| {
            cx.stop_propagation();
            let Some(state) = this.chart_state.as_mut() else {
                return;
            };
            let delta_y = ev.delta.pixel_delta(w.line_height()).y.as_f32();
            if delta_y == 0.0 {
                return;
            }
            let factor = (-delta_y / SCROLL_ZOOM_RATE).exp();
            // Clamp `view_size` before computing the new `view_start` so
            // hitting the min/max stops both zoom and horizontal drift — see
            // the canvas wheel handler for the longer reasoning.
            let center = state.view_start + state.view_size / 2.0;
            let total = state.candles.len() as f32;
            let new_view_size = (state.view_size * factor)
                .clamp(CHART_MIN_VIEW.min(total), CHART_MAX_VIEW.min(total));
            state.view_size = new_view_size;
            state.view_start = center - state.view_size / 2.0;
            state.clamp();
            this.maybe_load_older(cx);
            cx.notify();
        }));

    // Snapshot visible drawings + selection from the workspace
    // `DrawingService`. Each tick of render builds a view-coord `Vec<Drawing>`
    // anchored to *this* chart's candle buffer, so the paint pipeline doesn't
    // need to know about wall-clock ms. Hidden drawings and drawings whose
    // `tf_filter` excludes the current timeframe are filtered out here so
    // downstream code can iterate freely.
    let symbol_str = state.symbol.as_ref();
    let tf_str = state.timeframe.as_str();
    let (drawings_snapshot, selected_for_overlay) = {
        let service = cx.global::<DrawingServiceHandle>().0.clone();
        let svc = service.read(cx);
        let snapshot: Vec<Drawing> = svc
            .for_symbol(symbol_str)
            .iter()
            .filter(|d| d.visible_on(tf_str))
            .map(|d| drawings_view::shape_to_view(d, &state.candles, paint_candle_interval_ms))
            .collect();
        let sel = svc
            .selected_drawing()
            .filter(|(sym, _)| sym.as_ref() == symbol_str)
            .map(|(_, d)| d.id);
        (snapshot, sel)
    };
    let creating_preview = state.creating.as_ref().map(|c| c.preview());
    // Reuse the y_range computed at the top of render — overlay anchors must
    // match the candles they sit next to.
    let (y_lo_for_overlay, y_hi_for_overlay) = (y_lo, y_hi);
    let drawing_colors = DrawingColors {
        line: theme_foreground,
        // Rect uses foreground (white in dark theme) instead of accent —
        // accent is too muted to read against the candles. Low-alpha fill
        // gives a hint of the body without obscuring the bars.
        rect_fill: Hsla {
            a: 0.08,
            ..theme_foreground
        },
        rect_border: theme_foreground,
        ring: theme_ring,
        background: theme_background,
        bullish: theme_chart_bullish,
        bearish: theme_chart_bearish,
        muted: theme_muted_foreground,
    };
    let cursor_for_overlay = state.cursor;
    // Cross-pane shared x for the main pane's vertical guide. When the
    // cursor is on the main pane this duplicates `cursor.0`; when the
    // cursor is on a sub-pane this still drives the main pane's vertical
    // line so the user can see which bar their sub-pane readout matches.
    let cross_x_for_overlay = state.cross_cursor_x;
    let candles_for_overlay = state.candles.clone();
    let drawings_overlay = render_drawings_overlay(
        drawings_snapshot.clone(),
        creating_preview,
        selected_for_overlay,
        state.view_start,
        state.view_size,
        y_lo_for_overlay,
        y_hi_for_overlay,
        state.y_axis_gap_px.get(),
        cursor_for_overlay,
        cross_x_for_overlay,
        drawing_colors,
        candles_for_overlay,
    )
    .into_any_element();

    // Crosshair labels + OHLC readout. All depend on (cursor, bounds) being
    // Some — i.e. mouse is hovering and we've prepainted at least once.
    let crosshair_chrome: Vec<gpui::AnyElement> =
        if let (Some((cx_px, cy_px)), Some(bounds)) = (state.cursor, state.bounds) {
            let canvas_w = bounds.size.width.as_f32();
            let canvas_h = bounds.size.height.as_f32();
            let y_axis_gap = state.y_axis_gap_px.get();
            let world_t = screen_to_index(
                state.view_start,
                state.view_size,
                cx_px,
                canvas_w,
                state.y_axis_gap_px.get(),
            );
            let candle_idx = world_t.round() as i32;
            let candle: Option<&Candle> =
                if candle_idx >= 0 && (candle_idx as usize) < state.candles.len() {
                    Some(&state.candles[candle_idx as usize])
                } else {
                    None
                };
            let world_p = screen_to_price(y_lo_for_overlay, y_hi_for_overlay, cy_px, canvas_h);

            let mut chrome: Vec<gpui::AnyElement> = Vec::new();

            // OHLC pill at top-right (inboard of the y-axis labels). Was at
            // top-left, but the indicator list moved there as the
            // chart's primary "what am I looking at?" surface; the OHLC
            // hover readout reads cleanly from either corner.
            if let Some(c) = candle {
                let prev_close = if candle_idx > 0 {
                    state.candles.get(candle_idx as usize - 1).map(|p| p.close)
                } else {
                    None
                };
                let change = prev_close.map(|prev| c.close - prev);
                let pct = prev_close.and_then(|prev| {
                    if prev.abs() > 1e-9 {
                        Some((c.close - prev) / prev * 100.0)
                    } else {
                        None
                    }
                });
                let change_color = match change {
                    Some(d) if d >= 0.0 => theme_chart_bullish,
                    Some(_) => theme_chart_bearish,
                    None => theme_muted_foreground,
                };
                let change_text = match (change, pct) {
                    (Some(d), Some(p)) => format!("{:+.2} ({:+.2}%)", d, p),
                    (Some(d), None) => format!("{:+.2}", d),
                    _ => String::new(),
                };
                // Anchor inboard of the y-axis gutter so the pill doesn't
                // overlap the right-axis price labels.
                let ohlc_right = state.y_axis_gap_px.get() + 8.0;
                chrome.push(
                    h_flex()
                        .absolute()
                        .top(px(8.0))
                        .right(px(ohlc_right))
                        .gap(px(8.0))
                        .pl(px(8.0))
                        .pr(px(8.0))
                        .pt(px(4.0))
                        .pb(px(4.0))
                        .text_size(px(11.))
                        .bg(theme_background)
                        .border_1()
                        .border_color(theme_border)
                        .rounded(px(4.0))
                        .child(
                            div()
                                .text_color(theme_muted_foreground)
                                .child(SharedString::from(format_user_tz(c.open_time, cx))),
                        )
                        .child(
                            div()
                                .text_color(theme_foreground)
                                .child(format!("O {:.2}", c.open)),
                        )
                        .child(
                            div()
                                .text_color(theme_chart_bullish)
                                .child(format!("H {:.2}", c.high)),
                        )
                        .child(
                            div()
                                .text_color(theme_chart_bearish)
                                .child(format!("L {:.2}", c.low)),
                        )
                        .child(
                            div()
                                .text_color(theme_foreground)
                                .child(format!("C {:.2}", c.close)),
                        )
                        .when(!change_text.is_empty(), |this| {
                            this.child(div().text_color(change_color).child(change_text))
                        })
                        .into_any_element(),
                );
            }

            // Time label hugging the bottom axis at the cursor's x.
            let chart_w = canvas_w - y_axis_gap;
            if cx_px >= 0.0 && cx_px <= chart_w {
                let time_text = candle
                    .map(|c| format_user_tz(c.open_time, cx))
                    .unwrap_or_else(|| "—".to_string());
                // Width estimated for centring; we shift left so the label is
                // centred under the vertical line.
                let est_w = (time_text.len() as f32 * 7.0).max(48.0) + 12.0;
                let mut left = cx_px - est_w / 2.0;
                left = left.clamp(0.0, chart_w - est_w);
                chrome.push(
                    div()
                        .absolute()
                        .left(px(left))
                        .bottom(px(0.0))
                        .pl(px(6.0))
                        .pr(px(6.0))
                        .text_size(px(11.))
                        .text_color(theme_foreground)
                        .bg(theme_background)
                        .border_1()
                        .border_color(theme_border)
                        .rounded(px(2.0))
                        .child(SharedString::from(time_text))
                        .into_any_element(),
                );
            }

            // Price label hugging the right axis at the cursor's y. Uses a
            // fixed pixel size (not `text_xs`) so the readout stays compact
            // even when the user dials up the global font size.
            let chart_h = canvas_h - AXIS_GAP;
            if cy_px >= 0.0 && cy_px <= chart_h {
                chrome.push(
                    div()
                        .absolute()
                        .right(px(0.0))
                        .top(px((cy_px - 8.0).max(0.0)))
                        .w(px(y_axis_gap - 2.0))
                        .pl(px(4.0))
                        .pr(px(4.0))
                        .text_size(px(11.))
                        .text_color(theme_foreground)
                        .bg(theme_background)
                        .border_1()
                        .border_color(theme_border)
                        .rounded(px(2.0))
                        .child(format!("{:.2}", world_p))
                        .into_any_element(),
                );
            }
            chrome
        } else {
            Vec::new()
        };

    // Live developing-bar guide: a horizontal price ray from the current
    // (still-open) bar to the right edge of the chart, a colour-coded price
    // pill on the right axis, and a "M:SS" countdown to the next bar open.
    // Only live symbols have a developing bar; for historical charts this
    // collapses to an empty vec so we don't paint a stale last-close marker.
    let live_price_chrome: Vec<gpui::AnyElement> = if let (Some(bounds), Some(last)) =
        (state.bounds, state.candles.last())
    {
        let canvas_w = bounds.size.width.as_f32();
        let canvas_h = bounds.size.height.as_f32();
        let y_axis_gap = state.y_axis_gap_px.get();
        let chart_w = (canvas_w - y_axis_gap).max(0.0);
        let chart_h = (canvas_h - AXIS_GAP).max(0.0);

        let last_idx = (state.candles.len() - 1) as f32;
        let last_x = index_to_screen(
            state.view_start,
            state.view_size,
            last_idx,
            canvas_w,
            state.y_axis_gap_px.get(),
        );
        let price_y = price_to_screen(y_lo, y_hi, last.close, canvas_h);

        // Direction relative to bar open — colour matches the candle body so
        // the guide reads as "this is the current bar's close".
        let bar_color = if last.close >= last.open {
            theme_chart_bullish
        } else {
            theme_chart_bearish
        };

        let mut out: Vec<gpui::AnyElement> = Vec::new();

        // Horizontal price ray. Clamp left to the chart area; if the bar has
        // scrolled off-screen the ray still hugs the chart's right half so
        // the user can find the live price without re-anchoring.
        let line_left = last_x.clamp(0.0, chart_w);
        let line_width = (chart_w - line_left).max(0.0);
        if line_width > 0.0 && price_y >= 0.0 && price_y <= chart_h {
            out.push(
                div()
                    .absolute()
                    .left(px(line_left))
                    .top(px(price_y - 0.5))
                    .w(px(line_width))
                    .h(px(1.0))
                    // Faded so it doesn't fight the candles/drawings under it.
                    .bg(Hsla {
                        a: 0.55,
                        ..bar_color
                    })
                    .into_any_element(),
            );
        }

        // Right-axis price pill — solid background in the bar's direction
        // colour so it's the loudest thing on the axis (this is the "live"
        // signal users want at a glance).
        let pill_top = (price_y - 8.0).clamp(0.0, (chart_h - 16.0).max(0.0));
        out.push(
            div()
                .absolute()
                .right(px(0.0))
                .top(px(pill_top))
                .w(px((y_axis_gap - 2.0).max(0.0)))
                .pl(px(4.0))
                .pr(px(4.0))
                .text_size(px(11.))
                .font_semibold()
                .text_color(theme_background)
                .bg(bar_color)
                .rounded(px(2.0))
                .child(format!("{:.2}", last.close))
                .into_any_element(),
        );

        // M:SS countdown to bar close. Clamped to ≥0 — if `close_time` has
        // already elapsed (WS-stream hasn't told us yet that the bar
        // rolled), we just show 0:00 instead of a negative number.
        let now_ms = chrono::Utc::now().timestamp_millis();
        let remaining_ms = (last.close_time - now_ms).max(0);
        let total_sec = remaining_ms / 1000;
        let mm = total_sec / 60;
        let ss = total_sec % 60;
        let cd_top = (price_y + 8.0).clamp(0.0, (chart_h - 14.0).max(0.0));
        out.push(
            div()
                .absolute()
                .right(px(0.0))
                .top(px(cd_top))
                .w(px((y_axis_gap - 2.0).max(0.0)))
                .pl(px(4.0))
                .pr(px(4.0))
                .text_size(px(11.))
                .text_color(theme_muted_foreground)
                .bg(theme_background)
                .border_1()
                .border_color(theme_border)
                .rounded(px(2.0))
                .child(SharedString::from(format!("{}:{:02}", mm, ss)))
                .into_any_element(),
        );

        out
    } else {
        Vec::new()
    };

    // Y-axis price pill for every visible horizontal ray. Mirrors the
    // live-price pill so each ray's exact price is readable on the axis,
    // independent of grid spacing. Painted as positioned divs (over the
    // axis chrome) so it sits above the y-axis labels.
    let ray_price_chrome: Vec<gpui::AnyElement> = if let Some(bounds) = state.bounds {
        let canvas_h = bounds.size.height.as_f32();
        let y_axis_gap = state.y_axis_gap_px.get();
        let chart_h = (canvas_h - AXIS_GAP).max(0.0);
        let mut out: Vec<gpui::AnyElement> = Vec::new();
        for d in &drawings_snapshot {
            let Drawing::HorizontalRay { anchor, .. } = d else {
                continue;
            };
            let price_y = price_to_screen(y_lo_for_overlay, y_hi_for_overlay, anchor.1, canvas_h);
            if price_y < 0.0 || price_y > chart_h {
                continue;
            }
            let pill_top = (price_y - 8.0).clamp(0.0, (chart_h - 16.0).max(0.0));
            out.push(
                div()
                    .absolute()
                    .right(px(0.0))
                    .top(px(pill_top))
                    .w(px((y_axis_gap - 2.0).max(0.0)))
                    .h(px(16.0))
                    .pl(px(4.0))
                    .pr(px(4.0))
                    .text_size(px(11.))
                    .line_height(px(16.0))
                    .text_color(theme_background)
                    .bg(theme_foreground)
                    .rounded(px(2.0))
                    .child(format!("{:.2}", anchor.1))
                    .into_any_element(),
            );
        }
        out
    } else {
        Vec::new()
    };

    // Floating "Go to latest" button — appears bottom-right inside the
    // canvas when the most recent candle has scrolled off the right edge of
    // the viewport (e.g. user panned left into history, or new live ticks
    // formed past the visible window). Clicking shifts `view_start` so the
    // latest bar lands at the default trailing offset, preserving the
    // user's zoom. Anchored to the canvas, inset past the y-axis gutter
    // and the x-axis label row.
    let go_to_latest_chrome: Vec<gpui::AnyElement> = if state.bounds.is_some()
        && state.latest_off_right()
    {
        let y_axis_gap = state.y_axis_gap_px.get();
        // No tooltip: the button removes itself on click (latest comes back
        // into view), and the gpui-component tooltip overlay only hides on
        // hover-leave — which never fires for a vanished element, leaving a
        // sticky popup behind. The arrow icon is conventional enough for
        // "go to latest" without a label.
        let btn = Button::new("chart-go-to-latest")
            .icon(IconName::ArrowRight)
            .ghost()
            .xsmall()
            .rounded(gpui_component::button::ButtonRounded::Size(px(999.0)))
            .on_click(cx.listener(|this, _ev, _w, cx| {
                let Some(state) = this.chart_state.as_mut() else {
                    return;
                };
                state.snap_to_latest();
                cx.notify();
            }));
        vec![
            div()
                .absolute()
                .right(px(y_axis_gap + 12.0))
                .bottom(px(AXIS_GAP + 12.0))
                // Eat the mouse-down so the canvas's pan handler doesn't
                // arm a drag underneath the button. The Button's own
                // on_click still fires on release.
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(btn)
                .into_any_element(),
        ]
    } else {
        Vec::new()
    };

    // Text labels live as positioned divs (not painted in the overlay) so
    // editing reuses gpui-component's `Input` widget. Each label is purely
    // visual — selection and drag happen through canvas-level hit testing
    // against an estimated bounding box. Selected text gets a small handle
    // dot at the right edge as a resize affordance.
    let text_labels: Vec<gpui::AnyElement> = if let Some(bounds) = state.bounds {
        let canvas_w = bounds.size.width.as_f32();
        let canvas_h = bounds.size.height.as_f32();
        let editing_id = state.editing_text.as_ref().and_then(|e| e.existing_id);
        let mut out: Vec<gpui::AnyElement> = Vec::new();
        for d in &drawings_snapshot {
            let Drawing::Text {
                id,
                anchor,
                width,
                text,
            } = d
            else {
                continue;
            };
            if editing_id == Some(*id) {
                continue;
            }
            let sx = index_to_screen(
                state.view_start,
                state.view_size,
                anchor.0,
                canvas_w,
                state.y_axis_gap_px.get(),
            );
            let sy = price_to_screen(y_lo_for_overlay, y_hi_for_overlay, anchor.1, canvas_h);
            let selected = selected_for_overlay == Some(*id);
            let border = if selected { theme_ring } else { theme_border };
            out.push(
                div()
                    .absolute()
                    .left(px(sx))
                    .top(px(sy))
                    // Fixed pixel width — text inside wraps naturally; the
                    // div's height grows with content. No background fill so
                    // candles/grid show through; the thin border keeps the
                    // box outline visible.
                    .w(px(*width))
                    .px_1p5()
                    .py_0p5()
                    .text_xs()
                    .text_color(theme_foreground)
                    .border_1()
                    .border_color(border)
                    .rounded(px(3.))
                    .child(SharedString::from(text.clone()))
                    .into_any_element(),
            );
            if selected {
                // Right-edge resize handle: small square centred on the
                // right edge near the top so it's reachable even on a
                // multi-line box.
                out.push(
                    div()
                        .absolute()
                        .left(px(sx + *width - 4.0))
                        .top(px(sy + 2.0))
                        .w(px(8.0))
                        .h(px(8.0))
                        .bg(theme_background)
                        .border_1()
                        .border_color(theme_ring)
                        .rounded(px(2.0))
                        .into_any_element(),
                );
            }
        }
        out
    } else {
        Vec::new()
    };

    // Position labels: small chips at the right edge of each position rect
    // showing the three price levels (entry / TP / SL). Rendered as divs to
    // avoid bookkeeping a text-shape cache in the paint closure.
    let position_labels: Vec<gpui::AnyElement> = if let Some(bounds) = state.bounds {
        let canvas_w = bounds.size.width.as_f32();
        let canvas_h = bounds.size.height.as_f32();
        let mut out = Vec::new();
        for d in &drawings_snapshot {
            let (t0, t1, entry, tp, sl) = match d {
                Drawing::Long {
                    t0,
                    t1,
                    entry,
                    take_profit,
                    stop_loss,
                    ..
                }
                | Drawing::Short {
                    t0,
                    t1,
                    entry,
                    take_profit,
                    stop_loss,
                    ..
                } => (*t0, *t1, *entry, *take_profit, *stop_loss),
                _ => continue,
            };
            let x0 = index_to_screen(
                state.view_start,
                state.view_size,
                t0,
                canvas_w,
                state.y_axis_gap_px.get(),
            );
            let x1 = index_to_screen(
                state.view_start,
                state.view_size,
                t1,
                canvas_w,
                state.y_axis_gap_px.get(),
            );
            let xmax = x0.max(x1);
            let y_entry = price_to_screen(y_lo_for_overlay, y_hi_for_overlay, entry, canvas_h);
            let y_tp = price_to_screen(y_lo_for_overlay, y_hi_for_overlay, tp, canvas_h);
            let y_sl = price_to_screen(y_lo_for_overlay, y_hi_for_overlay, sl, canvas_h);
            let make_label = |y: f32, text: String, color: Hsla| -> gpui::AnyElement {
                div()
                    .absolute()
                    .left(px(xmax + 4.0))
                    .top(px(y - 7.0))
                    .text_xs()
                    .text_color(color)
                    .bg(theme_background)
                    .px_1()
                    .rounded(px(2.))
                    .child(SharedString::from(text))
                    .into_any_element()
            };
            out.push(make_label(
                y_entry,
                format!("E ${:.2}", entry),
                theme_muted_foreground,
            ));
            out.push(make_label(
                y_tp,
                format!("TP ${:.2}", tp),
                theme_chart_bullish,
            ));
            out.push(make_label(
                y_sl,
                format!("SL ${:.2}", sl),
                theme_chart_bearish,
            ));
            // R:R: reward / risk. Sign-flipped per direction so the printed
            // ratio is a positive number when the user followed convention.
            let (reward, risk) = match d {
                Drawing::Long { .. } => (tp - entry, entry - sl),
                Drawing::Short { .. } => (entry - tp, sl - entry),
                _ => (0.0, 0.0),
            };
            if risk.abs() > 1e-6 {
                let rr = reward / risk;
                out.push(make_label(
                    y_sl + 18.0,
                    format!("R:R 1:{:.2}", rr.abs()),
                    theme_muted_foreground,
                ));
            }
        }
        out
    } else {
        Vec::new()
    };

    // Inline text editor (Input wrapped in a positioned div). Stops mouse
    // propagation so clicks inside don't fire the canvas's commit-on-click.
    let editor_overlay: Option<gpui::AnyElement> =
        if let (Some(editing), Some(bounds)) = (state.editing_text.as_ref(), state.bounds) {
            let canvas_w = bounds.size.width.as_f32();
            let canvas_h = bounds.size.height.as_f32();
            let sx = index_to_screen(
                state.view_start,
                state.view_size,
                editing.anchor.0,
                canvas_w,
                state.y_axis_gap_px.get(),
            );
            let sy = price_to_screen(
                y_lo_for_overlay,
                y_hi_for_overlay,
                editing.anchor.1,
                canvas_h,
            );
            Some(
                div()
                    .absolute()
                    .left(px(sx))
                    .top(px(sy))
                    .w(px(editing.width))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(Input::new(&editing.input).text_xs())
                    .into_any_element(),
            )
        } else {
            None
        };

    let canvas = div()
        .id("chart-canvas")
        .relative()
        .flex_1()
        .min_h_0()
        .w_full()
        // Crosshair cursor everywhere on the canvas — the chart provides its
        // own guide lines + OHLC readout, and a crosshair OS cursor reads as
        // "this is a precise readout surface" better than the old grab/hand.
        .cursor_crosshair()
        .on_prepaint({
            let entity = entity.clone();
            move |bounds, _, cx| {
                entity.update(cx, |this, cx| {
                    if let Some(state) = this.chart_state.as_mut() {
                        // Overlay chrome (live price ray, axis pill, drawing
                        // labels) is positioned in render using `state.bounds`
                        // from the previous frame. When the canvas resizes
                        // (e.g. AI Chat opens and shrinks it), render runs with
                        // the stale wider size and the ray ends up clipped or
                        // misaligned until some other event triggers another
                        // render. Notify on size change so the next frame
                        // re-renders with the fresh bounds.
                        let size_changed = state
                            .bounds
                            .map_or(true, |prev| prev.size != bounds.size);
                        state.bounds = Some(bounds);
                        if size_changed {
                            cx.notify();
                        }
                    }
                });
            }
        })
        .on_hover({
            let entity = entity.clone();
            move |&entered, _, cx| {
                if entered {
                    return;
                }
                // Cursor left the main canvas — clear its crosshair. The
                // cross-pane vertical guide (`cross_cursor_x`) is left alone
                // here: cursor might be entering a sub-pane next, and that
                // sub-pane's mouse_move will reset cross_cursor_x. We only
                // wipe `cross_cursor_x` if the cursor isn't in a sub-pane
                // either — that means the cursor has truly left the chart.
                entity.update(cx, |this, cx| {
                    if let Some(state) = this.chart_state.as_mut() {
                        let mut changed = state.cursor.take().is_some();
                        if state.sub_cursor.is_none() && state.cross_cursor_x.take().is_some() {
                            changed = true;
                        }
                        if changed {
                            cx.notify();
                        }
                    }
                });
            }
        })
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|this, ev: &MouseDownEvent, w, cx| {
                let Some(state) = this.chart_state.as_mut() else {
                    return;
                };
                // Drain any active text editor first. Mouse-down on the
                // canvas (i.e. not on the Input itself, which stop-propagates)
                // counts as "click outside" → commit. Eat the click so the
                // current tool's dispatch doesn't also fire.
                if let Some(editing) = state.editing_text.take() {
                    let value = editing.input.read(cx).value();
                    let trimmed = value.trim();
                    if !trimmed.is_empty() {
                        let interval = state.candle_interval_ms();
                        let candles_snap = state.candles.clone();
                        let symbol = state.symbol.clone();
                        let svc = cx.global::<DrawingServiceHandle>().0.clone();
                        match editing.existing_id {
                            Some(id) => {
                                // Rewrite the existing text drawing's text in
                                // place, preserving its anchor + width.
                                let existing = svc
                                    .read(cx)
                                    .for_symbol(symbol.as_ref())
                                    .iter()
                                    .find(|d| d.id == id)
                                    .map(|d| d.shape.clone());
                                if let Some(crate::drawings::shapes::DrawingShape::Text(mut t)) =
                                    existing
                                {
                                    t.text = trimmed.to_string();
                                    let symbol2 = symbol.clone();
                                    svc.update(cx, move |s, cx| {
                                        s.update_shape(
                                            symbol2.as_ref(),
                                            id,
                                            crate::drawings::shapes::DrawingShape::Text(t),
                                            cx,
                                        );
                                    });
                                }
                            }
                            None => {
                                let view = Drawing::Text {
                                    id: 0,
                                    anchor: editing.anchor,
                                    width: editing.width,
                                    text: trimmed.to_string(),
                                };
                                let shape =
                                    drawings_view::view_to_shape(&view, &candles_snap, interval);
                                let symbol2 = symbol.clone();
                                let id = svc.update(cx, |s, cx| s.add(symbol2.clone(), shape, cx));
                                svc.update(cx, |s, cx| s.set_selected(Some((symbol2, id)), cx));
                            }
                        }
                    }
                    cx.notify();
                    return;
                }

                let Some(bounds) = state.bounds else {
                    return;
                };
                let canvas_x = ev.position.x.as_f32() - bounds.origin.x.as_f32();
                let canvas_y = ev.position.y.as_f32() - bounds.origin.y.as_f32();
                let canvas_w = bounds.size.width.as_f32();
                let canvas_h = bounds.size.height.as_f32();
                if canvas_w <= 0.0 || canvas_h <= 0.0 {
                    return;
                }
                let (y_lo, y_hi) = state.y_range();
                let world_t = snap_t(screen_to_index(
                    state.view_start,
                    state.view_size,
                    canvas_x,
                    canvas_w,
                    state.y_axis_gap_px.get(),
                ));
                let world_p = screen_to_price(y_lo, y_hi, canvas_y, canvas_h);

                // Active tool comes from the global state — set by the top
                // bar's Draw popover, mirrored across every chart.
                let active_tool = cx
                    .global::<crate::drawings::tool::DrawingToolStateHandle>()
                    .0
                    .read(cx)
                    .tool();

                match active_tool {
                    Tool::HorizontalRay => {
                        // One-click commit: a horizontal ray is defined by a
                        // single (time, price) anchor — no trailing endpoint
                        // to drag — so just write it through immediately.
                        let drawing = Drawing::HorizontalRay {
                            id: 0,
                            anchor: (world_t, world_p),
                            text: None,
                        };
                        let shape = drawings_view::view_to_shape(
                            &drawing,
                            &state.candles,
                            state.candle_interval_ms(),
                        );
                        let symbol = state.symbol.clone();
                        let svc = cx.global::<DrawingServiceHandle>().0.clone();
                        let symbol2 = symbol.clone();
                        let id = svc.update(cx, |s, cx| s.add(symbol2.clone(), shape, cx));
                        svc.update(cx, |s, cx| s.set_selected(Some((symbol2, id)), cx));
                        let tool_state = cx
                            .global::<crate::drawings::tool::DrawingToolStateHandle>()
                            .0
                            .clone();
                        tool_state.update(cx, |s, cx| s.reset(cx));
                        cx.notify();
                    }
                    Tool::AnchoredVwap => {
                        // Single-click commit: an Anchored VWAP needs only a
                        // time anchor (price isn't user-chosen; the line
                        // tracks the cumulative volume-weighted price from
                        // the anchor bar forward). `world_p` rides along for
                        // shape symmetry but is unused at render.
                        let drawing = Drawing::AnchoredVwap {
                            id: 0,
                            anchor: (world_t, world_p),
                        };
                        let shape = drawings_view::view_to_shape(
                            &drawing,
                            &state.candles,
                            state.candle_interval_ms(),
                        );
                        let symbol = state.symbol.clone();
                        let svc = cx.global::<DrawingServiceHandle>().0.clone();
                        let symbol2 = symbol.clone();
                        let id = svc.update(cx, |s, cx| s.add(symbol2.clone(), shape, cx));
                        svc.update(cx, |s, cx| s.set_selected(Some((symbol2, id)), cx));
                        let tool_state = cx
                            .global::<crate::drawings::tool::DrawingToolStateHandle>()
                            .0
                            .clone();
                        tool_state.update(cx, |s, cx| s.reset(cx));
                        cx.notify();
                    }
                    Tool::Line
                    | Tool::Arrow
                    | Tool::Rectangle
                    | Tool::Fibonacci
                    | Tool::Long
                    | Tool::Short => {
                        // Two-click creation: first click places the anchor,
                        // mouse-move updates the trailing point (in the
                        // mouse-move handler), and the second click commits.
                        // No drag-release path — the user explicitly asked
                        // for click-click.
                        if let Some(mut creating) = state.creating.take() {
                            // Second click: snap trailing anchor to this
                            // position and commit through the service.
                            creating.set_end((world_t, world_p));
                            let drawing = creating.into_drawing(0);
                            let shape = drawings_view::view_to_shape(
                                &drawing,
                                &state.candles,
                                state.candle_interval_ms(),
                            );
                            let symbol = state.symbol.clone();
                            let svc = cx.global::<DrawingServiceHandle>().0.clone();
                            let symbol2 = symbol.clone();
                            let id = svc.update(cx, |s, cx| s.add(symbol2.clone(), shape, cx));
                            svc.update(cx, |s, cx| s.set_selected(Some((symbol2, id)), cx));
                            // After a one-shot creation, revert to Select so
                            // the user can immediately move what they drew.
                            let tool_state = cx
                                .global::<crate::drawings::tool::DrawingToolStateHandle>()
                                .0
                                .clone();
                            tool_state.update(cx, |s, cx| s.reset(cx));
                            cx.notify();
                        } else {
                            // First click: start a new creation. Trailing
                            // anchor starts at the same point so the preview
                            // collapses to a dot until the cursor moves.
                            // Round the default width to a whole number of
                            // candle slots so the position's `t1` lands on a
                            // bar centre — otherwise the right edge would
                            // float between two candles on creation. Minimum
                            // 1 slot so a zoomed-in viewport still gives a
                            // visible position.
                            let default_width = (state.view_size * POSITION_DEFAULT_WIDTH_RATIO)
                                .round()
                                .max(1.0);
                            state.creating = CreatingDrawing::from_tool(
                                active_tool,
                                (world_t, world_p),
                                default_width,
                            );
                            cx.notify();
                        }
                    }
                    Tool::Text => {
                        let input = cx.new(|cx| {
                            // auto_grow already implies multi-line input;
                            // 1..8 rows lets the editor expand as text wraps
                            // without ballooning when the user empties it.
                            InputState::new(w, cx).placeholder("Text…").auto_grow(1, 8)
                        });
                        // Focus the new input so the user can type immediately
                        // — without this they'd have to click the field first.
                        input.focus_handle(cx).focus(w, cx);
                        state.editing_text = Some(TextEditing {
                            existing_id: None,
                            anchor: (world_t, world_p),
                            width: TEXT_DEFAULT_WIDTH_PX,
                            input,
                        });
                        // Per spec: revert to Select after starting a text.
                        let tool_state = cx
                            .global::<crate::drawings::tool::DrawingToolStateHandle>()
                            .0
                            .clone();
                        tool_state.update(cx, |s, cx| s.reset(cx));
                        cx.notify();
                    }
                    Tool::Select => {
                        // Build the hit-test snapshot from the service, in
                        // view-coords. Drawings filtered for visibility +
                        // current-TF here so hidden / out-of-filter drawings
                        // don't intercept clicks.
                        let symbol = state.symbol.clone();
                        let tf_str = state.timeframe.as_str();
                        let interval = state.candle_interval_ms();
                        let visible_drawings: Vec<Drawing> = {
                            let svc = cx.global::<DrawingServiceHandle>().0.clone();
                            let svc_read = svc.read(cx);
                            svc_read
                                .for_symbol(symbol.as_ref())
                                .iter()
                                .filter(|d| d.visible_on(tf_str))
                                .map(|d| drawings_view::shape_to_view(d, &state.candles, interval))
                                .collect()
                        };
                        if let Some((hit_id, handle)) = hit_test_drawings(
                            &visible_drawings,
                            state.view_start,
                            state.view_size,
                            y_lo,
                            y_hi,
                            canvas_w,
                            canvas_h,
                            state.y_axis_gap_px.get(),
                            canvas_x,
                            canvas_y,
                        ) {
                            let baseline =
                                visible_drawings.iter().find(|d| d.id() == hit_id).cloned();
                            if let Some(baseline) = baseline {
                                // Double-click on a text drawing → open the
                                // inline editor for it. (Use the hit
                                // drawing's existing data so the resize +
                                // existing text are preserved.)
                                if ev.click_count >= 2 {
                                    if let Drawing::Text {
                                        anchor,
                                        width,
                                        text,
                                        ..
                                    } = &baseline
                                    {
                                        let existing_text = text.clone();
                                        let existing_width = *width;
                                        let existing_anchor = *anchor;
                                        let input = cx.new(|cx| {
                                            InputState::new(w, cx)
                                                .placeholder("Text…")
                                                .auto_grow(1, 8)
                                                .default_value(existing_text)
                                        });
                                        input.focus_handle(cx).focus(w, cx);
                                        state.editing_text = Some(TextEditing {
                                            existing_id: Some(hit_id),
                                            anchor: existing_anchor,
                                            width: existing_width,
                                            input,
                                        });
                                        cx.notify();
                                        return;
                                    }
                                }
                                let symbol2 = symbol.clone();
                                let svc = cx.global::<DrawingServiceHandle>().0.clone();
                                svc.update(cx, |s, cx| s.set_selected(Some((symbol2, hit_id)), cx));
                                state.edit_drag = Some(EditDrag {
                                    id: hit_id,
                                    handle,
                                    baseline,
                                    anchor_world: (world_t, world_p),
                                    anchor_screen: (canvas_x, canvas_y),
                                    moved: false,
                                });
                                cx.notify();
                                return;
                            }
                        }
                        // Empty canvas: deselect + begin pan.
                        let svc = cx.global::<DrawingServiceHandle>().0.clone();
                        svc.update(cx, |s, cx| s.set_selected(None, cx));
                        state.drag_anchor = Some(CanvasDrag {
                            start_pos: ev.position,
                            start_view_start: state.view_start,
                            y_freeze: None,
                        });
                        cx.notify();
                    }
                }
            }),
        )
        .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _w, cx| {
            let Some(state) = this.chart_state.as_mut() else {
                return;
            };
            let Some(bounds) = state.bounds else {
                return;
            };
            let canvas_x = ev.position.x.as_f32() - bounds.origin.x.as_f32();
            let canvas_y = ev.position.y.as_f32() - bounds.origin.y.as_f32();
            let canvas_w = bounds.size.width.as_f32();
            let canvas_h = bounds.size.height.as_f32();
            if canvas_w <= 0.0 || canvas_h <= 0.0 {
                return;
            }
            // Crosshair: capture cursor unconditionally so the guide lines
            // and OHLC readout follow the mouse during pan/edit/creation too.
            // `cross_cursor_x` mirrors x so the cross-pane vertical guide and
            // chip-value-at-cursor pipeline work from any pane; clearing
            // `sub_cursor` ensures the previous sub-pane's horizontal guide
            // doesn't linger after the cursor returns to the main canvas.
            state.cursor = Some((canvas_x, canvas_y));
            state.cross_cursor_x = Some(canvas_x);
            state.sub_cursor = None;
            cx.notify();
            let (y_lo, y_hi) = state.y_range();
            let world_t = snap_t(screen_to_index(
                state.view_start,
                state.view_size,
                canvas_x,
                canvas_w,
                state.y_axis_gap_px.get(),
            ));
            let world_p = screen_to_price(y_lo, y_hi, canvas_y, canvas_h);

            // 1. In-progress 2-click creation: trailing anchor follows the
            // cursor regardless of mouse-button state. Don't gate on
            // `ev.dragging()` — hover-tracking is the whole point.
            if let Some(creating) = state.creating.as_mut() {
                creating.set_end((world_t, world_p));
                cx.notify();
                return;
            }

            if !ev.dragging() {
                // No button held and nothing in flight: clear any stale
                // pan/edit anchor so the next mouse-down starts fresh.
                let cleared =
                    state.drag_anchor.take().is_some() || state.edit_drag.take().is_some();
                if cleared {
                    cx.notify();
                }
                return;
            }

            // 2. Edit drag on an existing drawing. The baseline (snapshot at
            // drag start) is in chart-coords; we transform it by (dt, dp) and
            // broadcast the result via `preview_shape` (no persist). Final
            // persistence + snap-to-current-TF-grid happens on mouse-up.
            if let Some(drag) = state.edit_drag.clone() {
                let dt = world_t - drag.anchor_world.0;
                let dp = world_p - drag.anchor_world.1;
                let mut edited = drag.baseline.clone();
                let mut handled = false;
                if drag.handle == EditHandle::EndpointB {
                    // Text-width resize uses pixel delta, not world delta.
                    if let Drawing::Text { width: base_w, .. } = &drag.baseline {
                        let dx_px = canvas_x - drag.anchor_screen.0;
                        if let Drawing::Text { width, .. } = &mut edited {
                            *width = (base_w + dx_px).max(40.0);
                        }
                        handled = true;
                    }
                }
                if !handled {
                    apply_edit(&mut edited, &drag.baseline, drag.handle, dt, dp);
                }
                let shape = drawings_view::view_to_shape(
                    &edited,
                    &state.candles,
                    state.candle_interval_ms(),
                );
                let symbol = state.symbol.clone();
                let svc = cx.global::<DrawingServiceHandle>().0.clone();
                svc.update(cx, |s, cx| {
                    s.preview_shape(symbol.as_ref(), drag.id, shape, cx)
                });
                // Mark the drag as having actually produced motion so the
                // mouse-up handler knows to snap (vs treating a click-only as
                // a no-op).
                if let Some(d) = state.edit_drag.as_mut() {
                    if dt != 0.0 || dp != 0.0 {
                        d.moved = true;
                    }
                }
                cx.notify();
                return;
            }

            // 3. Canvas pan (Select tool, empty-area drag).
            let Some(mut pan_drag) = state.drag_anchor else {
                return;
            };

            // X pan: always active from drag start.
            let dx = ev.position.x.as_f32() - pan_drag.start_pos.x.as_f32();
            let candles_per_px = state.view_size / canvas_w;
            state.view_start = pan_drag.start_view_start - dx * candles_per_px;
            state.clamp();
            // Any horizontal motion during a pan means the user has chosen
            // to leave the live edge — disable sticky-tail. Pure clicks
            // (`dx == 0`) leave sticky alone so a no-motion mouse-down
            // doesn't silently drop the mode.
            if dx != 0.0 {
                state.sticky_to_latest = false;
            }

            // Y pan: lazy — only once vertical motion crosses the deadzone.
            // On first cross, freeze auto-fit and snapshot the y range so
            // subsequent moves translate from that baseline (no jump at
            // threshold cross).
            let dy = ev.position.y.as_f32() - pan_drag.start_pos.y.as_f32();
            if dy.abs() >= Y_FREEZE_DEADZONE_PX {
                if pan_drag.y_freeze.is_none() {
                    state.freeze_y_if_auto();
                    pan_drag.y_freeze = Some((ev.position, state.y_min, state.y_max));
                }
                if let Some((freeze_pos, baseline_min, baseline_max)) = pan_drag.y_freeze {
                    if canvas_h > 0.0 {
                        let dy_from_freeze = ev.position.y.as_f32() - freeze_pos.y.as_f32();
                        let range = baseline_max - baseline_min;
                        // Drag down (dy > 0) → price range shifts up so the
                        // chart content follows the hand. y = y_max maps to
                        // the canvas top, so increasing both min and max
                        // moves visible content downward on screen.
                        let delta = dy_from_freeze as f64 * range / canvas_h as f64;
                        state.y_min = baseline_min + delta;
                        state.y_max = baseline_max + delta;
                    }
                }
            }

            state.drag_anchor = Some(pan_drag);
            this.maybe_load_older(cx);
            cx.notify();
        }))
        .on_mouse_up(
            MouseButton::Left,
            cx.listener(|this, _ev, _w, cx| {
                let Some(state) = this.chart_state.as_mut() else {
                    return;
                };
                // Drawing creation commits on the *second* mouse-down (in
                // the on_mouse_down handler), not on mouse-up — so we don't
                // touch `state.creating` here.
                // Clear edit drag. If the drag actually moved the drawing,
                // snap each anchor to the current TF's candle grid (round
                // view-coord idx to integer) and commit; otherwise just
                // flush the in-memory state to disk in case any preview
                // happened to land on a non-integer position en route.
                if let Some(drag) = state.edit_drag.take() {
                    let svc = cx.global::<DrawingServiceHandle>().0.clone();
                    if drag.moved {
                        let symbol = state.symbol.clone();
                        let interval = state.candle_interval_ms();
                        let candles_snap = state.candles.clone();
                        let snapped: Option<crate::drawings::shapes::DrawingShape> = {
                            let svc_read = svc.read(cx);
                            svc_read
                                .for_symbol(symbol.as_ref())
                                .iter()
                                .find(|d| d.id == drag.id)
                                .map(|d| {
                                    let mut view =
                                        drawings_view::shape_to_view(d, &candles_snap, interval);
                                    snap_view_to_grid(&mut view);
                                    drawings_view::view_to_shape(&view, &candles_snap, interval)
                                })
                        };
                        if let Some(shape) = snapped {
                            svc.update(cx, |s, cx| {
                                s.update_shape(symbol.as_ref(), drag.id, shape, cx)
                            });
                        } else {
                            svc.update(cx, |s, _cx| s.flush_persist());
                        }
                    } else {
                        svc.update(cx, |s, _cx| s.flush_persist());
                    }
                    cx.notify();
                    return;
                }
                // Clear pan anchor.
                if state.drag_anchor.take().is_some() {
                    cx.notify();
                }
            }),
        )
        // Right-click hit-test → write the target (drawing or empty) into the
        // workspace `LastChartRightClick` global so the chart's context_menu
        // builder can shape itself per-drawing or canvas-wide. We don't stop
        // propagation here so the framework's ContextMenu element still sees
        // the right-mouse-down and opens its menu.
        .on_mouse_down(
            MouseButton::Right,
            cx.listener(|this, ev: &MouseDownEvent, _w, cx| {
                let Some(state) = this.chart_state.as_ref() else {
                    return;
                };
                let symbol = state.symbol.clone();
                let target: Option<DrawingId> = (|| {
                    let bounds = state.bounds?;
                    let canvas_x = ev.position.x.as_f32() - bounds.origin.x.as_f32();
                    let canvas_y = ev.position.y.as_f32() - bounds.origin.y.as_f32();
                    let canvas_w = bounds.size.width.as_f32();
                    let canvas_h = bounds.size.height.as_f32();
                    if canvas_w <= 0.0 || canvas_h <= 0.0 {
                        return None;
                    }
                    let (y_lo, y_hi) = state.y_range();
                    let tf_str = state.timeframe.as_str();
                    let interval = state.candle_interval_ms();
                    let visible: Vec<Drawing> = {
                        let svc = cx.global::<DrawingServiceHandle>().0.clone();
                        let svc_read = svc.read(cx);
                        svc_read
                            .for_symbol(symbol.as_ref())
                            .iter()
                            .filter(|d| d.visible_on(tf_str))
                            .map(|d| drawings_view::shape_to_view(d, &state.candles, interval))
                            .collect()
                    };
                    hit_test_drawings(
                        &visible,
                        state.view_start,
                        state.view_size,
                        y_lo,
                        y_hi,
                        canvas_w,
                        canvas_h,
                        state.y_axis_gap_px.get(),
                        canvas_x,
                        canvas_y,
                    )
                    .map(|(id, _handle)| id)
                })();
                let global = cx
                    .global::<crate::drawings::LastChartRightClick>()
                    .0
                    .clone();
                *global.borrow_mut() = Some(crate::drawings::RightClickTarget {
                    symbol,
                    drawing_id: target,
                });
            }),
        )
        .on_scroll_wheel(cx.listener(|this, ev: &ScrollWheelEvent, w, cx| {
            let Some(state) = this.chart_state.as_mut() else {
                return;
            };
            let delta_y = ev.delta.pixel_delta(w.line_height()).y.as_f32();
            if delta_y == 0.0 {
                return;
            }
            // Wheel-up (positive delta_y) zooms IN (smaller view_size); wheel-down zooms out.
            let factor = (-delta_y / SCROLL_ZOOM_RATE).exp();
            // Anchor the zoom at the rightmost candle slot of the viewport
            // (visible or virtual — i.e. inside the right buffer). Holding
            // this point fixed in candle-index space means scrolling controls
            // how far into the past the chart renders, while the rightmost
            // bar stays parked where it is.
            //
            // CRITICAL: clamp `view_size` BEFORE computing the new
            // `view_start`. If we clamp after, hitting the min/max would
            // still let the unclamped product through to `view_start`, then
            // `clamp()` would snap `view_size` back without un-shifting
            // `view_start` — drifting the right edge sideways every wheel
            // tick after the candle width hits its limit.
            let right_edge = state.view_start + state.view_size;
            let total = state.candles.len() as f32;
            let new_view_size = (state.view_size * factor)
                .clamp(CHART_MIN_VIEW.min(total), CHART_MAX_VIEW.min(total));
            state.view_size = new_view_size;
            state.view_start = right_edge - state.view_size;
            state.clamp();
            this.maybe_load_older(cx);
            cx.notify();
        }))
        // Inner wrapper for the candle paint primitive. Owns the chart's
        // right-click context menu so its hitbox is registered EARLY in
        // the prepaint order — `gpui::hit_test` iterates hitboxes in
        // reverse registration order and breaks on the first `BlockMouse`
        // (occluding) hitbox it encounters. The indicator chips register a
        // `.occlude()` hitbox during their prepaint LATER in this same
        // tree, so an early-registered chart-context-menu hitbox sits
        // BEHIND the chips and gets occluded when the cursor is over a
        // chip. (Putting the context_menu on the outer chart-canvas div
        // doesn't work — its hitbox is registered LAST, in front of every
        // chip, and reverse iteration visits it before any occluding chip
        // hitbox can break the loop.)
        .child(
            div()
                .size_full()
                .child(
            // Custom main-chart paint: continuous candle x-positions plus
            // auto-fit grid + axis labels. Replaces `CandlestickChart`
            // whose `ScaleBand` slot positioning made horizontal pan feel
            // discrete.
            canvas(
                |_, _, _| (),
                {
                    // Capture bullish/bearish before `main_chart_colors` is
                    // moved into the closure: `MainChartColors` isn't Copy
                    // and `paint_main_chart` consumes it.
                    let overlay_bullish = main_chart_colors.bullish;
                    let overlay_bearish = main_chart_colors.bearish;
                    move |bounds, _, window, cx| {
                        // Clip every paint call to the canvas's bounds.
                        // Without this, wicks of candles whose high/low
                        // sit outside the locked y range (or the chart's
                        // 10px top inset) paint past `chart_bottom` into
                        // the sub-pane below — visible as candle bleed.
                        // Mirrors what `render_drawings_overlay` does for
                        // drawing labels.
                        window.with_content_mask(Some(ContentMask { bounds }), |window| {
                        paint_main_chart(
                            bounds,
                            &paint_candles,
                            paint_start_idx,
                            paint_view_start,
                            paint_view_size,
                            y_lo,
                            y_hi,
                            paint_candle_interval_ms,
                            paint_y_axis_gap,
                            main_chart_colors,
                            window,
                            cx,
                        );
                        // Overlay indicators paint after candles + grid but
                        // before drawings, so user-drawn lines stay on top.
                        paint_overlay_indicators(
                            bounds,
                            paint_start_idx,
                            paint_candles.len(),
                            paint_view_start,
                            paint_view_size,
                            y_lo,
                            y_hi,
                            paint_y_axis_gap,
                            &paint_overlay_items,
                            overlay_bullish,
                            overlay_bearish,
                            window,
                        );
                        });
                    }
                },
            )
            .size_full(),
        )
        // Right-click → context menu shaped by the hit-test captured on
        // right-mouse-down. Drawing hit → per-drawing actions (Show/Hide,
        // Visible-on submenu, Delete) plus canvas defaults. Empty area →
        // canvas defaults only (Clear drawings on chart, Reset scale).
        // `action_context` routes dispatched actions up through this panel's
        // focus handle so multi-chart workspaces don't fight over them.
        // Hosted on the inner paint wrapper (not the outer chart-canvas
        // div) so its hitbox registers early in prepaint and indicator
        // chips with `.occlude()` can shadow it — see the wrapper's own
        // doc-comment above.
        .context_menu({
            let focus = focus.clone();
            move |menu, window, cx| {
                let mut menu = menu.action_context(focus.clone());
                let target = cx
                    .try_global::<crate::drawings::LastChartRightClick>()
                    .and_then(|g| g.0.borrow().clone());
                if let Some(target) = target {
                    if let Some(drawing_id) = target.drawing_id {
                        // Snapshot the drawing's `hidden`, `tf_filter`, and
                        // shape kind so the submenu builders don't re-borrow
                        // the service. `is_ray` gates the "Edit label" item
                        // since only horizontal rays carry a text label.
                        let (hidden, tf_filter, is_ray) = {
                            let svc = cx
                                .global::<crate::drawings::service::DrawingServiceHandle>()
                                .0
                                .clone();
                            let svc_read = svc.read(cx);
                            svc_read
                                .for_symbol(target.symbol.as_ref())
                                .iter()
                                .find(|d| d.id == drawing_id)
                                .map(|d| {
                                    let is_ray = matches!(
                                        &d.shape,
                                        crate::drawings::shapes::DrawingShape::HorizontalRay(_)
                                    );
                                    (d.hidden, d.tf_filter.clone(), is_ray)
                                })
                                .unwrap_or((false, None, false))
                        };
                        let sym_select = target.symbol.clone();
                        menu = menu.menu(
                            "Select",
                            Box::new(crate::drawings::actions::SelectDrawing {
                                symbol: sym_select,
                                id: drawing_id,
                            }),
                        );
                        if is_ray {
                            let sym_label = target.symbol.clone();
                            menu = menu.menu(
                                "Edit label",
                                Box::new(crate::drawings::actions::EditHorizontalRayText {
                                    symbol: sym_label,
                                    id: drawing_id,
                                }),
                            );
                        }
                        let sym_hidden = target.symbol.clone();
                        menu = menu.menu(
                            if hidden { "Show" } else { "Hide" },
                            Box::new(crate::drawings::actions::ToggleDrawingHidden {
                                symbol: sym_hidden,
                                id: drawing_id,
                            }),
                        );
                        // Per-drawing "Visible on" submenu (5 TF checkboxes).
                        let sym_for_sub = target.symbol.clone();
                        menu = menu.submenu("Visible on", window, cx, move |vis, _w, _cx| {
                            let mut vis = vis;
                            for tf in crate::services::market_data::Timeframe::ALL {
                                let checked = match &tf_filter {
                                    None => true,
                                    Some(set) => set.contains(tf.as_str()),
                                };
                                let prefix = if checked { "✓ " } else { "  " };
                                let label =
                                    SharedString::from(format!("{}{}", prefix, tf.as_str()));
                                vis = vis.menu(
                                    label,
                                    Box::new(crate::drawings::actions::ToggleDrawingTfFilter {
                                        symbol: sym_for_sub.clone(),
                                        id: drawing_id,
                                        tf: SharedString::from(tf.as_str()),
                                    }),
                                );
                            }
                            vis.separator().menu(
                                "Visible on all",
                                Box::new(crate::drawings::actions::ResetDrawingTfFilter {
                                    symbol: sym_for_sub.clone(),
                                    id: drawing_id,
                                }),
                            )
                        });
                        let sym_del = target.symbol.clone();
                        menu = menu.menu(
                            "Delete",
                            Box::new(crate::drawings::actions::DeleteDrawing {
                                symbol: sym_del,
                                id: drawing_id,
                            }),
                        );
                        menu = menu.separator();
                    }
                }
                menu.menu("Go to latest", Box::new(GoToLatest))
                    .menu(
                        "Clear drawings on chart",
                        Box::new(crate::drawings::actions::ClearChartDrawings),
                    )
                    .menu("Reset chart scale", Box::new(ResetChartScale))
            }
        }),
        )
        // Drawings paint between candles and the axis interaction zones —
        // visually above the chart, but the (non-interactive) overlay
        // doesn't intercept mouse events so the canvas's own handlers stay
        // in charge of tool dispatch.
        .child(drawings_overlay)
        // Text labels render as positioned divs above lines/rects.
        .children(text_labels)
        // Position price/R:R labels — wrapped in a clip surface that
        // matches the chart canvas area (excluding both axis gutters) so
        // labels drawn at a rect's right edge don't bleed past the y-axis
        // and overpaint the price labels there.
        .child(
            div()
                .absolute()
                .left_0()
                .top_0()
                .right(px(state.y_axis_gap_px.get()))
                .bottom(px(AXIS_GAP))
                .overflow_hidden()
                .children(position_labels),
        )
        // Main-pane indicator list (header chip + collapsible overlay
        // chips) absolute-anchored at the canvas top-left. Rendered after
        // position labels so chips sit on top of any position rect that
        // happens to land at the same corner.
        .child(render_main_indicator_list(state, cx))
        // Active text editor (Input). Above labels so its caret/selection
        // chrome isn't visually clipped by a stale label.
        .children(editor_overlay)
        // Axis zones go AFTER the chart so they sit on top in z-order and
        // get hit-tested first — their handlers `cx.stop_propagation()` to
        // keep mouse-down from also arming the canvas's pan drag.
        .child(right_axis)
        .child(bottom_axis)
        // Crosshair chrome (time + price labels, OHLC readout) on top of the
        // axes so the cursor's labels sit above the chart's static labels.
        .children(crosshair_chrome)
        // Live developing-bar guide (price ray, axis pill, countdown). Last
        // so the pill sits above the static y-axis labels on the right edge.
        .children(live_price_chrome)
        // Per-ray price pills on the y-axis. After live_price_chrome so a
        // ray drawn at the live price won't completely hide the live pill.
        .children(ray_price_chrome)
        // Floating "Go to latest" button (bottom-right). Last in the
        // children chain so it z-orders above the axis chrome and any
        // pills/labels that might sit at the corner.
        .children(go_to_latest_chrome);
    // Note: the chart-wide right-click context menu lives on the inner
    // paint wrapper div above (see its doc-comment for the z-order
    // reasoning). The outer canvas div intentionally has none.

    // Build one (splitter + sub-canvas) pair per pane indicator. The
    // splitter sits ABOVE its sub-pane; dragging it up grows the sub-pane,
    // dragging down shrinks it (the main canvas's `flex_1` absorbs the
    // remainder either way). Convention matches TradingView's per-pane
    // top-edge resize handle.
    let pane_grid_color = Hsla {
        a: 0.30,
        ..theme_border
    };
    let pane_label_color = theme_muted_foreground;
    let pane_bullish = theme_chart_bullish;
    let pane_bearish = theme_chart_bearish;
    // Pull these out once so the per-iter closure construction below doesn't
    // touch `paint_candles` (already moved into the main canvas closure).
    let pane_visible_count = paint_candles_len;
    let pane_start_idx = paint_start_idx;
    let pane_view_start = paint_view_start;
    let pane_view_size = paint_view_size;
    let pane_y_axis_gap = paint_y_axis_gap;
    // Snapshot the cross-pane cursor x once — it's the same value passed to
    // every sub-canvas closure for the vertical guide. Per-pane `hovered_y`
    // is derived inside the loop from `state.sub_cursor.id == instance_id`.
    let pane_cross_x = state.cross_cursor_x;
    let pane_sub_cursor = state.sub_cursor;
    let pane_cursor_idx = state.cursor_bar_index();
    let mut sub_panes: Vec<gpui::AnyElement> = Vec::new();
    for (instance_id, pane_height, item) in paint_pane_items.into_iter() {
        // Snapshot per-iter to move into closures.
        let item_for_paint = item;
        // Build the sub-pane chip overlay now while we still have an
        // immutable borrow on `state` — the canvas-building closures below
        // re-borrow `state` indirectly via the entity, so the chip needs
        // to be constructed up front and consumed into the sub-pane div.
        let pane_chip: Option<gpui::AnyElement> = state
            .indicators()
            .iter()
            .zip(state.indicator_outputs.iter())
            .find(|(i, _)| i.id == instance_id)
            .map(|(i, o)| render_indicator_chip(i, o, pane_cursor_idx, cx));
        // Horizontal y-guide + value-readout pill only paint when THIS pane
        // is the hovered one. `sub_cursor` carries (id, x, y); the y is
        // canvas-relative to whichever sub-pane wrote it.
        let pane_hovered_y = match pane_sub_cursor {
            Some((id, _x, y)) if id == instance_id => Some(y),
            _ => None,
        };
        let splitter_id_str = SharedString::from(format!("pane-splitter-{}", instance_id));
        let sub_canvas_id_str = SharedString::from(format!("pane-canvas-{}", instance_id));
        sub_panes.push(
            div()
                .id(splitter_id_str)
                .flex_none()
                .h(px(4.0))
                .w_full()
                .cursor_ns_resize()
                // Slight tinted bar so the resize handle is visible against
                // the panel background. Theme border at low alpha — same
                // visual weight as the chart's grid lines.
                .bg(Hsla {
                    a: 0.45,
                    ..theme_border
                })
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, ev: &MouseDownEvent, _w, cx| {
                        let Some(state) = this.chart_state.as_mut() else {
                            return;
                        };
                        // Look up CURRENT pane_height — paint_pane_items
                        // captured a copy at render time, but a sibling
                        // splitter might've adjusted it before this drag.
                        let current_h = state
                            .indicators()
                            .iter()
                            .find(|i| i.id == instance_id)
                            .and_then(|i| i.pane_height)
                            .unwrap_or(pane_height);
                        state.splitter_drag = Some(SplitterDrag {
                            instance_id,
                            start_y: ev.position.y.as_f32(),
                            start_height: current_h,
                        });
                        cx.stop_propagation();
                    }),
                )
                .into_any_element(),
        );
        sub_panes.push(
            div()
                .id(sub_canvas_id_str)
                .flex_none()
                .relative()
                .w_full()
                .h(px(pane_height))
                // Crosshair cursor mirrors the main canvas so the hover
                // affordance reads identically across panes.
                .cursor_crosshair()
                .on_prepaint({
                    let entity = entity.clone();
                    move |bounds, _, cx| {
                        // Stash this sub-pane's bounds so the on_mouse_move
                        // handler can convert window-relative event coords
                        // into canvas-relative cursor (x, y).
                        entity.update(cx, |this, _cx| {
                            if let Some(state) = this.chart_state.as_mut() {
                                state.pane_bounds.insert(instance_id, bounds);
                            }
                        });
                    }
                })
                .on_mouse_move(cx.listener(move |this, ev: &MouseMoveEvent, _w, cx| {
                    let Some(state) = this.chart_state.as_mut() else {
                        return;
                    };
                    let Some(bounds) = state.pane_bounds.get(&instance_id).copied() else {
                        return;
                    };
                    let canvas_x = ev.position.x.as_f32() - bounds.origin.x.as_f32();
                    let canvas_y = ev.position.y.as_f32() - bounds.origin.y.as_f32();
                    let canvas_w = bounds.size.width.as_f32();
                    let canvas_h = bounds.size.height.as_f32();
                    if canvas_w <= 0.0 || canvas_h <= 0.0 {
                        return;
                    }
                    // Sub-pane hover wipes the main-pane crosshair (only one
                    // pane is "active" at a time), then sets the cross-pane
                    // shared x + this pane's y for the horizontal guide.
                    state.cursor = None;
                    state.cross_cursor_x = Some(canvas_x);
                    state.sub_cursor = Some((instance_id, canvas_x, canvas_y));
                    cx.notify();
                }))
                .on_hover({
                    let entity = entity.clone();
                    move |&entered, _, cx| {
                        if entered {
                            return;
                        }
                        // Cursor left this sub-pane. Only clear if our id is
                        // the one currently held — moving to a sibling pane
                        // would mean the sibling already overwrote sub_cursor,
                        // and we don't want to clobber its state.
                        entity.update(cx, |this, cx| {
                            if let Some(state) = this.chart_state.as_mut() {
                                let mut changed = false;
                                if let Some((id, _, _)) = state.sub_cursor {
                                    if id == instance_id {
                                        state.sub_cursor = None;
                                        changed = true;
                                    }
                                }
                                // Cross-pane x stays alive while another pane
                                // is hovered. If neither main nor any sub-pane
                                // is hovered we let the cross x linger; the
                                // outer panel root has no on_hover wired (v1
                                // limitation — small visual artifact on exit).
                                if state.cursor.is_none()
                                    && state.sub_cursor.is_none()
                                    && state.cross_cursor_x.take().is_some()
                                {
                                    changed = true;
                                }
                                if changed {
                                    cx.notify();
                                }
                            }
                        });
                    }
                })
                .child(
                    // `canvas` the local div is bound below; use the
                    // fully-qualified path to reach the gpui paint helper.
                    gpui::canvas(
                        |_, _, _| (),
                        move |bounds, _, window, cx| {
                            paint_sub_pane(
                                bounds,
                                pane_start_idx,
                                pane_visible_count,
                                pane_view_start,
                                pane_view_size,
                                pane_y_axis_gap,
                                &item_for_paint,
                                pane_bullish,
                                pane_bearish,
                                pane_grid_color,
                                pane_label_color,
                                pane_cross_x,
                                pane_hovered_y,
                                window,
                                cx,
                            );
                        },
                    )
                    .size_full(),
                )
                // Sub-pane chip overlay: the lone indicator's chip pinned
                // at top-left of its own pane. Doubles as the un-hide
                // affordance when the pane is muted (paint_sub_pane is a
                // no-op then, but the chip still renders).
                .children(pane_chip.map(|chip| {
                    div()
                        .absolute()
                        .top(px(4.0))
                        .left(px(4.0))
                        .child(chip)
                        .into_any_element()
                }))
                .into_any_element(),
        );
    }

    v_flex()
        .id("chart-panel-root")
        .size_full()
        // No bottom padding so the chart-canvas reaches the panel's bottom
        // edge — otherwise the panel-level padding shows below the x-axis
        // chrome as a visible gap.
        .pt_3()
        .pl_3()
        .pr_3()
        .gap_2()
        // Splitter drag handlers attach here so the cursor can stray off
        // the 4px splitter bar and still drive the resize — mouse_move on
        // the splitter alone would die the moment the cursor crossed its
        // 4px boundary. Limitation: drag also dies when the cursor exits
        // the panel root entirely (v1 — a global pointer-capture would
        // fix it but isn't needed for the common adjust gesture).
        .on_mouse_move(
            cx.listener(|this, ev: &MouseMoveEvent, _w, cx| {
                let Some(state) = this.chart_state.as_mut() else {
                    return;
                };
                let Some(drag) = state.splitter_drag else {
                    return;
                };
                // Splitter sits ABOVE its sub-pane. Drag up (delta_y < 0)
                // grows the pane; drag down (delta_y > 0) shrinks it.
                let delta_y = ev.position.y.as_f32() - drag.start_y;
                let new_h = drag.start_height - delta_y;
                state.set_indicator_pane_height(drag.instance_id, new_h);
                cx.notify();
            }),
        )
        .on_mouse_up(
            MouseButton::Left,
            cx.listener(|this, _ev, _w, cx| {
                let Some(state) = this.chart_state.as_mut() else {
                    return;
                };
                if state.splitter_drag.take().is_some() {
                    cx.notify();
                }
            }),
        )
        .child(
            h_flex()
                // `flex_none` pins the header row at its natural height;
                // without it the canvas's `flex_1` can compress the header
                // and the canvas paint encroaches over the symbol/timeframe
                // controls.
                .flex_none()
                .w_full()
                .gap_3()
                .items_center()
                .child(symbol_button)
                .child(timeframe_btn)
                // `+ Indicator` button — dispatches `OpenIndicatorPicker`
                // which the workspace resolves to this chart via the
                // `LastFocusedChart` global (already kept fresh by the
                // panel's mouse-down handler). Cmd-I / Ctrl-I is the
                // keyboard equivalent (workspace-scoped).
                .child(
                    Button::new("chart-add-indicator")
                        .label(SharedString::from("+ Indicator"))
                        .small()
                        .ghost()
                        .on_click(|_ev, window, cx| {
                            window.dispatch_action(
                                Box::new(crate::indicator_picker::OpenIndicatorPicker),
                                cx,
                            );
                        }),
                )
                // Indicator chips no longer live in the toolbar — the
                // main-pane `Indicators (N) ▼` list at the canvas's top-left
                // is the visual home for overlay-placed indicators, and
                // pane-placed indicators wear their chip on their own
                // sub-pane. The toolbar keeps only actions (+ Indicator).
                // Company name + exchange takes the leftover space and is the
                // first thing to give up width when the panel shrinks — same
                // `flex_1().min_w_0().truncate()` idiom used in symbol_picker.
                // `min_w_0` is the load-bearing bit; without it the flex item
                // refuses to shrink below its content width and pushes the
                // status badge off the right edge.
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_color(theme_muted_foreground)
                        .text_sm()
                        .child(format!("{} · {}", header_name, header_exchange)),
                )
                .child(
                    h_flex()
                        .flex_none()
                        .gap_1p5()
                        .items_center()
                        .child(div().size_2().rounded_full().bg(badge_color))
                        .child(div().text_xs().text_color(badge_color).child(badge_label)),
                ),
        )
        // Chart stack: main candle canvas (flex_1) + (splitter + sub_canvas)
        // pairs for each pane indicator. Main canvas keeps its own `flex_1`
        // so it absorbs whatever space the sub-panes (and their splitters)
        // don't claim — including resize-driven changes to pane_height.
        .child(
            v_flex()
                .flex_1()
                .min_h_0()
                .w_full()
                .child(canvas)
                .children(sub_panes),
        )
}
