//! Drawing shape data + (de)serialization.
//!
//! Each variant of [`DrawingShape`] holds a small per-shape struct so adding a
//! new tool (horizontal ray, brush, fib …) only requires:
//!   1. add the data struct here,
//!   2. add the [`DrawingShape`] variant,
//!   3. add the [`Tool`](super::tool::Tool) entry + creation rule,
//!   4. wire it into the chart's paint + hit-test match arms.
//!
//! Time anchors are absolute `i64` epoch milliseconds (UTC). Chart-side render
//! maps ms ↔ fractional bar index via the chart's own candle buffer, so the
//! same drawing renders correctly across timeframes and after backfill.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

/// Endpoints for line / rectangle / arrow drawings — these shapes share the
/// same data (rectangles use the two corners as opposite ends; arrows put
/// the arrowhead at `b`).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LineRectShape {
    pub a_time: i64,
    pub a_price: f64,
    pub b_time: i64,
    pub b_price: f64,
}

/// Horizontal ray: anchored at a single (time, price); the line extends from
/// that point rightward to the chart's right edge at the constant price.
/// `text` is an optional label rendered at the top-right of the ray.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HorizontalRayShape {
    pub anchor_time: i64,
    pub anchor_price: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

/// Anchored VWAP: a single time anchor; the line is computed from the chart's
/// bars (cumulative volume-weighted price from the anchor bar forward) at
/// paint time. Per-bar `vwap × volume` accumulation, so the line follows the
/// data rather than being stored as a polyline.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AnchoredVwapShape {
    pub anchor_time: i64,
}

/// Text label anchored at one point. `width` is in screen pixels (text uses
/// pixel-based font sizing, so a world-coord width wouldn't make sense).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TextShape {
    pub anchor_time: i64,
    pub anchor_price: f64,
    pub width: f32,
    pub text: String,
}

/// Long / short position zone — entry, take-profit, stop-loss prices over a
/// time range. Direction (long vs short) lives on the enum tag.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PositionShape {
    pub t0: i64,
    pub t1: i64,
    pub entry: f64,
    pub take_profit: f64,
    pub stop_loss: f64,
}

/// All drawing shapes. Serialized with an external `type` tag so the JSON is
/// self-describing and unknown variants can be detected + skipped during load
/// (forward-compat for shapes added in newer binaries).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DrawingShape {
    Line(LineRectShape),
    Rect(LineRectShape),
    Text(TextShape),
    Long(PositionShape),
    Short(PositionShape),
    /// Horizontal ray at a fixed price, starting at `anchor_time` and
    /// extending to the chart's right edge.
    HorizontalRay(HorizontalRayShape),
    /// Line with an arrowhead at the `b` endpoint.
    Arrow(LineRectShape),
    /// Fibonacci retracement. `a`/`b` define the price range; the renderer
    /// draws the standard set of horizontal levels between them at the same
    /// time extent.
    Fibonacci(LineRectShape),
    /// Anchored VWAP starting at `anchor_time`. The line itself is computed
    /// per-bar from the chart's candle buffer; only the anchor is persisted.
    AnchoredVwap(AnchoredVwapShape),
}

impl DrawingShape {
    /// Short single-line label used in the object tree.
    pub fn label(&self, id: u64) -> String {
        match self {
            DrawingShape::Line(_) => format!("Line #{id}"),
            DrawingShape::Rect(_) => format!("Rect #{id}"),
            DrawingShape::Text(t) => {
                let preview: String = t.text.chars().take(16).collect();
                if t.text.chars().count() > 16 {
                    format!("Text #{id} · {preview}…")
                } else if preview.is_empty() {
                    format!("Text #{id}")
                } else {
                    format!("Text #{id} · {preview}")
                }
            }
            DrawingShape::Long(p) => format!("Long #{id} · ${:.2}", p.entry),
            DrawingShape::Short(p) => format!("Short #{id} · ${:.2}", p.entry),
            DrawingShape::HorizontalRay(r) => format!("Ray #{id} · ${:.2}", r.anchor_price),
            DrawingShape::Arrow(_) => format!("Arrow #{id}"),
            DrawingShape::Fibonacci(_) => format!("Fib #{id}"),
            DrawingShape::AnchoredVwap(_) => format!("AVWAP #{id}"),
        }
    }
}

/// Where a drawing came from. Persisted so the object-tree can badge AI
/// drawings + future auditing (e.g. "show only my levels"). Old drawings
/// from before this field existed load as `User` via `default_user_origin`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DrawingOrigin {
    /// Hand-drawn by the user via a drawing tool.
    User,
    /// Programmatically created by the AI assistant in response to a tool
    /// call. Renders identically on the chart but the object tree marks it.
    Ai,
}

fn default_user_origin() -> DrawingOrigin {
    DrawingOrigin::User
}

fn is_user_origin(o: &DrawingOrigin) -> bool {
    matches!(o, DrawingOrigin::User)
}

/// A complete drawing: shape data + metadata. Stored in the service keyed by
/// symbol; serialized to disk via `persistence::save_drawings`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Drawing {
    pub id: u64,
    #[serde(default)]
    pub hidden: bool,
    /// `None` → visible on every timeframe (default). `Some(set)` → visible only
    /// on the timeframes whose `Timeframe::as_str()` is in the set. Strings,
    /// not enum discriminants, so the schema survives Timeframe enum changes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tf_filter: Option<BTreeSet<String>>,
    /// Provenance. Defaults to `User` for pre-AI-tools data so existing
    /// drawings load unchanged. `skip_serializing_if` keeps the JSON
    /// compact for the common case.
    #[serde(default = "default_user_origin", skip_serializing_if = "is_user_origin")]
    pub created_by: DrawingOrigin,
    pub shape: DrawingShape,
}

impl Drawing {
    pub fn label(&self) -> String {
        self.shape.label(self.id)
    }

    /// True iff this drawing should render on a chart whose current timeframe
    /// matches `tf_str` (`Timeframe::as_str()`).
    pub fn visible_on(&self, tf_str: &str) -> bool {
        if self.hidden {
            return false;
        }
        match &self.tf_filter {
            None => true,
            Some(set) => set.contains(tf_str),
        }
    }
}
