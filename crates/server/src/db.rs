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

use crate::binance::parse::{KlineRow, TradeRow};

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
/// timestamps).
pub async fn fetch_snapshot(
    pool: &PgPool,
    symbol: &str,
    tf: &str,
    limit: i64,
) -> Result<Vec<Candle>> {
    let rows = sqlx::query(
        "SELECT open_time, close_time, open, high, low, close, volume, \
                quote_volume, trades, taker_buy_vol \
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
                quote_volume: r.try_get("quote_volume")?,
                trades: r.try_get("trades")?,
                taker_buy_vol: r.try_get("taker_buy_vol")?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    candles.reverse();
    Ok(candles)
}

// --- trades ----------------------------------------------------------------

/// Latest stored `agg_id` for `symbol`, or `None` if no rows. Used as the
/// pagination cursor for the trade gap-heal loop: the next REST call asks
/// for `from_id = max_agg_id + 1`.
pub async fn max_trade_agg_id(pool: &PgPool, symbol: &str) -> Result<Option<i64>> {
    let row = sqlx::query("SELECT MAX(agg_id) AS m FROM trades WHERE symbol = $1")
        .bind(symbol)
        .fetch_one(pool)
        .await?;
    Ok(row.try_get("m")?)
}

/// Latest stored trade `ts` for `symbol`, or `None` if no rows. Used by the
/// gap-heal cap check — outages longer than the cap skip backfill.
pub async fn max_trade_ts(pool: &PgPool, symbol: &str) -> Result<Option<DateTime<Utc>>> {
    let row = sqlx::query("SELECT MAX(ts) AS t FROM trades WHERE symbol = $1")
        .bind(symbol)
        .fetch_one(pool)
        .await?;
    Ok(row.try_get("t")?)
}

// --- book_snapshots --------------------------------------------------------

/// Insert one top-N book snapshot row. `bids` and `asks` are best-first;
/// parallel arrays (prices + sizes) are stored as columnar `double precision[]`.
/// ON CONFLICT (symbol, ts) DO NOTHING — the 1s snapshot timer is monotonic
/// in wall time so duplicates would only arise from clock skew on restart.
pub async fn upsert_book_snapshot(
    pool: &PgPool,
    symbol: &str,
    ts: DateTime<Utc>,
    bids: &[(f64, f64)],
    asks: &[(f64, f64)],
) -> Result<()> {
    let bid_prices: Vec<f64> = bids.iter().map(|(p, _)| *p).collect();
    let bid_sizes: Vec<f64> = bids.iter().map(|(_, s)| *s).collect();
    let ask_prices: Vec<f64> = asks.iter().map(|(p, _)| *p).collect();
    let ask_sizes: Vec<f64> = asks.iter().map(|(_, s)| *s).collect();

    sqlx::query(
        "INSERT INTO book_snapshots \
            (symbol, ts, bid_prices, bid_sizes, ask_prices, ask_sizes) \
         VALUES ($1, $2, $3, $4, $5, $6) \
         ON CONFLICT (symbol, ts) DO NOTHING",
    )
    .bind(symbol)
    .bind(ts)
    .bind(&bid_prices)
    .bind(&bid_sizes)
    .bind(&ask_prices)
    .bind(&ask_sizes)
    .execute(pool)
    .await?;
    Ok(())
}

/// UPSERT a batch of trades. ON CONFLICT DO NOTHING because aggTrades are
/// immutable once Binance issues them — re-ingest (gap-heal overlapping a
/// live WS event) is idempotent and the existing row is canonical.
///
/// Postgres has a 65535-parameter cap per query; we bind 6 params per row,
/// so the safe ceiling per call is ~10900 rows. Binance's 1000-row REST
/// page (and the 100ms broadcast batch from the live stream) sit well under.
pub async fn upsert_trades(pool: &PgPool, symbol: &str, rows: &[TradeRow]) -> Result<()> {
    if rows.is_empty() {
        return Ok(());
    }

    let mut qb: QueryBuilder<sqlx::Postgres> = QueryBuilder::new(
        "INSERT INTO trades (symbol, ts, agg_id, price, qty, is_buyer_maker) ",
    );

    qb.push_values(rows.iter(), |mut b, row| {
        b.push_bind(symbol)
            .push_bind(row.ts)
            .push_bind(row.agg_id)
            .push_bind(row.price)
            .push_bind(row.qty)
            .push_bind(row.is_buyer_maker);
    });

    qb.push(" ON CONFLICT (symbol, ts, agg_id) DO NOTHING");
    qb.build().execute(pool).await?;
    Ok(())
}

// --- Sub-second candle synthesis from trades --------------------------------

/// Translate a TF into the Postgres interval literal we feed to `time_bucket`.
/// Only S1/S5 are synthesized off trades; native-kline TFs read from the
/// `candles` table instead.
fn subsec_bucket_interval(tf: protocol::Timeframe) -> &'static str {
    match tf {
        protocol::Timeframe::S1 => "1 second",
        protocol::Timeframe::S5 => "5 seconds",
        _ => panic!("subsec_bucket_interval called with non-subsec TF: {tf:?}"),
    }
}

