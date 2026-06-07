//! Per-client WebSocket session.
//!
//! Each accepted connection runs one [`run`] task. That task:
//!   - decodes inbound [`ClientFrame`]s from the read half
//!   - on `Subscribe`, dispatches per-[`Channel`] kind to a forwarder task:
//!       * Candles: snapshot (native or sub-sec synthesized) + live kline
//!         ticks from the kline broadcast (sub-sec aggregator emits there
//!         too, so S1/S5 live ticks ride the same path).
//!       * Trades: snapshot from `trades` + live `TradeTick` batches
//!         (server-batched at 100ms) from the trade broadcast.
//!       * Footprint: snapshot from `trades` via `time_bucket` aggregation +
//!         per-subscription rolling cell state from the trade broadcast,
//!         emitted as `FootprintUpdate` every 100ms.
//!       * Book: snapshot of top-N from the shared in-memory book +
//!         relayed `BookDelta` frames from the depth broadcast (server-
//!         batched at 100ms).
//!     Each forwarder subscribes to its broadcast BEFORE running its
//!     snapshot query (Q5b ordering — no live event can slip the gap).
//!   - on `Unsubscribe`, aborts the forwarder
//!   - on `HistoryPage`, spawns a one-shot DB query dispatched by the
//!     stored channel kind
//!   - on `Ping`, replies with `Pong`
//!
//! All outbound frames travel through one `mpsc::Sender<Message>` that a
//! dedicated write task drains into the WS write half — multiple forwarder
//! tasks can write concurrently without contending on the socket.

use axum::extract::ws::{Message, WebSocket};
use protocol::{
    BookLevel, Candle, Channel, ClientFrame, FootprintCell, ServerFrame, SubId, Timeframe, Trade,
};
use futures::{SinkExt, StreamExt};
use sqlx::PgPool;
use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};
use tokio::{
    sync::{broadcast, mpsc},
    task::AbortHandle,
};
use tracing::{debug, info, warn};

use super::GatewayState;
use crate::binance::parse::{DepthDiff, KlineRow, Tick, TradeTick};
use crate::ingest::BookState;
use crate::db;

const SUPPORTED_SYMBOL: &str = "BTCUSDT";
const SNAPSHOT_LIMIT: i64 = 500;

/// Number of recent trades sent in a trade-channel snapshot.
const TRADE_SNAPSHOT_LIMIT: i64 = 500;

/// Number of recent bars (×all their price buckets) in a footprint snapshot.
const FOOTPRINT_SNAPSHOT_BARS: i64 = 100;

/// Server-side batching window for live trade / book / footprint frames.
const LIVE_BATCH_INTERVAL: Duration = Duration::from_millis(100);

/// Cadence of full-state `BookSnapshot` resends on the Book channel. Bounds
/// how long any client-side drift can persist: the depth diff filter can
/// silently drop a removal delta when the top-N window shifts under a fast
/// market (Mode 2 in the bug investigation), and during a maintainer
/// rebootstrap window the forwarder relays diffs against an empty
/// `book_state` (Mode 1). Either way, the next periodic snapshot replaces
/// the client's `cur_bids`/`cur_asks` wholesale.
const BOOK_SNAPSHOT_REFRESH: Duration = Duration::from_secs(5);

/// Per-subscription bookkeeping held by the session for routing
/// `Unsubscribe` and `HistoryPage` ops. `channel` carries the per-kind
/// parameters the HistoryPage handler needs to issue the right query.
struct SubMeta {
    symbol: String,
    channel: SubChannel,
    abort: AbortHandle,
}

#[derive(Clone, Copy)]
enum SubChannel {
    Candles { tf: Timeframe },
    Trades,
    Footprint { tf: Timeframe, price_bucket: f64 },
    Book,
}

impl Drop for SubMeta {
    fn drop(&mut self) {
        self.abort.abort();
    }
}

