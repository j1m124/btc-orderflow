//! Database helpers — pool init, migrations, and `candles` CRUD.
//!
//! Queries use the non-macro `sqlx::query`/`query_as` family. We give up the
//! compile-time column check in exchange for not needing a live DB or
//! `.sqlx/` offline cache at build time. At this codebase size the win on
//! ergonomics outweighs the lost safety net (Q14a-3 review).

use anyhow::Result;
use protocol::{BookLevel, BookSnapshotEntry, Candle, FootprintCell, Timeframe, Trade};
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

/// Read the most recent `limit` trades for `symbol`, chronological (oldest
/// first). Used by the trades-channel forwarder for the initial snapshot.
pub async fn fetch_trades_snapshot(
    pool: &PgPool,
    symbol: &str,
    limit: i64,
) -> Result<Vec<Trade>> {
    let rows = sqlx::query(
        "SELECT ts, agg_id, price, qty, is_buyer_maker \
         FROM trades \
         WHERE symbol = $1 \
         ORDER BY ts DESC, agg_id DESC \
         LIMIT $2",
    )
    .bind(symbol)
    .bind(limit)
    .fetch_all(pool)
    .await?;

    let mut trades = build_trades(rows)?;
    trades.reverse();
    Ok(trades)
}

/// Read up to `count` trades strictly older than `before_ms`, chronological.
pub async fn fetch_trades_history_page(
    pool: &PgPool,
    symbol: &str,
    before_ms: i64,
    count: i64,
) -> Result<Vec<Trade>> {
    let before = Utc.timestamp_millis_opt(before_ms).single().unwrap_or_else(|| {
        Utc.timestamp_opt(0, 0).unwrap()
    });

    let rows = sqlx::query(
        "SELECT ts, agg_id, price, qty, is_buyer_maker \
         FROM trades \
         WHERE symbol = $1 AND ts < $2 \
         ORDER BY ts DESC, agg_id DESC \
         LIMIT $3",
    )
    .bind(symbol)
    .bind(before)
    .bind(count)
    .fetch_all(pool)
    .await?;

    let mut trades = build_trades(rows)?;
    trades.reverse();
    Ok(trades)
}

fn build_trades(rows: Vec<sqlx::postgres::PgRow>) -> Result<Vec<Trade>> {
    rows.into_iter()
        .map(|r| -> Result<Trade> {
            let ts: DateTime<Utc> = r.try_get("ts")?;
            Ok(Trade {
                ts_ms: ts.timestamp_millis(),
                agg_id: r.try_get("agg_id")?,
                price: r.try_get("price")?,
                qty: r.try_get("qty")?,
                is_buyer_maker: r.try_get("is_buyer_maker")?,
            })
        })
        .collect()
}

// --- liquidations -----------------------------------------------------------

/// Read the most recent `limit` liquidations for `symbol`, chronological
/// (oldest first). Used by the liquidations-channel forwarder for the
/// initial snapshot.
pub async fn fetch_liquidations_snapshot(
    pool: &PgPool,
    symbol: &str,
    limit: i64,
) -> Result<Vec<protocol::Liquidation>> {
    let rows = sqlx::query(
        "SELECT ts, side, price, qty, quote_qty \
         FROM liquidations \
         WHERE symbol = $1 \
         ORDER BY ts DESC, price DESC, qty DESC \
         LIMIT $2",
    )
    .bind(symbol)
    .bind(limit)
    .fetch_all(pool)
    .await?;

    let mut liqs = build_liquidations(rows)?;
    liqs.reverse();
    Ok(liqs)
}

/// Read up to `count` liquidations strictly older than `before_ms`,
/// chronological (oldest first).
pub async fn fetch_liquidations_history_page(
    pool: &PgPool,
    symbol: &str,
    before_ms: i64,
    count: i64,
) -> Result<Vec<protocol::Liquidation>> {
    let before = Utc
        .timestamp_millis_opt(before_ms)
        .single()
        .unwrap_or_else(|| Utc.timestamp_opt(0, 0).unwrap());

    let rows = sqlx::query(
        "SELECT ts, side, price, qty, quote_qty \
         FROM liquidations \
         WHERE symbol = $1 AND ts < $2 \
         ORDER BY ts DESC, price DESC, qty DESC \
         LIMIT $3",
    )
    .bind(symbol)
    .bind(before)
    .bind(count)
    .fetch_all(pool)
    .await?;

    let mut liqs = build_liquidations(rows)?;
    liqs.reverse();
    Ok(liqs)
}

