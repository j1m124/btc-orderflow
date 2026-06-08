//! Bridge between the chart's view-coord drawing logic (fractional candle
//! index + price) and the workspace-wide [`DrawingService`] (absolute ms +
//! price).
//!
//! Charts paint/hit-test in `(fractional_index, price)` because that's the
//! coordinate system the existing paint pipeline + hit-test were built in. The
//! service stores anchors as `i64` epoch ms so drawings stay attached across
//! timeframes, symbols, and backfills. The conversion happens at the chart
//! boundary: snapshot service shapes into view-coords at render start; convert
//! view-coords back to ms when the chart commits a create or edit.

use gpui::Hsla;

use crate::drawings::shapes::{Drawing as ServiceDrawing, DrawingShape};
use crate::services::market_data::Candle;

use super::Drawing as ViewDrawing;

/// Per-drawing visual style snapshot. Lives parallel to the view-coord
/// `Drawing` list because [`ViewDrawing`] is geometry-only — adding style
/// fields to every variant would have rippled through ~140 destructuring
/// sites. The chart's render path builds a `HashMap<DrawingId, DrawingStyle>`
/// in the same scope it builds the drawings snapshot, and paint reads it
/// per drawing.
///
/// All fields are optional / defaulted; paint applies them as overrides
/// over the theme defaults, so absent values reproduce the pre-Phase-7
/// visual exactly.
#[derive(Clone, Debug, Default)]
pub struct DrawingStyle {
    /// Primary stroke / fill color override. `None` → keep theme default.
    /// For position shapes this is unused (they consume `profit_color` /
    /// `loss_color` instead).
    pub color: Option<Hsla>,
    /// TP-zone fill + line override for Long / Short.
    pub profit_color: Option<Hsla>,
    /// SL-zone fill + line override for Long / Short.
    pub loss_color: Option<Hsla>,
    /// Stroke width in pixels. `0.0` means "use the paint default" (so
    /// in-flight create previews that don't have a style record still
    /// paint with their original 1.5px).
    pub width: f32,
    /// Optional secondary label to render at the drawing's top-right
    /// corner. `HorizontalRay` reads its own `text` field — paint does
    /// not consume `label` for that variant (the ray's existing label
    /// pipeline owns it).
    pub label: Option<String>,
}

/// Project the per-shape style fields into the parallel style record.
/// Pre-Phase-1 blobs (and `Text`, which never had a stored width) map
/// through unchanged — `width: 0.0` sentinel tells paint to keep its
/// existing 1.5px floor.
pub fn style_from_shape(shape: &DrawingShape) -> DrawingStyle {
    use DrawingShape::*;
    let mut s = DrawingStyle::default();
    match shape {
        Line(d) | Rect(d) | Arrow(d) | Fibonacci(d) => {
            s.color = d.color.map(|c| c.into_hsla());
            s.width = d.width;
            s.label = d.label.clone();
        }
        HorizontalRay(d) => {
            s.color = d.color.map(|c| c.into_hsla());
            s.width = d.width;
            // `text` is the ray's own label; the paint pipeline reads it
            // directly from the ViewDrawing variant, not from this style
            // record. Leaving `s.label = None` keeps the generic top-right
            // label painter from double-drawing it.
        }
        AnchoredVwap(d) => {
            s.color = d.color.map(|c| c.into_hsla());
            s.width = d.width;
            s.label = d.label.clone();
        }
        Text(d) => {
            s.color = d.color.map(|c| c.into_hsla());
        }
        Long(p) | Short(p) => {
            s.profit_color = p.profit_color.map(|c| c.into_hsla());
            s.loss_color = p.loss_color.map(|c| c.into_hsla());
            s.width = p.width;
            s.label = p.label.clone();
        }
    }
    s
}

