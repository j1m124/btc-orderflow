//! SEC 8-K filings feed for the user's watchlist. Fetches
//! `GET /v1/filings?tickers=…` from centoflow-server, which fans out per
//! ticker to Massive and merges results. Service is a singleton; the panel
//! triggers `reload()` on mount, on the user's refresh click, and on
//! `WatchlistEvent::Changed` so the list stays scoped to the current
//! watchlist.
//!
//! Per-ticker server-side cache TTL is 5 min — repeated reloads inside that
//! window are cheap.

use std::time::Duration;

use chrono::{DateTime, TimeZone as _, Utc};
use gpui::{App, AppContext as _, Context, Entity, EventEmitter, Global, SharedString, Task};
use serde::Deserialize;

use crate::net::{CentoflowConfig, HttpClient};
use crate::services::watchlist::{WatchlistEvent, WatchlistServiceHandle};

#[derive(Clone, Debug)]
pub struct Filing {
    pub ticker: String,
    pub form_type: String,
    pub filing_date: String, // "YYYY-MM-DD"
    pub filed_at: DateTime<Utc>,
    pub url: String,
    pub title: String,
    pub accession_number: String,
}

#[derive(Clone, Debug)]
pub enum FilingsState {
    Idle,
    Loading,
    Loaded {
        filings: Vec<Filing>,
        fetched_at: DateTime<Utc>,
    },
    Error {
        message: String,
        last: Option<(Vec<Filing>, DateTime<Utc>)>,
    },
}

#[derive(Clone, Debug)]
pub enum FilingsEvent {
    Changed,
}

pub struct FilingsService {
    state: FilingsState,
    inflight: Option<Task<()>>,
    _watchlist_sub: gpui::Subscription,
}

impl EventEmitter<FilingsEvent> for FilingsService {}

impl FilingsService {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let watchlist = cx.global::<WatchlistServiceHandle>().0.clone();
        // Re-fetch on every watchlist mutation. Deferred via spawn so we
        // never call reload (which `read`s the watchlist entity) while
        // WatchlistService itself is still mid-`update` — that's a classic
        // gpui RefCell double-borrow panic.
        let sub = cx.subscribe(&watchlist, |_this, _wl, _ev: &WatchlistEvent, cx| {
            cx.spawn(async move |this, cx| {
                let _ = this.update(cx, |s, cx| s.reload(cx));
            })
            .detach();
        });
        // Defer the initial fetch the same way. Calling reload directly here
        // would (a) try to read globals before the entity slot is fully
        // populated and (b) put the service into Loading + an in-flight task
        // before any panel exists — if a panel then mounts and `read`s state
        // while the fetch-completion update closure is running, render hits
        // "already borrowed". Spawning pushes the first fetch one tick later.
        cx.spawn(async move |this, cx| {
            let _ = this.update(cx, |s, cx| s.reload(cx));
        })
        .detach();
        Self {
            state: FilingsState::Idle,
            inflight: None,
            _watchlist_sub: sub,
        }
    }

    pub fn state(&self) -> &FilingsState {
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

        // Preserve last-good so an error doesn't blank the list.
        let last = match &self.state {
            FilingsState::Loaded { filings, fetched_at } => {
                Some((filings.clone(), *fetched_at))
            }
            FilingsState::Error { last, .. } => last.clone(),
            _ => None,
        };

        self.state = FilingsState::Loading;
        cx.emit(FilingsEvent::Changed);
        cx.notify();

        let task = cx.spawn(async move |this, cx| {
            let result = fetch(&client, &cfg, &tickers).await;
            let _ = this.update(cx, |s, cx| {
                s.inflight = None;
                match result {
                    Ok((filings, fetched_at)) => {
                        s.state = FilingsState::Loaded { filings, fetched_at };
                    }
                    Err(err) => {
                        s.state = FilingsState::Error {
                            message: format!("{err:#}"),
                            last: last.clone(),
                        };
                    }
                }
                cx.emit(FilingsEvent::Changed);
                cx.notify();
            });
        });
        self.inflight = Some(task);
    }
}

#[derive(Clone)]
pub struct FilingsServiceHandle(pub Entity<FilingsService>);
impl Global for FilingsServiceHandle {}

pub fn init(cx: &mut App) {
    let entity = cx.new(FilingsService::new);
    cx.set_global(FilingsServiceHandle(entity));
}

// ---------------------------------------------------------------------------
// Wire types — keep in sync with internal/api/filings.go::FilingItem
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct FilingsResponse {
    #[serde(default)]
    filings: Vec<RawFiling>,
    #[serde(default)]
    fetched_at: i64,
}

#[derive(Debug, Deserialize)]
struct RawFiling {
    #[serde(default)]
    ticker: String,
    #[serde(default)]
    form_type: String,
    #[serde(default)]
    filing_date: String,
    #[serde(default)]
    filing_date_ms: i64,
    #[serde(default)]
    url: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    accession_number: String,
}

async fn fetch(
    client: &reqwest::Client,
    cfg: &CentoflowConfig,
    tickers: &[SharedString],
) -> anyhow::Result<(Vec<Filing>, DateTime<Utc>)> {
    let tickers_csv = tickers
        .iter()
        .map(|s| s.as_ref())
        .collect::<Vec<_>>()
        .join(",");
    let url = format!("{}/v1/filings?tickers={}", cfg.base_url, tickers_csv);
    let mut req = client.get(&url);
    if let Some(token) = &cfg.token {
        req = req.bearer_auth(token);
    }
    let resp = req.timeout(Duration::from_secs(25)).send().await?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("centoflow /v1/filings returned HTTP {status}: {body}");
    }
    let parsed: FilingsResponse = resp.json().await?;
    let fetched_at = Utc
        .timestamp_millis_opt(parsed.fetched_at)
        .single()
        .unwrap_or_else(Utc::now);
    let filings = parsed
        .filings
        .into_iter()
        .map(|r| {
            let filed_at = Utc
                .timestamp_millis_opt(r.filing_date_ms)
                .single()
                .unwrap_or_else(Utc::now);
            Filing {
                ticker: r.ticker,
                form_type: r.form_type,
                filing_date: r.filing_date,
                filed_at,
                url: r.url,
                title: r.title,
                accession_number: r.accession_number,
            }
        })
        .collect();
    Ok((filings, fetched_at))
}