/// Drive one WS connection from accept to close.
pub async fn run(socket: WebSocket, state: GatewayState) {
    let (mut ws_tx, mut ws_rx) = socket.split();
    let (write_tx, mut write_rx) = mpsc::channel::<Message>(64);

    // Single writer task: drains the mpsc into the WS. All per-subscription
    // forwarders push frames here.
    let writer = tokio::spawn(async move {
        while let Some(msg) = write_rx.recv().await {
            if ws_tx.send(msg).await.is_err() {
                break;
            }
        }
    });

    let mut subs: HashMap<SubId, SubMeta> = HashMap::new();

    info!("ws session opened");
    while let Some(msg) = ws_rx.next().await {
        let msg = match msg {
            Ok(m) => m,
            Err(e) => {
                warn!(error = %e, "ws read error; closing session");
                break;
            }
        };

        match msg {
            Message::Text(txt) => {
                handle_text(&txt, &mut subs, &state, &write_tx).await;
            }
            Message::Binary(_) => {
                send_error(&write_tx, None, "binary_unsupported", "send text frames")
                    .await;
            }
            Message::Close(_) => {
                info!("client requested close");
                break;
            }
            Message::Ping(_) | Message::Pong(_) => { /* axum handles WS-level pings */ }
        }
    }

    // Drop subs → AbortHandles → forwarders exit.
    drop(subs);
    drop(write_tx);
    let _ = writer.await;
    info!("ws session closed");
}

async fn handle_text(
    txt: &str,
    subs: &mut HashMap<SubId, SubMeta>,
    state: &GatewayState,
    write_tx: &mpsc::Sender<Message>,
) {
    let frame: ClientFrame = match serde_json::from_str(txt) {
        Ok(f) => f,
        Err(e) => {
            warn!(error = %e, txt = %truncate(txt), "decode client frame");
            send_error(write_tx, None, "decode_error", &e.to_string()).await;
            return;
        }
    };

    match frame {
        ClientFrame::Subscribe { id, symbol, channel } => {
            if symbol != SUPPORTED_SYMBOL {
                send_error(
                    write_tx,
                    Some(id),
                    "unknown_symbol",
                    &format!("v1 only serves {SUPPORTED_SYMBOL}"),
                )
                .await;
                return;
            }

            // Drop any existing sub with the same id; reuse is allowed and
            // matches the client's reconnect path (Q12c).
            subs.remove(&id);

            // Each branch:
            //   1. Subscribes to the relevant broadcast FIRST (Q5b ordering).
            //   2. Spawns the forwarder task.
            //   3. Records the channel-typed cursor info on the SubMeta so
            //      HistoryPage can route to the right query.
            let (sub_channel, abort) = match channel {
                Channel::Candles { tf } => {
                    let rx = state.broadcast_tx.subscribe();
                    let h = spawn_forwarder(
                        id,
                        write_tx.clone(),
                        "candles forwarder",
                        run_candles_subscription(
                            id,
                            symbol.clone(),
                            tf,
                            rx,
                            state.pool.clone(),
                            write_tx.clone(),
                        ),
                    );
                    (SubChannel::Candles { tf }, h)
                }
                Channel::Trades => {
                    let rx = state.trade_tx.subscribe();
                    let h = spawn_forwarder(
                        id,
                        write_tx.clone(),
                        "trades forwarder",
                        run_trades_subscription(
                            id,
                            symbol.clone(),
                            rx,
                            state.pool.clone(),
                            write_tx.clone(),
                        ),
                    );
                    (SubChannel::Trades, h)
                }
                Channel::Footprint { tf, price_bucket } => {
                    if !(price_bucket.is_finite() && price_bucket > 0.0) {
                        send_error(
                            write_tx,
                            Some(id),
                            "invalid_bucket",
                            "price_bucket must be > 0 and finite",
                        )
                        .await;
                        return;
                    }
                    let rx = state.trade_tx.subscribe();
                    let h = spawn_forwarder(
                        id,
                        write_tx.clone(),
                        "footprint forwarder",
                        run_footprint_subscription(
                            id,
                            symbol.clone(),
                            tf,
                            price_bucket,
                            rx,
                            state.pool.clone(),
                            write_tx.clone(),
                        ),
                    );
                    (SubChannel::Footprint { tf, price_bucket }, h)
                }
                Channel::Book { depth } => {
                    let rx = state.depth_tx.subscribe();
                    let h = spawn_forwarder(
                        id,
                        write_tx.clone(),
                        "book forwarder",
                        run_book_subscription(
                            id,
                            symbol.clone(),
                            depth,
                            rx,
                            state.book_state.clone(),
                            write_tx.clone(),
                        ),
                    );
                    (SubChannel::Book, h)
                }
            };

            subs.insert(
                id,
                SubMeta {
                    symbol,
                    channel: sub_channel,
                    abort,
                },
            );
            debug!(?id, "subscription registered");
        }

        ClientFrame::Unsubscribe { id } => {
            if subs.remove(&id).is_some() {
                debug!(?id, "unsubscribe");
            }
        }

        ClientFrame::HistoryPage {
            id,
            before_ms,
            count,
        } => {
            let Some(meta) = subs.get(&id) else {
                send_error(
                    write_tx,
                    Some(id),
                    "unknown_subscription",
                    "history_page for unknown id",
                )
                .await;
                return;
            };
            let symbol = meta.symbol.clone();
            let channel = meta.channel;
            let pool = state.pool.clone();
            let write_tx = write_tx.clone();
            tokio::spawn(async move {
                if let Err(e) =
                    history_page(id, symbol, channel, before_ms, count as i64, pool, &write_tx)
                        .await
                {
                    warn!(?id, error = ?e, "history_page query failed");
                    send_error(&write_tx, Some(id), "history_page_error", &format!("{e:#}"))
                        .await;
                }
            });
        }

        ClientFrame::Ping { ts_ms } => {
            send_frame(write_tx, &ServerFrame::Pong { ts_ms }).await;
        }
    }
}

