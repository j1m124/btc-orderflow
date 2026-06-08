//! Shared rendering for VRVP overlay + FRVP drawing paint passes.
//!
//! Phase 7 ships Volume mode (single filled bar per bucket sized by
//! `total`). Phases 8/9 add Delta + Volume+Delta-outline modes and
//! POC/VAH/VAL reference lines respectively.
//!
//! The painter takes the chart's already-resolved price-pane geometry
//! (`origin`, `chart_left`, `chart_w`, `chart_top`, `chart_bottom`,
//! `y_lo`, `y_hi`) so VRVP and FRVP can share one implementation —
//! VRVP passes the candle pane's geometry, FRVP (Phase 12) passes the
//! drawing-rect's geometry instead.

use gpui::{
    BorderStyle, Bounds, Corners, Edges, Hsla, PaintQuad, Pixels, Point, Window, point, px,
};

use super::output::VolumeProfileOutput;
use super::params::{AnchorEdge, VolumeProfileParams, VpRenderMode};

/// Paint the volume profile inside the price pane spanning
/// `chart_top..chart_bottom` vertically and `chart_left..chart_left + chart_w`
/// horizontally. All inputs are canvas-relative `f32`s (the same convention
/// the chart's other paint helpers use).
///
/// Phase 7 only handles the `Volume` render mode; the other modes fall
/// through to a no-op until Phase 8.
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

    match params.render_mode {
        VpRenderMode::Volume => paint_volume_mode(
            window, origin, anchor_x, band_w, params, output, &price_to_y, chart_top,
            chart_bottom,
        ),
        // Delta / VolDeltaOutline land in Phase 8. Until then the paint
        // pass renders nothing for those modes; the user still sees the
        // VRVP entry in the indicator chip strip so the bug surfaces
        // immediately if anyone forgets to fill them in.
        VpRenderMode::Delta | VpRenderMode::VolDeltaOutline => {}
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
