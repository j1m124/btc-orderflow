//! Binance Futures kline REST client.
//!
//! Wraps `GET /fapi/v1/klines` — used for cold-start backfill and gap-heal
//! after the WS connection drops (every 24h on Binance's hard limit, plus
//! any transient network blips). The endpoint is unauthenticated and
//! weight-1, so we don't paginate-with-care; the in-flight call count is
//! tiny (Q10: ~16 max across all 9 tfs for a 7-day cold start).

use anyhow::{Context, Result};
use reqwest::Client;
use serde_json::Value;
use std::time::Duration;

use super::{
    AGGTRADES_PAGE_LIMIT, KLINES_PAGE_LIMIT, REST_BASE,
    parse::{KlineRow, TradeRow},
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
