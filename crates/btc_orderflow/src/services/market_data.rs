//! Live market-data service backed by a WebSocket to `btc_orderflow_server`.
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

use std::collections::HashMap;
use std::time::Duration;

use btc_orderflow_protocol as proto;
use chrono::{Local, TimeZone as _};
use futures::{
    SinkExt, StreamExt,
    channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded},
};
use gpui::{App, AppContext as _, Context, Entity, EventEmitter, Global, SharedString};
use ws_stream_wasm::{WsMessage, WsMeta};

/// WS endpoint of the local server. Hardcoded for v1 (Q13d). Promote to a
/// build-time env var when the cloud-server path lands.
const SERVER_WS_URL: &str = "ws://127.0.0.1:8787/ws";

/// Page size for `HistoryPage` requests (Q9c).
const HISTORY_PAGE_SIZE: u32 = 500;

const RECONNECT_MIN: Duration = Duration::from_secs(1);
const RECONNECT_MAX: Duration = Duration::from_secs(30);

/// A chart timeframe.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Timeframe {
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
    pub const ALL: [Timeframe; 9] = [
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

/// Trading session filter. Kept on the API for compatibility with the chart
/// panel's selector; for crypto both variants are effectively the same.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Session {
    Regular,
    Extended,
}

pub const DEFAULT_SESSION: Session = Session::Regular;

impl Session {
    pub const ALL: [Session; 2] = [Session::Regular, Session::Extended];

    pub fn as_str(self) -> &'static str {
        match self {
            Session::Regular => "regular",
            Session::Extended => "extended",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Session::Regular => "RTH",
            Session::Extended => "ETH",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        Session::ALL.into_iter().find(|sn| sn.as_str() == s)
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
        Self::new_full(open_time, close_time, open, high, low, close, volume, None, None)
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
        }
    }
}

/// Whether a display symbol has a live feed. The stub answers `false` for
/// everything, which lets the chart's fallback paths render without ever
/// expecting WS data.
pub fn is_live(_display_symbol: &str) -> bool {
    false
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
        session: Session,
        candle: Candle,
        is_closed: bool,
    },
    Resnap {
        symbol: SharedString,
        tf: Timeframe,
        session: Session,
    },
    Prepended {
        symbol: SharedString,
        tf: Timeframe,
        session: Session,
        added: usize,
    },
    HistoryCapped {
        symbol: SharedString,
        tf: Timeframe,
        session: Session,
    },
    StatusChanged {
        symbol: SharedString,
        tf: Timeframe,
        session: Session,
        status: LiveStatus,
    },
}

#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) struct SubKey {
    pub(crate) symbol: String,
    pub(crate) tf: Timeframe,
    pub(crate) session: Session,
}

impl SubKey {
    fn new(symbol: &str, tf: Timeframe, session: Session) -> Self {
        Self {
            symbol: symbol.to_string(),
            tf,
            session,
        }
    }
}

pub struct MarketDataService {
    candles: HashMap<SubKey, Vec<Candle>>,
    statuses: HashMap<SubKey, LiveStatus>,

    /// Active subscriptions keyed both ways for fast lookup. `sub_ids[k]`
    /// is the id we use on the wire; `by_id[id]` is the inverse so incoming
    /// frames can route to the right SubKey.
    sub_ids: HashMap<SubKey, proto::SubId>,
    by_id: HashMap<proto::SubId, SubKey>,
    refcounts: HashMap<SubKey, usize>,
    next_sub_id: u32,

    /// Outbound queue drained by the connection driver task. Sends from
    /// here are non-blocking; while disconnected they buffer until the
    /// driver reconnects, at which point a `resubscribe_all` flushes any
    /// stale state with the canonical set.
    to_ws: UnboundedSender<proto::ClientFrame>,

    /// Receiver for `SubscriptionHandle::Drop` notifications. The release
    /// task in `init` drains this and calls `release_one` per key.
    release_tx: UnboundedSender<SubKey>,

    /// Taken by `init()` before spawning the driver/release tasks; `None`
    /// thereafter. Stored on the struct so `new()` can return both the
    /// service and the corresponding receivers without a side channel.
    pending_outbound_rx: Option<UnboundedReceiver<proto::ClientFrame>>,
    pending_release_rx: Option<UnboundedReceiver<SubKey>>,

    conn_status: LiveStatus,
    last_message_ms: Option<i64>,
}

impl EventEmitter<KlineEvent> for MarketDataService {}

