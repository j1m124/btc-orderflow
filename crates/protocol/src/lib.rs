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
    /// Per-symbol liquidation tape (`<symbol>@forceOrder`). One event per
    /// liquidation, throttled by Binance to ≤1/sec. Snapshot returns the
    /// most-recent-N events; pagination via `HistoryPage` (`before_ms`).
    Liquidations,
    /// Per-bar liquidation aggregation: `SUM(qty)` and `SUM(quote_qty)`
    /// split by side, bucketed by the subscription's tf via server-side
    /// `time_bucket`. Snapshot returns the most-recent-N bars; pagination
    /// via `HistoryPage`.
    LiquidationBars { tf: Timeframe },
    /// Per-bar open-interest OHLC: the open/high/low/close of the symbol's
    /// total open interest (in contracts / base asset) within each bar at
    /// the subscription's tf, computed server-side from raw OI samples via
    /// `time_bucket` (`first`/`max`/`min`/`last`). Snapshot returns the
    /// most-recent-N bars; pagination via `HistoryPage`.
    ///
    /// USD notional is NOT shipped — Binance's live `/fapi/v1/openInterest`
    /// endpoint returns contracts only. The client multiplies by the candle
    /// close to render the USD axis (approximate vs Binance's mark-price
    /// figure, but consistent with how the chart's Coin/USD toggle works).
    OpenInterest { tf: Timeframe },
    /// Per-bar mark-price OHLC + funding rate, computed server-side from the
    /// `<symbol>@markPrice` stream samples via `time_bucket`. Mark price is the
    /// canonical reference for accurate USD open-interest notional (the OI
    /// indicators read it instead of the candle close); `funding_rate` carries
    /// the per-bar funding for the funding indicator pane. Snapshot returns the
    /// most-recent-N bars; pagination via `HistoryPage`.
    MarkPrice { tf: Timeframe },
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

// --- Liquidation payload ----------------------------------------------------

/// Side of the *liquidated position* (not Binance's raw forced-order side).
/// `Long` = a long position was liquidated → forced to sell → bearish event.
/// `Short` = a short position was liquidated → forced to buy → bullish event.
///
/// Binance ships `S = "SELL"` for long-liqs and `S = "BUY"` for short-liqs;
/// the server flips at the ingest boundary so every downstream consumer sees
/// the position side directly. Inverting here once means nothing else has to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LiquidationSide {
    Long,
    Short,
}

/// One liquidation event. `ts_ms` is the trade timestamp; `price` is the
/// average fill price (Binance `ap`) and `qty` is the filled qty (Binance
/// `z`) — the *actuals*, not the limit-order price/orig-qty pair. `quote_qty`
/// is `price * qty` precomputed server-side at ingest so per-bar SUM(quote)
/// queries and client renderers don't redo the multiply per row.
///
/// Binance throttles the per-symbol `forceOrder` stream to ≤1 message/sec —
/// only the latest liquidation in each 1-second window survives. Cascade
/// detail is lost upstream; this is an irrecoverable property of the WS feed.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Liquidation {
    pub ts_ms: i64,
    pub side: LiquidationSide,
    pub price: f64,
    pub qty: f64,
    pub quote_qty: f64,
}

/// One per-bar liquidation cell: at `open_time` for the subscription's tf,
/// the sums of long-side vs short-side liquidations within that bar.
///
/// Both coin qty and USD notional are shipped so the client can flip between
/// units (via the chart panel's existing `VolumeUnit` toggle) without
/// re-subscribing. Bars with zero liquidations on a given side ship as zero,
/// not missing — the server emits a row for every bar in the range so the
/// client can distinguish "no data" (no row) from "data, none" (zero row).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LiquidationBar {
    pub open_time: i64,
    pub long_qty: f64,
    pub long_quote_qty: f64,
    pub short_qty: f64,
    pub short_quote_qty: f64,
}

// --- Open interest payload --------------------------------------------------

