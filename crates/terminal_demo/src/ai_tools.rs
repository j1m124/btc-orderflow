//! AI chat tool dispatcher + per-turn client context.
//!
//! `AiToolsBridge` implements two services:
//!
//! * [`ChatContextProvider`] — builds the small `client_context` payload
//!   (open charts + focused indicator) sent to the server each turn, and
//!   returns the list of enabled tool names (mode-gated to Charting).
//! * [`ToolDispatch`] — executes a `tool_use` block emitted by the model,
//!   returning a serializable `ToolOutcome` that becomes the matching
//!   `tool_result` in the next conversation turn.
//!
//! Tool semantics (matches the schemas registered on the server in
//! `aichat_tools.go`):
//!
//! * **Symbol resolution**: every tool accepts an optional `symbol`. If
//!   omitted, the symbol resolves to the currently-focused chart
//!   ([`LastFocusedChart`]). If the resolved symbol isn't an open chart,
//!   the tool returns an `is_error` outcome listing what *is* open.
//!
//! * **Time anchors are hidden** (design Q10). For drawings: ray and text
//!   anchor at the most recent candle's `open_time`; long-position spans
//!   from `now - 20 * tf_duration` to the most recent candle. The AI never
//!   sees these fields in its tool schema.
//!
//! * **AI provenance**: every drawing the bridge creates carries
//!   [`DrawingOrigin::Ai`] via [`DrawingService::add_with_origin`], which
//!   the object tree uses to badge AI levels.

use std::rc::Rc;

use gpui::{App, Entity, SharedString, WeakEntity};
use gpui_component::dock::DockArea;
use serde_json::{Value as JsonValue, json};

use crate::drawings::service::DrawingServiceHandle;
use crate::drawings::shapes::{
    DrawingOrigin, DrawingShape, HorizontalRayShape, LineRectShape, PositionShape, TextShape,
};
use crate::panels::{ContentPanel, LastFocusedChart};
use crate::persistence::Mode;
use crate::services::ai_chat::{
    CandleContext, ChartContext, ChatContextProvider, ChatContextProviderHandle, ClientContext,
    ToolDispatch, ToolDispatchHandle, ToolOutcome,
};
use crate::net::{CentoflowConfig, HttpClient};
use crate::services::market_data::{Candle, MarketDataServiceHandle, Session, Timeframe};

/// Canonical list of tool names the client knows how to execute. Must stay
/// in sync with the server's `toolRegistry` keys (drift = a 400 from the
/// server's `expandTools`). Mode-gated visibility is applied by
/// [`ChatContextProvider::enabled_tools`].
const TOOL_NAMES: &[&str] = &[
    // Data
    "get_candles",
    // Drawings — anchored at one point
    "add_horizontal_ray",
    "add_text",
    // Drawings — two-point geometry
    "add_line",
    "add_arrow",
    "add_rectangle",
    "add_fibonacci",
    // Trade visualizations
    "add_long_position",
    "add_short_position",
    // Chart mutations
    "set_symbol",
    "set_timeframe",
    "set_layout",
];

/// Maximum candle count returned by `get_candles` regardless of what the
/// model requests. Matches the documented cap in the server schema.
const MAX_CANDLES_PER_CALL: usize = 200;
/// Default candle count when the model omits the field.
const DEFAULT_CANDLES_PER_CALL: usize = 50;
/// How many bars the AI-placed long-position rectangle extends backward
/// from "now" — gives the user a recognisable retrospective frame.
const LONG_POSITION_LOOKBACK_BARS: i64 = 20;
/// How many recent bars the focused chart contributes to `client_context`
/// each turn so the AI can reason about price action without an extra
/// `get_candles` call. Matches `MAX_CANDLES_PER_CALL` so an explicit fetch
/// can't exceed what the default context already provides.
const DEFAULT_FOCUSED_CANDLES: usize = 200;

#[derive(Clone)]
pub struct AiToolsBridge {
    dock_area: WeakEntity<DockArea>,
}

impl AiToolsBridge {
    fn new(dock_area: WeakEntity<DockArea>) -> Self {
        Self { dock_area }
    }

    /// Every chart currently on the dock (any depth — splits and tabs).
    /// Empty if the dock has been dropped or no charts are open.
    fn open_charts(&self, cx: &App) -> Vec<Entity<ContentPanel>> {
        let Some(dock) = self.dock_area.upgrade() else {
            return Vec::new();
        };
        // `center()` returns &DockItem; clone the tree so we can release the
        // read borrow before walking.
        let root = dock.read(cx).center().clone();
        crate::workspace::collect_chart_panels(&root, cx)
    }

    fn focused_chart(&self, cx: &App) -> Option<Entity<ContentPanel>> {
        let global = cx.try_global::<LastFocusedChart>()?.0.clone();
        let weak = global.borrow().clone()?;
        weak.upgrade()
    }

