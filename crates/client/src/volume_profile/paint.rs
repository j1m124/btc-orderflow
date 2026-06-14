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
    BorderStyle, Bounds, Corners, Edges, Hsla, PaintQuad, Pixels, Point, Window, point, px,
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

/// Alpha multiplier applied to per-bucket bar colours that fall OUTSIDE the
/// value area when the "highlight VA" toggle is on. The VA bars keep their
/// configured alpha; the rest dim down so the value-area pocket pops without
/// painting a backdrop tint. 0.30 picked empirically — low enough to read as
/// "muted" against most themes, high enough that the bucket geometry is
/// still legible.
const NON_VA_DIM_ALPHA: f32 = 0.30;

/// True iff `bucket` lies *within* the value-area band `[val, vah]` — i.e.
/// it is one of the buckets the VAH/VAL reference lines bracket.
///
/// This is a containment test, not an overlap test, and the distinction
/// matters: `vah`/`val` are bucket *edges* (`vah_price` = the highest VA
/// bucket's `price_high`, `val_price` = the lowest's `price_low`), and
/// contiguous buckets share edges. The bucket just above VAH has
/// `price_low == vah`, the bucket just below VAL has `price_high == val` —
/// an overlap test counts both as inside, so the highlight band ends up one
/// bucket taller on each side than the lines, which is the discrepancy this
/// fixes. `eps` (a sliver of a bucket) absorbs float-add error on the shared
/// edges so the genuine boundary buckets stay solidly inside.
#[inline]
fn bucket_in_va(b: &VpBucket, vah: f64, val: f64, eps: f64) -> bool {
    b.price_low >= val - eps && b.price_high <= vah + eps
}

/// Pre-resolved VA dimming context. `None` → no dimming this paint pass
/// (toggle off, coverage too low, or VA bounds not materialized). Carried
/// through to the per-bucket loop so the inner test is a single bounds
/// check rather than re-evaluating params + output every iteration.
#[derive(Clone, Copy)]
struct DimCtx {
    vah: f64,
    val: f64,
    /// Containment slack — a thousandth of a bucket, far below one bucket
    /// (so just-outside buckets stay excluded) yet far above f64 add error
    /// on the shared edges (so VA boundary buckets stay included).
    eps: f64,
}

fn dim_ctx(params: &VolumeProfileParams, output: &VolumeProfileOutput) -> Option<DimCtx> {
    // Note: only `show_va_highlight` gates the dimming. `show_va` controls
    // the VAH / VAL reference *lines* — turning those off should not also
    // turn off the bucket dim, which is a separate visual layer the user
    // may want even when the lines themselves are noise.
    if !params.show_va_highlight {
        return None;
    }
    if output.coverage_pct < COVERAGE_THRESHOLD {
        return None;
    }
    match (output.vah_price, output.val_price) {
        (Some(vah), Some(val)) => Some(DimCtx {
            vah,
            val,
            eps: params.bucket_dollars().max(f64::MIN_POSITIVE) * 1e-3,
        }),
        _ => None,
    }
}

