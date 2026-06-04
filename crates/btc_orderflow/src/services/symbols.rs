//! Tradable-universe service (stub).
//!
//! The original implementation fetched `GET /v1/symbols` from a centoflow
//! server and held the result as a `Global`. The fork starts with a tiny
//! hardcoded universe containing only BTC; a real BTC backend / multi-venue
//! universe loader will replace this.

use gpui::{App, AppContext as _, Context, Entity, EventEmitter, Global, SharedString};

#[derive(Clone, Debug)]
pub struct SymbolInfo {
    pub ticker: SharedString,
    pub name: SharedString,
    pub exchange: SharedString,
    pub asset_class: AssetClass,
}

/// Asset-class filter tabs in the symbol picker. Today the stub universe is
/// crypto-only; the other variants remain so the picker UI is the same as in
/// the source repo and will light up automatically when the universe grows.
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
}

impl Default for AssetClass {
    fn default() -> Self {
        AssetClass::Crypto
    }
}

#[derive(Clone, Debug)]
pub enum SymbolsEvent {
    Loaded,
}

pub struct SymbolsService {
    symbols: Vec<SymbolInfo>,
}

impl EventEmitter<SymbolsEvent> for SymbolsService {}

impl SymbolsService {
    pub fn new(_cx: &mut Context<Self>) -> Self {
        // Subscribers read `symbols()` directly; no Loaded event needed for
        // a hardcoded universe.
        Self {
            symbols: hardcoded_universe(),
        }
    }

    pub fn symbols(&self) -> &[SymbolInfo] {
        &self.symbols
    }

    pub fn meta(&self, ticker: &str) -> Option<(SharedString, SharedString)> {
        self.symbols
            .iter()
            .find(|s| s.ticker.as_ref() == ticker)
            .map(|s| (s.name.clone(), s.exchange.clone()))
    }

    pub fn default_symbol(&self) -> Option<SharedString> {
        self.symbols.first().map(|s| s.ticker.clone())
    }

    pub fn reload(&mut self, _cx: &mut Context<Self>) {}
}

#[derive(Clone)]
pub struct SymbolsServiceHandle(pub Entity<SymbolsService>);
impl Global for SymbolsServiceHandle {}

pub fn init(cx: &mut App) {
    let entity = cx.new(SymbolsService::new);
    cx.set_global(SymbolsServiceHandle(entity));
}

fn hardcoded_universe() -> Vec<SymbolInfo> {
    vec![SymbolInfo {
        ticker: "BTCUSDT".into(),
        name: "Bitcoin / Tether".into(),
        exchange: "BINANCE".into(),
        asset_class: AssetClass::Crypto,
    }]
}
