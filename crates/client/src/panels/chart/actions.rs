//! gpui action types dispatched at the chart panel. Handled on `ContentPanel`
//! in `panels.rs`; registration is by `#[action(namespace = client)]`, not
//! module path, so they live here and are re-exported from the chart facade.

use gpui::{Action, SharedString};
use serde::Deserialize;

/// Switch the chart's timeframe. Carries the timeframe's wire string (`1m`,
/// `5m`, …); the handler parses it back to a [`Timeframe`]. Dispatched from the
/// chart's timeframe-selector dropdown, scoped to this panel's focus so it
/// dispatches up through *this* panel (not whichever element had focus when the
/// menu opened), keeping multiple Chart panels independent.
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = client, no_json)]
pub struct ChangeChartTimeframe(pub SharedString);

/// Switch the chart's render kind (`candlestick`/`cluster`/`profile`).
/// Dispatched from the header render-kind dropdown next to the TF
/// selector. Carries the [`RenderKind::as_id`] string; the handler
/// parses it back via [`RenderKind::from_id`] and routes through
/// `ContentPanel::switch_chart_render` so the footprint subscription
/// lifecycle is re-evaluated atomically with the state change.
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = client, no_json)]
pub struct ChangeChartRender(pub SharedString);

/// Toggle the eye on the synthetic render chip — suppresses the main
/// render layer (candles / cells / wireframes) without dropping any
/// subscription. Overlays and drawings keep painting. The chip is
/// special-cased (not an `IndicatorInstance`) so it carries no id;
/// the handler reads the chart's current `render_visible` flag and
/// flips it.
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = client, no_json)]
pub struct ToggleChartRenderVisible;

/// Switch the chart's volume display unit (`coin` / `usd`). Dispatched
/// from the header volume-unit dropdown (between the render-kind selector
/// and the `+ Indicator` button). Carries the wire id; the handler parses
/// it back and routes through `ContentPanel::set_chart_volume_unit` so
/// indicators (Volume / Volume Delta / CVD) recompute and the footprint
/// paint pipeline picks up the new unit in one shot.
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = client, no_json)]
pub struct ChangeChartVolumeUnit(pub SharedString);

// Drawing-delete + clear actions now live in `crate::drawings::actions` so
// they're shared with workspace-wide key bindings and the Objects popover.

/// Right-click context-menu actions on an indicator chip. The instance id
/// is carried inline; the handler lives on `ContentPanel` so the action
/// naturally routes to the chart panel hosting the chip (no
/// `LastFocusedChart` round-trip needed — context menus bubble from the
/// element they were opened on).
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = client, no_json)]
pub struct MoveIndicatorPaneUp(pub u64);

#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = client, no_json)]
pub struct MoveIndicatorPaneDown(pub u64);

#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = client, no_json)]
pub struct ToggleIndicatorHidden(pub u64);

#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = client, no_json)]
pub struct RemoveIndicator(pub u64);

/// Reset the chart's viewport to its default trailing window (x-axis) and
/// re-enable y-auto-fit (price axis). Dispatched from the chart's right-click
/// context menu — mirrors what double-clicking either axis would do, but in
/// one shot.
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = client, no_json)]
pub struct ResetChartScale;

/// Snap the viewport so the latest bar lands at the default trailing offset
/// AND enable sticky-tail mode: subsequent new bars advance `view_start` so
/// the chart stays glued to the live edge. Sticky stays on until the user
/// pans the canvas horizontally. Dispatched from the floating bottom-right
/// button and from the right-click context menu.
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = client, no_json)]
pub struct GoToLatest;