/// Apply non-VA dimming to `base` when `dim` is set and the bucket sits
/// outside the value area. Otherwise returns `base` unchanged.
#[inline]
fn bucket_color(base: Hsla, b: &VpBucket, dim: Option<DimCtx>) -> Hsla {
    match dim {
        Some(d) if !bucket_in_va(b, d.vah, d.val, d.eps) => Hsla {
            a: base.a * NON_VA_DIM_ALPHA,
            ..base
        },
        _ => base,
    }
}

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
    origin: Point<Pixels>,
    chart_left: f32,
    chart_w: f32,
    chart_top: f32,
    chart_bottom: f32,
    y_lo: f64,
    y_hi: f64,
    output: &VolumeProfileOutput,
    params: &VolumeProfileParams,
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

    let render_levels = output.coverage_pct >= COVERAGE_THRESHOLD;
    // Non-VA dimming is the inverse of the old "highlight VA buckets" filled
    // band: instead of tinting the VA region, attenuate the bars outside it
    // so the value-area pocket stands out by *contrast*. Predicate is hoisted
    // once and reused inside the per-mode loops below.
    let dim = dim_ctx(params, output);

    match params.render_mode {
        VpRenderMode::Volume => paint_volume_mode(
            window, origin, anchor_x, band_w, params, output, &price_to_y, chart_top,
            chart_bottom, dim,
        ),
        VpRenderMode::Delta => paint_delta_mode(
            window, origin, anchor_x, band_w, params, output, &price_to_y, chart_top,
            chart_bottom, dim,
        ),
        VpRenderMode::VolDeltaOutline => paint_vol_delta_outline_mode(
            window, origin, anchor_x, band_w, params, output, &price_to_y, chart_top,
            chart_bottom, dim,
        ),
    }

    // Reference levels (POC, VAH, VAL) layer above the bars so they remain
    // visible regardless of how dense the profile is at that price. Suppressed
    // when coverage is below threshold — partial profiles would otherwise
    // surface levels that snap around as more history arrives.
    if render_levels {
        paint_reference_levels(window, origin, chart_left, chart_w, params, output, &price_to_y);
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
    dim: Option<DimCtx>,
) {
    let max_total = output
        .buckets
        .iter()
        .map(|b| b.total)
        .fold(0.0_f64, f64::max);
    if max_total <= 0.0 {
        return;
    }
    let base_color = params.color_volume.into_hsla();
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
        let color = bucket_color(base_color, b, dim);
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
    dim: Option<DimCtx>,
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
        let base = if b.delta > 0.0 {
            bull
        } else if b.delta < 0.0 {
            bear
        } else {
            neutral
        };
        let color = bucket_color(base, b, dim);
        fill_rect(window, origin, x, bar_w, y_top_c, h, color);
    }
}

