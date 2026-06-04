# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

Personal BTC orderflow workspace, forked from a private centoflow trading-terminal demo (source SHA `cb904cf5289d0cb39bae32bc5496c7ec9c3af571`). The fork strips the original down to chart + watchlist on a tiling-window shell and replaces the backend with a from-scratch Rust server that ingests BTCUSDT-perp klines from Binance into TimescaleDB and streams them to the WASM client over WebSocket.

## Layout

Flat cargo workspace at the repo root (the original `client/` + `server/` split was collapsed once the server became Rust):

```
btc-orderflow/
├── crates/
│   ├── btc_orderflow/             # WASM client (gpui + gpui-component).
│   ├── btc_orderflow_protocol/    # Shared wire types — serde-only, no I/O.
│   └── btc_orderflow_server/      # Native server (tokio + axum + sqlx).
├── migrations are at crates/btc_orderflow_server/migrations/
├── vendor/gpui-component/         # Pinned upstream fork — adds whole-window-edge docking.
├── www/                           # Vite + Bun frontend host for the WASM blob.
├── fonts/, scripts/
├── Cargo.toml                     # Workspace root.
├── .cargo/config.toml             # wasm-bindgen-test-runner only; no global target.
├── docker-compose.yml             # TimescaleDB (port 5432).
└── Makefile                       # Single entry point for every target.
```

## Commands

```sh
make install                          # one-time: wasm-bindgen-cli + sqlx-cli + bun deps
make db-up                            # start TimescaleDB (Docker)
make server                           # cargo run -p btc_orderflow_server (gap-heals + ingests + serves WS)
make dev                              # debug WASM + Vite at localhost:3001

make check                            # cargo check for every crate (per-target)
make check-{protocol,client,server}   # individual checks

make db-psql                          # psql shell against the running DB
make db-reset                         # nuke the DB volume (drops all candles)
make db-migration NAME=foo            # generate a reversible migration pair (.up.sql + .down.sql)
```

Migrations are reversible (paired `<ts>_<name>.up.sql` + `<ts>_<name>.down.sql`). The server applies pending `.up.sql` files on boot via `sqlx::migrate!`. There's no `make db-migrate-revert` target — reverting is rare and deliberate; invoke `sqlx migrate revert --source crates/btc_orderflow_server/migrations` (with `DATABASE_URL` set) directly when you mean it.

`.cargo/config.toml` no longer sets a default target — the workspace is mixed (wasm client + host server). Bare `cargo check` from root fails; use `make check` or the per-crate `cargo check -p ... [--target ...]` form. The wasm-bindgen-test-runner is preserved under `[target.wasm32-unknown-unknown]` so `cargo test -p btc_orderflow --target wasm32-unknown-unknown` works.

After WASM source changes during `make dev`, re-run `./scripts/build-wasm.sh` and refresh — Vite hot-reloads JS but not the WASM blob.

## Critical dependency pinning

`gpui` and `gpui_platform` are git deps with **no `rev` pin** — they unify with the (un-revved) `gpui` dep declared inside `gpui-component`. Adding a `rev` makes cargo treat them as two separate copies and nothing typechecks. `gpui-component` itself is pinned to `d3d6e56c96659fb7516e2c743b80331af62e546d`. Reproducibility comes from `Cargo.lock`, which is committed.

`wasm-bindgen-cli` version (in `Makefile` and on your machine) must match the `wasm-bindgen` crate version pulled in by `Cargo.lock`. Currently `0.2.120`. Mismatch = JS bindings reference symbols the WASM doesn't export.

`sqlx-cli` is pinned the same way (`SQLX_CLI_VERSION` in `Makefile`, currently `0.8.6`, matching the `sqlx` crate). Drift here is less catastrophic — the CLI is mostly forward-compatible — but keeps reversible-migration semantics in sync with the runtime applier.

## Architecture

### Server (`crates/btc_orderflow_server`)

Single binary, single tokio runtime, three tasks:

1. **Binance ingest** (`binance/ws.rs` + `ingest.rs::run_binance_ingest`). Subscribes to the combined-stream URL covering `btcusdt@kline_{tf}` for every entry in `Timeframe::ALL` (9 streams). Reconnect loop with exp backoff (1s → 30s cap) plus a gap-heal REST call between every connect attempt — handles Binance's 24h hard-disconnect and the boot-time cold start with the same code path.
2. **DB writer** (`ingest.rs::run_db_writer`). Subscribes to a `tokio::sync::broadcast` channel populated by the ingest task. UPSERTs closed bars; skips in-progress bars (the canonical row is the closed bar; ON CONFLICT replaces any earlier persisted version).
3. **WS gateway** (`gateway/`). axum router on `127.0.0.1:8787` serving `GET /healthz` and `WS /ws`. Per-client task with per-`SubId` forwarders that (a) subscribe to the broadcast BEFORE querying the snapshot — Q5b ordering — and (b) dedupe closed-bar ticks whose `open_time ≤ snapshot tail`.

Storage is a single `candles` hypertable in TimescaleDB. PK `(symbol, tf, open_time)`. 1-day chunk interval. 7-day retention policy. `quote_volume`, `trades`, `taker_buy_vol` are stored from day one even though the v1 wire `Candle` is OHLCV-only — they unlock delta / VWAP / aggression indicators without a trade-tape.

Boot ordering matters: broadcast channel → DB writer subscriber → Binance WS subscriber → gap-heal REST → gateway listener. The writer's permanent receiver keeps the broadcast from going zero-consumer between Binance connect and the first gateway client.

### Protocol (`crates/btc_orderflow_protocol`)

