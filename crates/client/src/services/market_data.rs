//! Live market-data service backed by a WebSocket to `server`.
//!
//! Public types (Candle, Timeframe, Session, LiveStatus, KlineEvent, SubKey,
//! MarketDataService, MarketDataServiceHandle, SubscriptionHandle) keep the
//! exact shapes the chart and watchlist panels expect. Internally they're
//! now wired to:
//!   - one persistent WS connection opened at app boot (Q12a — eager)
//!   - per-SubKey refcounted subscriptions; Subscribe / Unsubscribe frames
//!     get pushed as refcounts cross 0
//!   - id-based routing of incoming Snapshot / Tick / HistoryPage / Resnap
//!     frames to per-SubKey state
//!   - exp-backoff reconnect (1s → 30s cap) that forever-retries

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use protocol as proto;
use chrono::{Local, TimeZone as _};
use futures::{
    SinkExt, StreamExt,
    channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded},
};
use gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, Global, SharedString, Task, WeakEntity,
};
use ws_stream_wasm::{WsMessage, WsMeta};

/// Fallback WS endpoint when `window.location` isn't available (shouldn't
/// happen on the web target, but `server_ws_url()` defends against it).
const FALLBACK_WS_URL: &str = "ws://127.0.0.1:8787/ws";

/// Derive the WS endpoint from `window.location`: `wss://<host>/ws` on
/// HTTPS pages, `ws://<host>/ws` otherwise. In dev (Vite on :3001) the
/// `/ws` path proxies to the local server (see `www/vite.config.js`); in
/// prod the client and server share an origin, so the same code path
/// works in both environments without env-var plumbing.
fn server_ws_url() -> String {
    let Some(window) = web_sys::window() else {
        return FALLBACK_WS_URL.to_string();
    };
    let loc = window.location();
    let protocol = loc.protocol().unwrap_or_else(|_| "http:".into());
    let host = match loc.host() {
        Ok(h) if !h.is_empty() => h,
        _ => return FALLBACK_WS_URL.to_string(),
    };
    let ws_proto = if protocol == "https:" { "wss" } else { "ws" };
    format!("{ws_proto}://{host}/ws")
}

/// Page size for `HistoryPage` requests (Q9c).
const HISTORY_PAGE_SIZE: u32 = 500;

/// Cap on the live trades buffer kept per subscription. The trades panel
/// keeps its own filter-aware persist buffer, so this ring only needs to
/// be deep enough to feed the orderbook's last-trade strip and to seed a
/// freshly-mounted or threshold-changed panel — a short window (~30s–2min
/// of BTC perp tape) is plenty. Drops oldest first.
/// `load_older_trades` deliberately bypasses this cap (user-initiated
/// growth).
const TRADES_BUFFER_CAP: usize = 200;

/// Cap on the per-symbol liquidations tape buffer. Liquidations are sparse
/// (≤1/sec from Binance) and the tape panel only paints what fits in the
/// visible rows — but we hold extra so a filter-aware persist (e.g. "$1M+
/// only") can keep showing the rare big print after a reconnect. Set higher
/// than the trade tape because liquidations matter farther back in time.
const LIQUIDATIONS_BUFFER_CAP: usize = 1000;

const RECONNECT_MIN: Duration = Duration::from_secs(1);
const RECONNECT_MAX: Duration = Duration::from_secs(30);

/// A chart timeframe.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Timeframe {
    S1,
    S5,
    M1,
    M5,
    M15,
    M30,
    H1,
    H2,
    H4,
    H6,
    D1,
}

/// Timeframe shown by default on a freshly-opened chart.
pub const DEFAULT_TIMEFRAME: Timeframe = Timeframe::M5;

impl Timeframe {
    /// All timeframes in display order — drives the chart's tf selector.
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

    /// Nominal bar span in milliseconds. Used by the chart x-axis step picker.
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
}

/// A single OHLCV bar.
#[derive(Clone, Debug)]
pub struct Candle {
    pub open_time: i64,
    pub close_time: i64,
    pub date: SharedString,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: f64,
    pub vwap: Option<f64>,
    pub trades: Option<i32>,
    /// Base-asset volume traded with the *taker on the buy side* (aggressive
    /// buys hitting asks). Drives volume-delta / CVD indicators without
    /// needing the trade tape: `delta = 2 * taker_buy_vol - volume`.
    pub taker_buy_vol: Option<f64>,
}

impl Candle {
    pub fn new(
        open_time: i64,
        close_time: i64,
        open: f64,
        high: f64,
        low: f64,
        close: f64,
        volume: f64,
    ) -> Self {
        Self::new_full(
            open_time, close_time, open, high, low, close, volume, None, None, None,
        )
    }

    pub fn new_full(
        open_time: i64,
        close_time: i64,
        open: f64,
        high: f64,
        low: f64,
        close: f64,
        volume: f64,
        vwap: Option<f64>,
        trades: Option<i32>,
        taker_buy_vol: Option<f64>,
    ) -> Self {
        let date = Local
            .timestamp_millis_opt(open_time)
            .single()
            .map(|dt| dt.format("%b %d %H:%M").to_string())
            .unwrap_or_default();
        Self {
            open_time,
            close_time,
            date: date.into(),
            open,
            high,
            low,
            close,
            volume,
            vwap,
            trades,
            taker_buy_vol,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum LiveStatus {
    Connecting,
    Connected,
    Reconnecting { attempts: u32 },
}

#[derive(Clone, Debug)]
pub enum KlineEvent {
    Tick {
        symbol: SharedString,
        tf: Timeframe,
        candle: Candle,
        is_closed: bool,
    },
    Resnap {
        symbol: SharedString,
        tf: Timeframe,
    },
    Prepended {
        symbol: SharedString,
        tf: Timeframe,
        added: usize,
    },
    HistoryCapped {
        symbol: SharedString,
        tf: Timeframe,
    },
    StatusChanged {
        symbol: SharedString,
        tf: Timeframe,
        status: LiveStatus,
    },
}

// --- Orderflow domain types -------------------------------------------------

/// One aggregated trade. Mirrors the wire shape; no extra display fields
/// (panels format on render).
#[derive(Clone, Debug)]
pub struct Trade {
    pub ts_ms: i64,
    pub agg_id: i64,
    pub price: f64,
    pub qty: f64,
    pub is_buyer_maker: bool,
}

/// One footprint cell — bid/ask volume at a (bar, price bucket).
#[derive(Clone, Debug)]
pub struct FootprintCell {
    pub open_time: i64,
    pub price_bucket_low: f64,
    pub bid_vol: f64,
    pub ask_vol: f64,
}

/// Borrowed bucket-keyed view over the footprint cell cache that
/// [`crate::panels::ContentPanel`] hands to [`crate::indicators::ComputeCtx`]
/// every compute pass. The indicator-side compute (notably VRVP) doesn't
/// otherwise have access to per-bucket footprint data, so we route it
/// through this read-only lookup; non-VP indicators ignore the field.
///
/// The backing map's outer key is the *bit pattern* of the bucket f64
/// (matches the existing `FootprintSubKey` keying so a chart-owned bucket
/// and a VP-instance-owned bucket at the same dollar value collide
/// correctly into one map entry).
#[derive(Clone, Copy, Debug)]
pub struct FootprintCellLookup<'a> {
    by_bucket: &'a std::collections::HashMap<u64, Vec<FootprintCell>>,
}

impl<'a> FootprintCellLookup<'a> {
    pub fn new(by_bucket: &'a std::collections::HashMap<u64, Vec<FootprintCell>>) -> Self {
        Self { by_bucket }
    }

    /// Cells for the bucket whose dollar size is `bucket`. `None` if no live
    /// subscription has populated cells for that bucket yet — caller
    /// (VP compute) treats it the same as an empty slice.
    pub fn cells_for_bucket(&self, bucket: f64) -> Option<&'a [FootprintCell]> {
        self.by_bucket
            .get(&bucket.to_bits())
            .map(|v| v.as_slice())
    }
}

/// Side of the *liquidated position* — matches the wire-form, decoded
/// server-side once at ingest. `Long` = a long position was liquidated
/// (bearish event, taker forced to sell). `Short` = a short position was
/// liquidated (bullish event, taker forced to buy).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LiquidationSide {
    Long,
    Short,
}

/// One liquidation event surfaced into the tape panel. Mirrors the wire
/// shape; `quote_qty` is `price * qty` precomputed server-side at ingest.
#[derive(Clone, Debug)]
pub struct Liquidation {
    pub ts_ms: i64,
    pub side: LiquidationSide,
    pub price: f64,
    pub qty: f64,
    pub quote_qty: f64,
}

/// One per-bar liquidation aggregation cell. Both qty and quote_qty are
/// shipped on each side so the bar-stat indicator can flip Coin/USD axes
/// without re-subscribing.
#[derive(Clone, Debug)]
pub struct LiquidationBar {
    pub open_time: i64,
    pub long_qty: f64,
    pub long_quote_qty: f64,
    pub short_qty: f64,
    pub short_quote_qty: f64,
}

/// One book price level.
#[derive(Clone, Debug)]
pub struct BookLevel {
    pub price: f64,
    pub size: f64,
}

/// One historical book snapshot row used by the heatmap replay path.
#[derive(Clone, Debug)]
pub struct BookSnapshotEntry {
    pub ts_ms: i64,
    pub bids: Vec<BookLevel>,
    pub asks: Vec<BookLevel>,
}

// --- Per-channel event surfaces --------------------------------------------

#[derive(Clone, Debug)]
pub enum TradeEvent {
    /// Initial trades on subscribe (oldest-first, most-recent N).
    Snapshot {
        symbol: SharedString,
        trades: Vec<Trade>,
    },
    /// Live batch (server emits at ~10 Hz).
    Tick {
        symbol: SharedString,
        trades: Vec<Trade>,
    },
    /// Older trades prepended in response to `load_older_trades`.
    Prepended {
        symbol: SharedString,
        added: usize,
    },
    /// No older trades available — paginator should stop.
    HistoryCapped { symbol: SharedString },
    /// Server requested a reset; buffer cleared and re-Subscribe sent.
    Resnap { symbol: SharedString },
}

