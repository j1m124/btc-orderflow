//! Form 4 insider-transactions feed for the user's watchlist. Fetches
//! `GET /v1/insider?tickers=…` from centoflow-server, which reads from the
//! scheduler-populated `insider_trades` table.
//!
//! Mirrors `services/filings.rs` — watchlist-scoped, reload on watchlist
//! mutation + manual refresh, last-good preserved on error.

use std::time::Duration;

use chrono::{DateTime, TimeZone as _, Utc};
use gpui::{App, AppContext as _, Context, Entity, EventEmitter, Global, SharedString, Task};
use serde::Deserialize;

use crate::net::{CentoflowConfig, HttpClient};
use crate::services::watchlist::{WatchlistEvent, WatchlistServiceHandle};

#[derive(Clone, Debug)]
pub struct InsiderTrade {
    pub ticker: String,
    pub filer_name: String,
    pub insider_title: String,
    pub transaction_date: String, // "YYYY-MM-DD" — may be empty
    pub transaction_at: Option<DateTime<Utc>>,
    pub transaction_code: String,
    pub shares: Option<f64>,
    pub price: Option<f64>,
    pub value: Option<f64>,
    pub shares_owned_after: Option<f64>,
    pub accession_number: String,
    pub filing_url: String,
}

#[derive(Clone, Debug)]
pub enum InsiderState {
    Idle,
    Loading,
    Loaded {
        trades: Vec<InsiderTrade>,
        fetched_at: DateTime<Utc>,
    },
    Error {
        message: String,
        last: Option<(Vec<InsiderTrade>, DateTime<Utc>)>,
    },
}

#[derive(Clone, Debug)]
pub enum InsiderEvent {
    Changed,
}

pub struct InsiderService {
    state: InsiderState,
    inflight: Option<Task<()>>,
    _watchlist_sub: gpui::Subscription,
}

impl EventEmitter<InsiderEvent> for InsiderService {}

impl InsiderService {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let watchlist = cx.global::<WatchlistServiceHandle>().0.clone();
        let sub = cx.subscribe(&watchlist, |_this, _wl, _ev: &WatchlistEvent, cx| {
            cx.spawn(async move |this, cx| {
                let _ = this.update(cx, |s, cx| s.reload(cx));
            })
            .detach();
        });
        cx.spawn(async move |this, cx| {
            let _ = this.update(cx, |s, cx| s.reload(cx));
        })
        .detach();
        Self {
            state: InsiderState::Idle,
            inflight: None,
            _watchlist_sub: sub,
        }
    }

    pub fn state(&self) -> &InsiderState {
        &self.state
    }

    pub fn reload(&mut self, cx: &mut Context<Self>) {
        if self.inflight.is_some() {
            return;
        }
        let client = cx.global::<HttpClient>().0.clone();
        let cfg = cx.global::<CentoflowConfig>().clone();
        let tickers: Vec<SharedString> = cx
            .global::<WatchlistServiceHandle>()
            .0
            .read(cx)
            .symbols()
            .to_vec();

        let last = match &self.state {
            InsiderState::Loaded { trades, fetched_at } => Some((trades.clone(), *fetched_at)),
            InsiderState::Error { last, .. } => last.clone(),
            _ => None,
        };

        self.state = InsiderState::Loading;
        cx.emit(InsiderEvent::Changed);
        cx.notify();

        let task = cx.spawn(async move |this, cx| {
            let result = fetch(&client, &cfg, &tickers).await;
            let _ = this.update(cx, |s, cx| {
                s.inflight = None;
                match result {
                    Ok((trades, fetched_at)) => {
                        s.state = InsiderState::Loaded { trades, fetched_at };
                    }
                    Err(err) => {
                        s.state = InsiderState::Error {
                            message: format!("{err:#}"),
                            last: last.clone(),
                        };
                    }
                }
                cx.emit(InsiderEvent::Changed);
                cx.notify();
            });
        });
        self.inflight = Some(task);
    }
}

#[derive(Clone)]
pub struct InsiderServiceHandle(pub Entity<InsiderService>);
impl Global for InsiderServiceHandle {}

pub fn init(cx: &mut App) {
    let entity = cx.new(InsiderService::new);
    cx.set_global(InsiderServiceHandle(entity));
}

// ---------------------------------------------------------------------------
// Wire types — keep in sync with internal/api/resources.go::InsiderTradeItem
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct InsiderResponse {
    #[serde(default)]
    trades: Vec<RawInsider>,
    #[serde(default)]
    fetched_at: i64,
}

#[derive(Debug, Deserialize)]
struct RawInsider {
    #[serde(default)]
    ticker: String,
    #[serde(default)]
    filer_name: String,
    #[serde(default)]
    insider_title: String,
    #[serde(default)]
    transaction_date: String,
    #[serde(default)]
    transaction_date_ms: i64,
    #[serde(default)]
    transaction_code: String,
    #[serde(default)]
    shares: Option<f64>,
    #[serde(default)]
    price: Option<f64>,
    #[serde(default)]
    value: Option<f64>,
    #[serde(default)]
    shares_owned_after: Option<f64>,
    #[serde(default)]
    accession_number: String,
    #[serde(default)]
    filing_url: String,
}

async fn fetch(
    client: &reqwest::Client,
    cfg: &CentoflowConfig,
    tickers: &[SharedString],
) -> anyhow::Result<(Vec<InsiderTrade>, DateTime<Utc>)> {
    let tickers_csv = tickers
        .iter()
        .map(|s| s.as_ref())
        .collect::<Vec<_>>()
        .join(",");
    let url = format!("{}/v1/insider?tickers={}", cfg.base_url, tickers_csv);
    let mut req = client.get(&url);
    if let Some(token) = &cfg.token {
        req = req.bearer_auth(token);
    }
    let resp = req.timeout(Duration::from_secs(25)).send().await?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("centoflow /v1/insider returned HTTP {status}: {body}");
    }
    let parsed: InsiderResponse = resp.json().await?;
    let fetched_at = Utc
        .timestamp_millis_opt(parsed.fetched_at)
        .single()
        .unwrap_or_else(Utc::now);
    let trades = parsed
        .trades
        .into_iter()
        .map(|r| {
            let transaction_at = if r.transaction_date_ms > 0 {
                Utc.timestamp_millis_opt(r.transaction_date_ms).single()
            } else {
                None
            };
            InsiderTrade {
                ticker: r.ticker,
                filer_name: r.filer_name,
                insider_title: r.insider_title,
                transaction_date: r.transaction_date,
                transaction_at,
                transaction_code: r.transaction_code,
                shares: r.shares,
                price: r.price,
                value: r.value,
                shares_owned_after: r.shares_owned_after,
                accession_number: r.accession_number,
                filing_url: r.filing_url,
            }
        })
        .collect();
    Ok((trades, fetched_at))
}