    /// Resolve a tool's optional `symbol` argument to an open chart entity.
    /// If `symbol_opt` is `None`, falls back to the focused chart.
    fn resolve_chart(
        &self,
        symbol_opt: Option<&str>,
        cx: &App,
    ) -> Result<Entity<ContentPanel>, String> {
        let opens = self.open_charts(cx);
        if opens.is_empty() {
            return Err("no charts are open".to_string());
        }
        match symbol_opt {
            Some(target) => {
                for chart in &opens {
                    let panel = chart.read(cx);
                    if let Some(state) = panel.chart_state.as_ref() {
                        if state.symbol().as_ref().eq_ignore_ascii_case(target) {
                            return Ok(chart.clone());
                        }
                    }
                }
                let summary = open_chart_summary(&opens, cx);
                Err(format!(
                    "symbol '{target}' is not open. Currently open: {summary}"
                ))
            }
            None => self
                .focused_chart(cx)
                .or_else(|| opens.first().cloned())
                .ok_or_else(|| {
                    "no chart focused and no charts open; cannot infer target".to_string()
                }),
        }
    }
}

// ---------------------------------------------------------------------------
// ChatContextProvider impl
// ---------------------------------------------------------------------------

impl ChatContextProvider for AiToolsBridge {
    fn enabled_tools(&self, cx: &App) -> Vec<String> {
        if !is_charting_mode(cx) {
            return Vec::new();
        }
        TOOL_NAMES.iter().map(|s| s.to_string()).collect()
    }

    fn client_context(&self, cx: &App) -> Option<ClientContext> {
        if !is_charting_mode(cx) {
            return None;
        }
        let opens = self.open_charts(cx);
        if opens.is_empty() {
            return None;
        }
        let focused = self.focused_chart(cx);
        let md_handle = cx.try_global::<MarketDataServiceHandle>()?.0.clone();
        let md_read = md_handle.read(cx);

        let mut charts = Vec::new();
        for entity in &opens {
            let panel = entity.read(cx);
            let Some(state) = panel.chart_state.as_ref() else {
                continue;
            };
            let symbol = state.symbol().to_string();
            let tf = state.timeframe();
            let session = state.session();
            let snapshot = md_read.snapshot(symbol.as_str(), tf, session);
            let last_close = snapshot
                .and_then(|cs| cs.last())
                .map(|c| c.close)
                .unwrap_or(0.0);
            let focused_match = focused.as_ref().map(|f| f == entity).unwrap_or(false);
            // Only the focused chart contributes a full candle array.
            // Other open charts stay in the lightweight summary so the
            // per-turn token budget doesn't scale with chart count.
            let candles = if focused_match {
                snapshot
                    .map(|cs| {
                        let start = cs.len().saturating_sub(DEFAULT_FOCUSED_CANDLES);
                        cs[start..].iter().map(candle_to_context).collect()
                    })
                    .unwrap_or_default()
            } else {
                Vec::new()
            };
            charts.push(ChartContext {
                symbol,
                tf: tf.as_str().to_string(),
                last_close,
                focused: focused_match,
                candles,
            });
        }
        Some(ClientContext { charts })
    }
}

/// Map a market-data `Candle` into the per-turn `CandleContext` wire shape.
/// `ts` is the candle's `open_time` rendered as ISO 8601 UTC — matches the
/// format the AI passes back into drawing-tool time anchors.
fn candle_to_context(c: &Candle) -> CandleContext {
    use chrono::TimeZone as _;
    let ts = chrono::Utc
        .timestamp_millis_opt(c.open_time)
        .single()
        .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_default();
    CandleContext {
        t: c.open_time,
        ts,
        o: c.open,
        h: c.high,
        l: c.low,
        c: c.close,
        v: c.volume,
    }
}

