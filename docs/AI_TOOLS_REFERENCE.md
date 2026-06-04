# AI Chat Tools Reference

This document describes every tool the AI assistant can call inside the trading terminal. It is the canonical reference used to assemble the system prompt and to register schemas on the server (`centoflow-server/aichat_tools.go`).

The server owns the canonical JSON Schema for each tool; the client owns the executors and ships an `enabled_tools: [names]` list per request. Tool names and field names below must stay in sync with both.

## Conventions

- **Time anchors**: ISO 8601 UTC strings (`"2026-05-29T14:30:00Z"`). The dispatcher parses to epoch ms via `chrono::DateTime::parse_from_rfc3339`. Any wall-clock ms is valid — the chart interpolates fractionally between bars; no snap-to-candle required.
- **Time is optional on every drawing tool.** When omitted, the dispatcher uses the focused chart's most-recent candle `open_time`.
- **Prices** are raw floats in the symbol's quote currency.
- **`symbol`** is optional on every tool. Omitted → focused chart. Provided → resolves to the first open chart matching that symbol; if no open chart matches AND the tool reads data (`get_candles`), falls back to a stateless REST fetch.
- **Mode gating**: all drawing tools and chart-mutation tools are only enabled in Charting mode. The server applies the `enabled_tools` filter; the client also mode-gates via `AiToolsBridge::enabled_tools`.
- **Error contract**: every tool returns `{ content, is_error }`. `is_error: true` becomes Anthropic `tool_result.is_error`; the model sees it and recovers.
- **Provenance**: every drawing the AI creates carries `DrawingOrigin::Ai` and renders identically to user-drawn shapes; the object tree badges them as AI-created.

## Per-turn context

Every request includes a `client_context` payload assembled by `AiToolsBridge::client_context`:

```json
{
  "charts": [
    {
      "symbol": "SPY",
      "tf": "5m",
      "last_close": 587.32,
      "focused": true,
      "candles": [
        {"t": 1719829800000, "ts": "2026-05-29T14:30:00Z", "o": 587.10, "h": 587.45, "l": 587.05, "c": 587.32, "v": 12480},
        ...
      ]
    },
    { "symbol": "QQQ", "tf": "15m", "last_close": 503.91, "focused": false }
  ]
}
```

- Only the **focused chart** carries a `candles` array (~200 bars by default).
- Other open charts contribute symbol/tf/last_close so the model knows what else is on screen.
- Each candle carries both `t` (epoch ms) and `ts` (ISO UTC). The AI should echo `ts` directly when anchoring a drawing.

## Tool catalog

### Data tools

#### `get_candles`

Read candles for any symbol/timeframe. If the symbol is open in a chart and the requested tf matches its current tf, returns from the in-memory cache. Otherwise hits the stateless REST helper (`fetch_candles`) — no WebSocket subscription side-effect.

**Input**:
- `symbol` (string, optional) — defaults to focused chart's symbol.
- `tf` (string, optional) — one of `"1m"`, `"5m"`, `"15m"`, `"1h"`, `"4h"`, `"1d"`. Defaults to focused chart's tf.
- `count` (integer, optional) — defaults to 50, clamped to `[1, 200]`.

**Returns** (`is_error: false`):
```json
{ "symbol": "AAPL", "tf": "1h", "count": 100,
  "candles": [ {"t":..., "ts":"...", "o":..., "h":..., "l":..., "c":..., "v":...}, ... ] }
```

**Errors**: `"symbol 'XYZ' not found"`, `"no candles loaded for X @ tf"`, transport errors as plain text.

### Drawing tools

All drawing tools accept an optional `symbol` (focused chart by default) and return:

```json
{ "drawing_id": 4271, "symbol": "SPY", "prior": null, ... }
```

The chip's Undo button uses `drawing_id` + `symbol` to delete the drawing.

#### `add_horizontal_ray`

Horizontal line at a fixed price, anchored at one point, extending rightward to the chart's right edge.

**Input**:
- `price` (number, required)
- `label` (string, optional) — rendered at top-right of the ray.
- `anchor_time` (ISO 8601 UTC, optional) — defaults to most-recent candle.
- `symbol` (string, optional).

#### `add_text`

Text label anchored at one point.

**Input**:
- `price` (number, required)
- `text` (string, required, non-empty)
- `anchor_time` (ISO 8601 UTC, optional) — defaults to most-recent candle.
- `symbol` (string, optional).

