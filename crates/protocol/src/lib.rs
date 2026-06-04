//! Wire types shared by the WASM client and the native server.
//!
//! The whole crate is `serde`-derive and value types — no I/O, no runtime
//! dependencies — so it compiles cleanly for both `wasm32-unknown-unknown`
//! and the host. All frames travel as JSON for v1; the tagged-enum derives
//! line up with `serde_json`'s `tag = "..."` representation directly.

use serde::{Deserialize, Serialize};

// --- Primitive identifiers --------------------------------------------------

/// Per-subscription identifier allocated by the client and echoed by the
/// server on every frame for routing. Reused across reconnects (the server
/// has forgotten the old connection so collisions are impossible).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SubId(pub u32);

// --- Timeframe --------------------------------------------------------------

/// Bar timeframe. Wire form is the Binance-style short string (`"1s"`,
/// `"5s"`, `"1m"`, … `"1d"`).
///
/// `S1` and `S5` are *synthesized client-side from aggTrades* (Binance USD-M
/// futures doesn't expose `kline_1s` / `kline_5s` streams — only spot does);
/// see [`Timeframe::is_native_kline`] for the discriminator the server uses
/// to decide whether a TF maps to a Binance stream / REST kline backfill.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Timeframe {
    #[serde(rename = "1s")]
    S1,
    #[serde(rename = "5s")]
    S5,
    #[serde(rename = "1m")]
    M1,
    #[serde(rename = "5m")]
    M5,
    #[serde(rename = "15m")]
    M15,
    #[serde(rename = "30m")]
    M30,
    #[serde(rename = "1h")]
    H1,
    #[serde(rename = "2h")]
    H2,
    #[serde(rename = "4h")]
    H4,
    #[serde(rename = "6h")]
    H6,
    #[serde(rename = "1d")]
    D1,
}

impl Timeframe {
    /// All timeframes in display order (smallest first). Drives the chart
    /// selector. The server's Binance subscription / kline backfill loops
    /// must filter by [`is_native_kline`](Self::is_native_kline) — S1/S5
    /// have no Binance kline stream and are derived from aggTrades.
    pub const ALL: [Timeframe; 11] = [
        Timeframe::S1,
        Timeframe::S5,
        Timeframe::M1,
        Timeframe::M5,
        Timeframe::M15,
        Timeframe::M30,
        Timeframe::H1,
        Timeframe::H2,
        Timeframe::H4,
        Timeframe::H6,
        Timeframe::D1,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Timeframe::S1 => "1s",
            Timeframe::S5 => "5s",
            Timeframe::M1 => "1m",
            Timeframe::M5 => "5m",
            Timeframe::M15 => "15m",
            Timeframe::M30 => "30m",
            Timeframe::H1 => "1h",
            Timeframe::H2 => "2h",
            Timeframe::H4 => "4h",
            Timeframe::H6 => "6h",
            Timeframe::D1 => "1d",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        Timeframe::ALL.into_iter().find(|tf| tf.as_str() == s)
    }

    /// Nominal bar span in milliseconds.
    pub fn duration_ms(self) -> i64 {
        match self {
            Timeframe::S1 => 1_000,
            Timeframe::S5 => 5_000,
            Timeframe::M1 => 60_000,
            Timeframe::M5 => 5 * 60_000,
            Timeframe::M15 => 15 * 60_000,
            Timeframe::M30 => 30 * 60_000,
            Timeframe::H1 => 60 * 60_000,
            Timeframe::H2 => 2 * 60 * 60_000,
            Timeframe::H4 => 4 * 60 * 60_000,
            Timeframe::H6 => 6 * 60 * 60_000,
            Timeframe::D1 => 24 * 60 * 60_000,
        }
    }

    /// True if Binance USD-M futures publishes a `kline_<tf>` stream for
    /// this TF. False for S1/S5 — those are synthesized from aggTrades.
    /// Server uses this to filter the combined-stream subscription list and
    /// the REST gap-heal loop.
    pub fn is_native_kline(self) -> bool {
        !matches!(self, Timeframe::S1 | Timeframe::S5)
    }
}

// --- Candle (wire-narrow OHLCV) ---------------------------------------------

