//! Per-ticker Details aggregator.
//!
//! Combines three independent endpoints — `GET /v1/overview`,
//! `GET /v1/financials`, `GET /v1/dividends` — into a single per-symbol
//! state bundle. The Details panel reads from this service and renders.
//!
//! ## Symbol tracking
//!
//! The "currently displayed symbol" follows the user's focused chart.
//! `ContentPanel` (chart kind) calls [`DetailsService::set_focused_symbol`]
//! whenever its `set_active(true)` / mouse-down focus handler fires, and
//! whenever `switch_chart_symbol` runs. When the focused chart is removed,
//! callers pass `None` to reset to the empty state.
//!
//! ## Cache
//!
//! Per-symbol entries are kept in an LRU of size [`MAX_CACHED_SYMBOLS`].
//! Flipping back-and-forth between AAPL / MSFT / NVDA stays instant; older
//! tickers fall out and refetch on demand.
//!
//! Each section (overview / financials / dividends) is fetched
//! independently so a slow endpoint doesn't block the others.

use std::collections::VecDeque;
use std::time::Duration;

use chrono::{DateTime, TimeZone as _, Utc};
use gpui::{App, AppContext as _, Context, Entity, EventEmitter, Global, SharedString, Task};
use serde::Deserialize;
use serde_json::Value as JsonValue;

use crate::net::{CentoflowConfig, HttpClient};

/// LRU bound for cached `(overview, financials, dividends)` triples. Memory
/// is modest (a few KB per symbol); the cap is mostly to keep the workspace
/// honest if a user pages through many tickers.
const MAX_CACHED_SYMBOLS: usize = 4;

// ---------------------------------------------------------------------------
// Typed records
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct Overview {
    pub ticker: String,
    pub name: String,
    pub exchange: String,
    pub description: String,
    pub sector: String,
    pub industry: String,
    pub market_cap: Option<f64>,
    pub cik: String,
    /// Vendor's raw payload (Massive `ticker overview`). Kept so panels can
    /// surface vendor-specific fields without an extra wire roundtrip.
    pub raw: Option<JsonValue>,
    pub ingested_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug)]
pub struct FinancialPeriod {
    pub period_end: String, // "YYYY-MM-DD"
    pub period_end_at: Option<DateTime<Utc>>,
    pub period_type: String,
    pub fiscal_year: Option<i32>,
    pub fiscal_period: String,
    /// Raw vendor blobs. Curated headlines + "show full statement" tree
    /// view both read from these.
    pub balance: Option<JsonValue>,
    pub income: Option<JsonValue>,
    pub cashflow: Option<JsonValue>,
    pub ratios: Option<JsonValue>,
}

#[derive(Clone, Debug)]
pub struct Dividend {
    pub ex_dividend_date: String, // "YYYY-MM-DD"
    pub ex_dividend_at: Option<DateTime<Utc>>,
    pub declaration_date: String,
    pub record_date: String,
    pub pay_date: String,
    pub cash_amount: f64,
    pub currency: String,
    pub frequency: Option<i32>,
    pub dividend_type: String,
}

// ---------------------------------------------------------------------------
// Section-level state machines
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default)]
pub enum SectionState<T: Clone> {
    #[default]
    Idle,
    Loading,
    Loaded(T),
    Error {
        message: String,
        last: Option<T>,
    },
}