#[derive(Clone, Debug)]
pub enum FootprintEvent {
    /// Initial cells for the recent N bars.
    Snapshot {
        symbol: SharedString,
        tf: Timeframe,
        cells: Vec<FootprintCell>,
    },
    /// Live cell update (compose by (open_time, price_bucket_low)).
    Update {
        symbol: SharedString,
        tf: Timeframe,
        cells: Vec<FootprintCell>,
    },
    /// Older cells prepended.
    Prepended {
        symbol: SharedString,
        tf: Timeframe,
        added: usize,
    },
    HistoryCapped {
        symbol: SharedString,
        tf: Timeframe,
    },
    Resnap {
        symbol: SharedString,
        tf: Timeframe,
    },
}

#[derive(Clone, Debug)]
pub enum LiquidationEvent {
    /// Initial liquidations on subscribe (oldest-first, most-recent N).
    Snapshot {
        symbol: SharedString,
        liquidations: Vec<Liquidation>,
    },
    /// Live batch (server emits at ~10 Hz; usually 0–1 events given the
    /// 1/sec Binance throttle).
    Tick {
        symbol: SharedString,
        liquidations: Vec<Liquidation>,
    },
    /// Older liquidations prepended in response to `load_older_liquidations`.
    Prepended {
        symbol: SharedString,
        added: usize,
    },
    /// No older liquidations available — paginator should stop.
    HistoryCapped { symbol: SharedString },
    /// Server requested a reset; buffer cleared and re-Subscribe sent.
    Resnap { symbol: SharedString },
}

#[derive(Clone, Debug)]
pub enum LiquidationBarEvent {
    /// Initial per-bar cells for the recent N bars.
    Snapshot {
        symbol: SharedString,
        tf: Timeframe,
        bars: Vec<LiquidationBar>,
    },
    /// Live per-bar update (compose by `open_time`).
    Update {
        symbol: SharedString,
        tf: Timeframe,
        bars: Vec<LiquidationBar>,
    },
    /// Older bars prepended.
    Prepended {
        symbol: SharedString,
        tf: Timeframe,
        added: usize,
    },
    HistoryCapped {
        symbol: SharedString,
        tf: Timeframe,
    },
    Resnap {
        symbol: SharedString,
        tf: Timeframe,
    },
}

#[derive(Clone, Debug)]
pub enum BookEvent {
    /// Initial top-N book state.
    Snapshot {
        symbol: SharedString,
        bids: Vec<BookLevel>,
        asks: Vec<BookLevel>,
    },
    /// Live changed levels (size = 0 → remove).
    Delta {
        symbol: SharedString,
        bids: Vec<BookLevel>,
        asks: Vec<BookLevel>,
    },
    /// Historical snapshots prepended (oldest-first within the batch).
    HistoryPrepended {
        symbol: SharedString,
        added: usize,
    },
    HistoryCapped { symbol: SharedString },
    Resnap { symbol: SharedString },
}

#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) struct SubKey {
    pub(crate) symbol: String,
    pub(crate) tf: Timeframe,
}

impl SubKey {
    fn new(symbol: &str, tf: Timeframe) -> Self {
        Self {
            symbol: symbol.to_string(),
            tf,
        }
    }
}

#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) struct TradeSubKey {
    pub(crate) symbol: String,
}

impl TradeSubKey {
    fn new(symbol: &str) -> Self {
        Self {
            symbol: symbol.to_string(),
        }
    }
}

/// Footprint subscription key. `bucket_bits` is the bit pattern of the f64
/// price bucket so `Hash`/`Eq` work; recover the float via `f64::from_bits`.
#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) struct FootprintSubKey {
    pub(crate) symbol: String,
    pub(crate) tf: Timeframe,
    pub(crate) bucket_bits: u64,
}

impl FootprintSubKey {
    fn new(symbol: &str, tf: Timeframe, price_bucket: f64) -> Self {
        Self {
            symbol: symbol.to_string(),
            tf,
            bucket_bits: price_bucket.to_bits(),
        }
    }
    fn bucket(&self) -> f64 {
        f64::from_bits(self.bucket_bits)
    }
}

#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) struct BookSubKey {
    pub(crate) symbol: String,
    pub(crate) depth: u16,
}

impl BookSubKey {
    fn new(symbol: &str, depth: u16) -> Self {
        Self {
            symbol: symbol.to_string(),
            depth,
        }
    }
}

#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) struct LiquidationsSubKey {
    pub(crate) symbol: String,
}

impl LiquidationsSubKey {
    fn new(symbol: &str) -> Self {
        Self {
            symbol: symbol.to_string(),
        }
    }
}

#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) struct LiquidationBarsSubKey {
    pub(crate) symbol: String,
    pub(crate) tf: Timeframe,
}

impl LiquidationBarsSubKey {
    fn new(symbol: &str, tf: Timeframe) -> Self {
        Self {
            symbol: symbol.to_string(),
            tf,
        }
    }
}

/// Discriminator on `by_id` so an incoming server frame can route to the
/// right per-channel handler from just its `SubId`.
#[derive(Clone)]
enum AnySubKey {
    Candles(SubKey),
    Trades(TradeSubKey),
    Footprint(FootprintSubKey),
    Book(BookSubKey),
    Liquidations(LiquidationsSubKey),
    LiquidationBars(LiquidationBarsSubKey),
}

/// Used by the release pump to know which kind of refcount to decrement
/// when a `SubscriptionHandle` drops.
#[derive(Clone)]
enum ReleaseKey {
    Candles(SubKey),
    Trades(TradeSubKey),
    Footprint(FootprintSubKey),
    Book(BookSubKey),
    Liquidations(LiquidationsSubKey),
    LiquidationBars(LiquidationBarsSubKey),
}

pub struct MarketDataService {
    // --- Candles channel state ---
    candles: HashMap<SubKey, Vec<Candle>>,
    statuses: HashMap<SubKey, LiveStatus>,
    sub_ids: HashMap<SubKey, proto::SubId>,
    refcounts: HashMap<SubKey, usize>,
    /// SubKeys with an outstanding `HistoryPage` request awaiting a reply.
    /// The chart fires `load_older` on every scroll-wheel/x-axis-drag tick
    /// while the leftmost bar is within a viewport of the canvas edge; without
    /// this guard a fast scroll bursts N identical requests (all keyed on the
    /// unchanged `candles.first().open_time`) and the server returns N copies
    /// of the same page → duplicated prepends.
    history_in_flight: HashSet<SubKey>,

    // --- Trades channel state ---
    trades: HashMap<TradeSubKey, Vec<Trade>>,
    trade_sub_ids: HashMap<TradeSubKey, proto::SubId>,
    trade_refcounts: HashMap<TradeSubKey, usize>,
    trade_history_in_flight: HashSet<TradeSubKey>,

    // --- Footprint channel state ---
    /// Cells keyed by `(open_time, price_bucket_low.to_bits())` so updates
    /// from the server compose by overwriting on the same composite key.
    footprint: HashMap<FootprintSubKey, HashMap<(i64, u64), FootprintCell>>,
    footprint_sub_ids: HashMap<FootprintSubKey, proto::SubId>,
    footprint_refcounts: HashMap<FootprintSubKey, usize>,
    footprint_history_in_flight: HashSet<FootprintSubKey>,

    // --- Book channel state ---
    /// Per-subscription book state: bids/asks sorted best-first.
    book: HashMap<BookSubKey, (Vec<BookLevel>, Vec<BookLevel>)>,
    /// Historical book snapshots (oldest-first) for heatmap replay.
    book_history: HashMap<BookSubKey, Vec<BookSnapshotEntry>>,
    book_sub_ids: HashMap<BookSubKey, proto::SubId>,
    book_refcounts: HashMap<BookSubKey, usize>,
    book_history_in_flight: HashSet<BookSubKey>,

    // --- Liquidations (tape) channel state ---
    liquidations: HashMap<LiquidationsSubKey, Vec<Liquidation>>,
    liquidations_sub_ids: HashMap<LiquidationsSubKey, proto::SubId>,
    liquidations_refcounts: HashMap<LiquidationsSubKey, usize>,
    liquidations_history_in_flight: HashSet<LiquidationsSubKey>,

    // --- LiquidationBars (per-bar) channel state ---
    /// Bars keyed by `open_time` so live updates compose by overwriting.
    liquidation_bars: HashMap<LiquidationBarsSubKey, HashMap<i64, LiquidationBar>>,
    liquidation_bars_sub_ids: HashMap<LiquidationBarsSubKey, proto::SubId>,
    liquidation_bars_refcounts: HashMap<LiquidationBarsSubKey, usize>,
    liquidation_bars_history_in_flight: HashSet<LiquidationBarsSubKey>,

    // --- Shared routing / connection state ---

    /// Maps wire `SubId` → channel-tagged key. One counter is shared across
    /// all four channels so IDs never collide.
    by_id: HashMap<proto::SubId, AnySubKey>,
    next_sub_id: u32,

    /// Outbound queue drained by the connection driver task. Sends from
    /// here are non-blocking; while disconnected they buffer until the
    /// driver reconnects, at which point `resubscribe_all` flushes any
    /// stale state with the canonical set.
    to_ws: UnboundedSender<proto::ClientFrame>,

    /// Receiver for `SubscriptionHandle::Drop` notifications. The release
    /// task drains this and calls `release_one_*` per kind.
    release_tx: UnboundedSender<ReleaseKey>,

    conn_status: LiveStatus,
    last_message_ms: Option<i64>,

    /// Owned Tasks for the connection driver + release pump. Stored on the
    /// struct so `Drop` on the entity tears them down. Spawned via
    /// `Context<Self>::spawn` (WeakEntity-based) so update closures never
    /// fight a strong-Entity borrow inside another `update` — that's the
    /// re-entry shape that causes "RefCell already borrowed" panics in
    /// gpui internals.
    _ws_task: Task<()>,
    _release_task: Task<()>,
}