fn is_charting_mode(cx: &App) -> bool {
    cx.try_global::<crate::panels::CurrentModeGlobal>()
        .map(|g| matches!(g.0, Mode::Charting))
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// ToolDispatch impl
// ---------------------------------------------------------------------------

impl ToolDispatch for AiToolsBridge {
    fn execute(
        &self,
        name: String,
        input: JsonValue,
        cx: gpui::AsyncApp,
    ) -> gpui::Task<ToolOutcome> {
        // `get_candles` is the only tool that may need to await REST work
        // (off-chart symbols / mismatched tf). Everything else is a quick
        // App read/write, wrapped in a single `cx.update(...)` call and
        // returned via `Task::ready`.
        if name == "get_candles" {
            let bridge = self.clone();
            return cx.spawn(async move |cx| {
                run_get_candles_async(&bridge, &input, cx).await
            });
        }
        let bridge = self.clone();
        let outcome = cx.update(|app| match name.as_str() {
            "add_horizontal_ray" => run_add_horizontal_ray(&bridge, &input, app),
            "add_text" => run_add_text(&bridge, &input, app),
            "add_long_position" => run_add_long_position(&bridge, &input, app),
            "add_line" => run_add_line(&bridge, &input, app),
            "add_arrow" => run_add_arrow(&bridge, &input, app),
            "add_rectangle" => run_add_rectangle(&bridge, &input, app),
            "add_fibonacci" => run_add_fibonacci(&bridge, &input, app),
            "add_short_position" => run_add_short_position(&bridge, &input, app),
            "set_symbol" => run_set_symbol(&bridge, &input, app),
            "set_timeframe" => run_set_timeframe(&bridge, &input, app),
            "set_layout" => run_set_layout(&bridge, &input, app),
            other => err(format!("unknown tool '{other}'")),
        });
        gpui::Task::ready(outcome)
    }
}

// ---------------------------------------------------------------------------
// Per-tool executors
// ---------------------------------------------------------------------------

fn run_add_horizontal_ray(
    bridge: &AiToolsBridge,
    input: &JsonValue,
    cx: &mut App,
) -> ToolOutcome {
    let Some(price) = input.get("price").and_then(|v| v.as_f64()) else {
        return err("missing required 'price' (number)");
    };
    let label = input
        .get("label")
        .and_then(|v| v.as_str())
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    let anchor_time_override = match read_iso_time_opt(input, "anchor_time") {
        Ok(t) => t,
        Err(e) => return err(e),
    };
    let symbol_opt = read_symbol_opt(input);
    let chart = match bridge.resolve_chart(symbol_opt.as_deref(), cx) {
        Ok(c) => c,
        Err(e) => return err(e),
    };
    let anchor = match chart_anchor(&chart, cx) {
        Ok(a) => a,
        Err(e) => return err(e),
    };
    let anchor_time = anchor_time_override.unwrap_or(anchor.anchor_time_ms);
    let shape = DrawingShape::HorizontalRay(HorizontalRayShape {
        anchor_time,
        anchor_price: price,
        text: label.clone(),
    });
    let id = insert_drawing(&anchor.symbol, shape, cx);
    ok(json!({
        "drawing_id": id,
        "symbol": anchor.symbol,
        "price": price,
        "label": label,
    }))
}

fn run_add_text(bridge: &AiToolsBridge, input: &JsonValue, cx: &mut App) -> ToolOutcome {
    let Some(price) = input.get("price").and_then(|v| v.as_f64()) else {
        return err("missing required 'price' (number)");
    };
    let Some(text) = input
        .get("text")
        .and_then(|v| v.as_str())
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    else {
        return err("missing required 'text' (non-empty string)");
    };
    let text = text.to_string();
    let anchor_time_override = match read_iso_time_opt(input, "anchor_time") {
        Ok(t) => t,
        Err(e) => return err(e),
    };
    let symbol_opt = read_symbol_opt(input);
    let chart = match bridge.resolve_chart(symbol_opt.as_deref(), cx) {
        Ok(c) => c,
        Err(e) => return err(e),
    };
    let anchor = match chart_anchor(&chart, cx) {
        Ok(a) => a,
        Err(e) => return err(e),
    };
    let anchor_time = anchor_time_override.unwrap_or(anchor.anchor_time_ms);
    let shape = DrawingShape::Text(TextShape {
        anchor_time,
        anchor_price: price,
        // Slightly above zero so the renderer's `max(0.0, w)` path doesn't
        // collapse it; the actual width is recomputed at paint time.
        width: 80.0,
        text: text.clone(),
    });
    let id = insert_drawing(&anchor.symbol, shape, cx);
    ok(json!({
        "drawing_id": id,
        "symbol": anchor.symbol,
        "price": price,
        "text": text,
    }))
}

fn run_add_long_position(
    bridge: &AiToolsBridge,
    input: &JsonValue,
    cx: &mut App,
) -> ToolOutcome {
    run_add_position(bridge, input, cx, PositionDirection::Long)
}

fn run_add_short_position(
    bridge: &AiToolsBridge,
    input: &JsonValue,
    cx: &mut App,
) -> ToolOutcome {
    run_add_position(bridge, input, cx, PositionDirection::Short)
}

#[derive(Clone, Copy)]
enum PositionDirection {
    Long,
    Short,
}

fn run_add_position(
    bridge: &AiToolsBridge,
    input: &JsonValue,
    cx: &mut App,
    direction: PositionDirection,
) -> ToolOutcome {
    let Some(entry) = input.get("entry").and_then(|v| v.as_f64()) else {
        return err("missing required 'entry' (number)");
    };
    let Some(tp) = input.get("take_profit").and_then(|v| v.as_f64()) else {
        return err("missing required 'take_profit' (number)");
    };
    let Some(sl) = input.get("stop_loss").and_then(|v| v.as_f64()) else {
        return err("missing required 'stop_loss' (number)");
    };
    match direction {
        PositionDirection::Long => {
            if !(tp > entry) {
                return err(format!(
                    "take_profit ({tp}) must be above entry ({entry}) for a long position"
                ));
            }
            if !(sl < entry) {
                return err(format!(
                    "stop_loss ({sl}) must be below entry ({entry}) for a long position"
                ));
            }
        }
        PositionDirection::Short => {
            if !(tp < entry) {
                return err(format!(
                    "take_profit ({tp}) must be below entry ({entry}) for a short position"
                ));
            }
            if !(sl > entry) {
                return err(format!(
                    "stop_loss ({sl}) must be above entry ({entry}) for a short position"
                ));
            }
        }
    }
    let t0_override = match read_iso_time_opt(input, "t0") {
        Ok(t) => t,
        Err(e) => return err(e),
    };
    let t1_override = match read_iso_time_opt(input, "t1") {
        Ok(t) => t,
        Err(e) => return err(e),
    };
    let symbol_opt = read_symbol_opt(input);
    let chart = match bridge.resolve_chart(symbol_opt.as_deref(), cx) {
        Ok(c) => c,
        Err(e) => return err(e),
    };
    let anchor = match chart_anchor(&chart, cx) {
        Ok(a) => a,
        Err(e) => return err(e),
    };
    let tf_ms = anchor.tf.duration_ms();
    let t1 = t1_override.unwrap_or(anchor.anchor_time_ms);
    let t0 = t0_override.unwrap_or_else(|| t1.saturating_sub(LONG_POSITION_LOOKBACK_BARS * tf_ms));
    let (t0, t1) = if t0 <= t1 { (t0, t1) } else { (t1, t0) };
    let pos = PositionShape {
        t0,
        t1,
        entry,
        take_profit: tp,
        stop_loss: sl,
    };
    let shape = match direction {
        PositionDirection::Long => DrawingShape::Long(pos),
        PositionDirection::Short => DrawingShape::Short(pos),
    };
    let id = insert_drawing(&anchor.symbol, shape, cx);
    ok(json!({
        "drawing_id": id,
        "symbol": anchor.symbol,
        "entry": entry,
        "take_profit": tp,
        "stop_loss": sl,
    }))
}

// ---------------------------------------------------------------------------
// Chart-mutation tools (set_symbol / set_timeframe / set_layout)
// ---------------------------------------------------------------------------

/// Resolve the chart to mutate: explicit `target_symbol` selects the first
/// open chart with that ticker; `None` selects the focused chart (or first
/// open if none is focused). Symbol match is case-insensitive.
fn resolve_target_chart(
    bridge: &AiToolsBridge,
    target_symbol: Option<&str>,
    cx: &App,
) -> Result<Entity<ContentPanel>, String> {
    // Reuse `resolve_chart` — it already implements the "match symbol /
    // fall back to focused" rule. The `Option<&str>` API matches 1:1.
    bridge.resolve_chart(target_symbol, cx)
}

fn run_set_symbol(bridge: &AiToolsBridge, input: &JsonValue, cx: &mut App) -> ToolOutcome {
    let Some(new_symbol) = input
        .get("symbol")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_uppercase())
        .filter(|s| !s.is_empty())
    else {
        return err("missing required 'symbol' (string)");
    };
    let target = input
        .get("target_symbol")
        .and_then(|v| v.as_str())
        .map(|s| s.trim())
        .filter(|s| !s.is_empty());
    let chart = match resolve_target_chart(bridge, target, cx) {
        Ok(c) => c,
        Err(e) => return err(e),
    };
    // Validate the new ticker against the loaded symbol universe. If the
    // service hasn't finished its initial fetch we skip validation rather
    // than block (better to mis-route a stray fetch than freeze the AI).
    if let Some(svc) = cx
        .try_global::<crate::services::symbols::SymbolsServiceHandle>()
        .map(|h| h.0.clone())
    {
        let read = svc.read(cx);
        if !read.symbols().is_empty() && read.meta(new_symbol.as_str()).is_none() {
            return err(format!("symbol '{new_symbol}' not found"));
        }
    }
    let (prior_target_symbol, prior_tf) = {
        let panel = chart.read(cx);
        let state = panel
            .chart_state
            .as_ref()
            .expect("resolved chart always has chart_state");
        (state.symbol().to_string(), state.timeframe().as_str().to_string())
    };
    if prior_target_symbol.eq_ignore_ascii_case(&new_symbol) {
        return ok(json!({
            "ok": true,
            "symbol": new_symbol,
            "noop": true,
            "prior": {
                "target_symbol": prior_target_symbol,
                "symbol": prior_target_symbol,
                "tf": prior_tf,
            }
        }));
    }
    let weak = chart.downgrade();
    let sym_for_call = new_symbol.clone();
    if let Some(entity) = weak.upgrade() {
        entity.update(cx, |panel, cx| {
            panel.switch_chart_symbol(sym_for_call.as_str(), cx);
        });
    }
    ok(json!({
        "ok": true,
        "symbol": new_symbol,
        "prior": {
            "target_symbol": prior_target_symbol,
            "symbol": prior_target_symbol,
            "tf": prior_tf,
        }
    }))
}