/// Spacing in ms between bar `i` and bar `i+1`, falling back to
/// `bar_duration_ms` at the edges of the loaded range. We use the actual
/// neighbour gap rather than the TF's nominal duration so the round-trip
/// `idx ↔ ms` stays exact even when the data's bar interval doesn't match
/// the chart's TF (which happens for the embedded chart_data — always
/// 1h-spaced — when viewed at any TF other than H1).
fn span_ms_at(i: usize, candles: &[Candle], bar_duration_ms: i64) -> i64 {
    if i + 1 < candles.len() {
        let s = candles[i + 1].open_time - candles[i].open_time;
        if s > 0 {
            return s;
        }
    }
    bar_duration_ms.max(1)
}

/// Map an absolute wall-clock ms `t` onto the chart's fractional candle index.
///
/// Gaps are NOT collapsed here in the traditional sense — the chart already
/// paints bars at integer slots regardless of wall-clock spacing, so the
/// natural representation of "halfway between bar i and bar i+1" is
/// `i + 0.5`, and we treat the time as proportional to the actual gap. Times
/// past the loaded range extrapolate linearly using the spacing of the edge
/// bar (so an off-screen drawing has a coherent x for pan-to-find).
pub fn time_to_idx(t: i64, candles: &[Candle], bar_duration_ms: i64) -> f32 {
    if candles.is_empty() {
        return 0.0;
    }
    let first = candles.first().unwrap().open_time;
    let last = candles.last().unwrap().open_time;
    if t <= first {
        // Pre-history: extrapolate using the leading edge's spacing.
        let span = span_ms_at(0, candles, bar_duration_ms);
        return (t - first) as f32 / span as f32;
    }
    if t >= last {
        // Past the loaded tail: extrapolate using the trailing edge's
        // spacing. (For a one-bar buffer the only span available is the
        // nominal TF duration; `span_ms_at` falls back to it.)
        let last_i = candles.len() - 1;
        let tail_span = if last_i > 0 {
            span_ms_at(last_i - 1, candles, bar_duration_ms)
        } else {
            bar_duration_ms.max(1)
        };
        return last_i as f32 + (t - last) as f32 / tail_span as f32;
    }
    // Binary search for the bar containing `t`. `partition_point` returns the
    // first index where `open_time > t`; subtract one for the bar at/before
    // `t`. The guards above ensure `pp >= 1`.
    let pp = candles.partition_point(|c| c.open_time <= t);
    let i = pp.saturating_sub(1);
    let bar_open = candles[i].open_time;
    let span = span_ms_at(i, candles, bar_duration_ms);
    let frac = (t - bar_open) as f32 / span as f32;
    // Clamp to [0, 1) so floating-point at the boundary doesn't spill into
    // the next bar's slot — the next-bar case is already handled above by
    // the `t >= last` branch when relevant.
    i as f32 + frac.clamp(0.0, 0.999_999)
}

/// Inverse of [`time_to_idx`]. Used when the chart commits a create or
/// edit-drag: the in-progress drawing lives in fractional index coords, but
/// the service needs absolute ms.
pub fn idx_to_time(idx: f32, candles: &[Candle], bar_duration_ms: i64) -> i64 {
    if candles.is_empty() {
        return 0;
    }
    let len = candles.len() as i32;
    let floor_i = idx.floor() as i32;
    let frac = idx - floor_i as f32;
    if floor_i < 0 {
        // Extrapolate backwards from the first bar using its leading spacing.
        let first = candles[0].open_time;
        let span = span_ms_at(0, candles, bar_duration_ms);
        first + (idx * span as f32) as i64
    } else if floor_i >= len {
        // Extrapolate forwards from the last bar using its trailing spacing.
        let last_i = (len - 1) as usize;
        let last = candles[last_i].open_time;
        let span = if last_i > 0 {
            span_ms_at(last_i - 1, candles, bar_duration_ms)
        } else {
            bar_duration_ms.max(1)
        };
        last + ((idx - last_i as f32) * span as f32) as i64
    } else {
        let i = floor_i as usize;
        let bar_open = candles[i].open_time;
        if frac.abs() < f32::EPSILON {
            return bar_open;
        }
        let span = span_ms_at(i, candles, bar_duration_ms);
        bar_open + (frac as f64 * span as f64).round() as i64
    }
}