/// Wire-narrow OHLCV bar. Timestamps are millis since the Unix epoch.
///
/// The `date` display string the client renders in axis labels is derived
/// client-side at deserialize time. `quote_volume`, `trades`, and
/// `taker_buy_vol` are shipped so VWAP, trade-count, and volume-delta
/// indicators can render off candles alone.
///
/// All three extras are `Option` for exchange-portability, NOT as a v1 TODO.
/// Binance kline endpoints populate them on every bar, but Bybit/OKX/Coinbase/
/// Deribit kline APIs are missing one or more fields — when ingest
/// generalizes beyond Binance those rows will legitimately carry `None`.
/// Keep these Optional even if the current DB rows are always full.
///
/// `taker_buy_vol` is the base-asset volume traded with the *taker on the
/// buy side* (aggressive buys hitting asks). Sell-side aggression = `volume
/// - taker_buy_vol`, so volume-delta = `2 * taker_buy_vol - volume`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Candle {
    pub open_time: i64,
    pub close_time: i64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: f64,
    pub quote_volume: Option<f64>,
    pub trades: Option<i32>,
    pub taker_buy_vol: Option<f64>,
}

// --- Connection status ------------------------------------------------------

/// Health of the client↔server WebSocket from the client's perspective.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum LiveStatus {
    Connecting,
    Connected,
    Reconnecting { attempts: u32 },
}

// --- Subscription channel ---------------------------------------------------

/// What kind of data a `Subscribe` op is asking for. Tagged enum on the wire
/// so adding new data kinds (trades, footprint, book) is purely additive on
/// both ends.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Channel {
    Candles { tf: Timeframe },
    /// Raw aggTrade stream (one tick per trade event, or batched).
    Trades,
    /// Per-bar bid/ask volume buckets, computed server-side from the trades
    /// table via `time_bucket` aggregation. `price_bucket` is the bucket
    /// width in quote currency (e.g. `1.0` for $1 buckets on BTCUSDT).
    Footprint {
        tf: Timeframe,
        price_bucket: f64,
    },
    /// Live orderbook state + delta updates. `depth` is the top-N levels
    /// per side the client wants surfaced (the server may cap this).
    Book { depth: u16 },
}

// --- Trade payload ----------------------------------------------------------

/// One aggregated trade event (Binance aggTrade unit). `ts_ms` is the trade
/// time; `agg_id` is the monotonic per-symbol aggregate ID (also used as the
/// pagination cursor on `HistoryPage`).
///
/// `is_buyer_maker = true` → the resting bid was hit → taker SOLD →
/// "sell-side" aggression. `false` → resting ask was lifted → taker BOUGHT →
/// "buy-side" aggression. (This is the Binance convention; we propagate it
/// unchanged.)
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Trade {
    pub ts_ms: i64,
    pub agg_id: i64,
    pub price: f64,
    pub qty: f64,
    pub is_buyer_maker: bool,
}

// --- Footprint payload ------------------------------------------------------

/// One cell of a footprint chart: at bar `open_time`, in the price bucket
/// `[price_bucket_low, price_bucket_low + bucket_width)`, the volume traded
/// with aggressive sells (`bid_vol`) vs aggressive buys (`ask_vol`).
///
/// Bucket width is fixed per subscription via [`Channel::Footprint`], so
/// it's not repeated per cell.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FootprintCell {
    pub open_time: i64,
    pub price_bucket_low: f64,
    pub bid_vol: f64,
    pub ask_vol: f64,
}

// --- Book payload -----------------------------------------------------------

/// One price level on the book. `size == 0` in a delta frame means the level
/// was removed.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BookLevel {
    pub price: f64,
    pub size: f64,
}

/// One historical book-snapshot row (for the heatmap backfill). `ts_ms` is
/// the snapshot timestamp; `bids` / `asks` are sorted best-first.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BookSnapshotEntry {
    pub ts_ms: i64,
    pub bids: Vec<BookLevel>,
    pub asks: Vec<BookLevel>,
}

// --- Client → server frames -------------------------------------------------

/// Frames the client sends to the server, tagged by `"op"`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ClientFrame {
    Subscribe {
        id: SubId,
        symbol: String,
        channel: Channel,
    },
    Unsubscribe {
        id: SubId,
    },
    HistoryPage {
        id: SubId,
        before_ms: i64,
        count: u32,
    },
    Ping {
        ts_ms: i64,
    },
}

