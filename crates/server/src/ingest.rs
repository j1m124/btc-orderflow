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
use chrono::{Duration as ChronoDuration, TimeZone, Utc};
use futures::future::try_join_all;
use sqlx::PgPool;
use std::{collections::HashMap, time::Duration as StdDuration};
use tokio::sync::broadcast;
use tracing::{debug, info, warn};

use std::sync::Arc;
use tokio::sync::RwLock;

use crate::binance::{
    AGGTRADES_PAGE_LIMIT, BroadcastTxs, KLINES_PAGE_LIMIT,
    book::Book,
    parse::{KlineRow, Tick, TradeRow, TradeTick},
    rest::RestClient,
    ws,
};
use crate::db;

/// Shared in-memory orderbook handle. The maintainer owns the write side;
/// the gateway forwarders read it briefly to populate the initial
/// `BookSnapshot` frame for new subscribers. Wrapped in an `Arc<RwLock<_>>`
/// so cloning the handle hands out new readers cheaply.
#[derive(Clone)]
pub struct BookState {
    pub inner: Arc<RwLock<Book>>,
}

impl BookState {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(Book::empty())),
        }
    }
}

impl Default for BookState {
    fn default() -> Self {
        Self::new()
    }
}

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

/// Capacity of the depth-diff broadcast channel. ~10 events/sec @ 100ms
/// cadence; the maintainer is the primary consumer + gateway forwarders.
/// Sized large enough that maintenance-pause spikes don't push consumers
/// to `Lagged`.
pub const DEPTH_BROADCAST_CAPACITY: usize = 4096;

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

/// Cadence of `book_snapshots` rows persisted by the book maintainer. 1s
/// matches the heatmap pixel granularity at typical zooms; smaller would
/// be over-resolution, larger would lose heatmap detail.
const BOOK_SNAPSHOT_INTERVAL: StdDuration = StdDuration::from_secs(1);

/// How many levels each side go into the persisted snapshot row. Matches
/// the `book_snapshots` schema invariant (top 50). The live wire BookDelta
/// stream is independent and may filter to any client-requested depth.
const BOOK_SNAPSHOT_DEPTH: usize = 50;