/// Project a service drawing into the chart's view-coord representation. Times
/// run through [`time_to_idx`] so the result is renderable by the existing
/// paint pipeline without further translation.
pub fn shape_to_view(
    service: &ServiceDrawing,
    candles: &[Candle],
    bar_duration_ms: i64,
) -> ViewDrawing {
    let t2i = |t: i64| time_to_idx(t, candles, bar_duration_ms);
    match &service.shape {
        DrawingShape::Line(s) => ViewDrawing::Line {
            id: service.id,
            a: (t2i(s.a_time), s.a_price),
            b: (t2i(s.b_time), s.b_price),
        },
        DrawingShape::Arrow(s) => ViewDrawing::Arrow {
            id: service.id,
            a: (t2i(s.a_time), s.a_price),
            b: (t2i(s.b_time), s.b_price),
        },
        DrawingShape::Fibonacci(s) => ViewDrawing::Fibonacci {
            id: service.id,
            a: (t2i(s.a_time), s.a_price),
            b: (t2i(s.b_time), s.b_price),
        },
        DrawingShape::Rect(s) => ViewDrawing::Rect {
            id: service.id,
            a: (t2i(s.a_time), s.a_price),
            b: (t2i(s.b_time), s.b_price),
        },
        DrawingShape::HorizontalRay(r) => ViewDrawing::HorizontalRay {
            id: service.id,
            anchor: (t2i(r.anchor_time), r.anchor_price),
            text: r.text.clone(),
            extend_left: r.extend_left,
        },
        DrawingShape::Text(s) => ViewDrawing::Text {
            id: service.id,
            anchor: (t2i(s.anchor_time), s.anchor_price),
            width: s.width,
            text: s.text.clone(),
            font_size: s.font_size,
        },
        DrawingShape::Long(p) => ViewDrawing::Long {
            id: service.id,
            t0: t2i(p.t0),
            t1: t2i(p.t1),
            entry: p.entry,
            take_profit: p.take_profit,
            stop_loss: p.stop_loss,
        },
        DrawingShape::Short(p) => ViewDrawing::Short {
            id: service.id,
            t0: t2i(p.t0),
            t1: t2i(p.t1),
            entry: p.entry,
            take_profit: p.take_profit,
            stop_loss: p.stop_loss,
        },
        DrawingShape::AnchoredVwap(a) => ViewDrawing::AnchoredVwap {
            id: service.id,
            // Price component is unused at render — the line is computed from
            // candle data. Pass 0.0 as a deterministic placeholder.
            anchor: (t2i(a.anchor_time), 0.0),
        },
    }
}

