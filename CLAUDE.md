# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

`terminal_demo` — a tiling-window financial-analytics workspace demo. Single Rust crate (`crates/terminal_demo/`) that compiles **wasm-only** and is loaded by Vite. Built on **gpui-component** (longbridge), which is built on **gpui** (zed-industries). Deployed via Railway pulling a pre-built static image from GHCR.

## Commands

```sh
make dev                 # build debug WASM + start Vite at localhost:3000
make build               # release WASM + Vite production build (mirror of CI)
./scripts/build-wasm.sh             # debug wasm + wasm-bindgen
./scripts/build-wasm.sh --release   # release wasm
cargo check                          # type-check (defaults to wasm via .cargo/config.toml)
cargo test                           # wasm-bindgen-test-runner (Node) over the test crate
```

`.cargo/config.toml` sets `[build] target = "wasm32-unknown-unknown"` so plain `cargo check`/`cargo build`/`cargo test` all use the wasm target without needing `--target`. `cargo test` uses `wasm-bindgen-test-runner` (Node) as the runner; tests are `#[wasm_bindgen_test]`, not `#[test]`.

After WASM source changes during `make dev`, re-run `./scripts/build-wasm.sh` and refresh the browser (Vite hot-reloads the JS but not the WASM blob).

## Critical dependency pinning

`gpui` and `gpui_platform` in `Cargo.toml` are git deps with **no `rev` pin**. This matches gpui-component's own spec exactly. If you add a `rev`, cargo treats them as separate copies of the crate and *nothing* typechecks (you'll get hundreds of "this is the expected type / this is the found type" errors pointing at two different commit hashes). `gpui-component` itself is pinned to a specific rev for reproducibility — bump deliberately, never accidentally. `Cargo.lock` is committed.

`wasm-bindgen-cli` version (in `Makefile`, `.github/workflows/web-image.yml`, and on your machine) must match the `wasm-bindgen` crate version pulled in by `Cargo.lock`. Currently pinned to `0.2.120`. Mismatch = JS bindings reference symbols the WASM doesn't export.

## Architecture

**Single entry point.** `lib.rs::run(app)` is the shared `App` lifecycle. `lib.rs::wasm_entry::run` (`#[wasm_bindgen]`) uses `gpui_platform::single_threaded_web()` plus a transmute leak of `Rc<AppCell>` (mirrored from gpui-component's `story-web`) — the leak is required to keep the app alive after `run()` returns to the JS caller. `install_wasm_fonts` loads bundled fonts (system fonts aren't available in the browser) and points `gpui_component_assets::Assets::new(url)` at longbridge's CDN for icons (`https://longbridge.github.io/gpui-component/gallery/` — soft external dep).

**Workspace shell** (`workspace.rs::TerminalWorkspace`) holds:
- `top_bar`: thin 36px header with "+ Panel" menu (dispatches `AddPanel(SharedString)`) and "⋯" menu (dispatches `ResetLayout`).
- `dock_area`: gpui-component's `DockArea`. The initial layout is **pure tiling** — `DockItem::split_with_sizes` of nested `TabPanel`s, **no left/right/bottom dock zones**. Users reshape via gpui-component's built-in drag-tab-to-edge (split) and drag-tab-to-center (merge as tab) — both come for free from `TabPanel`.
- Subscribes to `DockEvent::LayoutChanged` and debounces a save (500ms) to `persistence`.

**Panels.** All panel kinds are the same `ContentPanel` struct parameterized by a `Kind` enum (Watchlist, Chart, Details, Portfolio, Notification, SmartMoney, AiChat, …). `Render::render` dispatches to `render_<kind>` functions. Each panel kind has a stable `panel_name()` string — this is the discriminator the gpui-component `PanelRegistry` uses to deserialize persisted layouts, so **never rename panel IDs without bumping `LAYOUT_VERSION`**.

