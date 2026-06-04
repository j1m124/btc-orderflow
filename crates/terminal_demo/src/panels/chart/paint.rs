//! Paint pipeline for the chart panel: candles + grid + axis labels (main
//! chart canvas) and the drawings overlay. Lifted out of `chart.rs` to keep
//! the parent module focused on state + interaction handlers. The parent
//! still owns `index_to_screen` / `price_to_screen` (coordinate math is
//! shared with hit-testing) and the `Drawing` enum (mutated by edit drags
//! that live in `chart.rs`'s mouse handlers).

use chrono::{DateTime, Datelike as _, FixedOffset, TimeZone as _};
use chrono_tz::US::Eastern;
use gpui::{
    App, BorderStyle, Bounds, ContentMask, Corners, Edges, Hsla, IntoElement, ParentElement as _,
    PaintQuad, PathBuilder, Pixels, Point, SharedString, Styled as _, TextRun, Window, canvas,
    div, point, px,
};
use gpui_component::plot::AXIS_GAP;

use super::{Drawing, DrawingId, index_to_screen, price_to_screen};
use crate::indicators::IndicatorOutput;
use crate::services::market_data::Candle;

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
    /// Solid fill for the axis gutters (right + bottom). Painted on top of
    /// the chart area so drawings that bleed past the chart-canvas
    /// boundary don't show through alongside the axis labels.
    pub axis_bg: Hsla,
    /// Thin divider between the chart area and the axis gutters.
    pub axis_border: Hsla,
    /// Colour for the ETH session-boundary dashed lines (RTH open/close
    /// markers). Slightly more saturated than `grid` so the dashes read as a
    /// distinct annotation rather than another grid line.
    pub session_marker: Hsla,
}

/// RTH session boundary in ET. `Open` = 09:30 (pre → RTH transition);
/// `Close` = 16:00 (RTH → post transition). Drives both the dashed line in
/// `paint_main_chart` and the "Open"/"Close" label divs emitted from the
/// chart-render path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SessionBoundary {
    Open,
    Close,
}

/// One session-boundary occurrence: canvas-relative x (pixels) + which
/// boundary it represents. Computed once in chart-render via
/// `compute_session_markers`, then used to (a) draw the dashed line via
/// `paint_main_chart` and (b) emit "Open"/"Close" labels above the chart.
#[derive(Clone, Copy, Debug)]
pub(super) struct SessionMarker {
    pub x: f32,
    pub kind: SessionBoundary,
}

/// Hardcoded RTH session boundaries in ET wall-clock minutes-from-midnight.
/// Half-day (early-close) sessions are ignored — the existing
/// `ext_session_tag` classifier in chart.rs takes the same simplification, so
/// markers and labels agree on session classification.
const RTH_OPEN_MIN: u32 = 9 * 60 + 30;
const RTH_CLOSE_MIN: u32 = 16 * 60;

/// Locate the two `Candle`s in `candles` whose `open_time`s bracket
/// `target_ms`. Returns `(i, frac)` such that the boundary sits at
/// `index = i + frac` in candle-space. Returns `None` when:
/// - the buffer is empty
/// - `target_ms` lies outside the visible bars' time span (no bracketing pair)
/// - bracketing bars are non-monotonic (defensive — shouldn't happen)
/// - the gap between the bracketing bars exceeds `max_gap_ms`, which signals
///   a closed-market gap (weekend, holiday, overnight). Without this check,
///   a Saturday 09:30 ET boundary would bracket Friday's last bar and
///   Monday's first bar and draw a spurious line in the weekend gap.
fn bracket_index(candles: &[Candle], target_ms: i64, max_gap_ms: i64) -> Option<(usize, f32)> {
    if candles.len() < 2 {
        return None;
    }
    // Binary search by open_time. `candles` is sorted by open_time so we can
    // partition on the first bar whose open_time >= target_ms.
    let pos = candles.partition_point(|c| c.open_time < target_ms);
    if pos == 0 || pos >= candles.len() {
        // target_ms is before the first bar or after the last — no bracket.
        return None;
    }
    let a = &candles[pos - 1];
    let b = &candles[pos];
    let span = b.open_time - a.open_time;
    if span <= 0 || span > max_gap_ms {
        return None;
    }
    let frac = ((target_ms - a.open_time) as f64 / span as f64).clamp(0.0, 1.0) as f32;
    Some((pos - 1, frac))
}