impl MarketDataService {
    pub fn new(_cx: &mut Context<Self>) -> Self {
        let (release_tx, release_rx) = unbounded::<SubKey>();
        let (to_ws, outbound_rx) = unbounded::<proto::ClientFrame>();
        Self {
            candles: HashMap::new(),
            statuses: HashMap::new(),
            sub_ids: HashMap::new(),
            by_id: HashMap::new(),
            refcounts: HashMap::new(),
            next_sub_id: 0,
            to_ws,
            release_tx,
            pending_outbound_rx: Some(outbound_rx),
            pending_release_rx: Some(release_rx),
            conn_status: LiveStatus::Connecting,
            last_message_ms: None,
        }
    }

    /// Refcounted subscribe. The first `ensure` for a `SubKey` allocates a
    /// `SubId` and pushes a `Subscribe` frame; subsequent ensures just bump
    /// the count.
    pub fn ensure(
        &mut self,
        symbol: &str,
        tf: Timeframe,
        session: Session,
        cx: &mut Context<Self>,
    ) -> SubscriptionHandle {
        let key = SubKey::new(symbol, tf, session);
        let count = self.refcounts.entry(key.clone()).or_insert(0);
        *count += 1;
        let first = *count == 1;

        if first {
            let sub_id = proto::SubId(self.next_sub_id);
            self.next_sub_id = self.next_sub_id.saturating_add(1);
            self.sub_ids.insert(key.clone(), sub_id);
            self.by_id.insert(sub_id, key.clone());
            self.candles.insert(key.clone(), Vec::new());
            self.statuses.insert(key.clone(), self.conn_status.clone());

            let frame = proto::ClientFrame::Subscribe {
                id: sub_id,
                symbol: symbol.to_string(),
                channel: proto::Channel::Candles {
                    tf: proto_tf(tf),
                    session: proto_session(session),
                },
            };
            let _ = self.to_ws.unbounded_send(frame);

            let status = self.conn_status.clone();
            cx.emit(KlineEvent::StatusChanged {
                symbol: symbol.into(),
                tf,
                session,
                status,
            });
        }

        SubscriptionHandle {
            key,
            release_tx: self.release_tx.clone(),
        }
    }

    /// Reconnection is handled by the driver task automatically. Left as a
    /// no-op for call-site compatibility with the old stub.
    pub fn reconnect_all(&mut self, _cx: &mut Context<Self>) {}

    pub fn snapshot(&self, symbol: &str, tf: Timeframe, session: Session) -> Option<&[Candle]> {
        self.candles
            .get(&SubKey::new(symbol, tf, session))
            .map(|v| v.as_slice())
    }