#### `add_line`

Two-point line segment.

**Input**:
- `a_price` (number, required)
- `b_price` (number, required)
- `a_time` (ISO 8601 UTC, optional) — defaults to ~20 bars before most-recent.
- `b_time` (ISO 8601 UTC, optional) — defaults to most-recent candle.
- `symbol` (string, optional).

#### `add_arrow`

Line with an arrowhead at endpoint `b`. Same schema as `add_line`.

#### `add_rectangle`

Rectangle whose opposite corners are `(a_time, a_price)` and `(b_time, b_price)`. Same schema as `add_line`.

#### `add_fibonacci`

Fibonacci retracement levels between two price points across a time range. The renderer draws the standard 0/23.6/38.2/50/61.8/78.6/100% levels between `a_price` and `b_price`. Same schema as `add_line`.

#### `add_long_position`

Long trade visualization: entry, take-profit, stop-loss prices over a time range.

**Input**:
- `entry` (number, required)
- `take_profit` (number, required) — must be `> entry`
- `stop_loss` (number, required) — must be `< entry`
- `t0` (ISO 8601 UTC, optional) — defaults to ~20 bars before most-recent.
- `t1` (ISO 8601 UTC, optional) — defaults to most-recent candle.
- `symbol` (string, optional).

#### `add_short_position`

Short trade visualization. Mirror of `add_long_position`.

**Input**:
- `entry` (number, required)
- `take_profit` (number, required) — must be `< entry`
- `stop_loss` (number, required) — must be `> entry`
- `t0`, `t1`, `symbol` — same semantics as `add_long_position`.

### Chart mutation tools

These mutate the user's UI. The AI should use them when the user explicitly asks ("switch the SPY chart to 1h", "open a 2×2 layout"). They are mode-gated to Charting.

All mutations return:
```json
{ "ok": true, "prior": { ... } }
```
where `prior` carries the state before the mutation, so the chip's Undo button can dispatch the inverse.

#### `set_symbol`

Retarget an existing chart to a new symbol.

**Input**:
- `symbol` (string, required) — validated against `SymbolService`; unknown ticker errors out.
- `target_symbol` (string, optional) — selects which chart to mutate (first match in dock-walk order). Defaults to focused chart.

**Returns**:
```json
{ "ok": true, "prior": { "target_symbol": "SPY", "symbol": "SPY", "tf": "5m" } }
```

**Errors**: `"symbol 'XYZ' not found"`, `"no chart matching '...'  is open"`.

#### `set_timeframe`

Change an existing chart's timeframe.

**Input**:
- `tf` (string, required) — one of the allowlisted `Timeframe::from_str` values.
- `target_symbol` (string, optional) — defaults to focused chart.

**Returns**:
```json
{ "ok": true, "prior": { "target_symbol": "SPY", "tf": "5m" } }
```

**Errors**: `"unknown timeframe '...'"`, `"no chart matching '...' is open"`.

#### `set_layout`

Switch the Charting-mode panel grid.

**Input**:
- `layout` (string, required) — one of:
  - `"one"` — single chart
  - `"two_stacked"` — two charts stacked vertically
  - `"two_side"` — two charts side-by-side
  - `"two_by_two"` — 2×2 grid

**Returns**:
```json
{ "ok": true, "prior": { "layout": "one", "symbols": ["SPY"] } }
```

Layout switches preserve focused-first chart symbols across the rebuild (`on_apply_chart_layout` logic); the `prior.symbols` list lets Undo restore the original layout's symbol set.

**Errors**: `"unknown layout '...'"`.

## Tool-loop limits

Per user turn the agentic loop is capped at **`MAX_TOOL_ITERATIONS = 20`** in `services/ai_chat.rs`. Hitting the cap fails the assistant turn with a diagnostic; the user's Retry button continues from the same history.

`get_candles` caps each call at 200 bars (`MAX_CANDLES_PER_CALL`).

## Drift checks

- Tool names must match between `crates/terminal_demo/src/ai_tools.rs::TOOL_NAMES` (client) and the server's `toolRegistry` keys.
- Time-anchor parsing lives in `ai_tools.rs::parse_anchor_time`; ISO strings only — no fallback to epoch ms numbers.
- Drawing field names must match storage struct field names in `crates/terminal_demo/src/drawings/shapes.rs` (e.g. `a_time/b_time` for line/arrow/rect/fib, `t0/t1` for positions).