fn run_set_timeframe(bridge: &AiToolsBridge, input: &JsonValue, cx: &mut App) -> ToolOutcome {
    let Some(tf_str) = input.get("tf").and_then(|v| v.as_str()) else {
        return err("missing required 'tf' (string, e.g. '1m', '5m', '1h', '1d')");
    };
    let Some(new_tf) = Timeframe::from_str(tf_str.trim()) else {
        return err(format!(
            "unknown timeframe '{tf_str}' (valid: 1m, 5m, 15m, 1h, 4h, 1d)"
        ));
    };
    let target = input
        .get("target_symbol")
        .and_then(|v| v.as_str())
        .map(|s| s.trim())
        .filter(|s| !s.is_empty());
    let chart = match resolve_target_chart(bridge, target, cx) {
        Ok(c) => c,
        Err(e) => return err(e),
    };
    let (prior_target_symbol, prior_tf) = {
        let panel = chart.read(cx);
        let state = panel
            .chart_state
            .as_ref()
            .expect("resolved chart always has chart_state");
        (state.symbol().to_string(), state.timeframe())
    };
    if prior_tf == new_tf {
        return ok(json!({
            "ok": true,
            "tf": new_tf.as_str(),
            "noop": true,
            "prior": {
                "target_symbol": prior_target_symbol,
                "tf": prior_tf.as_str(),
            }
        }));
    }
    chart.update(cx, |panel, cx| {
        panel.switch_chart_timeframe(new_tf, cx);
    });
    ok(json!({
        "ok": true,
        "tf": new_tf.as_str(),
        "prior": {
            "target_symbol": prior_target_symbol,
            "tf": prior_tf.as_str(),
        }
    }))
}

