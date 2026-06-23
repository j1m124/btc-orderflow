//! Footprint render layer: wireframe + cluster cells + volume-profile bars,
//! plus the per-bar slot layout and metric helpers they share. Dispatched
//! from [`super::main_chart::paint_main_chart`] when a footprint render kind
//! is active. Shared paint primitives live in the parent module.

use gpui::{App, Hsla, Pixels, Point, Window};

use super::super::footprint::{
    ColorScope, FootprintParams, RenderMetric, TextMetric, WireframeVariant,
};
use super::super::{index_to_screen, price_to_screen};
use super::{SIDE_OHLC_FRACTION, fill_rect, paint_centred_text, slot_body_width, slot_edge_pad};
use crate::persistence::VolumeUnit;
use crate::services::market_data::{Candle, FootprintCell};

/// Minimum cell pixel dimensions for in-cell text to render. Below this, the
/// number auto-hides — readability tested at the chart's 10pt text size with
/// 4-digit volumes. Tuned wider/taller than the bare legibility floor so
/// shrinking the chart hides text *before* cells get cramped and unreadable,
/// rather than after. Independent of `TextMetric::None`, which always hides.
const MIN_CELL_W_FOR_TEXT: f32 = 48.0;
const MIN_CELL_H_FOR_TEXT: f32 = 14.0;

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
pub(super) fn paint_bar_wireframes(
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
pub(super) fn paint_cluster_cells(
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

/// Paint per-bar volume profile bars (one horizontal bar per price bucket).
///
/// Bar length encodes `params.render_metric` (Volume: total; Delta: signed
/// magnitude; BidAsk: stacked left/right of bar center). Optional text label
/// is per `params.text_metric`, auto-hidden when the bar is too small.
///
/// No-op when `cells` is empty.
#[allow(clippy::too_many_arguments)]
pub(super) fn paint_profile_bars(
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

/// In-cell label for footprint volumes. When the user toggles "Round cell
/// decimals" on, fractional digits are dropped (cells render as whole numbers,
/// rounded to nearest; K/M suffix preserved). Otherwise the standard `K`/`M`
/// shorthand at 1dp with a 2dp tail for sub-10 values.
fn format_short(v: f64) -> String {
    let abs = v.abs();
    if crate::prefs::round_cell_decimals() {
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

