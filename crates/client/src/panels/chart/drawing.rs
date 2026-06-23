//! Drawing model (the [`Drawing`] enum), in-progress creation / edit / text
//! state, and hit-testing. Anchored in world coords (time-index, price);
//! screen mapping borrows [`super::coords`]. Driven by `view`'s mouse handlers
//! and persisted via the drawings service.

use gpui::Entity;
use gpui_component::input::InputState;

use super::coords::{index_to_screen, price_to_screen};
use crate::drawings::service::DrawingId;
use crate::drawings::tool::Tool;

/// All drawings are anchored in world coords: time-index (fractional candle
/// position) for the x axis, price for the y axis. Strokes, handles, and text
/// are rendered in pixel sizes so they don't visually scale with zoom.
#[derive(Clone, Debug)]
pub enum Drawing {
    Line {
        id: DrawingId,
        a: (f32, f64),
        b: (f32, f64),
    },
    Rect {
        id: DrawingId,
        a: (f32, f64),
        b: (f32, f64),
    },
    Text {
        id: DrawingId,
        anchor: (f32, f64),
        /// Box width in screen pixels — text wraps inside this width and the
        /// rendered div's height grows with content. World-coord width
        /// wouldn't work because text uses pixel-based font sizing.
        width: f32,
        text: String,
        /// Font-size in pixels. Default 12. Edited via the settings window.
        font_size: f32,
    },
    Long {
        id: DrawingId,
        t0: f32,
        t1: f32,
        entry: f64,
        take_profit: f64,
        stop_loss: f64,
    },
    Short {
        id: DrawingId,
        t0: f32,
        t1: f32,
        entry: f64,
        take_profit: f64,
        stop_loss: f64,
    },
    HorizontalRay {
        id: DrawingId,
        /// `(time_idx, price)`. The line extends from this point to the
        /// chart's right edge at the constant price.
        anchor: (f32, f64),
        /// Optional label rendered at the top-right of the ray. Edited via
        /// the chart's right-click menu.
        text: Option<String>,
        /// When true, the stroke also extends left from the anchor to the
        /// chart's left edge — turning the ray into a full horizontal
        /// line. Drives both the Ray tool's "Extend left" toggle and the
        /// dedicated HorizontalLine tool (which creates a ray with this
        /// flag on).
        extend_left: bool,
    },
    Arrow {
        id: DrawingId,
        a: (f32, f64),
        /// Endpoint that carries the arrowhead.
        b: (f32, f64),
    },
    /// Fibonacci retracement. `a` and `b` define the price range and
    /// horizontal extent; the renderer paints horizontal levels at the
    /// standard ratios between `a.1` and `b.1`.
    Fibonacci {
        id: DrawingId,
        a: (f32, f64),
        b: (f32, f64),
    },
    /// Anchored VWAP starting at `anchor.0` (fractional bar index). Price
    /// component `anchor.1` is unused for rendering (the line is computed
    /// from candle data) but kept for symmetry with single-anchor drawings
    /// so existing handle/hit-test scaffolding doesn't special-case.
    AnchoredVwap { id: DrawingId, anchor: (f32, f64) },
    /// Fixed-Range Volume Profile. `t0` / `t1` are fractional bar
    /// indices; the painter aggregates per-bucket bid/ask volume over
    /// the corresponding wall-clock time range and renders the profile
    /// inside the bracket. `p0` / `p1` are the visual price anchors —
    /// they paint a rectangle outline + give the user 2 corner handles
    /// for resize, but do NOT clip the profile bars (which always span
    /// the chart's full vertical extent). `None` on legacy FRVPs that
    /// predated price anchors; paint then falls back to the chart edges
    /// and the rectangle reduces to two vertical hairlines.
    /// `params` is a snapshot of the persisted `VolumeProfileParams` so
    /// paint doesn't have to re-read the service inside its `'static`
    /// closure. `output` is the precomputed per-bucket aggregate —
    /// built at chart-render time using the chart's footprint cell
    /// cache. `None` when cells for the instance's bucket haven't
    /// loaded yet (paint then draws just the bracket so the user sees
    /// the geometry while waiting).
    Frvp {
        id: DrawingId,
        t0: f32,
        t1: f32,
        p0: Option<f64>,
        p1: Option<f64>,
        params: crate::volume_profile::VolumeProfileParams,
        output: Option<crate::volume_profile::VolumeProfileOutput>,
    },
}