impl EventEmitter<KlineEvent> for MarketDataService {}
impl EventEmitter<TradeEvent> for MarketDataService {}
impl EventEmitter<FootprintEvent> for MarketDataService {}
impl EventEmitter<BookEvent> for MarketDataService {}
impl EventEmitter<LiquidationEvent> for MarketDataService {}
impl EventEmitter<LiquidationBarEvent> for MarketDataService {}

impl MarketDataService {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let (release_tx, mut release_rx) = unbounded::<ReleaseKey>();
        let (to_ws, outbound_rx) = unbounded::<proto::ClientFrame>();

        // Release pump: drains SubscriptionHandle::Drop notifications and
        // calls release_one_* one tick later. `this.update(...)` here returns
        // `Result` (WeakEntity); break on entity drop.
        let release_task = cx.spawn(async move |this, cx| {
            while let Some(rk) = release_rx.next().await {
                if this
                    .update(cx, |s, cx| match rk {
                        ReleaseKey::Candles(k) => s.release_one(k, cx),
                        ReleaseKey::Trades(k) => s.release_one_trades(k, cx),
                        ReleaseKey::Footprint(k) => s.release_one_footprint(k, cx),
                        ReleaseKey::Book(k) => s.release_one_book(k, cx),
                        ReleaseKey::Liquidations(k) => s.release_one_liquidations(k, cx),
                        ReleaseKey::LiquidationBars(k) => s.release_one_liquidation_bars(k, cx),
                    })
                    .is_err()
                {
                    return;
                }
            }
        });

        // Connection driver: opens WS, drains outbound, dispatches inbound,
        // exp-backoff reconnect forever.
        let ws_task = cx.spawn(async move |this, cx| {
            run_connection(this, outbound_rx, cx).await;
        });