Shared serde-only types. Tagged enums (`ClientFrame.op`, `ServerFrame.type`) match `serde_json` defaults. `Channel` discriminator on `Subscribe` is a forward-compat slot — v1 only handles `Channel::Candles`; adding `Trades`/`Footprint`/`Book` later is purely additive on both ends.

### Client (`crates/btc_orderflow`)

**Single entry point.** `lib.rs::run(app)` is the shared `App` lifecycle. `lib.rs::wasm_entry::run` (`#[wasm_bindgen]`) uses `gpui_platform::single_threaded_web()` plus a transmute leak of `Rc<AppCell>` (mirrored from gpui-component's `story-web`) — the leak keeps the app alive after `run()` returns to the JS caller. `install_wasm_fonts` loads bundled fonts (system fonts aren't available in the browser) and points `gpui_component_assets::Assets::new(url)` at longbridge's CDN for icons.

**Workspace shell** (`workspace.rs::TerminalWorkspace`) holds:
- `sidebar`: single mode button (FreeLayout) + settings shortcut.
- `top_bar`: `+ Panel` menu, `Layouts` menu (saved layouts), drawing tools, objects popover.
- `dock_area`: gpui-component's `DockArea`. The default layout is one Chart panel filling the workspace; watchlist is available via `+ Panel`.
- `bottom_bar`: connection status (driven by the WS), clock, FPS, version.
- Subscribes to `DockEvent::LayoutChanged` and debounces a save (500ms) to `persistence`.

**Panels.** `panels.rs::ContentPanel` parameterized by a `Kind` enum (Watchlist, Chart). `Render::render` dispatches to `panels::watchlist::render` / `panels::chart::render`. Each panel kind has a stable `panel_name()` (used as the `PanelRegistry` discriminator). Bump `LAYOUT_VERSION` in `workspace.rs` if you change panel IDs.

**Focus tracking.** `LastFocusedTabPanel` global + per-panel `on_mouse_down` listeners record which `TabPanel` was last touched, so the `+ Panel` action can drop new tabs into the focused pane. Mouse-down rather than `track_focus`/`on_focus_in` because gpui's web focus uses a hidden `<input>` that pops the mobile soft keyboard on every tap.

**Persistence** (`persistence.rs`) stores every blob in `web_sys::window().local_storage()` under `btc_orderflow.*.v3` keys.

**Mode collapse.** The original had Charting/Signal/Research/Portfolio/FreeLayout modes. After the fork only `Mode::FreeLayout` remains; the enum stays so the UI code keeps its shape. Sidebar still renders one button; `SwitchMode` dispatches to a no-op handler.

### Market-data service (`services/market_data.rs`)

WS-driven, not stubbed. Opens one persistent `ws://127.0.0.1:8787/ws` connection at boot, reconnects forever with exp backoff (1s → 30s). The connection driver task and a release task are spawned from `services::market_data::init`.

`ensure(symbol, tf, session)` refcounts on `SubKey`; the first ensure allocates a `SubId` and pushes a `Subscribe` frame, the last `SubscriptionHandle::Drop` pushes `Unsubscribe`. `load_older` pushes a `HistoryPage` frame keyed on the oldest currently-held `open_time`. Incoming `ServerFrame`s route by `SubId` → `SubKey` and emit `KlineEvent::Resnap / Tick / Prepended / HistoryCapped / StatusChanged`. On `Resnap` the client clears the buffer AND re-pushes a `Subscribe` (the v1 server's forwarder exits its subscription on broadcast lag — Q12e).

The chart panel doesn't know about any of this — it consumes the same `KlineEvent` events the stub used to emit. The wire-Candle (i64-ms timestamps, no display fields) is converted to the client's `Candle` (with `date: SharedString`) via `candle_from_proto`.

## Subtle gotchas (carried from the source)

- **Inner `v_flex().size_full()` blocks scrolling.** A child with `size_full` is clamped to parent height — content can't overflow, so the outer `overflow_y_scroll` div has nothing to scroll. Use `.w_full()` on the inner content and reserve `.size_full()` for the scroll wrapper.
- **Bun + Node mismatch.** `www/package.json` scripts use `bun --bun vite` (not `bun run vite`) to force Bun's runtime. Without `--bun`, Bun shells out to Node — and if your Node is older than 20.19, Vite 8 won't load.
- **Vite COOP/COEP headers** are required for SharedArrayBuffer (gpui_platform wants it). Set in `vite.config.js`.
- **sqlx is non-macro form.** Queries use `sqlx::query` / `sqlx::query_as::<_, T>(...)` rather than the `query!` macro, because that macro needs either a live DB at compile time or a committed `.sqlx/` offline cache. We give up the compile-time column check; runtime type errors surface on the first query against a mis-typed column. At the current scope (~6 distinct queries) the trade-off was deliberate.

## When extending

- **New panel kind:** add a `Kind` variant + `id()` mapping in `panels.rs`; add a `render_<kind>` in `panels/<kind>.rs`; add the dispatch arm in `ContentPanel::Render::render`. The kind auto-appears in the "+ Panel" menu (driven by `Kind::ALL`).
- **Change initial layout:** edit `workspace.rs::build_default_layout`. Bump `LAYOUT_VERSION` so users with persisted state get reset.
- **New wire frame** (e.g. trade tape, footprint, book): add a `ServerFrame` variant in the protocol crate, add a `Channel` discriminator if it's a new subscription kind, route on both ends. Server: add a Binance stream / table / forwarder. Client: add a `KlineEvent`-equivalent and a service method.
- **Multiple symbols:** the server-side `SUPPORTED_SYMBOL` constant in `crates/btc_orderflow_server/src/main.rs` is the gate. Add to a `const SYMBOLS: &[&str]` slice, loop the ingest setup per symbol, drop the per-`Subscribe` validation in `gateway/session.rs`.
