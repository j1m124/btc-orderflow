//! Paint pipeline for the chart panel: candles + grid + axis labels (main
//! chart canvas) and the drawings overlay. Lifted out of `chart.rs` to keep
//! the parent module focused on state + interaction handlers. The parent
//! still owns `index_to_screen` / `price_to_screen` (coordinate math is
//! shared with hit-testing) and the `Drawing` enum (mutated by edit drags
//! that live in `chart.rs`'s mouse handlers).

use chrono::{DateTime, Datelike as _, FixedOffset, TimeZone as _};
use gpui::{
    App, BorderStyle, Bounds, ContentMask, Corners, Edges, Hsla, IntoElement, PaintQuad,
    ParentElement as _, PathBuilder, Pixels, Point, SharedString, Styled as _, TextRun, Window,
    canvas, div, point, px,
};
use gpui_component::plot::AXIS_GAP;

use super::footprint::{
    ColorScope, FootprintParams, RenderKind, RenderMetric, TextMetric, WireframeVariant,
};
use super::{Drawing, DrawingId, index_to_screen, price_to_screen};
use crate::indicators::IndicatorOutput;
use crate::persistence::VolumeUnit;
use crate::services::market_data::{Candle, FootprintCell};

/// Colours fed to the drawings overlay so its paint closure doesn't need
/// access to `cx` at paint time.
#[derive(Clone, Copy)]
pub(super) struct DrawingColors {
    pub line: Hsla,
    pub rect_fill: Hsla,
    pub rect_border: Hsla,
    pub ring: Hsla,
    pub background: Hsla,
    pub bullish: Hsla,
    pub bearish: Hsla,
    pub muted: Hsla,
}

// ============================================================================
// Main chart paint — candles + grid + axis labels
// ============================================================================
//
// Replaces gpui-component's `CandlestickChart` widget so candle positions are
// continuous in `view_start`. The widget binned candles into `ScaleBand`
// slots which meant fractional view_start changes had zero visual effect —
// horizontal pan felt chunky, and drawings (which already used
// `index_to_screen`) visually desynced from candles during pan.

pub(super) struct MainChartColors {
    pub bullish: Hsla,
    pub bearish: Hsla,
    pub grid: Hsla,
    pub label: Hsla,
    /// Full-contrast text color (theme `foreground`) for cell labels that
    /// must read on top of coloured fill backgrounds — flips white/black
    /// with the theme automatically. Distinct from `label` (muted) which
    /// is for the axis gutters.
    pub cell_text: Hsla,
    /// Solid fill for the axis gutters (right + bottom). Painted on top of
    /// the chart area so drawings that bleed past the chart-canvas
    /// boundary don't show through alongside the axis labels.
    pub axis_bg: Hsla,
    /// Thin divider between the chart area and the axis gutters.
    pub axis_border: Hsla,
}

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

/// Minimum horizontal gap (px) between adjacent x-axis labels. A safety net on
/// top of step selection so labels never crowd even when bucket boundaries fall
/// close together (e.g. short months, or a year label butting a month label).
const MIN_LABEL_GAP_PX: f32 = 55.0;

/// A calendar-aware x-axis label step. Minute multiples cover intraday; the
/// day/month/year variants keep the axis readable when the chart spans weeks to
/// decades (where minute math can't express "every month/year").
#[derive(Clone, Copy, PartialEq, Eq)]
enum TimeStep {
    Minutes(i64),
    Days(i64),
    Months(i64),
    Years(i64),
}

/// Approximate duration of a step in ms, for sizing a step against the visible
/// span. Months/years use average lengths — only used for selection, never for
/// placing labels (that uses real calendar buckets).
fn step_duration_ms(step: TimeStep) -> i64 {
    match step {
        TimeStep::Minutes(m) => m * 60_000,
        TimeStep::Days(d) => d * 86_400_000,
        TimeStep::Months(mo) => mo * 2_629_800_000, // ~30.44 d
        TimeStep::Years(y) => y * 31_557_600_000,   // 365.25 d
    }
}

/// Choose the smallest ladder step whose span is at least the per-label target
/// AND at least the candle interval (never label finer than the bars).
fn pick_time_step(view_ms: i64, target_labels: usize, min_interval_ms: i64) -> TimeStep {
    const LADDER: &[TimeStep] = &[
        TimeStep::Minutes(1),
        TimeStep::Minutes(2),
        TimeStep::Minutes(5),
        TimeStep::Minutes(10),
        TimeStep::Minutes(15),
        TimeStep::Minutes(30),
        TimeStep::Minutes(60),
        TimeStep::Minutes(120),
        TimeStep::Minutes(180),
        TimeStep::Minutes(240),
        TimeStep::Minutes(360),
        TimeStep::Minutes(720),
        TimeStep::Days(1),
        TimeStep::Days(2),
        TimeStep::Days(7),
        TimeStep::Days(14),
        TimeStep::Months(1),
        TimeStep::Months(3),
        TimeStep::Months(6),
        TimeStep::Years(1),
        TimeStep::Years(2),
        TimeStep::Years(5),
        TimeStep::Years(10),
    ];
    let want = (view_ms / target_labels.max(1) as i64).max(min_interval_ms);
    LADDER
        .iter()
        .copied()
        .find(|s| step_duration_ms(*s) >= want)
        .unwrap_or(TimeStep::Years(10))
}

/// Monotonic bucket id for `dt` under `step`. Consecutive bars with different
/// ids straddle a step boundary, so the later one gets a label. `local_ms` is
/// the bar's wall-clock time in ms (UTC ms + local offset).
fn bucket_id(dt: &DateTime<FixedOffset>, local_ms: i64, step: TimeStep) -> i64 {
    match step {
        TimeStep::Minutes(m) => local_ms.div_euclid(m * 60_000),
        // Week: align to Monday (epoch day 0 is a Thursday → shift by 4).
        TimeStep::Days(7) => (local_ms.div_euclid(86_400_000) - 4).div_euclid(7),
        TimeStep::Days(d) => local_ms.div_euclid(86_400_000).div_euclid(d),
        TimeStep::Months(mo) => (dt.year() as i64 * 12 + dt.month0() as i64).div_euclid(mo),
        TimeStep::Years(y) => (dt.year() as i64).div_euclid(y),
    }
}

/// Label text for `dt`, formatted to the step's granularity: intraday shows the
/// time (or the date on the first bar of a new day); day shows "MMM d"; month
/// shows "MMM" (or the year each January); year shows "YYYY".
fn format_label(dt: &DateTime<FixedOffset>, step: TimeStep, day_changed: bool) -> String {
    match step {
        TimeStep::Minutes(_) => {
            if day_changed {
                dt.format("%b %-d").to_string()
            } else {
                dt.format("%H:%M").to_string()
            }
        }
        TimeStep::Days(_) => dt.format("%b %-d").to_string(),
        TimeStep::Months(_) => {
            if dt.month() == 1 {
                dt.format("%Y").to_string()
            } else {
                dt.format("%b").to_string()
            }
        }
        TimeStep::Years(_) => dt.format("%Y").to_string(),
    }
}

/// Render a Fibonacci level (0..=1) as the short label shown next to its
/// horizontal line — integer percentages on whole ratios (0, 50, 100) and
/// the canonical three-decimal form on the fractional levels.
fn format_fib_level(level: f32) -> String {
    if (level - 0.0).abs() < 1e-4 {
        "0".to_string()
    } else if (level - 0.5).abs() < 1e-4 {
        "0.5".to_string()
    } else if (level - 1.0).abs() < 1e-4 {
        "1".to_string()
    } else {
        format!("{:.3}", level)
    }
}

