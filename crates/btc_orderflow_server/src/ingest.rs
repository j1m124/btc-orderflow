//! Boot-time + reconnect-time gap-heal flow.
//!
//! For each (symbol, tf) the server tracks:
//!   1. Read `MAX(open_time)` from the DB.
//!   2. Set start = last+1ms, or (now − cold-start window) if the DB is empty.
//!   3. Loop: GET /fapi/v1/klines?startTime=&limit=1500, UPSERT, advance the
//!      cursor to the last row's open_time+1ms. Stop when the response is
//!      shorter than the page size (we've caught up).
//!
//! Same algorithm runs on boot (cold start or after restart) and after every
//! WS reconnect. Per-tf gap-heals run in parallel — 9 tasks for the v1
//! configuration is trivial and stays well under Binance's REST weight cap.

use anyhow::{Context, Result};
use btc_orderflow_protocol::Timeframe;
use chrono::{Duration as ChronoDuration, Utc};
use futures::future::try_join_all;
use sqlx::PgPool;
use std::time::Duration as StdDuration;
use tokio::sync::broadcast;
use tracing::{debug, info, warn};

use crate::binance::{
    KLINES_PAGE_LIMIT,
    parse::{KlineRow, Tick},
    rest::RestClient,
    ws,
};
use crate::db;

/// Capacity of the Binance-tick broadcast channel. Slow consumers exceeding
/// this fall behind and recover by either skipping or resubscribing; the
/// gateway turns a `RecvError::Lagged` into a `Resnap` for affected
/// subscriptions (Q14a-7).
pub const BROADCAST_CAPACITY: usize = 4096;

/// Min/max wait between Binance WS reconnect attempts.
const RECONNECT_MIN: StdDuration = StdDuration::from_secs(1);
const RECONNECT_MAX: StdDuration = StdDuration::from_secs(30);

/// Backfill one (symbol, tf) up to "now", picking up where the DB left off
/// or starting `cold_start` ago for a fresh table. Returns the number of
/// rows upserted.
pub async fn backfill_one(
    pool: &PgPool,
    rest: &RestClient,
    symbol: &str,
    tf: Timeframe,
    cold_start: ChronoDuration,
) -> Result<usize> {
    let now = Utc::now();

    let last = db::max_open_time(pool, symbol, tf.as_str())
        .await
        .context("query MAX(open_time)")?;

    let mut cursor_ms = match last {
        Some(t) => t.timestamp_millis() + 1,
        None => (now - cold_start).timestamp_millis(),
    };

    let now_ms = now.timestamp_millis();
    let mut total = 0usize;

    loop {
        if cursor_ms >= now_ms {
            break;
        }

        let rows = rest
            .klines(symbol, tf.as_str(), cursor_ms, KLINES_PAGE_LIMIT)
            .await
            .with_context(|| format!("klines page for {symbol} {} from {cursor_ms}", tf.as_str()))?;

        if rows.is_empty() {
            break;
        }

        let page_size = rows.len();
        let last_open_ms = rows.last().map(KlineRow::open_time_ms).unwrap_or(cursor_ms);

        db::upsert_klines(pool, symbol, tf.as_str(), &rows)
            .await
            .with_context(|| format!("upsert klines for {symbol} {}", tf.as_str()))?;

        total += page_size;
        cursor_ms = last_open_ms + 1;

        debug!(
            symbol,
            tf = tf.as_str(),
            page_size,
            total,
            "backfill page applied"
        );

        if (page_size as u32) < KLINES_PAGE_LIMIT {
            break;
        }
    }

    info!(symbol, tf = tf.as_str(), rows = total, "backfill complete");
    Ok(total)
}

/// Backfill every timeframe in `Timeframe::ALL` for `symbol`, in parallel.
/// Returns the per-tf row counts in `Timeframe::ALL` order.
pub async fn backfill_symbol(
    pool: &PgPool,
    rest: &RestClient,
    symbol: &str,
    cold_start: ChronoDuration,
) -> Result<Vec<usize>> {
    let futures = Timeframe::ALL.into_iter().map(|tf| {
        let pool = pool.clone();
        let rest = rest;
        let symbol = symbol.to_string();
        async move { backfill_one(&pool, rest, &symbol, tf, cold_start).await }
    });
    try_join_all(futures).await
}

// --- Live ingest orchestrator ----------------------------------------------

/// Drain the broadcast channel forever, UPSERTing every **closed** kline
/// into the `candles` table. Open (in-progress) bars are streamed to
/// gateway clients but not persisted — only the final bar is canonical, and
/// the next closed-bar UPSERT replaces any earlier persisted state.
///
/// On `RecvError::Lagged`, log and skip — the broadcast capacity is large
/// enough that lag indicates the DB is very slow, not a normal blip.
pub async fn run_db_writer(
    pool: PgPool,
    mut rx: broadcast::Receiver<Tick>,
) -> Result<()> {
    info!("db writer task started");
    loop {
        match rx.recv().await {
            Ok(tick) if tick.is_closed => {
                let rows = std::slice::from_ref(&tick.kline);
                if let Err(e) =
                    db::upsert_klines(&pool, &tick.symbol, tick.tf.as_str(), rows).await
                {
                    warn!(
                        symbol = %tick.symbol,
                        tf = tick.tf.as_str(),
                        error = ?e,
                        "db upsert failed"
                    );
                }
            }
            Ok(_) => { /* open bar; skip persistence */ }
            Err(broadcast::error::RecvError::Lagged(n)) => {
                warn!(skipped = n, "db writer lagged behind broadcast");
            }
            Err(broadcast::error::RecvError::Closed) => {
                info!("broadcast closed; db writer exiting");
                return Ok(());
            }
        }
    }
}

/// Run the Binance ingest loop forever: gap-heal → connect WS → stream until
/// disconnect → backoff → repeat. The first gap-heal is the cold-start; every
/// subsequent one is just the gap accumulated during the prior outage.
pub async fn run_binance_ingest(
    pool: PgPool,
    rest: RestClient,
    broadcast_tx: broadcast::Sender<Tick>,
    symbol: String,
    cold_start: ChronoDuration,
) -> Result<()> {
    let mut backoff = RECONNECT_MIN;
    info!(symbol = %symbol, "binance ingest task started");
    loop {
        // Heal any gap between the latest DB row and now. Cheap after the
        // first iteration (just a few seconds of bars to catch up).
        match backfill_symbol(&pool, &rest, &symbol, cold_start).await {
            Ok(counts) => {
                for (tf, n) in Timeframe::ALL.iter().zip(counts.iter()) {
                    if *n > 0 {
                        debug!(tf = tf.as_str(), rows = n, "gap-heal applied");
                    }
                }
            }
            Err(e) => {
                warn!(error = ?e, "gap-heal failed; will retry on next reconnect cycle");
            }
        }

        // Stream until the connection drops or errors.
        match ws::connect_and_stream(&symbol, &broadcast_tx).await {
            Ok(()) => {
                info!("binance ws closed cleanly; reconnecting");
                backoff = RECONNECT_MIN;
            }
            Err(e) => {
                warn!(
                    error = ?e,
                    backoff_ms = backoff.as_millis() as u64,
                    "binance ws error; reconnecting after backoff"
                );
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(RECONNECT_MAX);
            }
        }
    }
}
