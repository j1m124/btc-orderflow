//! Binance USDⓂ-Futures market-data client.
//!
//! Only the public market-data surface is used (no API key). The combined
//! WebSocket stream and the kline REST endpoint share a small set of types,
//! collected here for ease of import.

pub mod parse;
pub mod rest;
pub mod ws;

/// Base hostname for the REST API (USDⓂ-Futures).
pub const REST_BASE: &str = "https://fapi.binance.com";

/// Base URL for the combined-stream WebSocket endpoint (USDⓂ-Futures). The
/// path component `/market` is the futures convention; spot uses `/ws`.
pub const WS_BASE: &str = "wss://fstream.binance.com";

/// Max bars per `/fapi/v1/klines` REST page.
pub const KLINES_PAGE_LIMIT: u32 = 1500;