impl Drawing {
    pub(super) fn id(&self) -> DrawingId {
        match self {
            Drawing::Line { id, .. }
            | Drawing::Rect { id, .. }
            | Drawing::Text { id, .. }
            | Drawing::Long { id, .. }
            | Drawing::Short { id, .. }
            | Drawing::HorizontalRay { id, .. }
            | Drawing::Arrow { id, .. }
            | Drawing::Fibonacci { id, .. }
            | Drawing::AnchoredVwap { id, .. }
            | Drawing::Frvp { id, .. } => *id,
        }
    }

    /// Shift this drawing's time (x) anchors right by `n` candle indices. Used
    /// when older candles are prepended so drawings stay attached to their bars.
    pub(super) fn shift_x(&mut self, n: f32) {
        match self {
            Drawing::Line { a, b, .. }
            | Drawing::Rect { a, b, .. }
            | Drawing::Arrow { a, b, .. }
            | Drawing::Fibonacci { a, b, .. } => {
                a.0 += n;
                b.0 += n;
            }
            Drawing::Text { anchor, .. }
            | Drawing::HorizontalRay { anchor, .. }
            | Drawing::AnchoredVwap { anchor, .. } => {
                anchor.0 += n;
            }
            Drawing::Long { t0, t1, .. } | Drawing::Short { t0, t1, .. } => {
                *t0 += n;
                *t1 += n;
            }
            Drawing::Frvp { t0, t1, .. } => {
                *t0 += n;
                *t1 += n;
            }
        }
    }
}

/// In-progress drawing being constructed via click-drag. Held in `ChartState`
/// from mouse-down on a drawing tool until mouse-up commits it into
/// `state.drawings` with a fresh `DrawingId`. Position variants store the
/// time anchor + a default horizontal width fixed at creation — vertical drag
/// changes only the TP price; width edits happen after via endpoint handles
/// (deferred to v2).
#[derive(Clone, Debug)]
pub(super) enum CreatingDrawing {
    Line {
        a: (f32, f64),
        b: (f32, f64),
    },
    Arrow {
        a: (f32, f64),
        b: (f32, f64),
    },
    Rect {
        a: (f32, f64),
        b: (f32, f64),
    },
    Fibonacci {
        a: (f32, f64),
        b: (f32, f64),
    },
    Long {
        entry: (f32, f64),
        tp: (f32, f64),
    },
    Short {
        entry: (f32, f64),
        tp: (f32, f64),
    },
    /// One-click horizontal ray. The mouse-down handler commits it
    /// immediately; this variant only exists for symmetry with the other
    /// in-flight states (e.g. so a tool switch mid-creation can drop it).
    /// `extend_left` carries through to the committed shape — true for
    /// drawings created via the HorizontalLine tool, false for the regular
    /// Ray tool.
    HorizontalRay {
        anchor: (f32, f64),
        extend_left: bool,
    },
    /// One-click Anchored VWAP. Same shape as `HorizontalRay`: the mouse-down
    /// handler commits the drawing with this anchor immediately; the line
    /// itself is computed at paint time from the chart's candle buffer.
    AnchoredVwap {
        anchor: (f32, f64),
    },
    /// FRVP being click-dragged into existence. Two clicks: first sets
    /// `a`, mouse-move updates `b`, second click commits. Vertical
    /// (price) component is ignored — the profile is time-only. We
    /// reuse `(f32, f64)` for shape-symmetry with the other two-anchor
    /// variants so `set_end` can be uniform.
    Frvp {
        a: (f32, f64),
        b: (f32, f64),
    },
}

/// Default-R:R risk-to-reward ratio for new positions. SL is placed half the
/// distance to TP on the opposite side of entry — per spec section 9d.
pub(super) const POSITION_DEFAULT_RR: f64 = 2.0;
/// Default position width as a fraction of `view_size` — per spec section 9d.
pub(super) const POSITION_DEFAULT_WIDTH_RATIO: f32 = 0.30;

impl CreatingDrawing {
    pub(super) fn from_tool(tool: Tool, pt: (f32, f64), default_width: f32) -> Option<Self> {
        match tool {
            Tool::Line => Some(CreatingDrawing::Line { a: pt, b: pt }),
            Tool::Arrow => Some(CreatingDrawing::Arrow { a: pt, b: pt }),
            Tool::Rectangle => Some(CreatingDrawing::Rect { a: pt, b: pt }),
            Tool::Fibonacci => Some(CreatingDrawing::Fibonacci { a: pt, b: pt }),
            Tool::HorizontalRay => Some(CreatingDrawing::HorizontalRay {
                anchor: pt,
                extend_left: false,
            }),
            Tool::HorizontalLine => Some(CreatingDrawing::HorizontalRay {
                anchor: pt,
                extend_left: true,
            }),
            Tool::AnchoredVwap => Some(CreatingDrawing::AnchoredVwap { anchor: pt }),
            Tool::FixedRangeVolumeProfile => Some(CreatingDrawing::Frvp { a: pt, b: pt }),
            Tool::Long => Some(CreatingDrawing::Long {
                entry: pt,
                tp: (pt.0 + default_width, pt.1),
            }),
            Tool::Short => Some(CreatingDrawing::Short {
                entry: pt,
                tp: (pt.0 + default_width, pt.1),
            }),
            _ => None,
        }
    }

