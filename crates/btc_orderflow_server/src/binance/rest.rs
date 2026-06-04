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

use super::{KLINES_PAGE_LIMIT, REST_BASE, parse::KlineRow};

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
            .user_agent("btc_orderflow_server/0.1")
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
}
