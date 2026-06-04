//! Singleton services backing the chart + watchlist panels.
//!
//! Each service is constructed once at `lib.rs::init()` time, wrapped in a
//! `Global` handle, and accessed from panels via `cx.global::<XHandle>().0`.
//!
//! These bodies are stubs. The fork was severed from its previous backend;
//! the public types and function signatures here are preserved so the chart
//! and watchlist panels compile and render an empty state until a real BTC
//! orderflow backend is wired in.

pub mod bar_stream;
pub mod market_data;
pub mod recents;
pub mod symbols;
pub mod watchlist;
