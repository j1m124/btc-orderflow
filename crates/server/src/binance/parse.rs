//! Parsing the Binance kline JSON array into a strongly-typed row.
//!
//! Binance returns klines as fixed-position arrays mixing ints and decimal
//! strings:
//!
//! ```json
//! [
//!   1672531200000,        // open_time (ms)
//!   "16500.00",           // open
//!   "16550.10",           // high
//!   "16480.50",           // low
//!   "16520.30",           // close
//!   "1234.567",           // volume (base)
//!   1672531259999,        // close_time (ms)
//!   "20384521.34",        // quote_volume
//!   42,                   // number of trades
//!   "600.123",            // taker_buy_base_volume
//!   "9912345.67",         // taker_buy_quote_volume
//!   "0"                   // unused
//! ]
//! ```

use anyhow::{Context, Result, anyhow};
use protocol::Timeframe;
use chrono::{DateTime, TimeZone, Utc};
use serde::Deserialize;
use serde_json::Value;

// --- Trade row + tick -------------------------------------------------------

/// One aggregated trade (Binance aggTrade unit). The REST aggTrades endpoint
/// and the `@aggTrade` WS stream both decode into this. `is_buyer_maker`
/// follows Binance convention: true → resting bid was hit → taker SOLD →
/// "sell-side" aggression; false → taker BOUGHT.
#[derive(Clone, Debug)]
pub struct TradeRow {
    pub agg_id: i64,
    pub ts: DateTime<Utc>,
    pub price: f64,
    pub qty: f64,
    pub is_buyer_maker: bool,
}

impl TradeRow {
    /// Parse one element of the `/fapi/v1/aggTrades` REST response.
    pub fn from_rest_value(v: &Value) -> Result<Self> {
        let obj = v.as_object().context("aggTrade element is not a JSON object")?;
        let agg_id = obj
            .get("a")
            .and_then(|x| x.as_i64())
            .context("aggTrade.a missing")?;
        let price = obj
            .get("p")
            .and_then(|x| x.as_str())
            .context("aggTrade.p missing")?
            .parse::<f64>()
            .context("aggTrade.p decode")?;
        let qty = obj
            .get("q")
            .and_then(|x| x.as_str())
            .context("aggTrade.q missing")?
            .parse::<f64>()
            .context("aggTrade.q decode")?;
        let ts_ms = obj
            .get("T")
            .and_then(|x| x.as_i64())
            .context("aggTrade.T missing")?;
        let is_buyer_maker = obj
            .get("m")
            .and_then(|x| x.as_bool())
            .context("aggTrade.m missing")?;
        Ok(TradeRow {
            agg_id,
            ts: ms_to_utc(ts_ms),
            price,
            qty,
            is_buyer_maker,
        })
    }
}

/// A trade event traveling on the internal trade broadcast. Distinct name
/// from the wire-level [`protocol::ServerFrame::TradeTick`] frame — this is
/// the in-process event; the gateway converts batches of these into the
/// wire frame.
#[derive(Clone, Debug)]
pub struct TradeTick {
    pub symbol: String,
    pub trade: TradeRow,
}

/// What kind of event we just decoded off the combined-stream WS. Lets the
/// connection loop fan out into the right typed broadcast.
pub enum InboundEvent {
    Kline(Tick),
    AggTrade(TradeTick),
    Depth(DepthDiff),
}

// --- Depth diff event -------------------------------------------------------

/// One depth-diff event off `<symbol>@depth@100ms`. Carries the Binance
/// sequence-validation fields (`U`/`u`/`pu`) along with per-side level
/// changes. `(price, size)` pairs follow Binance convention: `size = 0`
/// means the level was removed.
#[derive(Clone, Debug)]
pub struct DepthDiff {
    pub symbol: String,
    pub event_time_ms: i64,
    pub first_update_id: i64,
    pub final_update_id: i64,
    pub prev_final_update_id: i64,
    pub bids: Vec<(f64, f64)>,
    pub asks: Vec<(f64, f64)>,
}

