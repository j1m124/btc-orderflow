//! Per-client WebSocket session.
//!
//! Each accepted connection runs one [`run`] task. That task:
//!   - decodes inbound [`ClientFrame`]s from the read half
//!   - on `Subscribe`, spawns a forwarder task that owns:
//!       * a [`broadcast::Receiver<Tick>`] subscribed BEFORE the snapshot
//!         query runs (Q5b ordering — no live tick can slip through the gap)
//!       * a snapshot query
//!       * a tick-forwarding loop with dedupe against the snapshot tail
//!   - on `Unsubscribe`, aborts the forwarder
//!   - on `HistoryPage`, spawns a one-shot DB query
//!   - on `Ping`, replies with `Pong`
//!
//! All outbound frames travel through one `mpsc::Sender<Message>` that a
//! dedicated write task drains into the WS write half — multiple forwarder
//! tasks can write concurrently without contending on the socket.

use axum::extract::ws::{Message, WebSocket};
use protocol::{
    Candle, Channel, ClientFrame, ServerFrame, SubId, Timeframe,
};
use futures::{SinkExt, StreamExt};
use sqlx::PgPool;
use std::collections::HashMap;
use tokio::{
    sync::{broadcast, mpsc},
    task::AbortHandle,
};
use tracing::{debug, info, warn};

use super::GatewayState;
use crate::binance::parse::{KlineRow, Tick};
use crate::db;

const SUPPORTED_SYMBOL: &str = "BTCUSDT";
const SNAPSHOT_LIMIT: i64 = 500;

/// Per-subscription bookkeeping held by the session for routing
/// `Unsubscribe` and `HistoryPage` ops.
struct SubMeta {
    symbol: String,
    tf: Timeframe,
    abort: AbortHandle,
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
            let tf = match channel {
                Channel::Candles { tf, .. } => tf,
                // Trades / Footprint / Book land their own per-channel
                // forwarders in a follow-up commit; reject here so the
                // wire is honest about what's wired today.
                Channel::Trades | Channel::Footprint { .. } | Channel::Book { .. } => {
                    send_error(
                        write_tx,
                        Some(id),
                        "unsupported_channel",
                        "channel not yet wired on the server",
                    )
                    .await;
                    return;
                }
            };

            // Drop any existing sub with the same id; reuse is allowed and
            // matches the client's reconnect path (Q12c).
            subs.remove(&id);

            let rx = state.broadcast_tx.subscribe();
            let pool = state.pool.clone();
            let write_tx_clone = write_tx.clone();
            let symbol_clone = symbol.clone();
            let handle = tokio::spawn(async move {
                if let Err(e) =
                    run_subscription(id, symbol_clone, tf, rx, pool, write_tx_clone.clone()).await
                {
                    warn!(?id, error = ?e, "subscription forwarder exited with error");
                    let frame = ServerFrame::Error {
                        id: Some(id),
                        code: "subscription_error".into(),
                        msg: format!("{e:#}"),
                    };
                    send_frame(&write_tx_clone, &frame).await;
                }
            });

            subs.insert(
                id,
                SubMeta {
                    symbol,
                    tf,
                    abort: handle.abort_handle(),
                },
            );
            debug!(?id, tf = tf.as_str(), "subscription registered");
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
            let tf = meta.tf;
            let pool = state.pool.clone();
            let write_tx = write_tx.clone();
            tokio::spawn(async move {
                match db::fetch_history_page(&pool, &symbol, tf.as_str(), before_ms, count as i64)
                    .await
                {
                    Ok(candles) => {
                        let frame = ServerFrame::HistoryPage { id, candles };
                        send_frame(&write_tx, &frame).await;
                    }
                    Err(e) => {
                        warn!(?id, error = ?e, "history_page query failed");
                        send_error(
                            &write_tx,
                            Some(id),
                            "history_page_error",
                            &format!("{e:#}"),
                        )
                        .await;
                    }
                }
            });
        }

        ClientFrame::Ping { ts_ms } => {
            send_frame(write_tx, &ServerFrame::Pong { ts_ms }).await;
        }
    }
}

/// Snapshot + live-tick forwarder for one subscription. Lives until the
/// session aborts the handle or the broadcast channel closes.
async fn run_subscription(
    id: SubId,
    symbol: String,
    tf: Timeframe,
    mut rx: broadcast::Receiver<Tick>,
    pool: PgPool,
    write_tx: mpsc::Sender<Message>,
) -> anyhow::Result<()> {
    // Q5b: we already subscribed to the broadcast (by the time this fn runs,
    // `rx` exists); now we run the snapshot query. Any live tick that
    // arrives between here and the start of the forwarding loop is buffered
    // in `rx`.
    let snapshot = db::fetch_snapshot(&pool, &symbol, tf.as_str(), SNAPSHOT_LIMIT).await?;
    let dedupe_threshold = snapshot.last().map(|c| c.open_time);

    debug!(
        ?id,
        tf = tf.as_str(),
        bars = snapshot.len(),
        "sending snapshot"
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
                    // Session writer dropped — session is shutting down.
                    return Ok(());
                }
            }
            Err(broadcast::error::RecvError::Lagged(n)) => {
                warn!(
                    ?id,
                    skipped = n,
                    "subscription lagged broadcast; sending resnap"
                );
                send_frame(&write_tx, &ServerFrame::Resnap { id }).await;
                // For v1 we don't auto-resnap here — the client is expected
                // to send a fresh Subscribe with the same id (Q12c/Q12e).
                return Ok(());
            }
            Err(broadcast::error::RecvError::Closed) => {
                info!(?id, "broadcast closed; subscription exiting");
                return Ok(());
            }
        }
    }
}

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