**Focus tracking is non-trivial.** `gpui-component::DockArea::add_panel` only takes a `DockPlacement` (Center/Left/Right/Bottom) — it can't target a *specific* `TabPanel`. So we maintain a `LastFocusedTabPanel` global (`Rc<RefCell<Option<WeakEntity<TabPanel>>>>`) and:
- `Render::render` adds `.on_mouse_down(MouseButton::Left, ...)` on the panel body div (which has `id(...)` to be interactive). The handler writes the panel's `parent_tab_panel` into the global.
- `Panel::on_added_to(tab_panel, ...)` captures the parent `TabPanel` weak ref into `ContentPanel.parent_tab_panel`.
- `Panel::set_active(true, ...)` covers the tab-strip-click case where the active tab swaps.
- Workspace's `on_add_panel` reads the global to drop new tabs into the focused pane; falls back to `DockArea::add_panel(Center)` when nothing's been focused.
- **Why mouse-down, not `track_focus` + `on_focus_in`:** gpui's web backend implements focus via a hidden `<input>` element. Mobile browsers always pop the soft keyboard for any focused input — every tap was opening the keyboard. Mouse-down works identically on touch devices and doesn't claim text-input focus.

**Focused-panel border** is a 2px ring rendered always (color = transparent when unfocused, `theme.ring` when focused). The "focused" check compares the panel's `parent_tab_panel` `WeakEntity` id against the global. Constant ring width means toggling focus doesn't shift content.

**Persistence** (`persistence.rs`) stores every blob in `web_sys::window().local_storage()` under stable `terminal_demo.*.vN` keys. `LAYOUT_VERSION` bumps invalidate stored state at load time.

## Subtle gotchas

- **Inner `v_flex().size_full()` blocks scrolling.** A child with `size_full` is clamped to parent height — content can't overflow, so the outer `overflow_y_scroll` div has nothing to scroll. Use `.w_full()` on the inner content (full width, natural height); reserve `.size_full()` for the scroll wrapper itself, the focus-border container, and AI Chat's top-level flex (which uses internal `flex_1().min_h_0().overflow_y_scroll()` to keep its input bar pinned at the bottom).
- **Bun + Node mismatch.** `www/package.json` scripts use `bun --bun vite` (not `bun run vite`) to force Bun's runtime. Without `--bun`, Bun shells out to Node — and if your Node is older than 20.19, Vite 8 won't load.
- **Vite COOP/COEP headers** are required for SharedArrayBuffer (gpui_platform wants it). Already set in `vite.config.js`.
- **AnyView coercion.** `Entity<TerminalWorkspace>` already implements `Into<AnyView>` — pass it directly to `Root::new`, don't call `.into()` (the compiler can't infer the target type and errors out).
- **Action listeners use `cx.listener(Self::method)`**, registered on the outermost div via `.on_action(...)`. Adding an action means: `actions!(...)` or `#[derive(Action)]`, register a `cx.listener` on the workspace render, dispatch from a button via `window.dispatch_action(Box::new(MyAction), cx)`.

## Deploy

Push to `main` → `.github/workflows/web-image.yml`:

1. `cargo test` (wasm-bindgen-test-runner) — failed tests block deploy.
2. Builds release WASM + Vite bundle (Swatinem rust-cache, ~2–3 min cached).
3. Builds + pushes `ghcr.io/jimmyjai-lab/centoflow-terminal/web:latest` from `web-static.Dockerfile` (tiny Caddy + dist).
4. Railway picks up the new image (per `railway.toml`'s `builder = "image"`).

**GHCR package visibility**: the workflow pushes with `$GITHUB_TOKEN`, which creates *private* packages. Railway can't pull private GHCR images without extra credentials, so set the `web` package visibility to public in GitHub → Packages (one-time). The WASM bundle is already shipped to user browsers, so there's nothing sensitive in the image.

## When extending

- **New panel kind:** add a `Kind` variant + `id()` + `display()` mapping; add a `render_<kind>` function; add the dispatch arm in `Render::render`. The kind auto-appears in the "+ Panel" menu (driven by `Kind::ALL`).
- **Change initial layout:** edit `workspace.rs::apply_default_layout`. Bump `LAYOUT_VERSION` so users with persisted state get reset to the new default instead of restoring the old one.
- **Need real chart data:** gpui-component ships `candlestick_chart`, `line_chart`, `area_chart`, `bar_chart`, `pie_chart` and a primitive `plot` module. No third-party charting library required.
- **Add a test:** put it under `#[cfg(test)] mod tests { ... }` and tag it `#[wasm_bindgen_test]` (NOT `#[test]`). The runner config in `.cargo/config.toml` handles dispatching to Node.