fn build_liquidations(rows: Vec<sqlx::postgres::PgRow>) -> Result<Vec<protocol::Liquidation>> {
    rows.into_iter()
        .map(|r| -> Result<protocol::Liquidation> {
            let ts: DateTime<Utc> = r.try_get("ts")?;
            let side_str: String = r.try_get("side")?;
            let side = match side_str.as_str() {
                "long" => protocol::LiquidationSide::Long,
                "short" => protocol::LiquidationSide::Short,
                other => return Err(anyhow::anyhow!("unknown liquidation side {other:?} in DB")),
            };
            Ok(protocol::Liquidation {
                ts_ms: ts.timestamp_millis(),
                side,
                price: r.try_get("price")?,
                qty: r.try_get("qty")?,
                quote_qty: r.try_get("quote_qty")?,
            })
        })
        .collect()
}

/// Per-bar liquidation aggregation for the most recent `bars` bars at the
/// given `tf`. Returns chronological (oldest first). Every bar in the
/// covered range emits a row even if its long/short totals are zero — the
/// client distinguishes "no data" (missing row) from "data, none" (zero row).
pub async fn fetch_liquidation_bars_snapshot(
    pool: &PgPool,
    symbol: &str,
    tf: Timeframe,
    bars: i64,
) -> Result<Vec<protocol::LiquidationBar>> {
    let interval = liquidation_bucket_interval(tf);

    let sql = format!(
        "WITH range_bars AS ( \
            SELECT DISTINCT time_bucket(INTERVAL '{interval}', ts) AS bar \
            FROM liquidations \
            WHERE symbol = $1 \
            ORDER BY bar DESC \
            LIMIT $2 \
         ) \
         SELECT rb.bar AS open_time, \
            COALESCE(SUM(CASE WHEN l.side = 'long'  THEN l.qty       END), 0) AS long_qty, \
            COALESCE(SUM(CASE WHEN l.side = 'long'  THEN l.quote_qty END), 0) AS long_quote_qty, \
            COALESCE(SUM(CASE WHEN l.side = 'short' THEN l.qty       END), 0) AS short_qty, \
            COALESCE(SUM(CASE WHEN l.side = 'short' THEN l.quote_qty END), 0) AS short_quote_qty \
         FROM range_bars rb \
         LEFT JOIN liquidations l \
            ON time_bucket(INTERVAL '{interval}', l.ts) = rb.bar \
            AND l.symbol = $1 \
         GROUP BY rb.bar \
         ORDER BY rb.bar"
    );

    let rows = sqlx::query(&sql)
        .bind(symbol)
        .bind(bars)
        .fetch_all(pool)
        .await?;

    build_liquidation_bars(rows)
}

/// Per-bar liquidation aggregation for up to `bars` bars older than
/// `before_ms`. Same row-shape rules as the snapshot.
pub async fn fetch_liquidation_bars_history_page(
    pool: &PgPool,
    symbol: &str,
    tf: Timeframe,
    before_ms: i64,
    bars: i64,
) -> Result<Vec<protocol::LiquidationBar>> {
    let interval = liquidation_bucket_interval(tf);
    let before = Utc
        .timestamp_millis_opt(before_ms)
        .single()
        .unwrap_or_else(|| Utc.timestamp_opt(0, 0).unwrap());

    let sql = format!(
        "WITH range_bars AS ( \
            SELECT DISTINCT time_bucket(INTERVAL '{interval}', ts) AS bar \
            FROM liquidations \
            WHERE symbol = $1 \
              AND time_bucket(INTERVAL '{interval}', ts) < $2 \
            ORDER BY bar DESC \
            LIMIT $3 \
         ) \
         SELECT rb.bar AS open_time, \
            COALESCE(SUM(CASE WHEN l.side = 'long'  THEN l.qty       END), 0) AS long_qty, \
            COALESCE(SUM(CASE WHEN l.side = 'long'  THEN l.quote_qty END), 0) AS long_quote_qty, \
            COALESCE(SUM(CASE WHEN l.side = 'short' THEN l.qty       END), 0) AS short_qty, \
            COALESCE(SUM(CASE WHEN l.side = 'short' THEN l.quote_qty END), 0) AS short_quote_qty \
         FROM range_bars rb \
         LEFT JOIN liquidations l \
            ON time_bucket(INTERVAL '{interval}', l.ts) = rb.bar \
            AND l.symbol = $1 \
         GROUP BY rb.bar \
         ORDER BY rb.bar"
    );

    let rows = sqlx::query(&sql)
        .bind(symbol)
        .bind(before)
        .bind(bars)
        .fetch_all(pool)
        .await?;

    build_liquidation_bars(rows)
}

