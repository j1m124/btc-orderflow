# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

Personal BTC orderflow workspace, forked from a private centoflow trading-terminal demo (source SHA `cb904cf5289d0cb39bae32bc5496c7ec9c3af571`). The fork strips the original down to chart + watchlist on a tiling-window shell. The chart panel, indicators, drawings, free-layout docking, floating-window infrastructure, and watchlist are intentionally preserved verbatim; everything else was deleted.

## Layout

```
btc-orderflow/
├── client/    # Rust + WASM + Vite frontend (the only thing that builds today).
│   ├── crates/btc_orderflow/   # The single Rust crate (compiles wasm-only).
│   ├── vendor/gpui-component/  # Pinned upstream fork — adds whole-window-edge docking.
│   ├── fonts/, www/, scripts/, Makefile, Cargo.toml, .cargo/, ...
└── server/    # Placeholder. A BTC orderflow data backend will live here.
```

All build commands run from `client/`.

## Commands

```sh
cd client
make install                         # one-time: wasm-bindgen-cli + bun deps
make dev                             # debug WASM + Vite at localhost:3001
make build                           # release WASM + Vite production bundle
./scripts/build-wasm.sh              # debug wasm + wasm-bindgen
./scripts/build-wasm.sh --release    # release wasm
cargo check                          # type-check (wasm via .cargo/config.toml)
cargo test                           # wasm-bindgen-test-runner (Node)
```

`.cargo/config.toml` sets `[build] target = "wasm32-unknown-unknown"`. Tests use `wasm-bindgen-test-runner` (Node) so they're tagged `#[wasm_bindgen_test]`, not `#[test]`.

After WASM source changes during `make dev`, re-run `./scripts/build-wasm.sh` and refresh — Vite hot-reloads JS but not the WASM blob.

## Critical dependency pinning

`gpui` and `gpui_platform` are git deps with **no `rev` pin** — they unify with the (un-revved) `gpui` dep declared inside `gpui-component`. Adding a `rev` makes cargo treat them as two separate copies and nothing typechecks. `gpui-component` itself is pinned to `d3d6e56c96659fb7516e2c743b80331af62e546d`. Reproducibility comes from `Cargo.lock`, which is committed.

`wasm-bindgen-cli` version (in `Makefile` and on your machine) must match the `wasm-bindgen` crate version pulled in by `Cargo.lock`. Currently `0.2.120`. Mismatch = JS bindings reference symbols the WASM doesn't export.

## Architecture

**Single entry point.** `lib.rs::run(app)` is the shared `App` lifecycle. `lib.rs::wasm_entry::run` (`#[wasm_bindgen]`) uses `gpui_platform::single_threaded_web()` plus a transmute leak of `Rc<AppCell>` (mirrored from gpui-component's `story-web`) — the leak keeps the app alive after `run()` returns to the JS caller. `install_wasm_fonts` loads bundled fonts (system fonts aren't available in the browser) and points `gpui_component_assets::Assets::new(url)` at longbridge's CDN for icons.

**Workspace shell** (`workspace.rs::TerminalWorkspace`) holds:
- `sidebar`: single mode button (FreeLayout) + settings shortcut.
- `top_bar`: `+ Panel` menu, `Layouts` menu (saved layouts), drawing tools, objects popover.
- `dock_area`: gpui-component's `DockArea`. The default layout is one Chart panel filling the workspace; watchlist is available via `+ Panel`.
- `bottom_bar`: connection status (stubbed `Connecting`), clock, FPS, version.
- Subscribes to `DockEvent::LayoutChanged` and debounces a save (500ms) to `persistence`.

**Panels.** `panels.rs::ContentPanel` parameterized by a `Kind` enum (Watchlist, Chart). `Render::render` dispatches to `panels::watchlist::render` / `panels::chart::render`. Each panel kind has a stable `panel_name()` (used as the `PanelRegistry` discriminator). Bump `LAYOUT_VERSION` in `workspace.rs` if you change panel IDs.

**Focus tracking.** `LastFocusedTabPanel` global + per-panel `on_mouse_down` listeners record which `TabPanel` was last touched, so the `+ Panel` action can drop new tabs into the focused pane. Mouse-down rather than `track_focus`/`on_focus_in` because gpui's web focus uses a hidden `<input>` that pops the mobile soft keyboard on every tap.

**Persistence** (`persistence.rs`) stores every blob in `web_sys::window().local_storage()` under `btc_orderflow.*.v3` keys. `purge_v1` drops legacy keys from the pre-fork ancestor (`terminal_demo.*` and `centoflow.auth.*`) on first run.

**Mode collapse.** The original had Charting/Signal/Research/Portfolio/FreeLayout modes. After the fork only `Mode::FreeLayout` remains; the enum stays so the UI code keeps its shape. Sidebar still renders one button; `SwitchMode` dispatches to a no-op handler.

## Services are stubs

`crates/btc_orderflow/src/services/` keeps the public types and function signatures of the old centoflow-backed market-data layer (`Candle`, `Timeframe`, `Session`, `LiveStatus`, `KlineEvent`, `MarketDataService`, `SubscriptionHandle`, `SymbolsService`, `WatchlistService`, `RecentsService`, `BarStream`). Bodies are no-ops:

- `MarketDataService::ensure` returns a handle pointing at an empty candle buffer.
- `MarketDataService::status` is permanently `Connecting`.
- `SymbolsService` hardcodes a single `BTCUSDT / BINANCE` entry.
- `WatchlistService` defaults to `["BTCUSDT"]`.

The chart and watchlist panels compile and render their scaffolding, but no bars ever arrive until a real backend is wired in (planned under `server/`).

## Subtle gotchas (carried from the source)

- **Inner `v_flex().size_full()` blocks scrolling.** A child with `size_full` is clamped to parent height — content can't overflow, so the outer `overflow_y_scroll` div has nothing to scroll. Use `.w_full()` on the inner content and reserve `.size_full()` for the scroll wrapper.
- **Bun + Node mismatch.** `www/package.json` scripts use `bun --bun vite` (not `bun run vite`) to force Bun's runtime. Without `--bun`, Bun shells out to Node — and if your Node is older than 20.19, Vite 8 won't load.
- **Vite COOP/COEP headers** are required for SharedArrayBuffer (gpui_platform wants it). Set in `vite.config.js`.

## When extending

- **New panel kind:** add a `Kind` variant + `id()` mapping in `panels.rs`; add a `render_<kind>` in `panels/<kind>.rs`; add the dispatch arm in `ContentPanel::Render::render`. The kind auto-appears in the "+ Panel" menu (driven by `Kind::ALL`).
- **Change initial layout:** edit `workspace.rs::build_default_layout`. Bump `LAYOUT_VERSION` so users with persisted state get reset.
- **Real chart data:** wire a backend in `server/` and replace the stubs in `services/market_data.rs`. Same fn signatures; the chart panel doesn't need to change.