/// Volume+Delta mode — the volume profile is drawn as a single connected
/// outline (a staircase silhouette tracing each bucket's volume extent),
/// with a filled bull/bear delta bar inside each row.
///
/// We deliberately do NOT stroke a full box per bucket: stacked rows share
/// edges, so a per-row box draws the same horizontal divider twice at every
/// bucket boundary, piling up into a dense ladder of lines across the whole
/// profile. Instead we paint only the leading vertical edge of each bar plus
/// a short horizontal *step* connector where two price-adjacent buckets
/// differ in width. The result reads as one profile outline rather than a
/// stack of boxes. Buckets aren't guaranteed contiguous (compute only emits
/// rows that had volume), so connectors are gated on adjacency — a gap leaves
/// the silhouette open, which is correct.
///
/// Inner delta length is ALWAYS per-row scaled (`|delta| / bucket.total`) so
/// the fill can't overflow the volume extent regardless of
/// `params.delta_scale`. This matches the design call from the grilling:
/// "Mode 3 should make the delta easy to read inside the volume frame".
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
    dim: Option<DimCtx>,
) {
    let max_total = output
        .buckets
        .iter()
        .map(|b| b.total)
        .fold(0.0_f64, f64::max);
    if max_total <= 0.0 {
        return;
    }
    let outline_base = params.color_volume.into_hsla();
    let bull = params.color_bull.into_hsla();
    let bear = params.color_bear.into_hsla();
    const STROKE_W: f32 = 1.0;
    // Two buckets count as price-adjacent (and so get a step connector) when
    // the upper one's low edge meets the lower one's high edge. Half a bucket
    // of slack cleanly separates "touching" (diff ≈ 0) from "one row gap"
    // (diff ≥ bucket size) without tripping on f64 add error.
    let adj_eps = 0.5 * params.bucket_dollars().max(f64::MIN_POSITIVE);

    // `prev` carries the lower-priced neighbour already painted this pass:
    // its leading-edge x, its high edge price, and its outline color (so the
    // connector blends with the bar it steps from).
    let mut prev: Option<(f32, f64, Hsla)> = None;
    for b in &output.buckets {
        let y_top = price_to_y(b.price_high);
        let y_bot = price_to_y(b.price_low);
        let y_top_c = y_top.max(chart_top);
        let y_bot_c = y_bot.min(chart_bottom);
        let h = (y_bot_c - y_top_c).max(0.0);
        if h <= 0.0 {
            // Off-screen row — break silhouette continuity so the next visible
            // bucket doesn't connect across the gap.
            prev = None;
            continue;
        }
        let outer_w = (band_w * (b.total / max_total) as f32).max(1.0);
        let lead_x = match params.anchor {
            AnchorEdge::Right => anchor_x - outer_w,
            AnchorEdge::Left => anchor_x + outer_w,
        };
        let outline = bucket_color(outline_base, b, dim);

        // Inner (delta) fill first so the outline paints on top of it.
        let inner_frac = delta_fraction(b, VpDeltaScale::PerRow, 0.0);
        if inner_frac.is_finite() && inner_frac > 0.0 {
            let inner_w = (outer_w * inner_frac as f32).max(0.5);
            let inner_x = match params.anchor {
                AnchorEdge::Right => anchor_x - inner_w,
                AnchorEdge::Left => anchor_x,
            };
            let inner_base = if b.delta > 0.0 { bull } else { bear };
            let inner_color = bucket_color(inner_base, b, dim);
            fill_rect(window, origin, inner_x, inner_w, y_top_c, h, inner_color);
        }

        // Leading vertical edge of this bar.
        paint_v_line(window, origin, lead_x, y_top_c, h, outline, STROKE_W);

        // Step connector to the lower-priced neighbour, only when the two are
        // truly price-adjacent and the shared boundary is on-screen.
        if let Some((prev_lead_x, prev_high, prev_outline)) = prev {
            if (prev_high - b.price_low).abs() <= adj_eps {
                let boundary_y = y_bot; // unclamped shared edge (this row's low)
                if boundary_y >= chart_top && boundary_y <= chart_bottom {
                    paint_h_segment(
                        window, origin, prev_lead_x, lead_x, boundary_y, prev_outline, STROKE_W,
                    );
                }
            }
        }
        prev = Some((lead_x, b.price_high, outline));
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

/// Paint POC / VAH / VAL reference lines. Honors the per-flag show toggles.
/// Coverage gating is the caller's responsibility (we don't re-check
/// `coverage_pct` here).
fn paint_reference_levels(
    window: &mut Window,
    origin: Point<Pixels>,
    chart_left: f32,
    chart_w: f32,
    params: &VolumeProfileParams,
    output: &VolumeProfileOutput,
    price_to_y: &dyn Fn(f64) -> f32,
) {
    let poc_color = params.color_poc.into_hsla();
    let va_color = params.color_va.into_hsla();

    if params.show_poc {
        if let Some(p) = output.poc_price {
            paint_h_line(window, origin, chart_left, chart_w, price_to_y(p), poc_color, 1.5);
        }
    }
    if params.show_va {
        if let Some(p) = output.vah_price {
            paint_h_line(window, origin, chart_left, chart_w, price_to_y(p), va_color, 1.0);
        }
        if let Some(p) = output.val_price {
            paint_h_line(window, origin, chart_left, chart_w, price_to_y(p), va_color, 1.0);
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


/// Vertical line at canvas x `x`, spanning `y_top..y_top + h`. Painted as a
/// thin filled rect (same approach as [`paint_h_line`]) and centered on the
/// integer pixel column so it stays anti-alias-clean. Used for the leading
/// edge of each bar in the VolDeltaOutline silhouette.
fn paint_v_line(
    window: &mut Window,
    origin: Point<Pixels>,
    x: f32,
    y_top: f32,
    h: f32,
    color: Hsla,
    thickness: f32,
) {
    let x_left = (x - thickness * 0.5).round();
    fill_rect(window, origin, x_left, thickness, y_top, h, color);
}

/// Horizontal segment between canvas x `x0` and `x1` at canvas y `y` — the
/// step connector joining two adjacent bars' leading edges. Order-agnostic
/// in x; centered on the integer pixel row.
fn paint_h_segment(
    window: &mut Window,
    origin: Point<Pixels>,
    x0: f32,
    x1: f32,
    y: f32,
    color: Hsla,
    thickness: f32,
) {
    let x_lo = x0.min(x1);
    let w = (x0 - x1).abs() + thickness; // +thickness so the corner squares off
    let y_top = (y - thickness * 0.5).round();
    fill_rect(window, origin, x_lo - thickness * 0.5, w, y_top, thickness, color);
}
