//! Singleton services that own long-lived networked state.
//!
//! Each service is constructed once at `lib.rs::init()` time, wrapped in a
//! `Global` handle, and accessed from panels via `cx.global::<XHandle>().0`.
//! Services own their own poll/stream tasks and emit events that panels
//! subscribe to.

pub mod ai_chat;
pub mod bar_stream;
pub mod calendar;
pub mod details;
pub mod filings;
pub mod insider;
pub mod market_data;
pub mod news;
pub mod recents;
pub mod signal;
pub mod symbols;
pub mod watchlist;
