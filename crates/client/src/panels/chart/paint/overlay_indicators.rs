//! Overlay indicators: line / band / histogram / volume-profile indicators
//! painted in the main candle pane's coordinate space, on top of the candles
//! and before the drawings overlay. Pane-only outputs (MACD, BarStat, …) are
//! ignored here — they route to [`super::sub_panes`].

use gpui::{Bounds, Hsla, Pixels, Window};
use gpui_component::plot::AXIS_GAP;

use super::super::index_to_screen;
use super::{fill_rect, paint_line_series, slot_body_width};
use crate::indicators::IndicatorOutput;

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
pub fn paint_overlay_indicators(
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
            | IndicatorOutput::LiquidationBars { .. }
            | IndicatorOutput::OpenInterest { .. } => {
                // MACD, BarStat, LiquidationBars, and OpenInterest are
                // pane-only; ignore here. Pane render routes them to
                // `paint_sub_pane`.
            }
            IndicatorOutput::VolumeProfile { output, params } => {
                // VP renders inside the same price band as the candles
                // (top = 10px chrome below the top edge, bottom above the
                // x-axis gutter). `chart_left = 0` because the overlay
                // shares the candle pane's coordinate system; `chart_w`
                // excludes the y-axis gutter on the right.
                crate::volume_profile::paint::paint_volume_profile(
                    window,
                    origin,
                    0.0,
                    chart_w,
                    chart_top,
                    chart_bottom,
                    y_lo,
                    y_hi,
                    output,
                    params,
                );
            }
        }
    }
}