fn run_set_layout(bridge: &AiToolsBridge, input: &JsonValue, cx: &mut App) -> ToolOutcome {
    let Some(layout_id) = input
        .get("layout")
        .and_then(|v| v.as_str())
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    else {
        return err("missing required 'layout' (one of: one, two_stacked, two_side, two_by_two)");
    };
    let Some(_layout) = crate::persistence::ChartLayout::from_id(layout_id) else {
        return err(format!(
            "unknown layout '{layout_id}' (valid: one, two_stacked, two_side, two_by_two)"
        ));
    };
    let Some(ws_handle) = cx
        .try_global::<crate::workspace::TerminalWorkspaceHandle>()
        .map(|h| h.0.clone())
    else {
        return err("workspace handle not available");
    };
    let Some(workspace) = ws_handle.upgrade() else {
        return err("workspace has been dropped");
    };
    // Capture prior layout + symbols so the Undo path can restore the
    // exact state the user had. Bridge's `open_charts` walks the dock for
    // us — no private-field access into TerminalWorkspace needed.
    let prior_layout = workspace.read(cx).chart_layout();
    let prior_symbols: Vec<String> = bridge
        .open_charts(cx)
        .iter()
        .filter_map(|p| {
            p.read(cx)
                .chart_state
                .as_ref()
                .map(|s| s.symbol().to_string())
        })
        .collect();
    if prior_layout.id() == layout_id {
        return ok(json!({
            "ok": true,
            "layout": layout_id,
            "noop": true,
            "prior": { "layout": prior_layout.id(), "symbols": prior_symbols }
        }));
    }
    // The existing `ApplyChartLayout` handler is window-scoped, so we
    // dispatch through the active window. Falls back gracefully if no
    // window is active (shouldn't happen in normal operation).
    let dispatched = if let Some(handle) = cx.active_window() {
        handle
            .update(cx, |_, window, cx| {
                window.dispatch_action(
                    Box::new(crate::top_bar::ApplyChartLayout(SharedString::from(
                        layout_id.to_string(),
                    ))),
                    cx,
                );
            })
            .is_ok()
    } else {
        false
    };
    if !dispatched {
        return err("could not dispatch layout change (no active window)");
    }
    ok(json!({
        "ok": true,
        "layout": layout_id,
        "prior": { "layout": prior_layout.id(), "symbols": prior_symbols }
    }))
}

// ---------------------------------------------------------------------------
// Two-point drawings (line / arrow / rectangle / fibonacci)
// ---------------------------------------------------------------------------

fn run_add_line(bridge: &AiToolsBridge, input: &JsonValue, cx: &mut App) -> ToolOutcome {
    run_add_two_point(bridge, input, cx, "line", DrawingShape::Line)
}

fn run_add_arrow(bridge: &AiToolsBridge, input: &JsonValue, cx: &mut App) -> ToolOutcome {
    run_add_two_point(bridge, input, cx, "arrow", DrawingShape::Arrow)
}

