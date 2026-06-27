//! Binance Futures kline REST client.
//!
//! Wraps `GET /fapi/v1/klines` — used for cold-start backfill and gap-heal
//! after the WS connection drops (every 24h on Binance's hard limit, plus
//! any transient network blips). The endpoint is unauthenticated and
//! weight-1, so we don't paginate-with-care; the in-flight call count is
//! tiny (Q10: ~16 max across all 9 tfs for a 7-day cold start).

use anyhow::{Context, Result};
use chrono::{DateTime, TimeZone, Utc};
use reqwest::Client;
use serde_json::Value;
use std::time::Duration;

use super::{
    AGGTRADES_PAGE_LIMIT, FUNDING_RATE_PAGE_LIMIT, KLINES_PAGE_LIMIT,
    OPEN_INTEREST_HIST_PAGE_LIMIT, REST_BASE,
    parse::{FundingRateRow, KlineRow, MarkPriceRow, OpenInterestRow, TradeRow},
};

/// Bootstrap response from `GET /fapi/v1/depth`.
#[derive(Debug)]
pub struct DepthSnapshot {
    pub last_update_id: i64,
    pub bids: Vec<(f64, f64)>,
    pub asks: Vec<(f64, f64)>,
}

pub struct RestClient {
    http: Client,
    base: String,
}

impl Default for RestClient {
    fn default() -> Self {
        Self::new(REST_BASE)
    }
}

impl RestClient {
    pub fn new(base: &str) -> Self {
        let http = Client::builder()
            .timeout(Duration::from_secs(15))
            .user_agent("server/0.1")
            .build()
            .expect("reqwest client builder");
        Self {
            http,
            base: base.trim_end_matches('/').to_string(),
        }
    }

    /// Fetch up to `limit` (max [`KLINES_PAGE_LIMIT`]) closed klines for
    /// `(symbol, interval)`, starting at `start_time_ms`. Returns rows in
    /// chronological order.
    pub async fn klines(
        &self,
        symbol: &str,
        interval: &str,
        start_time_ms: i64,
        limit: u32,
    ) -> Result<Vec<KlineRow>> {
        let limit = limit.min(KLINES_PAGE_LIMIT);
        let url = format!("{}/fapi/v1/klines", self.base);
        let resp = self
            .http
            .get(&url)
            .query(&[
                ("symbol", symbol),
                ("interval", interval),
                ("startTime", &start_time_ms.to_string()),
                ("limit", &limit.to_string()),
            ])
            .send()
            .await
            .with_context(|| format!("GET {url}"))?
            .error_for_status()
            .with_context(|| format!("binance returned non-2xx for {symbol} {interval}"))?;

        let arr: Value = resp
            .json()
            .await
            .context("decode klines JSON")?;
        let items = arr
            .as_array()
            .context("klines response is not a JSON array")?;

        let mut rows = Vec::with_capacity(items.len());
        for (idx, v) in items.iter().enumerate() {
            let row = KlineRow::from_value(v)
                .with_context(|| format!("parse kline #{idx} for {symbol} {interval}"))?;
            rows.push(row);
        }
        Ok(rows)
    }

    /// Fetch up to `limit` (max [`AGGTRADES_PAGE_LIMIT`]) aggregated trades
    /// for `symbol`. Pagination uses either `from_id` (cursor-driven gap-
    /// heal — Binance returns trades with id ≥ from_id) or `start_time_ms`
    /// (cold-start window). The two are mutually exclusive per Binance's
    /// API; we encode that as `Option`s and never set both.
    ///
    /// Returns rows in chronological order (oldest first).
    ///
    /// Weight: 20 per call regardless of `limit`. The caller paces requests
    /// against the 2400/min IP budget — see `ingest::backfill_trades`.
    pub async fn agg_trades(
        &self,
        symbol: &str,
        from_id: Option<i64>,
        start_time_ms: Option<i64>,
        limit: u32,
    ) -> Result<Vec<TradeRow>> {
        let limit = limit.min(AGGTRADES_PAGE_LIMIT);
        let url = format!("{}/fapi/v1/aggTrades", self.base);
        let mut req = self.http.get(&url).query(&[
            ("symbol", symbol),
            ("limit", &limit.to_string()),
        ]);
        if let Some(id) = from_id {
            req = req.query(&[("fromId", id.to_string())]);
        }
        if let Some(ts) = start_time_ms {
            req = req.query(&[("startTime", ts.to_string())]);
        }
        let resp = req
            .send()
            .await
            .with_context(|| format!("GET {url}"))?
            .error_for_status()
            .with_context(|| format!("binance returned non-2xx for aggTrades {symbol}"))?;

        let arr: Value = resp.json().await.context("decode aggTrades JSON")?;
        let items = arr
            .as_array()
            .context("aggTrades response is not a JSON array")?;

        let mut rows = Vec::with_capacity(items.len());
        for (idx, v) in items.iter().enumerate() {
            let row = TradeRow::from_rest_value(v)
                .with_context(|| format!("parse aggTrade #{idx} for {symbol}"))?;
            rows.push(row);
        }
        Ok(rows)
    }

