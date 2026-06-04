# btc-orderflow

Personal BTC orderflow workspace. Forked from a private trading-terminal demo at SHA `cb904cf`; stripped down to chart + watchlist on a tiling-window shell built with [gpui-component](https://github.com/longbridge/gpui-component). Runs in the browser via WebAssembly.

## Layout

```
btc-orderflow/
├── client/    # Rust + WASM + Vite. The chart/watchlist UI lives here.
└── server/    # Placeholder. A future BTC orderflow data backend goes here.
```

## Run

```sh
cd client
make install        # one-time: wasm-bindgen-cli @ 0.2.120 + bun deps
make dev            # build debug WASM, start Vite at http://localhost:3001
```

After WASM source changes during `make dev`, re-run `./scripts/build-wasm.sh` and refresh the browser — Vite hot-reloads JS but not the WASM blob.

For a release build:

```sh
cd client
make build
```

## State

There is no backend yet. The market-data services return empty buffers; the chart panel renders its scaffolding (axes, indicators, crosshair, drawings) without live bars. A real BTC orderflow backend will land in `server/` and the `client/crates/btc_orderflow/src/services/` stubs will be reimplemented against it.
