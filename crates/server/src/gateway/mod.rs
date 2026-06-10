//! WebSocket gateway — accepts client connections at `WS /ws`, multiplexes
//! per-subscription snapshot+stream traffic over each socket.
//!
//! `GET /healthz` returns 200 once the server is past boot (this lives on
//! the same axum router for ops convenience).
//!
//! In prod the same router also serves the static SPA: any unmatched route
//! is served from `STATIC_DIR` (set by Dokploy), with `index.html` as the
//! SPA fallback. COOP/COEP response headers are applied to every response
//! (required for SharedArrayBuffer, which gpui_platform needs). Cache
//! headers split by path so Vite-hashed assets are cached forever while
//! `index.html` stays no-cache.

mod session;

use anyhow::{Context, Result};
use axum::{
    Router,
    extract::{Request, State, WebSocketUpgrade},
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
};
use sqlx::PgPool;
use std::{net::SocketAddr, path::PathBuf, sync::Arc};
use tokio::sync::broadcast;
use tower_http::services::{ServeDir, ServeFile};
use tracing::{info, warn};

use crate::binance::parse::{DepthDiff, LiquidationTick, Tick, TradeTick};
use crate::ingest::BookState;

/// Shared state passed to each per-connection axum handler. The kline
/// broadcast is the existing candles forwarder source; trade / depth /
/// book_state / liquidation are wired in for the orderflow channels (used
/// by per-channel forwarders).
#[derive(Clone)]
pub struct GatewayState {
    pub pool: PgPool,
    pub broadcast_tx: broadcast::Sender<Tick>,
    pub trade_tx: broadcast::Sender<TradeTick>,
    pub depth_tx: broadcast::Sender<DepthDiff>,
    pub liquidation_tx: broadcast::Sender<LiquidationTick>,
    pub book_state: BookState,
    /// Allowed `Origin` header values for WS upgrades. `None` skips the
    /// check (the local-dev default — set `ALLOWED_ORIGINS` in prod to
    /// e.g. `https://orderflow.j1mdev.net`). `Arc` so the per-connection
    /// state clone stays cheap.
    pub allowed_origins: Option<Arc<Vec<String>>>,
}

/// Bind the axum HTTP/WS router to `addr` and serve forever. Returns only
/// on a hard listen error. In prod, `STATIC_DIR` points at the SPA dist
/// directory; if unset (local dev), only `/healthz` and `/ws` are routed
/// and the rest 404s.
pub async fn serve(addr: SocketAddr, state: GatewayState) -> Result<()> {
    let mut app = Router::new()
        .route("/healthz", get(healthz))
        .route("/ws", get(ws_handler))
        .with_state(state);

    if let Some(dir) = std::env::var("STATIC_DIR").ok().map(PathBuf::from) {
        if dir.is_dir() {
            let index = dir.join("index.html");
            let serve_dir = ServeDir::new(&dir).not_found_service(ServeFile::new(&index));
            app = app.fallback_service(serve_dir);
            info!(static_dir = %dir.display(), "serving static SPA");
        } else {
            warn!(static_dir = %dir.display(), "STATIC_DIR set but path is not a directory; skipping SPA fallback");
        }
    }

    let app = app.layer(middleware::from_fn(response_headers));

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind {addr}"))?;
    info!(%addr, "gateway listening");

    axum::serve(listener, app).await.context("axum serve")?;
    Ok(())
}

async fn healthz() -> impl IntoResponse {
    (StatusCode::OK, "ok")
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<GatewayState>,
    headers: HeaderMap,
) -> Response {
    if let Some(allowed) = state.allowed_origins.as_ref() {
        let origin = headers
            .get(header::ORIGIN)
            .and_then(|h| h.to_str().ok());
        let ok = matches!(origin, Some(o) if allowed.iter().any(|a| a == o));
        if !ok {
            warn!(?origin, "ws upgrade rejected: origin not allowed");
            return (StatusCode::FORBIDDEN, "origin not allowed").into_response();
        }
    }
    ws.on_upgrade(move |socket| session::run(socket, state))
        .into_response()
}

/// Middleware that sets COOP/COEP (so the browser exposes SharedArrayBuffer,
/// which gpui_platform's web backend needs) and a path-derived Cache-Control
/// header (`immutable` for Vite-hashed assets, `no-cache` for everything
/// else so `index.html` and API responses stay fresh).
async fn response_headers(req: Request, next: Next) -> Response {
    let path = req.uri().path().to_owned();
    let mut resp = next.run(req).await;
    let h = resp.headers_mut();
    h.insert(
        HeaderName::from_static("cross-origin-opener-policy"),
        HeaderValue::from_static("same-origin"),
    );
    h.insert(
        HeaderName::from_static("cross-origin-embedder-policy"),
        HeaderValue::from_static("require-corp"),
    );
    let cache = if path.starts_with("/assets/") || path.ends_with(".wasm") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    resp
}
