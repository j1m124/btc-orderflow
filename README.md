# terminal_demo

Tiling-window financial-analytics workspace demo built with [gpui-component](https://github.com/longbridge/gpui-component). Runs in the browser via WebAssembly; deployed to Railway from a pre-built static image published by GitHub Actions.

## Run

```sh
make install        # one-time: wasm-bindgen-cli @ 0.2.120 + bun deps
make dev            # build debug WASM, start Vite at http://localhost:3000
```

After WASM source changes during `make dev`, re-run `./scripts/build-wasm.sh` and refresh the browser — Vite hot-reloads JS but not the WASM blob.

For a release build that mirrors what CI ships:

```sh
make build          # release WASM + Vite production build
```

The bottom bar shows `v0.1.0 (debug)` in debug builds and `v0.1.0` in release builds, so you can tell at a glance which mode is loaded.

## Layout

Three placeholder panels — **Watchlist** | **Chart** | **Details** — arranged horizontally. Drag a tab to a pane edge to split, drag tabs between panes to reorganize. Click `+ Panel` to spawn additional instances. `⋯` → **Reset Layout** restores the default.

State persists across reloads in `localStorage`.

## Deploy

`main` push triggers `.github/workflows/web-image.yml`:

1. Runs `cargo test` (wasm-bindgen-test runner) — failed tests block the deploy.
2. Builds release WASM + Vite bundle (Swatinem cache, ~2–3 min cached).
3. Builds a tiny Caddy image (`web-static.Dockerfile`) and pushes to GHCR.
4. Railway pulls `ghcr.io/jimmyjai-lab/centoflow-terminal/web:latest` (per `railway.toml`) and runs it.

**One-time GHCR setup**: the workflow pushes with `$GITHUB_TOKEN`, which produces a private package. For Railway to pull, set the `web` package's visibility to public in GitHub → Packages.
