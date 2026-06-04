//! Recent-symbols list shown at the top of the shared symbol picker.
//!
//! Independent from [`crate::services::watchlist`] — watchlist is a curated
//! monitoring list, recents is a write-through history of every confirm in the
//! picker (chart-switch or add-to-watchlist). Persisted to its own slot
//! (`terminal_demo.recents.v1`) so layout resets don't erase it.

use gpui::{App, AppContext as _, Context, Entity, EventEmitter, Global, SharedString};

use crate::persistence;

/// Hard cap on stored history. The picker renders the top
/// [`Self::DISPLAY_LIMIT`] entries; the rest are kept in case the user clears
/// recents back to a smaller list.
const STORAGE_LIMIT: usize = 20;

#[derive(Clone, Debug)]
pub enum RecentsEvent {
    Changed,
}

pub struct RecentsService {
    tickers: Vec<SharedString>,
}

impl EventEmitter<RecentsEvent> for RecentsService {}

impl RecentsService {
    /// Number of recents shown in the picker's "Recent" section, regardless of
    /// how many are stored.
    pub const DISPLAY_LIMIT: usize = 8;

    pub fn new(_cx: &mut Context<Self>) -> Self {
        Self {
            tickers: persistence::load_recents(),
        }
    }

    pub fn tickers(&self) -> &[SharedString] {
        &self.tickers
    }

    /// Move `ticker` to the front of the list (dedup, capped at
    /// [`STORAGE_LIMIT`]). Persists synchronously — list is tiny.
    pub fn push(&mut self, ticker: SharedString, cx: &mut Context<Self>) {
        self.tickers.retain(|t| t != &ticker);
        self.tickers.insert(0, ticker);
        if self.tickers.len() > STORAGE_LIMIT {
            self.tickers.truncate(STORAGE_LIMIT);
        }
        if let Err(err) = persistence::save_recents(&self.tickers) {
            log::warn!("save recents failed: {err:?}");
        }
        cx.emit(RecentsEvent::Changed);
        cx.notify();
    }

    /// Drop all recents and persist the empty list. No-op if already empty
    /// so we don't emit/notify uselessly.
    pub fn clear(&mut self, cx: &mut Context<Self>) {
        if self.tickers.is_empty() {
            return;
        }
        self.tickers.clear();
        if let Err(err) = persistence::save_recents(&self.tickers) {
            log::warn!("save recents failed: {err:?}");
        }
        cx.emit(RecentsEvent::Changed);
        cx.notify();
    }
}

#[derive(Clone)]
pub struct RecentsServiceHandle(pub Entity<RecentsService>);
impl Global for RecentsServiceHandle {}

pub fn init(cx: &mut App) {
    let entity = cx.new(RecentsService::new);
    cx.set_global(RecentsServiceHandle(entity));
}
