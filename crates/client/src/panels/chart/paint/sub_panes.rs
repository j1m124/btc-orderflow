//! Sub-pane paint: one canvas per `Placement::Pane` indicator (RSI, MACD,
//! volume, liquidation bars, open interest, BarStat). Each pane shares the
//! main pane's time axis (same view_start / view_size / y_axis_gap) but
//! computes its own y range from the indicator's `y_range()`. BarStat owns its
//! own grid-less layout via [`paint_bar_stat_pane`]. Shared paint primitives
//! live in the parent module.

use gpui::{
    App, Bounds, Hsla, PathBuilder, Pixels, Point, SharedString, TextRun, Window, point, px,
};

use super::super::index_to_screen;
use super::{
    band_y, fill_rect, paint_centred_text, paint_line_series, pick_nice_y_step, slot_body_width,
};
use crate::indicators::IndicatorOutput;

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
pub fn paint_sub_pane(
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
        show_volume,
        show_delta,
        show_long_liq,
        show_short_liq,
        show_oi_delta,
        show_net_ls,
        volume,
        delta,
        long_liq,
        short_liq,
        oi_delta,
        net_ls,
        ob_depths,
        ob_imbalance,
        daily_max_ob,
        daily_max_vol,
        daily_max_delta,
        daily_max_long_liq,
        daily_max_short_liq,
        daily_max_oi_delta,
        daily_max_net_ls,
    } = &item.output
    {
        let slot_w = (chart_w / view_size.max(1.0)).max(0.5);
        paint_bar_stat_pane(
            BarStatGeom {
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
            },
            *grade,
            BarStatShow {
                volume: *show_volume,
                delta: *show_delta,
                long_liq: *show_long_liq,
                short_liq: *show_short_liq,
                oi_delta: *show_oi_delta,
                net_ls: *show_net_ls,
            },
            BarStatSeries {
                volume,
                delta,
                long_liq,
                short_liq,
                oi_delta,
                net_ls,
                ob_depths,
                ob_imbalance,
                daily_max_ob,
                daily_max_vol,
                daily_max_delta,
                daily_max_long_liq,
                daily_max_short_liq,
                daily_max_oi_delta,
                daily_max_net_ls,
            },
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

    // -- horizontal grid (1px quads) -- toggled by Settings → Chart.
    if crate::prefs::chart_show_grid() {
        for &y_val in &y_ticks {
            let y = band_y(y_lo, y_hi, y_val, chart_top, chart_bottom);
            if y < chart_top || y > chart_bottom {
                continue;
            }
            fill_rect(window, origin, 0.0, chart_w, y, 1.0, grid);
        }
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
            // Full-pane histogram anchored at zero. Fully opaque in pane mode —
            // there are no candles to share the band with, so transparency would
            // only wash the bars out. Signed-capable: positive bars draw upward
            // from zero, negative bars downward (funding). For volume (y_lo == 0)
            // every bar is positive, so this matches the prior upward-only draw.
            let up_color = Hsla { a: 1.0, ..bullish };
            let down_color = Hsla { a: 1.0, ..bearish };
            let zero_y = band_y(y_lo, y_hi, 0.0, chart_top, chart_bottom);
            for i in start_idx..visible_end.min(values.len()) {
                let Some(v) = values[i] else { continue };
                if v == 0.0 {
                    continue;
                }
                let cx_px = index_to_screen(view_start, view_size, i as f32, canvas_w, y_axis_gap);
                if cx_px < -bar_w || cx_px > chart_w + bar_w {
                    continue;
                }
                let y_val = band_y(y_lo, y_hi, v, chart_top, chart_bottom);
                let (top, h) = if y_val <= zero_y {
                    (y_val, (zero_y - y_val).max(1.0))
                } else {
                    (zero_y, (y_val - zero_y).max(1.0))
                };
                let bar_x = cx_px - bar_w * 0.5;
                let color = if up.get(i).copied().unwrap_or(v >= 0.0) {
                    up_color
                } else {
                    down_color
                };
                fill_rect(window, origin, bar_x, bar_w, top, h, color);
            }
            // Zero baseline only when the range straddles zero (e.g. funding).
            // For volume (y_lo == 0) the baseline is the bottom edge; skip it to
            // keep the look identical.
            if y_lo < 0.0 {
                fill_rect(window, origin, 0.0, chart_w, zero_y, 1.0, grid);
            }
        }
        IndicatorOutput::Macd {
            macd,
            signal,
            histogram,
        } => {
            // Histogram first (behind lines). Sign drives color: positive bars
            // bullish-tinted, negative bars bearish-tinted, fully opaque in
            // pane mode; the macd/signal lines are drawn on top afterwards.
            let zero_y = band_y(y_lo, y_hi, 0.0, chart_top, chart_bottom);
            let up_color = Hsla { a: 1.0, ..bullish };
            let down_color = Hsla { a: 1.0, ..bearish };
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
            // Default to fully opaque bars in pane mode; a user-picked custom
            // color keeps whatever alpha they chose.
            let long_color = params.long_color.unwrap_or(Hsla { a: 1.0, ..bearish });
            let short_color = params.short_color.unwrap_or(Hsla { a: 1.0, ..bullish });
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
        IndicatorOutput::OpenInterest {
            open,
            high,
            low,
            close,
            price,
            params,
            unit,
        } => {
            use crate::indicators::OiRenderMode;
            // Contracts → active unit (USD = OI × candle close, precomputed
            // per bar in `price`).
            let conv = |raw: f64, i: usize| match unit {
                crate::persistence::VolumeUnit::Coin => raw,
                crate::persistence::VolumeUnit::Usd => {
                    raw * price.get(i).copied().flatten().unwrap_or(0.0)
                }
            };
            let up_color = params.up_color.unwrap_or(Hsla { a: 1.0, ..bullish });
            let down_color = params.down_color.unwrap_or(Hsla { a: 1.0, ..bearish });
            match params.render {
                OiRenderMode::Line => {
                    // Single-color close line off color slot 0 (same picker
                    // model as the CVD line). One accumulated path → one paint
                    // call regardless of bar count.
                    let line_color = item.color_at(0);
                    let mut pb = PathBuilder::stroke(px(1.5));
                    let mut prev: Option<Point<Pixels>> = None;
                    let lo = start_idx;
                    let hi = visible_end.min(close.len());
                    for i in lo..hi {
                        let Some(c) = close[i] else {
                            prev = None;
                            continue;
                        };
                        let v = conv(c, i);
                        let x = index_to_screen(
                            view_start, view_size, i as f32, canvas_w, y_axis_gap,
                        );
                        let y = band_y(y_lo, y_hi, v, chart_top, chart_bottom);
                        let p = point(px(x) + origin.x, px(y) + origin.y);
                        if let Some(p0) = prev {
                            pb.move_to(p0);
                            pb.line_to(p);
                        }
                        prev = Some(p);
                    }
                    if let Ok(path) = pb.build() {
                        window.paint_path(path, line_color);
                    }
                }
                OiRenderMode::Candles => {
                    for i in start_idx..visible_end.min(close.len()) {
                        let (Some(o), Some(h), Some(l), Some(c)) =
                            (open[i], high[i], low[i], close[i])
                        else {
                            continue;
                        };
                        let cx_px = index_to_screen(
                            view_start, view_size, i as f32, canvas_w, y_axis_gap,
                        );
                        if cx_px < -bar_w || cx_px > chart_w + bar_w {
                            continue;
                        }
                        let (ov, hv, lv, cv) =
                            (conv(o, i), conv(h, i), conv(l, i), conv(c, i));
                        let color = if cv >= ov { up_color } else { down_color };
                        let high_y = band_y(y_lo, y_hi, hv, chart_top, chart_bottom);
                        let low_y = band_y(y_lo, y_hi, lv, chart_top, chart_bottom);
                        let open_y = band_y(y_lo, y_hi, ov, chart_top, chart_bottom);
                        let close_y = band_y(y_lo, y_hi, cv, chart_top, chart_bottom);
                        // Wick.
                        let wick_top = high_y.min(low_y);
                        let wick_h = (high_y - low_y).abs().max(1.0);
                        fill_rect(window, origin, cx_px - 0.5, 1.0, wick_top, wick_h, color);
                        // Body.
                        let body_top = open_y.min(close_y);
                        let body_h = (open_y - close_y).abs().max(1.0);
                        fill_rect(
                            window,
                            origin,
                            cx_px - bar_w * 0.5,
                            bar_w,
                            body_top,
                            body_h,
                            color,
                        );
                    }
                }
            }
        }
        IndicatorOutput::Bands { .. }
        | IndicatorOutput::Lines(_)
        | IndicatorOutput::BarStat { .. }
        | IndicatorOutput::VolumeProfile { .. }
        | IndicatorOutput::Heatmap
        | IndicatorOutput::ObProfile => {
            // Bands (BB) and Lines (MA Suite) are overlay-only by kind
            // contract; no-op here for safety. BarStat is handled up
            // front (before the grid pass) since it owns its own layout.
            // VolumeProfile is overlay-only (VRVP is `OverlayOnly`); never
            // routes to a sub-pane. Heatmap + ObProfile (`OverlayOnly`) render
            // in the main pane via their own paint passes, never a sub-pane.
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

/// Which BarStat rows the paint pass should draw (mirrored from the
/// indicator's `show_*` flags via the output variant).
#[derive(Clone, Copy)]
struct BarStatShow {
    volume: bool,
    delta: bool,
    long_liq: bool,
    short_liq: bool,
    oi_delta: bool,
    net_ls: bool,
}

/// Pane geometry handed to [`paint_bar_stat_pane`] — the screen-space layout
/// shared by every row. Bundled so the function signature stays sane.
#[derive(Clone, Copy)]
struct BarStatGeom {
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
}

/// The per-bar series + their Daily-grade maxima, plus the variable-length
/// OB-imbalance rows (`ob_depths` carries the matching `OB_DEPTHS_PCT` indices,
/// parallel to `ob_imbalance`).
struct BarStatSeries<'a> {
    volume: &'a [Option<f64>],
    delta: &'a [Option<f64>],
    long_liq: &'a [Option<f64>],
    short_liq: &'a [Option<f64>],
    oi_delta: &'a [Option<f64>],
    net_ls: &'a [Option<f64>],
    ob_depths: &'a [usize],
    ob_imbalance: &'a [Vec<Option<f64>>],
    daily_max_ob: &'a [Vec<Option<f64>>],
    daily_max_vol: &'a [Option<f64>],
    daily_max_delta: &'a [Option<f64>],
    daily_max_long_liq: &'a [Option<f64>],
    daily_max_short_liq: &'a [Option<f64>],
    daily_max_oi_delta: &'a [Option<f64>],
    daily_max_net_ls: &'a [Option<f64>],
}

/// One row of the BarStat pane. `signed` = use bull/bear tint by data
/// sign (delta, net L/S, OB imbalance); otherwise the row uses its fixed
/// `base` color. `daily_max` carries the per-bar rolling-24h maxima for the
/// Daily grade. `header` is a short row tag painted in the right-edge gutter.
struct BarStatRow<'a> {
    values: &'a [Option<f64>],
    daily_max: Option<&'a [Option<f64>]>,
    base: Hsla,
    signed: bool,
    formatter: fn(f64) -> String,
    header: gpui::SharedString,
}

/// Paint the BarStat pane: one cell per visible bar, dynamic number of
/// stacked rows (count = `show.visible_row_count()`), optional heatmap
/// fill keyed by `grade`. Cell width follows the candle gap policy so
/// cells line up with their candles on the main pane.
#[allow(clippy::too_many_arguments)]
fn paint_bar_stat_pane(
    geom: BarStatGeom,
    grade: crate::indicators::BarStatGrade,
    show: BarStatShow,
    series: BarStatSeries<'_>,
    bullish: Hsla,
    bearish: Hsla,
    text_color: Hsla,
    origin: Point<Pixels>,
    window: &mut Window,
    cx: &mut App,
) {
    use crate::indicators::BarStatGrade;

    let BarStatGeom {
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
    } = geom;

    // Fixed blue base for the volume + OI-Δ rows — keeps them visually
    // distinct from the bull/bear-tinted delta cell so the eye can read each
    // row as a separate metric. Liq rows reuse the bull/bear theme colors
    // directly (long-liq = bearish, short-liq = bullish), so they share
    // visual vocabulary with the dedicated liq_bars indicator.
    let volume_base = gpui::hsla(0.61, 0.80, 0.55, 1.0);

    // Assemble the row list in fixed display order, skipping any row
    // whose show-flag is off.
    let mut rows: Vec<BarStatRow<'_>> = Vec::with_capacity(6 + series.ob_depths.len());
    if show.volume {
        rows.push(BarStatRow {
            values: series.volume,
            daily_max: Some(series.daily_max_vol),
            base: volume_base,
            signed: false,
            formatter: format_compact,
            header: "VOL".into(),
        });
    }
    if show.delta {
        rows.push(BarStatRow {
            values: series.delta,
            daily_max: Some(series.daily_max_delta),
            base: bullish, // overridden per cell when `signed`
            signed: true,
            formatter: format_signed_compact,
            header: "Δ".into(),
        });
    }
    if show.oi_delta {
        // Blue base like volume (not bull/bear tinted): OI Δ reads as its
        // own metric, with the rise/fall direction carried by the signed
        // text value rather than the cell color.
        rows.push(BarStatRow {
            values: series.oi_delta,
            daily_max: Some(series.daily_max_oi_delta),
            base: volume_base,
            signed: false,
            formatter: format_signed_compact,
            header: "OI Δ".into(),
        });
    }
    if show.net_ls {
        // Net positioning flow: bull/bear by sign like the delta row.
        rows.push(BarStatRow {
            values: series.net_ls,
            daily_max: Some(series.daily_max_net_ls),
            base: bullish,
            signed: true,
            formatter: format_signed_compact,
            header: "L/S".into(),
        });
    }
    // OB-imbalance rows: one per enabled depth preset. Signed (bid-heavy green /
    // ask-heavy red), graded like every other row via the active grade mode.
    for (k, &depth_idx) in series.ob_depths.iter().enumerate() {
        let Some(values) = series.ob_imbalance.get(k) else {
            continue;
        };
        let label = crate::indicators::ob_imbalance::ob_depth_label(
            crate::indicators::ob_imbalance::OB_DEPTHS_PCT
                .get(depth_idx)
                .copied()
                .unwrap_or(0.0),
        );
        rows.push(BarStatRow {
            values: values.as_slice(),
            daily_max: series.daily_max_ob.get(k).map(|s| s.as_slice()),
            base: bullish,
            signed: true,
            formatter: format_ratio,
            header: gpui::SharedString::from(format!("OBI {label}")),
        });
    }
    if show.long_liq {
        rows.push(BarStatRow {
            values: series.long_liq,
            daily_max: Some(series.daily_max_long_liq),
            base: bearish,
            signed: false,
            formatter: format_compact,
            header: "L LIQ".into(),
        });
    }
    if show.short_liq {
        rows.push(BarStatRow {
            values: series.short_liq,
            daily_max: Some(series.daily_max_short_liq),
            base: bullish,
            signed: false,
            formatter: format_compact,
            header: "S LIQ".into(),
        });
    }

    let n_rows = rows.len();
    if n_rows == 0 {
        return;
    }

    let pane_h = (chart_bottom - chart_top).max(1.0);
    let row_h = pane_h / n_rows as f32;
    // Cell fill width — fill the full slot so adjacent heatmap cells touch
    // (no gap stripe between bars in the pane). Text still centres in the
    // slot so it aligns with the candle above.
    let cell_w = slot_w.max(1.0);

    // VisibleRange mode normalises to the max absolute value across the
    // visible bar slice — precomputed per-row once so the per-bar loop is
    // a single division.
    let visible_max: Vec<f64> = if matches!(grade, BarStatGrade::VisibleRange) {
        rows.iter()
            .map(|r| slice_max_abs(r.values, start_idx, visible_end))
            .collect()
    } else {
        vec![0.0; n_rows]
    };

    // Auto-hide threshold for the per-cell text: when bars get narrower
    // than ~24px the K/M-formatted values stop being readable, so we drop
    // the label and just paint the heatmap fill. Keeps the pane useful
    // when zoomed all the way out.
    const TEXT_MIN_CELL_W: f32 = 24.0;
    let show_text = cell_w >= TEXT_MIN_CELL_W;

    // Longest visible series length — bounds the per-bar loop without
    // assuming any single row spans the full candle window.
    let max_len = rows.iter().map(|r| r.values.len()).max().unwrap_or(0);
    // Cull predicate shared by both paths: drop cells fully off the left edge,
    // and — critically — any cell whose right edge would cross into the
    // right-axis gutter where the per-row headers (VOL/Δ/…) live. That
    // rightmost cell is the most-recent bar that's only partially on the
    // chart; painting its column would draw a phantom stat past the last
    // fully-visible bar, overlapping the headers.
    let on_screen = |cx_px: f32| cx_px + cell_w * 0.5 >= 0.0 && cx_px + cell_w * 0.5 <= chart_w;

    if slot_w < 1.0 {
        // Zoomed out past one bar per pixel: drawing every cell would emit
        // thousands of overlapping 1px quads each frame (the pan/zoom FPS
        // sink). Collapse each screen column to a single cell per row, keeping
        // the max intensity so volume/liq spikes still register. Text is
        // already off at this width, so there's nothing else to draw per bar.
        let mut pend_col = vec![i32::MIN; n_rows];
        let mut pend_intensity = vec![0.0_f32; n_rows];
        let mut pend_base = vec![bullish; n_rows];
        let flush = |window: &mut Window, row_idx: usize, col: i32, intensity: f32, base: Hsla| {
            if col != i32::MIN && intensity > 0.0 {
                let y_top = chart_top + row_h * row_idx as f32;
                let color = grade_color(base, intensity);
                fill_rect(window, origin, col as f32, 1.0, y_top, row_h, color);
            }
        };
        for i in start_idx..visible_end.min(max_len) {
            let cx_px = index_to_screen(view_start, view_size, i as f32, canvas_w, y_axis_gap);
            if !on_screen(cx_px) {
                continue;
            }
            let col = cx_px.floor() as i32;
            for (row_idx, row) in rows.iter().enumerate() {
                let v = match row.values.get(i).copied().flatten() {
                    Some(v) => v,
                    None => continue,
                };
                let intensity =
                    bar_stat_intensity(grade, v, visible_max[row_idx], row.daily_max, i);
                let base = if row.signed {
                    if v >= 0.0 { bullish } else { bearish }
                } else {
                    row.base
                };
                if col != pend_col[row_idx] {
                    flush(window, row_idx, pend_col[row_idx], pend_intensity[row_idx], pend_base[row_idx]);
                    pend_col[row_idx] = col;
                    pend_intensity[row_idx] = intensity;
                    pend_base[row_idx] = base;
                } else if intensity > pend_intensity[row_idx] {
                    pend_intensity[row_idx] = intensity;
                    pend_base[row_idx] = base;
                }
            }
        }
        for row_idx in 0..n_rows {
            flush(window, row_idx, pend_col[row_idx], pend_intensity[row_idx], pend_base[row_idx]);
        }
    } else {
        for i in start_idx..visible_end.min(max_len) {
            let cx_px = index_to_screen(view_start, view_size, i as f32, canvas_w, y_axis_gap);
            let cell_x = cx_px - cell_w * 0.5;
            if !on_screen(cx_px) {
                continue;
            }

            for (row_idx, row) in rows.iter().enumerate() {
                let y_top = chart_top + row_h * row_idx as f32;
                // In-bounds cells with no data render as 0 (same as a real
                // zero); only skip positions past the end of this row's series.
                let v = match row.values.get(i) {
                    Some(cell) => cell.unwrap_or(0.0),
                    None => continue,
                };
                let intensity =
                    bar_stat_intensity(grade, v, visible_max[row_idx], row.daily_max, i);
                if intensity > 0.0 {
                    // Signed rows (delta) flip base by data sign; the
                    // disagreement between candle and cell tint is itself the
                    // signal, so this stays independent of the candle color.
                    let base = if row.signed {
                        if v >= 0.0 { bullish } else { bearish }
                    } else {
                        row.base
                    };
                    let color = grade_color(base, intensity);
                    fill_rect(window, origin, cell_x, cell_w, y_top, row_h, color);
                }
                if show_text {
                    paint_centred_text(
                        window,
                        cx,
                        origin,
                        cell_x,
                        cell_w,
                        y_top,
                        row_h,
                        text_color,
                        &(row.formatter)(v),
                    );
                }
            }
        }
    }

    // Per-row tag in the right-edge gutter. Painted last so it sits on
    // top of any cell fills that bled to the edge. Color follows the
    // row's base tint so VOL/L LIQ/S LIQ read at a glance — except the
    // delta row, which is sign-tinted per cell and has no fixed color,
    // so its header falls back to the neutral axis-label color.
    let gutter_w = y_axis_gap.max(0.0);
    if gutter_w > 4.0 {
        for (row_idx, row) in rows.iter().enumerate() {
            let y_top = chart_top + row_h * row_idx as f32;
            let header_color = if row.signed {
                text_color
            } else {
                Hsla {
                    a: 0.95,
                    ..row.base
                }
            };
            paint_centred_text(
                window,
                cx,
                origin,
                chart_w,
                gutter_w,
                y_top,
                row_h,
                header_color,
                row.header.as_ref(),
            );
        }
    }
}

/// Cell heatmap intensity (0..1) for one BarStat value under the active
/// grade. Factored out so the normal and the zoomed-out decimated paint
/// paths stay in lockstep.
#[inline]
fn bar_stat_intensity(
    grade: crate::indicators::BarStatGrade,
    v: f64,
    visible_max: f64,
    daily_max: Option<&[Option<f64>]>,
    i: usize,
) -> f32 {
    use crate::indicators::BarStatGrade;
    match grade {
        BarStatGrade::Off => 0.0,
        BarStatGrade::Bar => 1.0,
        BarStatGrade::VisibleRange => {
            if visible_max > 0.0 {
                (v.abs() / visible_max) as f32
            } else {
                0.0
            }
        }
        BarStatGrade::Daily => match daily_max.and_then(|s| s.get(i).copied().flatten()) {
            Some(mx) if mx > 0.0 => (v.abs() / mx) as f32,
            _ => 0.0,
        },
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
/// read consistently. Honours the same "round cell decimals" global flag as
/// the footprint cell formatter — when on, fractional digits are dropped
/// (K/M/B suffix preserved).
fn format_compact(v: f64) -> String {
    let abs = v.abs();
    if crate::prefs::round_cell_decimals() {
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

/// 2-decimal ratio formatter for the OB-imbalance rows (value ∈ [-1, +1]).
/// Negatives keep their `-`; positives are bare (`0.42` / `-0.30`).
fn format_ratio(v: f64) -> String {
    format!("{v:.2}")
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