/// One per-bar open-interest cell: the OHLC of the symbol's total open
/// interest within the bar at `open_time` for the subscription's tf. Values
/// are in **contracts** (base asset, e.g. BTC) — `open` is the first OI
/// sample in the bucket, `close` the last, `high`/`low` the extremes.
///
/// Computed server-side from raw OI samples via `time_bucket`. Only populated
/// buckets ship a row — unlike [`LiquidationBar`], OI is never zero, so a
/// missing bucket means "no sample in that window", not "OI = 0"; the server
/// emits no zero-fill rows and the client connects the points it receives.
///
/// USD notional is not shipped (the live Binance OI endpoint has no USD
/// figure). The client derives USD = `close * candle.close` per bar for the
/// chart's Coin/USD unit toggle — approximate vs Binance's mark-price
/// `sumOpenInterestValue`, but consistent across the terminal.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OpenInterestBar {
    pub open_time: i64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
}

// --- Mark price payload -----------------------------------------------------

/// One per-bar mark-price cell: OHLC of the symbol's mark price within the bar
/// at `open_time` for the subscription's tf, plus the bar's funding rate.
///
/// `open`/`high`/`low`/`close` are the mark price (Binance's fair-price mark —
/// the canonical reference for USD notional, vs the last-trade `candle.close`).
/// `funding_rate` is the per-bar funding as a fraction (e.g. `0.0001` = 0.01%):
/// the last *predicted* funding sampled in the bucket where the live curve
/// exists, falling back to the *settled* 8h rate for historical buckets that
/// predate the live capture. `None` for historical buckets between settlements
/// (no predicted sample, no settlement) — the client connects what it receives.
///
/// Computed server-side from the markPrice samples via `time_bucket`. Mark
/// price backfills continuously (`markPriceKlines`) so every bar in range
/// carries OHLC; funding history is sparse (8h settled points) until the live
/// predicted curve accumulates forward. `#[serde(default)]` keeps an old
/// server's frame readable by a new client.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MarkPriceBar {
    pub open_time: i64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    #[serde(default)]
    pub funding_rate: Option<f64>,
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

    // --- Liquidations channel (tape) ---
    /// Initial liquidation history on subscribe (most-recent-N events,
    /// oldest first). N is server-capped (`LIQ_TAPE_SNAPSHOT_COUNT`).
    LiquidationSnapshot {
        id: SubId,
        liquidations: Vec<Liquidation>,
        server_v: u64,
    },
    /// Batch of new liquidation events. Server batches into ~100ms windows
    /// for consistency with the trade-tick batching shape; given Binance's
    /// 1/sec throttle most ticks will carry a single liquidation.
    LiquidationTick {
        id: SubId,
        liquidations: Vec<Liquidation>,
        v: u64,
    },
    /// Reply to a `HistoryPage` request on a liquidations subscription.
    /// Chronological (oldest first), strictly older than the cursor.
    LiquidationHistoryPage {
        id: SubId,
        liquidations: Vec<Liquidation>,
    },

    // --- LiquidationBars channel (per-bar aggregation) ---
    /// Initial per-bar liquidation aggregation: cells for the most recent
    /// N bars at the subscription's tf. Bars with zero liquidations on a
    /// given side still ship as a zero row (one row per bar in range).
    LiquidationBarSnapshot {
        id: SubId,
        bars: Vec<LiquidationBar>,
        server_v: u64,
    },
    /// Incremental per-bar updates as new liquidations land in active bars.
    /// Bars share `open_time` keys with the snapshot; clients overwrite.
    LiquidationBarUpdate {
        id: SubId,
        bars: Vec<LiquidationBar>,
        v: u64,
    },
    /// Reply to a `HistoryPage` request on a liquidation-bars subscription.
    /// Chronological (oldest first), strictly older than the cursor.
    LiquidationBarHistoryPage {
        id: SubId,
        bars: Vec<LiquidationBar>,
    },

    // --- OpenInterest channel (per-bar OHLC) ---
    /// Initial open-interest bars: OHLC cells for the most recent N bars at
    /// the subscription's tf. One row per populated bucket (no zero-fill —
    /// OI is never zero, so a missing bucket means "no sample").
    OpenInterestSnapshot {
        id: SubId,
        bars: Vec<OpenInterestBar>,
        server_v: u64,
    },
    /// Incremental per-bar OI updates as new samples land in the active bar.
    /// Bars share `open_time` keys with the snapshot; clients overwrite.
    OpenInterestUpdate {
        id: SubId,
        bars: Vec<OpenInterestBar>,
        v: u64,
    },
    /// Reply to a `HistoryPage` request on an open-interest subscription.
    /// Chronological (oldest first), strictly older than the cursor.
    OpenInterestHistoryPage {
        id: SubId,
        bars: Vec<OpenInterestBar>,
    },

    // --- MarkPrice channel (per-bar OHLC + funding) ---
    /// Initial mark-price bars: OHLC + funding for the most recent N bars at
    /// the subscription's tf.
    MarkPriceSnapshot {
        id: SubId,
        bars: Vec<MarkPriceBar>,
        server_v: u64,
    },
    /// Incremental per-bar mark-price updates as new samples land in the active
    /// bar. Bars share `open_time` keys with the snapshot; clients overwrite.
    MarkPriceUpdate {
        id: SubId,
        bars: Vec<MarkPriceBar>,
        v: u64,
    },
    /// Reply to a `HistoryPage` request on a mark-price subscription.
    /// Chronological (oldest first), strictly older than the cursor.
    MarkPriceHistoryPage {
        id: SubId,
        bars: Vec<MarkPriceBar>,
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
    fn channel_liquidations_unit_variant() {
        let s = serde_json::to_string(&Channel::Liquidations).unwrap();
        assert_eq!(s, "{\"kind\":\"liquidations\"}");
    }

    #[test]
    fn channel_liquidation_bars_with_tf() {
        let s = serde_json::to_string(&Channel::LiquidationBars {
            tf: Timeframe::M5,
        })
        .unwrap();
        assert!(s.contains("\"kind\":\"liquidation_bars\""));
        assert!(s.contains("\"tf\":\"5m\""));
    }

    #[test]
    fn liquidation_side_serializes_lowercase() {
        assert_eq!(
            serde_json::to_string(&LiquidationSide::Long).unwrap(),
            "\"long\""
        );
        assert_eq!(
            serde_json::to_string(&LiquidationSide::Short).unwrap(),
            "\"short\""
        );
    }

    #[test]
    fn liquidation_tick_frame_shape() {
        let f = ServerFrame::LiquidationTick {
            id: SubId(11),
            liquidations: vec![Liquidation {
                ts_ms: 1_700_000_000_000,
                side: LiquidationSide::Long,
                price: 16500.0,
                qty: 0.5,
                quote_qty: 8250.0,
            }],
            v: 4,
        };
        let s = serde_json::to_string(&f).unwrap();
        assert!(s.contains("\"type\":\"liquidation_tick\""));
        assert!(s.contains("\"side\":\"long\""));
        assert!(s.contains("\"quote_qty\":8250.0"));
    }

    #[test]
    fn liquidation_bar_snapshot_frame_shape() {
        let f = ServerFrame::LiquidationBarSnapshot {
            id: SubId(12),
            bars: vec![LiquidationBar {
                open_time: 1_700_000_000_000,
                long_qty: 1.5,
                long_quote_qty: 25000.0,
                short_qty: 0.25,
                short_quote_qty: 4000.0,
            }],
            server_v: 0,
        };
        let s = serde_json::to_string(&f).unwrap();
        assert!(s.contains("\"type\":\"liquidation_bar_snapshot\""));
        assert!(s.contains("\"long_qty\":1.5"));
        assert!(s.contains("\"short_quote_qty\":4000.0"));
    }

    #[test]
    fn liquidation_roundtrip() {
        let l = Liquidation {
            ts_ms: 1_700_000_001_234,
            side: LiquidationSide::Short,
            price: 16600.5,
            qty: 0.123,
            quote_qty: 2041.86,
        };
        let s = serde_json::to_string(&l).unwrap();
        let back: Liquidation = serde_json::from_str(&s).unwrap();
        assert_eq!(back.ts_ms, l.ts_ms);
        assert_eq!(back.side, LiquidationSide::Short);
        assert!((back.price - l.price).abs() < 1e-9);
    }

    #[test]
    fn channel_open_interest_with_tf() {
        let s = serde_json::to_string(&Channel::OpenInterest {
            tf: Timeframe::M5,
        })
        .unwrap();
        assert!(s.contains("\"kind\":\"open_interest\""));
        assert!(s.contains("\"tf\":\"5m\""));
    }

    #[test]
    fn open_interest_snapshot_frame_shape() {
        let f = ServerFrame::OpenInterestSnapshot {
            id: SubId(13),
            bars: vec![OpenInterestBar {
                open_time: 1_700_000_000_000,
                open: 84_000.0,
                high: 84_300.0,
                low: 83_950.0,
                close: 84_210.0,
            }],
            server_v: 0,
        };
        let s = serde_json::to_string(&f).unwrap();
        assert!(s.contains("\"type\":\"open_interest_snapshot\""));
        assert!(s.contains("\"close\":84210.0"));
    }

    #[test]
    fn open_interest_bar_roundtrip() {
        let b = OpenInterestBar {
            open_time: 1_700_000_001_234,
            open: 84_000.5,
            high: 84_300.25,
            low: 83_950.75,
            close: 84_210.0,
        };
        let s = serde_json::to_string(&b).unwrap();
        let back: OpenInterestBar = serde_json::from_str(&s).unwrap();
        assert_eq!(back.open_time, b.open_time);
        assert!((back.high - b.high).abs() < 1e-9);
        assert!((back.low - b.low).abs() < 1e-9);
    }

    #[test]
    fn channel_mark_price_with_tf() {
        let s = serde_json::to_string(&Channel::MarkPrice {
            tf: Timeframe::M5,
        })
        .unwrap();
        assert!(s.contains("\"kind\":\"mark_price\""));
        assert!(s.contains("\"tf\":\"5m\""));
    }

    #[test]
    fn mark_price_bar_roundtrip() {
        let b = MarkPriceBar {
            open_time: 1_700_000_001_234,
            open: 84_000.5,
            high: 84_300.25,
            low: 83_950.75,
            close: 84_210.0,
            funding_rate: Some(0.0001),
        };
        let s = serde_json::to_string(&b).unwrap();
        let back: MarkPriceBar = serde_json::from_str(&s).unwrap();
        assert_eq!(back.open_time, b.open_time);
        assert!((back.close - b.close).abs() < 1e-9);
        assert!((back.funding_rate.unwrap() - 0.0001).abs() < 1e-12);
    }

    #[test]
    fn mark_price_bar_funding_defaults_when_missing() {
        // An old server frame without `funding_rate` still deserializes.
        let back: MarkPriceBar = serde_json::from_str(
            "{\"open_time\":1,\"open\":1.0,\"high\":2.0,\"low\":0.5,\"close\":1.5}",
        )
        .unwrap();
        assert!(back.funding_rate.is_none());
    }

    #[test]
    fn mark_price_snapshot_frame_shape() {
        let f = ServerFrame::MarkPriceSnapshot {
            id: SubId(14),
            bars: vec![MarkPriceBar {
                open_time: 1_700_000_000_000,
                open: 84_000.0,
                high: 84_300.0,
                low: 83_950.0,
                close: 84_210.0,
                funding_rate: Some(-0.00005),
            }],
            server_v: 0,
        };
        let s = serde_json::to_string(&f).unwrap();
        assert!(s.contains("\"type\":\"mark_price_snapshot\""));
        assert!(s.contains("\"funding_rate\":-0.00005"));
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
