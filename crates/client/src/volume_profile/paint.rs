//! Shared rendering for VRVP overlay + FRVP drawing paint passes.
//!
//! Phase 7 shipped Volume mode; Phase 8 adds Delta + Volume+Delta-outline
//! (the third "render mode" the design grilling settled on). Phase 9
//! layers POC/VAH/VAL reference levels on top.
//!
//! The painter takes the chart's already-resolved price-pane geometry
//! (`origin`, `chart_left`, `chart_w`, `chart_top`, `chart_bottom`,
//! `y_lo`, `y_hi`) so VRVP and FRVP can share one implementation —
//! VRVP passes the candle pane's geometry, FRVP (Phase 12) passes the
//! drawing-rect's geometry instead.

use gpui::{
    App, BorderStyle, Bounds, Corners, Edges, Hsla, PaintQuad, Pixels, Point, SharedString,
    TextRun, Window, point, px,
};

use super::output::{VolumeProfileOutput, VpBucket};
use super::params::{AnchorEdge, VolumeProfileParams, VpDeltaScale, VpRenderMode};

/// Coverage threshold below which POC/VAH/VAL are suppressed. Partial
/// profiles (still loading history) would otherwise show misleading
/// reference levels that snap around as cells arrive. 0.95 chosen during
/// the design grilling — high enough that the level only renders when the
/// aggregate is meaningful, low enough that the user isn't kept waiting on
/// the tail-end of the snapshot.
const COVERAGE_THRESHOLD: f32 = 0.95;

/// Paint the volume profile inside the price pane spanning
/// `chart_top..chart_bottom` vertically and `chart_left..chart_left + chart_w`
/// horizontally. All inputs are canvas-relative `f32`s (the same convention
/// the chart's other paint helpers use).
///
/// Phase 7 only handles the `Volume` render mode; the other modes fall
/// through to a no-op until Phase 8.
#[allow(clippy::too_many_arguments)]
pub fn paint_volume_profile(
    window: &mut Window,
    cx: &mut App,
    origin: Point<Pixels>,
    chart_left: f32,
    chart_w: f32,
    chart_top: f32,
    chart_bottom: f32,
    y_lo: f64,
    y_hi: f64,
    output: &VolumeProfileOutput,
    params: &VolumeProfileParams,
    label_color: Hsla,
) {
    if output.buckets.is_empty() {
        return;
    }
    let pane_h = (chart_bottom - chart_top).max(1.0);
    if pane_h <= 0.0 || chart_w <= 0.0 {
        return;
    }
    let band_w = (chart_w * params.width_pct as f32 / 100.0).max(1.0);
    let anchor_x = match params.anchor {
        AnchorEdge::Right => chart_left + chart_w,
        AnchorEdge::Left => chart_left,
    };
    let price_to_y = |p: f64| -> f32 {
        let range = y_hi - y_lo;
        if range.abs() < 1e-9 {
            return 0.5 * (chart_top + chart_bottom);
        }
        let t = ((y_hi - p) / range) as f32;
        chart_top + t * pane_h
    };

    // VA highlight goes first so the per-bucket bars (which follow) paint
    // on top of the highlight band — preserves the cleaner bar outline.
    let render_levels = output.coverage_pct >= COVERAGE_THRESHOLD;
    if render_levels && params.show_va_highlight && params.show_va {
        paint_va_highlight(
            window, origin, chart_left, chart_w, params, output, &price_to_y, chart_top,
            chart_bottom,
        );
    }

    match params.render_mode {
        VpRenderMode::Volume => paint_volume_mode(
            window, origin, anchor_x, band_w, params, output, &price_to_y, chart_top,
            chart_bottom,
        ),
        VpRenderMode::Delta => paint_delta_mode(
            window, origin, anchor_x, band_w, params, output, &price_to_y, chart_top,
            chart_bottom,
        ),
        VpRenderMode::VolDeltaOutline => paint_vol_delta_outline_mode(
            window, origin, anchor_x, band_w, params, output, &price_to_y, chart_top,
            chart_bottom,
        ),
    }

    // Reference levels (POC, VAH, VAL) layer above the bars so they remain
    // visible regardless of how dense the profile is at that price. Suppressed
    // when coverage is below threshold — partial profiles would otherwise
    // surface levels that snap around as more history arrives.
    if render_levels {
        paint_reference_levels(
            window, cx, origin, chart_left, chart_w, params, output, &price_to_y, label_color,
        );
    }
}

