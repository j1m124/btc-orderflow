//! Tradable-universe service.
//!
//! The v1 universe is a tiny hardcoded list (BTCUSDT-perp on Binance). The
//! plan is to grow it through this service as we add more symbols / venues.

use gpui::{App, AppContext as _, Context, Entity, EventEmitter, Global, SharedString};

#[derive(Clone, Debug)]
pub struct SymbolInfo {
    pub ticker: SharedString,
    pub name: SharedString,
    pub exchange: SharedString,
    pub instrument: InstrumentType,
}

/// Instrument category drives the symbol picker's filter tabs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum InstrumentType {
    Spot,
    Perp,
    Futures,
}

impl InstrumentType {
    pub const ALL: &'static [InstrumentType] = &[
        InstrumentType::Spot,
        InstrumentType::Perp,
        InstrumentType::Futures,
    ];

    pub fn display(self) -> &'static str {
        match self {
            InstrumentType::Spot => "Spot",
            InstrumentType::Perp => "Perp",
            InstrumentType::Futures => "Futures",
        }
    }

    pub fn wire_id(self) -> &'static str {
        match self {
            InstrumentType::Spot => "spot",
            InstrumentType::Perp => "perp",
            InstrumentType::Futures => "futures",
        }
    }
}

impl Default for InstrumentType {
    fn default() -> Self {
        InstrumentType::Perp
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
        instrument: InstrumentType::Perp,
    }]
}
