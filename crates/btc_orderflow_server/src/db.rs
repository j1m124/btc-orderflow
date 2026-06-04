//! Database helpers — pool init, migrations, and `candles` CRUD.
//!
//! Queries use the non-macro `sqlx::query`/`query_as` family. We give up the
//! compile-time column check in exchange for not needing a live DB or
//! `.sqlx/` offline cache at build time. At this codebase size the win on
//! ergonomics outweighs the lost safety net (Q14a-3 review).

use anyhow::Result;
use chrono::{DateTime, Utc};
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