/// Volume mode — one filled rectangle per bucket; length proportional to
/// `bucket.total / max_total`. Color is `params.color_volume`. Bars whose
/// pane y is outside the visible band are skipped (cheap clamp; the
/// gpui paint sink doesn't reward off-screen quads).
fn paint_volume_mode(
    window: &mut Window,
    origin: Point<Pixels>,
    anchor_x: f32,
    band_w: f32,
    params: &VolumeProfileParams,
    output: &VolumeProfileOutput,
    price_to_y: &dyn Fn(f64) -> f32,
    chart_top: f32,
    chart_bottom: f32,
) {
    let max_total = output
        .buckets
        .iter()
        .map(|b| b.total)
        .fold(0.0_f64, f64::max);
    if max_total <= 0.0 {
        return;
    }
    let color = params.color_volume.into_hsla();
    for b in &output.buckets {
        // Buckets are sorted ascending by `price_low`; the *top* of a row
        // in screen space corresponds to its *high* edge.
        let y_top = price_to_y(b.price_high);
        let y_bot = price_to_y(b.price_low);
        // Clamp to the pane so a row that straddles the visible band
        // still paints its visible slice (and a fully off-screen row is
        // a no-op via the height check).
        let y_top_c = y_top.max(chart_top);
        let y_bot_c = y_bot.min(chart_bottom);
        let h = (y_bot_c - y_top_c).max(0.0);
        if h <= 0.0 {
            continue;
        }
        let bar_w = (band_w * (b.total / max_total) as f32).max(0.5);
        let x = match params.anchor {
            AnchorEdge::Right => anchor_x - bar_w,
            AnchorEdge::Left => anchor_x,
        };
        fill_rect(window, origin, x, bar_w, y_top_c, h, color);
    }
}

/// Delta mode — one filled rectangle per bucket; length proportional to
/// `|delta|`, color picked by the sign of delta (bull / bear). The scaling
/// denominator depends on `params.delta_scale`:
/// - `PerRow`: divides by `bucket.total`, so a thin row with strongly
///   one-sided aggression draws full-width.
/// - `WholeProfile`: divides by `max |delta|` across all buckets, so the
///   bar lengths represent absolute aggression rather than per-row purity.
fn paint_delta_mode(
    window: &mut Window,
    origin: Point<Pixels>,
    anchor_x: f32,
    band_w: f32,
    params: &VolumeProfileParams,
    output: &VolumeProfileOutput,
    price_to_y: &dyn Fn(f64) -> f32,
    chart_top: f32,
    chart_bottom: f32,
) {
    // `WholeProfile` needs a global denominator — compute once.
    let global_max_abs_delta = output
        .buckets
        .iter()
        .map(|b| b.delta.abs())
        .fold(0.0_f64, f64::max);
    if matches!(params.delta_scale, VpDeltaScale::WholeProfile) && global_max_abs_delta <= 0.0 {
        return;
    }
    let bull = params.color_bull.into_hsla();
    let bear = params.color_bear.into_hsla();
    let neutral = params.color_volume.into_hsla();
    for b in &output.buckets {
        let y_top = price_to_y(b.price_high);
        let y_bot = price_to_y(b.price_low);
        let y_top_c = y_top.max(chart_top);
        let y_bot_c = y_bot.min(chart_bottom);
        let h = (y_bot_c - y_top_c).max(0.0);
        if h <= 0.0 {
            continue;
        }
        let frac = delta_fraction(b, params.delta_scale, global_max_abs_delta);
        if !frac.is_finite() || frac <= 0.0 {
            continue;
        }
        let bar_w = (band_w * frac as f32).max(0.5);
        let x = match params.anchor {
            AnchorEdge::Right => anchor_x - bar_w,
            AnchorEdge::Left => anchor_x,
        };
        let color = if b.delta > 0.0 {
            bull
        } else if b.delta < 0.0 {
            bear
        } else {
            neutral
        };
        fill_rect(window, origin, x, bar_w, y_top_c, h, color);
    }
}