fn build_liquidation_bars(
    rows: Vec<sqlx::postgres::PgRow>,
) -> Result<Vec<protocol::LiquidationBar>> {
    rows.into_iter()
        .map(|r| -> Result<protocol::LiquidationBar> {
            let bar: DateTime<Utc> = r.try_get("open_time")?;
            Ok(protocol::LiquidationBar {
                open_time: bar.timestamp_millis(),
                long_qty: r.try_get("long_qty")?,
                long_quote_qty: r.try_get("long_quote_qty")?,
                short_qty: r.try_get("short_qty")?,
                short_quote_qty: r.try_get("short_quote_qty")?,
            })
        })
        .collect()
}

/// `time_bucket` interval literal for liquidation-bar aggregation.
/// Liquidation bars cover the same TFs the chart supports; S1/S5 derive
/// from the raw liquidations table (no separate ingest, just finer-grained
/// `time_bucket` windows).
fn liquidation_bucket_interval(tf: Timeframe) -> &'static str {
    match tf {
        Timeframe::S1 => "1 second",
        Timeframe::S5 => "5 seconds",
        Timeframe::M1 => "1 minute",
        Timeframe::M5 => "5 minutes",
        Timeframe::M15 => "15 minutes",
        Timeframe::M30 => "30 minutes",
        Timeframe::H1 => "1 hour",
        Timeframe::H2 => "2 hours",
        Timeframe::H4 => "4 hours",
        Timeframe::H6 => "6 hours",
        Timeframe::D1 => "1 day",
    }
}

// --- footprint (computed on read from `trades`) ----------------------------

/// Pick the time_bucket interval literal for any TF (sub-second OR native).
/// Footprint subscriptions are valid for any TF that the client can chart —
/// the cells aggregate the same `trades` table regardless of bar length.
fn footprint_bucket_interval(tf: Timeframe) -> &'static str {
    match tf {
        Timeframe::S1 => "1 second",
        Timeframe::S5 => "5 seconds",
        Timeframe::M1 => "1 minute",
        Timeframe::M5 => "5 minutes",
        Timeframe::M15 => "15 minutes",
        Timeframe::M30 => "30 minutes",
        Timeframe::H1 => "1 hour",
        Timeframe::H2 => "2 hours",
        Timeframe::H4 => "4 hours",
        Timeframe::H6 => "6 hours",
        Timeframe::D1 => "1 day",
    }
}