        Self {
            candles: HashMap::new(),
            statuses: HashMap::new(),
            sub_ids: HashMap::new(),
            refcounts: HashMap::new(),
            history_in_flight: HashSet::new(),
            trades: HashMap::new(),
            trade_sub_ids: HashMap::new(),
            trade_refcounts: HashMap::new(),
            trade_history_in_flight: HashSet::new(),
            footprint: HashMap::new(),
            footprint_sub_ids: HashMap::new(),
            footprint_refcounts: HashMap::new(),
            footprint_history_in_flight: HashSet::new(),
            book: HashMap::new(),
            book_history: HashMap::new(),
            book_sub_ids: HashMap::new(),
            book_refcounts: HashMap::new(),
            book_history_in_flight: HashSet::new(),
            liquidations: HashMap::new(),
            liquidations_sub_ids: HashMap::new(),
            liquidations_refcounts: HashMap::new(),
            liquidations_history_in_flight: HashSet::new(),
            liquidation_bars: HashMap::new(),
            liquidation_bars_sub_ids: HashMap::new(),
            liquidation_bars_refcounts: HashMap::new(),
            liquidation_bars_history_in_flight: HashSet::new(),
            by_id: HashMap::new(),
            next_sub_id: 0,
            to_ws,
            release_tx,
            conn_status: LiveStatus::Connecting,
            last_message_ms: None,
            _ws_task: ws_task,
            _release_task: release_task,
        }
    }

    fn alloc_sub_id(&mut self) -> proto::SubId {
        let id = proto::SubId(self.next_sub_id);
        self.next_sub_id = self.next_sub_id.saturating_add(1);
        id
    }

    /// Refcounted subscribe. The first `ensure` for a `SubKey` allocates a
    /// `SubId` and pushes a `Subscribe` frame; subsequent ensures just bump
    /// the count.
    pub fn ensure(
        &mut self,
        symbol: &str,
        tf: Timeframe,
        cx: &mut Context<Self>,
    ) -> SubscriptionHandle {
        let key = SubKey::new(symbol, tf);
        let count = self.refcounts.entry(key.clone()).or_insert(0);
        *count += 1;
        let first = *count == 1;

        if first {
            let sub_id = self.alloc_sub_id();
            self.sub_ids.insert(key.clone(), sub_id);
            self.by_id.insert(sub_id, AnySubKey::Candles(key.clone()));
            self.candles.insert(key.clone(), Vec::new());
            self.statuses.insert(key.clone(), self.conn_status.clone());

            let frame = proto::ClientFrame::Subscribe {
                id: sub_id,
                symbol: symbol.to_string(),
                channel: proto::Channel::Candles {
                    tf: proto_tf(tf),
                },
            };
            let _ = self.to_ws.unbounded_send(frame);

            let status = self.conn_status.clone();
            cx.emit(KlineEvent::StatusChanged {
                symbol: symbol.into(),
                tf,
                status,
            });
        }

        SubscriptionHandle {
            key: ReleaseKey::Candles(key),
            release_tx: self.release_tx.clone(),
        }
    }

    /// Refcounted subscribe for the trades channel.
    pub fn ensure_trades(
        &mut self,
        symbol: &str,
        _cx: &mut Context<Self>,
    ) -> SubscriptionHandle {
        let key = TradeSubKey::new(symbol);
        let count = self.trade_refcounts.entry(key.clone()).or_insert(0);
        *count += 1;
        if *count == 1 {
            let sub_id = self.alloc_sub_id();
            self.trade_sub_ids.insert(key.clone(), sub_id);
            self.by_id.insert(sub_id, AnySubKey::Trades(key.clone()));
            self.trades.insert(key.clone(), Vec::new());
            let frame = proto::ClientFrame::Subscribe {
                id: sub_id,
                symbol: symbol.to_string(),
                channel: proto::Channel::Trades,
            };
            let _ = self.to_ws.unbounded_send(frame);
        }
        SubscriptionHandle {
            key: ReleaseKey::Trades(key),
            release_tx: self.release_tx.clone(),
        }
    }

    /// Refcounted subscribe for the footprint channel.
    pub fn ensure_footprint(
        &mut self,
        symbol: &str,
        tf: Timeframe,
        price_bucket: f64,
        _cx: &mut Context<Self>,
    ) -> SubscriptionHandle {
        let key = FootprintSubKey::new(symbol, tf, price_bucket);
        let count = self.footprint_refcounts.entry(key.clone()).or_insert(0);
        *count += 1;
        if *count == 1 {
            let sub_id = self.alloc_sub_id();
            self.footprint_sub_ids.insert(key.clone(), sub_id);
            self.by_id.insert(sub_id, AnySubKey::Footprint(key.clone()));
            self.footprint.insert(key.clone(), HashMap::new());
            let frame = proto::ClientFrame::Subscribe {
                id: sub_id,
                symbol: symbol.to_string(),
                channel: proto::Channel::Footprint {
                    tf: proto_tf(tf),
                    price_bucket,
                },
            };
            let _ = self.to_ws.unbounded_send(frame);
        }
        SubscriptionHandle {
            key: ReleaseKey::Footprint(key),
            release_tx: self.release_tx.clone(),
        }
    }

    /// Refcounted subscribe for the book channel.
    pub fn ensure_book(
        &mut self,
        symbol: &str,
        depth: u16,
        _cx: &mut Context<Self>,
    ) -> SubscriptionHandle {
        let key = BookSubKey::new(symbol, depth);
        let count = self.book_refcounts.entry(key.clone()).or_insert(0);
        *count += 1;
        if *count == 1 {
            let sub_id = self.alloc_sub_id();
            self.book_sub_ids.insert(key.clone(), sub_id);
            self.by_id.insert(sub_id, AnySubKey::Book(key.clone()));
            self.book.insert(key.clone(), (Vec::new(), Vec::new()));
            self.book_history.insert(key.clone(), Vec::new());
            let frame = proto::ClientFrame::Subscribe {
                id: sub_id,
                symbol: symbol.to_string(),
                channel: proto::Channel::Book { depth },
            };
            let _ = self.to_ws.unbounded_send(frame);
        }
        SubscriptionHandle {
            key: ReleaseKey::Book(key),
            release_tx: self.release_tx.clone(),
        }
    }

    /// Refcounted subscribe for the liquidations tape channel.
    pub fn ensure_liquidations(
        &mut self,
        symbol: &str,
        _cx: &mut Context<Self>,
    ) -> SubscriptionHandle {
        let key = LiquidationsSubKey::new(symbol);
        let count = self.liquidations_refcounts.entry(key.clone()).or_insert(0);
        *count += 1;
        if *count == 1 {
            let sub_id = self.alloc_sub_id();
            self.liquidations_sub_ids.insert(key.clone(), sub_id);
            self.by_id.insert(sub_id, AnySubKey::Liquidations(key.clone()));
            self.liquidations.insert(key.clone(), Vec::new());
            let frame = proto::ClientFrame::Subscribe {
                id: sub_id,
                symbol: symbol.to_string(),
                channel: proto::Channel::Liquidations,
            };
            let _ = self.to_ws.unbounded_send(frame);
        }
        SubscriptionHandle {
            key: ReleaseKey::Liquidations(key),
            release_tx: self.release_tx.clone(),
        }
    }

    /// Refcounted subscribe for the per-bar liquidations channel.
    pub fn ensure_liquidation_bars(
        &mut self,
        symbol: &str,
        tf: Timeframe,
        _cx: &mut Context<Self>,
    ) -> SubscriptionHandle {
        let key = LiquidationBarsSubKey::new(symbol, tf);
        let count = self
            .liquidation_bars_refcounts
            .entry(key.clone())
            .or_insert(0);
        *count += 1;
        if *count == 1 {
            let sub_id = self.alloc_sub_id();
            self.liquidation_bars_sub_ids.insert(key.clone(), sub_id);
            self.by_id
                .insert(sub_id, AnySubKey::LiquidationBars(key.clone()));
            self.liquidation_bars.insert(key.clone(), HashMap::new());
            let frame = proto::ClientFrame::Subscribe {
                id: sub_id,
                symbol: symbol.to_string(),
                channel: proto::Channel::LiquidationBars { tf: proto_tf(tf) },
            };
            let _ = self.to_ws.unbounded_send(frame);
        }
        SubscriptionHandle {
            key: ReleaseKey::LiquidationBars(key),
            release_tx: self.release_tx.clone(),
        }
    }

    pub fn liquidations(&self, symbol: &str) -> Option<&[Liquidation]> {
        self.liquidations
            .get(&LiquidationsSubKey::new(symbol))
            .map(|v| v.as_slice())
    }

    pub fn liquidation_bars(
        &self,
        symbol: &str,
        tf: Timeframe,
    ) -> Vec<LiquidationBar> {
        let key = LiquidationBarsSubKey::new(symbol, tf);
        match self.liquidation_bars.get(&key) {
            Some(map) => {
                let mut bars: Vec<LiquidationBar> = map.values().cloned().collect();
                bars.sort_by_key(|b| b.open_time);
                bars
            }
            None => Vec::new(),
        }
    }

    pub fn trades_snapshot(&self, symbol: &str) -> Option<&[Trade]> {
        self.trades.get(&TradeSubKey::new(symbol)).map(|v| v.as_slice())
    }

    pub fn book_snapshot(
        &self,
        symbol: &str,
        depth: u16,
    ) -> Option<(&[BookLevel], &[BookLevel])> {
        self.book
            .get(&BookSubKey::new(symbol, depth))
            .map(|(b, a)| (b.as_slice(), a.as_slice()))
    }

    pub fn footprint_cells(
        &self,
        symbol: &str,
        tf: Timeframe,
        price_bucket: f64,
    ) -> Vec<FootprintCell> {
        self.footprint
            .get(&FootprintSubKey::new(symbol, tf, price_bucket))
            .map(|m| m.values().cloned().collect())
            .unwrap_or_default()
    }

    /// Reconnection is handled by the driver task automatically. Left as a
    /// no-op for call-site compatibility with the old stub.
    pub fn reconnect_all(&mut self, _cx: &mut Context<Self>) {}

    pub fn snapshot(&self, symbol: &str, tf: Timeframe) -> Option<&[Candle]> {
        self.candles
            .get(&SubKey::new(symbol, tf))
            .map(|v| v.as_slice())
    }

    pub fn status(&self, symbol: &str, tf: Timeframe) -> LiveStatus {
        self.statuses
            .get(&SubKey::new(symbol, tf))
            .cloned()
            .unwrap_or_else(|| self.conn_status.clone())
    }

    pub fn overall_status(&self) -> LiveStatus {
        self.conn_status.clone()
    }

    pub fn last_message_ms(&self) -> Option<i64> {
        self.last_message_ms
    }

    /// Request older bars for an existing subscription. The page is keyed
    /// to `before_ms = oldest currently-held open_time`; the server replies
    /// with a `HistoryPage` frame that gets prepended to the buffer.
    pub fn load_older(
        &mut self,
        symbol: &str,
        tf: Timeframe,
        _cx: &mut Context<Self>,
    ) {
        let key = SubKey::new(symbol, tf);
        let Some(sub_id) = self.sub_ids.get(&key).copied() else {
            return;
        };
        // One in-flight HistoryPage per (symbol, tf); reset on response,
        // snapshot, resnap, or release.
        if !self.history_in_flight.insert(key.clone()) {
            return;
        }
        let before_ms = self
            .candles
            .get(&key)
            .and_then(|bars| bars.first())
            .map(|c| c.open_time)
            .unwrap_or_else(|| chrono::Utc::now().timestamp_millis());

        let frame = proto::ClientFrame::HistoryPage {
            id: sub_id,
            before_ms,
            count: HISTORY_PAGE_SIZE,
        };
        let _ = self.to_ws.unbounded_send(frame);
    }

    /// Request older trades. Cursor is `before_ms = oldest held trade.ts_ms`.
    pub fn load_older_trades(&mut self, symbol: &str, _cx: &mut Context<Self>) {
        let key = TradeSubKey::new(symbol);
        let Some(sub_id) = self.trade_sub_ids.get(&key).copied() else {
            return;
        };
        if !self.trade_history_in_flight.insert(key.clone()) {
            return;
        }
        let before_ms = self
            .trades
            .get(&key)
            .and_then(|v| v.first())
            .map(|t| t.ts_ms)
            .unwrap_or_else(|| chrono::Utc::now().timestamp_millis());
        let frame = proto::ClientFrame::HistoryPage {
            id: sub_id,
            before_ms,
            count: HISTORY_PAGE_SIZE,
        };
        let _ = self.to_ws.unbounded_send(frame);
    }

    /// Request older footprint cells. Cursor is the oldest held `open_time`.
    pub fn load_older_footprint(
        &mut self,
        symbol: &str,
        tf: Timeframe,
        price_bucket: f64,
        _cx: &mut Context<Self>,
    ) {
        let key = FootprintSubKey::new(symbol, tf, price_bucket);
        let Some(sub_id) = self.footprint_sub_ids.get(&key).copied() else {
            return;
        };
        if !self.footprint_history_in_flight.insert(key.clone()) {
            return;
        }
        let before_ms = self
            .footprint
            .get(&key)
            .and_then(|cells| cells.keys().map(|(t, _)| *t).min())
            .unwrap_or_else(|| chrono::Utc::now().timestamp_millis());
        let frame = proto::ClientFrame::HistoryPage {
            id: sub_id,
            before_ms,
            count: HISTORY_PAGE_SIZE,
        };
        let _ = self.to_ws.unbounded_send(frame);
    }

    /// Request older book snapshots. Cursor is the oldest held `ts_ms`.
    pub fn load_older_book(
        &mut self,
        symbol: &str,
        depth: u16,
        _cx: &mut Context<Self>,
    ) {
        let key = BookSubKey::new(symbol, depth);
        let Some(sub_id) = self.book_sub_ids.get(&key).copied() else {
            return;
        };
        if !self.book_history_in_flight.insert(key.clone()) {
            return;
        }
        let before_ms = self
            .book_history
            .get(&key)
            .and_then(|v| v.first())
            .map(|s| s.ts_ms)
            .unwrap_or_else(|| chrono::Utc::now().timestamp_millis());
        let frame = proto::ClientFrame::HistoryPage {
            id: sub_id,
            before_ms,
            count: HISTORY_PAGE_SIZE,
        };
        let _ = self.to_ws.unbounded_send(frame);
    }

    /// Request older liquidations. Cursor = `oldest held event.ts_ms`.
    pub fn load_older_liquidations(&mut self, symbol: &str, _cx: &mut Context<Self>) {
        let key = LiquidationsSubKey::new(symbol);
        let Some(sub_id) = self.liquidations_sub_ids.get(&key).copied() else {
            return;
        };
        if !self.liquidations_history_in_flight.insert(key.clone()) {
            return;
        }
        let before_ms = self
            .liquidations
            .get(&key)
            .and_then(|v| v.first())
            .map(|l| l.ts_ms)
            .unwrap_or_else(|| chrono::Utc::now().timestamp_millis());
        let frame = proto::ClientFrame::HistoryPage {
            id: sub_id,
            before_ms,
            count: HISTORY_PAGE_SIZE,
        };
        let _ = self.to_ws.unbounded_send(frame);
    }

    /// Request older liquidation bars. Cursor = oldest held `open_time`.
    pub fn load_older_liquidation_bars(
        &mut self,
        symbol: &str,
        tf: Timeframe,
        _cx: &mut Context<Self>,
    ) {
        let key = LiquidationBarsSubKey::new(symbol, tf);
        let Some(sub_id) = self.liquidation_bars_sub_ids.get(&key).copied() else {
            return;
        };
        if !self.liquidation_bars_history_in_flight.insert(key.clone()) {
            return;
        }
        let before_ms = self
            .liquidation_bars
            .get(&key)
            .and_then(|bars| bars.keys().min().copied())
            .unwrap_or_else(|| chrono::Utc::now().timestamp_millis());
        let frame = proto::ClientFrame::HistoryPage {
            id: sub_id,
            before_ms,
            count: HISTORY_PAGE_SIZE,
        };
        let _ = self.to_ws.unbounded_send(frame);
    }

    // --- Internal: driven by the connection task ---------------------------

    /// Route an incoming server frame into per-subscription state.
    fn handle_server_frame(&mut self, frame: proto::ServerFrame, cx: &mut Context<Self>) {
        self.last_message_ms = Some(chrono::Utc::now().timestamp_millis());
        match frame {
            proto::ServerFrame::Snapshot { id, candles, server_v: _ } => {
                self.on_snapshot(id, candles, cx);
            }
            proto::ServerFrame::Tick { id, candle, is_closed, v: _ } => {
                self.on_tick(id, candle, is_closed, cx);
            }
            proto::ServerFrame::HistoryPage { id, candles } => {
                self.on_history_page(id, candles, cx);
            }
            proto::ServerFrame::TradeSnapshot { id, trades, server_v: _ } => {
                self.on_trade_snapshot(id, trades, cx);
            }
            proto::ServerFrame::TradeTick { id, trades, v: _ } => {
                self.on_trade_tick(id, trades, cx);
            }
            proto::ServerFrame::TradeHistoryPage { id, trades } => {
                self.on_trade_history_page(id, trades, cx);
            }
            proto::ServerFrame::FootprintSnapshot { id, cells, server_v: _ } => {
                self.on_footprint_snapshot(id, cells, cx);
            }
            proto::ServerFrame::FootprintUpdate { id, cells, v: _ } => {
                self.on_footprint_update(id, cells, cx);
            }
            proto::ServerFrame::FootprintHistoryPage { id, cells } => {
                self.on_footprint_history_page(id, cells, cx);
            }
            proto::ServerFrame::BookSnapshot { id, bids, asks, server_v: _ } => {
                self.on_book_snapshot(id, bids, asks, cx);
            }
            proto::ServerFrame::BookDelta { id, bids, asks, v: _ } => {
                self.on_book_delta(id, bids, asks, cx);
            }
            proto::ServerFrame::BookHistoryPage { id, snapshots } => {
                self.on_book_history_page(id, snapshots, cx);
            }
            proto::ServerFrame::LiquidationSnapshot {
                id,
                liquidations,
                server_v: _,
            } => {
                self.on_liquidation_snapshot(id, liquidations, cx);
            }
            proto::ServerFrame::LiquidationTick {
                id,
                liquidations,
                v: _,
            } => {
                self.on_liquidation_tick(id, liquidations, cx);
            }
            proto::ServerFrame::LiquidationHistoryPage { id, liquidations } => {
                self.on_liquidation_history_page(id, liquidations, cx);
            }
            proto::ServerFrame::LiquidationBarSnapshot { id, bars, server_v: _ } => {
                self.on_liquidation_bar_snapshot(id, bars, cx);
            }
            proto::ServerFrame::LiquidationBarUpdate { id, bars, v: _ } => {
                self.on_liquidation_bar_update(id, bars, cx);
            }
            proto::ServerFrame::LiquidationBarHistoryPage { id, bars } => {
                self.on_liquidation_bar_history_page(id, bars, cx);
            }
            proto::ServerFrame::Resnap { id } => {
                self.on_resnap(id, cx);
            }
            proto::ServerFrame::Status { state } => {
                self.set_conn_status(live_status_from_proto(state), cx);
            }
            proto::ServerFrame::Pong { .. } => {}
            proto::ServerFrame::Error { id, code, msg } => {
                log::warn!("server error: id={id:?} code={code} msg={msg}");
            }
        }
    }

    fn on_snapshot(
        &mut self,
        id: proto::SubId,
        candles: Vec<proto::Candle>,
        cx: &mut Context<Self>,
    ) {
        let Some(AnySubKey::Candles(key)) = self.by_id.get(&id).cloned() else {
            return;
        };
        let bars: Vec<Candle> = candles.into_iter().map(candle_from_proto).collect();
        self.candles.insert(key.clone(), bars);
        self.history_in_flight.remove(&key);
        cx.emit(KlineEvent::Resnap {
            symbol: key.symbol.clone().into(),
            tf: key.tf,
        });
    }

    fn on_tick(
        &mut self,
        id: proto::SubId,
        candle: proto::Candle,
        is_closed: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(AnySubKey::Candles(key)) = self.by_id.get(&id).cloned() else {
            return;
        };
        let c = candle_from_proto(candle);
        let bars = self.candles.entry(key.clone()).or_default();
        merge_or_append(bars, c.clone());
        cx.emit(KlineEvent::Tick {
            symbol: key.symbol.clone().into(),
            tf: key.tf,
            candle: c,
            is_closed,
        });
    }

    fn on_history_page(
        &mut self,
        id: proto::SubId,
        candles: Vec<proto::Candle>,
        cx: &mut Context<Self>,
    ) {
        let Some(AnySubKey::Candles(key)) = self.by_id.get(&id).cloned() else {
            return;
        };
        self.history_in_flight.remove(&key);
        if candles.is_empty() {
            cx.emit(KlineEvent::HistoryCapped {
                symbol: key.symbol.clone().into(),
                tf: key.tf,
            });
            return;
        }
        let existing = self.candles.entry(key.clone()).or_default();
        // Defensive: server returns `open_time < before_ms`, so a fresh
        // request can't overlap — but a HistoryPage that races a Snapshot /
        // Resnap (which replaces the buffer) can. Drop anything not strictly
        // older than the current leftmost bar.
        let cutoff = existing.first().map(|c| c.open_time);
        let mut prepend: Vec<Candle> = candles
            .into_iter()
            .map(candle_from_proto)
            .filter(|c| cutoff.map_or(true, |t| c.open_time < t))
            .collect();
        if prepend.is_empty() {
            return;
        }
        let added = prepend.len();
        let mut merged = Vec::with_capacity(prepend.len() + existing.len());
        merged.append(&mut prepend);
        merged.append(existing);
        *existing = merged;
        cx.emit(KlineEvent::Prepended {
            symbol: key.symbol.clone().into(),
            tf: key.tf,
            added,
        });
    }

    /// Server told us to reset this subscription's state. Clear the buffer
    /// and re-Subscribe (the v1 server's gateway exits its forwarder on
    /// Resnap; the resub recreates it server-side — Q12e).
    fn on_resnap(&mut self, id: proto::SubId, cx: &mut Context<Self>) {
        let Some(any) = self.by_id.get(&id).cloned() else {
            return;
        };
        match any {
            AnySubKey::Candles(key) => {
                self.candles.insert(key.clone(), Vec::new());
                self.history_in_flight.remove(&key);
                cx.emit(KlineEvent::Resnap {
                    symbol: key.symbol.clone().into(),
                    tf: key.tf,
                });
                let _ = self.to_ws.unbounded_send(proto::ClientFrame::Subscribe {
                    id,
                    symbol: key.symbol.clone(),
                    channel: proto::Channel::Candles { tf: proto_tf(key.tf) },
                });
            }
            AnySubKey::Trades(key) => {
                self.trades.insert(key.clone(), Vec::new());
                self.trade_history_in_flight.remove(&key);
                cx.emit(TradeEvent::Resnap {
                    symbol: key.symbol.clone().into(),
                });
                let _ = self.to_ws.unbounded_send(proto::ClientFrame::Subscribe {
                    id,
                    symbol: key.symbol.clone(),
                    channel: proto::Channel::Trades,
                });
            }
            AnySubKey::Footprint(key) => {
                let tf = key.tf;
                self.footprint.insert(key.clone(), HashMap::new());
                self.footprint_history_in_flight.remove(&key);
                cx.emit(FootprintEvent::Resnap {
                    symbol: key.symbol.clone().into(),
                    tf,
                });
                let _ = self.to_ws.unbounded_send(proto::ClientFrame::Subscribe {
                    id,
                    symbol: key.symbol.clone(),
                    channel: proto::Channel::Footprint {
                        tf: proto_tf(tf),
                        price_bucket: key.bucket(),
                    },
                });
            }
            AnySubKey::Book(key) => {
                self.book.insert(key.clone(), (Vec::new(), Vec::new()));
                self.book_history.insert(key.clone(), Vec::new());
                self.book_history_in_flight.remove(&key);
                cx.emit(BookEvent::Resnap {
                    symbol: key.symbol.clone().into(),
                });
                let _ = self.to_ws.unbounded_send(proto::ClientFrame::Subscribe {
                    id,
                    symbol: key.symbol.clone(),
                    channel: proto::Channel::Book { depth: key.depth },
                });
            }
            AnySubKey::Liquidations(key) => {
                self.liquidations.insert(key.clone(), Vec::new());
                self.liquidations_history_in_flight.remove(&key);
                cx.emit(LiquidationEvent::Resnap {
                    symbol: key.symbol.clone().into(),
                });
                let _ = self.to_ws.unbounded_send(proto::ClientFrame::Subscribe {
                    id,
                    symbol: key.symbol.clone(),
                    channel: proto::Channel::Liquidations,
                });
            }
            AnySubKey::LiquidationBars(key) => {
                let tf = key.tf;
                self.liquidation_bars.insert(key.clone(), HashMap::new());
                self.liquidation_bars_history_in_flight.remove(&key);
                cx.emit(LiquidationBarEvent::Resnap {
                    symbol: key.symbol.clone().into(),
                    tf,
                });
                let _ = self.to_ws.unbounded_send(proto::ClientFrame::Subscribe {
                    id,
                    symbol: key.symbol.clone(),
                    channel: proto::Channel::LiquidationBars { tf: proto_tf(tf) },
                });
            }
        }
    }

    // --- Trades handlers ---

    fn on_trade_snapshot(
        &mut self,
        id: proto::SubId,
        trades: Vec<proto::Trade>,
        cx: &mut Context<Self>,
    ) {
        let Some(AnySubKey::Trades(key)) = self.by_id.get(&id).cloned() else {
            return;
        };
        let mut domain: Vec<Trade> = trades.into_iter().map(trade_from_proto).collect();
        // Defensive: cap the snapshot too. The server's snapshot size is
        // bounded but staying under TRADES_BUFFER_CAP keeps invariants
        // consistent across snapshot + tick code paths.
        if domain.len() > TRADES_BUFFER_CAP {
            let drop_n = domain.len() - TRADES_BUFFER_CAP;
            domain.drain(0..drop_n);
        }
        self.trades.insert(key.clone(), domain.clone());
        self.trade_history_in_flight.remove(&key);
        cx.emit(TradeEvent::Snapshot {
            symbol: key.symbol.clone().into(),
            trades: domain,
        });
    }

    fn on_trade_tick(
        &mut self,
        id: proto::SubId,
        trades: Vec<proto::Trade>,
        cx: &mut Context<Self>,
    ) {
        let Some(AnySubKey::Trades(key)) = self.by_id.get(&id).cloned() else {
            return;
        };
        let domain: Vec<Trade> = trades.into_iter().map(trade_from_proto).collect();
        let buf = self.trades.entry(key.clone()).or_default();
        for t in &domain {
            buf.push(t.clone());
        }
        // Drop oldest to keep the live tape from growing unbounded over
        // long sessions. `load_older_trades` skips this path on purpose.
        if buf.len() > TRADES_BUFFER_CAP {
            let drop_n = buf.len() - TRADES_BUFFER_CAP;
            buf.drain(0..drop_n);
        }
        cx.emit(TradeEvent::Tick {
            symbol: key.symbol.clone().into(),
            trades: domain,
        });
    }

    fn on_trade_history_page(
        &mut self,
        id: proto::SubId,
        trades: Vec<proto::Trade>,
        cx: &mut Context<Self>,
    ) {
        let Some(AnySubKey::Trades(key)) = self.by_id.get(&id).cloned() else {
            return;
        };
        self.trade_history_in_flight.remove(&key);
        if trades.is_empty() {
            cx.emit(TradeEvent::HistoryCapped {
                symbol: key.symbol.clone().into(),
            });
            return;
        }
        let existing = self.trades.entry(key.clone()).or_default();
        let cutoff = existing.first().map(|t| t.ts_ms);
        let mut prepend: Vec<Trade> = trades
            .into_iter()
            .map(trade_from_proto)
            .filter(|t| cutoff.map_or(true, |c| t.ts_ms < c))
            .collect();
        if prepend.is_empty() {
            return;
        }
        let added = prepend.len();
        let mut merged = Vec::with_capacity(prepend.len() + existing.len());
        merged.append(&mut prepend);
        merged.append(existing);
        *existing = merged;
        cx.emit(TradeEvent::Prepended {
            symbol: key.symbol.clone().into(),
            added,
        });
    }

    // --- Footprint handlers ---

    fn on_footprint_snapshot(
        &mut self,
        id: proto::SubId,
        cells: Vec<proto::FootprintCell>,
        cx: &mut Context<Self>,
    ) {
        let Some(AnySubKey::Footprint(key)) = self.by_id.get(&id).cloned() else {
            return;
        };
        let domain: Vec<FootprintCell> = cells.into_iter().map(footprint_from_proto).collect();
        let mut map: HashMap<(i64, u64), FootprintCell> = HashMap::new();
        for c in &domain {
            map.insert((c.open_time, c.price_bucket_low.to_bits()), c.clone());
        }
        self.footprint.insert(key.clone(), map);
        self.footprint_history_in_flight.remove(&key);
        cx.emit(FootprintEvent::Snapshot {
            symbol: key.symbol.clone().into(),
            tf: key.tf,
            cells: domain,
        });
    }

    fn on_footprint_update(
        &mut self,
        id: proto::SubId,
        cells: Vec<proto::FootprintCell>,
        cx: &mut Context<Self>,
    ) {
        let Some(AnySubKey::Footprint(key)) = self.by_id.get(&id).cloned() else {
            return;
        };
        let domain: Vec<FootprintCell> = cells.into_iter().map(footprint_from_proto).collect();
        let map = self.footprint.entry(key.clone()).or_default();
        for c in &domain {
            map.insert((c.open_time, c.price_bucket_low.to_bits()), c.clone());
        }
        cx.emit(FootprintEvent::Update {
            symbol: key.symbol.clone().into(),
            tf: key.tf,
            cells: domain,
        });
    }

    fn on_footprint_history_page(
        &mut self,
        id: proto::SubId,
        cells: Vec<proto::FootprintCell>,
        cx: &mut Context<Self>,
    ) {
        let Some(AnySubKey::Footprint(key)) = self.by_id.get(&id).cloned() else {
            return;
        };
        self.footprint_history_in_flight.remove(&key);
        if cells.is_empty() {
            cx.emit(FootprintEvent::HistoryCapped {
                symbol: key.symbol.clone().into(),
                tf: key.tf,
            });
            return;
        }
        let domain: Vec<FootprintCell> = cells.into_iter().map(footprint_from_proto).collect();
        let map = self.footprint.entry(key.clone()).or_default();
        let added = domain.len();
        for c in &domain {
            map.insert((c.open_time, c.price_bucket_low.to_bits()), c.clone());
        }
        cx.emit(FootprintEvent::Prepended {
            symbol: key.symbol.clone().into(),
            tf: key.tf,
            added,
        });
    }

    // --- Book handlers ---

    fn on_book_snapshot(
        &mut self,
        id: proto::SubId,
        bids: Vec<proto::BookLevel>,
        asks: Vec<proto::BookLevel>,
        cx: &mut Context<Self>,
    ) {
        let Some(AnySubKey::Book(key)) = self.by_id.get(&id).cloned() else {
            return;
        };
        let bids: Vec<BookLevel> = bids.into_iter().map(book_level_from_proto).collect();
        let asks: Vec<BookLevel> = asks.into_iter().map(book_level_from_proto).collect();
        self.book.insert(key.clone(), (bids.clone(), asks.clone()));
        self.book_history_in_flight.remove(&key);
        cx.emit(BookEvent::Snapshot {
            symbol: key.symbol.clone().into(),
            bids,
            asks,
        });
    }

    fn on_book_delta(
        &mut self,
        id: proto::SubId,
        bids: Vec<proto::BookLevel>,
        asks: Vec<proto::BookLevel>,
        cx: &mut Context<Self>,
    ) {
        let Some(AnySubKey::Book(key)) = self.by_id.get(&id).cloned() else {
            return;
        };
        let bid_dom: Vec<BookLevel> = bids.into_iter().map(book_level_from_proto).collect();
        let ask_dom: Vec<BookLevel> = asks.into_iter().map(book_level_from_proto).collect();
        if let Some((cur_bids, cur_asks)) = self.book.get_mut(&key) {
            apply_levels(cur_bids, &bid_dom, true);
            apply_levels(cur_asks, &ask_dom, false);
        }
        cx.emit(BookEvent::Delta {
            symbol: key.symbol.clone().into(),
            bids: bid_dom,
            asks: ask_dom,
        });
    }

    fn on_book_history_page(
        &mut self,
        id: proto::SubId,
        snapshots: Vec<proto::BookSnapshotEntry>,
        cx: &mut Context<Self>,
    ) {
        let Some(AnySubKey::Book(key)) = self.by_id.get(&id).cloned() else {
            return;
        };
        self.book_history_in_flight.remove(&key);
        if snapshots.is_empty() {
            cx.emit(BookEvent::HistoryCapped {
                symbol: key.symbol.clone().into(),
            });
            return;
        }
        let existing = self.book_history.entry(key.clone()).or_default();
        let cutoff = existing.first().map(|s| s.ts_ms);
        let mut prepend: Vec<BookSnapshotEntry> = snapshots
            .into_iter()
            .map(book_snapshot_entry_from_proto)
            .filter(|s| cutoff.map_or(true, |c| s.ts_ms < c))
            .collect();
        if prepend.is_empty() {
            return;
        }
        let added = prepend.len();
        let mut merged = Vec::with_capacity(prepend.len() + existing.len());
        merged.append(&mut prepend);
        merged.append(existing);
        *existing = merged;
        cx.emit(BookEvent::HistoryPrepended {
            symbol: key.symbol.clone().into(),
            added,
        });
    }

    // --- Liquidations handlers ---

    fn on_liquidation_snapshot(
        &mut self,
        id: proto::SubId,
        liquidations: Vec<proto::Liquidation>,
        cx: &mut Context<Self>,
    ) {
        let Some(AnySubKey::Liquidations(key)) = self.by_id.get(&id).cloned() else {
            return;
        };
        let mut domain: Vec<Liquidation> =
            liquidations.into_iter().map(liquidation_from_proto).collect();
        if domain.len() > LIQUIDATIONS_BUFFER_CAP {
            let drop_n = domain.len() - LIQUIDATIONS_BUFFER_CAP;
            domain.drain(0..drop_n);
        }
        self.liquidations.insert(key.clone(), domain.clone());
        self.liquidations_history_in_flight.remove(&key);
        cx.emit(LiquidationEvent::Snapshot {
            symbol: key.symbol.clone().into(),
            liquidations: domain,
        });
    }

    fn on_liquidation_tick(
        &mut self,
        id: proto::SubId,
        liquidations: Vec<proto::Liquidation>,
        cx: &mut Context<Self>,
    ) {
        let Some(AnySubKey::Liquidations(key)) = self.by_id.get(&id).cloned() else {
            return;
        };
        let domain: Vec<Liquidation> =
            liquidations.into_iter().map(liquidation_from_proto).collect();
        let buf = self.liquidations.entry(key.clone()).or_default();
        for l in &domain {
            buf.push(l.clone());
        }
        if buf.len() > LIQUIDATIONS_BUFFER_CAP {
            let drop_n = buf.len() - LIQUIDATIONS_BUFFER_CAP;
            buf.drain(0..drop_n);
        }
        cx.emit(LiquidationEvent::Tick {
            symbol: key.symbol.clone().into(),
            liquidations: domain,
        });
    }

    fn on_liquidation_history_page(
        &mut self,
        id: proto::SubId,
        liquidations: Vec<proto::Liquidation>,
        cx: &mut Context<Self>,
    ) {
        let Some(AnySubKey::Liquidations(key)) = self.by_id.get(&id).cloned() else {
            return;
        };
        self.liquidations_history_in_flight.remove(&key);
        if liquidations.is_empty() {
            cx.emit(LiquidationEvent::HistoryCapped {
                symbol: key.symbol.clone().into(),
            });
            return;
        }
        let existing = self.liquidations.entry(key.clone()).or_default();
        let cutoff = existing.first().map(|l| l.ts_ms);
        let mut prepend: Vec<Liquidation> = liquidations
            .into_iter()
            .map(liquidation_from_proto)
            .filter(|l| cutoff.map_or(true, |c| l.ts_ms < c))
            .collect();
        if prepend.is_empty() {
            return;
        }
        let added = prepend.len();
        let mut merged = Vec::with_capacity(prepend.len() + existing.len());
        merged.append(&mut prepend);
        merged.append(existing);
        *existing = merged;
        cx.emit(LiquidationEvent::Prepended {
            symbol: key.symbol.clone().into(),
            added,
        });
    }

    fn on_liquidation_bar_snapshot(
        &mut self,
        id: proto::SubId,
        bars: Vec<proto::LiquidationBar>,
        cx: &mut Context<Self>,
    ) {
        let Some(AnySubKey::LiquidationBars(key)) = self.by_id.get(&id).cloned() else {
            return;
        };
        let domain: Vec<LiquidationBar> =
            bars.into_iter().map(liquidation_bar_from_proto).collect();
        let mut map: HashMap<i64, LiquidationBar> = HashMap::new();
        for b in &domain {
            map.insert(b.open_time, b.clone());
        }
        self.liquidation_bars.insert(key.clone(), map);
        self.liquidation_bars_history_in_flight.remove(&key);
        cx.emit(LiquidationBarEvent::Snapshot {
            symbol: key.symbol.clone().into(),
            tf: key.tf,
            bars: domain,
        });
    }

    fn on_liquidation_bar_update(
        &mut self,
        id: proto::SubId,
        bars: Vec<proto::LiquidationBar>,
        cx: &mut Context<Self>,
    ) {
        let Some(AnySubKey::LiquidationBars(key)) = self.by_id.get(&id).cloned() else {
            return;
        };
        let domain: Vec<LiquidationBar> =
            bars.into_iter().map(liquidation_bar_from_proto).collect();
        let map = self.liquidation_bars.entry(key.clone()).or_default();
        for b in &domain {
            map.insert(b.open_time, b.clone());
        }
        cx.emit(LiquidationBarEvent::Update {
            symbol: key.symbol.clone().into(),
            tf: key.tf,
            bars: domain,
        });
    }

    fn on_liquidation_bar_history_page(
        &mut self,
        id: proto::SubId,
        bars: Vec<proto::LiquidationBar>,
        cx: &mut Context<Self>,
    ) {
        let Some(AnySubKey::LiquidationBars(key)) = self.by_id.get(&id).cloned() else {
            return;
        };
        self.liquidation_bars_history_in_flight.remove(&key);
        if bars.is_empty() {
            cx.emit(LiquidationBarEvent::HistoryCapped {
                symbol: key.symbol.clone().into(),
                tf: key.tf,
            });
            return;
        }
        let domain: Vec<LiquidationBar> =
            bars.into_iter().map(liquidation_bar_from_proto).collect();
        let map = self.liquidation_bars.entry(key.clone()).or_default();
        let mut added = 0;
        for b in &domain {
            // Server emits one row per bar in the covered range; insert as
            // new only when we don't already have that open_time, otherwise
            // a history page for an already-visible bar would clobber the
            // live cumulative with a stale (snapshot-time) total.
            if !map.contains_key(&b.open_time) {
                map.insert(b.open_time, b.clone());
                added += 1;
            }
        }
        if added == 0 {
            return;
        }
        cx.emit(LiquidationBarEvent::Prepended {
            symbol: key.symbol.clone().into(),
            tf: key.tf,
            added,
        });
    }

    fn set_conn_status(&mut self, status: LiveStatus, cx: &mut Context<Self>) {
        if self.conn_status == status {
            return;
        }
        self.conn_status = status.clone();
        let keys: Vec<SubKey> = self.refcounts.keys().cloned().collect();
        for key in keys {
            self.statuses.insert(key.clone(), status.clone());
            cx.emit(KlineEvent::StatusChanged {
                symbol: key.symbol.clone().into(),
                tf: key.tf,
                status: status.clone(),
            });
        }
    }

    /// Push a `Subscribe` for every active subscription across all channels.
    /// Used by the driver task on connect (covers boot + every reconnect).
    fn resubscribe_all(&mut self) {
        let candle_subs: Vec<(SubKey, proto::SubId)> = self
            .sub_ids
            .iter()
            .map(|(k, id)| (k.clone(), *id))
            .collect();
        for (key, sub_id) in candle_subs {
            let _ = self.to_ws.unbounded_send(proto::ClientFrame::Subscribe {
                id: sub_id,
                symbol: key.symbol.clone(),
                channel: proto::Channel::Candles { tf: proto_tf(key.tf) },
            });
        }
        let trade_subs: Vec<(TradeSubKey, proto::SubId)> = self
            .trade_sub_ids
            .iter()
            .map(|(k, id)| (k.clone(), *id))
            .collect();
        for (key, sub_id) in trade_subs {
            let _ = self.to_ws.unbounded_send(proto::ClientFrame::Subscribe {
                id: sub_id,
                symbol: key.symbol.clone(),
                channel: proto::Channel::Trades,
            });
        }
        let footprint_subs: Vec<(FootprintSubKey, proto::SubId)> = self
            .footprint_sub_ids
            .iter()
            .map(|(k, id)| (k.clone(), *id))
            .collect();
        for (key, sub_id) in footprint_subs {
            let _ = self.to_ws.unbounded_send(proto::ClientFrame::Subscribe {
                id: sub_id,
                symbol: key.symbol.clone(),
                channel: proto::Channel::Footprint {
                    tf: proto_tf(key.tf),
                    price_bucket: key.bucket(),
                },
            });
        }
        let book_subs: Vec<(BookSubKey, proto::SubId)> = self
            .book_sub_ids
            .iter()
            .map(|(k, id)| (k.clone(), *id))
            .collect();
        for (key, sub_id) in book_subs {
            let _ = self.to_ws.unbounded_send(proto::ClientFrame::Subscribe {
                id: sub_id,
                symbol: key.symbol.clone(),
                channel: proto::Channel::Book { depth: key.depth },
            });
        }
        let liquidation_subs: Vec<(LiquidationsSubKey, proto::SubId)> = self
            .liquidations_sub_ids
            .iter()
            .map(|(k, id)| (k.clone(), *id))
            .collect();
        for (key, sub_id) in liquidation_subs {
            let _ = self.to_ws.unbounded_send(proto::ClientFrame::Subscribe {
                id: sub_id,
                symbol: key.symbol.clone(),
                channel: proto::Channel::Liquidations,
            });
        }
        let liquidation_bar_subs: Vec<(LiquidationBarsSubKey, proto::SubId)> = self
            .liquidation_bars_sub_ids
            .iter()
            .map(|(k, id)| (k.clone(), *id))
            .collect();
        for (key, sub_id) in liquidation_bar_subs {
            let _ = self.to_ws.unbounded_send(proto::ClientFrame::Subscribe {
                id: sub_id,
                symbol: key.symbol.clone(),
                channel: proto::Channel::LiquidationBars {
                    tf: proto_tf(key.tf),
                },
            });
        }
    }

    /// Decrement the refcount for `key`. On 0 → 0, send `Unsubscribe` and
    /// drop the per-sub state. Called from the release task in `init`.
    fn release_one(&mut self, key: SubKey, _cx: &mut Context<Self>) {
        let zero = match self.refcounts.get_mut(&key) {
            Some(c) => {
                *c = c.saturating_sub(1);
                *c == 0
            }
            None => return,
        };
        if zero {
            self.refcounts.remove(&key);
            if let Some(sub_id) = self.sub_ids.remove(&key) {
                self.by_id.remove(&sub_id);
                let _ = self
                    .to_ws
                    .unbounded_send(proto::ClientFrame::Unsubscribe { id: sub_id });
            }
            self.candles.remove(&key);
            self.statuses.remove(&key);
            self.history_in_flight.remove(&key);
        }
    }

    fn release_one_trades(&mut self, key: TradeSubKey, _cx: &mut Context<Self>) {
        let zero = match self.trade_refcounts.get_mut(&key) {
            Some(c) => {
                *c = c.saturating_sub(1);
                *c == 0
            }
            None => return,
        };
        if zero {
            self.trade_refcounts.remove(&key);
            if let Some(sub_id) = self.trade_sub_ids.remove(&key) {
                self.by_id.remove(&sub_id);
                let _ = self
                    .to_ws
                    .unbounded_send(proto::ClientFrame::Unsubscribe { id: sub_id });
            }
            self.trades.remove(&key);
            self.trade_history_in_flight.remove(&key);
        }
    }

    fn release_one_footprint(&mut self, key: FootprintSubKey, _cx: &mut Context<Self>) {
        let zero = match self.footprint_refcounts.get_mut(&key) {
            Some(c) => {
                *c = c.saturating_sub(1);
                *c == 0
            }
            None => return,
        };
        if zero {
            self.footprint_refcounts.remove(&key);
            if let Some(sub_id) = self.footprint_sub_ids.remove(&key) {
                self.by_id.remove(&sub_id);
                let _ = self
                    .to_ws
                    .unbounded_send(proto::ClientFrame::Unsubscribe { id: sub_id });
            }
            self.footprint.remove(&key);
            self.footprint_history_in_flight.remove(&key);
        }
    }

    fn release_one_book(&mut self, key: BookSubKey, _cx: &mut Context<Self>) {
        let zero = match self.book_refcounts.get_mut(&key) {
            Some(c) => {
                *c = c.saturating_sub(1);
                *c == 0
            }
            None => return,
        };
        if zero {
            self.book_refcounts.remove(&key);
            if let Some(sub_id) = self.book_sub_ids.remove(&key) {
                self.by_id.remove(&sub_id);
                let _ = self
                    .to_ws
                    .unbounded_send(proto::ClientFrame::Unsubscribe { id: sub_id });
            }
            self.book.remove(&key);
            self.book_history.remove(&key);
            self.book_history_in_flight.remove(&key);
        }
    }

    fn release_one_liquidations(
        &mut self,
        key: LiquidationsSubKey,
        _cx: &mut Context<Self>,
    ) {
        let zero = match self.liquidations_refcounts.get_mut(&key) {
            Some(c) => {
                *c = c.saturating_sub(1);
                *c == 0
            }
            None => return,
        };
        if zero {
            self.liquidations_refcounts.remove(&key);
            if let Some(sub_id) = self.liquidations_sub_ids.remove(&key) {
                self.by_id.remove(&sub_id);
                let _ = self
                    .to_ws
                    .unbounded_send(proto::ClientFrame::Unsubscribe { id: sub_id });
            }
            self.liquidations.remove(&key);
            self.liquidations_history_in_flight.remove(&key);
        }
    }

    fn release_one_liquidation_bars(
        &mut self,
        key: LiquidationBarsSubKey,
        _cx: &mut Context<Self>,
    ) {
        let zero = match self.liquidation_bars_refcounts.get_mut(&key) {
            Some(c) => {
                *c = c.saturating_sub(1);
                *c == 0
            }
            None => return,
        };
        if zero {
            self.liquidation_bars_refcounts.remove(&key);
            if let Some(sub_id) = self.liquidation_bars_sub_ids.remove(&key) {
                self.by_id.remove(&sub_id);
                let _ = self
                    .to_ws
                    .unbounded_send(proto::ClientFrame::Unsubscribe { id: sub_id });
            }
            self.liquidation_bars.remove(&key);
            self.liquidation_bars_history_in_flight.remove(&key);
        }
    }
}

