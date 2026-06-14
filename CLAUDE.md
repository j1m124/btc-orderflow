# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

Personal BTC orderflow terminal, forked from a private centoflow trading-terminal demo (source SHA `cb904cf5289d0cb39bae32bc5496c7ec9c3af571`). The original was stripped to a tiling-window shell and rebuilt around a from-scratch Rust backend; it has since grown into a full orderflow stack: the server ingests BTCUSDT-perp klines, aggTrades, depth diffs, and liquidations from Binance USD-M futures into TimescaleDB and streams six channel kinds (candles, trades, footprint, book, liquidations, liquidation bars) to the WASM client over one WebSocket. The client has five panel kinds (chart, watchlist, trades tape, orderbook, liquidations), footprint render modes, an indicator framework, drawing tools, and a declarative settings system.

A detailed subsystem-by-subsystem breakdown lives in `docs/ARCHITECTURE.md` — prefer it over re-deriving how something works. Keep both that file and this one updated when architecture changes.

## Layout

Flat cargo workspace at the repo root:

```
btc-orderflow/
├── crates/
│   ├── client/                    # WASM client (gpui + gpui-component).
│   ├── protocol/                  # Shared wire types — serde-only, no I/O.
│   └── server/                    # Native server (tokio + axum + sqlx).
│       └── migrations/            # sqlx-cli reversible migrations (.up.sql + .down.sql).
├── vendor/gpui-component/         # Pinned upstream fork — adds whole-window-edge docking.
├── www/                           # Vite + Bun frontend host for the WASM blob.
├── docs/ARCHITECTURE.md           # Deep technical breakdown (keep in sync).
├── fonts/, scripts/
├── Cargo.toml                     # Workspace root.
├── .cargo/config.toml             # wasm-bindgen-test-runner only; no global target.
├── docker-compose.yml             # TimescaleDB (port 5432).
├── Dockerfile                     # 5-stage cargo-chef build → debian-slim runtime.
└── Makefile                       # Single entry point for every target.
```

## Commands

```sh
make install                          # one-time: wasm-bindgen-cli + sqlx-cli + bun deps
make db-up                            # start TimescaleDB (Docker)
make server                           # cargo run -p server (gap-heals + ingests + serves WS)
make dev                              # debug WASM + Vite at localhost:3001
make dev-vps                          # same as `dev` but proxies /ws to the prod VPS backend
                                      #   (BACKEND_TARGET=$(VPS_BACKEND) — see Makefile);
                                      #   runs Vite under Node to dodge a Bun WS-proxy bug.

make check                            # cargo check for every crate (per-target)
make check-{protocol,client,server}   # individual checks

make db-psql                          # psql shell against the running DB
make db-reset                         # nuke the DB volume (drops all market data)
make db-migration NAME=foo            # generate a reversible migration pair (.up.sql + .down.sql)
```

Migrations are reversible (paired `<ts>_<name>.up.sql` + `<ts>_<name>.down.sql`). The server applies pending `.up.sql` files on boot via `sqlx::migrate!`. There's no `make db-migrate-revert` target — reverting is rare and deliberate; invoke `sqlx migrate revert --source crates/server/migrations` (with `DATABASE_URL` set) directly when you mean it.

`.cargo/config.toml` no longer sets a default target — the workspace is mixed (wasm client + host server). Bare `cargo check` from root fails; use `make check` or the per-crate `cargo check -p ... [--target ...]` form. The wasm-bindgen-test-runner is preserved under `[target.wasm32-unknown-unknown]` so `cargo test -p client --target wasm32-unknown-unknown` works.

After WASM source changes during `make dev`, re-run `./scripts/build-wasm.sh` and refresh — Vite hot-reloads JS but not the WASM blob.

## Critical dependency pinning

`gpui` and `gpui_platform` are git deps with **no `rev` pin** — they unify with the (un-revved) `gpui` dep declared inside `gpui-component`. Adding a `rev` makes cargo treat them as two separate copies and nothing typechecks. `gpui-component` itself is pinned to `d3d6e56c96659fb7516e2c743b80331af62e546d` and path-patched to the vendored fork. Reproducibility comes from `Cargo.lock`, which is committed.

`wasm-bindgen-cli` version (in `Makefile`, `Dockerfile`, and on your machine) must match the `wasm-bindgen` crate version pulled in by `Cargo.lock`. Currently `0.2.120`. Mismatch = JS bindings reference symbols the WASM doesn't export.

`sqlx-cli` is pinned the same way (`SQLX_CLI_VERSION` in `Makefile`, currently `0.8.6`, matching the `sqlx` crate). Drift here is less catastrophic — the CLI is mostly forward-compatible — but keeps reversible-migration semantics in sync with the runtime applier.

## Architecture

See `docs/ARCHITECTURE.md` for the full picture. The essentials:

### Server (`crates/server`)