/// Spawn a forwarder future, wrap errors into an `Error` frame to the client.
fn spawn_forwarder<F>(
    id: SubId,
    write_tx: mpsc::Sender<Message>,
    name: &'static str,
    fut: F,
) -> AbortHandle
where
    F: std::future::Future<Output = anyhow::Result<()>> + Send + 'static,
{
    let handle = tokio::spawn(async move {
        if let Err(e) = fut.await {
            warn!(?id, %name, error = ?e, "forwarder exited with error");
            send_frame(
                &write_tx,
                &ServerFrame::Error {
                    id: Some(id),
                    code: "subscription_error".into(),
                    msg: format!("{e:#}"),
                },
            )
            .await;
        }
    });
    handle.abort_handle()
}

// --- HistoryPage dispatch --------------------------------------------------

async fn history_page(
    id: SubId,
    symbol: String,
    channel: SubChannel,
    before_ms: i64,
    count: i64,
    pool: PgPool,
    write_tx: &mpsc::Sender<Message>,
) -> anyhow::Result<()> {
    match channel {
        SubChannel::Candles { tf } => {
            let candles = if tf.is_native_kline() {
                db::fetch_history_page(&pool, &symbol, tf.as_str(), before_ms, count).await?
            } else {
                db::fetch_subsec_history_page(&pool, &symbol, tf, before_ms, count).await?
            };
            send_frame(write_tx, &ServerFrame::HistoryPage { id, candles }).await;
        }
        SubChannel::Trades => {
            let trades = db::fetch_trades_history_page(&pool, &symbol, before_ms, count).await?;
            send_frame(write_tx, &ServerFrame::TradeHistoryPage { id, trades }).await;
        }
        SubChannel::Footprint { tf, price_bucket } => {
            let cells = db::fetch_footprint_history_page(
                &pool,
                &symbol,
                tf,
                price_bucket,
                before_ms,
                count,
            )
            .await?;
            send_frame(write_tx, &ServerFrame::FootprintHistoryPage { id, cells }).await;
        }
        SubChannel::Book => {
            let snapshots = db::fetch_book_history_page(&pool, &symbol, before_ms, count).await?;
            send_frame(write_tx, &ServerFrame::BookHistoryPage { id, snapshots }).await;
        }
    }
    Ok(())
}

// --- Candles forwarder -----------------------------------------------------

