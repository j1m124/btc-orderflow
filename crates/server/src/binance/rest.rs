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
}