/// Depth-snapshot REST limit. Always request 1000 — gives the maintainer a
/// robust resync point even if diffs lag at boot.
const DEPTH_SNAPSHOT_LIMIT: u32 = 1000;

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
        // No `biased;`: the aggTrade broadcast is hot enough that biased
        // polling order would keep `rx.recv()` Ready every iteration and
        // starve `flush_timer.tick()`, so the writer would buffer
        // indefinitely and never write to the DB. Random select fixes that.
        tokio::select! {
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

// --- Sub-second aggregator --------------------------------------------------

/// Minimum gap between consecutive *open-bar* tick emissions for a synthesized
/// TF. At ~100 aggTrades/sec emitting per trade would push 100 Hz to the
/// gateway forwarder — too chatty over the wire. 100ms throttling caps each
/// synthesized TF at ~10 Hz of open-bar updates, matching the trade-batch and
/// book-delta cadence. Closed-bar emissions always fire immediately.
const SUBSEC_EMIT_THROTTLE_MS: i64 = 100;

/// Rolling state for one in-progress sub-second bar.
#[derive(Clone, Debug)]
struct PartialBar {
    open_time_ms: i64,
    close_time_ms: i64,
    open: f64,
    high: f64,
    low: f64,
    close: f64,
    volume: f64,
    quote_volume: f64,
    trades: i32,
    taker_buy_vol: f64,
    /// Wall-clock of the last open-bar tick we emitted for this bar. Used to
    /// throttle open-bar emits without affecting close-bar emits.
    last_emit_ms: i64,
}

/// Fold the aggTrade stream into rolling S1 / S5 bars per symbol and emit
/// them on the kline broadcast. The DB writer ignores synthesized TFs (the
/// trades table is the source of truth); the gateway treats live S1/S5 ticks
/// identically to the native TFs.
///
/// Resilience: on `RecvError::Lagged` the in-progress bars are discarded —
/// any open/high/low/close we'd accumulate from a partial sequence would be
/// wrong. The next trade after the gap starts a fresh bar; the snapshot
/// query against `trades` will reconstruct the missed window when a client
/// subscribes.
pub async fn run_subsec_aggregator(
    symbol: String,
    mut trade_rx: broadcast::Receiver<TradeTick>,
    kline_tx: broadcast::Sender<Tick>,
) -> Result<()> {
    info!(symbol = %symbol, "subsec aggregator task started");
    const SUBSEC_TFS: [Timeframe; 2] = [Timeframe::S1, Timeframe::S5];
    let mut bars: HashMap<Timeframe, PartialBar> = HashMap::new();

    loop {
        match trade_rx.recv().await {
            Ok(tick) => {
                if tick.symbol != symbol {
                    continue;
                }
                for &tf in &SUBSEC_TFS {
                    update_bar(&mut bars, tf, &tick.trade, &symbol, &kline_tx);
                }
            }
            Err(broadcast::error::RecvError::Lagged(n)) => {
                warn!(skipped = n, "subsec aggregator lagged trade broadcast");
                bars.clear();
            }
            Err(broadcast::error::RecvError::Closed) => {
                info!("trade broadcast closed; subsec aggregator exiting");
                return Ok(());
            }
        }
    }
}

fn update_bar(
    bars: &mut HashMap<Timeframe, PartialBar>,
    tf: Timeframe,
    trade: &TradeRow,
    symbol: &str,
    kline_tx: &broadcast::Sender<Tick>,
) {
    let bar_ms = tf.duration_ms();
    let ts_ms = trade.ts.timestamp_millis();
    // Align to TF boundary. The SQL fallback path (fetch_subsec_snapshot)
    // uses time_bucket with the same TF, which agrees with this integer
    // floor for any TF whose epoch-aligned origin matches (it does for
    // sub-second TFs against the Unix epoch).
    let bar_open = (ts_ms / bar_ms) * bar_ms;
    let bar_close = bar_open + bar_ms - 1;
    let qty_quote = trade.qty * trade.price;
    let taker_buy = if trade.is_buyer_maker { 0.0 } else { trade.qty };

    match bars.get(&tf) {
        Some(bar) if bar.open_time_ms == bar_open => {
            // Same bar — accumulate.
            let mut updated = bar.clone();
            updated.high = updated.high.max(trade.price);
            updated.low = updated.low.min(trade.price);
            updated.close = trade.price;
            updated.volume += trade.qty;
            updated.quote_volume += qty_quote;
            updated.trades += 1;
            updated.taker_buy_vol += taker_buy;
            if ts_ms - updated.last_emit_ms >= SUBSEC_EMIT_THROTTLE_MS {
                emit_tick(&updated, tf, symbol, false, kline_tx);
                updated.last_emit_ms = ts_ms;
            }
            bars.insert(tf, updated);
        }
        Some(bar) if bar.open_time_ms < bar_open => {
            // Bar rollover. Close the previous (always emit close), start fresh.
            let prev = bar.clone();
            emit_tick(&prev, tf, symbol, true, kline_tx);
            let new_bar = PartialBar {
                open_time_ms: bar_open,
                close_time_ms: bar_close,
                open: trade.price,
                high: trade.price,
                low: trade.price,
                close: trade.price,
                volume: trade.qty,
                quote_volume: qty_quote,
                trades: 1,
                taker_buy_vol: taker_buy,
                last_emit_ms: ts_ms,
            };
            emit_tick(&new_bar, tf, symbol, false, kline_tx);
            bars.insert(tf, new_bar);
        }
        None => {
            // First trade we've seen — initialize the bar.
            let new_bar = PartialBar {
                open_time_ms: bar_open,
                close_time_ms: bar_close,
                open: trade.price,
                high: trade.price,
                low: trade.price,
                close: trade.price,
                volume: trade.qty,
                quote_volume: qty_quote,
                trades: 1,
                taker_buy_vol: taker_buy,
                last_emit_ms: ts_ms,
            };
            emit_tick(&new_bar, tf, symbol, false, kline_tx);
            bars.insert(tf, new_bar);
        }
        Some(_) => {
            // Trade older than the current bar. The aggTrade stream is
            // monotonic in practice; this branch is a defensive no-op.
        }
    }
}

fn emit_tick(
    bar: &PartialBar,
    tf: Timeframe,
    symbol: &str,
    is_closed: bool,
    tx: &broadcast::Sender<Tick>,
) {
    let open_time = match Utc.timestamp_millis_opt(bar.open_time_ms).single() {
        Some(t) => t,
        None => return,
    };
    let close_time = match Utc.timestamp_millis_opt(bar.close_time_ms).single() {
        Some(t) => t,
        None => return,
    };
    let kline = KlineRow {
        open_time,
        close_time,
        open: bar.open,
        high: bar.high,
        low: bar.low,
        close: bar.close,
        volume: bar.volume,
        quote_volume: bar.quote_volume,
        trades: bar.trades,
        taker_buy_vol: bar.taker_buy_vol,
    };
    let _ = tx.send(Tick {
        symbol: symbol.to_string(),
        tf,
        kline,
        is_closed,
    });
}

// --- Book maintainer --------------------------------------------------------

/// Maintain the live orderbook from the `@depth@100ms` diff stream.
///
/// Lifecycle per bootstrap attempt:
///   1. Subscribe to the depth broadcast and buffer events.
///   2. REST snapshot from `/fapi/v1/depth?limit=1000`, populate the book.
///   3. Drain buffered diffs; drop any with `final_update_id <= snapshot.last_update_id`
///      (already covered by the snapshot).
///   4. Apply the first diff that overlaps `snapshot.last_update_id + 1`.
///   5. Apply subsequent diffs in order; on a `pu` mismatch or any other
///      sync error, mark the book uninitialized and restart from step 2.
///
/// A separate 1s timer reads the current top-50 each side and persists it
/// to `book_snapshots`. No persistence is possible during the (re-)bootstrap
/// window — the book is empty/transitioning, so the snapshot is meaningless
/// for replay until initialized.
pub async fn run_book_maintainer(
    pool: PgPool,
    symbol: String,
    rest: RestClient,
    txs: BroadcastTxs,
    book_state: BookState,
) -> Result<()> {
    info!(symbol = %symbol, "book maintainer task started");

    loop {
        // Each iteration is one bootstrap attempt. On hard failure (REST
        // error, sync gap, etc.) we mark the book uninitialized and loop
        // to re-bootstrap.
        if let Err(e) = maintain_one_session(
            &pool,
            &symbol,
            &rest,
            &txs,
            &book_state,
        )
        .await
        {
            warn!(error = ?e, "book maintainer session failed; rebooting");
            // Mark uninitialized so any reader can see we have nothing.
            *book_state.inner.write().await = Book::empty();
            tokio::time::sleep(StdDuration::from_secs(2)).await;
        }
    }
}

async fn maintain_one_session(
    pool: &PgPool,
    symbol: &str,
    rest: &RestClient,
    txs: &BroadcastTxs,
    book_state: &BookState,
) -> Result<()> {
    // Step 1: open the broadcast subscription FIRST so any diffs that fly
    // by while we wait on REST land in our buffer.
    let mut depth_rx = txs.depth.subscribe();

    // Step 2: REST bootstrap.
    let snapshot = rest
        .depth_snapshot(symbol, DEPTH_SNAPSHOT_LIMIT)
        .await
        .context("REST depth_snapshot")?;
    info!(
        symbol,
        last_update_id = snapshot.last_update_id,
        bids = snapshot.bids.len(),
        asks = snapshot.asks.len(),
        "depth snapshot fetched"
    );

    {
        // Install the snapshot. We drop any diffs already covered below.
        let mut book = book_state.inner.write().await;
        *book = Book::from_snapshot(snapshot.bids, snapshot.asks, snapshot.last_update_id);
    }
    let bootstrap_id = snapshot.last_update_id;

    // Step 3+4: drain buffered diffs, find the first one to apply, then
    // ride the live stream. Snapshot timer fires in parallel.
    let mut snapshot_timer = tokio::time::interval(BOOK_SNAPSHOT_INTERVAL);
    snapshot_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // First tick fires immediately; skip it — book may still be settling.
    snapshot_timer.tick().await;

    let mut found_first = false;
    loop {
        // No `biased;`: the depth firehose keeps `depth_rx.recv()` Ready
        // every iteration; biased polling would starve `snapshot_timer.tick()`
        // and historical book snapshots would never persist.
        tokio::select! {
            msg = depth_rx.recv() => {
                match msg {
                    Ok(diff) => {
                        if diff.symbol != symbol {
                            continue;
                        }
                        if !found_first {
                            // Skip events already covered by the snapshot.
                            if diff.final_update_id <= bootstrap_id {
                                continue;
                            }
                            // The first diff to apply must straddle
                            // bootstrap_id+1. apply_diff enforces this.
                            found_first = true;
                        }
                        let mut book = book_state.inner.write().await;
                        if let Err(e) = book.apply_diff(&diff) {
                            // Drop the lock before returning the Err so the
                            // outer loop can reset cleanly.
                            drop(book);
                            return Err(anyhow::anyhow!("book sync error: {e}"));
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!(skipped = n, "book maintainer lagged depth broadcast; rebooting");
                        return Err(anyhow::anyhow!("depth broadcast lagged ({n} events)"));
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        info!("depth broadcast closed; book maintainer exiting");
                        return Ok(());
                    }
                }
            }
            _ = snapshot_timer.tick() => {
                persist_snapshot(pool, symbol, book_state).await;
            }
        }
    }
}

/// Read the current top-50 each side and write one `book_snapshots` row.
async fn persist_snapshot(pool: &PgPool, symbol: &str, book_state: &BookState) {
    let (bids, asks) = {
        let book = book_state.inner.read().await;
        if !book.is_initialized() {
            return;
        }
        book.top_n(BOOK_SNAPSHOT_DEPTH)
    };
    if bids.is_empty() && asks.is_empty() {
        return;
    }
    if let Err(e) = db::upsert_book_snapshot(pool, symbol, Utc::now(), &bids, &asks).await {
        warn!(symbol, error = ?e, "book snapshot upsert failed");
    }
}

/// Drain the broadcast channel forever, UPSERTing every **closed** kline
/// into the `candles` table. Open (in-progress) bars are streamed to
/// gateway clients but not persisted — only the final bar is canonical, and
/// the next closed-bar UPSERT replaces any earlier persisted state.
///
/// Synthesized sub-second bars (S1/S5 from the aggregator) ride the same
/// broadcast for live forwarding but are *not* persisted — the `trades`
/// table is the source of truth for those, and snapshot/history-page reads
/// derive them on demand via `time_bucket`.
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
            Ok(tick) if tick.is_closed && tick.tf.is_native_kline() => {
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
            Ok(_) => { /* open bar OR synthesized subsec; skip persistence */ }
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

/// Run the Binance ingest forever. Binance Futures partitions streams across
/// two endpoint families — `/market` (kline + aggTrade) and `/stream`
/// (diff-depth) — so we drive two concurrent WS connections, each with its
/// own reconnect/backoff state. The market loop also runs the kline+trade
/// REST gap-heal between connect attempts; the depth loop has no gap-heal
/// (the book maintainer fetches its own REST snapshot on every bootstrap).
pub async fn run_binance_ingest(
    pool: PgPool,
    rest: RestClient,
    txs: BroadcastTxs,
    symbol: String,
    cold_start: ChronoDuration,
) -> Result<()> {
    info!(symbol = %symbol, "binance ingest task started");
    let market = run_market_loop(pool, rest, txs.clone(), symbol.clone(), cold_start);
    let public = run_public_loop(txs, symbol);
    // Both loops are infinite; tokio::join! returns only if both ever return.
    let (_, _) = tokio::join!(market, public);
    Ok(())
}

/// Gap-heal → connect to `/market/stream` → stream until disconnect → backoff.
async fn run_market_loop(
    pool: PgPool,
    rest: RestClient,
    txs: BroadcastTxs,
    symbol: String,
    cold_start: ChronoDuration,
) {
    let mut backoff = RECONNECT_MIN;
    loop {
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

        let url = ws::market_combined_url(&symbol);
        match ws::connect_and_stream("market", &url, &txs).await {
            Ok(()) => {
                info!("binance market ws closed cleanly; reconnecting");
                backoff = RECONNECT_MIN;
            }
            Err(e) => {
                warn!(
                    error = ?e,
                    backoff_ms = backoff.as_millis() as u64,
                    "binance market ws error; reconnecting after backoff"
                );
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(RECONNECT_MAX);
            }
        }
    }
}

/// Connect to the depth combined-stream → stream → backoff. No REST gap-heal:
/// the book maintainer fetches a fresh `/fapi/v1/depth?limit=1000` snapshot on
/// every bootstrap attempt and resyncs from a buffered diff.
async fn run_public_loop(txs: BroadcastTxs, symbol: String) {
    let mut backoff = RECONNECT_MIN;
    loop {
        let url = ws::public_combined_url(&symbol);
        match ws::connect_and_stream("public", &url, &txs).await {
            Ok(()) => {
                info!("binance public ws closed cleanly; reconnecting");
                backoff = RECONNECT_MIN;
            }
            Err(e) => {
                warn!(
                    error = ?e,
                    backoff_ms = backoff.as_millis() as u64,
                    "binance public ws error; reconnecting after backoff"
                );
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(RECONNECT_MAX);
            }
        }
    }
}