/// Walk the ET-local day range of `candles` (the *visible* slice already
/// trimmed by the caller) and emit a `SessionMarker` for each 09:30 and 16:00
/// boundary that falls between two adjacent bars. Days without bracketing
/// bars (weekends, holidays, missing data) emit nothing.
///
/// The caller is responsible for the activation gate (Extended session, not
/// 1d timeframe, pref enabled) — this function blindly computes whatever
/// boundaries fit the bars.
pub(super) fn compute_session_markers(
    candles: &[Candle],
    start_idx: usize,
    view_start: f32,
    view_size: f32,
    canvas_w: f32,
    y_axis_gap: f32,
    candle_interval_ms: i64,
) -> Vec<SessionMarker> {
    if candles.len() < 2 {
        return Vec::new();
    }
    let chart_w = (canvas_w - y_axis_gap).max(0.0);
    // Two adjacent bars from the same trading day are separated by exactly
    // `candle_interval_ms`. A gap larger than ~2× the interval signals a
    // closed-market hole — weekend, holiday, or overnight on intraday
    // timeframes — and any boundary that falls inside such a hole should
    // NOT produce a line (otherwise Saturday 09:30 ET would appear at the
    // Friday→Monday boundary). 2× is a soft buffer for missing single bars.
    let max_gap_ms = candle_interval_ms.saturating_mul(2);
    let mut out: Vec<SessionMarker> = Vec::new();
    // Iterate ET-local days from the first bar's date to the last bar's date,
    // emitting up to two boundaries per day. Dates rather than ms so DST
    // shifts are absorbed by chrono_tz when we re-materialise wall-clock
    // 09:30 / 16:00.
    let Some(first_et) = Eastern
        .timestamp_millis_opt(candles.first().unwrap().open_time)
        .single()
    else {
        return out;
    };
    let Some(last_et) = Eastern
        .timestamp_millis_opt(candles.last().unwrap().open_time)
        .single()
    else {
        return out;
    };
    let mut day = first_et.date_naive();
    let last_day = last_et.date_naive();
    // Safety bound — at 1m timeframe the visible buffer holds ~5k bars
    // (~3.5 days); 400 days is well past anything a user can zoom out to
    // and protects against pathological inputs.
    for _ in 0..400 {
        if day > last_day {
            break;
        }
        for (boundary_min, kind) in [
            (RTH_OPEN_MIN, SessionBoundary::Open),
            (RTH_CLOSE_MIN, SessionBoundary::Close),
        ] {
            let Some(naive) = day.and_hms_opt(boundary_min / 60, boundary_min % 60, 0) else {
                continue;
            };
            let Some(et_dt) = Eastern.from_local_datetime(&naive).single() else {
                continue;
            };
            let target_ms = et_dt.timestamp_millis();
            let Some((i, frac)) = bracket_index(candles, target_ms, max_gap_ms) else {
                continue;
            };
            let global_index = (start_idx + i) as f32 + frac;
            let x = index_to_screen(view_start, view_size, global_index, canvas_w, y_axis_gap);
            if x < 0.0 || x > chart_w {
                continue;
            }
            out.push(SessionMarker { x, kind });
        }
        let Some(next) = day.succ_opt() else { break };
        day = next;
    }
    out
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

/// Paint the main chart canvas: horizontal y-grid, vertical x-grid,
/// continuous-position candle wicks + bodies, and axis labels in the right
/// + bottom gutters. Runs in a single `canvas` paint pass — z-order is
/// determined by paint sequence (grid → candles → labels).
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
    session_markers: &[SessionMarker],
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
    let target_y_count = ((chart_bottom - chart_top) / 50.0).floor().max(2.0) as usize;
    let y_step = pick_nice_y_step(y_hi - y_lo, target_y_count);
    let mut y_ticks: Vec<f64> = Vec::new();
    if y_step > 0.0 && y_step.is_finite() {
        let first = (y_lo / y_step).ceil() * y_step;
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
            let center_x = index_to_screen(view_start, view_size, (start_idx + i) as f32, canvas_w, y_axis_gap);
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

    // -- 2b. ETH session boundary lines (dashed) --
    //
    // Drawn after the solid grid and before candles so the candles always
    // visually overlap the dashes (same z-order rule as the grid). Caller
    // already filtered for Session::Extended + non-1d + pref-on; here we
    // blindly paint whatever markers came in. Dashes are emitted as a stack
    // of short fill_rect quads (gpui paint has no dashed primitive) using a
    // 4-on / 3-off pattern.
    if !session_markers.is_empty() {
        let dash_on = 4.0_f32;
        let dash_off = 3.0_f32;
        let stride = dash_on + dash_off;
        let line_h = (chart_bottom - chart_top).max(0.0);
        for marker in session_markers {
            let mut y = chart_top;
            while y < chart_top + line_h {
                let seg_h = dash_on.min(chart_top + line_h - y);
                fill_rect(
                    window,
                    origin,
                    marker.x,
                    1.0,
                    y,
                    seg_h,
                    colors.session_marker,
                );
                y += stride;
            }
        }
    }

    // -- 3. candles (continuous x positions) --
    //
    // Wicks + bodies are painted as quads (not stroked paths) so per-candle
    // paint cost is a flat instanced primitive. Two regimes:
    //
    //  * Sparse (>= 1px per candle): draw each candle's wick + body normally.
    //  * Dense  (more candles than pixels): aggregate consecutive candles that
    //    fall in the same pixel column into a single high↔low bar coloured by
    //    the column's net direction. This bounds the primitive count to ~chart
    //    width regardless of how far the user zooms out, and avoids painting
    //    thousands of overlapping sub-pixel quads.
    let slot_width = chart_w / view_size.max(1.0);
    if slot_width >= 1.0 {
        let body_width = (slot_width * 0.7).max(1.0);
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
                colors.bullish
            } else {
                colors.bearish
            };

            // Wick (high ↔ low) as a 1px-wide quad.
            let wick_top = high_y.min(low_y);
            let wick_h = (high_y - low_y).abs().max(1.0);
            fill_rect(window, origin, center_x - 0.5, 1.0, wick_top, wick_h, color);

            // Body (open ↔ close). Min 1px height keeps doji visible.
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
        // Dense mode: bucket consecutive candles by integer pixel column.
        // `index_to_screen` is monotonic in idx, so columns are non-decreasing
        // and a single forward pass groups them.
        let mut i = 0usize;
        while i < candles.len() {
            let col = index_to_screen(view_start, view_size, (start_idx + i) as f32, canvas_w, y_axis_gap)
                .floor();
            let mut hi = candles[i].high;
            let mut lo = candles[i].low;
            let open = candles[i].open;
            let mut close = candles[i].close;
            let mut j = i + 1;
            while j < candles.len() {
                let cx = index_to_screen(view_start, view_size, (start_idx + j) as f32, canvas_w, y_axis_gap);
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
            let color = if close >= open {
                colors.bullish
            } else {
                colors.bearish
            };
            let hi_y = price_to_screen(y_lo, y_hi, hi, canvas_h);
            let lo_y = price_to_screen(y_lo, y_hi, lo, canvas_h);
            let top = hi_y.min(lo_y);
            let h = (hi_y - lo_y).abs().max(1.0);
            fill_rect(window, origin, col, 1.0, top, h, color);
        }
    }

    // -- 3.5 axis gutters --
    //
    // Paint solid backgrounds for the right (price) and bottom (time) gutters
    // BEFORE the labels so any drawing pixels that overflow the chart canvas
    // (e.g. text shaped near the right edge) are masked by the axis chrome
    // and don't visually collide with the price/time labels.
    if y_axis_gap > 0.0 {
        fill_rect(window, origin, chart_w, y_axis_gap, 0.0, canvas_h, colors.axis_bg);
        // 1px divider on the chart side.
        fill_rect(window, origin, chart_w, 1.0, 0.0, canvas_h, colors.axis_border);
    }
    if AXIS_GAP > 0.0 {
        fill_rect(window, origin, 0.0, chart_w, chart_bottom, AXIS_GAP, colors.axis_bg);
        fill_rect(window, origin, 0.0, chart_w, chart_bottom, 1.0, colors.axis_border);
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

/// Custom overlay that paints all committed drawings + the in-progress
/// preview + selection chrome on top of the candlestick canvas. Positioned
/// absolutely inside the chart-canvas div with the y-axis-label gutter and
/// x-axis-label band excluded so drawings don't paint over the axis area.
pub(super) fn render_drawings_overlay(
    drawings: Vec<Drawing>,
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

    let paint_line = |window: &mut Window, ax: f32, ay: f32, bx: f32, by: f32, color: Hsla| {
        let mut pb = PathBuilder::stroke(px(1.5));
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
                         _is_preview: bool| {
        let (x0, _) = to_screen((t0, entry));
        let (x1, _) = to_screen((t1, entry));
        let (xmin, xmax) = (x0.min(x1), x0.max(x1));
        let (_, y_entry) = to_screen((t0, entry));
        let (_, y_tp) = to_screen((t0, tp));
        let (_, y_sl) = to_screen((t0, sl));
        // TP zone always uses the bullish tint (profit zone), SL the bearish
        // (loss zone). Direction matters only for which side of entry each
        // sits on, which the caller already computed.
        paint_filled_zone(
            window,
            xmin,
            xmax,
            y_entry.min(y_tp),
            y_entry.max(y_tp),
            Hsla {
                a: 0.18,
                ..colors.bullish
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
                ..colors.bearish
            },
        );
        // Three horizontal lines. Entry in muted, TP/SL in their zone colour.
        let entry_color = if is_selected {
            colors.ring
        } else {
            colors.muted
        };
        let tp_color = if is_selected {
            colors.ring
        } else {
            colors.bullish
        };
        let sl_color = if is_selected {
            colors.ring
        } else {
            colors.bearish
        };
        paint_line(window, xmin, y_entry, xmax, y_entry, entry_color);
        paint_line(window, xmin, y_tp, xmax, y_tp, tp_color);
        paint_line(window, xmin, y_sl, xmax, y_sl, sl_color);

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

    let draw_one = |window: &mut Window,
                    cx: &mut App,
                    d: &Drawing,
                    is_selected: bool,
                    is_preview: bool| {
        let stroke = if is_selected || is_preview {
            colors.ring
        } else {
            colors.line
        };
        match d {
            Drawing::Line { a, b, .. } => {
                let (ax, ay) = to_screen(*a);
                let (bx, by) = to_screen(*b);
                paint_line(window, ax, ay, bx, by, stroke);
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
                paint_line(window, ax, ay, bx, by, stroke);
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
                    paint_line(window, bx, by, w1x, w1y, stroke);
                    paint_line(window, bx, by, w2x, w2y, stroke);
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
                let level_color = if is_selected || is_preview {
                    colors.ring
                } else {
                    colors.line
                };
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
                    paint_line(window, xmin, y, xmax, y, fade_color);
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
            Drawing::HorizontalRay { anchor, text, .. } => {
                // Horizontal ray: line from the anchor x to the right edge
                // of the overlay at the anchor's y. Anchors past the right
                // edge collapse to a dot at the edge so the user can still
                // find them.
                let (ax, ay) = to_screen(*anchor);
                let right_edge = bounds.size.width.as_f32();
                let overlay_h = bounds.size.height.as_f32();
                let start_x = ax.max(0.0).min(right_edge);
                // Skip painting entirely when the ray's y sits outside the
                // overlay — text isn't clipped by the overlay's bounds, so
                // without this guard the label would float in the axis
                // gutter even though the line itself is off-screen.
                let line_visible = ay >= 0.0 && ay <= overlay_h;
                if line_visible {
                    paint_line(window, start_x, ay, right_edge, ay, stroke);
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
                            let line = window
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
                            let label_x =
                                (right_edge - text_w - pad).max(start_x);
                            let label_y = ay - 14.0;
                            let _ = line.paint(
                                point(
                                    px(label_x) + origin.x,
                                    px(label_y) + origin.y,
                                ),
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
                                paint_line(window, px_, py_, sx, sy, stroke);
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
                let border_color = if is_selected || is_preview {
                    colors.ring
                } else {
                    colors.rect_border
                };
                window.paint_quad(PaintQuad {
                    bounds: rb,
                    corner_radii: Corners::default(),
                    background: colors.rect_fill.into(),
                    border_widths: Edges {
                        top: px(1.5),
                        right: px(1.5),
                        bottom: px(1.5),
                        left: px(1.5),
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
                );
            }
            // Text painted as a positioned div outside the overlay.
            _ => {}
        }
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
            paint_line(window, cx_local, 0.0, cx_local, chart_h, cross_color);
        }
    }
    if let Some((_cx_local, cy_local)) = cursor {
        if cy_local >= 0.0 && cy_local <= chart_h {
            paint_line(window, 0.0, cy_local, chart_w, cy_local, cross_color);
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
    window: &mut Window,
) {
    let canvas_w = bounds.size.width.as_f32();
    let canvas_h = bounds.size.height.as_f32();
    let chart_w = (canvas_w - y_axis_gap).max(0.0);
    let chart_top = 10.0_f32;
    let chart_bottom = (canvas_h - AXIS_GAP).max(chart_top + 1.0);
    let origin = bounds.origin;
    let visible_end = start_idx.saturating_add(visible_count);

    // Slot width mirrors the candle paint pipeline so volume bars line up
    // with candle bodies. 0.7 of the slot leaves narrow gaps between bars
    // for visual separation.
    let slot_w = (chart_w / view_size).max(0.5);
    let bar_w = (slot_w * 0.7).max(1.0);

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
            IndicatorOutput::Bands { upper, middle, lower } => {
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
                let up_color = Hsla { a: alpha, ..bullish };
                let down_color = Hsla { a: alpha, ..bearish };
                for i in start_idx..visible_end.min(values.len()) {
                    let Some(v) = values[i] else { continue };
                    if v <= 0.0 {
                        continue;
                    }
                    let cx_px = index_to_screen(
                        view_start,
                        view_size,
                        i as f32,
                        canvas_w,
                        y_axis_gap,
                    );
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
            IndicatorOutput::Macd { .. } => {
                // MACD is pane-only; ignore here. The multi-pane restructure
                // (T13) routes it to its own canvas.
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
    let bar_w = (slot_w * 0.7).max(1.0);
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
            let up_color = Hsla { a: alpha, ..bullish };
            let down_color = Hsla { a: alpha, ..bearish };
            let zero_y = band_y(y_lo, y_hi, 0.0, chart_top, chart_bottom);
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
                let cx_px =
                    index_to_screen(view_start, view_size, i as f32, canvas_w, y_axis_gap);
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
        IndicatorOutput::Bands { .. } | IndicatorOutput::Lines(_) => {
            // Bands (BB) and Lines (MA Suite) are overlay-only by kind
            // contract; no-op here for safety.
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