#[derive(Clone)]
pub struct MarketDataServiceHandle(pub Entity<MarketDataService>);
impl Global for MarketDataServiceHandle {}

pub fn init(cx: &mut App) {
    let entity = cx.new(MarketDataService::new);
    cx.set_global(MarketDataServiceHandle(entity));
}

/// Handle to a live subscription. The keyed slot stays registered as long
/// as at least one handle exists; on drop the key gets sent to the release
/// channel and the service decrements the refcount for whichever channel
/// this handle belongs to.
pub struct SubscriptionHandle {
    key: ReleaseKey,
    release_tx: UnboundedSender<ReleaseKey>,
}

impl Drop for SubscriptionHandle {
    fn drop(&mut self) {
        let _ = self.release_tx.unbounded_send(self.key.clone());
    }
}

// --- Connection driver ------------------------------------------------------

async fn run_connection(
    this: WeakEntity<MarketDataService>,
    mut outbound_rx: UnboundedReceiver<proto::ClientFrame>,
    cx: &mut gpui::AsyncApp,
) {
    let mut attempts: u32 = 0;
    loop {
        let status = if attempts == 0 {
            LiveStatus::Connecting
        } else {
            LiveStatus::Reconnecting { attempts }
        };
        // Defer to a fresh tick — see the deeper comment in `pump`.
        defer_update(&this, cx, move |s, cx| s.set_conn_status(status, cx));

        let url = server_ws_url();
        match WsMeta::connect(&url, None).await {
            Ok((_meta, stream)) => {
                attempts = 0;
                log::info!("ws connected to {url}");
                // Drop any stale Subscribes that piled up during the outage;
                // resubscribe_all re-issues the canonical set.
                drain_pending(&mut outbound_rx);
                defer_update(&this, cx, |s, cx| {
                    s.set_conn_status(LiveStatus::Connected, cx);
                    s.resubscribe_all();
                });

                pump(stream, &mut outbound_rx, &this, cx).await;
                log::info!("ws disconnected; will reconnect");
            }
            Err(e) => {
                log::warn!("ws connect failed: {e:?}");
            }
        }

        attempts = attempts.saturating_add(1);
        let backoff = backoff_for(attempts);
        cx.background_executor().timer(backoff).await;
    }
}