    /// Fetch the current orderbook snapshot for `symbol` (top `limit` levels
    /// each side). Used to bootstrap the in-memory book for the diff-stream
    /// maintainer; the returned `last_update_id` is the sequence cursor
    /// against which subsequent diffs are validated.
    ///
    /// Weight depends on `limit`: 2 for ≤100, 5 for ≤500, 10 for ≤1000.
    /// We always request 1000 — full depth gives the maintainer a robust
    /// resync point even if the diff stream momentarily lags during boot.
    pub async fn depth_snapshot(&self, symbol: &str, limit: u32) -> Result<DepthSnapshot> {
        let url = format!("{}/fapi/v1/depth", self.base);
        let resp = self
            .http
            .get(&url)
            .query(&[
                ("symbol", symbol),
                ("limit", &limit.to_string()),
            ])
            .send()
            .await
            .with_context(|| format!("GET {url}"))?
            .error_for_status()
            .with_context(|| format!("binance returned non-2xx for depth {symbol}"))?;

        let json: Value = resp.json().await.context("decode depth JSON")?;
        let last_update_id = json
            .get("lastUpdateId")
            .and_then(|v| v.as_i64())
            .context("depth.lastUpdateId missing")?;
        let bids = parse_depth_levels(&json, "bids")?;
        let asks = parse_depth_levels(&json, "asks")?;

        Ok(DepthSnapshot {
            last_update_id,
            bids,
            asks,
        })
    }

    /// Current open interest for `symbol` from `GET /fapi/v1/openInterest`
    /// (weight 1, unauthenticated). Returns the value in contracts (base
    /// asset) tagged with Binance's own `time` — using the exchange timestamp
    /// rather than wall-clock means consecutive polls that land on the same
    /// unchanged OI snapshot collide on the `(symbol, ts)` PK and dedupe for
    /// free (Binance recomputes OI slower than our poll cadence).
    pub async fn open_interest(&self, symbol: &str) -> Result<OpenInterestRow> {
        let url = format!("{}/fapi/v1/openInterest", self.base);
        let resp = self
            .http
            .get(&url)
            .query(&[("symbol", symbol)])
            .send()
            .await
            .with_context(|| format!("GET {url}"))?
            .error_for_status()
            .with_context(|| format!("binance returned non-2xx for openInterest {symbol}"))?;

        let json: Value = resp.json().await.context("decode openInterest JSON")?;
        let oi = json
            .get("openInterest")
            .and_then(|v| v.as_str())
            .context("openInterest.openInterest missing")?
            .parse::<f64>()
            .context("openInterest.openInterest decode")?;
        let ts_ms = json
            .get("time")
            .and_then(|v| v.as_i64())
            .context("openInterest.time missing")?;
        Ok(OpenInterestRow {
            ts: ms_to_utc(ts_ms),
            oi,
        })
    }

    /// Historical open-interest statistics from `GET /futures/data/open
    /// InterestHist`. Only `sumOpenInterest` (contracts) + `timestamp` are
    /// decoded; `sumOpenInterestValue` (USD) is dropped — the client derives
    /// USD from the candle close. `period` is one of Binance's fixed buckets
    /// ("5m","15m",…,"1d"); only the last 30 days are available and `limit`
    /// caps at [`OPEN_INTEREST_HIST_PAGE_LIMIT`]. Returns rows ascending.
    pub async fn open_interest_hist(
        &self,
        symbol: &str,
        period: &str,
        start_time_ms: i64,
        limit: u32,
    ) -> Result<Vec<OpenInterestRow>> {
        let limit = limit.min(OPEN_INTEREST_HIST_PAGE_LIMIT);
        let url = format!("{}/futures/data/openInterestHist", self.base);
        let resp = self
            .http
            .get(&url)
            .query(&[
                ("symbol", symbol),
                ("period", period),
                ("startTime", &start_time_ms.to_string()),
                ("limit", &limit.to_string()),
            ])
            .send()
            .await
            .with_context(|| format!("GET {url}"))?
            .error_for_status()
            .with_context(|| format!("binance returned non-2xx for openInterestHist {symbol}"))?;

        let arr: Value = resp.json().await.context("decode openInterestHist JSON")?;
        let items = arr
            .as_array()
            .context("openInterestHist response is not a JSON array")?;

        let mut rows = Vec::with_capacity(items.len());
        for (idx, v) in items.iter().enumerate() {
            let obj = v
                .as_object()
                .with_context(|| format!("openInterestHist #{idx} is not an object"))?;
            let oi = obj
                .get("sumOpenInterest")
                .and_then(|x| x.as_str())
                .with_context(|| format!("openInterestHist #{idx}.sumOpenInterest missing"))?
                .parse::<f64>()
                .with_context(|| format!("openInterestHist #{idx}.sumOpenInterest decode"))?;
            let ts_ms = obj
                .get("timestamp")
                .and_then(|x| x.as_i64())
                .with_context(|| format!("openInterestHist #{idx}.timestamp missing"))?;
            rows.push(OpenInterestRow {
                ts: ms_to_utc(ts_ms),
                oi,
            });
        }
        Ok(rows)
    }

