//! Singleton services backing the chart + watchlist panels.
//!
//! Each service is constructed once at `lib.rs::init()` time, wrapped in a
//! `Global` handle, and accessed from panels via `cx.global::<XHandle>().0`.
//!
//! `market_data` is the live WS-backed service (see its module docs); the
//! others are local-state stubs (recents, watchlist) or a hardcoded source
//! (symbols).

pub mod market_data;
pub mod recents;
pub mod symbols;
pub mod watchlist;
