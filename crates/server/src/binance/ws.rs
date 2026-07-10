//! Binance combined-stream WebSocket client.
//!
//! Subscribes to every `<symbol>@kline_<tf>` stream we care about + the
//! `<symbol>@aggTrade` stream over a single connection. Binance hard-
//! disconnects every connection at 24h plus may drop on network blips; the
//! caller wraps [`connect_and_stream`] in a reconnect loop that also runs
//! gap-heal REST passes between connections.

use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use protocol::Timeframe;
use futures::{SinkExt, StreamExt};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, info, warn};

use super::{
    BroadcastTxs, WS_BASE,
    parse::{CombinedStreamMsg, InboundEvent},
};

/// Cap on the initial WS handshake. A half-open TCP path can otherwise wedge
/// `connect_async` on the OS SYN/TLS retry budget (minutes) with no way for the
/// caller's reconnect loop to intervene. Normal connects finish in <1s, so 10s
/// is ~10× headroom; a spurious timeout just backs off and retries.
const WS_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Liveness watchdog for the read side — **the fix for the 2026-07-07 incident**
/// where the market stream wedged for 3 days with no error logged.
///
/// A half-open TCP connection (a NAT/router silently drops the path without a
/// FIN/RST reaching us) leaves `ws.next()` blocked *forever*: no error, so the
/// caller's reconnect + gap-heal loop never fires. We defend by requiring *some*
/// frame within this window; if none arrives we return an error and let the
/// caller reconnect.
///
/// Chosen above Binance's server-side keepalive cadence so it never false-fires
/// on a legitimately data-quiet connection: Binance sends an application `ping`
/// every ~3 min on every stream family, and those pings surface through
/// `ws.next()` (see the `Message::Ping` arm) and reset this timer. 4 min leaves
/// a full ping interval of margin over that 3-min floor while still detecting a
/// genuinely dead socket within minutes instead of never. (Our actual data
/// cadence — kline updates every ~1–2s, depth@100ms, markPrice@1s — is far
/// faster still, so in practice detection is bounded by data, not pings.)
const WS_IDLE_TIMEOUT: Duration = Duration::from_secs(240);

/// Binance Futures partitions WS streams across two endpoint families and
/// silently drops events for streams that don't belong to the connected
/// family:
///   * `/market/stream` carries klines + aggTrade (trade-derived feeds).
///   * `/stream` (unrouted / public) carries diff-depth.
/// A single connection therefore cannot cover our full feed set; we open one
/// per family and fan into the shared `BroadcastTxs`.
///
/// Build the combined-stream URL for the market endpoint:
/// `(symbol × native-kline TFs) ∪ aggTrade ∪ forceOrder`. S1/S5 are excluded
/// — Binance USD-M futures doesn't publish `kline_1s` / `kline_5s`, so those
/// bars are synthesized from the aggTrade stream by the sub-second aggregator.
///
/// `@forceOrder` is throttled by Binance to ≤1 message/sec per symbol — the
/// stream silently drops every liquidation but the latest in each 1-second
/// window. No REST endpoint exists to back-fill what we miss; this is an
/// upstream property we accept.
pub fn market_combined_url(symbol: &str) -> String {
    let s = symbol.to_lowercase();
    let mut streams: Vec<String> = Timeframe::ALL
        .iter()
        .filter(|tf| tf.is_native_kline())
        .map(|tf| format!("{s}@kline_{}", tf.as_str()))
        .collect();
    streams.push(format!("{s}@aggTrade"));
    streams.push(format!("{s}@forceOrder"));
    format!("{}/market/stream?streams={}", WS_BASE, streams.join("/"))
}

/// Combined-stream URL for the public endpoint, carrying diff-depth only.
/// Routed `/public/stream` isn't documented; bare `/stream` works and returns
/// the same `{stream, data}` envelope as the market endpoint.
pub fn public_combined_url(symbol: &str) -> String {
    let s = symbol.to_lowercase();
    format!("{}/stream?streams={s}@depth@100ms", WS_BASE)
}

/// Combined-stream URL for the mark-price feed. Rides the `/market/stream`
/// endpoint — the same family as kline / aggTrade / forceOrder, NOT the `/stream`
/// (diff-depth) family. This is load-bearing and was verified empirically: a
/// `<symbol>@markPrice@1s` subscription on `/stream` connects but silently
/// delivers ZERO frames (the documented endpoint-family drop), while the same
/// stream on `/market/stream` delivers normally. `@markPrice@1s` pushes a sample
/// every second (bare `@markPrice` is 3s); we want the finer cadence for crisp
/// USD open-interest notional and the future liquidation heatmap. Given its own
/// connection (rather than folded into the market socket) so the kline/trade
/// gap-heal + reconnect cadence stays independent of mark-price capture.
pub fn mark_price_combined_url(symbol: &str) -> String {
    let s = symbol.to_lowercase();
    format!("{}/market/stream?streams={s}@markPrice@1s", WS_BASE)
}

/// Connect to the given combined-stream URL, parse every event, and fan into
/// the right typed broadcast. Returns when the connection drops (either side);
/// the caller decides whether to reconnect. `label` is purely for logging so
/// the two concurrent connections (market vs public) can be told apart.
pub async fn connect_and_stream(
    label: &'static str,
    url: &str,
    txs: &BroadcastTxs,
) -> Result<()> {
    info!(label, url = %url, "connecting to Binance combined stream");

    let (mut ws, _resp) = tokio::time::timeout(WS_CONNECT_TIMEOUT, connect_async(url))
        .await
        .map_err(|_| anyhow!("connect to {url} timed out after {WS_CONNECT_TIMEOUT:?}"))?
        .with_context(|| format!("connect to {url}"))?;
    info!(label, "binance combined stream connected");

    loop {
        // Bound the read on a liveness watchdog: a half-open connection leaves
        // `ws.next()` pending forever, so without this the reconnect loop can
        // never fire (the 2026-07-07 3-day stall). Any frame — data, ping, or
        // pong — resets the window.
        let msg = match tokio::time::timeout(WS_IDLE_TIMEOUT, ws.next()).await {
            Ok(Some(Ok(msg))) => msg,
            Ok(Some(Err(e))) => return Err(anyhow!("ws read error: {e}")),
            Ok(None) => return Err(anyhow!("ws stream ended")),
            Err(_) => {
                return Err(anyhow!(
                    "ws idle: no frame from Binance in {WS_IDLE_TIMEOUT:?} \
                     ({label}); treating connection as half-open"
                ));
            }
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
        InboundEvent::Liquidation(tick) => {
            debug!(
                symbol = %tick.symbol,
                side = ?tick.liq.side,
                price = tick.liq.price,
                qty = tick.liq.qty,
                "liquidation"
            );
            let _ = txs.liquidation.send(tick);
        }
        InboundEvent::MarkPrice(tick) => {
            debug!(
                symbol = %tick.symbol,
                mark = tick.mark.mark_price,
                funding = ?tick.mark.funding_rate,
                "mark price"
            );
            let _ = txs.mark_price.send(tick);
        }
    }
    Ok(())
}
