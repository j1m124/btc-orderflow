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
use protocol::Timeframe;
use chrono::{Duration as ChronoDuration, Utc};
use futures::future::try_join_all;
use sqlx::PgPool;
use std::{collections::HashMap, time::Duration as StdDuration};
use tokio::sync::broadcast;
use tracing::{debug, info, warn};

use crate::binance::{
    AGGTRADES_PAGE_LIMIT, BroadcastTxs, KLINES_PAGE_LIMIT,
    parse::{KlineRow, Tick, TradeRow, TradeTick},
    rest::RestClient,
    ws,
};
use crate::db;

/// Capacity of the Binance-kline broadcast channel. Slow consumers exceeding
/// this fall behind and recover by either skipping or resubscribing; the
/// gateway turns a `RecvError::Lagged` into a `Resnap` for affected
/// subscriptions (Q14a-7).
pub const BROADCAST_CAPACITY: usize = 4096;

/// Capacity of the trade broadcast channel. ~100-200 aggTrades/sec at peak,
/// 100ms gateway batching → headroom for ~20s of buffering before any
/// consumer needs to either drain or accept a `Lagged` resync. Larger than
/// the kline channel because tick rate is two orders of magnitude higher.
pub const TRADE_BROADCAST_CAPACITY: usize = 32_768;

/// Min/max wait between Binance WS reconnect attempts.
const RECONNECT_MIN: StdDuration = StdDuration::from_secs(1);
const RECONNECT_MAX: StdDuration = StdDuration::from_secs(30);

/// Hard cap on a single trade gap-heal operation. Outages longer than this
/// would burn the REST weight budget (each aggTrades page = 20 weight, IP
/// cap = 2400/min). 60min × 100 trades/sec ÷ 1000/page × 20 = ~7200 weight,
/// already 3× the per-minute cap; anything bigger is impractical.
const TRADE_BACKFILL_CAP: ChronoDuration = ChronoDuration::minutes(60);

/// Cold-start window when the trades table is empty (no cursor to resume
/// from). Forgivable blank period for fresh-boot UX vs. weight burned.
const TRADE_COLD_START: ChronoDuration = ChronoDuration::minutes(15);

/// Pause between paginated aggTrades REST calls during backfill. 600ms keeps
/// the loop under the 2400-weight/min IP budget (~100 calls/min × 20 weight
/// = 2000 weight/min with headroom for the parallel kline gap-heal).
const AGGTRADE_PAGE_DELAY: StdDuration = StdDuration::from_millis(600);

/// Flush cadence for the trade writer's row buffer. Trades arrive at high
/// rate; batching 100ms cuts upsert count ~10x without adding noticeable
/// latency to the snapshot/footprint query path.
const TRADE_WRITER_FLUSH: StdDuration = StdDuration::from_millis(100);

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

/// Backfill every native-kline timeframe in `Timeframe::ALL` for `symbol`,
/// in parallel. Returns the per-tf row counts in iteration order.
///
/// S1/S5 are skipped — there's no Binance REST kline endpoint for those
/// intervals (they're synthesized from the aggTrade stream).
pub async fn backfill_symbol(
    pool: &PgPool,
    rest: &RestClient,
    symbol: &str,
    cold_start: ChronoDuration,
) -> Result<Vec<usize>> {
    let futures = Timeframe::ALL
        .into_iter()
        .filter(|tf| tf.is_native_kline())
        .map(|tf| {
            let pool = pool.clone();
            let rest = rest;
            let symbol = symbol.to_string();
            async move { backfill_one(&pool, rest, &symbol, tf, cold_start).await }
        });
    try_join_all(futures).await
}

// --- Trade backfill --------------------------------------------------------

/// Catch the `trades` table up to "now" via REST. Cursor-driven against
/// `MAX(agg_id)`: each call asks for `from_id = max + 1` and we advance by
/// the last id in the returned page. Hard-caps at [`TRADE_BACKFILL_CAP`] —
/// outages longer than that skip backfill entirely (the table just stays
/// sparse for that window). Cold start (empty table) uses
/// [`TRADE_COLD_START`] as the initial `startTime`.
///
/// Returns the number of trades upserted.
pub async fn backfill_trades(
    pool: &PgPool,
    rest: &RestClient,
    symbol: &str,
) -> Result<usize> {
    let now = Utc::now();

    let cursor_id = db::max_trade_agg_id(pool, symbol)
        .await
        .context("query MAX(agg_id)")?;

    // Decide initial request shape. With a known agg_id we paginate by
    // fromId; cold start uses startTime instead (Binance forbids both).
    let (mut from_id, mut start_time_ms): (Option<i64>, Option<i64>) = match cursor_id {
        Some(id) => (Some(id + 1), None),
        None => {
            let start = (now - TRADE_COLD_START).timestamp_millis();
            (None, Some(start))
        }
    };

    // Gap-size check. Only applies when we have a real cursor — cold start
    // is bounded by TRADE_COLD_START itself.
    if cursor_id.is_some() {
        if let Some(ts) = db::max_trade_ts(pool, symbol).await? {
            let gap = now - ts;
            if gap > TRADE_BACKFILL_CAP {
                warn!(
                    symbol,
                    gap_minutes = gap.num_minutes(),
                    cap_minutes = TRADE_BACKFILL_CAP.num_minutes(),
                    "trade gap exceeds cap; skipping backfill (table will stay sparse for the outage window)"
                );
                return Ok(0);
            }
        }
    }

    let mut total: usize = 0;
    let mut pages: usize = 0;

    loop {
        let rows = rest
            .agg_trades(symbol, from_id, start_time_ms, AGGTRADES_PAGE_LIMIT)
            .await
            .with_context(|| format!("aggTrades page for {symbol}"))?;

        if rows.is_empty() {
            break;
        }

        let page_size = rows.len();
        let last_id = rows.last().map(|r| r.agg_id).unwrap_or(0);
        let last_ts_ms = rows.last().map(|r| r.ts.timestamp_millis()).unwrap_or(0);

        db::upsert_trades(pool, symbol, &rows)
            .await
            .with_context(|| format!("upsert aggTrades for {symbol}"))?;

        total += page_size;
        pages += 1;

        debug!(
            symbol,
            page_size,
            total,
            last_id,
            "trade backfill page applied"
        );

        // Caught up to ~now: any page smaller than the limit means Binance
        // returned everything available so far.
        if (page_size as u32) < AGGTRADES_PAGE_LIMIT {
            break;
        }
        // Bail if we'd overshoot the cap mid-backfill (e.g. the cap check
        // above didn't hit because the gap was just under it but pages keep
        // arriving — pathological but cheap to guard).
        if last_ts_ms >= now.timestamp_millis() {
            break;
        }

        // Advance cursor and switch to fromId for subsequent pages.
        from_id = Some(last_id + 1);
        start_time_ms = None;

        // Pace against the 2400-weight/min IP budget.
        tokio::time::sleep(AGGTRADE_PAGE_DELAY).await;
    }

    if total > 0 {
        info!(symbol, rows = total, pages, "trade backfill complete");
    }
    Ok(total)
}