/// Wire shape of the `@depth@100ms` event payload.
#[derive(Debug, Deserialize)]
struct DepthDiffEventRaw {
    #[serde(rename = "e")]
    event: String,
    #[serde(rename = "E")]
    event_time_ms: i64,
    #[serde(rename = "s")]
    symbol: String,
    #[serde(rename = "U")]
    first_update_id: i64,
    #[serde(rename = "u")]
    final_update_id: i64,
    #[serde(rename = "pu")]
    prev_final_update_id: i64,
    #[serde(rename = "b")]
    bids: Vec<[String; 2]>,
    #[serde(rename = "a")]
    asks: Vec<[String; 2]>,
}

fn decode_level_array(arr: &[String; 2], field: &str) -> Result<(f64, f64)> {
    let price = arr[0]
        .parse::<f64>()
        .with_context(|| format!("{field}.price decode"))?;
    let size = arr[1]
        .parse::<f64>()
        .with_context(|| format!("{field}.size decode"))?;
    Ok((price, size))
}

/// One kline as it lands in the `candles` table. All fields are committed
/// from the REST response (no client-side derivation).
#[derive(Clone, Debug)]
pub struct KlineRow {
    pub open_time: DateTime<Utc>,
    pub close_time: DateTime<Utc>,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: f64,
    pub quote_volume: f64,
    pub trades: i32,
    pub taker_buy_vol: f64,
}

impl KlineRow {
    /// Parse one element of the top-level klines array.
    pub fn from_value(v: &Value) -> Result<Self> {
        let arr = v.as_array().context("kline element is not a JSON array")?;
        if arr.len() < 11 {
            return Err(anyhow!(
                "kline element has {} entries; expected >= 11",
                arr.len()
            ));
        }

        let open_time_ms = arr[0].as_i64().context("open_time not an integer")?;
        let open = parse_decimal(&arr[1], "open")?;
        let high = parse_decimal(&arr[2], "high")?;
        let low = parse_decimal(&arr[3], "low")?;
        let close = parse_decimal(&arr[4], "close")?;
        let volume = parse_decimal(&arr[5], "volume")?;
        let close_time_ms = arr[6].as_i64().context("close_time not an integer")?;
        let quote_volume = parse_decimal(&arr[7], "quote_volume")?;
        let trades = arr[8].as_i64().context("trades not an integer")? as i32;
        let taker_buy_vol = parse_decimal(&arr[9], "taker_buy_base_volume")?;

        Ok(KlineRow {
            open_time: ms_to_utc(open_time_ms),
            close_time: ms_to_utc(close_time_ms),
            open,
            high,
            low,
            close,
            volume,
            quote_volume,
            trades,
            taker_buy_vol,
        })
    }

    pub fn open_time_ms(&self) -> i64 {
        self.open_time.timestamp_millis()
    }
}

/// Binance ships numeric fields as decimal strings (no trailing zeros, no
/// exponential notation). Parse into f64.
fn parse_decimal(v: &Value, field: &str) -> Result<f64> {
    let s = v
        .as_str()
        .with_context(|| format!("{field} is not a string"))?;
    s.parse::<f64>()
        .with_context(|| format!("{field} = {s:?} is not a valid decimal"))
}

fn ms_to_utc(ms: i64) -> DateTime<Utc> {
    Utc.timestamp_millis_opt(ms)
        .single()
        .unwrap_or_else(|| Utc.timestamp_opt(0, 0).unwrap())
}

// --- Combined-stream WebSocket message --------------------------------------

/// Envelope around every event on a combined-stream WS connection.
///
/// Binance wraps each event as `{"stream": "<name>", "data": {...event...}}`.
/// We only care about kline streams; everything else returns `None` from
/// [`CombinedStreamMsg::parse_kline_event`].
#[derive(Debug, Deserialize)]
pub struct CombinedStreamMsg {
    #[allow(dead_code)]
    pub stream: String,
    pub data: Value,
}

