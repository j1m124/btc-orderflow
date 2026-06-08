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

use gpui::Hsla;
use serde::{Deserialize, Serialize};

/// Lossless serde-compatible mirror of [`gpui::Hsla`]. gpui's `Hsla` does
/// not derive `Serialize`/`Deserialize`, so persisted style fields go
/// through this and convert on access.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub struct DrawingColor {
    pub h: f32,
    pub s: f32,
    pub l: f32,
    pub a: f32,
}

impl DrawingColor {
    pub fn from_hsla(c: Hsla) -> Self {
        Self {
            h: c.h,
            s: c.s,
            l: c.l,
            a: c.a,
        }
    }

    pub fn into_hsla(self) -> Hsla {
        gpui::hsla(self.h, self.s, self.l, self.a)
    }
}

/// Which paint surface a drawing lives on. `Main` is the price/candle area;
/// `Indicator(id)` pins the drawing to a sub-pane, identified by the
/// indicator instance id (persisted across reloads via [`crate::panels::
/// IndicatorPrefs::id`]). One drawing belongs to exactly one pane.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaneRef {
    Main,
    Indicator(u64),
}

impl Default for PaneRef {
    fn default() -> Self {
        PaneRef::Main
    }
}

impl PaneRef {
    pub fn is_main(&self) -> bool {
        matches!(self, PaneRef::Main)
    }
}

/// Default line width for shapes that didn't carry a width before this
/// field existed. Serde reaches for this when older blobs lack `width`.
fn default_width() -> f32 {
    1.0
}

fn is_default_width(w: &f32) -> bool {
    (*w - 1.0).abs() < f32::EPSILON
}

/// Endpoints for line / rectangle / arrow / fibonacci drawings — these
/// shapes share the same data (rectangles use the two corners as opposite
/// ends; arrows put the arrowhead at `b`).
///
/// Style fields (`color`, `width`, `label`) live alongside the geometry so
/// the same struct backs every endpoint-style shape. `color = None` means
/// "fall back to the shape's theme default" — paint code resolves it; the
/// settings strip surfaces it as a swatch the user can override.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LineRectShape {
    pub a_time: i64,
    pub a_price: f64,
    pub b_time: i64,
    pub b_price: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<DrawingColor>,
    #[serde(default = "default_width", skip_serializing_if = "is_default_width")]
    pub width: f32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// Horizontal ray: anchored at a single (time, price); the line extends from
/// that point rightward to the chart's right edge at the constant price.
/// `text` is an optional label rendered at the top-right of the ray — kept
/// as `text` (not `label`) for backwards compat with persisted blobs that
/// already use this name.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HorizontalRayShape {
    pub anchor_time: i64,
    pub anchor_price: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<DrawingColor>,
    #[serde(default = "default_width", skip_serializing_if = "is_default_width")]
    pub width: f32,
}

/// Anchored VWAP: a single time anchor; the line is computed from the chart's
/// bars (cumulative volume-weighted price from the anchor bar forward) at
/// paint time. Per-bar `vwap × volume` accumulation, so the line follows the
/// data rather than being stored as a polyline.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AnchoredVwapShape {
    pub anchor_time: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<DrawingColor>,
    #[serde(default = "default_width", skip_serializing_if = "is_default_width")]
    pub width: f32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// Text label anchored at one point. `width` is in screen pixels (text uses
/// pixel-based font sizing, so a world-coord width wouldn't make sense). The
/// text content IS the visible label — there's no separate `label` field; the
/// strip suppresses the label slot for Text. `color` overrides the theme
/// foreground; line-width isn't meaningful for Text.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TextShape {
    pub anchor_time: i64,
    pub anchor_price: f64,
    pub width: f32,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<DrawingColor>,
}

/// Long / short position zone — entry, take-profit, stop-loss prices over a
/// time range. Direction (long vs short) lives on the enum tag.
///
/// Two style colors: `profit_color` shades the TP band, `loss_color` shades
/// the SL band. Strip surfaces both swatches; gear-window controls (line
/// widths per side, label content) are future scope.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PositionShape {
    pub t0: i64,
    pub t1: i64,
    pub entry: f64,
    pub take_profit: f64,
    pub stop_loss: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profit_color: Option<DrawingColor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loss_color: Option<DrawingColor>,
    #[serde(default = "default_width", skip_serializing_if = "is_default_width")]
    pub width: f32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
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
    /// User lock — when true the strip's lock toggle blocks geometry edits
    /// (drag handles disappear, trash is disabled) and the right-click
    /// context menu's delete entry hides. Selection + style changes still
    /// work so the user can unlock from the strip.
    #[serde(default, skip_serializing_if = "is_false")]
    pub locked: bool,
    /// `None` → visible on every timeframe (default). `Some(set)` → visible only
    /// on the timeframes whose `Timeframe::as_str()` is in the set. Strings,
    /// not enum discriminants, so the schema survives Timeframe enum changes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tf_filter: Option<BTreeSet<String>>,
    /// Which paint surface the drawing lives on. Defaults to `Main` so older
    /// blobs (and every drawing predating sub-pane drawing) load unchanged.
    #[serde(default, skip_serializing_if = "PaneRef::is_main")]
    pub pane: PaneRef,
    /// Provenance. Defaults to `User` for pre-AI-tools data so existing
    /// drawings load unchanged. `skip_serializing_if` keeps the JSON
    /// compact for the common case.
    #[serde(default = "default_user_origin", skip_serializing_if = "is_user_origin")]
    pub created_by: DrawingOrigin,
    pub shape: DrawingShape,
}

fn is_false(b: &bool) -> bool {
    !*b
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