/// Volume+Delta-outline mode — outlined volume bar (stroke only) with an
/// inner filled delta bar. The outline carries `color_volume`; the inner
/// fill picks bull/bear by delta sign. Inner length is ALWAYS per-row
/// scaled (`|delta| / bucket.total`) so the inner bar can't overflow the
/// outline frame regardless of `params.delta_scale`. This matches the
/// design call from the grilling: "Mode 3 should make the delta easy to
/// read inside the volume frame".
fn paint_vol_delta_outline_mode(
    window: &mut Window,
    origin: Point<Pixels>,
    anchor_x: f32,
    band_w: f32,
    params: &VolumeProfileParams,
    output: &VolumeProfileOutput,
    price_to_y: &dyn Fn(f64) -> f32,
    chart_top: f32,
    chart_bottom: f32,
) {
    let max_total = output
        .buckets
        .iter()
        .map(|b| b.total)
        .fold(0.0_f64, f64::max);
    if max_total <= 0.0 {
        return;
    }
    let outline = params.color_volume.into_hsla();
    let bull = params.color_bull.into_hsla();
    let bear = params.color_bear.into_hsla();
    for b in &output.buckets {
        let y_top = price_to_y(b.price_high);
        let y_bot = price_to_y(b.price_low);
        let y_top_c = y_top.max(chart_top);
        let y_bot_c = y_bot.min(chart_bottom);
        let h = (y_bot_c - y_top_c).max(0.0);
        if h <= 0.0 {
            continue;
        }
        // Outer (volume) bar.
        let outer_w = (band_w * (b.total / max_total) as f32).max(1.0);
        let outer_x = match params.anchor {
            AnchorEdge::Right => anchor_x - outer_w,
            AnchorEdge::Left => anchor_x,
        };
        stroke_rect(window, origin, outer_x, outer_w, y_top_c, h, outline, 1.0);
        // Inner (delta) bar — anchored to the same edge as the outer; per-row
        // scaling so `inner_w <= outer_w` always.
        let inner_frac = delta_fraction(b, VpDeltaScale::PerRow, 0.0);
        if !inner_frac.is_finite() || inner_frac <= 0.0 {
            continue;
        }
        let inner_w = (outer_w * inner_frac as f32).max(0.5);
        let inner_x = match params.anchor {
            AnchorEdge::Right => anchor_x - inner_w,
            AnchorEdge::Left => anchor_x,
        };
        let inner_color = if b.delta > 0.0 { bull } else { bear };
        fill_rect(window, origin, inner_x, inner_w, y_top_c, h, inner_color);
    }
}

/// Common delta scaler — returns the per-bucket fraction of `band_w` the
/// bar should occupy. Caller multiplies by band width. Returns `0.0` when
/// the denominator is zero (degenerate input).
fn delta_fraction(b: &VpBucket, scale: VpDeltaScale, global_max_abs_delta: f64) -> f64 {
    let abs_delta = b.delta.abs();
    match scale {
        VpDeltaScale::PerRow => {
            if b.total <= 0.0 {
                0.0
            } else {
                (abs_delta / b.total).min(1.0)
            }
        }
        VpDeltaScale::WholeProfile => {
            if global_max_abs_delta <= 0.0 {
                0.0
            } else {
                (abs_delta / global_max_abs_delta).min(1.0)
            }
        }
    }
}

/// Helper exposed so both modes / both consumers paint via the same quad
/// primitive. Same shape as the equivalent helper in
/// `crate::panels::chart::paint` — duplicated here to keep this module
/// self-contained (no upward dependency on chart-paint internals).
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

/// Paint the value-area highlight band — a low-alpha tint covering the
/// full pane width from VAL to VAH. Sits *below* the per-bucket bars so
/// the bars remain crisp. No-op if VA bounds aren't materialized yet.
fn paint_va_highlight(
    window: &mut Window,
    origin: Point<Pixels>,
    chart_left: f32,
    chart_w: f32,
    params: &VolumeProfileParams,
    output: &VolumeProfileOutput,
    price_to_y: &dyn Fn(f64) -> f32,
    chart_top: f32,
    chart_bottom: f32,
) {
    let (Some(vah), Some(val)) = (output.vah_price, output.val_price) else {
        return;
    };
    // y_top = price_to_y(vah), y_bot = price_to_y(val) — VAH is the upper
    // price so it maps to smaller y; VAL to larger y.
    let y_top = price_to_y(vah).max(chart_top);
    let y_bot = price_to_y(val).min(chart_bottom);
    let h = (y_bot - y_top).max(0.0);
    if h <= 0.0 {
        return;
    }
    // Soft tint — the configured `color_va` (alpha 0.6 by default) gets
    // further attenuated so the highlight reads as a backdrop, not a fill.
    let base = params.color_va.into_hsla();
    let tint = Hsla {
        a: base.a * 0.20,
        ..base
    };
    fill_rect(window, origin, chart_left, chart_w, y_top, h, tint);
}