/// Convert a view-coord drawing back to a service shape. Called at commit
/// points (mouse-up after create or edit) so the service stores absolute ms.
///
/// `baseline` carries the style fields (color/width/label/…) that should be
/// preserved across the round-trip. Pass the previous `DrawingShape` for
/// edits; pass `None` for fresh creations (the result then carries the
/// per-shape defaults). Mismatched variant → defaults (which only matters
/// if a future caller tries to "morph" a shape, which we don't today).
pub fn view_to_shape(
    view: &ViewDrawing,
    candles: &[Candle],
    bar_duration_ms: i64,
    baseline: Option<&DrawingShape>,
) -> DrawingShape {
    let i2t = |i: f32| idx_to_time(i, candles, bar_duration_ms);
    use crate::drawings::shapes::{
        AnchoredVwapShape, HorizontalRayShape, LineRectShape, PositionShape, TextShape,
    };
    match view {
        ViewDrawing::Line { a, b, .. } => {
            let mut s = LineRectShape {
                a_time: i2t(a.0),
                a_price: a.1,
                b_time: i2t(b.0),
                b_price: b.1,
                color: None,
                width: 2.0,
                label: None,
            };
            if let Some(DrawingShape::Line(b)) = baseline {
                s.color = b.color;
                s.width = b.width;
                s.label = b.label.clone();
            }
            DrawingShape::Line(s)
        }
        ViewDrawing::Arrow { a, b, .. } => {
            let mut s = LineRectShape {
                a_time: i2t(a.0),
                a_price: a.1,
                b_time: i2t(b.0),
                b_price: b.1,
                color: None,
                width: 2.0,
                label: None,
            };
            if let Some(DrawingShape::Arrow(b)) = baseline {
                s.color = b.color;
                s.width = b.width;
                s.label = b.label.clone();
            }
            DrawingShape::Arrow(s)
        }
        ViewDrawing::Fibonacci { a, b, .. } => {
            let mut s = LineRectShape {
                a_time: i2t(a.0),
                a_price: a.1,
                b_time: i2t(b.0),
                b_price: b.1,
                color: None,
                width: 2.0,
                label: None,
            };
            if let Some(DrawingShape::Fibonacci(b)) = baseline {
                s.color = b.color;
                s.width = b.width;
                s.label = b.label.clone();
            }
            DrawingShape::Fibonacci(s)
        }
        ViewDrawing::Rect { a, b, .. } => {
            let mut s = LineRectShape {
                a_time: i2t(a.0),
                a_price: a.1,
                b_time: i2t(b.0),
                b_price: b.1,
                color: None,
                width: 2.0,
                label: None,
            };
            if let Some(DrawingShape::Rect(b)) = baseline {
                s.color = b.color;
                s.width = b.width;
                s.label = b.label.clone();
            }
            DrawingShape::Rect(s)
        }
        ViewDrawing::HorizontalRay {
            anchor,
            text,
            extend_left,
            ..
        } => {
            let mut s = HorizontalRayShape {
                anchor_time: i2t(anchor.0),
                anchor_price: anchor.1,
                text: text.clone(),
                color: None,
                width: 2.0,
                extend_left: *extend_left,
            };
            if let Some(DrawingShape::HorizontalRay(b)) = baseline {
                s.color = b.color;
                s.width = b.width;
                // Round-trip the persisted extend_left across edits so a
                // drag doesn't reset the flag back to the view default.
                s.extend_left = b.extend_left;
            }
            DrawingShape::HorizontalRay(s)
        }
        ViewDrawing::Text {
            anchor,
            width,
            text,
            font_size,
            ..
        } => {
            let mut s = TextShape {
                anchor_time: i2t(anchor.0),
                anchor_price: anchor.1,
                width: *width,
                text: text.clone(),
                color: None,
                font_size: *font_size,
            };
            if let Some(DrawingShape::Text(b)) = baseline {
                s.color = b.color;
                s.font_size = b.font_size;
            }
            DrawingShape::Text(s)
        }
        ViewDrawing::Long {
            t0,
            t1,
            entry,
            take_profit,
            stop_loss,
            ..
        } => {
            let mut s = PositionShape {
                t0: i2t(*t0),
                t1: i2t(*t1),
                entry: *entry,
                take_profit: *take_profit,
                stop_loss: *stop_loss,
                profit_color: None,
                loss_color: None,
                width: 2.0,
                label: None,
            };
            if let Some(DrawingShape::Long(b)) = baseline {
                s.profit_color = b.profit_color;
                s.loss_color = b.loss_color;
                s.width = b.width;
                s.label = b.label.clone();
            }
            DrawingShape::Long(s)
        }
        ViewDrawing::Short {
            t0,
            t1,
            entry,
            take_profit,
            stop_loss,
            ..
        } => {
            let mut s = PositionShape {
                t0: i2t(*t0),
                t1: i2t(*t1),
                entry: *entry,
                take_profit: *take_profit,
                stop_loss: *stop_loss,
                profit_color: None,
                loss_color: None,
                width: 2.0,
                label: None,
            };
            if let Some(DrawingShape::Short(b)) = baseline {
                s.profit_color = b.profit_color;
                s.loss_color = b.loss_color;
                s.width = b.width;
                s.label = b.label.clone();
            }
            DrawingShape::Short(s)
        }
        ViewDrawing::AnchoredVwap { anchor, .. } => {
            let mut s = AnchoredVwapShape {
                anchor_time: i2t(anchor.0),
                color: None,
                width: 2.0,
                label: None,
            };
            if let Some(DrawingShape::AnchoredVwap(b)) = baseline {
                s.color = b.color;
                s.width = b.width;
                s.label = b.label.clone();
            }
            DrawingShape::AnchoredVwap(s)
        }
    }
}
