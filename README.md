# btc-orderflow

Personal BTC orderflow workspace. A Rust + WASM trading-terminal frontend backed by a from-scratch Rust ingest server that pulls BTCUSDT-perp data from Binance into TimescaleDB and streams it to the browser over WebSocket.

Forked from a private centoflow demo (SHA `cb904cf`); stripped down to chart + watchlist on a tiling-window shell built with [gpui-component](https://github.com/longbridge/gpui-component), then rebuilt around a real backend.

Live at <https://orderflow.j1mdev.net>.

## Layout

```
btc-orderflow/
├── crates/
│   ├── client/      # WASM client (gpui + gpui-component). Chart, watchlist,
│   │                #   trades tape, liquidations, orderbook, footprint, etc.
│   ├── protocol/    # Shared serde wire types — no I/O.
│   └── server/      # Native server: tokio + axum + sqlx.
│       └── migrations/
├── www/             # Vite + Bun host for the WASM blob.
├── docker-compose.yml   # Local TimescaleDB.
├── Dockerfile           # Multi-stage prod build (rust + bun → debian-slim).
└── Makefile             # Single entry point for every workflow.
```

## Run locally

```sh
make install        # one-time: wasm-bindgen-cli + sqlx-cli + bun deps
make db-up          # start TimescaleDB in Docker
make server         # ingest from Binance + serve WS on :8787
make dev            # debug WASM + Vite on http://localhost:3001
```

After Rust client changes during `make dev`, re-run `./scripts/build-wasm.sh` and refresh — Vite hot-reloads JS but not the WASM blob.

## Iterating against the deployed backend

`make dev-vps` runs Vite locally but proxies `/ws` to the deployed VPS, so you can hack on the Rust client against real production data without rebuilding the server image. Useful for client-only iteration when you don't want to run TimescaleDB locally.

## Architecture

**Server.** Single tokio binary. Three tasks: Binance WS ingest (9 timeframes via combined stream, exp-backoff reconnect + REST gap-heal on every connect attempt), DB writer (UPSERTs closed bars into a TimescaleDB hypertable), and an axum WS gateway on `:8787`. Per-client forwarders subscribe-before-snapshot to avoid the obvious tick/snapshot race.

**Protocol.** Tagged-enum serde frames (`ClientFrame.op`, `ServerFrame.type`). Forward-compat `Channel` slot — v1 only handles candles; trades / footprint / book are additive on both ends.

**Client.** One persistent WebSocket opened at boot. Refcounted per-`SubKey` subscriptions, id-routed inbound frames, exp-backoff reconnect, local-storage persistence. Panels (chart, watchlist, trades, liquidations, orderbook, footprint) all consume the same `KlineEvent` / `TradeEvent` / `LiquidationEvent` streams.

**Storage.** Single `candles` hypertable, PK `(symbol, tf, open_time)`, 1-day chunks, 7-day retention. `quote_volume / trades / taker_buy_vol` persisted from day one to unlock delta + VWAP later without a trade tape.

## Deploy

`main` push → `.github/workflows/deploy.yml` builds the multi-stage image → pushes to GHCR (`:latest` + `:sha-<short>`) → (optionally) pings Dokploy's redeploy webhook. The runtime image only needs `DATABASE_URL` and `ALLOWED_ORIGINS` env vars at start. Currently running on Hetzner CPX22 via Dokploy.

## Stack

- **Frontend:** Rust → wasm32-unknown-unknown via `wasm-bindgen`, gpui + [gpui-component](https://github.com/longbridge/gpui-component), Vite host via Bun.
- **Backend:** axum + tokio + sqlx, TimescaleDB on Postgres 16.
- **Build/deploy:** Docker multi-stage, GitHub Actions → GHCR, Dokploy on Hetzner.