    pub(super) fn set_end(&mut self, pt: (f32, f64)) {
        match self {
            CreatingDrawing::Line { b, .. }
            | CreatingDrawing::Arrow { b, .. }
            | CreatingDrawing::Rect { b, .. }
            | CreatingDrawing::Fibonacci { b, .. } => *b = pt,
            // FRVP only uses the x (time) component of `pt`; the y
            // (price) is captured for shape-symmetry but never read.
            CreatingDrawing::Frvp { b, .. } => *b = pt,
            // Position width is locked at creation; only the TP price (y)
            // follows the cursor. Otherwise zero-width rects would be easy to
            // accidentally create with a near-vertical drag.
            CreatingDrawing::Long { tp, .. } | CreatingDrawing::Short { tp, .. } => {
                tp.1 = pt.1;
            }
            // Horizontal ray is single-click; no trailing endpoint to track.
            CreatingDrawing::HorizontalRay { .. } => {}
            // Same single-click model.
            CreatingDrawing::AnchoredVwap { .. } => {}
        }
    }

    /// Shift the in-progress drawing's time (x) anchors right by `n` indices.
    pub(super) fn shift_x(&mut self, n: f32) {
        match self {
            CreatingDrawing::Line { a, b }
            | CreatingDrawing::Arrow { a, b }
            | CreatingDrawing::Rect { a, b }
            | CreatingDrawing::Fibonacci { a, b } => {
                a.0 += n;
                b.0 += n;
            }
            CreatingDrawing::Long { entry, tp } | CreatingDrawing::Short { entry, tp } => {
                entry.0 += n;
                tp.0 += n;
            }
            CreatingDrawing::HorizontalRay { anchor, .. } => {
                anchor.0 += n;
            }
            CreatingDrawing::AnchoredVwap { anchor } => {
                anchor.0 += n;
            }
            CreatingDrawing::Frvp { a, b } => {
                a.0 += n;
                b.0 += n;
            }
        }
    }

    pub(super) fn into_drawing(self, id: DrawingId) -> Drawing {
        match self {
            CreatingDrawing::Line { a, b } => Drawing::Line { id, a, b },
            CreatingDrawing::Arrow { a, b } => Drawing::Arrow { id, a, b },
            CreatingDrawing::Rect { a, b } => Drawing::Rect { id, a, b },
            CreatingDrawing::Fibonacci { a, b } => Drawing::Fibonacci { id, a, b },
            CreatingDrawing::HorizontalRay { anchor, extend_left } => Drawing::HorizontalRay {
                id,
                anchor,
                text: None,
                extend_left,
            },
            CreatingDrawing::AnchoredVwap { anchor } => Drawing::AnchoredVwap { id, anchor },
            CreatingDrawing::Long { entry, tp } => {
                // 1 : POSITION_DEFAULT_RR — risk = reward / RR. For long the
                // SL sits *below* entry.
                let reward = tp.1 - entry.1;
                let sl = entry.1 - reward / POSITION_DEFAULT_RR;
                Drawing::Long {
                    id,
                    t0: entry.0,
                    t1: tp.0,
                    entry: entry.1,
                    take_profit: tp.1,
                    stop_loss: sl,
                }
            }
            CreatingDrawing::Short { entry, tp } => {
                // For short, reward materialises when price *falls*, so swap
                // the sign — SL sits above entry.
                let reward = entry.1 - tp.1;
                let sl = entry.1 + reward / POSITION_DEFAULT_RR;
                Drawing::Short {
                    id,
                    t0: entry.0,
                    t1: tp.0,
                    entry: entry.1,
                    take_profit: tp.1,
                    stop_loss: sl,
                }
            }
            CreatingDrawing::Frvp { a, b } => {
                // Order time anchors so `t0 <= t1`. Price anchors are kept
                // paired with their respective time corner so the diagonal
                // of the drag (which way the user moved) is preserved on
                // commit — the persisted shape stores `(a_time, a_price)`
                // as the start of the drag and `(b_time, b_price)` as the
                // end, regardless of which order the user dragged.
                let ((t0, p0), (t1, p1)) = if a.0 <= b.0 {
                    ((a.0, a.1), (b.0, b.1))
                } else {
                    ((b.0, b.1), (a.0, a.1))
                };
                Drawing::Frvp {
                    id,
                    t0,
                    t1,
                    p0: Some(p0),
                    p1: Some(p1),
                    params: crate::volume_profile::VolumeProfileParams {
                        // FRVP defaults to a Left anchor — matches the
                        // design call from the grilling that VRVP is
                        // right-anchored, FRVP is left-anchored.
                        anchor: crate::volume_profile::AnchorEdge::Left,
                        ..crate::volume_profile::VolumeProfileParams::default()
                    },
                    // Filled in by the chart render path after the
                    // shape ↔ view conversion (only it knows the cells).
                    output: None,
                }
            }
        }
    }