/// Per-kline event payload — what lives inside `data.k` on a kline stream
/// frame. Binance ships single-letter field names; field-level renames
/// translate to the names we want to work with.
#[derive(Debug, Deserialize)]
struct KlineEventRaw {
    #[serde(rename = "e")]
    event: String,
    #[serde(rename = "s")]
    symbol: String,
    #[serde(rename = "k")]
    k: KlineInner,
}

/// `data.k` shape for aggTrade WS events. Field renames follow Binance's
/// single-letter convention.
#[derive(Debug, Deserialize)]
struct AggTradeEventRaw {
    #[serde(rename = "e")]
    event: String,
    #[serde(rename = "s")]
    symbol: String,
    #[serde(rename = "a")]
    agg_id: i64,
    #[serde(rename = "p")]
    price: String,
    #[serde(rename = "q")]
    qty: String,
    #[serde(rename = "T")]
    ts_ms: i64,
    #[serde(rename = "m")]
    is_buyer_maker: bool,
}

#[derive(Debug, Deserialize)]
struct KlineInner {
    #[serde(rename = "t")]
    open_time_ms: i64,
    #[serde(rename = "T")]
    close_time_ms: i64,
    #[serde(rename = "i")]
    interval: String,
    #[serde(rename = "o")]
    open: String,
    #[serde(rename = "h")]
    high: String,
    #[serde(rename = "l")]
    low: String,
    #[serde(rename = "c")]
    close: String,
    #[serde(rename = "v")]
    volume: String,
    #[serde(rename = "q")]
    quote_volume: String,
    #[serde(rename = "n")]
    trades: i64,
    #[serde(rename = "V")]
    taker_buy_vol: String,
    #[serde(rename = "x")]
    is_closed: bool,
}

/// A successfully-parsed kline tick — server-internal type that travels on
/// the broadcast channel. The `kline` field reuses the same [`KlineRow`]
/// shape the REST backfill produces, so the DB writer is symbol-agnostic
/// across sources.
#[derive(Clone, Debug)]
pub struct Tick {
    pub symbol: String,
    pub tf: Timeframe,
    pub kline: KlineRow,
    pub is_closed: bool,
}

impl CombinedStreamMsg {
    /// Decode the envelope into one of the inbound event variants we route.
    /// Returns `Ok(None)` for events we don't track (different interval,
    /// unknown event type).
    pub fn parse_event(&self) -> Result<Option<InboundEvent>> {
        let event = self
            .data
            .get("e")
            .and_then(|v| v.as_str())
            .context("event field `e` missing on combined-stream payload")?;
        match event {
            "kline" => Ok(self.parse_kline_event()?.map(InboundEvent::Kline)),
            "aggTrade" => Ok(self.parse_agg_trade_event()?.map(InboundEvent::AggTrade)),
            "depthUpdate" => Ok(self.parse_depth_event()?.map(InboundEvent::Depth)),
            _ => Ok(None),
        }
    }

    fn parse_depth_event(&self) -> Result<Option<DepthDiff>> {
        let raw: DepthDiffEventRaw =
            serde_json::from_value(self.data.clone()).context("decode depth payload")?;
        if raw.event != "depthUpdate" {
            return Ok(None);
        }
        let mut bids = Vec::with_capacity(raw.bids.len());
        for (i, lvl) in raw.bids.iter().enumerate() {
            bids.push(decode_level_array(lvl, &format!("bids[{i}]"))?);
        }
        let mut asks = Vec::with_capacity(raw.asks.len());
        for (i, lvl) in raw.asks.iter().enumerate() {
            asks.push(decode_level_array(lvl, &format!("asks[{i}]"))?);
        }
        Ok(Some(DepthDiff {
            symbol: raw.symbol,
            event_time_ms: raw.event_time_ms,
            first_update_id: raw.first_update_id,
            final_update_id: raw.final_update_id,
            prev_final_update_id: raw.prev_final_update_id,
            bids,
            asks,
        }))
    }