/// Synthesize the most recent `limit` S1/S5 candles for `symbol` by bucketing
/// the `trades` hypertable. Uses Timescale's `first` / `last` ordered
/// aggregates over `agg_id` to lock down OHLC ordering even when multiple
/// trades share a millisecond. Empty buckets (no trades in that second)
/// don't appear — BTCUSDT volume makes that vanishingly rare for S5 and
/// only occasional for S1.
pub async fn fetch_subsec_snapshot(
    pool: &PgPool,
    symbol: &str,
    tf: protocol::Timeframe,
    limit: i64,
) -> Result<Vec<Candle>> {
    let interval = subsec_bucket_interval(tf);
    let sql = format!(
        "SELECT \
            time_bucket(INTERVAL '{interval}', ts) AS open_time, \
            first(price, agg_id) AS open, \
            max(price) AS high, \
            min(price) AS low, \
            last(price, agg_id) AS close, \
            sum(qty) AS volume, \
            sum(qty * price) AS quote_volume, \
            count(*)::int AS trades, \
            sum(CASE WHEN NOT is_buyer_maker THEN qty ELSE 0 END) AS taker_buy_vol \
         FROM trades \
         WHERE symbol = $1 \
         GROUP BY open_time \
         ORDER BY open_time DESC \
         LIMIT $2",
    );

    let rows = sqlx::query(&sql)
        .bind(symbol)
        .bind(limit)
        .fetch_all(pool)
        .await?;

    let bar_ms = tf.duration_ms();
    let mut candles = build_subsec_candles(rows, bar_ms)?;
    candles.reverse();
    Ok(candles)
}

/// Synthesize up to `count` S1/S5 candles strictly older than `before_ms`
/// for `symbol`. Same shape as the snapshot but with an exclusive upper bound.
pub async fn fetch_subsec_history_page(
    pool: &PgPool,
    symbol: &str,
    tf: protocol::Timeframe,
    before_ms: i64,
    count: i64,
) -> Result<Vec<Candle>> {
    let interval = subsec_bucket_interval(tf);
    let before = Utc.timestamp_millis_opt(before_ms).single().unwrap_or_else(|| {
        Utc.timestamp_opt(0, 0).unwrap()
    });
    let sql = format!(
        "SELECT \
            time_bucket(INTERVAL '{interval}', ts) AS open_time, \
            first(price, agg_id) AS open, \
            max(price) AS high, \
            min(price) AS low, \
            last(price, agg_id) AS close, \
            sum(qty) AS volume, \
            sum(qty * price) AS quote_volume, \
            count(*)::int AS trades, \
            sum(CASE WHEN NOT is_buyer_maker THEN qty ELSE 0 END) AS taker_buy_vol \
         FROM trades \
         WHERE symbol = $1 AND ts < $2 \
         GROUP BY open_time \
         ORDER BY open_time DESC \
         LIMIT $3",
    );

    let rows = sqlx::query(&sql)
        .bind(symbol)
        .bind(before)
        .bind(count)
        .fetch_all(pool)
        .await?;

    let bar_ms = tf.duration_ms();
    let mut candles = build_subsec_candles(rows, bar_ms)?;
    candles.reverse();
    Ok(candles)
}

fn build_subsec_candles(
    rows: Vec<sqlx::postgres::PgRow>,
    bar_ms: i64,
) -> Result<Vec<Candle>> {
    rows.into_iter()
        .map(|r| -> Result<Candle> {
            let open_time: DateTime<Utc> = r.try_get("open_time")?;
            let open_time_ms = open_time.timestamp_millis();
            Ok(Candle {
                open_time: open_time_ms,
                close_time: open_time_ms + bar_ms - 1,
                open: r.try_get("open")?,
                high: r.try_get("high")?,
                low: r.try_get("low")?,
                close: r.try_get("close")?,
                volume: r.try_get("volume")?,
                quote_volume: Some(r.try_get("quote_volume")?),
                trades: Some(r.try_get("trades")?),
                taker_buy_vol: Some(r.try_get("taker_buy_vol")?),
            })
        })
        .collect()
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
        "SELECT open_time, close_time, open, high, low, close, volume, \
                quote_volume, trades, taker_buy_vol \
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
                quote_volume: r.try_get("quote_volume")?,
                trades: r.try_get("trades")?,
                taker_buy_vol: r.try_get("taker_buy_vol")?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    candles.reverse();
    Ok(candles)
}