/// Footprint cells for the most recent `bars` bars at `(tf, price_bucket)`.
/// Returned chronological (oldest bar first, within each bar buckets are
/// ordered ascending by price).
///
/// Also returns `MAX(agg_id)` over the full `trades` table for `symbol` —
/// captured atomically with the snapshot via a scalar subquery so a live
/// forwarder can use it as a dedup watermark for trades streamed via the
/// broadcast (any trade with `agg_id <= snapshot_max_agg_id` is already
/// summed into the snapshot). `None` when the trades table holds no rows
/// for the symbol (forwarder treats that as "process everything").
pub async fn fetch_footprint_snapshot(
    pool: &PgPool,
    symbol: &str,
    tf: Timeframe,
    price_bucket: f64,
    bars: i64,
) -> Result<(Vec<FootprintCell>, Option<i64>)> {
    let interval = footprint_bucket_interval(tf);

    // Bound the scan to the window covering the most recent `bars` buckets:
    // [aligned_now - (bars-1)*width, +inf). A direct `ts >=` range lets
    // TimescaleDB do chunk exclusion + use the PK index (symbol, ts, agg_id),
    // instead of scanning every trade for the symbol and computing
    // time_bucket() on each row (the old self-join CTE did exactly that, with
    // no time bound — O(48h of trades) per snapshot). `time_bucket` is
    // epoch-aligned for all our widths, so flooring `now` by the width hits the
    // same boundaries the bucket aggregation produces.
    let width_ms = tf.duration_ms().max(1);
    let now_ms = Utc::now().timestamp_millis();
    let lower_ms = (now_ms - now_ms.rem_euclid(width_ms)) - (bars - 1).max(0) * width_ms;
    let lower = Utc
        .timestamp_millis_opt(lower_ms)
        .single()
        .unwrap_or_else(|| Utc.timestamp_opt(0, 0).unwrap());

    let sql = format!(
        "SELECT \
            time_bucket(INTERVAL '{interval}', ts) AS open_time, \
            floor(price / $2) * $2 AS price_bucket_low, \
            coalesce(sum(qty) FILTER (WHERE is_buyer_maker), 0.0) AS bid_vol, \
            coalesce(sum(qty) FILTER (WHERE NOT is_buyer_maker), 0.0) AS ask_vol, \
            (SELECT max(agg_id) FROM trades WHERE symbol = $1 AND ts >= $3) \
                AS snapshot_max_agg_id \
        FROM trades \
        WHERE symbol = $1 AND ts >= $3 \
        GROUP BY open_time, price_bucket_low \
        ORDER BY open_time ASC, price_bucket_low ASC",
    );

    let rows = sqlx::query(&sql)
        .bind(symbol)
        .bind(price_bucket)
        .bind(lower)
        .fetch_all(pool)
        .await?;

    // The scalar subquery is constant across rows; if the result set is
    // empty (no trades for this symbol yet), fall back to a separate
    // query so we still surface "no trades exist" cleanly as None.
    let snapshot_max_agg_id = if let Some(first) = rows.first() {
        first.try_get::<Option<i64>, _>("snapshot_max_agg_id")?
    } else {
        sqlx::query_scalar::<_, Option<i64>>(
            "SELECT max(agg_id) FROM trades WHERE symbol = $1",
        )
        .bind(symbol)
        .fetch_one(pool)
        .await?
    };

    Ok((build_footprint(rows)?, snapshot_max_agg_id))
}

/// Footprint cells for `bars` bars strictly older than `before_open_time_ms`,
/// chronological. Pagination cursor is the open_time of the oldest bar the
/// client already has.
pub async fn fetch_footprint_history_page(
    pool: &PgPool,
    symbol: &str,
    tf: Timeframe,
    price_bucket: f64,
    before_open_time_ms: i64,
    bars: i64,
) -> Result<Vec<FootprintCell>> {
    let interval = footprint_bucket_interval(tf);

    // `before_open_time_ms` is a bucket boundary (an open_time the client
    // already holds), so the `bars` buckets immediately before it occupy
    // exactly [before - bars*width, before). Bounding `ts` to that window
    // (both ends direct comparisons on the partitioning column) lets Timescale
    // prune chunks and ride the PK index, instead of the old self-join CTE that
    // scanned every trade for the symbol and bucketed each row. For BTC perp's
    // continuous tape this returns the same `bars` populated buckets the old
    // DISTINCT-LIMIT did; they only diverge across an ingest gap, where a
    // fixed-time-window page is the more natural "scroll back" unit anyway.
    let width_ms = tf.duration_ms().max(1);
    let lower_ms = before_open_time_ms - bars.max(0) * width_ms;
    let before = Utc
        .timestamp_millis_opt(before_open_time_ms)
        .single()
        .unwrap_or_else(|| Utc.timestamp_opt(0, 0).unwrap());
    let lower = Utc
        .timestamp_millis_opt(lower_ms)
        .single()
        .unwrap_or_else(|| Utc.timestamp_opt(0, 0).unwrap());

    let sql = format!(
        "SELECT \
            time_bucket(INTERVAL '{interval}', ts) AS open_time, \
            floor(price / $4) * $4 AS price_bucket_low, \
            coalesce(sum(qty) FILTER (WHERE is_buyer_maker), 0.0) AS bid_vol, \
            coalesce(sum(qty) FILTER (WHERE NOT is_buyer_maker), 0.0) AS ask_vol \
        FROM trades \
        WHERE symbol = $1 AND ts >= $2 AND ts < $3 \
        GROUP BY open_time, price_bucket_low \
        ORDER BY open_time ASC, price_bucket_low ASC",
    );

    let rows = sqlx::query(&sql)
        .bind(symbol)
        .bind(lower)
        .bind(before)
        .bind(price_bucket)
        .fetch_all(pool)
        .await?;

    build_footprint(rows)
}