fn run_add_rectangle(bridge: &AiToolsBridge, input: &JsonValue, cx: &mut App) -> ToolOutcome {
    run_add_two_point(bridge, input, cx, "rectangle", DrawingShape::Rect)
}

fn run_add_fibonacci(bridge: &AiToolsBridge, input: &JsonValue, cx: &mut App) -> ToolOutcome {
    run_add_two_point(bridge, input, cx, "fibonacci", DrawingShape::Fibonacci)
}

/// Shared executor for the four LineRectShape-backed drawing tools. Each
/// caller supplies a constructor that wraps the shared shape in the right
/// `DrawingShape` variant. Default `a_time` is ~20 bars before the latest
/// candle (gives a recognisable left endpoint) and default `b_time` is the
/// latest candle's open_time.
fn run_add_two_point(
    bridge: &AiToolsBridge,
    input: &JsonValue,
    cx: &mut App,
    kind_label: &str,
    wrap: fn(LineRectShape) -> DrawingShape,
) -> ToolOutcome {
    let Some(a_price) = input.get("a_price").and_then(|v| v.as_f64()) else {
        return err(format!(
            "missing required 'a_price' (number) for {kind_label}"
        ));
    };
    let Some(b_price) = input.get("b_price").and_then(|v| v.as_f64()) else {
        return err(format!(
            "missing required 'b_price' (number) for {kind_label}"
        ));
    };
    let a_override = match read_iso_time_opt(input, "a_time") {
        Ok(t) => t,
        Err(e) => return err(e),
    };
    let b_override = match read_iso_time_opt(input, "b_time") {
        Ok(t) => t,
        Err(e) => return err(e),
    };
    let symbol_opt = read_symbol_opt(input);
    let chart = match bridge.resolve_chart(symbol_opt.as_deref(), cx) {
        Ok(c) => c,
        Err(e) => return err(e),
    };
    let anchor = match chart_anchor(&chart, cx) {
        Ok(a) => a,
        Err(e) => return err(e),
    };
    let tf_ms = anchor.tf.duration_ms();
    let b_time = b_override.unwrap_or(anchor.anchor_time_ms);
    let a_time =
        a_override.unwrap_or_else(|| b_time.saturating_sub(LONG_POSITION_LOOKBACK_BARS * tf_ms));
    let shape = wrap(LineRectShape {
        a_time,
        a_price,
        b_time,
        b_price,
    });
    let id = insert_drawing(&anchor.symbol, shape, cx);
    ok(json!({
        "drawing_id": id,
        "symbol": anchor.symbol,
        "kind": kind_label,
        "a_price": a_price,
        "b_price": b_price,
    }))
}

/// Resolution of `get_candles` arguments under a single sync `cx.update`,
/// before any await. Determines whether the in-memory cache can answer the
/// request directly or whether we need a stateless REST fetch.
enum CandlesResolve {
    /// Pre-built result, no REST needed.
    Cached(JsonValue),
    /// REST fetch required: symbol isn't open, tf differs, or cache is
    /// empty. We carry the http client + config out of the sync section
    /// so the async REST call doesn't need to re-touch `App`.
    Fetch {
        client: reqwest::Client,
        cfg: CentoflowConfig,
        symbol: String,
        tf: Timeframe,
        session: Session,
        count: usize,
    },
    /// Bad input or missing global — return as-is.
    Error(ToolOutcome),
}

async fn run_get_candles_async(
    bridge: &AiToolsBridge,
    input: &JsonValue,
    cx: &mut gpui::AsyncApp,
) -> ToolOutcome {
    let symbol_opt = read_symbol_opt(input);
    let tf_opt = input
        .get("tf")
        .and_then(|v| v.as_str())
        .and_then(Timeframe::from_str);
    let count_req = input
        .get("count")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .unwrap_or(DEFAULT_CANDLES_PER_CALL)
        .clamp(1, MAX_CANDLES_PER_CALL);

    let resolved = cx.update(|app| {
        // First try to satisfy from the open-chart snapshot. Same path as
        // before — for the focused-chart current-tf case this stays sync.
        if let Ok(chart) = bridge.resolve_chart(symbol_opt.as_deref(), app) {
            let (chart_symbol, chart_tf, chart_session) = {
                let panel = chart.read(app);
                let state = panel
                    .chart_state
                    .as_ref()
                    .expect("resolved chart always has chart_state");
                (
                    state.symbol().to_string(),
                    state.timeframe(),
                    state.session(),
                )
            };
            let tf = tf_opt.unwrap_or(chart_tf);
            // Cache hit requires both the chart's tf and the request to
            // agree. Anything else falls through to REST.
            if tf == chart_tf {
                if let Some(md_handle) =
                    app.try_global::<MarketDataServiceHandle>().map(|h| h.0.clone())
                {
                    let cached: Vec<Candle> = md_handle
                        .read(app)
                        .snapshot(chart_symbol.as_str(), tf, chart_session)
                        .map(|cs| {
                            let start = cs.len().saturating_sub(count_req);
                            cs[start..].to_vec()
                        })
                        .unwrap_or_default();
                    if !cached.is_empty() {
                        return CandlesResolve::Cached(serialize_candles(
                            &chart_symbol,
                            tf,
                            &cached,
                        ));
                    }
                }
            }
            // Fall through to REST with the chart's session preserved.
            return collect_fetch_args(
                app,
                symbol_opt.unwrap_or(chart_symbol),
                tf,
                chart_session,
                count_req,
            );
        }
        // No matching open chart. Off-chart fetch with sensible defaults.
        let symbol = match symbol_opt {
            Some(s) => s,
            None => {
                return CandlesResolve::Error(err(
                    "no chart focused and no 'symbol' provided",
                ));
            }
        };
        let tf = tf_opt.unwrap_or(Timeframe::H1);
        collect_fetch_args(app, symbol, tf, Session::Regular, count_req)
    });

    match resolved {
        CandlesResolve::Cached(v) => ok(v),
        CandlesResolve::Error(e) => e,
        CandlesResolve::Fetch {
            client,
            cfg,
            symbol,
            tf,
            session,
            count,
        } => {
            match crate::services::market_data::fetch_candles(
                &client, &cfg, &symbol, tf, session, count, None,
            )
            .await
            {
                Ok(candles) if !candles.is_empty() => ok(serialize_candles(&symbol, tf, &candles)),
                Ok(_) => err(format!(
                    "no candles returned for {symbol} @ {}",
                    tf.as_str()
                )),
                Err(e) => err(format!("centoflow /v1/candles failed: {e:#}")),
            }
        }
    }
}

