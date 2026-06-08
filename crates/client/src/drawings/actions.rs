//! Workspace-dispatched actions for the drawing subsystem. Carrying string ids
//! (not the enum) keeps them serializable via `no_json` and decouples action
//! handlers from `Tool`/`Timeframe` enum churn.

use gpui::{Action, SharedString};
use serde::Deserialize;

/// Set the global [`Tool`](super::tool::Tool). Carries `Tool::id()` so the
/// dispatcher can look up the enum value without serializing the enum itself.
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = client, no_json)]
pub struct SetActiveTool(pub SharedString);

/// Workspace-wide. Deletes the globally-selected drawing (if any). Rebinds the
/// old `chart`-scoped binding to a no-context binding so Delete works from the
/// chart canvas and the Objects popover alike.
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = client, no_json)]
pub struct DeleteSelectedDrawing;

/// Wipes every drawing on the focused chart's symbol. Surfaces from the
/// Objects popover footer (no confirm) and from the chart canvas's right-click
/// menu over empty area.
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = client, no_json)]
pub struct ClearChartDrawings;

/// Wipes every drawing on every symbol. Surfaces from the Objects popover
/// footer and prompts a confirm dialog first.
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = client, no_json)]
pub struct ClearAllDrawings;

/// Toggle the `hidden` flag on a drawing. `(symbol, id)`.
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = client, no_json)]
pub struct ToggleDrawingHidden {
    pub symbol: SharedString,
    pub id: u64,
}

/// Toggle a single timeframe in a drawing's `tf_filter`. `tf` is
/// `Timeframe::as_str()`.
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = client, no_json)]
pub struct ToggleDrawingTfFilter {
    pub symbol: SharedString,
    pub id: u64,
    pub tf: SharedString,
}

/// Reset the `tf_filter` on a drawing back to "visible on all TFs".
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = client, no_json)]
pub struct ResetDrawingTfFilter {
    pub symbol: SharedString,
    pub id: u64,
}

/// Delete a single drawing. `(symbol, id)`. Surfaces from the per-drawing
/// right-click menu (chart canvas) and from the object-tree row.
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = client, no_json)]
pub struct DeleteDrawing {
    pub symbol: SharedString,
    pub id: u64,
}

/// Select a drawing globally. `(symbol, id)`. Object-tree row clicks dispatch
/// this; chart-canvas clicks set it directly via the service.
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = client, no_json)]
pub struct SelectDrawing {
    pub symbol: SharedString,
    pub id: u64,
}

/// Open a dialog to edit the optional label on a horizontal-ray drawing.
/// Workspace-level handler — the chart's right-click context menu dispatches
/// this when the right-click target is a `HorizontalRay`.
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = client, no_json)]
pub struct EditHorizontalRayText {
    pub symbol: SharedString,
    pub id: u64,
}

/// Toggle the `locked` flag on a drawing. `(symbol, id)`. Surfaced from the
/// floating settings strip.
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = client, no_json)]
pub struct ToggleDrawingLocked {
    pub symbol: SharedString,
    pub id: u64,
}

/// Open a dialog to edit the per-shape secondary label. Routes to the
/// shape's `label`/`text` field via `DrawingService::set_label`; no-op for
/// `Text` shapes (whose text content IS the label).
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = client, no_json)]
pub struct EditDrawingLabel {
    pub symbol: SharedString,
    pub id: u64,
}

/// Open the floating per-drawing settings window. Dispatched from the
/// gear button on the floating settings strip. Singleton — a re-dispatch
/// with a different `(symbol, id)` retargets the existing window.
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = client, no_json)]
pub struct OpenDrawingSettings {
    pub symbol: SharedString,
    pub id: u64,
}

/// Drop the global drawing selection (closes the floating settings strip).
/// Dispatched from ESC, empty-canvas clicks, and TF-mismatch auto-deselect.
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = client, no_json)]
pub struct DeselectDrawing;