/// Snapshot + live-tick forwarder for one candles subscription. Lives until
/// the session aborts the handle or the broadcast channel closes. Handles
/// both native TFs (snapshot from `candles` table) and S1/S5 (snapshot
/// synthesized from `trades`); live ticks ride the same kline broadcast in
/// both cases — the sub-sec aggregator emits there alongside Binance klines.
async fn run_candles_subscription(
    id: SubId,
    symbol: String,
    tf: Timeframe,
    mut rx: broadcast::Receiver<Tick>,
    pool: PgPool,
    write_tx: mpsc::Sender<Message>,
) -> anyhow::Result<()> {
    let snapshot = if tf.is_native_kline() {
        db::fetch_snapshot(&pool, &symbol, tf.as_str(), SNAPSHOT_LIMIT).await?
    } else {
        db::fetch_subsec_snapshot(&pool, &symbol, tf, SNAPSHOT_LIMIT).await?
    };
    let dedupe_threshold = snapshot.last().map(|c| c.open_time);

    debug!(
        ?id,
        tf = tf.as_str(),
        bars = snapshot.len(),
        "sending candles snapshot"
    );
    let mut server_v: u64 = 0;
    send_frame(
        &write_tx,
        &ServerFrame::Snapshot {
            id,
            candles: snapshot,
            server_v,
        },
    )
    .await;

    loop {
        match rx.recv().await {
            Ok(tick) => {
                if tick.symbol != symbol || tick.tf != tf {
                    continue;
                }
                // Dedupe: skip any closed bar already covered by the
                // snapshot. Open bars always have a newer open_time than
                // the snapshot tail, so they pass through unconditionally.
                if tick.is_closed {
                    if let Some(thr) = dedupe_threshold {
                        if tick.kline.open_time_ms() <= thr {
                            continue;
                        }
                    }
                }
                server_v += 1;
                let frame = ServerFrame::Tick {
                    id,
                    candle: kline_to_wire(&tick.kline),
                    is_closed: tick.is_closed,
                    v: server_v,
                };
                if write_tx.send(message_from_frame(&frame)).await.is_err() {
                    return Ok(());
                }
            }
            Err(broadcast::error::RecvError::Lagged(n)) => {
                warn!(?id, skipped = n, "candles sub lagged; sending resnap");
                send_frame(&write_tx, &ServerFrame::Resnap { id }).await;
                return Ok(());
            }
            Err(broadcast::error::RecvError::Closed) => {
                info!(?id, "broadcast closed; candles sub exiting");
                return Ok(());
            }
        }
    }
}

// --- Trades forwarder ------------------------------------------------------

/// Trade-channel forwarder. Snapshot is the most recent 500 trades; live
/// trades are batched into 100ms windows on the wire to keep the WS frame
/// rate to ~10 Hz/subscription regardless of underlying trade arrival rate.
async fn run_trades_subscription(
    id: SubId,
    symbol: String,
    mut rx: broadcast::Receiver<TradeTick>,
    pool: PgPool,
    write_tx: mpsc::Sender<Message>,
) -> anyhow::Result<()> {
    let snapshot = db::fetch_trades_snapshot(&pool, &symbol, TRADE_SNAPSHOT_LIMIT).await?;
    let dedupe_threshold = snapshot.last().map(|t| t.agg_id);

    debug!(
        ?id,
        trades = snapshot.len(),
        "sending trades snapshot"
    );
    let mut server_v: u64 = 0;
    send_frame(
        &write_tx,
        &ServerFrame::TradeSnapshot {
            id,
            trades: snapshot,
            server_v,
        },
    )
    .await;

    let mut buffer: Vec<Trade> = Vec::with_capacity(256);
    let mut batch_timer = tokio::time::interval(LIVE_BATCH_INTERVAL);
    batch_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    batch_timer.tick().await; // burn immediate tick

    loop {
        // No `biased;` — `rx.recv()` is essentially always Ready on the
        // BTCUSDT-perp aggTrade firehose (~10–100 trades/sec). Under biased
        // polling, the timer branch would be starved and the buffer would
        // accumulate without ever flushing, so the client only ever sees the
        // initial TradeSnapshot. Random select gives both branches a turn.
        tokio::select! {
            msg = rx.recv() => {
                match msg {
                    Ok(tick) => {
                        if tick.symbol != symbol {
                            continue;
                        }
                        // Dedupe against snapshot tail. The snapshot is the
                        // most-recent N trades; any live agg_id < snapshot.last
                        // is already covered.
                        if let Some(thr) = dedupe_threshold {
                            if tick.trade.agg_id <= thr {
                                continue;
                            }
                        }
                        buffer.push(Trade {
                            ts_ms: tick.trade.ts.timestamp_millis(),
                            agg_id: tick.trade.agg_id,
                            price: tick.trade.price,
                            qty: tick.trade.qty,
                            is_buyer_maker: tick.trade.is_buyer_maker,
                        });
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!(?id, skipped = n, "trades sub lagged; sending resnap");
                        send_frame(&write_tx, &ServerFrame::Resnap { id }).await;
                        return Ok(());
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        info!(?id, "trade broadcast closed; trades sub exiting");
                        return Ok(());
                    }
                }
            }
            _ = batch_timer.tick() => {
                if buffer.is_empty() {
                    continue;
                }
                server_v += 1;
                let trades = std::mem::take(&mut buffer);
                let frame = ServerFrame::TradeTick {
                    id,
                    trades,
                    v: server_v,
                };
                if write_tx.send(message_from_frame(&frame)).await.is_err() {
                    return Ok(());
                }
            }
        }
    }
}