    /// Try to parse `self.data` as an aggTrade event.
    fn parse_agg_trade_event(&self) -> Result<Option<TradeTick>> {
        let raw: AggTradeEventRaw =
            serde_json::from_value(self.data.clone()).context("decode aggTrade payload")?;
        if raw.event != "aggTrade" {
            return Ok(None);
        }
        let trade = TradeRow {
            agg_id: raw.agg_id,
            ts: ms_to_utc(raw.ts_ms),
            price: parse_decimal_str(&raw.price, "aggTrade.p")?,
            qty: parse_decimal_str(&raw.qty, "aggTrade.q")?,
            is_buyer_maker: raw.is_buyer_maker,
        };
        Ok(Some(TradeTick {
            symbol: raw.symbol,
            trade,
        }))
    }

    /// Try to parse `self.data` as a kline event. Returns `Ok(None)` for
    /// non-kline events (we ignore them) or for events on intervals we don't
    /// track. Returns `Err` for malformed JSON we expected to be a kline.
    pub fn parse_kline_event(&self) -> Result<Option<Tick>> {
        let raw: KlineEventRaw =
            serde_json::from_value(self.data.clone()).context("decode kline event payload")?;
        if raw.event != "kline" {
            return Ok(None);
        }
        let tf = match Timeframe::from_str(&raw.k.interval) {
            Some(tf) => tf,
            None => return Ok(None), // not in our tracked set
        };

        let k = &raw.k;
        let kline = KlineRow {
            open_time: ms_to_utc(k.open_time_ms),
            close_time: ms_to_utc(k.close_time_ms),
            open: parse_decimal_str(&k.open, "open")?,
            high: parse_decimal_str(&k.high, "high")?,
            low: parse_decimal_str(&k.low, "low")?,
            close: parse_decimal_str(&k.close, "close")?,
            volume: parse_decimal_str(&k.volume, "volume")?,
            quote_volume: parse_decimal_str(&k.quote_volume, "quote_volume")?,
            trades: k.trades as i32,
            taker_buy_vol: parse_decimal_str(&k.taker_buy_vol, "taker_buy_vol")?,
        };

        Ok(Some(Tick {
            symbol: raw.symbol,
            tf,
            kline,
            is_closed: k.is_closed,
        }))
    }
}

