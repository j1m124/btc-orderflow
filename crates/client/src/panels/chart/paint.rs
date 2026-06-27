//! Paint pipeline for the chart panel. Lifted out of `chart.rs` to keep the
//! parent module focused on state + interaction handlers. The parent still owns
//! `index_to_screen` / `price_to_screen` (coordinate math is shared with
//! hit-testing) and the `Drawing` enum (mutated by edit drags that live in
//! `chart.rs`'s mouse handlers).
//!
//! Split into focused submodules behind this facade:
//! - [`main_chart`] — candles + grid + axis labels (the entry point).
//! - [`footprint_render`] — cluster / profile / wireframe render layers.
//! - [`drawings_overlay`] — drawings + preview + selection chrome + crosshair.
//! - [`overlay_indicators`] — line / band / histogram / VP overlays.
//! - [`sub_panes`] — one canvas per pane indicator (incl. BarStat).
//!
//! This module itself holds only the paint primitives shared across those
//! submodules (`fill_rect`, the slot-width gap policy, `pick_nice_y_step`,
//! `band_y`, `paint_centred_text`, `paint_line_series`). Submodules reach them
//! via `super::`.

use gpui::{
    App, BorderStyle, Bounds, Corners, Edges, Hsla, PaintQuad, PathBuilder, Pixels, Point,
    SharedString, TextRun, Window, point, px,
};

use super::index_to_screen;

mod drawings_overlay;
mod footprint_render;
mod heatmap;
mod liq_heatmap;
mod main_chart;
mod overlay_indicators;
mod sub_panes;

pub(super) use drawings_overlay::{DrawingColors, render_drawings_overlay};
pub use heatmap::{
    COLOR_RANGE_MAX, COLOR_RANGE_MIN, HEATMAP_DEPTH, HeatmapLayer, HeatmapRect, HeatmapSettings,
    paint_heatmap,
};
pub use liq_heatmap::{LIQ_COLOR_RANGE_MAX, LIQ_COLOR_RANGE_MIN, LiqHeatmapLayer};
pub(super) use main_chart::{MainChartColors, paint_main_chart};
pub(super) use overlay_indicators::{OverlayPaintItem, paint_overlay_indicators};
pub(super) use sub_panes::{PanePaintItem, paint_sub_pane};

/// Paint a solid axis-aligned rectangle as a single GPU quad. Used for grid
/// lines, candle wicks, and bodies — all of which were previously stroked
/// `PathBuilder` paths. A quad is an instanced primitive (no per-call
/// tessellation or heap allocation), so this keeps paint cost flat as the
/// visible-candle count grows on zoom-out. `x`/`y_top` are canvas-relative.
#[inline]
fn fill_rect(
    window: &mut Window,
    origin: Point<Pixels>,
    x: f32,
    w: f32,
    y_top: f32,
    h: f32,
    color: Hsla,
) {
    window.paint_quad(PaintQuad {
        bounds: Bounds {
            origin: point(px(x) + origin.x, px(y_top) + origin.y),
            size: gpui::size(px(w), px(h)),
        },
        corner_radii: Corners::default(),
        background: color.into(),
        border_widths: Edges::default(),
        border_color: gpui::transparent_black(),
        border_style: BorderStyle::default(),
    });
}

/// Nice-step picker for y-axis ticks: returns a value of the form
/// {1, 2, 5} × 10^k closest to `range / target_count`. The classic
/// d3-scale algorithm — produces tick values that read cleanly.
fn pick_nice_y_step(range: f64, target_count: usize) -> f64 {
    if !range.is_finite() || range <= 0.0 {
        return 1.0;
    }
    let raw = range / (target_count.max(1) as f64);
    let exp = raw.log10().floor();
    let pow10 = 10f64.powf(exp);
    let frac = raw / pow10;
    let nice_frac = if frac < 1.5 {
        1.0
    } else if frac < 3.5 {
        2.0
    } else if frac < 7.5 {
        5.0
    } else {
        10.0
    };
    nice_frac * pow10
}

/// Per-bar gap policy. At default zoom (`slot_width` ~5–15 px) the gap is
/// ~30% of the slot, matching the classic candlestick look. When the user
/// zooms way in (footprint inspection often pushes `slot_width` past 50 px),
/// scaling the gap proportionally would leave huge empty stripes between
/// bars — so the gap is capped at `MAX_BAR_GAP` and the body grows to fill
/// the rest. Floor is 1 px so bars never visually merge.
const BAR_GAP_FRACTION: f32 = 0.30;
const MAX_BAR_GAP: f32 = 5.0;
const MIN_BAR_GAP: f32 = 1.0;
const SIDE_OHLC_FRACTION: f32 = 0.22;