// --- Book forwarder --------------------------------------------------------

/// Book-channel forwarder. Snapshot reads top-N from the shared in-memory
/// book; live diffs from Binance are relayed (filtered to the subscription's
/// depth) as `BookDelta` frames, batched at 100ms.
///
/// Diff levels outside top-N are filtered out before emission. The set of
/// "top-N prices" is recomputed from the shared book each batch — the live
/// book moves around enough that pre-computing once would drift.
async fn run_book_subscription(
    id: SubId,
    symbol: String,
    depth: u16,
    mut rx: broadcast::Receiver<DepthDiff>,
    book_state: BookState,
    write_tx: mpsc::Sender<Message>,
) -> anyhow::Result<()> {
    // Wait briefly for the maintainer to install a snapshot. On a fresh
    // boot the book is empty; sending an empty BookSnapshot is legal but
    // visually unhelpful, so a short bounded wait lets the client see a
    // non-empty book the moment the maintainer finishes bootstrap.
    let depth = depth as usize;
    let mut wait_attempts = 0;
    let (bids, asks) = loop {
        {
            let book = book_state.inner.read().await;
            if book.is_initialized() {
                break book.top_n(depth);
            }
        }
        if wait_attempts >= 20 {
            // ~2s budget; send empty if maintainer is still bootstrapping.
            break (Vec::new(), Vec::new());
        }
        wait_attempts += 1;
        tokio::time::sleep(Duration::from_millis(100)).await;
    };

    let mut server_v: u64 = 0;
    send_frame(
        &write_tx,
        &ServerFrame::BookSnapshot {
            id,
            bids: bids.into_iter().map(level_from_pair).collect(),
            asks: asks.into_iter().map(level_from_pair).collect(),
            server_v,
        },
    )
    .await;

    let mut bid_buffer: Vec<BookLevel> = Vec::new();
    let mut ask_buffer: Vec<BookLevel> = Vec::new();
    let mut batch_timer = tokio::time::interval(LIVE_BATCH_INTERVAL);
    batch_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    batch_timer.tick().await;
    // Periodic full snapshot resync — see `BOOK_SNAPSHOT_REFRESH`. The first
    // tick is consumed here so we don't immediately re-send the snapshot we
    // just emitted at subscription.
    let mut refresh_timer = tokio::time::interval(BOOK_SNAPSHOT_REFRESH);
    refresh_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    refresh_timer.tick().await;

    loop {
        // See `run_trades_subscription` for why `biased;` is omitted: the
        // depth-diff broadcast fires at ~10 Hz minimum, often higher, so
        // biased polling starves the batch timer and BookDelta frames never
        // emit.
        tokio::select! {
            msg = rx.recv() => {
                match msg {
                    Ok(diff) => {
                        if diff.symbol != symbol {
                            continue;
                        }
                        for (price, size) in diff.bids {
                            bid_buffer.push(BookLevel { price, size });
                        }
                        for (price, size) in diff.asks {
                            ask_buffer.push(BookLevel { price, size });
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!(?id, skipped = n, "book sub lagged; sending resnap");
                        send_frame(&write_tx, &ServerFrame::Resnap { id }).await;
                        return Ok(());
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        info!(?id, "depth broadcast closed; book sub exiting");
                        return Ok(());
                    }
                }
            }
            _ = batch_timer.tick() => {
                if bid_buffer.is_empty() && ask_buffer.is_empty() {
                    continue;
                }
                // Filter to current top-N prices each side.
                let (top_bid_price, top_ask_price) = {
                    let book = book_state.inner.read().await;
                    let (bids, asks) = book.top_n(depth);
                    let bottom_bid = bids.last().map(|(p, _)| *p);
                    let top_ask = asks.last().map(|(p, _)| *p);
                    (bottom_bid, top_ask)
                };
                if let Some(min_bid) = top_bid_price {
                    bid_buffer.retain(|l| l.price >= min_bid);
                }
                if let Some(max_ask) = top_ask_price {
                    ask_buffer.retain(|l| l.price <= max_ask);
                }
                if bid_buffer.is_empty() && ask_buffer.is_empty() {
                    continue;
                }
                server_v += 1;
                let bids = std::mem::take(&mut bid_buffer);
                let asks = std::mem::take(&mut ask_buffer);
                let frame = ServerFrame::BookDelta {
                    id,
                    bids,
                    asks,
                    v: server_v,
                };
                if write_tx.send(message_from_frame(&frame)).await.is_err() {
                    return Ok(());
                }
            }
            _ = refresh_timer.tick() => {
                // Read the current top-N under the shared lock. If the
                // maintainer is mid-rebootstrap (`book_state` was reset to
                // `Book::empty()`), skip this tick — sending an empty
                // snapshot would briefly wipe the client's display. The next
                // tick after the maintainer recovers will resync.
                let snapshot = {
                    let book = book_state.inner.read().await;
                    if !book.is_initialized() {
                        None
                    } else {
                        Some(book.top_n(depth))
                    }
                };
                let Some((bids, asks)) = snapshot else {
                    continue;
                };
                server_v += 1;
                let frame = ServerFrame::BookSnapshot {
                    id,
                    bids: bids.into_iter().map(level_from_pair).collect(),
                    asks: asks.into_iter().map(level_from_pair).collect(),
                    server_v,
                };
                if write_tx.send(message_from_frame(&frame)).await.is_err() {
                    return Ok(());
                }
            }
        }
    }
}

fn level_from_pair(p: (f64, f64)) -> BookLevel {
    BookLevel { price: p.0, size: p.1 }
}

// --- Footprint forwarder ---------------------------------------------------

/// Footprint-channel forwarder. Snapshot bucketed via `time_bucket` over
/// the trades table. Live updates: server holds per-bar **cumulative** cell
/// state (sticky across emit windows; only cleared on bar rollover), folds
/// incoming aggTrades into it, and every 100ms emits a `FootprintUpdate`
/// frame containing the running totals for every bucket touched in that
/// window. Cells are absolute, not deltas — the client overwrites by
/// `(open_time, price_bucket_low)` to compose snapshot + updates.
///
/// Snapshot/live merge correctness:
///   * **Tail-bar seed**: `bar_totals` is pre-populated from snapshot cells
///     at `snapshot_tail_open_time` so the very first live trade in that
///     bar accumulates on top of the (already-counted) DB rows, instead of
///     resetting the cumulative to ~zero.
///   * **agg_id watermark**: any broadcast trade whose `agg_id` is at or
///     below `snapshot_max_agg_id` was already summed into the snapshot;
///     dropping it on the live path closes the double-count race between
///     the broadcast-subscribe and snapshot-query.
async fn run_footprint_subscription(
    id: SubId,
    symbol: String,
    tf: Timeframe,
    price_bucket: f64,
    mut rx: broadcast::Receiver<TradeTick>,
    pool: PgPool,
    write_tx: mpsc::Sender<Message>,
) -> anyhow::Result<()> {
    let (snapshot, snapshot_max_agg_id) =
        db::fetch_footprint_snapshot(&pool, &symbol, tf, price_bucket, FOOTPRINT_SNAPSHOT_BARS)
            .await?;
    let snapshot_tail_open_time = snapshot.iter().map(|c| c.open_time).max();

    // Cumulative per-bar bucket totals, sticky across emit windows. Cleared
    // entries-by-entry on bar rollover (anything older than the new bar is
    // gone — Binance aggTrades are strictly monotonic per symbol, so a
    // closed bar will never receive another trade).
    let mut bar_totals: HashMap<(i64, i64), (f64, f64)> = HashMap::new();
    if let Some(tail) = snapshot_tail_open_time {
        for cell in snapshot.iter().filter(|c| c.open_time == tail) {
            let bucket_idx = (cell.price_bucket_low / price_bucket).round() as i64;
            bar_totals.insert((tail, bucket_idx), (cell.bid_vol, cell.ask_vol));
        }
    }

    debug!(
        ?id,
        tf = tf.as_str(),
        bucket = price_bucket,
        cells = snapshot.len(),
        seeded = bar_totals.len(),
        snapshot_max_agg_id,
        "sending footprint snapshot"
    );
    let mut server_v: u64 = 0;
    send_frame(
        &write_tx,
        &ServerFrame::FootprintSnapshot {
            id,
            cells: snapshot,
            server_v,
        },
    )
    .await;

    // Keys touched since the last emit. The emit reads `bar_totals[key]`
    // for each touched key — so emitted cells carry the bar-cumulative
    // value, not a per-window delta. Cleared on every emit.
    let mut touched: HashSet<(i64, i64)> = HashSet::new();
    let mut current_bar_open: Option<i64> = snapshot_tail_open_time;
    let bar_ms = tf.duration_ms();
    let mut batch_timer = tokio::time::interval(LIVE_BATCH_INTERVAL);
    batch_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    batch_timer.tick().await;

    loop {
        // See `run_trades_subscription` for why `biased;` is omitted: the
        // trade broadcast is too hot to let the timer branch get fair
        // scheduling without random select.
        tokio::select! {
            msg = rx.recv() => {
                match msg {
                    Ok(tick) => {
                        if tick.symbol != symbol {
                            continue;
                        }
                        // Watermark: this trade was already summed into the
                        // snapshot — skip to avoid double-counting.
                        if let Some(max) = snapshot_max_agg_id {
                            if tick.trade.agg_id <= max {
                                continue;
                            }
                        }
                        let ts_ms = tick.trade.ts.timestamp_millis();
                        let bar_open = (ts_ms / bar_ms) * bar_ms;
                        // Skip events already covered by the snapshot —
                        // anything in a strictly older bar than the tail.
                        if let Some(tail) = snapshot_tail_open_time {
                            if bar_open < tail {
                                continue;
                            }
                        }
                        // On bar rollover, retire totals for any bar older
                        // than the new one. The closing bar's final value
                        // was already published by the most recent batch
                        // tick (or will be by the touched flush below if
                        // any cells are still pending).
                        match current_bar_open {
                            Some(prev) if prev != bar_open => {
                                bar_totals.retain(|(t, _), _| *t >= bar_open);
                                current_bar_open = Some(bar_open);
                            }
                            None => current_bar_open = Some(bar_open),
                            _ => {}
                        }
                        let bucket_idx = (tick.trade.price / price_bucket).floor() as i64;
                        let entry = bar_totals
                            .entry((bar_open, bucket_idx))
                            .or_insert((0.0, 0.0));
                        if tick.trade.is_buyer_maker {
                            entry.0 += tick.trade.qty;
                        } else {
                            entry.1 += tick.trade.qty;
                        }
                        touched.insert((bar_open, bucket_idx));
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!(?id, skipped = n, "footprint sub lagged; sending resnap");
                        send_frame(&write_tx, &ServerFrame::Resnap { id }).await;
                        return Ok(());
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        info!(?id, "trade broadcast closed; footprint sub exiting");
                        return Ok(());
                    }
                }
            }
            _ = batch_timer.tick() => {
                emit_touched(
                    &write_tx,
                    id,
                    price_bucket,
                    &bar_totals,
                    &mut touched,
                    &mut server_v,
                ).await;
            }
        }
    }
}

async fn emit_touched(
    write_tx: &mpsc::Sender<Message>,
    id: SubId,
    price_bucket: f64,
    bar_totals: &HashMap<(i64, i64), (f64, f64)>,
    touched: &mut HashSet<(i64, i64)>,
    server_v: &mut u64,
) {
    if touched.is_empty() {
        return;
    }
    let cells: Vec<FootprintCell> = touched
        .drain()
        .filter_map(|key| {
            let (open_time, bucket_idx) = key;
            bar_totals
                .get(&key)
                .map(|(bid_vol, ask_vol)| FootprintCell {
                    open_time,
                    price_bucket_low: bucket_idx as f64 * price_bucket,
                    bid_vol: *bid_vol,
                    ask_vol: *ask_vol,
                })
        })
        .collect();
    if cells.is_empty() {
        return;
    }
    *server_v += 1;
    let frame = ServerFrame::FootprintUpdate {
        id,
        cells,
        v: *server_v,
    };
    let _ = write_tx.send(message_from_frame(&frame)).await;
}

// --- Wire converters / IO helpers ------------------------------------------

fn kline_to_wire(k: &KlineRow) -> Candle {
    Candle {
        open_time: k.open_time.timestamp_millis(),
        close_time: k.close_time.timestamp_millis(),
        open: k.open,
        high: k.high,
        low: k.low,
        close: k.close,
        volume: k.volume,
        quote_volume: Some(k.quote_volume),
        trades: Some(k.trades),
        taker_buy_vol: Some(k.taker_buy_vol),
    }
}

fn message_from_frame(frame: &ServerFrame) -> Message {
    let json = serde_json::to_string(frame).expect("ServerFrame is always serializable");
    Message::Text(json.into())
}

async fn send_frame(write_tx: &mpsc::Sender<Message>, frame: &ServerFrame) {
    let _ = write_tx.send(message_from_frame(frame)).await;
}

async fn send_error(
    write_tx: &mpsc::Sender<Message>,
    id: Option<SubId>,
    code: &str,
    msg: &str,
) {
    let frame = ServerFrame::Error {
        id,
        code: code.into(),
        msg: msg.into(),
    };
    send_frame(write_tx, &frame).await;
}

fn truncate(s: &str) -> String {
    if s.len() <= 200 {
        s.to_string()
    } else {
        format!("{}…", &s[..200])
    }
}

#[cfg(test)]
mod tests {
    //! Tests for the periodic `BookSnapshot` resync. Covers the two
    //! drift-leak modes the resync is meant to mask: a maintainer
    //! rebootstrap window (book briefly uninitialized) and the steady-state
    //! self-heal cadence.
    use super::*;
    use crate::binance::book::Book;
    use protocol::SubId;
    use tokio::sync::broadcast;

    fn decode(msg: &Message) -> ServerFrame {
        match msg {
            Message::Text(txt) => serde_json::from_str(&*txt).expect("ServerFrame deserialize"),
            other => panic!("expected text message, got {other:?}"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn periodic_snapshot_resends_after_refresh_interval() {
        let book_state = BookState::new();
        {
            let mut guard = book_state.inner.write().await;
            *guard = Book::from_snapshot(
                vec![(100.0, 1.0), (99.0, 2.0)],
                vec![(101.0, 1.5), (102.0, 0.5)],
                42,
            );
        }
        let (depth_tx, depth_rx) = broadcast::channel::<DepthDiff>(16);
        let (write_tx, mut write_rx) = mpsc::channel::<Message>(16);
        let task = tokio::spawn(run_book_subscription(
            SubId(7),
            "BTCUSDT".to_string(),
            10,
            depth_rx,
            book_state.clone(),
            write_tx,
        ));

        // First frame: initial BookSnapshot at subscription start.
        let first = write_rx
            .recv()
            .await
            .expect("expected initial BookSnapshot frame");
        match decode(&first) {
            ServerFrame::BookSnapshot { bids, asks, .. } => {
                assert_eq!(bids.len(), 2);
                assert_eq!(asks.len(), 2);
            }
            other => panic!("expected initial BookSnapshot, got {other:?}"),
        }

        // Advance past the refresh interval. The 100ms batch_timer fires
        // ~50× during the window but short-circuits because the bid/ask
        // buffers are empty; refresh_timer fires and emits a fresh
        // BookSnapshot.
        tokio::time::advance(BOOK_SNAPSHOT_REFRESH + Duration::from_millis(100)).await;

        let second = tokio::time::timeout(Duration::from_secs(1), write_rx.recv())
            .await
            .expect("timed out waiting for periodic snapshot")
            .expect("write channel closed before periodic snapshot");
        match decode(&second) {
            ServerFrame::BookSnapshot { id, bids, asks, .. } => {
                assert_eq!(id, SubId(7));
                assert_eq!(bids.len(), 2);
                assert_eq!(asks.len(), 2);
            }
            other => panic!("expected periodic BookSnapshot, got {other:?}"),
        }

        drop(depth_tx);
        task.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn periodic_snapshot_skips_while_book_uninitialized() {
        let book_state = BookState::new();
        {
            let mut guard = book_state.inner.write().await;
            *guard = Book::from_snapshot(vec![(100.0, 1.0)], vec![(101.0, 1.0)], 10);
        }
        let (depth_tx, depth_rx) = broadcast::channel::<DepthDiff>(16);
        let (write_tx, mut write_rx) = mpsc::channel::<Message>(16);
        let task = tokio::spawn(run_book_subscription(
            SubId(7),
            "BTCUSDT".to_string(),
            10,
            depth_rx,
            book_state.clone(),
            write_tx,
        ));
        let _initial = write_rx.recv().await.expect("initial snapshot");

        // Simulate maintainer rebootstrap: wipe `book_state` so
        // `is_initialized()` is false during the refresh tick.
        {
            let mut guard = book_state.inner.write().await;
            *guard = Book::empty();
        }

        tokio::time::advance(BOOK_SNAPSHOT_REFRESH + Duration::from_millis(100)).await;

        let observed = tokio::time::timeout(Duration::from_millis(200), write_rx.recv()).await;
        assert!(
            observed.is_err(),
            "no frame should be sent while book is uninitialized, got {observed:?}"
        );

        drop(depth_tx);
        task.abort();
    }
}
