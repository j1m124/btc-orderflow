//! Database helpers — pool init, migrations, and `candles` CRUD.
//!
//! Queries use the non-macro `sqlx::query`/`query_as` family. We give up the
//! compile-time column check in exchange for not needing a live DB or
//! `.sqlx/` offline cache at build time. At this codebase size the win on
//! ergonomics outweighs the lost safety net (Q14a-3 review).

use anyhow::Result;
use protocol::Candle;
use chrono::{DateTime, TimeZone, Utc};
use sqlx::{PgPool, QueryBuilder, Row};
use tracing::info;

use crate::binance::parse::KlineRow;

/// Run every migration in `migrations/` that hasn't been applied yet.
pub async fn run_migrations(pool: &PgPool) -> Result<()> {
    info!("applying pending migrations");
    sqlx::migrate!("./migrations").run(pool).await?;
    info!("migrations up to date");
    Ok(())
}

/// Latest stored `open_time` for `(symbol, tf)`, or `None` if no rows.
pub async fn max_open_time(
    pool: &PgPool,
    symbol: &str,
    tf: &str,
) -> Result<Option<DateTime<Utc>>> {
    let row = sqlx::query("SELECT MAX(open_time) AS t FROM candles WHERE symbol = $1 AND tf = $2")
        .bind(symbol)
        .bind(tf)
        .fetch_one(pool)
        .await?;
    let ts: Option<DateTime<Utc>> = row.try_get("t")?;
    Ok(ts)
}

/// UPSERT a batch of klines for `(symbol, tf)`. Uses one multi-row INSERT
/// with an ON CONFLICT clause keyed on the primary key — idempotent against
/// the gap-heal/WS-ingest overlap when a bar finalizes mid-backfill.
///
/// Postgres has a 65535-parameter cap per query and we bind 12 params per
/// row, so the safe ceiling per call is ~5460 rows. Binance's 1500-row REST
/// page is well under that.
pub async fn upsert_klines(
    pool: &PgPool,
    symbol: &str,
    tf: &str,
    rows: &[KlineRow],
) -> Result<()> {
    if rows.is_empty() {
        return Ok(());
    }

    let mut qb: QueryBuilder<sqlx::Postgres> = QueryBuilder::new(
        "INSERT INTO candles (\
            symbol, tf, open_time, close_time, \
            open, high, low, close, volume, \
            quote_volume, trades, taker_buy_vol\
         ) ",
    );

    qb.push_values(rows.iter(), |mut b, row| {
        b.push_bind(symbol)
            .push_bind(tf)
            .push_bind(row.open_time)
            .push_bind(row.close_time)
            .push_bind(row.open)
            .push_bind(row.high)
            .push_bind(row.low)
            .push_bind(row.close)
            .push_bind(row.volume)
            .push_bind(Some(row.quote_volume))
            .push_bind(Some(row.trades))
            .push_bind(Some(row.taker_buy_vol));
    });

    qb.push(
        " ON CONFLICT (symbol, tf, open_time) DO UPDATE SET \
            close_time    = EXCLUDED.close_time, \
            open          = EXCLUDED.open, \
            high          = EXCLUDED.high, \
            low           = EXCLUDED.low, \
            close         = EXCLUDED.close, \
            volume        = EXCLUDED.volume, \
            quote_volume  = EXCLUDED.quote_volume, \
            trades        = EXCLUDED.trades, \
            taker_buy_vol = EXCLUDED.taker_buy_vol",
    );

    qb.build().execute(pool).await?;
    Ok(())
}

/// Read the most recent `limit` closed bars for `(symbol, tf)` in
/// chronological order. Returns the wire-narrow [`Candle`] shape (i64-ms
/// timestamps; DB extras like `quote_volume` are dropped).
pub async fn fetch_snapshot(
    pool: &PgPool,
    symbol: &str,
    tf: &str,
    limit: i64,
) -> Result<Vec<Candle>> {
    let rows = sqlx::query(
        "SELECT open_time, close_time, open, high, low, close, volume \
         FROM candles \
         WHERE symbol = $1 AND tf = $2 \
         ORDER BY open_time DESC \
         LIMIT $3",
    )
    .bind(symbol)
    .bind(tf)
    .bind(limit)
    .fetch_all(pool)
    .await?;

    let mut candles: Vec<Candle> = rows
        .into_iter()
        .map(|r| -> Result<Candle> {
            let open_time: DateTime<Utc> = r.try_get("open_time")?;
            let close_time: DateTime<Utc> = r.try_get("close_time")?;
            Ok(Candle {
                open_time: open_time.timestamp_millis(),
                close_time: close_time.timestamp_millis(),
                open: r.try_get("open")?,
                high: r.try_get("high")?,
                low: r.try_get("low")?,
                close: r.try_get("close")?,
                volume: r.try_get("volume")?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    candles.reverse();
    Ok(candles)
}

/// Read up to `count` closed bars for `(symbol, tf)` strictly older than
/// `before_ms`, returned chronologically (oldest first).
pub async fn fetch_history_page(
    pool: &PgPool,
    symbol: &str,
    tf: &str,
    before_ms: i64,
    count: i64,
) -> Result<Vec<Candle>> {
    let before = Utc.timestamp_millis_opt(before_ms).single().unwrap_or_else(|| {
        // Saturate to the epoch in the impossible-input case; callers
        // querying with absurd timestamps just get nothing.
        Utc.timestamp_opt(0, 0).unwrap()
    });

    let rows = sqlx::query(
        "SELECT open_time, close_time, open, high, low, close, volume \
         FROM candles \
         WHERE symbol = $1 AND tf = $2 AND open_time < $3 \
         ORDER BY open_time DESC \
         LIMIT $4",
    )
    .bind(symbol)
    .bind(tf)
    .bind(before)
    .bind(count)
    .fetch_all(pool)
    .await?;

    let mut candles: Vec<Candle> = rows
        .into_iter()
        .map(|r| -> Result<Candle> {
            let open_time: DateTime<Utc> = r.try_get("open_time")?;
            let close_time: DateTime<Utc> = r.try_get("close_time")?;
            Ok(Candle {
                open_time: open_time.timestamp_millis(),
                close_time: close_time.timestamp_millis(),
                open: r.try_get("open")?,
                high: r.try_get("high")?,
                low: r.try_get("low")?,
                close: r.try_get("close")?,
                volume: r.try_get("volume")?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    candles.reverse();
    Ok(candles)
}