    /// Synthesize a paint-only `Drawing` with a sentinel id so the same render
    /// path used for committed drawings can also draw the in-progress preview.
    pub(super) fn preview(&self) -> Drawing {
        self.clone().into_drawing(0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum EditHandle {
    Body,
    /// Line/Rect first anchor (the "a" endpoint).
    EndpointA,
    /// Line/Rect second anchor (the "b" endpoint).
    EndpointB,
    /// Long/Short entry price line — vertical drag moves entry only.
    PositionEntry,
    /// Long/Short TP price line — vertical drag moves TP only.
    PositionTakeProfit,
    /// Long/Short SL price line — vertical drag moves SL only.
    PositionStopLoss,
    /// Long/Short left time edge (t0) — horizontal drag moves t0 only.
    PositionStart,
    /// Long/Short right time edge (t1) — horizontal drag moves t1 only.
    PositionEnd,
}

/// Active handle drag on an existing drawing. `baseline` is the drawing's
/// world coords at mouse-down so translations apply from a stable reference.
/// `anchor_screen` is the cursor position in canvas-relative pixels at
/// mouse-down — needed for handles that operate in pixel units (e.g. the
/// text-box width handle). `moved` flips to true the first time mouse-move
/// produces a non-zero translation; mouse-up reads it to distinguish "user
/// dragged the drawing" (snap to current-TF grid) from "user just clicked it
/// to select" (leave the drawing alone).
#[derive(Clone, Debug)]
pub(super) struct EditDrag {
    pub(super) id: DrawingId,
    pub(super) handle: EditHandle,
    pub(super) baseline: Drawing,
    pub(super) anchor_world: (f32, f64),
    pub(super) anchor_screen: (f32, f32),
    pub(super) moved: bool,
}

/// Active inline text editor. Holds the live `Input` while the user types;
/// committed on mouse-down outside the input (canvas's mouse-down handler
/// drains this first). `existing_id == Some` when re-editing an existing
/// text drawing; `None` for new-text creation.
pub(super) struct TextEditing {
    pub(super) existing_id: Option<DrawingId>,
    pub(super) anchor: (f32, f64),
    pub(super) width: f32,
    pub(super) input: Entity<InputState>,
}

/// Default pixel width for new text boxes. Drag the right-edge handle to
/// resize after creation.
pub(super) const TEXT_DEFAULT_WIDTH_PX: f32 = 160.0;

/// Estimate the rendered pixel height of a text-drawing box of given pixel
/// `width` containing `text`. Matches the visual extent of the
/// `text_xs() / px_1p5() / py_0p5() / border_1` div used in the render path
/// so hit-test bounds line up with what the user sees.
fn estimate_text_box_height(text: &str, width: f32, font_size: f32) -> f32 {
    // Line height ≈ 1.5× font-size (matches gpui's default leading for the
    // proportional fonts we ship). Padding totals ~4px vertical
    // (py_0p5 each side = 2px) + ~2px for the 1px border each side.
    let line_height = (font_size * 1.5).max(font_size + 2.0);
    const VERTICAL_PADDING_AND_BORDER: f32 = 6.0;
    // Horizontal padding (px_1p5 = 6px each side = 12px) eats into the
    // content width.
    const HORIZONTAL_PADDING: f32 = 12.0;
    // Approximate proportional char width — scales with font-size so a
    // 24 px text box doesn't dramatically over-wrap.
    let avg_char_width = (font_size * 0.55).max(3.0);

    let content_w = (width - HORIZONTAL_PADDING).max(avg_char_width);
    let chars_per_line = (content_w / avg_char_width).floor().max(1.0);
    // Count wrapped lines per paragraph (split by '\n') so multi-line text
    // doesn't under-report when the user typed explicit breaks.
    let mut total_lines = 0.0f32;
    let mut had_content = false;
    for paragraph in text.split('\n') {
        had_content = true;
        let chars = paragraph.chars().count().max(1) as f32;
        let para_lines = (chars / chars_per_line).ceil().max(1.0);
        total_lines += para_lines;
    }
    if !had_content {
        total_lines = 1.0;
    }
    total_lines * line_height + VERTICAL_PADDING_AND_BORDER
}

/// Round every time-anchor in a view-coord [`Drawing`] to the nearest integer
/// candle slot. Used at edit-drag commit so a drawing dragged on TF B
/// (regardless of where its original anchors were exact) ends up flush with
/// TF B's candle grid.
pub(super) fn snap_view_to_grid(view: &mut Drawing) {
    match view {
        Drawing::Line { a, b, .. }
        | Drawing::Rect { a, b, .. }
        | Drawing::Arrow { a, b, .. }
        | Drawing::Fibonacci { a, b, .. } => {
            a.0 = a.0.round();
            b.0 = b.0.round();
        }
        Drawing::Text { anchor, .. }
        | Drawing::HorizontalRay { anchor, .. }
        | Drawing::AnchoredVwap { anchor, .. } => {
            anchor.0 = anchor.0.round();
        }
        Drawing::Long { t0, t1, .. } | Drawing::Short { t0, t1, .. } => {
            *t0 = t0.round();
            *t1 = t1.round();
        }
        Drawing::Frvp { t0, t1, .. } => {
            *t0 = t0.round();
            *t1 = t1.round();
        }
    }
}

const DRAWING_HANDLE_HIT_PX: f32 = 8.0;
const DRAWING_STROKE_HIT_PX: f32 = 6.0;

fn point_to_segment_dist(ax: f32, ay: f32, bx: f32, by: f32, px: f32, py: f32) -> f32 {
    let abx = bx - ax;
    let aby = by - ay;
    let apx = px - ax;
    let apy = py - ay;
    let len2 = abx * abx + aby * aby;
    if len2 < 1e-6 {
        return (apx * apx + apy * apy).sqrt();
    }
    let t = ((apx * abx + apy * aby) / len2).clamp(0.0, 1.0);
    let cx = ax + t * abx;
    let cy = ay + t * aby;
    let dx = px - cx;
    let dy = py - cy;
    (dx * dx + dy * dy).sqrt()
}

impl Drawing {
    /// Translate all anchors in world coords.
    fn translate(&mut self, dt: f32, dp: f64) {
        match self {
            Drawing::Line { a, b, .. }
            | Drawing::Rect { a, b, .. }
            | Drawing::Arrow { a, b, .. }
            | Drawing::Fibonacci { a, b, .. } => {
                a.0 += dt;
                a.1 += dp;
                b.0 += dt;
                b.1 += dp;
            }
            Drawing::Text { anchor, .. } | Drawing::HorizontalRay { anchor, .. } => {
                anchor.0 += dt;
                anchor.1 += dp;
            }
            // AnchoredVwap: only the time component is meaningful; the y-axis
            // movement is intentionally ignored so vertical drags don't shift
            // an anchor away from its bar.
            Drawing::AnchoredVwap { anchor, .. } => {
                anchor.0 += dt;
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
                *t0 += dt;
                *t1 += dt;
                *entry += dp;
                *take_profit += dp;
                *stop_loss += dp;
            }
            // FRVP — time anchors always shift. Price anchors shift only
            // when present (legacy FRVPs that never had price anchors
            // stay None, so their bracket continues to span the chart's
            // full vertical extent on every paint).
            Drawing::Frvp { t0, t1, p0, p1, .. } => {
                *t0 += dt;
                *t1 += dt;
                if let Some(p) = p0 {
                    *p += dp;
                }
                if let Some(p) = p1 {
                    *p += dp;
                }
            }
        }
    }

    /// Set the A or B endpoint of a Line/Rect/Arrow drawing. No-op on other
    /// variants — they have variant-specific handles wired in later steps.
    fn set_endpoint(&mut self, handle: EditHandle, pt: (f32, f64)) {
        match handle {
            EditHandle::EndpointA => match self {
                Drawing::Line { a, .. }
                | Drawing::Rect { a, .. }
                | Drawing::Arrow { a, .. }
                | Drawing::Fibonacci { a, .. } => *a = pt,
                Drawing::HorizontalRay { anchor, .. } => *anchor = pt,
                // FRVP A-corner: (t0, p0). Price is wrapped so legacy
                // shapes that never had a price anchor materialize one
                // the first time the user drags a corner.
                Drawing::Frvp { t0, p0, .. } => {
                    *t0 = pt.0;
                    *p0 = Some(pt.1);
                }
                _ => {}
            },
            EditHandle::EndpointB => match self {
                Drawing::Line { b, .. }
                | Drawing::Rect { b, .. }
                | Drawing::Arrow { b, .. }
                | Drawing::Fibonacci { b, .. } => *b = pt,
                Drawing::Frvp { t1, p1, .. } => {
                    *t1 = pt.0;
                    *p1 = Some(pt.1);
                }
                _ => {}
            },
            // Body + position handles handled outside (translate / apply_edit
            // do per-field mutations rather than going through endpoint
            // assignment).
            _ => {}
        }
    }

    /// Anchors in the order rendered, for both painting handles and hit
    /// testing. Empty for variants without endpoint handles (positions add
    /// their own set in their dedicated step).
    fn endpoint_anchors(&self) -> Vec<(EditHandle, (f32, f64))> {
        match self {
            Drawing::Line { a, b, .. }
            | Drawing::Rect { a, b, .. }
            | Drawing::Arrow { a, b, .. }
            | Drawing::Fibonacci { a, b, .. } => {
                vec![(EditHandle::EndpointA, *a), (EditHandle::EndpointB, *b)]
            }
            Drawing::HorizontalRay { anchor, .. } => {
                vec![(EditHandle::EndpointA, *anchor)]
            }
            // FRVP exposes opposite-corner handles only when both price
            // anchors are present (i.e. NOT a legacy time-only shape).
            // Legacy shapes have no resizable handles — body drag still
            // works, and editing prices via the gear window would be a
            // follow-up if anyone needs to upgrade old shapes in place.
            Drawing::Frvp {
                t0,
                t1,
                p0: Some(p0),
                p1: Some(p1),
                ..
            } => vec![
                (EditHandle::EndpointA, (*t0, *p0)),
                (EditHandle::EndpointB, (*t1, *p1)),
            ],
            _ => Vec::new(),
        }
    }
}

/// Apply an edit drag in-place: copy `baseline` into `out`, then move the
/// specified handle (or whole body) by the world-delta `(dt, dp)`. Position
/// handles ignore the dimension they don't own (e.g. dragging a TP price
/// only changes the price, not the time anchors).
pub(super) fn apply_edit(out: &mut Drawing, baseline: &Drawing, handle: EditHandle, dt: f32, dp: f64) {
    *out = baseline.clone();
    match handle {
        EditHandle::Body => out.translate(dt, dp),
        EditHandle::EndpointA | EditHandle::EndpointB => {
            let baseline_pt = match baseline {
                Drawing::Line { a, b, .. }
                | Drawing::Rect { a, b, .. }
                | Drawing::Arrow { a, b, .. }
                | Drawing::Fibonacci { a, b, .. } => {
                    if handle == EditHandle::EndpointA {
                        *a
                    } else {
                        *b
                    }
                }
                // Horizontal ray only has the A anchor; B doesn't apply.
                Drawing::HorizontalRay { anchor, .. } if handle == EditHandle::EndpointA => *anchor,
                // FRVP — corner drag against the matching (t, p) anchor.
                // Only fires for shapes that surfaced handles in the
                // first place, so price unwrap is safe.
                Drawing::Frvp { t0, t1, p0: Some(p0), p1: Some(p1), .. } => {
                    if handle == EditHandle::EndpointA {
                        (*t0, *p0)
                    } else {
                        (*t1, *p1)
                    }
                }
                _ => return,
            };
            out.set_endpoint(handle, (baseline_pt.0 + dt, baseline_pt.1 + dp));
        }
        EditHandle::PositionEntry => {
            let base = match baseline {
                Drawing::Long { entry, .. } | Drawing::Short { entry, .. } => *entry,
                _ => return,
            };
            match out {
                Drawing::Long { entry, .. } | Drawing::Short { entry, .. } => {
                    *entry = base + dp;
                }
                _ => {}
            }
        }
        EditHandle::PositionTakeProfit => {
            let base = match baseline {
                Drawing::Long { take_profit, .. } | Drawing::Short { take_profit, .. } => {
                    *take_profit
                }
                _ => return,
            };
            match out {
                Drawing::Long { take_profit, .. } | Drawing::Short { take_profit, .. } => {
                    *take_profit = base + dp;
                }
                _ => {}
            }
        }
        EditHandle::PositionStopLoss => {
            let base = match baseline {
                Drawing::Long { stop_loss, .. } | Drawing::Short { stop_loss, .. } => *stop_loss,
                _ => return,
            };
            match out {
                Drawing::Long { stop_loss, .. } | Drawing::Short { stop_loss, .. } => {
                    *stop_loss = base + dp;
                }
                _ => {}
            }
        }
        EditHandle::PositionStart => {
            let base = match baseline {
                Drawing::Long { t0, .. } | Drawing::Short { t0, .. } => *t0,
                _ => return,
            };
            match out {
                Drawing::Long { t0, .. } | Drawing::Short { t0, .. } => {
                    *t0 = base + dt;
                }
                _ => {}
            }
        }
        EditHandle::PositionEnd => {
            let base = match baseline {
                Drawing::Long { t1, .. } | Drawing::Short { t1, .. } => *t1,
                _ => return,
            };
            match out {
                Drawing::Long { t1, .. } | Drawing::Short { t1, .. } => {
                    *t1 = base + dt;
                }
                _ => {}
            }
        }
    }
}

/// Hit-test all drawings against a canvas-relative screen point. Returns the
/// topmost hit (drawings later in the vec are drawn last and thus on top).
pub(super) fn hit_test_drawings(
    drawings: &[Drawing],
    view_start: f32,
    view_size: f32,
    y_lo: f64,
    y_hi: f64,
    canvas_w: f32,
    canvas_h: f32,
    y_axis_gap: f32,
    pt_x: f32,
    pt_y: f32,
) -> Option<(DrawingId, EditHandle)> {
    let to_screen = |w: (f32, f64)| {
        let x = index_to_screen(view_start, view_size, w.0, canvas_w, y_axis_gap);
        let y = price_to_screen(y_lo, y_hi, w.1, canvas_h);
        (x, y)
    };
    for d in drawings.iter().rev() {
        // Endpoint handles win over body — they're smaller hit targets but
        // user intent is unambiguous when clicking near a handle.
        for (handle, world) in d.endpoint_anchors() {
            let (hx, hy) = to_screen(world);
            let dx = pt_x - hx;
            let dy = pt_y - hy;
            if (dx * dx + dy * dy).sqrt() <= DRAWING_HANDLE_HIT_PX {
                return Some((d.id(), handle));
            }
        }
        match d {
            Drawing::Line { a, b, .. } | Drawing::Arrow { a, b, .. } => {
                let (ax, ay) = to_screen(*a);
                let (bx, by) = to_screen(*b);
                if point_to_segment_dist(ax, ay, bx, by, pt_x, pt_y) <= DRAWING_STROKE_HIT_PX {
                    return Some((d.id(), EditHandle::Body));
                }
            }
            Drawing::HorizontalRay { anchor, .. } => {
                // Hit if the point sits within stroke-hit-px of the ray's
                // y-line, anywhere to the right of the anchor x (clipped to
                // canvas).
                let (ax, ay) = to_screen(*anchor);
                if (pt_y - ay).abs() <= DRAWING_STROKE_HIT_PX && pt_x >= ax - DRAWING_HANDLE_HIT_PX
                {
                    return Some((d.id(), EditHandle::Body));
                }
            }
            Drawing::Rect { a, b, .. } | Drawing::Fibonacci { a, b, .. } => {
                let (ax, ay) = to_screen(*a);
                let (bx, by) = to_screen(*b);
                let (xmin, xmax) = (ax.min(bx), ax.max(bx));
                let (ymin, ymax) = (ay.min(by), ay.max(by));
                if pt_x >= xmin && pt_x <= xmax && pt_y >= ymin && pt_y <= ymax {
                    return Some((d.id(), EditHandle::Body));
                }
            }
            Drawing::Text {
                anchor,
                width,
                text,
                font_size,
                ..
            } => {
                // Box height is estimated to match the rendered div, which
                // uses `text_size(font_size)` and `~1.5×` line-height inside
                // `px_1p5().py_0p5()` padding (12px horizontal, 4px vertical
                // total). Count explicit newlines AND wrap each paragraph
                // against `content_width = width - 12`, then total
                // line count drives the height. Mirrors the natural-grow
                // behaviour of the rendered div so the hit area expands with
                // visible text rather than under-estimating long content.
                let (ax, ay) = to_screen(*anchor);
                let h_est = estimate_text_box_height(text, *width, *font_size);
                // Right-edge resize handle: within `DRAWING_HANDLE_HIT_PX`
                // of the right edge, anywhere within the box's vertical
                // extent.
                if (pt_x - (ax + *width)).abs() <= DRAWING_HANDLE_HIT_PX
                    && pt_y >= ay - DRAWING_HANDLE_HIT_PX
                    && pt_y <= ay + h_est + DRAWING_HANDLE_HIT_PX
                {
                    return Some((d.id(), EditHandle::EndpointB));
                }
                if pt_x >= ax && pt_x <= ax + *width && pt_y >= ay && pt_y <= ay + h_est {
                    return Some((d.id(), EditHandle::Body));
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
                let (x0, _) = to_screen((*t0, *entry));
                let (x1, _) = to_screen((*t1, *entry));
                let (xmin, xmax) = (x0.min(x1), x0.max(x1));
                let (_, y_entry) = to_screen((*t0, *entry));
                let (_, y_tp) = to_screen((*t0, *take_profit));
                let (_, y_sl) = to_screen((*t0, *stop_loss));
                let ymin = y_entry.min(y_tp).min(y_sl);
                let ymax = y_entry.max(y_tp).max(y_sl);

                // Time-edge handles: vertical edges at xmin / xmax within
                // the vertical extent of the rect. Checked FIRST so the
                // visual handle dots at `(xmin/xmax, y_entry)` resolve to
                // a width drag — without this, the price-line check below
                // would intercept the click as `PositionEntry`.
                let in_y_extent =
                    pt_y >= ymin - DRAWING_STROKE_HIT_PX && pt_y <= ymax + DRAWING_STROKE_HIT_PX;
                if in_y_extent {
                    if (pt_x - xmin).abs() <= DRAWING_STROKE_HIT_PX {
                        return Some((d.id(), EditHandle::PositionStart));
                    }
                    if (pt_x - xmax).abs() <= DRAWING_STROKE_HIT_PX {
                        return Some((d.id(), EditHandle::PositionEnd));
                    }
                }

                // Price-line handles: any click on a price line (within the
                // horizontal extent of the rect, plus a small margin) grabs
                // that line. Test TP first → SL → entry, so when lines are
                // stacked the user gets the most "intentful" pick.
                let in_x_extent =
                    pt_x >= xmin - DRAWING_STROKE_HIT_PX && pt_x <= xmax + DRAWING_STROKE_HIT_PX;
                if in_x_extent {
                    if (pt_y - y_tp).abs() <= DRAWING_STROKE_HIT_PX {
                        return Some((d.id(), EditHandle::PositionTakeProfit));
                    }
                    if (pt_y - y_sl).abs() <= DRAWING_STROKE_HIT_PX {
                        return Some((d.id(), EditHandle::PositionStopLoss));
                    }
                    if (pt_y - y_entry).abs() <= DRAWING_STROKE_HIT_PX {
                        return Some((d.id(), EditHandle::PositionEntry));
                    }
                }

                // Body: anywhere inside the rect that didn't hit a handle.
                if pt_x >= xmin && pt_x <= xmax && pt_y >= ymin && pt_y <= ymax {
                    return Some((d.id(), EditHandle::Body));
                }
            }
            Drawing::AnchoredVwap { anchor, .. } => {
                // Rough hit test: a vertical strip at the anchor x — enough
                // for the user to right-click and Delete / hide. Proper
                // polyline hit-testing would re-walk the candle buffer; v1
                // keeps it cheap.
                let (ax, _) = to_screen(*anchor);
                if (pt_x - ax).abs() <= DRAWING_HANDLE_HIT_PX {
                    return Some((d.id(), EditHandle::Body));
                }
            }
            Drawing::Frvp { t0, t1, p0, p1, .. } => {
                // FRVP body hit. Two shapes:
                //  - Modern (both prices Some): rectangle hit-test like
                //    Rect — pt inside (xmin..xmax × ymin..ymax). Lets the
                //    user click anywhere inside the painted rect to drag
                //    it without snagging the underlying chart.
                //  - Legacy (price None): the original full-canvas-height
                //    bracket test — only x range matters because the
                //    bracket renders edge-to-edge vertically.
                let (x0, _) = to_screen((*t0, 0.0));
                let (x1, _) = to_screen((*t1, 0.0));
                let (xmin, xmax) = (x0.min(x1), x0.max(x1));
                match (p0, p1) {
                    (Some(a_price), Some(b_price)) => {
                        let (_, y0) = to_screen((*t0, *a_price));
                        let (_, y1) = to_screen((*t1, *b_price));
                        let (ymin, ymax) = (y0.min(y1), y0.max(y1));
                        if pt_x >= xmin - DRAWING_STROKE_HIT_PX
                            && pt_x <= xmax + DRAWING_STROKE_HIT_PX
                            && pt_y >= ymin - DRAWING_STROKE_HIT_PX
                            && pt_y <= ymax + DRAWING_STROKE_HIT_PX
                        {
                            return Some((d.id(), EditHandle::Body));
                        }
                    }
                    _ => {
                        if pt_x >= xmin - DRAWING_STROKE_HIT_PX
                            && pt_x <= xmax + DRAWING_STROKE_HIT_PX
                            && pt_y >= 0.0
                            && pt_y <= canvas_h
                        {
                            return Some((d.id(), EditHandle::Body));
                        }
                    }
                }
            }
        }
    }
    None
}
