//! WebSocket gateway — accepts client connections at `WS /ws`, multiplexes
//! per-subscription snapshot+stream traffic over each socket.
//!
//! `GET /healthz` returns 200 once the server is past boot (this lives on
//! the same axum router for ops convenience).

mod session;

use anyhow::{Context, Result};
use axum::{
    Router,
    extract::{State, WebSocketUpgrade},
    http::StatusCode,
    response::IntoResponse,
    routing::get,
};
use sqlx::PgPool;
use std::net::SocketAddr;
use tokio::sync::broadcast;
use tracing::info;

use crate::binance::parse::Tick;

/// Shared state passed to each per-connection axum handler.
#[derive(Clone)]
pub struct GatewayState {
    pub pool: PgPool,
    pub broadcast_tx: broadcast::Sender<Tick>,
}

/// Bind the axum HTTP/WS router to `addr` and serve forever. Returns only
/// on a hard listen error.
pub async fn serve(addr: SocketAddr, state: GatewayState) -> Result<()> {
    let app = Router::new()
        .route("/healthz", get(healthz))
        .route("/ws", get(ws_handler))
        .with_state(state);

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
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| session::run(socket, state))
}
