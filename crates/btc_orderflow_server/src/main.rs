//! btc_orderflow_server — entry point.
//!
//! Boot sequence (Q5/Q10):
//!   1. env → init tracing
//!   2. connect TimescaleDB → run migrations
//!   3. spawn DB writer task (subscribes to broadcast)
//!   4. spawn Binance ingest task (gap-heal REST → connect WS → broadcast →
//!      reconnect-with-gap-heal on every drop)
//!   5. spawn WS gateway (axum router serving GET /healthz + WS /ws)
//!   6. block on Ctrl-C

use anyhow::{Context, Result};
use chrono::Duration as ChronoDuration;
use sqlx::postgres::PgPoolOptions;
use std::net::SocketAddr;
use tokio::sync::broadcast;
use tracing::{info, warn};

mod binance;
mod db;
mod gateway;
mod ingest;

/// Hardcoded for v1 (Q14b: BTCUSDT whitelist).
const SYMBOL: &str = "BTCUSDT";

/// Cold-start backfill window. Matches the DB retention policy (Q9/Q10).
const COLD_START_DAYS: i64 = 7;

/// Default listen address for the gateway (Q13c).
const DEFAULT_LISTEN_ADDR: &str = "127.0.0.1:8787";

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    let db_url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://btc:btc@127.0.0.1:5432/btc_orderflow".into());
    info!(db_url = %redact_url(&db_url), "connecting to database");

    let pool = PgPoolOptions::new()
        .max_connections(8)
        .connect(&db_url)
        .await
        .context("connect to TimescaleDB")?;

    db::run_migrations(&pool)
        .await
        .context("run sqlx migrations")?;

    let (tx, _bootstrap_rx) = broadcast::channel::<binance::parse::Tick>(ingest::BROADCAST_CAPACITY);

    // DB writer holds a permanent Receiver so the broadcast never runs out
    // of consumers (which would otherwise drop every send during the gap
    // between Binance connect and the gateway's first client).
    let writer_rx = tx.subscribe();
    let writer_pool = pool.clone();
    let writer = tokio::spawn(async move {
        if let Err(e) = ingest::run_db_writer(writer_pool, writer_rx).await {
            warn!(error = ?e, "db writer task exited with error");
        }
    });

    // Drop the bootstrap receiver from main — the writer's receiver is the
    // canonical one, and any future gateway subs clone from `tx`.
    drop(_bootstrap_rx);

    let ingest_handle = {
        let pool = pool.clone();
        let rest = binance::rest::RestClient::default();
        let tx = tx.clone();
        tokio::spawn(async move {
            if let Err(e) = ingest::run_binance_ingest(
                pool,
                rest,
                tx,
                SYMBOL.to_string(),
                ChronoDuration::days(COLD_START_DAYS),
            )
            .await
            {
                warn!(error = ?e, "binance ingest task exited with error");
            }
        })
    };

    let listen_addr: SocketAddr = std::env::var("LISTEN_ADDR")
        .unwrap_or_else(|_| DEFAULT_LISTEN_ADDR.into())
        .parse()
        .context("parse LISTEN_ADDR")?;

    let gateway_state = gateway::GatewayState {
        pool: pool.clone(),
        broadcast_tx: tx.clone(),
    };
    let gateway_handle = tokio::spawn(async move {
        if let Err(e) = gateway::serve(listen_addr, gateway_state).await {
            warn!(error = ?e, "gateway task exited with error");
        }
    });

    info!("stack up; waiting for Ctrl-C");
    tokio::signal::ctrl_c()
        .await
        .context("install Ctrl-C handler")?;

    info!("shutdown requested");
    gateway_handle.abort();
    ingest_handle.abort();
    writer.abort();
    Ok(())
}

fn init_tracing() {
    use tracing_subscriber::{EnvFilter, fmt};
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("btc_orderflow_server=info,sqlx=warn"));
    fmt().with_env_filter(filter).init();
}

/// Strip the password from a postgres URL before logging it.
fn redact_url(url: &str) -> String {
    match reqwest::Url::parse(url) {
        Ok(mut u) => {
            if u.password().is_some() {
                let _ = u.set_password(Some("***"));
            }
            u.to_string()
        }
        Err(_) => "<unparseable url>".to_string(),
    }
}
