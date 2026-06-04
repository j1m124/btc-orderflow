//! server — entry point.
//!
//! Boot sequence:
//!   1. env → init tracing
//!   2. connect TimescaleDB → run migrations
//!   3. spawn DB writer tasks (kline + trade) subscribed to their broadcasts
//!   4. spawn Binance ingest task (kline+trade gap-heal REST → connect WS →
//!      broadcast → reconnect-with-gap-heal on every drop)
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

use binance::BroadcastTxs;

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

    let (kline_tx, _kline_bootstrap_rx) =
        broadcast::channel::<binance::parse::Tick>(ingest::BROADCAST_CAPACITY);
    let (trade_tx, _trade_bootstrap_rx) =
        broadcast::channel::<binance::parse::TradeTick>(ingest::TRADE_BROADCAST_CAPACITY);
    let (depth_tx, _depth_bootstrap_rx) =
        broadcast::channel::<binance::parse::DepthDiff>(ingest::DEPTH_BROADCAST_CAPACITY);

    // Shared live book state. Maintainer writes; gateway readers borrow
    // briefly to populate initial BookSnapshot frames.
    let book_state = ingest::BookState::new();

    // Writer tasks hold permanent Receivers so the broadcasts never run out
    // of consumers (which would otherwise drop every send during the gap
    // between Binance connect and the gateway's first client).
    let kline_writer = {
        let rx = kline_tx.subscribe();
        let pool = pool.clone();
        tokio::spawn(async move {
            if let Err(e) = ingest::run_db_writer(pool, rx).await {
                warn!(error = ?e, "kline db writer task exited with error");
            }
        })
    };
    let trade_writer = {
        let rx = trade_tx.subscribe();
        let pool = pool.clone();
        tokio::spawn(async move {
            if let Err(e) = ingest::run_trade_writer(pool, rx).await {
                warn!(error = ?e, "trade writer task exited with error");
            }
        })
    };

    // Sub-second aggregator: subscribes to trades, emits synthesized S1/S5
    // bars on the kline broadcast so the gateway treats live sub-second
    // ticks identically to the native TFs. The DB writer filters these
    // out — sub-second bars live only in the `trades` table.
    let subsec_aggregator = {
        let rx = trade_tx.subscribe();
        let kline_tx = kline_tx.clone();
        tokio::spawn(async move {
            if let Err(e) = ingest::run_subsec_aggregator(
                SYMBOL.to_string(),
                rx,
                kline_tx,
            )
            .await
            {
                warn!(error = ?e, "subsec aggregator task exited with error");
            }
        })
    };

    // Book maintainer: bootstraps via REST, applies depth diffs, exposes
    // shared state for the gateway, and runs the 1s book_snapshots writer.
    let book_maintainer = {
        let pool = pool.clone();
        let rest = binance::rest::RestClient::default();
        let txs = BroadcastTxs {
            kline: kline_tx.clone(),
            trade: trade_tx.clone(),
            depth: depth_tx.clone(),
        };
        let book_state = book_state.clone();
        tokio::spawn(async move {
            if let Err(e) = ingest::run_book_maintainer(
                pool,
                SYMBOL.to_string(),
                rest,
                txs,
                book_state,
            )
            .await
            {
                warn!(error = ?e, "book maintainer task exited with error");
            }
        })
    };

    // Drop bootstrap receivers — the writers' / aggregator's / maintainer's
    // receivers are canonical.
    drop(_kline_bootstrap_rx);
    drop(_trade_bootstrap_rx);
    drop(_depth_bootstrap_rx);

    let ingest_handle = {
        let pool = pool.clone();
        let rest = binance::rest::RestClient::default();
        let txs = BroadcastTxs {
            kline: kline_tx.clone(),
            trade: trade_tx.clone(),
            depth: depth_tx.clone(),
        };
        tokio::spawn(async move {
            if let Err(e) = ingest::run_binance_ingest(
                pool,
                rest,
                txs,
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
        broadcast_tx: kline_tx.clone(),
        trade_tx: trade_tx.clone(),
        depth_tx: depth_tx.clone(),
        book_state: book_state.clone(),
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
    book_maintainer.abort();
    subsec_aggregator.abort();
    trade_writer.abort();
    kline_writer.abort();
    Ok(())
}

fn init_tracing() {
    use tracing_subscriber::{EnvFilter, fmt};
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("server=info,sqlx=warn"));
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