/// Paint the main chart canvas: horizontal y-grid, vertical x-grid, the
/// active render (candles or footprint), and axis labels in the right +
/// bottom gutters. Runs in a single `canvas` paint pass — z-order is
/// determined by paint sequence (grid → render → labels).
///
/// `render_kind` selects the main render: Candlestick paints wicks + bodies
/// the classical way; Cluster / Profile paint a wireframe-then-cells stack
/// driven by `footprint_params` + `footprint_cells`. When a footprint kind
/// is selected but no cells are loaded yet, the renderer falls back to
/// candlesticks so the chart never goes blank.
#[allow(clippy::too_many_arguments)]
pub(super) fn paint_main_chart(
    bounds: Bounds<Pixels>,
    candles: &[Candle],
    start_idx: usize,
    view_start: f32,
    view_size: f32,
    y_lo: f64,
    y_hi: f64,
    candle_interval_ms: i64,
    y_axis_gap: f32,
    colors: MainChartColors,
    render_kind: RenderKind,
    render_visible: bool,
    footprint_params: Option<&FootprintParams>,
    footprint_cells: &[FootprintCell],
    volume_unit: VolumeUnit,
    window: &mut Window,
    cx: &mut App,
) {
    let canvas_w = bounds.size.width.as_f32();
    let canvas_h = bounds.size.height.as_f32();
    let chart_w = (canvas_w - y_axis_gap).max(0.0);
    let chart_top = 10.0_f32;
    let chart_bottom = (canvas_h - AXIS_GAP).max(chart_top + 1.0);
    let origin = bounds.origin;

    // -- y-axis ticks (price levels) --
    //
    // Footprint modes snap the step to a multiple of `bucket` so cells line
    // up with grid lines; in candlestick mode (or until cells load) we fall
    // back to the classic d3 nice-step picker.
    let target_y_count = ((chart_bottom - chart_top) / 50.0).floor().max(2.0) as usize;
    let bucket_snap = match render_kind {
        RenderKind::Cluster | RenderKind::Profile => footprint_params
            .map(|p| p.bucket)
            .filter(|b| FootprintParams::bucket_is_valid(*b)),
        RenderKind::Candlestick => None,
    };
    let y_step = match bucket_snap {
        Some(bucket) => pick_bucket_y_step(y_hi - y_lo, target_y_count, bucket),
        None => pick_nice_y_step(y_hi - y_lo, target_y_count),
    };
    let mut y_ticks: Vec<f64> = Vec::new();
    if y_step > 0.0 && y_step.is_finite() {
        // When snapping, anchor the first tick to a bucket boundary so the
        // whole tick column lines up with bar cells across the chart.
        let anchor = bucket_snap.unwrap_or(y_step);
        let first = (y_lo / anchor).ceil() * anchor;
        let mut t = first;
        // Cap iterations so a degenerate range can never spin forever.
        for _ in 0..200 {
            if t > y_hi + 1e-9 {
                break;
            }
            y_ticks.push(t);
            t += y_step;
        }
    }

    // -- x-axis ticks (time labels) --
    //
    // Pick a calendar-aware step (minutes … years) sized to the visible span and
    // never finer than the candle interval, then label the FIRST visible bar of
    // each step "bucket". Bucket-crossing (rather than exact-time matching) means
    // session gaps / weekends still land a label on the next bar, and a minimum
    // pixel gap guarantees labels never crowd — for any timeframe or zoom level.
    let target_x_count = (chart_w / 90.0).floor().max(2.0) as usize;
    let view_ms = (view_size as f64 * candle_interval_ms as f64) as i64;
    let step = pick_time_step(view_ms, target_x_count, candle_interval_ms);
    let mut x_ticks: Vec<(f32, SharedString)> = Vec::new();
    // Resolve the active TZ offset ONCE (from the first visible bar) and reuse
    // it via cheap fixed-offset arithmetic. A per-candle TZ lookup here was
    // O(visible candles) of timezone work — costly when thousands are on screen.
    // The offset honours the user's Settings → Timezone choice; Auto falls
    // back to OS local (the historical behaviour).
    let tz_offset: Option<FixedOffset> = candles
        .first()
        .map(|c| crate::prefs::offset_for(cx, c.open_time));
    if let Some(offset) = tz_offset {
        let offset_ms = offset.local_minus_utc() as i64 * 1000;
        let mut prev_bucket: Option<i64> = None;
        let mut prev_day: Option<i64> = None;
        let mut last_label_x = f32::NEG_INFINITY;
        for (i, candle) in candles.iter().enumerate() {
            let local_ms = candle.open_time + offset_ms;
            let local_day = local_ms.div_euclid(86_400_000);
            let Some(dt) = offset.timestamp_millis_opt(candle.open_time).single() else {
                continue;
            };
            let bucket = bucket_id(&dt, local_ms, step);
            // Label the bar where the bucket first changes; track day changes so
            // an intraday step can show the date on the first bar of a new day.
            let crossed = prev_bucket.is_some_and(|p| p != bucket);
            let day_changed = prev_day.is_some_and(|p| p != local_day);
            prev_bucket = Some(bucket);
            prev_day = Some(local_day);
            if !crossed {
                continue;
            }
            let center_x = index_to_screen(
                view_start,
                view_size,
                (start_idx + i) as f32,
                canvas_w,
                y_axis_gap,
            );
            if center_x < 0.0 || center_x > chart_w {
                continue;
            }
            if center_x - last_label_x < MIN_LABEL_GAP_PX {
                continue;
            }
            last_label_x = center_x;
            x_ticks.push((center_x, format_label(&dt, step, day_changed).into()));
        }
    }

    // -- 1. horizontal grid (behind candles) -- 1px-tall quads.
    for &y_val in &y_ticks {
        let y = price_to_screen(y_lo, y_hi, y_val, canvas_h);
        if y < chart_top || y > chart_bottom {
            continue;
        }
        fill_rect(window, origin, 0.0, chart_w, y, 1.0, colors.grid);
    }

    // -- 2. vertical grid (at each time label) -- 1px-wide quads.
    for &(x, _) in &x_ticks {
        fill_rect(
            window,
            origin,
            x,
            1.0,
            chart_top,
            (chart_bottom - chart_top).max(0.0),
            colors.grid,
        );
    }

    // -- 3. main render layer --
    //
    // Dispatch on the chart's active render kind. Candlestick paints the
    // classic wick+body pipeline; the footprint kinds fall back to candles
    // until per-bucket cells are loaded, then paint a wireframe (Behind /
    // SideOhlc / None per `params.wireframe`) plus cluster or profile cells.
    //
    // `render_visible` gates this whole layer — false suppresses the
    // candle / cell / wireframe paint while the grid + axes + overlays
    // keep painting (driven by the synthetic render chip's eye toggle).
    let render_cells_available = matches!(render_kind, RenderKind::Cluster | RenderKind::Profile)
        && !footprint_cells.is_empty()
        && footprint_params.is_some();
    if !render_visible {
        // Skip the main render layer entirely; everything else still paints.
    } else if !render_cells_available {
        paint_candle_bodies(
            origin,
            candles,
            start_idx,
            view_start,
            view_size,
            canvas_w,
            canvas_h,
            chart_w,
            y_lo,
            y_hi,
            y_axis_gap,
            colors.bullish,
            colors.bearish,
            window,
        );
    } else {
        let params = footprint_params.expect("guarded above");
        // Wireframe paints first so cells/bars land on top of it (Behind
        // semantics). SideOhlc renders the candle alongside cells rather than
        // behind — `paint_bar_wireframes` handles the layout split internally.
        paint_bar_wireframes(
            origin,
            candles,
            start_idx,
            view_start,
            view_size,
            canvas_w,
            canvas_h,
            chart_w,
            y_lo,
            y_hi,
            y_axis_gap,
            params.wireframe,
            colors.bullish,
            colors.bearish,
            window,
        );
        match render_kind {
            RenderKind::Cluster => {
                paint_cluster_cells(
                    origin,
                    candles,
                    footprint_cells,
                    start_idx,
                    view_start,
                    view_size,
                    canvas_w,
                    canvas_h,
                    chart_w,
                    y_lo,
                    y_hi,
                    y_axis_gap,
                    params,
                    colors.bullish,
                    colors.bearish,
                    colors.cell_text,
                    volume_unit,
                    window,
                    cx,
                );
            }
            RenderKind::Profile => {
                paint_profile_bars(
                    origin,
                    candles,
                    footprint_cells,
                    start_idx,
                    view_start,
                    view_size,
                    canvas_w,
                    canvas_h,
                    chart_w,
                    y_lo,
                    y_hi,
                    y_axis_gap,
                    params,
                    colors.bullish,
                    colors.bearish,
                    colors.cell_text,
                    volume_unit,
                    window,
                    cx,
                );
            }
            RenderKind::Candlestick => unreachable!("guarded by render_cells_available"),
        }
    }

    // -- 3.5 axis gutters --
    //
    // Paint solid backgrounds for the right (price) and bottom (time) gutters
    // BEFORE the labels so any drawing pixels that overflow the chart canvas
    // (e.g. text shaped near the right edge) are masked by the axis chrome
    // and don't visually collide with the price/time labels.
    if y_axis_gap > 0.0 {
        fill_rect(
            window,
            origin,
            chart_w,
            y_axis_gap,
            0.0,
            canvas_h,
            colors.axis_bg,
        );
        // 1px divider on the chart side.
        fill_rect(
            window,
            origin,
            chart_w,
            1.0,
            0.0,
            canvas_h,
            colors.axis_border,
        );
    }
    if AXIS_GAP > 0.0 {
        fill_rect(
            window,
            origin,
            0.0,
            chart_w,
            chart_bottom,
            AXIS_GAP,
            colors.axis_bg,
        );
        fill_rect(
            window,
            origin,
            0.0,
            chart_w,
            chart_bottom,
            1.0,
            colors.axis_border,
        );
    }

    // -- 4. y-axis labels (right gutter) --
    for &y_val in &y_ticks {
        let y = price_to_screen(y_lo, y_hi, y_val, canvas_h);
        if y < chart_top - 6.0 || y > chart_bottom + 6.0 {
            continue;
        }
        let label = SharedString::from(format!("{:.2}", y_val));
        let run = TextRun {
            len: label.len(),
            font: window.text_style().font(),
            color: colors.label,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let line = window
            .text_system()
            .shape_line(label, px(10.0), &[run], None);
        let _ = line.paint(
            point(px(chart_w + 4.0) + origin.x, px(y - 5.0) + origin.y),
            px(10.0),
            gpui::TextAlign::Left,
            None,
            window,
            cx,
        );
    }

    // -- 5. x-axis labels (bottom gutter) --
    for (x, label) in &x_ticks {
        let run = TextRun {
            len: label.len(),
            font: window.text_style().font(),
            color: colors.label,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let line = window
            .text_system()
            .shape_line(label.clone(), px(10.0), &[run], None);
        // Centre the label under its tick, clamped to the chart area so
        // the first / last labels don't get clipped by the gutter edges.
        let label_w = line.width().as_f32();
        let label_x = (*x - label_w / 2.0).clamp(0.0, (chart_w - label_w).max(0.0));
        let _ = line.paint(
            point(px(label_x) + origin.x, px(chart_bottom + 4.0) + origin.y),
            px(10.0),
            gpui::TextAlign::Left,
            None,
            window,
            cx,
        );
    }
}

// ============================================================================
// Render-mode dispatch helpers — candle / wireframe / cluster / profile
// ============================================================================
//
// All three live behind `paint_main_chart`'s render-kind switch. They share
// the same coordinate plumbing (slot width, `index_to_screen`,
// `price_to_screen`) so a Cluster / Profile bar always lines up with its
// candlestick counterpart at the same view.

/// Snap a price-range step to a bucket multiple — produces a step of the
/// form `n × bucket` (n ≥ 1) closest to `range / target_count`. Used for the
/// y-axis grid in footprint modes so tick lines fall on bucket boundaries.
fn pick_bucket_y_step(range: f64, target_count: usize, bucket: f64) -> f64 {
    if !range.is_finite() || range <= 0.0 || !bucket.is_finite() || bucket <= 0.0 {
        return bucket.max(1.0);
    }
    let raw = range / (target_count.max(1) as f64);
    // Smallest multiple of bucket that is ≥ raw; minimum 1× bucket so the
    // grid never falls below cell resolution.
    let n = (raw / bucket).ceil().max(1.0);
    n * bucket
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

/// Minimum cell pixel dimensions for in-cell text to render. Below this, the
/// number auto-hides — readability tested at the chart's 10pt text size with
/// 4-digit volumes. Tuned wider/taller than the bare legibility floor so
/// shrinking the chart hides text *before* cells get cramped and unreadable,
/// rather than after. Independent of `TextMetric::None`, which always hides.
const MIN_CELL_W_FOR_TEXT: f32 = 48.0;
const MIN_CELL_H_FOR_TEXT: f32 = 14.0;

/// Paint the classic candlestick layer: per-candle wick + body in sparse
/// mode, column-aggregated bars in dense mode. Extracted verbatim from
/// `paint_main_chart` so the render-kind switch can dispatch to it
/// without duplicating the (well-tuned) sparse/dense split.
#[allow(clippy::too_many_arguments)]
fn paint_candle_bodies(
    origin: Point<Pixels>,
    candles: &[Candle],
    start_idx: usize,
    view_start: f32,
    view_size: f32,
    canvas_w: f32,
    canvas_h: f32,
    chart_w: f32,
    y_lo: f64,
    y_hi: f64,
    y_axis_gap: f32,
    bullish: Hsla,
    bearish: Hsla,
    window: &mut Window,
) {
    let slot_width = chart_w / view_size.max(1.0);
    if slot_width >= 1.0 {
        let body_width = slot_body_width(slot_width);
        let half_body = body_width / 2.0;
        for (i, candle) in candles.iter().enumerate() {
            let idx = (start_idx + i) as f32;
            let center_x = index_to_screen(view_start, view_size, idx, canvas_w, y_axis_gap);
            // Skip candles whose body would paint entirely outside the chart
            // area — keeps GPU paint work proportional to visible candles.
            if center_x + half_body < 0.0 || center_x - half_body > chart_w {
                continue;
            }
            let open_y = price_to_screen(y_lo, y_hi, candle.open, canvas_h);
            let high_y = price_to_screen(y_lo, y_hi, candle.high, canvas_h);
            let low_y = price_to_screen(y_lo, y_hi, candle.low, canvas_h);
            let close_y = price_to_screen(y_lo, y_hi, candle.close, canvas_h);
            let color = if candle.close >= candle.open {
                bullish
            } else {
                bearish
            };
            let wick_top = high_y.min(low_y);
            let wick_h = (high_y - low_y).abs().max(1.0);
            fill_rect(window, origin, center_x - 0.5, 1.0, wick_top, wick_h, color);
            let body_top = open_y.min(close_y);
            let body_h = (open_y - close_y).abs().max(1.0);
            fill_rect(
                window,
                origin,
                center_x - half_body,
                body_width,
                body_top,
                body_h,
                color,
            );
        }
    } else {
        let mut i = 0usize;
        while i < candles.len() {
            let col = index_to_screen(
                view_start,
                view_size,
                (start_idx + i) as f32,
                canvas_w,
                y_axis_gap,
            )
            .floor();
            let mut hi = candles[i].high;
            let mut lo = candles[i].low;
            let open = candles[i].open;
            let mut close = candles[i].close;
            let mut j = i + 1;
            while j < candles.len() {
                let cx = index_to_screen(
                    view_start,
                    view_size,
                    (start_idx + j) as f32,
                    canvas_w,
                    y_axis_gap,
                );
                if cx.floor() != col {
                    break;
                }
                hi = hi.max(candles[j].high);
                lo = lo.min(candles[j].low);
                close = candles[j].close;
                j += 1;
            }
            i = j;
            if col + 1.0 < 0.0 || col > chart_w {
                continue;
            }
            let color = if close >= open { bullish } else { bearish };
            let hi_y = price_to_screen(y_lo, y_hi, hi, canvas_h);
            let lo_y = price_to_screen(y_lo, y_hi, lo, canvas_h);
            let top = hi_y.min(lo_y);
            let h = (hi_y - lo_y).abs().max(1.0);
            fill_rect(window, origin, col, 1.0, top, h, color);
        }
    }
}

/// Layout for one footprint bar slot: where the cells/bars paint vs where
/// the side-OHLC candle (if any) paints. Cluster / Profile cell painters
/// honour `cell_x_min`/`cell_x_max`; the wireframe painter uses
/// `side_candle_center` when SideOhlc is selected.
#[derive(Clone, Copy)]
struct SlotLayout {
    cell_x_min: f32,
    cell_x_max: f32,
    /// Pixel center of the side-OHLC candle; `None` for Behind / None.
    side_candle_center: Option<f32>,
    /// Pixel body-width for the side-OHLC candle when present.
    side_candle_body_w: f32,
}

/// Inset between the Behind body's outer edge and the cell band. The body
/// outline paints at 1 px; the inset adds a small breathing gap on top so
/// cells visibly sit *inside* the frame rather than flush against it.
const BEHIND_CELL_INSET: f32 = 2.0;

/// Build the slot layout for a bar whose center is `center_x` and slot width
/// is `slot_width`. Cells are inset on both sides by the same gap candlestick
/// bodies leave (slot - body) so footprint bars breathe the same as candles —
/// without this, cells stretched edge-to-edge merge into one continuous band
/// at typical zoom levels. SideOhlc tucks the candle into the left edge of
/// the slot, with cells filling the leftover space to the right. Behind
/// nudges the cell band further inward so cells live inside the body frame.
fn footprint_slot_layout(center_x: f32, slot_width: f32, variant: WireframeVariant) -> SlotLayout {
    let half = slot_width * 0.5;
    let slot_left = center_x - half;
    let slot_right = center_x + half;
    // Same per-bar breathing room as candlestick bodies — shared with the
    // candlestick paint via `slot_edge_pad` so both renders line up.
    let edge_pad = slot_edge_pad(slot_width);
    let bar_left = slot_left + edge_pad;
    let bar_right = slot_right - edge_pad;
    match variant {
        WireframeVariant::SideOhlc => {
            let side_w = (slot_width * SIDE_OHLC_FRACTION).max(2.0);
            // Side candle anchored to the LEFT edge of the inset bar area;
            // cells fill from where the candle ends out to `bar_right`. User
            // preference: candle reads left → cells right, matching the
            // direction price/time flow on the chart.
            let cell_x_min = (bar_left + side_w).min(bar_right);
            let side_center = (bar_left + cell_x_min) * 0.5;
            SlotLayout {
                cell_x_min,
                cell_x_max: bar_right,
                side_candle_center: Some(side_center),
                side_candle_body_w: side_w.max(1.0),
            }
        }
        WireframeVariant::Behind => {
            // Sit cells inside the body frame so the wireframe outline reads
            // cleanly around them. Inset capped at half the bar width so
            // very narrow bars don't collapse the cell band to nothing.
            let max_inset = ((bar_right - bar_left) * 0.5 - 0.5).max(0.0);
            let inset = BEHIND_CELL_INSET.min(max_inset);
            SlotLayout {
                cell_x_min: bar_left + inset,
                cell_x_max: bar_right - inset,
                side_candle_center: None,
                side_candle_body_w: 0.0,
            }
        }
        WireframeVariant::None => SlotLayout {
            cell_x_min: bar_left,
            cell_x_max: bar_right,
            side_candle_center: None,
            side_candle_body_w: 0.0,
        },
    }
}

/// Paint the wireframe layer per `WireframeVariant`:
///
/// - `Behind`: thin OHLC outline across the slot, painted before cells so
///   cells render on top. Wick at center, open/close as horizontal ticks.
/// - `SideOhlc`: a narrow real candle painted to the right of the cells.
/// - `None`: no-op.
#[allow(clippy::too_many_arguments)]
fn paint_bar_wireframes(
    origin: Point<Pixels>,
    candles: &[Candle],
    start_idx: usize,
    view_start: f32,
    view_size: f32,
    canvas_w: f32,
    canvas_h: f32,
    chart_w: f32,
    y_lo: f64,
    y_hi: f64,
    y_axis_gap: f32,
    variant: WireframeVariant,
    bullish: Hsla,
    bearish: Hsla,
    window: &mut Window,
) {
    if matches!(variant, WireframeVariant::None) {
        return;
    }
    let slot_width = (chart_w / view_size.max(1.0)).max(1.0);
    for (i, candle) in candles.iter().enumerate() {
        let idx = (start_idx + i) as f32;
        let center_x = index_to_screen(view_start, view_size, idx, canvas_w, y_axis_gap);
        let half = slot_width * 0.5;
        if center_x + half < 0.0 || center_x - half > chart_w {
            continue;
        }
        let open_y = price_to_screen(y_lo, y_hi, candle.open, canvas_h);
        let high_y = price_to_screen(y_lo, y_hi, candle.high, canvas_h);
        let low_y = price_to_screen(y_lo, y_hi, candle.low, canvas_h);
        let close_y = price_to_screen(y_lo, y_hi, candle.close, canvas_h);
        let color = if candle.close >= candle.open {
            bullish
        } else {
            bearish
        };
        let layout = footprint_slot_layout(center_x, slot_width, variant);
        match variant {
            WireframeVariant::Behind => {
                // Full-contrast OHLC frame: top/bottom wick stubs that STOP at
                // the body frame (so no vertical line tracks through the
                // body's interior — that's the user's reading surface for the
                // cells), plus a hollow rectangle around the open→close body
                // (top, bottom, left, right). Painted at full bullish/bearish
                // opacity (no alpha tint) so the bar shape stays the dominant
                // read with cells overlaid inside it.
                let body_top = open_y.min(close_y);
                let body_bottom = open_y.max(close_y);
                let body_h = (body_bottom - body_top).max(1.0);
                let body_w = slot_body_width(slot_width).max(2.0);
                let body_left = center_x - body_w * 0.5;
                let wick_top = high_y.min(low_y);
                let wick_bottom = high_y.max(low_y);
                // Upper wick: high → top of body. Skip if the body is at the
                // top of the candle's range (no upper wick).
                if body_top > wick_top {
                    fill_rect(
                        window,
                        origin,
                        center_x - 0.5,
                        1.0,
                        wick_top,
                        body_top - wick_top,
                        color,
                    );
                }
                // Lower wick: bottom of body → low. Skip if body is at the
                // bottom of the candle's range.
                if wick_bottom > body_bottom {
                    fill_rect(
                        window,
                        origin,
                        center_x - 0.5,
                        1.0,
                        body_bottom,
                        wick_bottom - body_bottom,
                        color,
                    );
                }
                // Left + right edges
                fill_rect(window, origin, body_left, 1.0, body_top, body_h, color);
                fill_rect(
                    window,
                    origin,
                    body_left + body_w - 1.0,
                    1.0,
                    body_top,
                    body_h,
                    color,
                );
                // Top + bottom edges — close the rectangle so the body
                // reads as a fully enclosed frame, not three loose verticals.
                fill_rect(window, origin, body_left, body_w, body_top, 1.0, color);
                fill_rect(
                    window,
                    origin,
                    body_left,
                    body_w,
                    body_top + body_h - 1.0,
                    1.0,
                    color,
                );
            }
            WireframeVariant::SideOhlc => {
                // A real (full-alpha) narrow candle on the slot's right side.
                let Some(side_x) = layout.side_candle_center else {
                    continue;
                };
                let body_w = layout.side_candle_body_w;
                let wick_top = high_y.min(low_y);
                let wick_h = (high_y - low_y).abs().max(1.0);
                fill_rect(window, origin, side_x - 0.5, 1.0, wick_top, wick_h, color);
                let body_top = open_y.min(close_y);
                let body_h = (open_y - close_y).abs().max(1.0);
                fill_rect(
                    window,
                    origin,
                    side_x - body_w * 0.5,
                    body_w,
                    body_top,
                    body_h,
                    color,
                );
            }
            WireframeVariant::None => {}
        }
    }
}

/// Paint per-bar cluster cells (one cell per (bar, price_bucket)).
///
/// Bid/Ask render splits the cell horizontally into two sub-cells (bid left,
/// ask right) coloured by `bearish` / `bullish` with intensity from the
/// side's local volume; Volume / Delta render one cell per bucket coloured
/// by the metric. Text label is per `params.text_metric`, auto-hidden when
/// the cell box is too small to read.
///
/// No-op when `cells` is empty — the dispatcher in `paint_main_chart`
/// already falls back to candle bodies in that case; this guard is a
/// belt-and-braces safety so the function can be called speculatively.
#[allow(clippy::too_many_arguments)]
fn paint_cluster_cells(
    origin: Point<Pixels>,
    candles: &[Candle],
    cells: &[FootprintCell],
    start_idx: usize,
    view_start: f32,
    view_size: f32,
    canvas_w: f32,
    canvas_h: f32,
    chart_w: f32,
    y_lo: f64,
    y_hi: f64,
    y_axis_gap: f32,
    params: &FootprintParams,
    bullish: Hsla,
    bearish: Hsla,
    text_color: Hsla,
    volume_unit: VolumeUnit,
    window: &mut Window,
    cx: &mut App,
) {
    if cells.is_empty() || candles.is_empty() {
        return;
    }
    let bucket = params.bucket;
    if !FootprintParams::bucket_is_valid(bucket) {
        return;
    }
    let slot_width = (chart_w / view_size.max(1.0)).max(1.0);
    // Cell pixel height = bucket in price space mapped to screen.
    let cell_h = {
        let p0 = price_to_screen(y_lo, y_hi, 0.0, canvas_h);
        let p1 = price_to_screen(y_lo, y_hi, bucket, canvas_h);
        (p0 - p1).abs().max(1.0)
    };

    // Per-bar normalisation factor depends on the colour scope. Individual
    // recomputes per bar inside the loop; Visible / Daily would precompute
    // a viewport / day max (Daily falls back to Visible until a day cache
    // exists — see Phase 3 notes in the design memo).
    let scope = params.color_scope;
    let metric = params.render_metric;
    let global_max = if matches!(scope, ColorScope::Visible | ColorScope::Daily) {
        compute_global_metric_max(cells, metric, bucket, volume_unit)
    } else {
        0.0
    };

    // Build a per-bar cells index to avoid an O(bars × cells) scan. Cells
    // share an `open_time` per bar; bucket by that key.
    let mut by_bar: std::collections::HashMap<i64, Vec<&FootprintCell>> =
        std::collections::HashMap::new();
    for c in cells {
        by_bar.entry(c.open_time).or_default().push(c);
    }

    let show_text_base = !matches!(params.text_metric, TextMetric::None);

    for (i, candle) in candles.iter().enumerate() {
        let Some(bar_cells) = by_bar.get(&candle.open_time) else {
            continue;
        };
        let idx = (start_idx + i) as f32;
        let center_x = index_to_screen(view_start, view_size, idx, canvas_w, y_axis_gap);
        let layout = footprint_slot_layout(center_x, slot_width, params.wireframe);
        let cell_left = layout.cell_x_min;
        let cell_right = layout.cell_x_max;
        let cell_w = (cell_right - cell_left).max(1.0);
        if cell_right < 0.0 || cell_left > chart_w {
            continue;
        }

        let local_max = if matches!(scope, ColorScope::Individual) {
            bar_cells
                .iter()
                .map(|c| metric_value(c, metric, bucket, volume_unit).abs())
                .fold(0.0_f64, f64::max)
        } else {
            global_max
        };
        if local_max <= 0.0 {
            continue;
        }
        let show_text =
            show_text_base && cell_w >= MIN_CELL_W_FOR_TEXT && cell_h >= MIN_CELL_H_FOR_TEXT;

        for c in bar_cells {
            let top_price = c.price_bucket_low + bucket;
            let y_top = price_to_screen(y_lo, y_hi, top_price, canvas_h);
            if y_top + cell_h < 0.0 || y_top > canvas_h {
                continue;
            }
            // Sided volumes in the selected unit. Cluster paints sides
            // individually for BidAsk, so we convert per-side rather than
            // collapsing to a single metric scalar.
            let (bid_v, ask_v) = sided_volumes(c, bucket, volume_unit);
            match metric {
                RenderMetric::BidAsk => {
                    let side_w = cell_w * 0.5;
                    let side_max = bid_v.max(ask_v).max(1e-9);
                    let bid_intensity = (bid_v / side_max) as f32;
                    let ask_intensity = (ask_v / side_max) as f32;
                    let bid_color = Hsla {
                        a: 0.25 + 0.55 * bid_intensity,
                        ..bearish
                    };
                    let ask_color = Hsla {
                        a: 0.25 + 0.55 * ask_intensity,
                        ..bullish
                    };
                    fill_rect(window, origin, cell_left, side_w, y_top, cell_h, bid_color);
                    fill_rect(
                        window,
                        origin,
                        cell_left + side_w,
                        side_w,
                        y_top,
                        cell_h,
                        ask_color,
                    );
                }
                RenderMetric::Volume => {
                    let v = bid_v + ask_v;
                    if v <= 0.0 {
                        continue;
                    }
                    let intensity = ((v / local_max) as f32).min(1.0);
                    let color = Hsla {
                        a: 0.20 + 0.65 * intensity,
                        ..text_color
                    };
                    fill_rect(window, origin, cell_left, cell_w, y_top, cell_h, color);
                }
                RenderMetric::Delta => {
                    let d = ask_v - bid_v;
                    let intensity = ((d.abs() / local_max) as f32).min(1.0);
                    let base = if d >= 0.0 { bullish } else { bearish };
                    let color = Hsla {
                        a: 0.20 + 0.65 * intensity,
                        ..base
                    };
                    fill_rect(window, origin, cell_left, cell_w, y_top, cell_h, color);
                }
            }

            if show_text {
                // Per-half centring for BidAsk text: bid number sits in the
                // left half (over the bid fill), ask in the right half. For
                // every other text metric, fall back to single-string
                // centring across the whole cell.
                if matches!(params.text_metric, TextMetric::BidAsk) {
                    let half_w = cell_w * 0.5;
                    paint_centred_text(
                        window,
                        cx,
                        origin,
                        cell_left,
                        half_w,
                        y_top,
                        cell_h,
                        text_color,
                        &format_short(bid_v),
                    );
                    paint_centred_text(
                        window,
                        cx,
                        origin,
                        cell_left + half_w,
                        half_w,
                        y_top,
                        cell_h,
                        text_color,
                        &format_short(ask_v),
                    );
                } else {
                    let text = format_cell_text(c, params.text_metric, bucket, volume_unit);
                    if !text.is_empty() {
                        paint_centred_text(
                            window, cx, origin, cell_left, cell_w, y_top, cell_h, text_color, &text,
                        );
                    }
                }
            }
        }
    }
}

/// Convert a cell's raw bid/ask coin volumes into the requested display unit.
/// USD uses the bucket midpoint as the conversion price — cheap to compute
/// and matches what the cell's labels display. Returns `(bid, ask)`.
fn sided_volumes(c: &FootprintCell, bucket: f64, unit: VolumeUnit) -> (f64, f64) {
    match unit {
        VolumeUnit::Coin => (c.bid_vol, c.ask_vol),
        VolumeUnit::Usd => {
            let mid = c.price_bucket_low + bucket * 0.5;
            (c.bid_vol * mid, c.ask_vol * mid)
        }
    }
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

/// Paint per-bar volume profile bars (one horizontal bar per price bucket).
///
/// Bar length encodes `params.render_metric` (Volume: total; Delta: signed
/// magnitude; BidAsk: stacked left/right of bar center). Optional text label
/// is per `params.text_metric`, auto-hidden when the bar is too small.
///
/// No-op when `cells` is empty.
#[allow(clippy::too_many_arguments)]
fn paint_profile_bars(
    origin: Point<Pixels>,
    candles: &[Candle],
    cells: &[FootprintCell],
    start_idx: usize,
    view_start: f32,
    view_size: f32,
    canvas_w: f32,
    canvas_h: f32,
    chart_w: f32,
    y_lo: f64,
    y_hi: f64,
    y_axis_gap: f32,
    params: &FootprintParams,
    bullish: Hsla,
    bearish: Hsla,
    text_color: Hsla,
    volume_unit: VolumeUnit,
    window: &mut Window,
    cx: &mut App,
) {
    if cells.is_empty() || candles.is_empty() {
        return;
    }
    let bucket = params.bucket;
    if !FootprintParams::bucket_is_valid(bucket) {
        return;
    }
    let slot_width = (chart_w / view_size.max(1.0)).max(1.0);
    let cell_h = {
        let p0 = price_to_screen(y_lo, y_hi, 0.0, canvas_h);
        let p1 = price_to_screen(y_lo, y_hi, bucket, canvas_h);
        (p0 - p1).abs().max(1.0)
    };

    let scope = params.color_scope;
    let metric = params.render_metric;
    let global_max = if matches!(scope, ColorScope::Visible | ColorScope::Daily) {
        compute_global_metric_max(cells, metric, bucket, volume_unit)
    } else {
        0.0
    };

    let mut by_bar: std::collections::HashMap<i64, Vec<&FootprintCell>> =
        std::collections::HashMap::new();
    for c in cells {
        by_bar.entry(c.open_time).or_default().push(c);
    }

    let show_text_base = !matches!(params.text_metric, TextMetric::None);

    for (i, candle) in candles.iter().enumerate() {
        let Some(bar_cells) = by_bar.get(&candle.open_time) else {
            continue;
        };
        let idx = (start_idx + i) as f32;
        let center_x = index_to_screen(view_start, view_size, idx, canvas_w, y_axis_gap);
        let layout = footprint_slot_layout(center_x, slot_width, params.wireframe);
        let bar_x_min = layout.cell_x_min;
        let bar_x_max = layout.cell_x_max;
        let bar_w_total = (bar_x_max - bar_x_min).max(1.0);
        if bar_x_max < 0.0 || bar_x_min > chart_w {
            continue;
        }

        let local_max = if matches!(scope, ColorScope::Individual) {
            bar_cells
                .iter()
                .map(|c| metric_value(c, metric, bucket, volume_unit).abs())
                .fold(0.0_f64, f64::max)
        } else {
            global_max
        };
        if local_max <= 0.0 {
            continue;
        }
        let show_text =
            show_text_base && bar_w_total >= MIN_CELL_W_FOR_TEXT && cell_h >= MIN_CELL_H_FOR_TEXT;

        for c in bar_cells {
            let top_price = c.price_bucket_low + bucket;
            let y_top = price_to_screen(y_lo, y_hi, top_price, canvas_h);
            if y_top + cell_h < 0.0 || y_top > canvas_h {
                continue;
            }
            let (bid_v, ask_v) = sided_volumes(c, bucket, volume_unit);
            match metric {
                RenderMetric::Volume => {
                    let v = bid_v + ask_v;
                    if v <= 0.0 {
                        continue;
                    }
                    let frac = ((v / local_max) as f32).min(1.0);
                    let bar_w = (bar_w_total * frac).max(1.0);
                    let color = Hsla {
                        a: 0.65,
                        ..text_color
                    };
                    fill_rect(window, origin, bar_x_min, bar_w, y_top, cell_h, color);
                }
                RenderMetric::Delta => {
                    let d = ask_v - bid_v;
                    if d.abs() <= 0.0 {
                        continue;
                    }
                    let frac = ((d.abs() / local_max) as f32).min(1.0);
                    // Anchor every delta bar at the slot's left edge — both
                    // signs grow rightwards. Color encodes sign (bull/bear),
                    // length encodes magnitude. Per user feedback: reads
                    // cleaner as a one-sided horizontal histogram than a
                    // diverging-from-midpoint layout.
                    let bar_w = (bar_w_total * frac).max(1.0);
                    let base = if d >= 0.0 { bullish } else { bearish };
                    let color = Hsla { a: 0.75, ..base };
                    fill_rect(window, origin, bar_x_min, bar_w, y_top, cell_h, color);
                }
                RenderMetric::BidAsk => {
                    // Stacked bid/ask: bid extends left of midpoint, ask right.
                    let mid = (bar_x_min + bar_x_max) * 0.5;
                    let half_w = bar_w_total * 0.5;
                    let bid_frac = ((bid_v / local_max) as f32).min(1.0);
                    let ask_frac = ((ask_v / local_max) as f32).min(1.0);
                    let bid_w = (half_w * bid_frac).max(0.0);
                    let ask_w = (half_w * ask_frac).max(0.0);
                    if bid_w > 0.0 {
                        let color = Hsla { a: 0.7, ..bearish };
                        fill_rect(window, origin, mid - bid_w, bid_w, y_top, cell_h, color);
                    }
                    if ask_w > 0.0 {
                        let color = Hsla { a: 0.7, ..bullish };
                        fill_rect(window, origin, mid, ask_w, y_top, cell_h, color);
                    }
                }
            }

            if show_text {
                if matches!(params.text_metric, TextMetric::BidAsk) {
                    let half_w = bar_w_total * 0.5;
                    paint_centred_text(
                        window,
                        cx,
                        origin,
                        bar_x_min,
                        half_w,
                        y_top,
                        cell_h,
                        text_color,
                        &format_short(bid_v),
                    );
                    paint_centred_text(
                        window,
                        cx,
                        origin,
                        bar_x_min + half_w,
                        half_w,
                        y_top,
                        cell_h,
                        text_color,
                        &format_short(ask_v),
                    );
                } else {
                    let text = format_cell_text(c, params.text_metric, bucket, volume_unit);
                    if !text.is_empty() {
                        paint_centred_text(
                            window,
                            cx,
                            origin,
                            bar_x_min,
                            bar_w_total,
                            y_top,
                            cell_h,
                            text_color,
                            &text,
                        );
                    }
                }
            }
        }
    }
}

/// Extract the metric scalar for a cell in the display unit. `Volume` =
/// bid+ask; `Delta` = signed ask-bid; `BidAsk` returns `max(bid, ask)` so
/// that normalisation against this value gives each side its own 0..1
/// intensity range when dividing per-side downstream.
fn metric_value(c: &FootprintCell, metric: RenderMetric, bucket: f64, unit: VolumeUnit) -> f64 {
    let (bid, ask) = sided_volumes(c, bucket, unit);
    match metric {
        RenderMetric::Volume => bid + ask,
        RenderMetric::Delta => ask - bid,
        RenderMetric::BidAsk => bid.max(ask),
    }
}

/// Max absolute metric value across all cells — used by `Visible` / `Daily`
/// colour scopes. `Daily` currently falls back to Visible (no day cache
/// yet — see design memo Phase 3 note).
fn compute_global_metric_max(
    cells: &[FootprintCell],
    metric: RenderMetric,
    bucket: f64,
    unit: VolumeUnit,
) -> f64 {
    cells
        .iter()
        .map(|c| metric_value(c, metric, bucket, unit).abs())
        .fold(0.0_f64, f64::max)
}

/// Format the in-cell text for a footprint cell. None → empty (auto-hide
/// also bypasses this path). Volumes use 0-decimal short form when ≥ 1000.
fn format_cell_text(
    c: &FootprintCell,
    metric: TextMetric,
    bucket: f64,
    unit: VolumeUnit,
) -> String {
    let (bid, ask) = sided_volumes(c, bucket, unit);
    match metric {
        TextMetric::None => String::new(),
        TextMetric::Volume => format_short(bid + ask),
        TextMetric::Delta => format_short(ask - bid),
        // Single-string fallback when this helper is reached from a path
        // that DOESN'T split bid/ask per-half (currently unused — the
        // cluster + profile painters short-circuit BidAsk into per-half
        // paint_centred_text calls above).
        TextMetric::BidAsk => format!("{}×{}", format_short(bid), format_short(ask)),
    }
}

/// In-cell label for footprint volumes. When the user toggles "Truncate
/// footprint decimals" on, fractional digits are dropped (cells render as
/// whole numbers, rounded to nearest; K/M suffix preserved). Otherwise the
/// standard `K`/`M` shorthand at 1dp with a 2dp tail for sub-10 values.
fn format_short(v: f64) -> String {
    let abs = v.abs();
    if crate::prefs::footprint_truncate_decimals() {
        if abs >= 1_000_000.0 {
            return format!("{:.0}M", (v / 1_000_000.0).round());
        } else if abs >= 1_000.0 {
            return format!("{:.0}K", (v / 1_000.0).round());
        }
        return format!("{:.0}", v.round());
    }
    if abs >= 1_000_000.0 {
        format!("{:.1}M", v / 1_000_000.0)
    } else if abs >= 1_000.0 {
        format!("{:.1}K", v / 1_000.0)
    } else if abs >= 10.0 {
        format!("{:.0}", v)
    } else {
        format!("{:.2}", v)
    }
}

/// Custom overlay that paints all committed drawings + the in-progress
/// preview + selection chrome on top of the candlestick canvas. Positioned
/// absolutely inside the chart-canvas div with the y-axis-label gutter and
/// x-axis-label band excluded so drawings don't paint over the axis area.
pub(super) fn render_drawings_overlay(
    drawings: Vec<Drawing>,
    styles: std::collections::HashMap<DrawingId, super::drawings_view::DrawingStyle>,
    creating_preview: Option<Drawing>,
    selected: Option<DrawingId>,
    view_start: f32,
    view_size: f32,
    y_lo: f64,
    y_hi: f64,
    y_axis_gap: f32,
    cursor: Option<(f32, f32)>,
    cross_x: Option<f32>,
    colors: DrawingColors,
    // Candle buffer reused by `Drawing::AnchoredVwap` paint to walk forward
    // from the anchor and accumulate Σ(vw·v)/Σ(v) per bar. Cloned because the
    // canvas closure is `'static` and needs ownership.
    candles: Vec<Candle>,
) -> impl IntoElement {
    div()
        .absolute()
        .left_0()
        .top_0()
        .bottom(px(AXIS_GAP))
        .right(px(y_axis_gap))
        .child(
            canvas(
                |_, _, _| (),
                move |bounds, _, window, cx| {
                    // Clip every paint call (including shape_line text)
                    // to the overlay's bounds. Without this, a drawing
                    // label rendered via shape_line would bleed past the
                    // chart's right edge and paint over the y-axis
                    // labels, since text paint isn't bounded by the
                    // canvas's div.
                    window.with_content_mask(Some(ContentMask { bounds }), |window| {
                        paint_drawings_overlay(
                            bounds,
                            &drawings,
                            &styles,
                            creating_preview.as_ref(),
                            selected,
                            view_start,
                            view_size,
                            y_lo,
                            y_hi,
                            y_axis_gap,
                            cursor,
                            cross_x,
                            colors,
                            &candles,
                            window,
                            cx,
                        );
                    });
                },
            )
            .size_full(),
        )
}

fn paint_drawings_overlay(
    bounds: Bounds<Pixels>,
    drawings: &[Drawing],
    styles: &std::collections::HashMap<DrawingId, super::drawings_view::DrawingStyle>,
    creating_preview: Option<&Drawing>,
    selected: Option<DrawingId>,
    view_start: f32,
    view_size: f32,
    y_lo: f64,
    y_hi: f64,
    y_axis_gap: f32,
    cursor: Option<(f32, f32)>,
    cross_x: Option<f32>,
    colors: DrawingColors,
    candles: &[Candle],
    window: &mut Window,
    cx: &mut App,
) {
    let origin = bounds.origin;
    let w = bounds.size.width.as_f32();
    let h = bounds.size.height.as_f32();
    // The chart paints prices into `[10, canvas_h]` where canvas_h excludes
    // the bottom axis. Our overlay's height already excludes that band (we
    // bottom-clip via the wrapping div), so the y range maps to [10, h].
    let chart_h_for_y = h + AXIS_GAP; // re-add to use shared price_to_screen formula
    let chart_w_for_x = w + y_axis_gap;

    let to_screen = |anchor: (f32, f64)| -> (f32, f32) {
        let x = index_to_screen(view_start, view_size, anchor.0, chart_w_for_x, y_axis_gap);
        let y = price_to_screen(y_lo, y_hi, anchor.1, chart_h_for_y);
        (x, y)
    };

    let paint_line =
        |window: &mut Window, ax: f32, ay: f32, bx: f32, by: f32, color: Hsla, width: f32| {
            // `width <= 0.0` is the "no per-drawing override" sentinel
            // (in-flight create previews, crosshair guides). Falls back to
            // the global 2 px default so previews match the committed look.
            let w = if width > 0.0 { width } else { 2.0 };
            let mut pb = PathBuilder::stroke(px(w));
            pb.move_to(point(px(ax) + origin.x, px(ay) + origin.y));
            pb.line_to(point(px(bx) + origin.x, px(by) + origin.y));
            if let Ok(path) = pb.build() {
                window.paint_path(path, color);
            }
        };

    let paint_handle = |window: &mut Window, hx: f32, hy: f32| {
        let half = 4.0_f32;
        let b = Bounds {
            origin: point(px(hx - half) + origin.x, px(hy - half) + origin.y),
            size: gpui::size(px(half * 2.0), px(half * 2.0)),
        };
        window.paint_quad(PaintQuad {
            bounds: b,
            corner_radii: Corners::default(),
            background: colors.background.into(),
            border_widths: Edges {
                top: px(1.0),
                right: px(1.0),
                bottom: px(1.0),
                left: px(1.0),
            },
            border_color: colors.ring,
            border_style: BorderStyle::default(),
        });
    };

    let paint_filled_zone =
        |window: &mut Window, xmin: f32, xmax: f32, y_lo: f32, y_hi: f32, fill: Hsla| {
            let b = Bounds {
                origin: point(px(xmin) + origin.x, px(y_lo) + origin.y),
                size: gpui::size(px((xmax - xmin).max(0.0)), px((y_hi - y_lo).max(0.0))),
            };
            window.paint_quad(PaintQuad {
                bounds: b,
                corner_radii: Corners::default(),
                background: fill.into(),
                border_widths: Edges::default(),
                border_color: gpui::transparent_black(),
                border_style: BorderStyle::default(),
            });
        };

    let draw_position = |window: &mut Window,
                         t0: f32,
                         t1: f32,
                         entry: f64,
                         tp: f64,
                         sl: f64,
                         is_selected: bool,
                         _is_preview: bool,
                         profit_override: Option<Hsla>,
                         loss_override: Option<Hsla>,
                         width: f32| {
        let (x0, _) = to_screen((t0, entry));
        let (x1, _) = to_screen((t1, entry));
        let (xmin, xmax) = (x0.min(x1), x0.max(x1));
        let (_, y_entry) = to_screen((t0, entry));
        let (_, y_tp) = to_screen((t0, tp));
        let (_, y_sl) = to_screen((t0, sl));
        let profit_base = profit_override.unwrap_or(colors.bullish);
        let loss_base = loss_override.unwrap_or(colors.bearish);
        // TP zone always uses the profit tint, SL the loss tint. Direction
        // (long vs short) only decides which side of entry each sits on —
        // that's already encoded in the y values handed in.
        paint_filled_zone(
            window,
            xmin,
            xmax,
            y_entry.min(y_tp),
            y_entry.max(y_tp),
            Hsla {
                a: 0.18,
                ..profit_base
            },
        );
        paint_filled_zone(
            window,
            xmin,
            xmax,
            y_entry.min(y_sl),
            y_entry.max(y_sl),
            Hsla {
                a: 0.18,
                ..loss_base
            },
        );
        // Three horizontal lines. Entry stays muted (structural marker, not
        // user-recolourable); TP/SL track the zone tints with the user
        // override applied above. Selection no longer flips these to the ring
        // colour — endpoint handles mark selection on their own.
        paint_line(window, xmin, y_entry, xmax, y_entry, colors.muted, width);
        paint_line(window, xmin, y_tp, xmax, y_tp, profit_base, width);
        paint_line(window, xmin, y_sl, xmax, y_sl, loss_base, width);

        if is_selected {
            // Price handles: dots at the horizontal middle of each price
            // line — keeps them inside the rect so they don't compete with
            // the time-edge handles at xmin / xmax, and reads as
            // "vertical-drag this line".
            let x_mid = (xmin + xmax) / 2.0;
            paint_handle(window, x_mid, y_tp);
            paint_handle(window, x_mid, y_entry);
            paint_handle(window, x_mid, y_sl);
            // Time-edge handles: pinned to the entry (breakeven) line so
            // the user has a clear "drag this edge horizontally" cue. The
            // hit-test still accepts any y within the rect extent.
            paint_handle(window, xmin, y_entry);
            paint_handle(window, xmax, y_entry);
        }
    };

    let draw_one =
        |window: &mut Window, cx: &mut App, d: &Drawing, is_selected: bool, is_preview: bool| {
            // Preview drawings carry no style record (they live on the chart's
            // `creating` state, not the service). Lookup returns the default
            // style → paint falls back to the theme line color + 2 px default.
            let style = styles.get(&d.id()).cloned().unwrap_or_default();
            let custom_stroke = style.color;
            // Stroke colour: previews and committed drawings both use the
            // shape's effective colour (theme default unless overridden).
            // Endpoint handles are the selection cue; previews are visible by
            // virtue of being drawn under the cursor — no blue tint needed.
            let stroke = custom_stroke.unwrap_or(colors.line);
            // Width: previews carry no style record, so width=0 → paint's 2 px
            // fallback. Committed drawings use their stored width.
            let line_w = style.width;
            match d {
                Drawing::Line { a, b, .. } => {
                    let (ax, ay) = to_screen(*a);
                    let (bx, by) = to_screen(*b);
                    paint_line(window, ax, ay, bx, by, stroke, line_w);
                    if is_selected {
                        paint_handle(window, ax, ay);
                        paint_handle(window, bx, by);
                    }
                }
                Drawing::Arrow { a, b, .. } => {
                    // Arrow = line plus a short "V" at b pointing back along the
                    // segment direction. Skip the head when the segment is too
                    // short to look like anything sensible (collapsed click).
                    let (ax, ay) = to_screen(*a);
                    let (bx, by) = to_screen(*b);
                    paint_line(window, ax, ay, bx, by, stroke, line_w);
                    let dx = bx - ax;
                    let dy = by - ay;
                    let len = (dx * dx + dy * dy).sqrt();
                    if len >= 4.0 {
                        let ux = dx / len;
                        let uy = dy / len;
                        // Perpendicular unit vector.
                        let px_ = -uy;
                        let py_ = ux;
                        // Arrowhead base point (`head_back` px back along the
                        // line) and the two wing tips offset by `wing` px.
                        let head_back = 10.0_f32;
                        let wing = 5.0_f32;
                        let basex = bx - ux * head_back;
                        let basey = by - uy * head_back;
                        let w1x = basex + px_ * wing;
                        let w1y = basey + py_ * wing;
                        let w2x = basex - px_ * wing;
                        let w2y = basey - py_ * wing;
                        paint_line(window, bx, by, w1x, w1y, stroke, line_w);
                        paint_line(window, bx, by, w2x, w2y, stroke, line_w);
                    }
                    if is_selected {
                        paint_handle(window, ax, ay);
                        paint_handle(window, bx, by);
                    }
                }
                Drawing::Fibonacci { a, b, .. } => {
                    // Horizontal lines at standard fib levels between a.price
                    // and b.price, spanning the same x-extent as the bounding
                    // rect. The top + bottom horizontals at level 0.0 / 1.0
                    // serve as the visual frame; no vertical sides — the user
                    // asked for a cleaner readout that reads as a ladder of
                    // levels rather than a closed box. Each level gets a small
                    // ratio label flush against the right edge.
                    const LEVELS: &[f32] = &[0.0, 0.236, 0.382, 0.5, 0.618, 0.786, 1.0];
                    let (ax, _ay) = to_screen(*a);
                    let (bx, _by) = to_screen(*b);
                    let (xmin, xmax) = (ax.min(bx), ax.max(bx));
                    let level_color = custom_stroke.unwrap_or(colors.line);
                    let fade_color = Hsla {
                        a: 0.6,
                        ..level_color
                    };
                    // Reversed convention: level 1 lands at `a` (where the user
                    // started the drag), level 0 at `b` (where they released).
                    // Read top-to-bottom on an uptrend drag (b above a), the
                    // labels go 0, 0.236, …, 1; on a downtrend drag they go
                    // 1, 0.786, …, 0 — i.e. "1" always marks the move's start.
                    let price_a = a.1;
                    let price_b = b.1;
                    for &level in LEVELS {
                        let price = price_b + (price_a - price_b) * level as f64;
                        let (_, y) = to_screen((a.0, price));
                        paint_line(window, xmin, y, xmax, y, fade_color, line_w);
                        // Ratio label just outside the right edge of the level
                        // line. Format mirrors common charting platforms:
                        // integer percentages on whole ratios, three-decimal
                        // on the fractional ones.
                        let label_text = format_fib_level(level);
                        let label: SharedString = SharedString::from(label_text);
                        let run = TextRun {
                            len: label.len(),
                            font: window.text_style().font(),
                            color: level_color,
                            background_color: None,
                            underline: None,
                            strikethrough: None,
                        };
                        let line = window
                            .text_system()
                            .shape_line(label, px(10.0), &[run], None);
                        let _ = line.paint(
                            point(px(xmax + 4.0) + origin.x, px(y - 5.0) + origin.y),
                            px(10.0),
                            gpui::TextAlign::Left,
                            None,
                            window,
                            cx,
                        );
                    }
                    if is_selected {
                        let ay_screen = to_screen(*a).1;
                        let by_screen = to_screen(*b).1;
                        paint_handle(window, ax, ay_screen);
                        paint_handle(window, bx, by_screen);
                    }
                }
                Drawing::HorizontalRay {
                    anchor,
                    text,
                    extend_left,
                    ..
                } => {
                    // Horizontal ray: line from the anchor x to the right edge
                    // of the overlay at the anchor's y. Anchors past the right
                    // edge collapse to a dot at the edge so the user can still
                    // find them. When `extend_left` is set, the stroke also
                    // reaches back to the overlay's left edge — turning the
                    // ray into a full horizontal line.
                    let (ax, ay) = to_screen(*anchor);
                    let right_edge = bounds.size.width.as_f32();
                    let overlay_h = bounds.size.height.as_f32();
                    let start_x = if *extend_left {
                        0.0
                    } else {
                        ax.max(0.0).min(right_edge)
                    };
                    // Skip painting entirely when the ray's y sits outside the
                    // overlay — text isn't clipped by the overlay's bounds, so
                    // without this guard the label would float in the axis
                    // gutter even though the line itself is off-screen.
                    let line_visible = ay >= 0.0 && ay <= overlay_h;
                    if line_visible {
                        paint_line(window, start_x, ay, right_edge, ay, stroke, line_w);
                        if is_selected {
                            paint_handle(window, ax, ay);
                        }
                        if let Some(text) = text.as_ref() {
                            if !text.is_empty() {
                                let label = SharedString::from(text.clone());
                                let run = TextRun {
                                    len: label.len(),
                                    font: window.text_style().font(),
                                    color: stroke,
                                    background_color: None,
                                    underline: None,
                                    strikethrough: None,
                                };
                                let line =
                                    window
                                        .text_system()
                                        .shape_line(label, px(11.0), &[run], None);
                                let text_w = line.width().as_f32();
                                let pad = 4.0_f32;
                                // Anchor the label at the right edge of the
                                // overlay, but let it track the ray's start
                                // once the ray scrolls right enough to reach
                                // it — that way the label disappears with the
                                // ray instead of lingering at the right edge
                                // after the ray is off-canvas. The y-axis
                                // chrome paints over this overlay and masks
                                // any bleed past `right_edge`.
                                let label_x = (right_edge - text_w - pad).max(start_x);
                                let label_y = ay - 14.0;
                                let _ = line.paint(
                                    point(px(label_x) + origin.x, px(label_y) + origin.y),
                                    px(11.0),
                                    gpui::TextAlign::Left,
                                    None,
                                    window,
                                    cx,
                                );
                            }
                        }
                    }
                }
                Drawing::AnchoredVwap { anchor, .. } => {
                    // Walk forward from the anchor bar, accumulating Σ(vw·v)/Σ(v)
                    // per visible bar, and paint as a polyline. The anchor's
                    // fractional index is snapped to the nearest integer bar so
                    // the polyline starts cleanly at a bar center.
                    let start_idx = (anchor.0.round() as i64).max(0) as usize;
                    if !candles.is_empty() && start_idx < candles.len() {
                        let mut num = 0.0_f64;
                        let mut den = 0.0_f64;
                        let mut prev: Option<(f32, f32)> = None;
                        let mut first_visible: Option<(f32, f32)> = None;
                        for (offset, c) in candles[start_idx..].iter().enumerate() {
                            if let Some(vw) = c.vwap {
                                if vw > 0.0 && c.volume > 0.0 {
                                    num += vw * c.volume;
                                    den += c.volume;
                                }
                            }
                            if den > 0.0 {
                                let idx = (start_idx + offset) as f32;
                                let sx = index_to_screen(
                                    view_start,
                                    view_size,
                                    idx,
                                    chart_w_for_x,
                                    y_axis_gap,
                                );
                                let sy = price_to_screen(y_lo, y_hi, num / den, chart_h_for_y);
                                if let Some((px_, py_)) = prev {
                                    paint_line(window, px_, py_, sx, sy, stroke, line_w);
                                }
                                if first_visible.is_none() {
                                    first_visible = Some((sx, sy));
                                }
                                prev = Some((sx, sy));
                            }
                        }
                        if is_selected {
                            if let Some((sx, sy)) = first_visible {
                                paint_handle(window, sx, sy);
                            }
                        }
                    }
                }
                Drawing::Rect { a, b, .. } => {
                    let (ax, ay) = to_screen(*a);
                    let (bx, by) = to_screen(*b);
                    let (xmin, xmax) = (ax.min(bx), ax.max(bx));
                    let (ymin, ymax) = (ay.min(by), ay.max(by));
                    let rb = Bounds {
                        origin: point(px(xmin) + origin.x, px(ymin) + origin.y),
                        size: gpui::size(px(xmax - xmin), px(ymax - ymin)),
                    };
                    // Border colour/width: user override (style.color, style.width)
                    // both flow through `stroke` + `line_w` above. Falls back to
                    // the theme rect-border + 2 px default when the user hasn't
                    // customised the shape.
                    let border_color = custom_stroke.unwrap_or(colors.rect_border);
                    let border_w = if line_w > 0.0 { line_w } else { 2.0 };
                    window.paint_quad(PaintQuad {
                        bounds: rb,
                        corner_radii: Corners::default(),
                        background: colors.rect_fill.into(),
                        border_widths: Edges {
                            top: px(border_w),
                            right: px(border_w),
                            bottom: px(border_w),
                            left: px(border_w),
                        },
                        border_color,
                        border_style: BorderStyle::default(),
                    });
                    if is_selected {
                        paint_handle(window, ax, ay);
                        paint_handle(window, bx, by);
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
                    draw_position(
                        window,
                        *t0,
                        *t1,
                        *entry,
                        *take_profit,
                        *stop_loss,
                        is_selected,
                        is_preview,
                        style.profit_color,
                        style.loss_color,
                        line_w,
                    );
                }
                Drawing::Frvp {
                    t0,
                    t1,
                    p0,
                    p1,
                    params,
                    output,
                    ..
                } => {
                    let (x0, _) = to_screen((*t0, 0.0));
                    let (x1, _) = to_screen((*t1, 0.0));
                    let (xmin, xmax) = (x0.min(x1), x0.max(x1));
                    let frvp_w = (xmax - xmin).max(1.0);
                    // Visual guide. Modern shapes (price anchors present)
                    // get a thin dashed diagonal directly between the two
                    // anchor points — the rectangle outline that used to
                    // frame them was too heavy a chrome for what's really a
                    // bounding hint. Legacy shapes still get two vertical
                    // hairlines edge-to-edge. The profile bars further below
                    // always span the full pane in either case — price
                    // anchors are decorative per the design call.
                    // Fixed light-grey for the dashed guide regardless of
                    // selection state — the selection ring colour competed
                    // with the profile bars; a neutral grey reads as "guide
                    // chrome" without fighting the data underneath. Legacy
                    // hairline fallback (further down) keeps the original
                    // theme-driven colour, since it's the only chrome that
                    // surfaces those shapes.
                    let bracket_color = Hsla {
                        h: 0.0,
                        s: 0.0,
                        l: 0.3,
                        a: 0.8,
                    };
                    let bracket_w = if is_selected { 1.5 } else { 1.0 };
                    let legacy_color = custom_stroke.unwrap_or(colors.muted);
                    let (ay_screen, by_screen) = match (p0, p1) {
                        (Some(p0), Some(p1)) => {
                            let (_, ay) = to_screen((*t0, *p0));
                            let (_, by) = to_screen((*t1, *p1));
                            (Some(ay), Some(by))
                        }
                        _ => (None, None),
                    };
                    match (ay_screen, by_screen) {
                        (Some(ay), Some(by)) => {
                            // Dashed guide line + corner handles only when
                            // selected. Unselected FRVPs render as just the
                            // profile bars — the guide is purely a selection
                            // affordance, not a permanent chrome that would
                            // clutter the chart when the user isn't editing.
                            if is_selected {
                                // Dashed line from (x0, ay) → (x1, by). Walk
                                // the parametric `t ∈ [0,1]` in pixel-length
                                // steps, alternating on/off segments. 5-on /
                                // 4-off reads as a noticeable dash without
                                // becoming visually noisy when the FRVP is
                                // small.
                                let dx = x1 - x0;
                                let dy = by - ay;
                                let total_len = (dx * dx + dy * dy).sqrt().max(1.0);
                                let dash_on = 5.0_f32;
                                let dash_off = 4.0_f32;
                                let stride = dash_on + dash_off;
                                let mut covered = 0.0_f32;
                                while covered < total_len {
                                    let seg_end = (covered + dash_on).min(total_len);
                                    let t_start = covered / total_len;
                                    let t_end = seg_end / total_len;
                                    let sx = x0 + dx * t_start;
                                    let sy = ay + dy * t_start;
                                    let ex = x0 + dx * t_end;
                                    let ey = ay + dy * t_end;
                                    paint_line(window, sx, sy, ex, ey, bracket_color, bracket_w);
                                    covered += stride;
                                }
                                // Handles sit on the corner anchors: A at
                                // (x0, ay), B at (x1, by).
                                paint_handle(window, x0, ay);
                                paint_handle(window, x1, by);
                            }
                        }
                        _ => {
                            // Legacy bracket — two vertical hairlines, no
                            // resize handles. Uses the theme-driven colour
                            // (with selection bumping to the ring colour
                            // via `legacy_color`'s caller code path) so the
                            // shape stays visible without the dashed guide.
                            let lc = if is_selected {
                                colors.ring
                            } else {
                                legacy_color
                            };
                            paint_line(window, xmin, 0.0, xmin, h, lc, bracket_w);
                            paint_line(window, xmax, 0.0, xmax, h, lc, bracket_w);
                        }
                    }
                    // Paint the profile only when we have cells aggregated.
                    // The overlay's vertical band is [0, h] (we bottom-clipped
                    // the wrapper div by AXIS_GAP); FRVP shares the same
                    // price->y mapping VRVP uses by passing the same y_lo /
                    // y_hi the overlay was built with.
                    if let Some(out) = output.as_ref() {
                        if !out.buckets.is_empty() && frvp_w > 1.0 {
                            crate::volume_profile::paint::paint_volume_profile(
                                window,
                                cx,
                                origin,
                                xmin,
                                frvp_w,
                                10.0,
                                h,
                                y_lo,
                                y_hi,
                                out,
                                params,
                                colors.muted,
                            );
                        }
                    }
                }
                // Text painted as a positioned div outside the overlay.
                _ => {}
            }

            // Optional top-right label. Only Rect / Fibonacci / Long / Short
            // surface a user-editable label today — Line / Arrow / AnchoredVwap
            // were trimmed at user request (visual noise outweighed the value).
            // HorizontalRay and Text paint their own text inline / as a div.
            if is_preview {
                return;
            }
            let label_str = match style.label.as_deref() {
                Some(s) if !s.is_empty() => s,
                _ => return,
            };
            // (label_anchor_x, label_anchor_y, right_align)
            //  - right_align=true → text is right-aligned to anchor_x (the
            //    label's right edge lands at anchor_x). Used by Rect so the
            //    label sits above the rect's right edge.
            //  - right_align=false → text is left-aligned at `anchor_x + 4`;
            //    anchor_y is the y of the label's top edge.
            let (label_anchor_x, label_anchor_y, right_align) = match d {
                Drawing::Rect { a, b, .. } => {
                    let (ax, ay) = to_screen(*a);
                    let (bx, by) = to_screen(*b);
                    let xmax = ax.max(bx);
                    let ymin = ay.min(by);
                    // Above the rect's top edge, right-aligned to the rect's
                    // right edge — sits cleanly above the corner without
                    // overlapping the stroke.
                    (xmax, ymin - 16.0, true)
                }
                Drawing::Fibonacci { a, b, .. } => {
                    let (ax, ay) = to_screen(*a);
                    let (bx, by) = to_screen(*b);
                    // Fib's top horizontal line sits at `ay.min(by)`. Float the
                    // label well above it so it doesn't overlap the level line
                    // or its ratio chip on the right.
                    (ax.max(bx), ay.min(by) - 22.0, false)
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
                    let (_, y_entry) = to_screen((*t0, *entry));
                    let (_, y_tp) = to_screen((*t0, *take_profit));
                    let (_, y_sl) = to_screen((*t0, *stop_loss));
                    // The E / TP / SL chips render at `top(y - 7)` and are
                    // ~12 px tall. Drop the user label clearly above the
                    // top-most chip so the chip background doesn't hide it.
                    let ytop = y_entry.min(y_tp).min(y_sl);
                    (x0.max(x1), ytop - 22.0, false)
                }
                // HorizontalRay paints its own inline label; Text's content IS
                // the label; Line / Arrow / AnchoredVwap intentionally suppress.
                _ => return,
            };
            let label_owned = SharedString::from(label_str.to_string());
            let label_len = label_owned.len();
            let run = TextRun {
                len: label_len,
                font: window.text_style().font(),
                color: stroke,
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            let line = window
                .text_system()
                .shape_line(label_owned, px(11.0), &[run], None);
            let (paint_x, paint_y) = if right_align {
                let text_w = line.width().as_f32();
                // Right edge of the text lands on label_anchor_x.
                (label_anchor_x - text_w, label_anchor_y)
            } else {
                (label_anchor_x + 4.0, label_anchor_y)
            };
            let _ = line.paint(
                point(px(paint_x) + origin.x, px(paint_y) + origin.y),
                px(11.0),
                gpui::TextAlign::Left,
                None,
                window,
                cx,
            );
        };

    for d in drawings {
        let sel = selected == Some(d.id());
        draw_one(window, cx, d, sel, false);
    }
    if let Some(preview) = creating_preview {
        draw_one(window, cx, preview, false, true);
    }

    // Crosshair guide lines. Painted last so they sit above all drawings.
    // Coords are canvas-relative (cursor and cross_x are both written in
    // canvas-relative coords by the chart's mouse_move handlers); the
    // overlay div is positioned at `(0, 0)` inside the canvas div, so
    // canvas-relative == overlay-relative for x/y.
    //
    // Vertical line uses `cross_x`: when the cursor sits over a sub-pane,
    // the main pane still shows the time-axis guide so the user can see
    // which bar the sub-pane reading lines up with. Horizontal line stays
    // gated on `cursor` (= main-pane hover) since the main pane's y range
    // is meaningless from a sub-pane.
    let chart_w = bounds.size.width.as_f32();
    let chart_h = bounds.size.height.as_f32();
    let cross_color = Hsla {
        a: 0.55,
        ..colors.muted
    };
    if let Some(cx_local) = cross_x {
        if cx_local >= 0.0 && cx_local <= chart_w {
            paint_line(window, cx_local, 0.0, cx_local, chart_h, cross_color, 0.0);
        }
    }
    if let Some((_cx_local, cy_local)) = cursor {
        if cy_local >= 0.0 && cy_local <= chart_h {
            paint_line(window, 0.0, cy_local, chart_w, cy_local, cross_color, 0.0);
        }
    }
}

/// Per-render snapshot of one overlay indicator for the paint closure.
/// Capturing only `(colors, output)` avoids holding a reference back into
/// `ChartState`, so the paint closure can be `'static` like the rest of
/// the chart's canvas closures. `colors[0]` is the primary line; multi-
/// series kinds index further.
pub struct OverlayPaintItem {
    pub colors: Vec<Hsla>,
    pub output: IndicatorOutput,
}

impl OverlayPaintItem {
    /// Color for a slot index; falls back to slot 0 (then to a palette
    /// default) when the requested slot isn't allocated. Lets paint code
    /// read by-slot without bounds-checking inline at every call site.
    pub fn color_at(&self, slot: usize) -> Hsla {
        self.colors
            .get(slot)
            .or_else(|| self.colors.first())
            .copied()
            .unwrap_or(gpui::hsla(0.0, 0.85, 0.55, 1.0))
    }
}

/// Paint overlay indicators on top of the candles, before drawings. Pane
/// indicators aren't routed here — they live in sibling canvases (added
/// in the multi-pane restructure). Lines (SMA/EMA) and Bands (BB) share
/// the main pane's price y-range so they sit on the candles. Volume-as-
/// overlay uses the bottom 20% of the pane as a histogram band, anchored
/// at 0, with up/down tint and low alpha so candles stay visible behind.
#[allow(clippy::too_many_arguments)]
pub(super) fn paint_overlay_indicators(
    bounds: Bounds<Pixels>,
    start_idx: usize,
    visible_count: usize,
    view_start: f32,
    view_size: f32,
    y_lo: f64,
    y_hi: f64,
    y_axis_gap: f32,
    items: &[OverlayPaintItem],
    bullish: Hsla,
    bearish: Hsla,
    label_color: Hsla,
    window: &mut Window,
    cx: &mut App,
) {
    let canvas_w = bounds.size.width.as_f32();
    let canvas_h = bounds.size.height.as_f32();
    let chart_w = (canvas_w - y_axis_gap).max(0.0);
    let chart_top = 10.0_f32;
    let chart_bottom = (canvas_h - AXIS_GAP).max(chart_top + 1.0);
    let origin = bounds.origin;
    let visible_end = start_idx.saturating_add(visible_count);

    // Slot width mirrors the candle paint pipeline so volume bars line up
    // with candle bodies. Uses the shared `slot_body_width` helper so the
    // gap policy stays in lockstep with the main candles (cap on zoom-in).
    let slot_w = (chart_w / view_size).max(0.5);
    let bar_w = slot_body_width(slot_w);

    for item in items {
        let primary = item.color_at(0);
        match &item.output {
            IndicatorOutput::Line(series) => {
                paint_line_series(
                    series,
                    start_idx,
                    visible_end,
                    view_start,
                    view_size,
                    canvas_w,
                    y_axis_gap,
                    y_lo,
                    y_hi,
                    chart_top,
                    chart_bottom,
                    primary,
                    1.5,
                    origin,
                    window,
                );
            }
            IndicatorOutput::Lines(series_list) => {
                // One line per slot, in declared order. MA Suite drives
                // this — each user-added MA paints with the matching
                // `colors[i]` slot.
                for (i, series) in series_list.iter().enumerate() {
                    paint_line_series(
                        series,
                        start_idx,
                        visible_end,
                        view_start,
                        view_size,
                        canvas_w,
                        y_axis_gap,
                        y_lo,
                        y_hi,
                        chart_top,
                        chart_bottom,
                        item.color_at(i),
                        1.5,
                        origin,
                        window,
                    );
                }
            }
            IndicatorOutput::Bands {
                upper,
                middle,
                lower,
            } => {
                // Lower-alpha middle so upper/lower envelopes read as the
                // primary lines (TV convention).
                let mid_color = Hsla {
                    a: primary.a * 0.6,
                    ..primary
                };
                for (series, color, width) in [
                    (upper, primary, 1.5),
                    (lower, primary, 1.5),
                    (middle, mid_color, 1.0),
                ] {
                    paint_line_series(
                        series,
                        start_idx,
                        visible_end,
                        view_start,
                        view_size,
                        canvas_w,
                        y_axis_gap,
                        y_lo,
                        y_hi,
                        chart_top,
                        chart_bottom,
                        color,
                        width,
                        origin,
                        window,
                    );
                }
            }
            IndicatorOutput::Histogram { values, up } => {
                // Volume overlay: bottom 20% of pane, anchored at 0.
                let pane_h = chart_bottom - chart_top;
                let band_h = (pane_h * 0.20).max(20.0);
                let band_bottom = chart_bottom;
                let band_top = chart_bottom - band_h;
                // Max volume across visible range for proportional scaling.
                let mut max_v: f64 = 0.0;
                for v in values[start_idx.min(values.len())..visible_end.min(values.len())]
                    .iter()
                    .filter_map(|v| *v)
                {
                    if v > max_v {
                        max_v = v;
                    }
                }
                if max_v <= 0.0 {
                    continue;
                }
                let alpha = 0.45_f32;
                let up_color = Hsla {
                    a: alpha,
                    ..bullish
                };
                let down_color = Hsla {
                    a: alpha,
                    ..bearish
                };
                for i in start_idx..visible_end.min(values.len()) {
                    let Some(v) = values[i] else { continue };
                    if v <= 0.0 {
                        continue;
                    }
                    let cx_px =
                        index_to_screen(view_start, view_size, i as f32, canvas_w, y_axis_gap);
                    if cx_px < -bar_w || cx_px > chart_w + bar_w {
                        continue;
                    }
                    let h = (v / max_v) as f32 * band_h;
                    let bar_x = cx_px - bar_w * 0.5;
                    let bar_y = band_bottom - h;
                    if bar_y >= band_top - 0.5 {
                        let color = if up.get(i).copied().unwrap_or(true) {
                            up_color
                        } else {
                            down_color
                        };
                        fill_rect(window, origin, bar_x, bar_w, bar_y, h, color);
                    }
                }
            }
            IndicatorOutput::Macd { .. }
            | IndicatorOutput::BarStat { .. }
            | IndicatorOutput::LiquidationBars { .. } => {
                // MACD, BarStat, and LiquidationBars are pane-only; ignore
                // here. Pane render routes them to `paint_sub_pane`.
            }
            IndicatorOutput::VolumeProfile { output, params } => {
                // VP renders inside the same price band as the candles
                // (top = 10px chrome below the top edge, bottom above the
                // x-axis gutter). `chart_left = 0` because the overlay
                // shares the candle pane's coordinate system; `chart_w`
                // excludes the y-axis gutter on the right.
                crate::volume_profile::paint::paint_volume_profile(
                    window,
                    cx,
                    origin,
                    0.0,
                    chart_w,
                    chart_top,
                    chart_bottom,
                    y_lo,
                    y_hi,
                    output,
                    params,
                    label_color,
                );
            }
        }
    }
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

// ============================================================================
// Sub-pane paint — one canvas per `Placement::Pane` indicator
// ============================================================================
//
// Sub-panes share the time axis with the main pane (same view_start /
// view_size / y_axis_gap), but each gets its own y range computed from the
// indicator's `y_range()` over the visible bar slice. No bottom-axis gutter
// — the time axis labels live on the main pane.

/// Per-render snapshot of one pane indicator for its paint closure. y_lo /
/// y_hi come from `IndicatorKind::y_range` computed at render time, so the
/// paint closure stays trait-object-free and `'static`.
pub struct PanePaintItem {
    /// Per-slot draw colors, indexed parallel to the kind's
    /// `color_slots()`. Slot 0 is the primary line; multi-series kinds
    /// (MACD, future Ichimoku/etc.) index further. Paint code reads via
    /// `color_at(slot)` so missing slots fall back to slot 0.
    pub colors: Vec<Hsla>,
    pub output: IndicatorOutput,
    pub kind_id: &'static str,
    pub y_lo: f64,
    pub y_hi: f64,
    /// When true, the pane keeps its full `pane_height` so the chip overlay
    /// at top-left remains reachable as an un-hide affordance, but
    /// `paint_sub_pane` skips all painting (no grid, no data, no axis).
    pub hidden: bool,
}

impl PanePaintItem {
    /// Color for `slot`. Falls back to slot 0, then to a palette default,
    /// so paint code can read by index without inline bounds checks.
    pub fn color_at(&self, slot: usize) -> Hsla {
        self.colors
            .get(slot)
            .or_else(|| self.colors.first())
            .copied()
            .unwrap_or(gpui::hsla(0.0, 0.85, 0.55, 1.0))
    }
}

/// Paint one indicator into its sub-pane canvas: y-axis grid + labels (right
/// gutter), kind-specific guides (RSI 30/70 dashed), the series itself
/// (line / histogram / MACD trio), and the crosshair chrome. The sub-pane
/// has a tight 2px top padding and uses the full canvas height — no bottom
/// gutter, since the time-axis labels live on the main pane.
///
/// `cursor_x` is the canvas-relative x of the cursor in WHICHEVER pane
/// is currently hovered — sub-panes share a time axis, so the vertical
/// guide paints at the same x across all of them. `hovered_y` is set
/// only when THIS sub-pane is the one being hovered; it drives the
/// horizontal y-line + the value-readout pill on the right gutter.
#[allow(clippy::too_many_arguments)]
pub(super) fn paint_sub_pane(
    bounds: Bounds<Pixels>,
    start_idx: usize,
    visible_count: usize,
    view_start: f32,
    view_size: f32,
    y_axis_gap: f32,
    item: &PanePaintItem,
    bullish: Hsla,
    bearish: Hsla,
    grid: Hsla,
    label_color: Hsla,
    cell_text_color: Hsla,
    cursor_x: Option<f32>,
    hovered_y: Option<f32>,
    window: &mut Window,
    cx: &mut App,
) {
    // Hidden pane: keep the slot at full height but paint nothing.
    // The chip overlay at top-left (rendered as a sibling div in chart.rs)
    // still shows, giving the user a clickable un-hide affordance.
    if item.hidden {
        return;
    }
    let canvas_w = bounds.size.width.as_f32();
    let canvas_h = bounds.size.height.as_f32();
    let chart_w = (canvas_w - y_axis_gap).max(0.0);
    let chart_top = 2.0_f32;
    let chart_bottom = canvas_h.max(chart_top + 1.0);
    let origin = bounds.origin;
    let visible_end = start_idx.saturating_add(visible_count);
    let (y_lo, y_hi) = (item.y_lo, item.y_hi);

    // BarStat owns its layout entirely (text cells, no scalar axis). Skip
    // the grid + y-axis tick computation + value pill — those would draw
    // meaningless 0.00/0.50/1.00 ticks against the dummy y_range. Crosshair
    // vertical guide still useful for time alignment with neighbouring panes.
    if let IndicatorOutput::BarStat {
        grade,
        volume,
        delta,
        daily_max_vol,
        daily_max_delta,
    } = &item.output
    {
        let slot_w = (chart_w / view_size.max(1.0)).max(0.5);
        paint_bar_stat_pane(
            start_idx,
            visible_end,
            view_start,
            view_size,
            canvas_w,
            y_axis_gap,
            chart_w,
            chart_top,
            chart_bottom,
            slot_w,
            *grade,
            volume,
            delta,
            daily_max_vol,
            daily_max_delta,
            bullish,
            bearish,
            cell_text_color,
            origin,
            window,
            cx,
        );
        if let Some(cx_local) = cursor_x {
            if cx_local >= 0.0 && cx_local <= chart_w {
                let cross_color = Hsla {
                    a: 0.55,
                    ..label_color
                };
                fill_rect(
                    window,
                    origin,
                    cx_local,
                    1.0,
                    chart_top,
                    chart_bottom - chart_top,
                    cross_color,
                );
            }
        }
        return;
    }

    // -- y-axis ticks --
    let target_y_count = ((chart_bottom - chart_top) / 36.0).floor().max(2.0) as usize;
    let y_step = pick_nice_y_step(y_hi - y_lo, target_y_count);
    let mut y_ticks: Vec<f64> = Vec::new();
    if y_step > 0.0 && y_step.is_finite() {
        let first = (y_lo / y_step).ceil() * y_step;
        let mut t = first;
        // Safety cap — degenerate (lo, hi) can't spin forever.
        for _ in 0..50 {
            if t > y_hi + 1e-9 {
                break;
            }
            y_ticks.push(t);
            t += y_step;
        }
    }

    // -- horizontal grid (1px quads) --
    for &y_val in &y_ticks {
        let y = band_y(y_lo, y_hi, y_val, chart_top, chart_bottom);
        if y < chart_top || y > chart_bottom {
            continue;
        }
        fill_rect(window, origin, 0.0, chart_w, y, 1.0, grid);
    }

    // -- the indicator's series --
    let slot_w = (chart_w / view_size.max(1.0)).max(0.5);
    let bar_w = slot_body_width(slot_w);
    match &item.output {
        IndicatorOutput::Line(series) => {
            // RSI overbought/oversold/midline guides. Stronger-alpha grid so
            // the dashes read as a distinct annotation layer.
            if item.kind_id == "rsi" {
                let dash_color = Hsla {
                    a: (grid.a * 1.5).min(1.0),
                    ..grid
                };
                for level in [70.0_f64, 30.0_f64] {
                    if level < y_lo || level > y_hi {
                        continue;
                    }
                    let y = band_y(y_lo, y_hi, level, chart_top, chart_bottom);
                    paint_dashed_horizontal(window, origin, 0.0, chart_w, y, dash_color);
                }
            }
            paint_line_series(
                series,
                start_idx,
                visible_end,
                view_start,
                view_size,
                canvas_w,
                y_axis_gap,
                y_lo,
                y_hi,
                chart_top,
                chart_bottom,
                item.color_at(0),
                1.5,
                origin,
                window,
            );
        }
        IndicatorOutput::Histogram { values, up } => {
            // Volume-as-pane: full-pane histogram anchored at zero. Higher
            // alpha than the overlay-mode variant since there are no candles
            // to share the band with.
            let alpha = 0.7_f32;
            let up_color = Hsla {
                a: alpha,
                ..bullish
            };
            let down_color = Hsla {
                a: alpha,
                ..bearish
            };
            let zero_y = band_y(y_lo, y_hi, 0.0, chart_top, chart_bottom);
            for i in start_idx..visible_end.min(values.len()) {
                let Some(v) = values[i] else { continue };
                if v <= 0.0 {
                    continue;
                }
                let cx_px = index_to_screen(view_start, view_size, i as f32, canvas_w, y_axis_gap);
                if cx_px < -bar_w || cx_px > chart_w + bar_w {
                    continue;
                }
                let y_top = band_y(y_lo, y_hi, v, chart_top, chart_bottom);
                let h = (zero_y - y_top).max(1.0);
                let bar_x = cx_px - bar_w * 0.5;
                let color = if up.get(i).copied().unwrap_or(true) {
                    up_color
                } else {
                    down_color
                };
                fill_rect(window, origin, bar_x, bar_w, y_top, h, color);
            }
        }
        IndicatorOutput::Macd {
            macd,
            signal,
            histogram,
        } => {
            // Histogram first (behind lines). Sign drives color: positive bars
            // bullish-tinted, negative bars bearish-tinted, both at reduced
            // alpha so the macd/signal lines on top stay legible.
            let zero_y = band_y(y_lo, y_hi, 0.0, chart_top, chart_bottom);
            let up_color = Hsla { a: 0.65, ..bullish };
            let down_color = Hsla { a: 0.65, ..bearish };
            for i in start_idx..visible_end.min(histogram.len()) {
                let Some(v) = histogram[i] else { continue };
                let cx_px = index_to_screen(view_start, view_size, i as f32, canvas_w, y_axis_gap);
                if cx_px < -bar_w || cx_px > chart_w + bar_w {
                    continue;
                }
                let y_val = band_y(y_lo, y_hi, v, chart_top, chart_bottom);
                let color = if v >= 0.0 { up_color } else { down_color };
                let (top, h) = if y_val <= zero_y {
                    (y_val, (zero_y - y_val).max(1.0))
                } else {
                    (zero_y, (y_val - zero_y).max(1.0))
                };
                fill_rect(window, origin, cx_px - bar_w * 0.5, bar_w, top, h, color);
            }
            // Zero line, on top of histogram but behind the macd/signal lines.
            fill_rect(window, origin, 0.0, chart_w, zero_y, 1.0, grid);
            // Slot 0 → macd line, slot 1 → signal line. `color_at` falls
            // back to slot 0 if slot 1 isn't allocated, so kinds that
            // someday emit a single-line MACD-shaped output still render.
            let macd_color = item.color_at(0);
            let signal_color = item.color_at(1);
            for (series, color) in [(macd, macd_color), (signal, signal_color)] {
                paint_line_series(
                    series,
                    start_idx,
                    visible_end,
                    view_start,
                    view_size,
                    canvas_w,
                    y_axis_gap,
                    y_lo,
                    y_hi,
                    chart_top,
                    chart_bottom,
                    color,
                    1.5,
                    origin,
                    window,
                );
            }
        }
        IndicatorOutput::LiquidationBars {
            long_qty,
            long_quote_qty,
            short_qty,
            short_quote_qty,
            params,
            unit,
        } => {
            // Two-sided histogram around the zero line. Long-liq plots
            // *downward* (negative y) in bearish red; short-liq plots
            // *upward* (positive y) in bullish green. Series unit follows
            // the chart's VolumeUnit toggle — paint just reads whichever
            // pair matches `unit`.
            let (longs, shorts) = match unit {
                crate::persistence::VolumeUnit::Coin => (long_qty, short_qty),
                crate::persistence::VolumeUnit::Usd => (long_quote_qty, short_quote_qty),
            };
            let long_color = params.long_color.unwrap_or(Hsla { a: 0.78, ..bearish });
            let short_color = params.short_color.unwrap_or(Hsla { a: 0.78, ..bullish });
            let zero_y = band_y(y_lo, y_hi, 0.0, chart_top, chart_bottom);
            for i in start_idx..visible_end.min(longs.len()) {
                let cx_px =
                    index_to_screen(view_start, view_size, i as f32, canvas_w, y_axis_gap);
                if cx_px < -bar_w || cx_px > chart_w + bar_w {
                    continue;
                }
                let bar_x = cx_px - bar_w * 0.5;
                // Long-liq → negative slot (drawn downward from 0).
                if let Some(v) = longs[i] {
                    if v > 0.0 {
                        let y_bot = band_y(y_lo, y_hi, -v, chart_top, chart_bottom);
                        let h = (y_bot - zero_y).max(1.0);
                        fill_rect(window, origin, bar_x, bar_w, zero_y, h, long_color);
                    }
                }
                // Short-liq → positive slot (drawn upward from 0).
                if let Some(v) = shorts[i] {
                    if v > 0.0 {
                        let y_top = band_y(y_lo, y_hi, v, chart_top, chart_bottom);
                        let h = (zero_y - y_top).max(1.0);
                        fill_rect(window, origin, bar_x, bar_w, y_top, h, short_color);
                    }
                }
            }
            // Zero baseline on top of the bars so the split is glance-able.
            fill_rect(window, origin, 0.0, chart_w, zero_y, 1.0, grid);

            // Optional cumulative-net overlay line: running sum of
            // (short - long) across the visible bar slice. Zero-clamped at
            // visible-window start so the line always begins at 0 and
            // diverges based on visible activity (matches the way most
            // liquidation dashboards render the metric).
            if params.show_cumulative {
                let cum_color = params.cumulative_color.unwrap_or(label_color);
                let mut cum_series: Vec<Option<f64>> = vec![None; longs.len()];
                let mut acc = 0.0_f64;
                let mut seen_any = false;
                for i in start_idx..visible_end.min(longs.len()) {
                    let long_v = longs[i].unwrap_or(0.0);
                    let short_v = shorts[i].unwrap_or(0.0);
                    if longs[i].is_some() || shorts[i].is_some() {
                        seen_any = true;
                    }
                    if seen_any {
                        acc += short_v - long_v;
                        cum_series[i] = Some(acc);
                    }
                }
                paint_line_series(
                    &cum_series,
                    start_idx,
                    visible_end,
                    view_start,
                    view_size,
                    canvas_w,
                    y_axis_gap,
                    y_lo,
                    y_hi,
                    chart_top,
                    chart_bottom,
                    cum_color,
                    1.5,
                    origin,
                    window,
                );
            }
        }
        IndicatorOutput::Bands { .. }
        | IndicatorOutput::Lines(_)
        | IndicatorOutput::BarStat { .. }
        | IndicatorOutput::VolumeProfile { .. } => {
            // Bands (BB) and Lines (MA Suite) are overlay-only by kind
            // contract; no-op here for safety. BarStat is handled up
            // front (before the grid pass) since it owns its own layout.
            // VolumeProfile is overlay-only (VRVP is `OverlayOnly`); never
            // routes to a sub-pane.
        }
    }

    // -- crosshair guide lines --
    //
    // Vertical guide paints whenever the cursor sits over ANY pane (cross-
    // pane shared time axis). Horizontal guide paints only when THIS sub-
    // pane is the hovered one. Value pill is painted *after* the y-axis
    // labels below so it sits on top of any colliding tick label.
    let cross_color = Hsla {
        a: 0.55,
        ..label_color
    };
    if let Some(cx_local) = cursor_x {
        if cx_local >= 0.0 && cx_local <= chart_w {
            fill_rect(
                window,
                origin,
                cx_local,
                1.0,
                chart_top,
                chart_bottom - chart_top,
                cross_color,
            );
        }
    }
    if let Some(cy_local) = hovered_y {
        if cy_local >= chart_top && cy_local <= chart_bottom {
            fill_rect(window, origin, 0.0, chart_w, cy_local, 1.0, cross_color);
        }
    }

    // -- y-axis labels (right gutter) --
    for &y_val in &y_ticks {
        let y = band_y(y_lo, y_hi, y_val, chart_top, chart_bottom);
        if y < chart_top - 6.0 || y > chart_bottom + 6.0 {
            continue;
        }
        let label = SharedString::from(format_pane_axis_label(item.kind_id, y_val));
        let run = TextRun {
            len: label.len(),
            font: window.text_style().font(),
            color: label_color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let line = window
            .text_system()
            .shape_line(label, px(10.0), &[run], None);
        let _ = line.paint(
            point(px(chart_w + 4.0) + origin.x, px(y - 5.0) + origin.y),
            px(10.0),
            gpui::TextAlign::Left,
            None,
            window,
            cx,
        );
    }

    // -- value-readout pill (after labels so it sits on top) --
    //
    // Translates the cursor-y back to data space via the inverse band map,
    // then formats with the same axis-label formatter so the pill reads
    // consistently with the static tick labels. Solid pill backing keeps
    // it legible against grid lines and the labels it overlays.
    if let Some(cy_local) = hovered_y {
        if cy_local >= chart_top && cy_local <= chart_bottom {
            let range = (y_hi - y_lo).max(1e-9);
            let t = ((cy_local - chart_top) / (chart_bottom - chart_top).max(1.0)) as f64;
            let v = y_hi - t * range;
            let label = SharedString::from(format_pane_axis_label(item.kind_id, v));
            let run = TextRun {
                len: label.len(),
                font: window.text_style().font(),
                color: label_color,
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            let line = window
                .text_system()
                .shape_line(label.clone(), px(10.0), &[run], None);
            let label_w = line.width().as_f32();
            let pill_pad_x = 4.0_f32;
            let pill_h = 14.0_f32;
            let pill_y = (cy_local - pill_h * 0.5).clamp(chart_top, chart_bottom - pill_h);
            let pill_w = label_w + pill_pad_x * 2.0;
            fill_rect(
                window,
                origin,
                chart_w + 2.0,
                pill_w,
                pill_y,
                pill_h,
                Hsla {
                    a: 0.85,
                    ..cross_color
                },
            );
            let _ = line.paint(
                point(
                    px(chart_w + 2.0 + pill_pad_x) + origin.x,
                    px(pill_y + 2.0) + origin.y,
                ),
                px(10.0),
                gpui::TextAlign::Left,
                None,
                window,
                cx,
            );
        }
    }
}

/// Paint the BarStat pane: one cell per visible bar, two text rows
/// (volume on top, signed delta below), optional heatmap fill keyed by
/// `grade`. Cell width follows the candle gap policy so the cells line up
/// with their candles on the main pane.
#[allow(clippy::too_many_arguments)]
fn paint_bar_stat_pane(
    start_idx: usize,
    visible_end: usize,
    view_start: f32,
    view_size: f32,
    canvas_w: f32,
    y_axis_gap: f32,
    chart_w: f32,
    chart_top: f32,
    chart_bottom: f32,
    slot_w: f32,
    grade: crate::indicators::BarStatGrade,
    volume: &[Option<f64>],
    delta: &[Option<f64>],
    daily_max_vol: &[Option<f64>],
    daily_max_delta: &[Option<f64>],
    bullish: Hsla,
    bearish: Hsla,
    text_color: Hsla,
    origin: Point<Pixels>,
    window: &mut Window,
    cx: &mut App,
) {
    use crate::indicators::BarStatGrade;

    let pane_h = (chart_bottom - chart_top).max(1.0);
    let row_h = pane_h * 0.5;
    let vol_y_top = chart_top;
    let delta_y_top = chart_top + row_h;
    // Cell fill width — fill the full slot so adjacent heatmap cells touch
    // (no gap stripe between bars in the pane). Text still centres in the
    // slot so it aligns with the candle above.
    let cell_w = slot_w.max(1.0);

    // VisibleRange mode normalises to the max absolute value across the
    // visible bar slice. Computed once up front so the per-bar loop is
    // a single division.
    let visible_max_vol = if matches!(grade, BarStatGrade::VisibleRange) {
        slice_max_abs(volume, start_idx, visible_end)
    } else {
        0.0
    };
    let visible_max_delta = if matches!(grade, BarStatGrade::VisibleRange) {
        slice_max_abs(delta, start_idx, visible_end)
    } else {
        0.0
    };

    // Fixed blue base for the volume row — keeps the volume cell visually
    // distinct from the bull/bear-tinted delta cell so the eye can read
    // the two rows as separate metrics rather than a single coloured bar.
    let volume_base = gpui::hsla(0.61, 0.80, 0.55, 1.0);

    // Auto-hide threshold for the per-cell text: when bars get narrower
    // than ~24px the K/M-formatted values stop being readable, so we drop
    // the label and just paint the heatmap fill. Keeps the pane useful
    // when zoomed all the way out.
    const TEXT_MIN_CELL_W: f32 = 24.0;
    let show_text = cell_w >= TEXT_MIN_CELL_W;

    for i in start_idx..visible_end.min(volume.len()) {
        let cx_px = index_to_screen(view_start, view_size, i as f32, canvas_w, y_axis_gap);
        if cx_px < -cell_w || cx_px > chart_w + cell_w {
            continue;
        }
        let cell_x = cx_px - cell_w * 0.5;
        let vol_v = volume.get(i).copied().flatten();
        let delta_v = delta.get(i).copied().flatten();

        // -- volume cell (top row) --
        if let Some(v) = vol_v {
            let intensity = match grade {
                BarStatGrade::Off => 0.0,
                BarStatGrade::Bar => 1.0,
                BarStatGrade::VisibleRange => {
                    if visible_max_vol > 0.0 {
                        (v.abs() / visible_max_vol) as f32
                    } else {
                        0.0
                    }
                }
                BarStatGrade::Daily => match daily_max_vol.get(i).copied().flatten() {
                    Some(mx) if mx > 0.0 => (v.abs() / mx) as f32,
                    _ => 0.0,
                },
            };
            if intensity > 0.0 {
                let color = grade_color(volume_base, intensity);
                fill_rect(window, origin, cell_x, cell_w, vol_y_top, row_h, color);
            }
            if show_text {
                paint_centred_text(
                    window,
                    cx,
                    origin,
                    cell_x,
                    cell_w,
                    vol_y_top,
                    row_h,
                    text_color,
                    &format_compact(v),
                );
            }
        }

        // -- delta cell (bottom row) --
        if let Some(v) = delta_v {
            let intensity = match grade {
                BarStatGrade::Off => 0.0,
                BarStatGrade::Bar => 1.0,
                BarStatGrade::VisibleRange => {
                    if visible_max_delta > 0.0 {
                        (v.abs() / visible_max_delta) as f32
                    } else {
                        0.0
                    }
                }
                BarStatGrade::Daily => match daily_max_delta.get(i).copied().flatten() {
                    Some(mx) if mx > 0.0 => (v.abs() / mx) as f32,
                    _ => 0.0,
                },
            };
            // Delta tint follows the sign of the delta itself, not the
            // candle. A bull candle with a sell-side delta still paints
            // bearish here — that disagreement is the signal.
            if intensity > 0.0 {
                let base = if v >= 0.0 { bullish } else { bearish };
                let color = grade_color(base, intensity);
                fill_rect(window, origin, cell_x, cell_w, delta_y_top, row_h, color);
            }
            if show_text {
                paint_centred_text(
                    window,
                    cx,
                    origin,
                    cell_x,
                    cell_w,
                    delta_y_top,
                    row_h,
                    text_color,
                    &format_signed_compact(v),
                );
            }
        }
    }
}

/// Max abs value over a contiguous slice of an `Option<f64>` series,
/// clamped to the actual series length. Returns 0.0 when the slice is
/// all-None.
fn slice_max_abs(series: &[Option<f64>], start: usize, end: usize) -> f64 {
    let lo = start.min(series.len());
    let hi = end.min(series.len());
    let mut mx = 0.0_f64;
    for v in series[lo..hi].iter().filter_map(|v| *v) {
        let av = v.abs();
        if av > mx {
            mx = av;
        }
    }
    mx
}

/// Map a 0..1 intensity to a tinted background. Starts at no tint
/// (alpha 0 when intensity is 0) so the lowest cells fade into the
/// pane background; ceiling at ~0.65 so the cell text stays readable
/// against the fill.
fn grade_color(base: Hsla, intensity: f32) -> Hsla {
    let t = intensity.clamp(0.0, 1.0);
    let alpha = 0.65 * t;
    Hsla { a: alpha, ..base }
}

/// Compact unsigned formatter used for the BarStat volume row. Mirrors
/// the K/M/B convention used by the volume axis labels so the two panes
/// read consistently. Honours the same "truncate cell decimals" global
/// flag as the footprint cell formatter — when on, fractional digits are
/// dropped (K/M/B suffix preserved).
fn format_compact(v: f64) -> String {
    let abs = v.abs();
    if crate::prefs::footprint_truncate_decimals() {
        if abs >= 1_000_000_000.0 {
            return format!("{:.0}B", (v / 1_000_000_000.0).round());
        } else if abs >= 1_000_000.0 {
            return format!("{:.0}M", (v / 1_000_000.0).round());
        } else if abs >= 1_000.0 {
            return format!("{:.0}K", (v / 1_000.0).round());
        }
        return format!("{:.0}", v.round());
    }
    if abs >= 1_000_000_000.0 {
        format!("{:.2}B", v / 1_000_000_000.0)
    } else if abs >= 1_000_000.0 {
        format!("{:.2}M", v / 1_000_000.0)
    } else if abs >= 1_000.0 {
        format!("{:.1}K", v / 1_000.0)
    } else if abs >= 10.0 {
        format!("{:.0}", v)
    } else {
        format!("{:.2}", v)
    }
}

/// Signed variant — emits a leading `-` for negative values; positives
/// render bare (the heatmap fill already encodes sign via bull/bear tint).
fn format_signed_compact(v: f64) -> String {
    let body = format_compact(v.abs());
    if v < 0.0 { format!("-{body}") } else { body }
}

/// Y-axis label formatter for sub-panes. Volume gets K/M/B shorthand so
/// large values (BTC daily ~$80B) don't crowd the gutter; oscillators and
/// other panes get plain 2dp.
fn format_pane_axis_label(kind_id: &str, v: f64) -> String {
    if kind_id == "volume" {
        let abs = v.abs();
        if abs >= 1_000_000_000.0 {
            return format!("{:.1}B", v / 1_000_000_000.0);
        } else if abs >= 1_000_000.0 {
            return format!("{:.1}M", v / 1_000_000.0);
        } else if abs >= 1_000.0 {
            return format!("{:.1}K", v / 1_000.0);
        }
    }
    format!("{:.2}", v)
}

/// Dashed 1px horizontal line, 4-on/3-off pattern. Mirrors the dashed style
/// used by the main chart's session markers, applied to RSI overbought/
/// oversold guides.
fn paint_dashed_horizontal(
    window: &mut Window,
    origin: Point<Pixels>,
    x0: f32,
    x1: f32,
    y: f32,
    color: Hsla,
) {
    let dash_on = 4.0_f32;
    let dash_off = 3.0_f32;
    let stride = dash_on + dash_off;
    let mut x = x0;
    while x < x1 {
        let seg_w = dash_on.min(x1 - x);
        fill_rect(window, origin, x, seg_w, y, 1.0, color);
        x += stride;
    }
}
