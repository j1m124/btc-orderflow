//! Binance combined-stream WebSocket client.
//!
//! Subscribes to every `<symbol>@kline_<tf>` stream we care about + the
//! `<symbol>@aggTrade` stream over a single connection. Binance hard-
//! disconnects every connection at 24h plus may drop on network blips; the
//! caller wraps [`connect_and_stream`] in a reconnect loop that also runs
//! gap-heal REST passes between connections.

use anyhow::{Context, Result, anyhow};
use protocol::Timeframe;
use futures::{SinkExt, StreamExt};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, info, warn};

use super::{
    BroadcastTxs, WS_BASE,
    parse::{CombinedStreamMsg, InboundEvent},
};

/// Build the combined-stream URL for
/// `(symbol × native-kline TFs) ∪ aggTrade ∪ depth@100ms`. S1/S5 are excluded
/// — Binance USD-M futures doesn't publish `kline_1s` / `kline_5s`, so those
/// bars are synthesized from the aggTrade stream by the sub-second aggregator.
///
/// Output looks like
/// `wss://fstream.binance.com/stream?streams=btcusdt@kline_1m/.../btcusdt@aggTrade/btcusdt@depth@100ms`.
fn combined_url(symbol: &str) -> String {
    let s = symbol.to_lowercase();
    let mut streams: Vec<String> = Timeframe::ALL
        .iter()
        .filter(|tf| tf.is_native_kline())
        .map(|tf| format!("{s}@kline_{}", tf.as_str()))
        .collect();
    streams.push(format!("{s}@aggTrade"));
    streams.push(format!("{s}@depth@100ms"));
    format!("{}/stream?streams={}", WS_BASE, streams.join("/"))
}

/// Connect to Binance, parse every kline / aggTrade event, and fan into the
/// right typed broadcast. Returns when the connection drops (either side);
/// the caller decides whether to reconnect.
pub async fn connect_and_stream(symbol: &str, txs: &BroadcastTxs) -> Result<()> {
    let url = combined_url(symbol);
    info!(symbol, "connecting to Binance combined stream");

    let (mut ws, _resp) = connect_async(&url)
        .await
        .with_context(|| format!("connect to {url}"))?;
    info!(symbol, "binance combined stream connected");

    loop {
        let msg = match ws.next().await {
            Some(Ok(msg)) => msg,
            Some(Err(e)) => return Err(anyhow!("ws read error: {e}")),
            None => return Err(anyhow!("ws stream ended")),
        };

        match msg {
            Message::Text(txt) => {
                if let Err(e) = handle_text(&txt, txs) {
                    warn!(error = %e, "failed to handle text frame");
                }
            }
            Message::Binary(bin) => {
                let txt = match std::str::from_utf8(&bin) {
                    Ok(s) => s.to_string(),
                    Err(_) => {
                        warn!("binary frame is not utf-8; skipping");
                        continue;
                    }
                };
                if let Err(e) = handle_text(&txt, txs) {
                    warn!(error = %e, "failed to handle binary-as-text frame");
                }
            }
            Message::Ping(payload) => {
                // tungstenite auto-pongs in most setups, but Binance's ping
                // interval (every 3 min) is tight enough that an explicit
                // reply removes any chance of a drop on the 10-min grace.
                if let Err(e) = ws.send(Message::Pong(payload)).await {
                    return Err(anyhow!("send pong: {e}"));
                }
            }
            Message::Pong(_) => {}
            Message::Close(frame) => {
                info!(?frame, "binance closed the connection");
                return Ok(());
            }
            Message::Frame(_) => {}
        }
    }
}

fn handle_text(txt: &str, txs: &BroadcastTxs) -> Result<()> {
    let env: CombinedStreamMsg = serde_json::from_str(txt).context("decode envelope")?;
    let Some(evt) = env.parse_event()? else {
        return Ok(());
    };
    // `send` errors when there are zero receivers; not fatal — just means
    // a consumer hasn't subscribed yet (boot ordering) or has fully
    // disconnected. The writer tasks hold permanent Receivers after boot,
    // so during normal operation this is never empty.
    match evt {
        InboundEvent::Kline(tick) => {
            debug!(
                symbol = %tick.symbol,
                tf = tick.tf.as_str(),
                is_closed = tick.is_closed,
                "kline tick"
            );
            let _ = txs.kline.send(tick);
        }
        InboundEvent::AggTrade(tick) => {
            debug!(
                symbol = %tick.symbol,
                agg_id = tick.trade.agg_id,
                "agg trade"
            );
            let _ = txs.trade.send(tick);
        }
        InboundEvent::Depth(diff) => {
            debug!(
                symbol = %diff.symbol,
                u_first = diff.first_update_id,
                u_final = diff.final_update_id,
                "depth diff"
            );
            let _ = txs.depth.send(diff);
        }
    }
    Ok(())
}
