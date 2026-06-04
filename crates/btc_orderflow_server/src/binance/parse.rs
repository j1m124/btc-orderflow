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
use chrono::{DateTime, TimeZone, Utc};
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
}
