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
├── Dockerfile.server    # Native server image (cargo-chef → debian-slim).
├── Dockerfile.client    # WASM + Vite → Caddy static image.
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

The deep dive lives in [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md). The short version:

**Server.** Single tokio binary. Binance ingest over two WebSocket connections (9 kline streams + aggTrade + forceOrder on one, depth@100ms on the other), exp-backoff reconnect with REST gap-heal on every connect attempt. Four broadcast channels fan out to batched DB writers, a sub-second aggregator (1s/5s bars synthesized from aggTrades — Binance futures has no sub-minute klines), a sequence-checked orderbook maintainer, and an axum WS gateway. Per-client forwarders subscribe-before-snapshot, dedupe against the snapshot tail, and conflate live streams into 100ms batches.

**Protocol.** Tagged-enum serde frames (`ClientFrame.op`, `ServerFrame.type`). Six `Channel` kinds: candles, trades, footprint, book, liquidations, liquidation bars — each with the same snapshot / tick / history-page frame triple, plus `Resnap` for gap recovery.

**Client.** One persistent WebSocket opened at boot. Refcounted per-`SubKey` subscriptions, id-routed inbound frames, exp-backoff reconnect, local-storage persistence. Five panel kinds (chart, watchlist, trades, orderbook, liquidations) plus chart-level footprint render modes, an indicator plugin framework, drawing tools, and a declarative settings system — all consuming per-channel event streams, never the wire types.

**Storage.** Four TimescaleDB hypertables: `candles` (1-day chunks, 7-day retention), `trades` (1-hour, 48h), `book_snapshots` (1-hour, 48h), `liquidations` (1-day, 7-day). Only raw events are persisted — footprint cells, sub-second bars, and liquidation bars are `time_bucket` queries on read, so any bucket size or timeframe works retroactively.

## Deploy

`main` push → two path-filtered workflows build independently: `.github/workflows/server.yml` (native server image) and `client.yml` (WASM + Caddy static image), each → GHCR (`:latest` + `:sha-<short>`) → (optionally) pings its own Dokploy redeploy webhook. Server and client deploy on separate cadences (a client change never restarts the server); a wire-protocol change rebuilds both — deploy server-first (see CLAUDE.md "Split deployment"). The server image only needs `DATABASE_URL` + `ALLOWED_ORIGINS`; the client image is static. Both behind one Traefik on Hetzner CPX22 via Dokploy.

## Stack

- **Frontend:** Rust → wasm32-unknown-unknown via `wasm-bindgen`, gpui + [gpui-component](https://github.com/longbridge/gpui-component), Vite host via Bun.
- **Backend:** axum + tokio + sqlx, TimescaleDB on Postgres 16.
- **Build/deploy:** Docker multi-stage, GitHub Actions → GHCR, Dokploy on Hetzner.