/// Body width for a candle / wireframe / cluster bar centred in `slot_width`.
/// Uses the gap policy above (capped, with a floor) so both the candlestick
/// render and the footprint footprint layout share one source of truth.
#[inline]
fn slot_body_width(slot_width: f32) -> f32 {
    let gap = (slot_width * BAR_GAP_FRACTION).clamp(MIN_BAR_GAP, MAX_BAR_GAP);
    (slot_width - gap).max(1.0)
}

/// Per-side edge pad: half the gap left after the body claims its width.
#[inline]
fn slot_edge_pad(slot_width: f32) -> f32 {
    (slot_width - slot_body_width(slot_width)) * 0.5
}

/// Shape + paint a label centred inside `(x, w) × (y, h)`. Hoisted so the
/// per-side bid/ask path and the single-string path share one shaper.
#[allow(clippy::too_many_arguments)]
fn paint_centred_text(
    window: &mut Window,
    cx: &mut App,
    origin: Point<Pixels>,
    x: f32,
    w: f32,
    y: f32,
    h: f32,
    color: Hsla,
    text: &str,
) {
    if text.is_empty() {
        return;
    }
    let label = SharedString::from(text.to_string());
    let run = TextRun {
        len: label.len(),
        font: window.text_style().font(),
        color,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let line = window
        .text_system()
        .shape_line(label, px(10.0), &[run], None);
    let tw = line.width().as_f32();
    let tx = x + (w - tw) * 0.5;
    let ty = y + (h - 10.0) * 0.5;
    let _ = line.paint(
        point(px(tx) + origin.x, px(ty) + origin.y),
        px(10.0),
        gpui::TextAlign::Left,
        None,
        window,
        cx,
    );
}

/// Helper: paint a `Series` as a polyline using `PathBuilder::stroke`.
/// Segments only connect adjacent `Some` values; a `None` breaks the line.
/// `chart_top` / `chart_bottom` define the vertical pixel band the line maps
/// into; callers pass the main pane's [10, canvas_h - AXIS_GAP] band or the
/// sub-pane's [2, canvas_h] band depending on which canvas they're painting.
#[allow(clippy::too_many_arguments)]
fn paint_line_series(
    series: &[Option<f64>],
    start_idx: usize,
    visible_end: usize,
    view_start: f32,
    view_size: f32,
    canvas_w: f32,
    y_axis_gap: f32,
    y_lo: f64,
    y_hi: f64,
    chart_top: f32,
    chart_bottom: f32,
    color: Hsla,
    width: f32,
    origin: Point<Pixels>,
    window: &mut Window,
) {
    let chart_w = (canvas_w - y_axis_gap).max(0.0);
    let lo = start_idx;
    let hi = visible_end.min(series.len());
    if hi <= lo + 1 {
        return;
    }
    let mut pb = PathBuilder::stroke(px(width));
    let mut has_anchor = false;
    for i in lo..hi {
        let Some(v) = series[i] else {
            has_anchor = false;
            continue;
        };
        let x = index_to_screen(view_start, view_size, i as f32, canvas_w, y_axis_gap);
        let y = band_y(y_lo, y_hi, v, chart_top, chart_bottom);
        // Clip points outside the chart band — they'd still draw cleanly
        // via gpui's path mask, but skipping saves work.
        if x < -10.0 || x > chart_w + 10.0 || y < chart_top - 10.0 || y > chart_bottom + 10.0 {
            // We still need to keep `has_anchor` correct so segments resume
            // when we re-enter the band, so route through the same point.
        }
        let p = point(px(x) + origin.x, px(y) + origin.y);
        if has_anchor {
            pb.line_to(p);
        } else {
            pb.move_to(p);
            has_anchor = true;
        }
    }
    if let Ok(path) = pb.build() {
        window.paint_path(path, color);
    }
}

/// Inverse-affine map from a price/value `v` (in `[y_lo, y_hi]`) into the
/// pixel band `[chart_top, chart_bottom]`. Mirrors `price_to_screen` from
/// `chart.rs`, but with an explicit band so sub-panes (whose top/bottom
/// differ from the main pane) can reuse it.
#[inline]
fn band_y(y_lo: f64, y_hi: f64, v: f64, chart_top: f32, chart_bottom: f32) -> f32 {
    let range = y_hi - y_lo;
    if range.abs() < 1e-9 {
        return (chart_top + chart_bottom) / 2.0;
    }
    let t = ((y_hi - v) / range) as f32;
    chart_top + t * (chart_bottom - chart_top)
}