Single binary, single tokio runtime. Tasks: Binance ingest (two WS connections — a combined market stream with 9 native kline streams + `aggTrade` + `forceOrder`, and a separate `depth@100ms` stream; exp-backoff reconnect 1s→30s with REST gap-heal before every connect attempt), three batched DB writers (klines per closed bar, trades on a 100ms flush, liquidations on 250ms), a sub-second aggregator (synthesizes `1s`/`5s` bars from aggTrades onto the same kline broadcast — Binance futures has no sub-minute kline streams), a book maintainer (Binance local-book sync: REST snapshot + sequence-checked diffs, re-bootstraps on any gap; persists top-50 snapshots every 1s), and the axum WS gateway.

Four `tokio::sync::broadcast` channels (kline / trade / depth / liquidation) fan ingest out to writers and per-client gateway forwarders. Writers hold permanent receivers so channels never go zero-consumer. Boot ordering in `main.rs` matters: channels → writers → aggregator/maintainer → ingest → gateway.

Gateway (`gateway/`): axum on `127.0.0.1:8787` serving `GET /healthz`, `WS /ws` (optional `ALLOWED_ORIGINS` allowlist), and a static-SPA fallback from `STATIC_DIR` with COOP/COEP headers. Per-client session: one writer task draining a shared mpsc, one forwarder task per `SubId`. Every forwarder subscribes to the broadcast BEFORE its snapshot query, then dedupes the live stream against the snapshot tail (`open_time` / `agg_id` / `ts_ms` cursors). Trades, footprint, book, and liquidation forwarders conflate into 100ms batches; broadcast lag → send `Resnap` and exit (client resubscribes).

Storage: four hypertables — `candles` (PK `(symbol, tf, open_time)`, 1-day chunks, 7-day retention), `trades` (1-hour chunks, 48h), `book_snapshots` (1-hour chunks, 48h), `liquidations` (1-day chunks, 7-day). Only raw events are persisted; footprint cells, sub-second bars, and liquidation bars are computed on read with `time_bucket` queries, so any bucket size / TF works retroactively.

### Protocol (`crates/protocol`)