/// Paint POC / VAH / VAL reference lines + optional right-edge labels.
/// Honors the per-flag show toggles. Coverage gating is the caller's
/// responsibility (we don't re-check `coverage_pct` here).
#[allow(clippy::too_many_arguments)]
fn paint_reference_levels(
    window: &mut Window,
    cx: &mut App,
    origin: Point<Pixels>,
    chart_left: f32,
    chart_w: f32,
    params: &VolumeProfileParams,
    output: &VolumeProfileOutput,
    price_to_y: &dyn Fn(f64) -> f32,
    label_color: Hsla,
) {
    let poc_color = params.color_poc.into_hsla();
    let va_color = params.color_va.into_hsla();

    if params.show_poc {
        if let Some(p) = output.poc_price {
            paint_h_line(window, origin, chart_left, chart_w, price_to_y(p), poc_color, 1.5);
            if params.show_labels {
                paint_level_label(
                    window,
                    cx,
                    origin,
                    chart_left,
                    chart_w,
                    price_to_y(p),
                    "POC",
                    p,
                    poc_color,
                    label_color,
                );
            }
        }
    }
    if params.show_va {
        if let Some(p) = output.vah_price {
            paint_h_line(window, origin, chart_left, chart_w, price_to_y(p), va_color, 1.0);
            if params.show_labels {
                paint_level_label(
                    window,
                    cx,
                    origin,
                    chart_left,
                    chart_w,
                    price_to_y(p),
                    "VAH",
                    p,
                    va_color,
                    label_color,
                );
            }
        }
        if let Some(p) = output.val_price {
            paint_h_line(window, origin, chart_left, chart_w, price_to_y(p), va_color, 1.0);
            if params.show_labels {
                paint_level_label(
                    window,
                    cx,
                    origin,
                    chart_left,
                    chart_w,
                    price_to_y(p),
                    "VAL",
                    p,
                    va_color,
                    label_color,
                );
            }
        }
    }
}

/// Horizontal full-width line at canvas y `y`, painted as a thin filled
/// rect (`PaintQuad`) rather than a stroked path so it stays anti-alias-
/// clean even at sub-pixel `y` positions. Centered on the integer pixel.
fn paint_h_line(
    window: &mut Window,
    origin: Point<Pixels>,
    chart_left: f32,
    chart_w: f32,
    y: f32,
    color: Hsla,
    thickness: f32,
) {
    let y_top = (y - thickness * 0.5).round();
    fill_rect(window, origin, chart_left, chart_w, y_top, thickness, color);
}

/// "POC 1234.50"-style label rendered at the right edge of the chart band,
/// just above the line. Uses the configured `label_color` for the prefix
/// (matches the chart's axis labels) and the level's color for the price
/// — keeps the price visually anchored to its line even when the user
/// retints the level.
#[allow(clippy::too_many_arguments)]
fn paint_level_label(
    window: &mut Window,
    cx: &mut App,
    origin: Point<Pixels>,
    chart_left: f32,
    chart_w: f32,
    y: f32,
    name: &str,
    price: f64,
    name_color: Hsla,
    _label_color: Hsla,
) {
    let text = SharedString::from(format!("{} {:.2}", name, price));
    let run = TextRun {
        len: text.len(),
        font: window.text_style().font(),
        color: name_color,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let shaped = window
        .text_system()
        .shape_line(text, px(10.0), &[run], None);
    // Right-aligned with a small inset from the y-axis gutter; baseline
    // sits a few px above the line so it doesn't cover its own underline.
    let approx_w_px = 60.0_f32; // small over-estimate; gpui will clip if needed
    let x = chart_left + chart_w - approx_w_px - 4.0;
    let y_label = y - 12.0;
    let _ = shaped.paint(
        point(px(x) + origin.x, px(y_label) + origin.y),
        px(10.0),
        gpui::TextAlign::Left,
        None,
        window,
        cx,
    );
}

/// Stroke-only rectangle (outline) — same quad primitive with a non-zero
/// border and a transparent background. Used for the outer volume frame
/// in VolDeltaOutline mode.
#[inline]
fn stroke_rect(
    window: &mut Window,
    origin: Point<Pixels>,
    x: f32,
    w: f32,
    y_top: f32,
    h: f32,
    color: Hsla,
    stroke_w: f32,
) {
    window.paint_quad(PaintQuad {
        bounds: Bounds {
            origin: point(px(x) + origin.x, px(y_top) + origin.y),
            size: gpui::size(px(w), px(h)),
        },
        corner_radii: Corners::default(),
        background: gpui::transparent_black().into(),
        border_widths: Edges::all(px(stroke_w)),
        border_color: color,
        border_style: BorderStyle::Solid,
    });
}
