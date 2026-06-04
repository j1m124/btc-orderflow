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
use tracing::{debug, info};

use crate::binance::{KLINES_PAGE_LIMIT, parse::KlineRow, rest::RestClient};
use crate::db;

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