Shared serde-only types. Tagged enums (`ClientFrame.op`, `ServerFrame.type`) match `serde_json` defaults. Six `Channel` kinds are live: `candles`, `trades`, `footprint {tf, price_bucket}`, `book {depth}`, `liquidations`, `liquidation_bars {tf}`. Each follows the same frame triple (`*Snapshot {server_v}` / `*Tick`-or-`*Update {v}` / `*HistoryPage`) plus cross-channel `Resnap` / `Status` / `Pong` / `Error`. `Timeframe::ALL` has 11 entries; `1s`/`5s` are synthesized (no Binance stream) — filter on `Timeframe::is_native_kline()` anywhere that maps TFs to Binance streams or REST backfills. `LiquidationSide` is the liquidated *position* side (server flips Binance's order side once at ingest). Crypto trades 24/7 so there's no session / RTH-ETH dimension on the wire.

### Client (`crates/client`)

**Single entry point.** `lib.rs::run(app)` is the shared `App` lifecycle. `lib.rs::wasm_entry::run` (`#[wasm_bindgen]`) uses `gpui_platform::single_threaded_web()` plus a transmute leak of `Rc<AppCell>` (mirrored from gpui-component's `story-web`) — the leak keeps the app alive after `run()` returns to the JS caller. `install_wasm_fonts` loads bundled fonts (system fonts aren't available in the browser) and points `gpui_component_assets::Assets::new(url)` at longbridge's CDN for icons.

**Workspace shell** (`workspace.rs::TerminalWorkspace`) holds:
- `top_bar`: title, Draw menu (12 drawing tools), Objects popover, `+ Panel` menu, `Layouts` menu, screenshot button, settings button.
- `dock_area`: gpui-component's `DockArea` (`LAYOUT_VERSION = 5`; bump on panel-ID changes so persisted layouts reset).
- `bottom_bar`: connection status (driven by the WS), clock, FPS, version (from `BUILD_SHA` via `build.rs`).
- Floating layer: `FloatingWindow` slots (indicator settings, footprint render settings, drawing settings, code editor) and the `FloatingStrip` shown when a drawing is selected.
- Subscribes to `DockEvent::LayoutChanged` and debounces a save (500ms) to `persistence`.

**Panels.** `panels.rs::ContentPanel` parameterized by `Kind` — Watchlist, Chart, Trades, Orderbook, Liquidations (`Kind::ALL` drives the `+ Panel` menu). The chart's main render is itself switchable (`RenderKind`: Candlestick / Cluster / Profile); footprint modes lazily open a footprint subscription.

**Focus tracking.** `LastFocusedTabPanel` + `LastFocusedChart` globals record the last-touched panel via `on_mouse_down` listeners (not `track_focus` — gpui's web focus rides a hidden `<input>` that pops the mobile soft keyboard). Watchlist clicks and `+ Panel` inserts route through these.

**Market-data service** (`services/market_data.rs`). One persistent WS (`wss://<host>/ws` derived from `window.location`; falls back to `ws://127.0.0.1:8787/ws`), reconnects forever with exp backoff. Per-channel refcounted `ensure_*` APIs keyed on typed sub-keys; the first ensure sends `Subscribe`, the last handle drop sends `Unsubscribe`. Incoming frames route by `SubId` and surface as per-channel event enums (`KlineEvent`, `TradeEvent`, `FootprintEvent`, `BookEvent`, `LiquidationEvent`, `LiquidationBarEvent`) with a common shape: `Snapshot / Tick|Update / Prepended / HistoryCapped / Resnap`. On `Resnap` the client clears its buffer AND re-sends `Subscribe` (the server forwarder exits on broadcast lag). Panels and indicators consume the events, never the wire types.

**Subsystems.** Indicators (`indicators/`): trait-object plugins (`IndicatorKind::compute(&[Candle], ComputeCtx) → IndicatorOutput`) with overlay-vs-pane placement; `ComputeCtx` carries footprint/liquidation data so indicators don't own subscriptions. Drawings (`drawings/`): tool state + serde shapes anchored in (time, price) space + a persisting service. Volume profile (`volume_profile/`): shared compute/paint for the VRVP indicator and FRVP drawing. Settings (`settings_form/`): declarative form framework used by every settings surface; edits route through typed targets, never mutate directly. Screenshot (`screenshot.rs`): captures gpui's canvas via `toBlob` before opening the preview dialog.

**Persistence** (`persistence.rs`) stores every blob in `web_sys::window().local_storage()` under `btc_orderflow.*.v3` keys; chart prefs are mirrored to static atomics for lock-free paint-path reads.

## Subtle gotchas

- **gpui's render canvas is anonymous.** gpui_web appends its own `<canvas>` to `<body>` at boot; the static `#canvas` in `www/index.html` is loading-shell decoration that `main.js` removes. Query `body > canvas`, never `#canvas`.
- **Inner `v_flex().size_full()` blocks scrolling.** A child with `size_full` is clamped to parent height — content can't overflow, so the outer `overflow_y_scroll` div has nothing to scroll. Use `.w_full()` on the inner content and reserve `.size_full()` for the scroll wrapper.
- **Bun + Node mismatch.** `www/package.json` scripts use `bun --bun vite` (not `bun run vite`) to force Bun's runtime. Without `--bun`, Bun shells out to Node — and if your Node is older than 20.19, Vite 8 won't load. (Exception: `make dev-vps` deliberately runs Vite under Node because Bun lacks `socket.destroySoon`, which Vite's WS proxy needs.)
- **Vite COOP/COEP headers** are required for SharedArrayBuffer (gpui_platform wants it). Set in `vite.config.js`; the prod gateway sets the same headers on the static fallback.
- **sqlx is non-macro form.** Queries use `sqlx::query` / `sqlx::query_as::<_, T>(...)` rather than the `query!` macro, because that macro needs either a live DB at compile time or a committed `.sqlx/` offline cache. We give up the compile-time column check; runtime type errors surface on the first query against a mis-typed column.
- **S1/S5 are not Binance streams.** Any code mapping timeframes to Binance kline streams or REST backfills must filter on `Timeframe::is_native_kline()`; sub-second bars come from aggTrades (live: subsec aggregator; history: `time_bucket` over `trades`).
- **Dockerfile layer caching is deliberate.** The cargo-chef cook layer is the only thing that makes CI fast; `BUILD_SHA`/`BUILD_REF` ARGs must stay declared *after* the cook, the chef base image stays digest-pinned, and `vendor/` must be COPYed before the cook (chef's recipe misses path-patched manifests). See the rationale comments in the Dockerfile before restructuring it.

## When extending

- **New panel kind:** add a `Kind` variant + `id()` mapping in `panels.rs`; add a render module in `panels/<kind>.rs`; add the dispatch arm in `ContentPanel::Render::render`. The kind auto-appears in the "+ Panel" menu (driven by `Kind::ALL`). Bump `LAYOUT_VERSION` in `workspace.rs` if panel IDs change.
- **New wire channel:** add the `Channel` variant + payload type + `ServerFrame` triple (snapshot/tick/history-page) in the protocol crate. Server: route in `gateway/session.rs` (forwarder follows the subscribe-before-snapshot + dedupe-cursor recipe), add db.rs queries, and a Binance stream/table if it's a new source. Client: add a sub-key + `ensure_*` + event enum in `market_data.rs`. Existing channels are the template — trades or liquidations are the simplest to copy.
- **New indicator:** implement `IndicatorKind` in `indicators/<name>.rs`, register it in the picker list in `indicators.rs`, and return a `SettingsForm` from `settings_form()` if it has parameters.
- **New drawing tool:** add a `Tool` variant in `drawings/tool.rs`, a shape struct in `drawings/shapes.rs`, and the create/paint/hit-test arms in the chart panel.
- **Multiple symbols:** the server-side `SYMBOL` constant in `crates/server/src/main.rs` is the gate. Loop the ingest/writer/maintainer setup per symbol and drop the per-`Subscribe` validation in `gateway/session.rs`. The client is already symbol-keyed throughout.
- **New schema migration:** `make db-migration NAME=add_xyz` → fill in the paired `.up.sql` + `.down.sql` in `crates/server/migrations/`. The server applies up-migrations on next boot.
