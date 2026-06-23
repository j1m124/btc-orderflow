//! Main chart paint: candles + grid + axis labels. [`paint_main_chart`]'s
//! render-kind switch dispatches to the candlestick pipeline here
//! ([`paint_candle_bodies`]) or to the footprint renderers in
//! [`super::footprint_render`]. Shared paint primitives (`fill_rect`,
//! `slot_body_width`, `pick_nice_y_step`) live in the parent module.

use chrono::{DateTime, Datelike as _, FixedOffset, TimeZone as _};
use gpui::{App, Bounds, Hsla, Pixels, Point, SharedString, TextRun, Window, point, px};
use gpui_component::plot::AXIS_GAP;

use super::super::footprint::{FootprintParams, RenderKind};
use super::super::{index_to_screen, price_to_screen};
use super::footprint_render::{paint_bar_wireframes, paint_cluster_cells, paint_profile_bars};
use super::{fill_rect, pick_nice_y_step, slot_body_width};
use crate::persistence::VolumeUnit;
use crate::services::market_data::{Candle, FootprintCell};

// ============================================================================
// Main chart paint — candles + grid + axis labels
// ============================================================================
//
// Replaces gpui-component's `CandlestickChart` widget so candle positions are
// continuous in `view_start`. The widget binned candles into `ScaleBand`
// slots which meant fractional view_start changes had zero visual effect —
// horizontal pan felt chunky, and drawings (which already used
// `index_to_screen`) visually desynced from candles during pan.

pub struct MainChartColors {
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
pub fn paint_main_chart(
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

    // -- 1 & 2. grid lines (behind candles), toggled by Settings → Chart.
    // `x_ticks` / `y_ticks` are still computed unconditionally above because
    // the axis *labels* below always render — only the grid quads are gated.
    if crate::prefs::chart_show_grid() {
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