fn collect_fetch_args(
    app: &App,
    symbol: String,
    tf: Timeframe,
    session: Session,
    count: usize,
) -> CandlesResolve {
    let Some(client) = app.try_global::<HttpClient>().map(|h| h.0.clone()) else {
        return CandlesResolve::Error(err("http client not available"));
    };
    let cfg = app.global::<CentoflowConfig>().clone();
    CandlesResolve::Fetch {
        client,
        cfg,
        symbol,
        tf,
        session,
        count,
    }
}

fn serialize_candles(symbol: &str, tf: Timeframe, candles: &[Candle]) -> JsonValue {
    let items: Vec<JsonValue> = candles
        .iter()
        .map(|c| {
            json!({
                "t": c.open_time,
                "ts": format_iso_utc(c.open_time),
                "o": c.open,
                "h": c.high,
                "l": c.low,
                "c": c.close,
                "v": c.volume,
            })
        })
        .collect();
    json!({
        "symbol": symbol,
        "tf": tf.as_str(),
        "count": items.len(),
        "candles": items,
    })
}

fn format_iso_utc(ms: i64) -> String {
    use chrono::TimeZone as _;
    chrono::Utc
        .timestamp_millis_opt(ms)
        .single()
        .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

struct ChartAnchor {
    symbol: String,
    tf: Timeframe,
    /// `open_time` of the most-recent candle in the snapshot. Used as the
    /// anchor time for all drawings.
    anchor_time_ms: i64,
}

fn chart_anchor(chart: &Entity<ContentPanel>, cx: &App) -> Result<ChartAnchor, String> {
    let (symbol, tf, session) = {
        let panel = chart.read(cx);
        let state = panel
            .chart_state
            .as_ref()
            .ok_or_else(|| "resolved chart has no chart_state".to_string())?;
        (
            state.symbol().to_string(),
            state.timeframe(),
            state.session(),
        )
    };
    let md = cx
        .try_global::<MarketDataServiceHandle>()
        .ok_or_else(|| "market data service not available".to_string())?
        .0
        .clone();
    let anchor_time_ms = md
        .read(cx)
        .snapshot(symbol.as_str(), tf, session)
        .and_then(|cs| cs.last())
        .map(|c| c.open_time)
        .ok_or_else(|| format!("no candles loaded for {symbol} @ {}", tf.as_str()))?;
    Ok(ChartAnchor {
        symbol,
        tf,
        anchor_time_ms,
    })
}

fn insert_drawing(symbol: &str, shape: DrawingShape, cx: &mut App) -> u64 {
    let drawings = cx.global::<DrawingServiceHandle>().0.clone();
    drawings.update(cx, |s, cx| {
        s.add_with_origin(SharedString::from(symbol.to_string()), shape, DrawingOrigin::Ai, cx)
    })
}

fn open_chart_summary(opens: &[Entity<ContentPanel>], cx: &App) -> String {
    let parts: Vec<String> = opens
        .iter()
        .filter_map(|e| {
            let p = e.read(cx);
            p.chart_state
                .as_ref()
                .map(|s| format!("{} {}", s.symbol(), s.timeframe().as_str()))
        })
        .collect();
    if parts.is_empty() {
        "(none)".to_string()
    } else {
        parts.join(", ")
    }
}

fn read_symbol_opt(input: &JsonValue) -> Option<String> {
    input
        .get("symbol")
        .and_then(|v| v.as_str())
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

/// Parse an optional ISO 8601 UTC timestamp field on a drawing tool input.
///
/// Returns `Ok(Some(ms))` for a valid ISO string, `Ok(None)` when the field
/// is absent or `null` (callers fall back to "latest candle"), and `Err`
/// when the field is present but malformed (callers surface the message as
/// an is_error tool_result so the model can correct the format).
fn read_iso_time_opt(input: &JsonValue, field: &str) -> Result<Option<i64>, String> {
    let Some(v) = input.get(field) else {
        return Ok(None);
    };
    if v.is_null() {
        return Ok(None);
    }
    let Some(s) = v.as_str() else {
        return Err(format!(
            "'{field}' must be an ISO 8601 UTC string like '2026-05-29T14:30:00Z'"
        ));
    };
    let s = s.trim();
    if s.is_empty() {
        return Ok(None);
    }
    // Accept the strict RFC 3339 form (covers `Z` and `+HH:MM`). We only
    // care about epoch ms; the original offset is discarded.
    match chrono::DateTime::parse_from_rfc3339(s) {
        Ok(dt) => Ok(Some(dt.timestamp_millis())),
        Err(e) => Err(format!(
            "'{field}' is not valid ISO 8601: {e} (got '{s}')"
        )),
    }
}

fn err(msg: impl Into<String>) -> ToolOutcome {
    ToolOutcome {
        content: msg.into(),
        is_error: true,
    }
}

fn ok(value: JsonValue) -> ToolOutcome {
    ToolOutcome {
        content: value.to_string(),
        is_error: false,
    }
}

// ---------------------------------------------------------------------------
// Mutation undo — invoked by the chip's Undo button. Reads the captured
// `prior` snapshot from the tool_result and dispatches the inverse.
// ---------------------------------------------------------------------------

/// Reverse a previously-executed `set_*` mutation by reading the `prior`
/// JSON from the original tool_result and applying the inverse. Best-effort:
/// failures are silent (the chart simply doesn't move) because Undo is a
/// convenience UI, not a transactional primitive.
pub fn undo_mutation(name: &str, prior: &JsonValue, window: &mut gpui::Window, cx: &mut App) {
    match name {
        "set_symbol" => {
            let Some(target) = prior
                .get("target_symbol")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
            else {
                return;
            };
            let Some(restore) = prior
                .get("symbol")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
            else {
                return;
            };
            let Some(chart) = find_chart_by_symbol(target.as_str(), cx) else {
                return;
            };
            chart.update(cx, |panel, cx| {
                panel.switch_chart_symbol(restore.as_str(), cx);
            });
        }
        "set_timeframe" => {
            let Some(target) = prior
                .get("target_symbol")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
            else {
                return;
            };
            let Some(tf_str) = prior.get("tf").and_then(|v| v.as_str()) else {
                return;
            };
            let Some(tf) = Timeframe::from_str(tf_str) else {
                return;
            };
            let Some(chart) = find_chart_by_symbol(target.as_str(), cx) else {
                return;
            };
            chart.update(cx, |panel, cx| {
                panel.switch_chart_timeframe(tf, cx);
            });
        }
        "set_layout" => {
            let Some(layout_id) = prior.get("layout").and_then(|v| v.as_str()) else {
                return;
            };
            window.dispatch_action(
                Box::new(crate::top_bar::ApplyChartLayout(SharedString::from(
                    layout_id.to_string(),
                ))),
                cx,
            );
        }
        _ => {}
    }
}

/// Walk the dock for the first open chart panel whose symbol matches `target`
/// (case-insensitive). Used by [`undo_mutation`] so the chip can locate the
/// pre-mutation target chart without depending on the AI bridge directly.
fn find_chart_by_symbol(target: &str, cx: &App) -> Option<Entity<ContentPanel>> {
    let dock = cx.try_global::<crate::panels::DockAreaHandle>()?.0.clone();
    let dock_entity = dock.upgrade()?;
    let root = dock_entity.read(cx).center().clone();
    for chart in crate::workspace::collect_chart_panels(&root, cx) {
        if let Some(state) = chart.read(cx).chart_state.as_ref() {
            if state.symbol().as_ref().eq_ignore_ascii_case(target) {
                return Some(chart);
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Registration — call once from workspace setup.
// ---------------------------------------------------------------------------

pub fn init(dock_area: WeakEntity<DockArea>, cx: &mut App) {
    let bridge = Rc::new(AiToolsBridge::new(dock_area));
    let ctx_handle: Rc<dyn ChatContextProvider> = bridge.clone();
    let disp_handle: Rc<dyn ToolDispatch> = bridge;
    cx.set_global(ChatContextProviderHandle(ctx_handle));
    cx.set_global(ToolDispatchHandle(disp_handle));
}
