//! News feed for the user's watchlist. Fetches `GET /v1/news?tickers=…`
//! from centoflow-server, which fans out per-ticker to Massive and returns
//! merged newest-first.
//!
//! Auto-refresh is opt-in via [`InsiderService`-style] `reload()` calls.
//! Panels start a 60s `cx.spawn` loop on mount that nudges the service; the
//! loop's Task is held on the panel so dropping the panel cancels it. The
//! service itself does NOT spawn a self-perpetuating timer because the
//! service is long-lived (singleton via Global) and we don't want stray
//! background fetches when no News panel is open.

use std::time::Duration;

use chrono::{DateTime, TimeZone as _, Utc};
use gpui::{App, AppContext as _, Context, Entity, EventEmitter, Global, SharedString, Task};
use serde::Deserialize;

use crate::net::{CentoflowConfig, HttpClient};
use crate::services::watchlist::{WatchlistEvent, WatchlistServiceHandle};

#[derive(Clone, Debug)]
pub struct NewsArticle {
    pub id: String,
    pub title: String,
    pub author: String,
    pub published_at: Option<DateTime<Utc>>,
    pub article_url: String,
    pub image_url: String,
    pub description: String,
    pub tickers: Vec<String>,
}

#[derive(Clone, Debug)]
pub enum NewsState {
    Idle,
    Loading,
    Loaded {
        articles: Vec<NewsArticle>,
        fetched_at: DateTime<Utc>,
    },
    Error {
        message: String,
        last: Option<(Vec<NewsArticle>, DateTime<Utc>)>,
    },
}

#[derive(Clone, Debug)]
pub enum NewsServiceEvent {
    Changed,
}

pub struct NewsService {
    state: NewsState,
    inflight: Option<Task<()>>,
    _watchlist_sub: gpui::Subscription,
}

impl EventEmitter<NewsServiceEvent> for NewsService {}

impl NewsService {
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
            state: NewsState::Idle,
            inflight: None,
            _watchlist_sub: sub,
        }
    }

    pub fn state(&self) -> &NewsState {
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
            NewsState::Loaded { articles, fetched_at } => Some((articles.clone(), *fetched_at)),
            NewsState::Error { last, .. } => last.clone(),
            _ => None,
        };

        self.state = NewsState::Loading;
        cx.emit(NewsServiceEvent::Changed);
        cx.notify();

        let task = cx.spawn(async move |this, cx| {
            let result = fetch(&client, &cfg, &tickers).await;
            let _ = this.update(cx, |s, cx| {
                s.inflight = None;
                match result {
                    Ok((articles, fetched_at)) => {
                        s.state = NewsState::Loaded { articles, fetched_at };
                    }
                    Err(err) => {
                        s.state = NewsState::Error {
                            message: format!("{err:#}"),
                            last: last.clone(),
                        };
                    }
                }
                cx.emit(NewsServiceEvent::Changed);
                cx.notify();
            });
        });
        self.inflight = Some(task);
    }
}

#[derive(Clone)]
pub struct NewsServiceHandle(pub Entity<NewsService>);
impl Global for NewsServiceHandle {}

pub fn init(cx: &mut App) {
    let entity = cx.new(NewsService::new);
    cx.set_global(NewsServiceHandle(entity));
}

// ---------------------------------------------------------------------------
// Wire types — keep in sync with internal/api/resources.go::NewsArticleItem
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct NewsResponse {
    #[serde(default)]
    articles: Vec<RawArticle>,
    #[serde(default)]
    fetched_at: i64,
}

#[derive(Debug, Deserialize)]
struct RawArticle {
    #[serde(default)]
    id: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    author: String,
    #[serde(default)]
    published_ms: i64,
    #[serde(default)]
    article_url: String,
    #[serde(default)]
    image_url: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    tickers: Vec<String>,
}

async fn fetch(
    client: &reqwest::Client,
    cfg: &CentoflowConfig,
    tickers: &[SharedString],
) -> anyhow::Result<(Vec<NewsArticle>, DateTime<Utc>)> {
    let tickers_csv = tickers
        .iter()
        .map(|s| s.as_ref())
        .collect::<Vec<_>>()
        .join(",");
    let url = format!("{}/v1/news?tickers={}", cfg.base_url, tickers_csv);
    let mut req = client.get(&url);
    if let Some(token) = &cfg.token {
        req = req.bearer_auth(token);
    }
    let resp = req.timeout(Duration::from_secs(25)).send().await?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("centoflow /v1/news returned HTTP {status}: {body}");
    }
    let parsed: NewsResponse = resp.json().await?;
    let fetched_at = Utc
        .timestamp_millis_opt(parsed.fetched_at)
        .single()
        .unwrap_or_else(Utc::now);
    let articles = parsed
        .articles
        .into_iter()
        .map(|r| {
            let published_at = if r.published_ms > 0 {
                Utc.timestamp_millis_opt(r.published_ms).single()
            } else {
                None
            };
            NewsArticle {
                id: r.id,
                title: r.title,
                author: r.author,
                published_at,
                article_url: r.article_url,
                image_url: r.image_url,
                description: r.description,
                tickers: r.tickers,
            }
        })
        .collect();
    Ok((articles, fetched_at))
}
