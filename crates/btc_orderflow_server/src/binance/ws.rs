//! Binance combined-stream WebSocket client.
//!
//! Subscribes to every `<symbol>@kline_<tf>` stream we care about over a
//! single connection (Q6: relay-and-persist, one Binance stream per tf).
//! Binance hard-disconnects every connection at 24h plus may drop on
//! network blips; the caller wraps [`connect_and_stream`] in a reconnect
//! loop that also runs a gap-heal between connections.

use anyhow::{Context, Result, anyhow};
use btc_orderflow_protocol::Timeframe;
use futures::{SinkExt, StreamExt};
use tokio::sync::broadcast;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, info, warn};

use super::{WS_BASE, parse::CombinedStreamMsg};

/// Build the combined-stream URL for `(symbol × Timeframe::ALL)`.
///
/// Output looks like
/// `wss://fstream.binance.com/stream?streams=btcusdt@kline_1m/btcusdt@kline_5m/...`.
fn combined_url(symbol: &str) -> String {
    let s = symbol.to_lowercase();
    let streams: Vec<String> = Timeframe::ALL
        .iter()
        .map(|tf| format!("{s}@kline_{}", tf.as_str()))
        .collect();
    format!("{}/stream?streams={}", WS_BASE, streams.join("/"))
}

/// Connect to Binance, parse every kline event, and broadcast it. Returns
/// when the connection drops (either side); the caller decides whether to
/// reconnect.
pub async fn connect_and_stream(
    symbol: &str,
    broadcast_tx: &broadcast::Sender<super::parse::Tick>,
) -> Result<()> {
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
                if let Err(e) = handle_text(&txt, broadcast_tx) {
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
                if let Err(e) = handle_text(&txt, broadcast_tx) {
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

fn handle_text(
    txt: &str,
    broadcast_tx: &broadcast::Sender<super::parse::Tick>,
) -> Result<()> {
    let env: CombinedStreamMsg = serde_json::from_str(txt).context("decode envelope")?;
    let tick = match env.parse_kline_event()? {
        Some(t) => t,
        None => return Ok(()),
    };
    debug!(
        symbol = %tick.symbol,
        tf = tick.tf.as_str(),
        is_closed = tick.is_closed,
        "kline tick"
    );
    // `send` errors when there are zero receivers; not fatal — just means
    // the gateway hasn't subscribed yet (boot ordering) or has fully
    // disconnected. The DB writer holds one Receiver permanently after
    // boot, so during normal operation this is never empty.
    let _ = broadcast_tx.send(tick);
    Ok(())
}
