//! Tradable-universe service.
//!
//! The v1 universe is a tiny hardcoded list (BTCUSDT-perp on Binance). The
//! plan is to grow it through this service as we add more symbols / venues.

use gpui::{App, AppContext as _, Context, Entity, EventEmitter, Global, SharedString};

#[derive(Clone, Debug)]
pub struct SymbolInfo {
    /// Native exchange ticker (e.g. `BTCUSDT`). Used as the wire identifier
    /// in WS subscribe frames and as the lookup key across the codebase.
    pub ticker: SharedString,
    /// Cross-exchange normalized label rendered in panel headers and
    /// anywhere else the user identifies the instrument. Format is
    /// `base/quote@venuekind` (e.g. `btc/usd@binancef`) where `venuekind`
    /// is `<exchange><instrument-marker>` — `f` for perp/futures, omitted
    /// for spot. Lets the same instrument from different venues sit side-
    /// by-side with consistent naming.
    pub normalized: SharedString,
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

    /// Cross-exchange normalized label (e.g. `btc/usd@binancef`). Returns
    /// `None` if `ticker` is not in the universe; callers should fall back
    /// to the raw ticker string in that case.
    pub fn normalized(&self, ticker: &str) -> Option<SharedString> {
        self.symbols
            .iter()
            .find(|s| s.ticker.as_ref() == ticker)
            .map(|s| s.normalized.clone())
    }

    /// Like [`Self::normalized`], but falls back to a lowercase copy of
    /// `ticker` when the symbol is unknown — handy for panel headers
    /// where rendering the bare ticker is preferable to rendering nothing.
    pub fn normalized_or_lower(&self, ticker: &str) -> SharedString {
        self.normalized(ticker)
            .unwrap_or_else(|| SharedString::from(ticker.to_lowercase()))
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
        normalized: "btc/usd@binancef".into(),
        name: "Bitcoin / Tether".into(),
        exchange: "BINANCE".into(),
        instrument: InstrumentType::Perp,
    }]
}
