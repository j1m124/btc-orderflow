//! Drawings overlay: the committed-drawings + in-progress-preview + selection-
//! chrome canvas painted on top of the chart, plus the crosshair guides. Owns
//! the [`DrawingColors`] palette handed in by `chart.rs` at paint time. The
//! overlay is its own `canvas` element ([`render_drawings_overlay`]) layered
//! over the main chart canvas, so it can be content-masked independently.

use gpui::{
    App, BorderStyle, Bounds, ContentMask, Corners, Edges, Hsla, IntoElement, PaintQuad,
    ParentElement as _, PathBuilder, Pixels, SharedString, Styled as _, TextRun, Window, canvas,
    div, point, px,
};
use gpui_component::plot::AXIS_GAP;

use super::super::{Drawing, DrawingId, index_to_screen, price_to_screen};
use crate::services::market_data::Candle;

/// Colours fed to the drawings overlay so its paint closure doesn't need
/// access to `cx` at paint time.
#[derive(Clone, Copy)]
pub struct DrawingColors {
    pub line: Hsla,
    pub rect_fill: Hsla,
    pub rect_border: Hsla,
    pub ring: Hsla,
    pub background: Hsla,
    pub bullish: Hsla,
    pub bearish: Hsla,
    pub muted: Hsla,
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

/// Custom overlay that paints all committed drawings + the in-progress
/// preview + selection chrome on top of the candlestick canvas. Positioned
/// absolutely inside the chart-canvas div with the y-axis-label gutter and
/// x-axis-label band excluded so drawings don't paint over the axis area.
pub fn render_drawings_overlay(
    drawings: Vec<Drawing>,
    styles: std::collections::HashMap<DrawingId, super::super::drawings_view::DrawingStyle>,
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
    styles: &std::collections::HashMap<DrawingId, super::super::drawings_view::DrawingStyle>,
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
                                origin,
                                xmin,
                                frvp_w,
                                10.0,
                                h,
                                y_lo,
                                y_hi,
                                out,
                                params,
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