/// Spawn a fresh tick that applies `f` to the service entity. Used by the
/// connection driver and pump to avoid running entity.update synchronously
/// inside the same executor task that just resumed from a WS poll —
/// synchronous emit/notify chains during gpui_web's animation-frame borrow
/// is the panic shape we're avoiding (see `pump` comment).
fn defer_update<F>(this: &WeakEntity<MarketDataService>, cx: &mut gpui::AsyncApp, f: F)
where
    F: FnOnce(&mut MarketDataService, &mut Context<MarketDataService>) + 'static,
{
    let this = this.clone();
    cx.spawn(async move |cx| {
        let _ = this.update(cx, f);
    })
    .detach();
}

// `try_next` on `UnboundedReceiver` is deprecation-flagged in some futures
// versions in favor of `try_recv`, but the replacement is not yet on the
// `mpsc::UnboundedReceiver` flavor we depend on. Silence the warning until
// the dep is bumped.
#[allow(deprecated)]
fn drain_pending(rx: &mut UnboundedReceiver<proto::ClientFrame>) {
    while let Ok(Some(_)) = rx.try_next() {}
}

fn backoff_for(attempts: u32) -> Duration {
    // 1s, 2s, 4s, 8s, 16s, 30s, 30s, ...
    let shift = attempts.min(5);
    let raw = RECONNECT_MIN * (1u32 << shift);
    if raw > RECONNECT_MAX { RECONNECT_MAX } else { raw }
}