    pub fn status(&self, symbol: &str, tf: Timeframe, session: Session) -> LiveStatus {
        self.statuses
            .get(&SubKey::new(symbol, tf, session))
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
        session: Session,
        _cx: &mut Context<Self>,
    ) {
        let key = SubKey::new(symbol, tf, session);
        let Some(sub_id) = self.sub_ids.get(&key).copied() else {
            return;
        };
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

    // --- Internal: driven by the connection task ---------------------------

    /// Route an incoming server frame into per-subscription state.
    fn handle_server_frame(&mut self, frame: proto::ServerFrame, cx: &mut Context<Self>) {
        self.last_message_ms = Some(chrono::Utc::now().timestamp_millis());
        match frame {
            proto::ServerFrame::Snapshot { id, candles, server_v: _ } => {
                self.on_snapshot(id, candles, cx);
            }
            proto::ServerFrame::Tick {
                id,
                candle,
                is_closed,
                v: _,
            } => {
                self.on_tick(id, candle, is_closed, cx);
            }
            proto::ServerFrame::HistoryPage { id, candles } => {
                self.on_history_page(id, candles, cx);
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
        let Some(key) = self.by_id.get(&id).cloned() else {
            return;
        };
        let bars: Vec<Candle> = candles.into_iter().map(candle_from_proto).collect();
        self.candles.insert(key.clone(), bars);
        cx.emit(KlineEvent::Resnap {
            symbol: key.symbol.clone().into(),
            tf: key.tf,
            session: key.session,
        });
    }

    fn on_tick(
        &mut self,
        id: proto::SubId,
        candle: proto::Candle,
        is_closed: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(key) = self.by_id.get(&id).cloned() else {
            return;
        };
        let c = candle_from_proto(candle);
        let bars = self.candles.entry(key.clone()).or_default();
        merge_or_append(bars, c.clone());
        cx.emit(KlineEvent::Tick {
            symbol: key.symbol.clone().into(),
            tf: key.tf,
            session: key.session,
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
        let Some(key) = self.by_id.get(&id).cloned() else {
            return;
        };
        if candles.is_empty() {
            cx.emit(KlineEvent::HistoryCapped {
                symbol: key.symbol.clone().into(),
                tf: key.tf,
                session: key.session,
            });
            return;
        }
        let added = candles.len();
        let prepend: Vec<Candle> = candles.into_iter().map(candle_from_proto).collect();
        let existing = self.candles.entry(key.clone()).or_default();
        let mut merged = Vec::with_capacity(prepend.len() + existing.len());
        merged.extend(prepend);
        merged.append(existing);
        *existing = merged;
        cx.emit(KlineEvent::Prepended {
            symbol: key.symbol.clone().into(),
            tf: key.tf,
            session: key.session,
            added,
        });
    }

    /// Server told us to reset this subscription's state. Clear the buffer
    /// and re-Subscribe (the v1 server's gateway exits its forwarder on
    /// Resnap; the resub recreates it server-side — Q12e).
    fn on_resnap(&mut self, id: proto::SubId, cx: &mut Context<Self>) {
        let Some(key) = self.by_id.get(&id).cloned() else {
            return;
        };
        self.candles.insert(key.clone(), Vec::new());
        cx.emit(KlineEvent::Resnap {
            symbol: key.symbol.clone().into(),
            tf: key.tf,
            session: key.session,
        });
        let frame = proto::ClientFrame::Subscribe {
            id,
            symbol: key.symbol.clone(),
            channel: proto::Channel::Candles {
                tf: proto_tf(key.tf),
                session: proto_session(key.session),
            },
        };
        let _ = self.to_ws.unbounded_send(frame);
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
                session: key.session,
                status: status.clone(),
            });
        }
    }

    /// Push a `Subscribe` for every active SubKey. Used by the driver task
    /// on connect (covers boot + every reconnect).
    fn resubscribe_all(&mut self) {
        let entries: Vec<(SubKey, proto::SubId)> = self
            .sub_ids
            .iter()
            .map(|(k, id)| (k.clone(), *id))
            .collect();
        for (key, sub_id) in entries {
            let frame = proto::ClientFrame::Subscribe {
                id: sub_id,
                symbol: key.symbol.clone(),
                channel: proto::Channel::Candles {
                    tf: proto_tf(key.tf),
                    session: proto_session(key.session),
                },
            };
            let _ = self.to_ws.unbounded_send(frame);
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
        }
    }
}

#[derive(Clone)]
pub struct MarketDataServiceHandle(pub Entity<MarketDataService>);
impl Global for MarketDataServiceHandle {}

pub fn init(cx: &mut App) {
    let entity = cx.new(MarketDataService::new);
    cx.set_global(MarketDataServiceHandle(entity.clone()));

    let (outbound_rx, release_rx) = entity.update(cx, |s, _| {
        let o = s
            .pending_outbound_rx
            .take()
            .expect("init runs once before any other access");
        let r = s
            .pending_release_rx
            .take()
            .expect("init runs once before any other access");
        (o, r)
    });

    // Release task: drains SubscriptionHandle::Drop → release_one.
    {
        let entity = entity.clone();
        let mut release_rx = release_rx;
        cx.spawn(async move |cx| {
            while let Some(key) = release_rx.next().await {
                entity.update(cx, |s, cx| s.release_one(key, cx));
            }
        })
        .detach();
    }

    // Connection driver: forever reconnect loop.
    {
        let entity = entity.clone();
        cx.spawn(async move |cx| {
            run_connection(entity, outbound_rx, cx).await;
        })
        .detach();
    }
}

/// Handle to a live subscription. The keyed slot stays registered as long
/// as at least one handle exists; on drop the key gets sent to the release
/// channel and the service decrements the refcount.
pub struct SubscriptionHandle {
    key: SubKey,
    release_tx: UnboundedSender<SubKey>,
}

impl Drop for SubscriptionHandle {
    fn drop(&mut self) {
        let _ = self.release_tx.unbounded_send(self.key.clone());
    }
}

// --- Connection driver ------------------------------------------------------

async fn run_connection(
    entity: Entity<MarketDataService>,
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
        entity.update(cx, |s, cx| s.set_conn_status(status, cx));

        match WsMeta::connect(SERVER_WS_URL, None).await {
            Ok((_meta, stream)) => {
                attempts = 0;
                log::info!("ws connected to {SERVER_WS_URL}");
                // Drop any stale Subscribes that piled up during the outage;
                // resubscribe_all (just below) re-issues the canonical set.
                drain_pending(&mut outbound_rx);
                entity.update(cx, |s, cx| {
                    s.set_conn_status(LiveStatus::Connected, cx);
                    s.resubscribe_all();
                });

                pump(stream, &mut outbound_rx, &entity, cx).await;
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
    entity: &Entity<MarketDataService>,
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
                                entity.update(cx, |s, cx| s.handle_server_frame(frame, cx));
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

fn proto_session(s: Session) -> proto::Session {
    match s {
        Session::Regular => proto::Session::Regular,
        Session::Extended => proto::Session::Extended,
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
    Candle::new_full(
        c.open_time,
        c.close_time,
        c.open,
        c.high,
        c.low,
        c.close,
        c.volume,
        None,
        None,
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