// --- Server → client frames -------------------------------------------------

/// Frames the server sends to the client, tagged by `"type"`. `Snapshot`,
/// `Tick`, and `HistoryPage` carry a `SubId` so the client routes them to
/// the right per-subscription `BarStream`. `Resnap` requests the client
/// reset its buffer for a subscription before the next `Snapshot` arrives.
///
/// The per-channel variants (`TradeSnapshot`, `BookDelta`, etc.) are
/// kind-specific rather than wrapped in a generic envelope so the client's
/// `match` does the routing/typing work statically — no runtime check that
/// the payload kind matches the subscription kind.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerFrame {
    // --- Candles channel ---
    Snapshot {
        id: SubId,
        candles: Vec<Candle>,
        /// Monotonic per-subscription cursor; the first `Tick` after this
        /// frame carries `v = server_v + 1`.
        server_v: u64,
    },
    Tick {
        id: SubId,
        candle: Candle,
        is_closed: bool,
        v: u64,
    },
    HistoryPage {
        id: SubId,
        candles: Vec<Candle>,
    },

    // --- Trades channel ---
    /// Initial trade history on subscribe (most-recent-N trades, oldest first).
    TradeSnapshot {
        id: SubId,
        trades: Vec<Trade>,
        server_v: u64,
    },
    /// Batch of new trades (server batches into ~100ms windows to keep WS
    /// message rate to ~10 Hz per subscription).
    TradeTick {
        id: SubId,
        trades: Vec<Trade>,
        v: u64,
    },
    /// Reply to a `HistoryPage` request on a trades subscription. Trades are
    /// chronological (oldest first) and strictly older than the cursor the
    /// client asked about.
    TradeHistoryPage {
        id: SubId,
        trades: Vec<Trade>,
    },

    // --- Footprint channel ---
    /// Initial footprint state: cells for the most recent N bars at the
    /// subscription's `(tf, price_bucket)`.
    FootprintSnapshot {
        id: SubId,
        cells: Vec<FootprintCell>,
        server_v: u64,
    },
    /// Incremental cell updates as new trades land in active bars. Cells
    /// share `(open_time, price_bucket_low)` keys; clients overwrite.
    FootprintUpdate {
        id: SubId,
        cells: Vec<FootprintCell>,
        v: u64,
    },
    /// Older footprint cells, chronological, strictly older than the cursor
    /// the client asked about.
    FootprintHistoryPage {
        id: SubId,
        cells: Vec<FootprintCell>,
    },

    // --- Book channel ---
    /// Initial full book state at subscription. Bids/asks sorted best-first.
    BookSnapshot {
        id: SubId,
        bids: Vec<BookLevel>,
        asks: Vec<BookLevel>,
        server_v: u64,
    },
    /// Incremental book changes. Each level is `{price, size}`; `size == 0`
    /// means the level was removed. Server-batched at ~100ms.
    BookDelta {
        id: SubId,
        bids: Vec<BookLevel>,
        asks: Vec<BookLevel>,
        v: u64,
    },
    /// Paginated historical book snapshots (for heatmap replay), chronological,
    /// strictly older than the cursor.
    BookHistoryPage {
        id: SubId,
        snapshots: Vec<BookSnapshotEntry>,
    },

    // --- Cross-channel control frames ---
    /// Server-detected gap on this subscription — client should reset the
    /// per-subscription state and await the next `Snapshot`.
    Resnap {
        id: SubId,
    },
    Status {
        state: LiveStatus,
    },
    Pong {
        ts_ms: i64,
    },
    Error {
        id: Option<SubId>,
        code: String,
        msg: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeframe_roundtrip() {
        for tf in Timeframe::ALL {
            let s = serde_json::to_string(&tf).unwrap();
            let back: Timeframe = serde_json::from_str(&s).unwrap();
            assert_eq!(tf, back);
            // Wire form matches `as_str()`.
            assert_eq!(s, format!("\"{}\"", tf.as_str()));
        }
    }

    #[test]
    fn subscribe_frame_shape() {
        let f = ClientFrame::Subscribe {
            id: SubId(7),
            symbol: "BTCUSDT".into(),
            channel: Channel::Candles {
                tf: Timeframe::M1,
            },
        };
        let s = serde_json::to_string(&f).unwrap();
        assert!(s.contains("\"op\":\"subscribe\""));
        assert!(s.contains("\"kind\":\"candles\""));
        assert!(s.contains("\"tf\":\"1m\""));
        assert!(s.contains("\"id\":7"));
    }

    #[test]
    fn snapshot_frame_shape() {
        let f = ServerFrame::Snapshot {
            id: SubId(1),
            candles: vec![],
            server_v: 42,
        };
        let s = serde_json::to_string(&f).unwrap();
        assert!(s.contains("\"type\":\"snapshot\""));
        assert!(s.contains("\"server_v\":42"));
    }

    #[test]
    fn live_status_reconnecting() {
        let s =
            serde_json::to_string(&LiveStatus::Reconnecting { attempts: 3 }).unwrap();
        assert!(s.contains("\"state\":\"reconnecting\""));
        assert!(s.contains("\"attempts\":3"));
    }

    #[test]
    fn native_kline_discriminator() {
        assert!(!Timeframe::S1.is_native_kline());
        assert!(!Timeframe::S5.is_native_kline());
        assert!(Timeframe::M1.is_native_kline());
        assert!(Timeframe::D1.is_native_kline());
        // Server's combined-stream URL must only include native kline TFs.
        let native: Vec<_> = Timeframe::ALL
            .into_iter()
            .filter(|tf| tf.is_native_kline())
            .collect();
        assert_eq!(native.len(), 9);
    }

    #[test]
    fn channel_trades_unit_variant() {
        let s = serde_json::to_string(&Channel::Trades).unwrap();
        assert_eq!(s, "{\"kind\":\"trades\"}");
    }

    #[test]
    fn channel_footprint_with_bucket() {
        let s = serde_json::to_string(&Channel::Footprint {
            tf: Timeframe::M1,
            price_bucket: 1.0,
        })
        .unwrap();
        assert!(s.contains("\"kind\":\"footprint\""));
        assert!(s.contains("\"tf\":\"1m\""));
        assert!(s.contains("\"price_bucket\":1.0"));
    }

    #[test]
    fn channel_book_depth() {
        let s = serde_json::to_string(&Channel::Book { depth: 50 }).unwrap();
        assert_eq!(s, "{\"kind\":\"book\",\"depth\":50}");
    }

    #[test]
    fn trade_tick_frame_shape() {
        let f = ServerFrame::TradeTick {
            id: SubId(3),
            trades: vec![Trade {
                ts_ms: 1_700_000_000_000,
                agg_id: 42,
                price: 16500.5,
                qty: 0.123,
                is_buyer_maker: true,
            }],
            v: 7,
        };
        let s = serde_json::to_string(&f).unwrap();
        assert!(s.contains("\"type\":\"trade_tick\""));
        assert!(s.contains("\"agg_id\":42"));
        assert!(s.contains("\"is_buyer_maker\":true"));
    }

    #[test]
    fn book_delta_frame_shape() {
        let f = ServerFrame::BookDelta {
            id: SubId(9),
            bids: vec![BookLevel {
                price: 16500.0,
                size: 2.5,
            }],
            asks: vec![BookLevel {
                price: 16510.0,
                size: 0.0,
            }],
            v: 1,
        };
        let s = serde_json::to_string(&f).unwrap();
        assert!(s.contains("\"type\":\"book_delta\""));
        assert!(s.contains("\"price\":16510.0"));
        assert!(s.contains("\"size\":0.0"));
    }

    #[test]
    fn footprint_snapshot_frame_shape() {
        let f = ServerFrame::FootprintSnapshot {
            id: SubId(1),
            cells: vec![FootprintCell {
                open_time: 1_700_000_000_000,
                price_bucket_low: 16500.0,
                bid_vol: 1.0,
                ask_vol: 2.0,
            }],
            server_v: 0,
        };
        let s = serde_json::to_string(&f).unwrap();
        assert!(s.contains("\"type\":\"footprint_snapshot\""));
        assert!(s.contains("\"price_bucket_low\":16500.0"));
    }
}