async fn pump(
    stream: ws_stream_wasm::WsStream,
    outbound_rx: &mut UnboundedReceiver<proto::ClientFrame>,
    this: &WeakEntity<MarketDataService>,
    cx: &mut gpui::AsyncApp,
) {
    use futures::future::FutureExt;
    let (mut sink, mut input) = stream.split();
    loop {
        futures::select! {
            inc = input.next().fuse() => {
                match inc {
                    Some(WsMessage::Text(txt)) => {
                        match serde_json::from_str::<proto::ServerFrame>(&txt) {
                            Ok(frame) => {
                                // Defer entity.update to a fresh executor
                                // tick. If we apply the frame synchronously
                                // here, the emit() → subscriber callbacks
                                // chain (chart panel cx.notify(), bottom-bar
                                // status repaint) runs while gpui_web's
                                // request_frame may still hold callbacks
                                // borrowed from the current animation frame.
                                // A pointerleave that fires under that
                                // borrow panics ("RefCell already borrowed"
                                // at gpui_web/src/events.rs:512). Yielding
                                // here lets the in-flight RAF release first.
                                defer_update(this, cx, move |s, cx| {
                                    s.handle_server_frame(frame, cx)
                                });
                            }
                            Err(e) => {
                                log::warn!("decode server frame: {e:?}");
                            }
                        }
                    }
                    Some(WsMessage::Binary(_)) => { /* server only sends text */ }
                    None => return,
                }
            }
            out = outbound_rx.next().fuse() => {
                match out {
                    Some(frame) => {
                        match serde_json::to_string(&frame) {
                            Ok(json) => {
                                if sink.send(WsMessage::Text(json)).await.is_err() {
                                    return;
                                }
                            }
                            Err(e) => log::warn!("encode client frame: {e:?}"),
                        }
                    }
                    None => return, // service dropped
                }
            }
        }
    }
}