fn parse_decimal_str(s: &str, field: &str) -> Result<f64> {
    s.parse::<f64>()
        .with_context(|| format!("{field} = {s:?} is not a valid decimal"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_binance_kline() {
        let raw = serde_json::json!([
            1672531200000_i64,
            "16500.00",
            "16550.10",
            "16480.50",
            "16520.30",
            "1234.567",
            1672531259999_i64,
            "20384521.34",
            42,
            "600.123",
            "9912345.67",
            "0"
        ]);
        let row = KlineRow::from_value(&raw).unwrap();
        assert_eq!(row.open_time_ms(), 1672531200000);
        assert_eq!(row.open, 16500.00);
        assert_eq!(row.close, 16520.30);
        assert_eq!(row.trades, 42);
        assert!((row.taker_buy_vol - 600.123).abs() < 1e-9);
    }

    #[test]
    fn errors_on_short_array() {
        let raw = serde_json::json!([1, 2, 3]);
        assert!(KlineRow::from_value(&raw).is_err());
    }

    #[test]
    fn parses_a_combined_stream_kline_event() {
        let raw = serde_json::json!({
            "stream": "btcusdt@kline_1m",
            "data": {
                "e": "kline",
                "E": 1672531259900_i64,
                "s": "BTCUSDT",
                "k": {
                    "t": 1672531200000_i64,
                    "T": 1672531259999_i64,
                    "s": "BTCUSDT",
                    "i": "1m",
                    "o": "16500.00",
                    "c": "16520.30",
                    "h": "16550.10",
                    "l": "16480.50",
                    "v": "1234.567",
                    "n": 42,
                    "x": false,
                    "q": "20384521.34",
                    "V": "600.123",
                    "Q": "9912345.67"
                }
            }
        });
        let env: CombinedStreamMsg = serde_json::from_value(raw).unwrap();
        let tick = env.parse_kline_event().unwrap().unwrap();
        assert_eq!(tick.symbol, "BTCUSDT");
        assert_eq!(tick.tf, Timeframe::M1);
        assert_eq!(tick.kline.open_time_ms(), 1672531200000);
        assert_eq!(tick.kline.close, 16520.30);
        assert_eq!(tick.kline.trades, 42);
        assert!(!tick.is_closed);
    }

    #[test]
    fn parses_a_combined_stream_agg_trade() {
        let raw = serde_json::json!({
            "stream": "btcusdt@aggTrade",
            "data": {
                "e": "aggTrade",
                "E": 1672531200900_i64,
                "s": "BTCUSDT",
                "a": 1234567,
                "p": "16500.50",
                "q": "0.123",
                "f": 100,
                "l": 102,
                "T": 1672531200500_i64,
                "m": true
            }
        });
        let env: CombinedStreamMsg = serde_json::from_value(raw).unwrap();
        let evt = env.parse_event().unwrap().expect("agg trade decoded");
        match evt {
            InboundEvent::AggTrade(tick) => {
                assert_eq!(tick.symbol, "BTCUSDT");
                assert_eq!(tick.trade.agg_id, 1234567);
                assert!((tick.trade.price - 16500.50).abs() < 1e-9);
                assert!((tick.trade.qty - 0.123).abs() < 1e-9);
                assert!(tick.trade.is_buyer_maker);
                assert_eq!(tick.trade.ts.timestamp_millis(), 1672531200500);
            }
            _ => panic!("expected AggTrade variant"),
        }
    }

    #[test]
    fn parses_an_agg_trade_rest_row() {
        let raw = serde_json::json!({
            "a": 26129,
            "p": "16550.50",
            "q": "0.250",
            "f": 27781,
            "l": 27781,
            "T": 1498793709153_i64,
            "m": false
        });
        let row = TradeRow::from_rest_value(&raw).unwrap();
        assert_eq!(row.agg_id, 26129);
        assert!((row.price - 16550.50).abs() < 1e-9);
        assert!(!row.is_buyer_maker);
    }

    #[test]
    fn parses_a_combined_stream_depth_update() {
        let raw = serde_json::json!({
            "stream": "btcusdt@depth@100ms",
            "data": {
                "e": "depthUpdate",
                "E": 1672531200500_i64,
                "T": 1672531200490_i64,
                "s": "BTCUSDT",
                "U": 100,
                "u": 105,
                "pu": 99,
                "b": [["16500.0", "1.5"], ["16499.0", "0.0"]],
                "a": [["16510.0", "2.5"]]
            }
        });
        let env: CombinedStreamMsg = serde_json::from_value(raw).unwrap();
        let evt = env.parse_event().unwrap().expect("depth diff decoded");
        match evt {
            InboundEvent::Depth(diff) => {
                assert_eq!(diff.symbol, "BTCUSDT");
                assert_eq!(diff.first_update_id, 100);
                assert_eq!(diff.final_update_id, 105);
                assert_eq!(diff.prev_final_update_id, 99);
                assert_eq!(diff.bids.len(), 2);
                assert_eq!(diff.bids[0], (16500.0, 1.5));
                assert_eq!(diff.bids[1], (16499.0, 0.0));
                assert_eq!(diff.asks[0], (16510.0, 2.5));
            }
            _ => panic!("expected Depth variant"),
        }
    }

    #[test]
    fn unknown_interval_returns_none() {
        let raw = serde_json::json!({
            "stream": "btcusdt@kline_3m",
            "data": {
                "e": "kline",
                "E": 1_i64,
                "s": "BTCUSDT",
                "k": {
                    "t": 0_i64, "T": 0_i64, "s": "BTCUSDT", "i": "3m",
                    "o": "1", "c": "1", "h": "1", "l": "1", "v": "0",
                    "n": 0, "x": false, "q": "0", "V": "0", "Q": "0"
                }
            }
        });
        let env: CombinedStreamMsg = serde_json::from_value(raw).unwrap();
        assert!(env.parse_kline_event().unwrap().is_none());
    }
}
