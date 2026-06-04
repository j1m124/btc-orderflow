//! Tradable-universe service. Fetches `GET /v1/symbols` from the centoflow
//! server once at startup (retrying with backoff until it succeeds) and holds
//! the list as a `Global`. The chart panel reads it to populate its symbol
//! selector and to resolve a ticker's display name/exchange, and subscribes to
//! [`SymbolsEvent::Loaded`] to re-render when the list arrives.

use std::time::Duration;

use gpui::{App, AppContext as _, Context, Entity, EventEmitter, Global, SharedString, Task};
use serde::Deserialize;

use crate::net::{CentoflowConfig, HttpClient};

/// One tradable symbol's display metadata.
#[derive(Clone, Debug)]
pub struct SymbolInfo {
    pub ticker: SharedString,
    pub name: SharedString,
    pub exchange: SharedString,
    /// Coarse asset class for the symbol picker's filter tabs. The server
    /// doesn't return this yet — see [`asset_class_from_server`]. Defaults to
    /// [`AssetClass::Stocks`] for the current S&P 100 universe.
    pub asset_class: AssetClass,
}

/// TradingView-style asset class buckets shown as filter tabs in the symbol
/// picker. Today every symbol is [`AssetClass::Stocks`]; the other variants
/// exist so the picker UI matches TradingView and lights up automatically when
/// the server starts returning non-stock symbols.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AssetClass {
    Stocks,
    Funds,
    Futures,
    Forex,
    Crypto,
    Indices,
    Bonds,
}

impl AssetClass {
    pub const ALL: &'static [AssetClass] = &[
        AssetClass::Stocks,
        AssetClass::Funds,
        AssetClass::Futures,
        AssetClass::Forex,
        AssetClass::Crypto,
        AssetClass::Indices,
        AssetClass::Bonds,
    ];

    pub fn display(self) -> &'static str {
        match self {
            AssetClass::Stocks => "Stocks",
            AssetClass::Funds => "Funds",
            AssetClass::Futures => "Futures",
            AssetClass::Forex => "Forex",
            AssetClass::Crypto => "Crypto",
            AssetClass::Indices => "Indices",
            AssetClass::Bonds => "Bonds",
        }
    }

    /// Wire id — matches the string the server is expected to emit when the
    /// column lands. Stable so future server payloads don't need a translation
    /// table on the client.
    pub fn wire_id(self) -> &'static str {
        match self {
            AssetClass::Stocks => "stocks",
            AssetClass::Funds => "funds",
            AssetClass::Futures => "futures",
            AssetClass::Forex => "forex",
            AssetClass::Crypto => "crypto",
            AssetClass::Indices => "indices",
            AssetClass::Bonds => "bonds",
        }
    }

    fn from_wire(s: &str) -> Option<AssetClass> {
        Self::ALL.iter().copied().find(|a| a.wire_id() == s)
    }
}

impl Default for AssetClass {
    fn default() -> Self {
        AssetClass::Stocks
    }
}

#[derive(Clone, Debug)]
pub enum SymbolsEvent {
    /// The universe was (re)loaded from the server.
    Loaded,
}

pub struct SymbolsService {
    symbols: Vec<SymbolInfo>,
    _task: Task<()>,
}

impl EventEmitter<SymbolsEvent> for SymbolsService {}

impl SymbolsService {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let client = cx.global::<HttpClient>().0.clone();
        let task = cx.spawn(async move |this, cx| {
            run_fetch(this, cx, client).await;
        });
        Self {
            symbols: Vec::new(),
            _task: task,
        }
    }

    /// The loaded universe (empty until the first successful fetch).
    pub fn symbols(&self) -> &[SymbolInfo] {
        &self.symbols
    }

    /// Display `(name, exchange)` for a ticker, if known.
    pub fn meta(&self, ticker: &str) -> Option<(SharedString, SharedString)> {
        self.symbols
            .iter()
            .find(|s| s.ticker.as_ref() == ticker)
            .map(|s| (s.name.clone(), s.exchange.clone()))
    }

    /// First symbol in the universe, used as the default chart symbol once
    /// loaded. `None` until the fetch succeeds.
    pub fn default_symbol(&self) -> Option<SharedString> {
        self.symbols.first().map(|s| s.ticker.clone())
    }

    /// Re-fetch the universe (e.g. after the auth token changes). Dropping the
    /// old `_task` cancels any in-flight retry loop.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        let client = cx.global::<HttpClient>().0.clone();
        self._task = cx.spawn(async move |this, cx| {
            run_fetch(this, cx, client).await;
        });
    }

    fn set(&mut self, symbols: Vec<SymbolInfo>, cx: &mut Context<Self>) {
        self.symbols = symbols;
        cx.emit(SymbolsEvent::Loaded);
        cx.notify();
    }
}

#[derive(Clone)]
pub struct SymbolsServiceHandle(pub Entity<SymbolsService>);
impl Global for SymbolsServiceHandle {}

pub fn init(cx: &mut App) {
    let entity = cx.new(SymbolsService::new);
    cx.set_global(SymbolsServiceHandle(entity));
}

async fn run_fetch(
    this: gpui::WeakEntity<SymbolsService>,
    cx: &mut gpui::AsyncApp,
    client: reqwest::Client,
) {
    let mut attempts: u32 = 0;
    loop {
        let Ok(cfg) = this.update(cx, |_s, cx| cx.global::<CentoflowConfig>().clone()) else {
            return;
        };
        match fetch_symbols(&client, &cfg).await {
            Ok(symbols) => {
                let _ = this.update(cx, |s, cx| s.set(symbols, cx));
                return; // one successful load is enough
            }
            Err(e) => {
                log::warn!("centoflow /v1/symbols: {e:#}");
                attempts = attempts.saturating_add(1);
                let shift = attempts.saturating_sub(1).min(5);
                let secs = (1u64 << shift).min(30);
                cx.background_executor()
                    .timer(Duration::from_secs(secs))
                    .await;
            }
        }
    }
}

#[derive(Debug, Deserialize)]
struct SymbolsResponse {
    #[serde(default)]
    symbols: Vec<RawSymbol>,
}

#[derive(Debug, Deserialize)]
struct RawSymbol {
    ticker: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    exchange: String,
    /// Optional — server doesn't emit this today. Future-proof.
    #[serde(default)]
    asset_class: Option<String>,
}

async fn fetch_symbols(
    client: &reqwest::Client,
    cfg: &CentoflowConfig,
) -> anyhow::Result<Vec<SymbolInfo>> {
    let url = format!("{}/v1/symbols", cfg.base_url);
    let mut req = client.get(&url);
    if let Some(token) = &cfg.token {
        req = req.bearer_auth(token);
    }
    let resp = req.send().await?;
    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!("centoflow /v1/symbols returned HTTP {status}");
    }
    let parsed: SymbolsResponse = resp.json().await?;
    Ok(parsed
        .symbols
        .into_iter()
        .map(|s| SymbolInfo {
            ticker: s.ticker.into(),
            name: s.name.into(),
            exchange: s.exchange.into(),
            asset_class: s
                .asset_class
                .as_deref()
                .and_then(AssetClass::from_wire)
                .unwrap_or_default(),
        })
        .collect())
}