    /// Mark-price OHLC klines from `GET /fapi/v1/markPriceKlines` (weight 1).
    /// Same fixed-position array shape as `/fapi/v1/klines`, but the OHLC are
    /// mark prices and the volume/trade columns are zero. Used for cold-start /
    /// gap-heal backfill of the `mark_price` table; we keep only the bar close
    /// (mapped to a sample at the bar open_time) — index / settle / funding
    /// have no kline form, so those columns stay `None` on backfilled rows.
    /// Returns rows in chronological order.
    pub async fn mark_price_klines(
        &self,
        symbol: &str,
        interval: &str,
        start_time_ms: i64,
        limit: u32,
    ) -> Result<Vec<MarkPriceRow>> {
        let limit = limit.min(KLINES_PAGE_LIMIT);
        let url = format!("{}/fapi/v1/markPriceKlines", self.base);
        let resp = self
            .http
            .get(&url)
            .query(&[
                ("symbol", symbol),
                ("interval", interval),
                ("startTime", &start_time_ms.to_string()),
                ("limit", &limit.to_string()),
            ])
            .send()
            .await
            .with_context(|| format!("GET {url}"))?
            .error_for_status()
            .with_context(|| format!("binance returned non-2xx for markPriceKlines {symbol}"))?;

        let arr: Value = resp.json().await.context("decode markPriceKlines JSON")?;
        let items = arr
            .as_array()
            .context("markPriceKlines response is not a JSON array")?;

        let mut rows = Vec::with_capacity(items.len());
        for (idx, v) in items.iter().enumerate() {
            let row = KlineRow::from_value(v)
                .with_context(|| format!("parse markPriceKline #{idx} for {symbol}"))?;
            rows.push(MarkPriceRow {
                ts: row.open_time,
                mark_price: row.close,
                index_price: None,
                est_settle_price: None,
                funding_rate: None,
            });
        }
        Ok(rows)
    }

    /// Settled funding-rate history from `GET /fapi/v1/fundingRate` (weight 1).
    /// One row per 8h settlement, ascending. `startTime` filters from that
    /// instant; `limit` caps at [`FUNDING_RATE_PAGE_LIMIT`]. Only `fundingTime`
    /// + `fundingRate` are decoded.
    pub async fn funding_rate_hist(
        &self,
        symbol: &str,
        start_time_ms: i64,
        limit: u32,
    ) -> Result<Vec<FundingRateRow>> {
        let limit = limit.min(FUNDING_RATE_PAGE_LIMIT);
        let url = format!("{}/fapi/v1/fundingRate", self.base);
        let resp = self
            .http
            .get(&url)
            .query(&[
                ("symbol", symbol),
                ("startTime", &start_time_ms.to_string()),
                ("limit", &limit.to_string()),
            ])
            .send()
            .await
            .with_context(|| format!("GET {url}"))?
            .error_for_status()
            .with_context(|| format!("binance returned non-2xx for fundingRate {symbol}"))?;

        let arr: Value = resp.json().await.context("decode fundingRate JSON")?;
        let items = arr
            .as_array()
            .context("fundingRate response is not a JSON array")?;

        let mut rows = Vec::with_capacity(items.len());
        for (idx, v) in items.iter().enumerate() {
            let obj = v
                .as_object()
                .with_context(|| format!("fundingRate #{idx} is not an object"))?;
            let ts_ms = obj
                .get("fundingTime")
                .and_then(|x| x.as_i64())
                .with_context(|| format!("fundingRate #{idx}.fundingTime missing"))?;
            let rate = obj
                .get("fundingRate")
                .and_then(|x| x.as_str())
                .with_context(|| format!("fundingRate #{idx}.fundingRate missing"))?
                .parse::<f64>()
                .with_context(|| format!("fundingRate #{idx}.fundingRate decode"))?;
            rows.push(FundingRateRow {
                ts: ms_to_utc(ts_ms),
                rate,
            });
        }
        Ok(rows)
    }
}

fn ms_to_utc(ms: i64) -> DateTime<Utc> {
    Utc.timestamp_millis_opt(ms)
        .single()
        .unwrap_or_else(|| Utc.timestamp_opt(0, 0).unwrap())
}

fn parse_depth_levels(json: &Value, field: &str) -> Result<Vec<(f64, f64)>> {
    let arr = json
        .get(field)
        .and_then(|v| v.as_array())
        .with_context(|| format!("depth.{field} missing or not array"))?;
    let mut out = Vec::with_capacity(arr.len());
    for (i, v) in arr.iter().enumerate() {
        let lvl = v.as_array().with_context(|| format!("{field}[{i}] not array"))?;
        if lvl.len() != 2 {
            return Err(anyhow::anyhow!("{field}[{i}] not a 2-element array"));
        }
        let price = lvl[0]
            .as_str()
            .context("depth level price not string")?
            .parse::<f64>()
            .with_context(|| format!("{field}[{i}].price decode"))?;
        let size = lvl[1]
            .as_str()
            .context("depth level size not string")?
            .parse::<f64>()
            .with_context(|| format!("{field}[{i}].size decode"))?;
        out.push((price, size));
    }
    Ok(out)
}