// --- Type conversions to/from the protocol crate ----------------------------

fn proto_tf(tf: Timeframe) -> proto::Timeframe {
    match tf {
        Timeframe::S1 => proto::Timeframe::S1,
        Timeframe::S5 => proto::Timeframe::S5,
        Timeframe::M1 => proto::Timeframe::M1,
        Timeframe::M5 => proto::Timeframe::M5,
        Timeframe::M15 => proto::Timeframe::M15,
        Timeframe::M30 => proto::Timeframe::M30,
        Timeframe::H1 => proto::Timeframe::H1,
        Timeframe::H2 => proto::Timeframe::H2,
        Timeframe::H4 => proto::Timeframe::H4,
        Timeframe::H6 => proto::Timeframe::H6,
        Timeframe::D1 => proto::Timeframe::D1,
    }
}

fn live_status_from_proto(s: proto::LiveStatus) -> LiveStatus {
    match s {
        proto::LiveStatus::Connecting => LiveStatus::Connecting,
        proto::LiveStatus::Connected => LiveStatus::Connected,
        proto::LiveStatus::Reconnecting { attempts } => LiveStatus::Reconnecting { attempts },
    }
}

fn candle_from_proto(c: proto::Candle) -> Candle {
    // Per-bar VWAP = quote_volume / base_volume. When the wire ships a non-
    // zero base volume and a quote-volume column, the ratio is the bar's
    // true volume-weighted price — what Anchored VWAP and similar indicators
    // accumulate against. Zero-volume bars get `None` so the painter skips
    // them rather than drawing a flat 0-line.
    let vwap = c.quote_volume.and_then(|qv| {
        if c.volume > 0.0 {
            Some(qv / c.volume)
        } else {
            None
        }
    });
    Candle::new_full(
        c.open_time,
        c.close_time,
        c.open,
        c.high,
        c.low,
        c.close,
        c.volume,
        vwap,
        c.trades,
        c.taker_buy_vol,
    )
}

/// Merge a new candle into the trailing edge of `bars`. Same `open_time`
/// → overwrite; older `open_time` → drop (out-of-order tick).
fn merge_or_append(bars: &mut Vec<Candle>, c: Candle) {
    if let Some(last) = bars.last_mut() {
        if last.open_time == c.open_time {
            *last = c;
            return;
        }
        if c.open_time < last.open_time {
            return;
        }
    }
    bars.push(c);
}

fn trade_from_proto(t: proto::Trade) -> Trade {
    Trade {
        ts_ms: t.ts_ms,
        agg_id: t.agg_id,
        price: t.price,
        qty: t.qty,
        is_buyer_maker: t.is_buyer_maker,
    }
}

fn footprint_from_proto(c: proto::FootprintCell) -> FootprintCell {
    FootprintCell {
        open_time: c.open_time,
        price_bucket_low: c.price_bucket_low,
        bid_vol: c.bid_vol,
        ask_vol: c.ask_vol,
    }
}

fn book_level_from_proto(l: proto::BookLevel) -> BookLevel {
    BookLevel {
        price: l.price,
        size: l.size,
    }
}

fn book_snapshot_entry_from_proto(e: proto::BookSnapshotEntry) -> BookSnapshotEntry {
    BookSnapshotEntry {
        ts_ms: e.ts_ms,
        bids: e.bids.into_iter().map(book_level_from_proto).collect(),
        asks: e.asks.into_iter().map(book_level_from_proto).collect(),
    }
}

fn liquidation_from_proto(l: proto::Liquidation) -> Liquidation {
    Liquidation {
        ts_ms: l.ts_ms,
        side: match l.side {
            proto::LiquidationSide::Long => LiquidationSide::Long,
            proto::LiquidationSide::Short => LiquidationSide::Short,
        },
        price: l.price,
        qty: l.qty,
        quote_qty: l.quote_qty,
    }
}

fn liquidation_bar_from_proto(b: proto::LiquidationBar) -> LiquidationBar {
    LiquidationBar {
        open_time: b.open_time,
        long_qty: b.long_qty,
        long_quote_qty: b.long_quote_qty,
        short_qty: b.short_qty,
        short_quote_qty: b.short_quote_qty,
    }
}

/// Apply a delta batch to a sorted book side. `is_bids = true` keeps the
/// side sorted descending (best-first); `false` keeps it ascending. A delta
/// level with `size == 0` removes; otherwise overwrites.
///
/// `current` is small and bounded (top-N from the subscription), so a
/// linear scan per level is fine — sub-microsecond at depth=50.
fn apply_levels(current: &mut Vec<BookLevel>, deltas: &[BookLevel], is_bids: bool) {
    for d in deltas {
        // Linear find: prices are exact f64 values that round-trip from the
        // server, so equality matches when the level was previously sent.
        let pos = current.iter().position(|l| l.price.to_bits() == d.price.to_bits());
        match pos {
            Some(i) if d.size <= 0.0 => {
                current.remove(i);
            }
            Some(i) => {
                current[i].size = d.size;
            }
            None if d.size > 0.0 => {
                current.push(BookLevel {
                    price: d.price,
                    size: d.size,
                });
            }
            None => {}
        }
    }
    // Re-sort: best-first.
    if is_bids {
        current.sort_by(|a, b| {
            b.price
                .partial_cmp(&a.price)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    } else {
        current.sort_by(|a, b| {
            a.price
                .partial_cmp(&b.price)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    }
}
