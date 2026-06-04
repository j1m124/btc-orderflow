//! User-managed watchlist of tickers. Backed by a global entity; the watchlist
//! panel renders from it and the workspace's FocusSymbol dispatch reads it.
//! Persisted to local storage so the user's curated list survives reloads.

use gpui::{App, AppContext as _, Context, Entity, EventEmitter, Global, SharedString};

use crate::persistence;

/// Default popular tickers shown the first time the user opens the app. Kept
/// short and recognisable so the watchlist isn't overwhelming on day one.
pub const DEFAULT_WATCHLIST: &[&str] = &[
    "AAPL", "MSFT", "NVDA", "GOOGL", "TSLA", "META", "AMZN", "BRK.B",
];

#[derive(Clone, Debug)]
pub enum WatchlistEvent {
    Changed,
}

pub struct WatchlistService {
    symbols: Vec<SharedString>,
}

impl EventEmitter<WatchlistEvent> for WatchlistService {}

impl WatchlistService {
    fn new(_cx: &mut Context<Self>) -> Self {
        let symbols = persistence::load_watchlist().unwrap_or_else(|| {
            DEFAULT_WATCHLIST
                .iter()
                .map(|s| SharedString::from(*s))
                .collect()
        });
        Self { symbols }
    }

    pub fn symbols(&self) -> &[SharedString] {
        &self.symbols
    }

    /// Add `ticker` to the watchlist if not already present. Returns true on
    /// insert. Persists immediately on change.
    pub fn add(&mut self, ticker: impl Into<SharedString>, cx: &mut Context<Self>) -> bool {
        let ticker = ticker.into();
        if self.symbols.iter().any(|t| t.as_ref() == ticker.as_ref()) {
            return false;
        }
        self.symbols.push(ticker);
        self.persist();
        cx.emit(WatchlistEvent::Changed);
        cx.notify();
        true
    }

    /// Remove `ticker` from the watchlist. Returns true if it was present.
    pub fn remove(&mut self, ticker: &str, cx: &mut Context<Self>) -> bool {
        let before = self.symbols.len();
        self.symbols.retain(|t| t.as_ref() != ticker);
        if self.symbols.len() == before {
            return false;
        }
        self.persist();
        cx.emit(WatchlistEvent::Changed);
        cx.notify();
        true
    }

    /// Move `source` to the position currently occupied by `target` (insert
    /// before it). If `target` is empty, append to the end. Returns true if
    /// the order actually changed.
    pub fn move_before(
        &mut self,
        source: &str,
        target: Option<&str>,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(src_pos) = self.symbols.iter().position(|t| t.as_ref() == source) else {
            return false;
        };
        let item = self.symbols.remove(src_pos);
        let dst = match target {
            Some(t) => self
                .symbols
                .iter()
                .position(|x| x.as_ref() == t)
                .unwrap_or(self.symbols.len()),
            None => self.symbols.len(),
        };
        if src_pos == dst && self.symbols.get(src_pos).map(SharedString::as_ref) == Some(source) {
            // No-op: caller dropped the row on itself.
            self.symbols.insert(src_pos, item);
            return false;
        }
        self.symbols.insert(dst, item);
        self.persist();
        cx.emit(WatchlistEvent::Changed);
        cx.notify();
        true
    }

    fn persist(&self) {
        if let Err(err) = persistence::save_watchlist(&self.symbols) {
            log::warn!("save watchlist failed: {err:?}");
        }
    }
}

#[derive(Clone)]
pub struct WatchlistServiceHandle(pub Entity<WatchlistService>);
impl Global for WatchlistServiceHandle {}

pub fn init(cx: &mut App) {
    let entity = cx.new(WatchlistService::new);
    cx.set_global(WatchlistServiceHandle(entity));
}
