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