fn build_footprint(rows: Vec<sqlx::postgres::PgRow>) -> Result<Vec<FootprintCell>> {
    rows.into_iter()
        .map(|r| -> Result<FootprintCell> {
            let open_time: DateTime<Utc> = r.try_get("open_time")?;
            Ok(FootprintCell {
                open_time: open_time.timestamp_millis(),
                price_bucket_low: r.try_get("price_bucket_low")?,
                bid_vol: r.try_get("bid_vol")?,
                ask_vol: r.try_get("ask_vol")?,
            })
        })
        .collect()
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

/// Read up to `count` historical book snapshots for `symbol` strictly older
/// than `before_ms`, chronological. Feeds the heatmap's history-page scroll.
pub async fn fetch_book_history_page(
    pool: &PgPool,
    symbol: &str,
    before_ms: i64,
    count: i64,
) -> Result<Vec<BookSnapshotEntry>> {
    let before = Utc.timestamp_millis_opt(before_ms).single().unwrap_or_else(|| {
        Utc.timestamp_opt(0, 0).unwrap()
    });

    let rows = sqlx::query(
        "SELECT ts, bid_prices, bid_sizes, ask_prices, ask_sizes \
         FROM book_snapshots \
         WHERE symbol = $1 AND ts < $2 \
         ORDER BY ts DESC \
         LIMIT $3",
    )
    .bind(symbol)
    .bind(before)
    .bind(count)
    .fetch_all(pool)
    .await?;

    let mut entries: Vec<BookSnapshotEntry> = rows
        .into_iter()
        .map(|r| -> Result<BookSnapshotEntry> {
            let ts: DateTime<Utc> = r.try_get("ts")?;
            let bid_prices: Vec<f64> = r.try_get("bid_prices")?;
            let bid_sizes: Vec<f64> = r.try_get("bid_sizes")?;
            let ask_prices: Vec<f64> = r.try_get("ask_prices")?;
            let ask_sizes: Vec<f64> = r.try_get("ask_sizes")?;
            Ok(BookSnapshotEntry {
                ts_ms: ts.timestamp_millis(),
                bids: zip_levels(&bid_prices, &bid_sizes),
                asks: zip_levels(&ask_prices, &ask_sizes),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    entries.reverse();
    Ok(entries)
}

fn zip_levels(prices: &[f64], sizes: &[f64]) -> Vec<BookLevel> {
    prices
        .iter()
        .zip(sizes.iter())
        .map(|(p, s)| BookLevel { price: *p, size: *s })
        .collect()
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

/// Bulk-UPSERT a window of liquidation rows. PK is `(symbol, ts, price,
/// qty)`; `ON CONFLICT DO NOTHING` absorbs redelivery on reconnect.
///
/// 6 params/row × 10900 ≈ row cap before the 65535 Postgres-parameter
/// ceiling matters. Liquidations are sparse (≤1/sec/symbol upstream), so
/// per-flush batches are tiny — the cap is purely defensive.
pub async fn upsert_liquidations(
    pool: &PgPool,
    symbol: &str,
    rows: &[crate::binance::parse::LiquidationRow],
) -> Result<()> {
    if rows.is_empty() {
        return Ok(());
    }

    let mut qb: QueryBuilder<sqlx::Postgres> = QueryBuilder::new(
        "INSERT INTO liquidations (symbol, ts, price, qty, quote_qty, side) ",
    );

    qb.push_values(rows.iter(), |mut b, row| {
        b.push_bind(symbol)
            .push_bind(row.ts)
            .push_bind(row.price)
            .push_bind(row.qty)
            .push_bind(row.quote_qty)
            .push_bind(row.side.as_db_str());
    });

    qb.push(" ON CONFLICT (symbol, ts, price, qty) DO NOTHING");
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