impl<T: Clone> SectionState<T> {
    pub fn is_loading(&self) -> bool {
        matches!(self, SectionState::Loading)
    }
    pub fn loaded(&self) -> Option<&T> {
        match self {
            SectionState::Loaded(t) => Some(t),
            SectionState::Error { last: Some(t), .. } => Some(t),
            _ => None,
        }
    }
    pub fn error(&self) -> Option<&str> {
        match self {
            SectionState::Error { message, .. } => Some(message.as_str()),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct DetailsEntry {
    pub overview: SectionState<Overview>,
    pub financials: SectionState<Vec<FinancialPeriod>>,
    pub dividends: SectionState<Vec<Dividend>>,
}

// ---------------------------------------------------------------------------
// Service
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub enum DetailsEvent {
    /// Focused symbol changed (or cleared). Panels re-evaluate which entry
    /// to render.
    FocusedChanged,
    /// Data for `symbol` was updated (a section's fetch resolved). Panels
    /// re-render if they're currently showing this symbol.
    DataChanged { symbol: SharedString },
}

pub struct DetailsService {
    /// Currently focused chart's symbol, or `None` when no chart is focused.
    focused: Option<SharedString>,
    /// LRU keyed by symbol. The back is the most-recently-used entry; the
    /// front is the eviction candidate.
    cache: VecDeque<(SharedString, DetailsEntry)>,
    /// In-flight section fetches. Per-(symbol, section) so the three
    /// sections never compete for the same slot. Kept as a flat Vec instead
    /// of a HashMap because cardinality is tiny (≤ 3 sections × MAX_CACHED
    /// symbols).
    inflight: Vec<((SharedString, Section), Task<()>)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Section {
    Overview,
    Financials,
    Dividends,
}

impl EventEmitter<DetailsEvent> for DetailsService {}

impl DetailsService {
    pub fn new(_cx: &mut Context<Self>) -> Self {
        Self {
            focused: None,
            cache: VecDeque::with_capacity(MAX_CACHED_SYMBOLS),
            inflight: Vec::new(),
        }
    }

    pub fn focused_symbol(&self) -> Option<&SharedString> {
        self.focused.as_ref()
    }

    /// Set (or clear) the focused symbol. Idempotent for the same value.
    /// Touches the LRU and triggers any missing fetches for the new symbol.
    pub fn set_focused_symbol(
        &mut self,
        symbol: Option<SharedString>,
        cx: &mut Context<Self>,
    ) {
        if self.focused == symbol {
            return;
        }
        self.focused = symbol.clone();
        if let Some(s) = symbol.as_ref() {
            self.ensure(s.clone(), cx);
        }
        cx.emit(DetailsEvent::FocusedChanged);
        cx.notify();
    }

    /// Look up the entry for `symbol`. Returns `None` if it isn't cached
    /// yet — callers should call [`Self::ensure`] to start fetches.
    pub fn entry(&self, symbol: &str) -> Option<&DetailsEntry> {
        self.cache
            .iter()
            .find(|(k, _)| k.as_ref() == symbol)
            .map(|(_, v)| v)
    }

    /// Make sure `symbol` is in the cache and its three sections are
    /// being fetched (or have already been fetched). Touches LRU recency.
    pub fn ensure(&mut self, symbol: SharedString, cx: &mut Context<Self>) {
        self.touch(symbol.clone());
        // After `touch`, the entry is guaranteed to be at the back.
        // Borrow checker: read what we need from the entry, then mutate
        // self via spawn_fetch below.
        let (need_overview, need_financials, need_dividends) = {
            let entry = &self
                .cache
                .back()
                .expect("touch() always pushes/keeps an entry")
                .1;
            (
                matches!(entry.overview, SectionState::Idle | SectionState::Error { .. }),
                matches!(entry.financials, SectionState::Idle | SectionState::Error { .. }),
                matches!(entry.dividends, SectionState::Idle | SectionState::Error { .. }),
            )
        };
        if need_overview {
            self.spawn_fetch(symbol.clone(), Section::Overview, cx);
        }
        if need_financials {
            self.spawn_fetch(symbol.clone(), Section::Financials, cx);
        }
        if need_dividends {
            self.spawn_fetch(symbol, Section::Dividends, cx);
        }
    }

    /// Force-refresh all three sections for `symbol`. Used by the Details
    /// panel's manual refresh button.
    pub fn reload(&mut self, symbol: SharedString, cx: &mut Context<Self>) {
        self.touch(symbol.clone());
        // Reset to Idle so spawn_fetch will pick them up.
        if let Some((_, e)) = self
            .cache
            .iter_mut()
            .find(|(k, _)| k.as_ref() == symbol.as_ref())
        {
            e.overview = match std::mem::take(&mut e.overview) {
                SectionState::Loaded(t) => SectionState::Error {
                    message: String::new(),
                    last: Some(t),
                },
                other => other,
            };
            e.financials = match std::mem::take(&mut e.financials) {
                SectionState::Loaded(t) => SectionState::Error {
                    message: String::new(),
                    last: Some(t),
                },
                other => other,
            };
            e.dividends = match std::mem::take(&mut e.dividends) {
                SectionState::Loaded(t) => SectionState::Error {
                    message: String::new(),
                    last: Some(t),
                },
                other => other,
            };
        }
        self.spawn_fetch(symbol.clone(), Section::Overview, cx);
        self.spawn_fetch(symbol.clone(), Section::Financials, cx);
        self.spawn_fetch(symbol, Section::Dividends, cx);
    }

    // ----- private helpers -----

    /// LRU bookkeeping. Inserts a fresh entry if missing; otherwise moves
    /// the existing entry to the back. Evicts the front when over capacity.
    fn touch(&mut self, symbol: SharedString) {
        if let Some(pos) = self
            .cache
            .iter()
            .position(|(k, _)| k.as_ref() == symbol.as_ref())
        {
            let item = self.cache.remove(pos).expect("position is in-bounds");
            self.cache.push_back(item);
            return;
        }
        if self.cache.len() >= MAX_CACHED_SYMBOLS {
            // Drop the LRU entry. Any inflight tasks for it stay running
            // but their `update` closures will silently no-op when they
            // can't find the entry.
            let _ = self.cache.pop_front();
        }
        self.cache
            .push_back((symbol, DetailsEntry::default()));
    }

    fn spawn_fetch(
        &mut self,
        symbol: SharedString,
        section: Section,
        cx: &mut Context<Self>,
    ) {
        let key = (symbol.clone(), section);
        // If a fetch is already in flight for this exact (symbol, section),
        // skip — let it finish.
        if self.inflight.iter().any(|(k, _)| k == &key) {
            return;
        }
        // Move section to Loading so the panel can show a spinner.
        self.set_section_loading(&symbol, section);

        let client = cx.global::<HttpClient>().0.clone();
        let cfg = cx.global::<CentoflowConfig>().clone();
        let task = match section {
            Section::Overview => cx.spawn({
                let symbol = symbol.clone();
                async move |this, cx| {
                    let result = fetch_overview(&client, &cfg, symbol.as_ref()).await;
                    let _ = this.update(cx, |s, cx| {
                        s.finish_overview(symbol.clone(), result, cx);
                    });
                }
            }),
            Section::Financials => cx.spawn({
                let symbol = symbol.clone();
                async move |this, cx| {
                    let result = fetch_financials(&client, &cfg, symbol.as_ref()).await;
                    let _ = this.update(cx, |s, cx| {
                        s.finish_financials(symbol.clone(), result, cx);
                    });
                }
            }),
            Section::Dividends => cx.spawn({
                let symbol = symbol.clone();
                async move |this, cx| {
                    let result = fetch_dividends(&client, &cfg, symbol.as_ref()).await;
                    let _ = this.update(cx, |s, cx| {
                        s.finish_dividends(symbol.clone(), result, cx);
                    });
                }
            }),
        };
        self.inflight.push((key, task));
    }

    fn finish_overview(
        &mut self,
        symbol: SharedString,
        result: anyhow::Result<Overview>,
        cx: &mut Context<Self>,
    ) {
        self.inflight
            .retain(|(k, _)| k != &(symbol.clone(), Section::Overview));
        if let Some((_, entry)) = self
            .cache
            .iter_mut()
            .find(|(k, _)| k.as_ref() == symbol.as_ref())
        {
            let prior = entry.overview.loaded().cloned();
            entry.overview = match result {
                Ok(t) => SectionState::Loaded(t),
                Err(e) => SectionState::Error {
                    message: format!("{e:#}"),
                    last: prior,
                },
            };
        }
        cx.emit(DetailsEvent::DataChanged { symbol });
        cx.notify();
    }

    fn finish_financials(
        &mut self,
        symbol: SharedString,
        result: anyhow::Result<Vec<FinancialPeriod>>,
        cx: &mut Context<Self>,
    ) {
        self.inflight
            .retain(|(k, _)| k != &(symbol.clone(), Section::Financials));
        if let Some((_, entry)) = self
            .cache
            .iter_mut()
            .find(|(k, _)| k.as_ref() == symbol.as_ref())
        {
            let prior = entry.financials.loaded().cloned();
            entry.financials = match result {
                Ok(t) => SectionState::Loaded(t),
                Err(e) => SectionState::Error {
                    message: format!("{e:#}"),
                    last: prior,
                },
            };
        }
        cx.emit(DetailsEvent::DataChanged { symbol });
        cx.notify();
    }

    fn finish_dividends(
        &mut self,
        symbol: SharedString,
        result: anyhow::Result<Vec<Dividend>>,
        cx: &mut Context<Self>,
    ) {
        self.inflight
            .retain(|(k, _)| k != &(symbol.clone(), Section::Dividends));
        if let Some((_, entry)) = self
            .cache
            .iter_mut()
            .find(|(k, _)| k.as_ref() == symbol.as_ref())
        {
            let prior = entry.dividends.loaded().cloned();
            entry.dividends = match result {
                Ok(t) => SectionState::Loaded(t),
                Err(e) => SectionState::Error {
                    message: format!("{e:#}"),
                    last: prior,
                },
            };
        }
        cx.emit(DetailsEvent::DataChanged { symbol });
        cx.notify();
    }

    fn set_section_loading(&mut self, symbol: &SharedString, section: Section) {
        if let Some((_, entry)) = self
            .cache
            .iter_mut()
            .find(|(k, _)| k.as_ref() == symbol.as_ref())
        {
            match section {
                Section::Overview => entry.overview = SectionState::Loading,
                Section::Financials => entry.financials = SectionState::Loading,
                Section::Dividends => entry.dividends = SectionState::Loading,
            }
        }
    }
}

#[derive(Clone)]
pub struct DetailsServiceHandle(pub Entity<DetailsService>);
impl Global for DetailsServiceHandle {}

pub fn init(cx: &mut App) {
    let entity = cx.new(DetailsService::new);
    cx.set_global(DetailsServiceHandle(entity));
}

// ---------------------------------------------------------------------------
// Wire — overview
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct OverviewResp {
    #[serde(default)]
    ticker: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    exchange: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    sector: String,
    #[serde(default)]
    industry: String,
    #[serde(default)]
    market_cap: Option<f64>,
    #[serde(default)]
    cik: String,
    #[serde(default)]
    raw: Option<JsonValue>,
    #[serde(default)]
    ingested_at: i64,
}

async fn fetch_overview(
    client: &reqwest::Client,
    cfg: &CentoflowConfig,
    ticker: &str,
) -> anyhow::Result<Overview> {
    let url = format!("{}/v1/overview?ticker={}", cfg.base_url, ticker);
    let mut req = client.get(&url);
    if let Some(token) = &cfg.token {
        req = req.bearer_auth(token);
    }
    let resp = req.timeout(Duration::from_secs(20)).send().await?;
    let status = resp.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        anyhow::bail!("{ticker} is not in the tracked universe");
    }
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("centoflow /v1/overview returned HTTP {status}: {body}");
    }
    let parsed: OverviewResp = resp.json().await?;
    let ingested_at = if parsed.ingested_at > 0 {
        Utc.timestamp_millis_opt(parsed.ingested_at).single()
    } else {
        None
    };
    Ok(Overview {
        ticker: parsed.ticker,
        name: parsed.name,
        exchange: parsed.exchange,
        description: parsed.description,
        sector: parsed.sector,
        industry: parsed.industry,
        market_cap: parsed.market_cap,
        cik: parsed.cik,
        raw: parsed.raw,
        ingested_at,
    })
}

// ---------------------------------------------------------------------------
// Wire — financials
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct FinancialsResp {
    #[serde(default)]
    periods: Vec<RawPeriod>,
}

#[derive(Debug, Deserialize)]
struct RawPeriod {
    #[serde(default)]
    period_end: String,
    #[serde(default)]
    period_end_ms: i64,
    #[serde(default)]
    period_type: String,
    #[serde(default)]
    fiscal_year: Option<i32>,
    #[serde(default)]
    fiscal_period: String,
    #[serde(default)]
    balance: Option<JsonValue>,
    #[serde(default)]
    income: Option<JsonValue>,
    #[serde(default)]
    cashflow: Option<JsonValue>,
    #[serde(default)]
    ratios: Option<JsonValue>,
}

async fn fetch_financials(
    client: &reqwest::Client,
    cfg: &CentoflowConfig,
    ticker: &str,
) -> anyhow::Result<Vec<FinancialPeriod>> {
    let url = format!("{}/v1/financials?ticker={}", cfg.base_url, ticker);
    let mut req = client.get(&url);
    if let Some(token) = &cfg.token {
        req = req.bearer_auth(token);
    }
    let resp = req.timeout(Duration::from_secs(25)).send().await?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("centoflow /v1/financials returned HTTP {status}: {body}");
    }
    let parsed: FinancialsResp = resp.json().await?;
    let periods = parsed
        .periods
        .into_iter()
        .map(|r| {
            let period_end_at = if r.period_end_ms > 0 {
                Utc.timestamp_millis_opt(r.period_end_ms).single()
            } else {
                None
            };
            FinancialPeriod {
                period_end: r.period_end,
                period_end_at,
                period_type: r.period_type,
                fiscal_year: r.fiscal_year,
                fiscal_period: r.fiscal_period,
                balance: r.balance,
                income: r.income,
                cashflow: r.cashflow,
                ratios: r.ratios,
            }
        })
        .collect();
    Ok(periods)
}

// ---------------------------------------------------------------------------
// Wire — dividends
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct DividendsResp {
    #[serde(default)]
    dividends: Vec<RawDividend>,
}

#[derive(Debug, Deserialize)]
struct RawDividend {
    #[serde(default)]
    ex_dividend_date: String,
    #[serde(default)]
    ex_dividend_ms: i64,
    #[serde(default)]
    declaration_date: String,
    #[serde(default)]
    record_date: String,
    #[serde(default)]
    pay_date: String,
    #[serde(default)]
    cash_amount: f64,
    #[serde(default)]
    currency: String,
    #[serde(default)]
    frequency: Option<i32>,
    #[serde(default)]
    dividend_type: String,
}

async fn fetch_dividends(
    client: &reqwest::Client,
    cfg: &CentoflowConfig,
    ticker: &str,
) -> anyhow::Result<Vec<Dividend>> {
    let url = format!("{}/v1/dividends?ticker={}", cfg.base_url, ticker);
    let mut req = client.get(&url);
    if let Some(token) = &cfg.token {
        req = req.bearer_auth(token);
    }
    let resp = req.timeout(Duration::from_secs(20)).send().await?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("centoflow /v1/dividends returned HTTP {status}: {body}");
    }
    let parsed: DividendsResp = resp.json().await?;
    let divs = parsed
        .dividends
        .into_iter()
        .map(|r| {
            let ex_dividend_at = if r.ex_dividend_ms > 0 {
                Utc.timestamp_millis_opt(r.ex_dividend_ms).single()
            } else {
                None
            };
            Dividend {
                ex_dividend_date: r.ex_dividend_date,
                ex_dividend_at,
                declaration_date: r.declaration_date,
                record_date: r.record_date,
                pay_date: r.pay_date,
                cash_amount: r.cash_amount,
                currency: r.currency,
                frequency: r.frequency,
                dividend_type: r.dividend_type,
            }
        })
        .collect();
    Ok(divs)
}