// --- Live ingest orchestrator ----------------------------------------------

/// Drain the trade broadcast forever, batching trades by symbol for
/// [`TRADE_WRITER_FLUSH`] and bulk-upserting each window. Persisted rows
/// are the source of truth for footprint / volume profile / volume delta /
/// trade-tape queries — live forwarders read from the broadcast directly.
///
/// On `RecvError::Lagged`, log and skip — broadcast capacity is sized so
/// lag indicates the DB is very slow.
pub async fn run_trade_writer(
    pool: PgPool,
    mut rx: broadcast::Receiver<TradeTick>,
) -> Result<()> {
    info!("trade writer task started");
    let mut buffer: HashMap<String, Vec<TradeRow>> = HashMap::new();
    let mut flush_timer = tokio::time::interval(TRADE_WRITER_FLUSH);
    // Skip the immediate-fire of the first tick; nothing to flush yet.
    flush_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            biased;
            msg = rx.recv() => {
                match msg {
                    Ok(tick) => {
                        buffer.entry(tick.symbol).or_default().push(tick.trade);
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!(skipped = n, "trade writer lagged behind broadcast");
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        info!("trade broadcast closed; flushing and exiting");
                        flush_buffer(&pool, &mut buffer).await;
                        return Ok(());
                    }
                }
            }
            _ = flush_timer.tick() => {
                flush_buffer(&pool, &mut buffer).await;
            }
        }
    }
}

/// Drain the per-symbol buffer into one bulk UPSERT per symbol.
async fn flush_buffer(pool: &PgPool, buffer: &mut HashMap<String, Vec<TradeRow>>) {
    for (symbol, trades) in buffer.drain() {
        if trades.is_empty() {
            continue;
        }
        let count = trades.len();
        if let Err(e) = db::upsert_trades(pool, &symbol, &trades).await {
            warn!(symbol, count, error = ?e, "trade upsert failed");
        }
    }
}

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

/// Run the Binance ingest loop forever: kline+trade gap-heal in parallel →
/// connect WS → stream until disconnect → backoff → repeat. The first gap-
/// heal pair is the cold-start; subsequent passes only cover the outage
/// window.
pub async fn run_binance_ingest(
    pool: PgPool,
    rest: RestClient,
    txs: BroadcastTxs,
    symbol: String,
    cold_start: ChronoDuration,
) -> Result<()> {
    let mut backoff = RECONNECT_MIN;
    info!(symbol = %symbol, "binance ingest task started");
    loop {
        // Heal any gap. Kline gap-heal fans out 9 parallel REST loops (one
        // per TF); trade gap-heal is one cursor-driven sequential loop.
        // Running them in parallel overlaps wait times for the typical
        // tiny gap and lets the slower side dominate the wall clock for a
        // big cold start.
        let kline_fut = backfill_symbol(&pool, &rest, &symbol, cold_start);
        let trade_fut = backfill_trades(&pool, &rest, &symbol);
        let (kline_res, trade_res) = tokio::join!(kline_fut, trade_fut);

        match kline_res {
            Ok(counts) => {
                for (tf, n) in Timeframe::ALL
                    .iter()
                    .filter(|tf| tf.is_native_kline())
                    .zip(counts.iter())
                {
                    if *n > 0 {
                        debug!(tf = tf.as_str(), rows = n, "kline gap-heal applied");
                    }
                }
            }
            Err(e) => {
                warn!(error = ?e, "kline gap-heal failed; will retry on next reconnect cycle");
            }
        }
        if let Err(e) = trade_res {
            warn!(error = ?e, "trade gap-heal failed; will retry on next reconnect cycle");
        }

        // Stream until the connection drops or errors.
        match ws::connect_and_stream(&symbol, &txs).await {
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
