//! Pure coordinate <-> screen mapping and label/number formatting for the
//! chart. No state and no gpui elements — just math and string formatting
//! shared by [`super::state`], [`super::drawing`], [`super::view`], and the
//! `paint/` submodules (which reach `index_to_screen` / `price_to_screen`
//! through the chart facade's re-export).

use gpui_component::plot::AXIS_GAP;

use crate::indicators::ValueReadout;

/// Format a bar's open_time in the user's chosen timezone for crosshair /
/// OHLC pill display. `Candle::date` is frozen at ingestion time using
/// `Local`, so the pre-formatted string ignores any later Settings change —
/// reading from `open_time` at render time fixes that.
pub(super) fn format_user_tz(open_time: i64, cx: &gpui::App) -> String {
    use chrono::TimeZone as _;
    let offset = crate::prefs::offset_for(cx, open_time);
    offset
        .timestamp_millis_opt(open_time)
        .single()
        .map(|dt| dt.format("%b %d %H:%M").to_string())
        .unwrap_or_default()
}

/// Width of the y-axis label gutter for a given price range. Labels paint
/// at px(10) via `format_price`, so the widest label width drives the
/// gutter. Clamped so the gutter never collapses (small prices) nor steals
/// the whole chart (anomalous ranges).
pub(super) fn compute_y_axis_gap(y_lo: f64, y_hi: f64) -> f32 {
    let widest = y_lo.abs().max(y_hi.abs());
    if !widest.is_finite() {
        return 52.0;
    }
    let label = format_price(widest);
    // Each character ~6.5 px at the px(10) font size used in paint, plus
    // 14 px combined left+right padding so labels don't kiss the chart.
    (label.len() as f32 * 6.5 + 14.0).clamp(44.0, 120.0)
}

/// Format a price value at the standard 2dp used across the main chart
/// (axis labels, OHLC pill, live-price pill, crosshair, ray pills, position
/// E/TP/SL labels). Single place so any future precision change can land
/// in one diff.
pub(super) fn format_price(value: f64) -> String {
    format!("{:.2}", value)
}

/// Convert a screen-space x pixel (relative to canvas origin) to a fractional
/// candle index. The chart paints candles inside `(0, width - y_axis_gap)`,
/// so we exclude the right-side label gutter from the mapping. The `-0.5`
/// offset aligns the click to the *centre* of each candle slot, matching
/// where `paint_main_chart` places each candle's body (centred within its
/// slot of width `chart_w / view_size`).
pub(super) fn screen_to_index(
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
pub(super) fn index_to_screen(
    view_start: f32,
    view_size: f32,
    index: f32,
    canvas_width: f32,
    y_axis_gap: f32,
) -> f32 {
    let chart_w = (canvas_width - y_axis_gap).max(1.0);
    (index - view_start + 0.5) / view_size * chart_w
}

/// Format an indicator's `ValueReadout` for the chip label. `None` slots
/// render as `—` so a no-history bar still reads cleanly. Volume values
/// use the K/M/B abbreviations standard in trading platforms; everything
/// else gets two decimals.
pub(super) fn format_readout(r: ValueReadout) -> String {
    // Non-breaking space: a blank readout that still holds the chip's line
    // height + the hover-button overlay anchor.
    const BLANK: &str = "\u{00A0}";
    let parts: Vec<Option<f64>> = match r {
        ValueReadout::Empty => return BLANK.to_string(),
        ValueReadout::One(a) => vec![a],
        ValueReadout::Two(a, b) => vec![a, b],
        ValueReadout::Three(a, b, c) => vec![a, b, c],
        ValueReadout::Many(vs) => vs,
    };
    // No value at this bar → blank, not a dash.
    if parts.iter().all(Option::is_none) {
        return BLANK.to_string();
    }
    parts
        .into_iter()
        .map(fmt_scalar)
        .collect::<Vec<_>>()
        .join(" / ")
}

pub(super) fn fmt_scalar(v: Option<f64>) -> String {
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
pub(super) fn price_to_screen(y_lo: f64, y_hi: f64, price: f64, canvas_height: f32) -> f32 {
    let top = 10.0_f32;
    let bottom = (canvas_height - AXIS_GAP).max(top + 1.0);
    let range = y_hi - y_lo;
    if range.abs() < 1e-9 {
        return (top + bottom) / 2.0;
    }
    let t = ((y_hi - price) / range) as f32;
    top + t * (bottom - top)
}

pub(super) fn screen_to_price(y_lo: f64, y_hi: f64, y_in_canvas: f32, canvas_height: f32) -> f64 {
    let top = 10.0_f32;
    let bottom = (canvas_height - AXIS_GAP).max(top + 1.0);
    let t = ((y_in_canvas - top) / (bottom - top)).clamp(-2.0, 2.0) as f64;
    y_hi - t * (y_hi - y_lo)
}

/// Snap a fractional candle index to its integer slot. Per Q10a, x-anchors
/// always snap so drawings sit on candle centres.
pub(super) fn snap_t(t: f32) -> f32 {
    t.round()
}
