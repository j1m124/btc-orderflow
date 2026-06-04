//! Binance USDⓂ-Futures market-data client.
//!
//! Only the public market-data surface is used (no API key). The combined
//! WebSocket stream and the kline REST endpoint share a small set of types,
//! collected here for ease of import.

pub mod book;
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

/// Max trades per `/fapi/v1/aggTrades` REST page (Binance hard limit).
pub const AGGTRADES_PAGE_LIMIT: u32 = 1000;

/// Bundle of typed broadcast senders threaded into the ingest path. The
/// connection loop fans inbound events (kline, aggTrade, depth) into the
/// right channel so each consumer task — DB writers, sub-second aggregator,
/// book maintainer, gateway forwarders — subscribes only to what it needs.
#[derive(Clone)]
pub struct BroadcastTxs {
    pub kline: tokio::sync::broadcast::Sender<parse::Tick>,
    pub trade: tokio::sync::broadcast::Sender<parse::TradeTick>,
    pub depth: tokio::sync::broadcast::Sender<parse::DepthDiff>,
}
