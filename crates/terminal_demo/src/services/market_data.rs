//! Live market-data service backed by the **centoflow** server (US equities).
//!
//! ### Shape
//!
//! One shared, multiplexed WS to `/v1/stream` for the whole app. Chart and
//! watchlist panels register interest in `(symbol, timeframe, session)` keys
//! via [`MarketDataService::ensure`], which returns a refcounted RAII
//! [`SubscriptionHandle`]. When the last handle for a key drops, the service
//! sends `{"action":"unsubscribe",…}` on the shared WS and evicts the key.
//!
//! Bar arrival is dispatched into one of two paths per key:
//! 1. After backfill has landed — `apply_tick` merges (in place by `open_time`)
//!    or appends.
//! 2. Before backfill — the tick is buffered in `pending_ticks` (capped); when
//!    the backfill task completes it replaces the candle buffer, then drains
//!    `pending_ticks` through the same merge logic. This closes the
//!    market-open gap where a bar could finalize between the REST snapshot
//!    and the WS subscribe taking effect server-side.
//!
//! ### Connection task
//!
//! A single background task owns the socket and runs a `select!` over an
//! inbound mpsc of [`Cmd`] (sent by `ensure` / handle drop / `reconnect_all`)
//! and the WS frame stream. On disconnect it backs off (`backoff_seconds`),
//! reopens, and triggers a per-key restoration: re-sends the subscribe frame
//! and respawns each sub's backfill task in parallel.
//!
//! `reconnect_all` (login / token rotation) sets a `planned_reconnect` flag
//! and sends [`Cmd::Reset`]. The connection task drops its socket, the
//! reconnect loop sees the flag, skips backoff, and uses `Connecting` (not
//! `Reconnecting`) for the per-key status so the UX reads as "authenticating"
//! rather than "something broke".

use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use chrono::{Local, TimeZone as _};
use futures::{
    SinkExt as _, StreamExt as _,
    channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded},
};
use gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, Global, SharedString, Task,
};
use serde::Deserialize;
use ws_stream_wasm::WsMessage;

use super::bar_stream::{BarStream, Outcome};
use crate::net::{CentoflowConfig, HttpClient, ws_open};

/// Bars fetched on the first backfill of a (symbol, tf).
const INITIAL_BACKFILL: usize = 300;
/// Bars fetched per lazy "load older history" page.
const PAGE_SIZE: usize = 500;
/// Hard cap on a key's retained candle buffer. The server also clamps `limit`
/// to this; when the buffer reaches it we stop paging older and notify once.
pub(crate) const MAX_CANDLES: usize = 5000;
/// Per-key cap on ticks buffered while a backfill is in flight. If the WS
/// somehow outpaces the REST round-trip for this long (200 ticks ≈ minutes of
/// flow on a hot symbol), drop newest and `log::warn!` — backfill is broken.
const PENDING_TICKS_CAP: usize = 200;

/// A chart timeframe. The string forms match the centoflow `tf` query/protocol
/// values exactly (`1m`, `5m`, …).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Timeframe {
    M1,
    M5,
    M15,
    H1,
    D1,
}

/// Timeframe shown by default on a freshly-opened chart.
pub const DEFAULT_TIMEFRAME: Timeframe = Timeframe::M5;

impl Timeframe {
    /// All timeframes in display order — drives the chart's tf selector.
    pub const ALL: [Timeframe; 5] = [
        Timeframe::M1,
        Timeframe::M5,
        Timeframe::M15,
        Timeframe::H1,
        Timeframe::D1,
    ];

    /// Wire value used by the server (`tf=` query param / subscribe frame) and
    /// the selector label.
    pub fn as_str(self) -> &'static str {
        match self {
            Timeframe::M1 => "1m",
            Timeframe::M5 => "5m",
            Timeframe::M15 => "15m",
            Timeframe::H1 => "1h",
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
            Timeframe::H1 => 60 * 60_000,
            Timeframe::D1 => 24 * 60 * 60_000,
        }
    }
}

/// Trading session filter. `Regular` is RTH (09:30–16:00 ET) only; `Extended`
/// includes pre-market and after-hours bars too. Wire values (`regular` /
/// `extended`) match the centoflow `session=` query param and the WS subscribe
/// frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Session {
    Regular,
    Extended,
}

/// Session shown by default on a freshly-opened chart.
pub const DEFAULT_SESSION: Session = Session::Regular;

impl Session {
    pub const ALL: [Session; 2] = [Session::Regular, Session::Extended];

    pub fn as_str(self) -> &'static str {
        match self {
            Session::Regular => "regular",
            Session::Extended => "extended",
        }
    }

    /// Short label for compact UI (the toggle button in the chart header).
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

/// A single OHLCV bar. Shared between the live service and the chart panel —
/// chart's render path consumes `Vec<Candle>` directly. `open_time` (unix ms)
/// is the canonical identity used by the merge-or-append tick logic.
///
/// `vwap` and `trades` are optional: backfilled bars and closed live bars carry
/// them; a developing 1m bar may not have a trade count until the closed-bar
/// reconcile lands.
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
    /// Build a candle from already-parsed numeric fields. Formats `date` from
    /// `open_time` using the user's local TZ so the chart x-axis is readable.
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

    /// Build a candle including the optional VWAP + trade count fields.
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

/// Every charted symbol is served live by centoflow. Kept as a function so the
/// chart's live-vs-fallback branch (and future allow-listing) has one home.
pub fn is_live(_display_symbol: &str) -> bool {
    true
}

#[derive(Clone, Debug, PartialEq)]
pub enum LiveStatus {
    /// Backfilling, or first WS connect not yet open, or planned reconnect.
    Connecting,
    /// WS is open, subscribe sent, backfill applied.
    Connected,
    /// Lost WS; retrying. `attempts` is the (1-indexed) attempt count.
    Reconnecting { attempts: u32 },
}

#[derive(Clone, Debug)]
pub enum KlineEvent {
    /// A new (or in-progress) bar for `(symbol, tf, session)`. `is_closed=false`
    /// means "update last bar in place"; `true` means "final value of that bar".
    Tick {
        symbol: SharedString,
        tf: Timeframe,
        session: Session,
        candle: Candle,
        is_closed: bool,
    },
    /// Service replaced the canonical buffer for `(symbol, tf, session)`
    /// (initial backfill or post-reconnect resync). Subscribers should re-pull
    /// snapshot.
    Resnap {
        symbol: SharedString,
        tf: Timeframe,
        session: Session,
    },
    /// Older history was prepended to `(symbol, tf, session)`: `added` bars
    /// were inserted at the front. Subscribers must re-pull the snapshot AND
    /// shift any index-anchored state right by `added`.
    Prepended {
        symbol: SharedString,
        tf: Timeframe,
        session: Session,
        added: usize,
    },
    /// The key's buffer reached [`MAX_CANDLES`]; no more history will load.
    /// Emitted exactly once per key (until a fresh backfill resets the cap).
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

/// Map key for one live subscription. Includes session so a chart panel viewing
/// regular hours and another viewing extended hours on the same (symbol, tf)
/// each get their own backfill + WS stream.
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

    fn subscribe_frame(&self) -> String {
        format!(
            r#"{{"action":"subscribe","symbol":"{}","tf":"{}","session":"{}"}}"#,
            self.symbol,
            self.tf.as_str(),
            self.session.as_str(),
        )
    }

    fn unsubscribe_frame(&self) -> String {
        format!(
            r#"{{"action":"unsubscribe","symbol":"{}","tf":"{}","session":"{}"}}"#,
            self.symbol,
            self.tf.as_str(),
            self.session.as_str(),
        )
    }
}

/// Commands written to the WS task over its inbound mpsc. Sent by `ensure`,
/// the [`SubscriptionHandle`] drop path, and `reconnect_all`.
enum Cmd {
    Subscribe(SubKey),
    Unsubscribe(SubKey),
    /// Drop the current socket so the reconnect loop runs. Paired with the
    /// `planned_reconnect` flag (set by `reconnect_all`) to skip the backoff
    /// sleep and surface `Connecting` instead of `Reconnecting`.
    Reset,
}

struct Subscription {
    /// Per-sub bar buffer + ordering state machine (candles, pending_ticks,
    /// backfill_done, version cursor — see `bar_stream::BarStream`).
    stream: BarStream,
    status: LiveStatus,
    /// Bars to (re)backfill on the next connect. Starts at [`INITIAL_BACKFILL`]
    /// and grows (high-water) as older pages load, so a reconnect restores the
    /// depth the user had scrolled to instead of snapping back to the latest.
    desired_count: usize,
    /// A `load_older` page is in flight; guards against concurrent fetches.
    loading_older: bool,
    /// Buffer reached [`MAX_CANDLES`]; stop paging older.
    capped: bool,
    /// Server has no older data than what we hold; stop paging older.
    exhausted: bool,
    /// Number of live [`SubscriptionHandle`]s pointing at this entry. The key
    /// is evicted (and `Cmd::Unsubscribe` sent) when this hits 0.
    refcount: usize,
    /// In-flight backfill task; dropped (cancelled) when a new backfill is
    /// spawned (handle-driven `ensure` or reconnect-driven restoration).
    _backfill_task: Option<Task<()>>,
    /// In-flight `load_older` task, held so it cancels on drop.
    _older_task: Option<Task<()>>,
}

pub struct MarketDataService {
    subs: HashMap<SubKey, Subscription>,
    /// Unix ms of last successfully-received WS message (any key). Bottom-bar
    /// reads this for the latency readout.
    last_message_ms: Option<i64>,
    /// Outbound to the WS task: `Subscribe` / `Unsubscribe` frames, or `Reset`
    /// for planned reconnects.
    cmd_tx: UnboundedSender<Cmd>,
    /// Inbound from [`SubscriptionHandle::drop`]: keys whose refcount should
    /// be decremented. Drained by `_release_task` because Drop has no Context
    /// to update the entity from.
    release_tx: UnboundedSender<SubKey>,
    /// `reconnect_all` sets this; the connection task reads it at the top of
    /// each reconnect cycle to skip backoff and use `Connecting` status.
    /// Single-threaded (wasm) so `Rc<Cell<bool>>` is fine.
    planned_reconnect: Rc<Cell<bool>>,
    /// Background WS task. Dropped (cancelled) only when the service entity
    /// itself is dropped at app shutdown.
    _ws_task: Task<()>,
    /// Drains `release_rx` so handle drops update the service.
    _release_task: Task<()>,
}

impl EventEmitter<KlineEvent> for MarketDataService {}

impl MarketDataService {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let (cmd_tx, cmd_rx) = unbounded::<Cmd>();
        let (release_tx, mut release_rx) = unbounded::<SubKey>();
        let planned_reconnect = Rc::new(Cell::new(false));
        let planned_for_task = planned_reconnect.clone();
        let ws_task = cx.spawn(async move |this, cx| {
            run_connection(this, cx, cmd_rx, planned_for_task).await;
        });
        let release_task = cx.spawn(async move |this, cx| {
            while let Some(key) = release_rx.next().await {
                if this
                    .update(cx, |svc, _cx| svc.release(&key))
                    .is_err()
                {
                    return;
                }
            }
        });
        Self {
            subs: HashMap::new(),
            last_message_ms: None,
            cmd_tx,
            release_tx,
            planned_reconnect,
            _ws_task: ws_task,
            _release_task: release_task,
        }
    }

    /// Register interest in `(symbol, tf, session)`. Returns a refcounted
    /// handle — the caller (chart panel, watchlist panel) is expected to hold
    /// it for as long as the data is being displayed. When the last handle
    /// drops, the service sends `Cmd::Unsubscribe` and evicts the entry.
    ///
    /// Idempotent: a second `ensure` for the same key just bumps the refcount
    /// and returns a fresh handle.
    pub fn ensure(
        &mut self,
        symbol: &str,
        tf: Timeframe,
        session: Session,
        cx: &mut Context<Self>,
    ) -> SubscriptionHandle {
        let key = SubKey::new(symbol, tf, session);
        let release_tx = self.release_tx.clone();
        if let Some(sub) = self.subs.get_mut(&key) {
            sub.refcount = sub.refcount.saturating_add(1);
            return SubscriptionHandle {
                key,
                release_tx,
            };
        }
        // New entry: insert empty, then drive the option-C flow —
        // 1. send Cmd::Subscribe so the server starts pushing ticks,
        // 2. spawn the backfill task; until it completes, incoming ticks land
        //    in `pending_ticks` and are drained on apply.
        self.subs.insert(
            key.clone(),
            Subscription {
                stream: BarStream::new(),
                status: LiveStatus::Connecting,
                desired_count: INITIAL_BACKFILL,
                loading_older: false,
                capped: false,
                exhausted: false,
                refcount: 1,
                _backfill_task: None,
                _older_task: None,
            },
        );
        let _ = self.cmd_tx.unbounded_send(Cmd::Subscribe(key.clone()));
        self.spawn_backfill(&key, cx);
        SubscriptionHandle {
            key,
            release_tx,
        }
    }

    /// Force every active subscription back through a fresh backfill +
    /// re-subscribe, picking up any token change from `CentoflowConfig`. Trips
    /// the planned-reconnect flag so the WS task skips backoff and surfaces
    /// `Connecting` (not `Reconnecting`).
    pub fn reconnect_all(&mut self, _cx: &mut Context<Self>) {
        self.planned_reconnect.set(true);
        let _ = self.cmd_tx.unbounded_send(Cmd::Reset);
    }

    /// Current bars for `(symbol, tf, session)`, or `None` if not yet
    /// subscribed.
    pub fn snapshot(&self, symbol: &str, tf: Timeframe, session: Session) -> Option<&[Candle]> {
        self.subs
            .get(&SubKey::new(symbol, tf, session))
            .map(|s| s.stream.candles())
    }

    /// Status for `(symbol, tf, session)`. Defaults to `Connecting` for an
    /// unknown key (the subscription is about to be created).
    pub fn status(&self, symbol: &str, tf: Timeframe, session: Session) -> LiveStatus {
        self.subs
            .get(&SubKey::new(symbol, tf, session))
            .map(|s| s.status.clone())
            .unwrap_or(LiveStatus::Connecting)
    }

    /// Aggregate status across all subscriptions, for the global bottom-bar
    /// readout: `Connected` if any key is connected, else `Connecting` if any
    /// is connecting, else the worst `Reconnecting`.
    pub fn overall_status(&self) -> LiveStatus {
        let mut worst_attempts = 0u32;
        let mut any_connecting = false;
        for sub in self.subs.values() {
            match &sub.status {
                LiveStatus::Connected => return LiveStatus::Connected,
                LiveStatus::Connecting => any_connecting = true,
                LiveStatus::Reconnecting { attempts } => {
                    worst_attempts = worst_attempts.max(*attempts)
                }
            }
        }
        if any_connecting || self.subs.is_empty() {
            LiveStatus::Connecting
        } else {
            LiveStatus::Reconnecting {
                attempts: worst_attempts,
            }
        }
    }

    pub fn last_message_ms(&self) -> Option<i64> {
        self.last_message_ms
    }

    /// Called by `SubscriptionHandle::drop`. Refcount-- and, on zero, send
    /// `Cmd::Unsubscribe` + evict the entry (cancelling its backfill task).
    fn release(&mut self, key: &SubKey) {
        let Some(sub) = self.subs.get_mut(key) else {
            return;
        };
        if sub.refcount > 1 {
            sub.refcount -= 1;
            return;
        }
        // Last handle: tell the server we're done and drop the entry. The
        // task drop inside Subscription cancels any in-flight backfill or
        // older-page fetch automatically.
        let _ = self.cmd_tx.unbounded_send(Cmd::Unsubscribe(key.clone()));
        self.subs.remove(key);
    }

    fn set_status(
        &mut self,
        key: &SubKey,
        status: LiveStatus,
        cx: &mut Context<Self>,
    ) {
        match self.subs.get_mut(key) {
            Some(sub) if sub.status == status => return,
            Some(sub) => sub.status = status.clone(),
            None => return,
        }
        cx.emit(KlineEvent::StatusChanged {
            symbol: SharedString::from(key.symbol.clone()),
            tf: key.tf,
            session: key.session,
            status,
        });
        cx.notify();
    }

    fn set_all_status(&mut self, status: LiveStatus, cx: &mut Context<Self>) {
        let keys: Vec<SubKey> = self.subs.keys().cloned().collect();
        for key in keys {
            self.set_status(&key, status.clone(), cx);
        }
    }

    /// Spawn (or replace) the backfill task for `key`. Resets `pending_ticks`
    /// and `backfill_done` so incoming ticks buffer until the new backfill
    /// applies. Called from `ensure` (new sub) and from the WS task's
    /// reconnect-restore loop.
    fn spawn_backfill(&mut self, key: &SubKey, cx: &mut Context<Self>) {
        let Some(sub) = self.subs.get_mut(key) else {
            return;
        };
        sub.stream.reset_for_backfill();
        sub.status = LiveStatus::Connecting;
        let desired = sub.desired_count;
        let client = cx.global::<HttpClient>().0.clone();
        let key2 = key.clone();
        let task = cx.spawn(async move |this, cx| {
            let mut attempts: u32 = 0;
            loop {
                let cfg = match this.update(cx, |_s, cx| cx.global::<CentoflowConfig>().clone()) {
                    Ok(v) => v,
                    Err(_) => return,
                };
                match fetch_candles(
                    &client, &cfg, &key2.symbol, key2.tf, key2.session, desired, None,
                )
                .await
                {
                    Ok(candles) => {
                        let _ = this.update(cx, |svc, cx| {
                            svc.apply_backfill_and_drain(&key2, candles, cx);
                        });
                        return;
                    }
                    Err(e) => {
                        log::warn!(
                            "centoflow backfill {} {}: {e:#}",
                            key2.symbol,
                            key2.tf.as_str()
                        );
                        attempts = attempts.saturating_add(1);
                        let _ = this.update(cx, |svc, cx| {
                            svc.set_status(
                                &key2,
                                LiveStatus::Reconnecting { attempts },
                                cx,
                            );
                        });
                        cx.background_executor()
                            .timer(Duration::from_secs(backoff_seconds(attempts)))
                            .await;
                    }
                }
            }
        });
        if let Some(sub) = self.subs.get_mut(key) {
            sub._backfill_task = Some(task);
        }
    }

    /// Apply a completed backfill: hand the candles to `BarStream` (which
    /// also drains buffered pre-backfill ticks), update the surrounding
    /// flags, and emit `Resnap`.
    fn apply_backfill_and_drain(
        &mut self,
        key: &SubKey,
        candles: Vec<Candle>,
        cx: &mut Context<Self>,
    ) {
        {
            let Some(sub) = self.subs.get_mut(key) else {
                return;
            };
            sub.stream.apply_backfill(candles);
            sub.loading_older = false;
            sub.exhausted = false;
            let len = sub.stream.candles().len();
            sub.capped = len >= MAX_CANDLES;
            sub.desired_count = sub.desired_count.max(len).max(INITIAL_BACKFILL);
            sub.status = LiveStatus::Connected;
        }
        cx.emit(KlineEvent::Resnap {
            symbol: SharedString::from(key.symbol.clone()),
            tf: key.tf,
            session: key.session,
        });
        cx.emit(KlineEvent::StatusChanged {
            symbol: SharedString::from(key.symbol.clone()),
            tf: key.tf,
            session: key.session,
            status: LiveStatus::Connected,
        });
        cx.notify();
    }

    /// Receive one decoded WS bar frame. Delegates to `BarStream::on_tick`
    /// and reacts to the Outcome — emit `Tick`, log a drop, or trigger a
    /// gap-driven re-snapshot.
    fn handle_bar_frame(
        &mut self,
        key: SubKey,
        candle: Candle,
        is_closed: bool,
        version: u64,
        cx: &mut Context<Self>,
    ) {
        self.last_message_ms = Some(chrono::Utc::now().timestamp_millis());
        let outcome = {
            let Some(sub) = self.subs.get_mut(&key) else {
                return;
            };
            sub.stream.on_tick(candle, is_closed, version, PENDING_TICKS_CAP)
        };
        match outcome {
            Outcome::Applied { candle, is_closed } => {
                cx.emit(KlineEvent::Tick {
                    symbol: SharedString::from(key.symbol.clone()),
                    tf: key.tf,
                    session: key.session,
                    candle,
                    is_closed,
                });
                cx.notify();
            }
            Outcome::Buffered => {}
            Outcome::Dropped => {
                // Either pending cap reached or an out-of-order tick. The
                // former is the actionable case (backfill is broken / WS is
                // outpacing REST way past expected); the latter is benign.
                log::warn!(
                    "centoflow tick dropped for {} {} {}",
                    key.symbol,
                    key.tf.as_str(),
                    key.session.as_str()
                );
            }
            Outcome::Gap { last_seen, received } => {
                log::warn!(
                    "centoflow sequence gap on {} {} {}: last={last_seen} got={received}; re-snapshotting",
                    key.symbol,
                    key.tf.as_str(),
                    key.session.as_str(),
                );
                // Drop local state and kick off a fresh backfill. The server
                // is still streaming on this socket; no need to re-subscribe.
                self.spawn_backfill(&key, cx);
            }
        }
    }

    /// Lazily fetch and prepend an older page for `(symbol, tf, session)`.
    /// No-op if the key is unknown, already loading, capped, exhausted, or has
    /// no anchor bar to page back from. Spawns a one-shot task that calls
    /// [`Self::finish_load_older`] on completion.
    pub fn load_older(
        &mut self,
        symbol: &str,
        tf: Timeframe,
        session: Session,
        cx: &mut Context<Self>,
    ) {
        let key = SubKey::new(symbol, tf, session);
        let to_ms = {
            let Some(sub) = self.subs.get_mut(&key) else {
                return;
            };
            if sub.loading_older || sub.capped || sub.exhausted {
                return;
            }
            if sub.stream.candles().len() >= MAX_CANDLES {
                return;
            }
            let Some(oldest) = sub.stream.candles().first() else {
                return;
            };
            sub.loading_older = true;
            oldest.open_time
        };

        let client = cx.global::<HttpClient>().0.clone();
        let key2 = key.clone();
        let task = cx.spawn(async move |this, cx| {
            let Ok(cfg) = this.update(cx, |_s, cx| cx.global::<CentoflowConfig>().clone()) else {
                return;
            };
            let result = fetch_candles(
                &client,
                &cfg,
                &key2.symbol,
                key2.tf,
                key2.session,
                PAGE_SIZE,
                Some(to_ms),
            )
            .await;
            let _ = this.update(cx, |s, cx| {
                s.finish_load_older(&key2, to_ms, result, cx);
            });
        });
        if let Some(sub) = self.subs.get_mut(&key) {
            sub._older_task = Some(task);
        }
    }

    /// Merge a completed older-page fetch: dedup against the current head,
    /// prepend, trim to [`MAX_CANDLES`], and emit `Prepended` / one-shot
    /// `HistoryCapped`.
    fn finish_load_older(
        &mut self,
        key: &SubKey,
        head_open: i64,
        result: anyhow::Result<Vec<Candle>>,
        cx: &mut Context<Self>,
    ) {
        let (added, now_capped) = {
            let Some(sub) = self.subs.get_mut(key) else {
                return;
            };
            sub.loading_older = false;
            let older = match result {
                Ok(c) => c,
                Err(e) => {
                    log::warn!(
                        "centoflow load_older {} {}: {e:#}",
                        key.symbol,
                        key.tf.as_str()
                    );
                    return;
                }
            };
            // Keep only bars strictly older than our current head (the server's
            // `to` is inclusive of the boundary bucket, so drop the overlap).
            let mut older: Vec<Candle> = older
                .into_iter()
                .filter(|c| c.open_time < head_open)
                .collect();
            older.sort_by(|a, b| a.open_time.cmp(&b.open_time));
            if older.is_empty() {
                sub.exhausted = true;
                return;
            }
            let page_len = older.len();
            // Drain current candles into `older`, optionally trim from the
            // front to respect MAX_CANDLES, then swap back. Keep the &mut
            // borrow scoped tightly so we can touch sub.desired_count after.
            {
                let buf = sub.stream.candles_mut();
                older.append(buf);
            }
            let total = older.len();
            let added = if total > MAX_CANDLES {
                let drop_front = total - MAX_CANDLES;
                older.drain(0..drop_front);
                page_len - drop_front
            } else {
                page_len
            };
            let new_len = older.len();
            *sub.stream.candles_mut() = older;
            sub.desired_count = sub.desired_count.max(new_len);
            let now_capped = new_len >= MAX_CANDLES && !sub.capped;
            if now_capped {
                sub.capped = true;
            }
            (added, now_capped)
        };

        let sym = SharedString::from(key.symbol.clone());
        if added > 0 {
            cx.emit(KlineEvent::Prepended {
                symbol: sym.clone(),
                tf: key.tf,
                session: key.session,
                added,
            });
        }
        if now_capped {
            cx.emit(KlineEvent::HistoryCapped {
                symbol: sym,
                tf: key.tf,
                session: key.session,
            });
        }
        cx.notify();
    }

    /// Snapshot of every active key. Called by the WS task on reconnect to
    /// know what to restore.
    fn active_keys(&self) -> Vec<SubKey> {
        self.subs.keys().cloned().collect()
    }
}

#[derive(Clone)]
pub struct MarketDataServiceHandle(pub Entity<MarketDataService>);
impl Global for MarketDataServiceHandle {}

pub fn init(cx: &mut App) {
    let entity = cx.new(MarketDataService::new);
    cx.set_global(MarketDataServiceHandle(entity));
}

// ---------------------------------------------------------------------------
// SubscriptionHandle: RAII handle returned by `ensure`. Drops trigger refcount
// decrement and eviction (Cmd::Unsubscribe) when the last handle for a key
// goes away.
// ---------------------------------------------------------------------------

/// Handle to a live `(symbol, tf, session)` subscription. While at least one
/// handle exists the service keeps the sub registered with the server; when
/// the last handle drops, the service unsubscribes and frees the candle
/// buffer. Hold one in any panel that depends on the data being streamed.
pub struct SubscriptionHandle {
    key: SubKey,
    /// Sends the key on Drop so the service's release task can decrement the
    /// refcount inside an entity update (Drop has no Context).
    release_tx: UnboundedSender<SubKey>,
}

impl Drop for SubscriptionHandle {
    fn drop(&mut self) {
        let _ = self.release_tx.unbounded_send(self.key.clone());
    }
}

// ---------------------------------------------------------------------------
// Connection task — one shared WS, multiplexed subs. Lives for the service's
// lifetime; reads commands from `cmd_rx` and ws frames in a `select!`, calls
// back into the service entity for state mutations.
// ---------------------------------------------------------------------------

async fn run_connection(
    this: gpui::WeakEntity<MarketDataService>,
    cx: &mut gpui::AsyncApp,
    mut cmd_rx: UnboundedReceiver<Cmd>,
    planned_reconnect: Rc<Cell<bool>>,
) {
    let mut attempts: u32 = 0;
    loop {
        let planned = planned_reconnect.get();

        // Backoff unless this is the first attempt or a planned reconnect.
        if attempts > 0 && !planned {
            cx.background_executor()
                .timer(Duration::from_secs(backoff_seconds(attempts)))
                .await;
        }

        // Re-read config so a token rotated since the last open takes effect.
        let cfg = match this.update(cx, |_s, cx| cx.global::<CentoflowConfig>().clone()) {
            Ok(v) => v,
            Err(_) => return,
        };

        // Surface the next status to the UI: `Connecting` on first try or a
        // planned reconnect, otherwise `Reconnecting { attempts }`.
        let pre_status = if planned || attempts == 0 {
            LiveStatus::Connecting
        } else {
            LiveStatus::Reconnecting { attempts }
        };
        if this
            .update(cx, |svc, cx| svc.set_all_status(pre_status, cx))
            .is_err()
        {
            return;
        }

        let url = stream_url(&cfg);
        let ws = match ws_open(&url).await {
            Ok(ws) => ws,
            Err(e) => {
                log::warn!("centoflow ws connect failed: {e:#}");
                attempts = attempts.saturating_add(1);
                continue;
            }
        };

        // Planned-reconnect handshake satisfied: clear the flag so a later
        // organic disconnect uses normal backoff + Reconnecting status.
        planned_reconnect.set(false);
        attempts = 0;
        log::info!("centoflow ws open ({url})");

        let (mut tx, rx) = ws.split();
        // SplitStream isn't FusedStream out of the box — wrap once so the
        // `select!` macro can poll `.next()` repeatedly.
        let mut rx = rx.fuse();

        // Restore every active key on the new socket: re-send subscribe frame
        // and respawn the per-key backfill task in parallel. Bars that land
        // before backfill completes go to `pending_ticks` and drain on apply.
        let keys = this
            .update(cx, |svc, _| svc.active_keys())
            .unwrap_or_default();
        let mut send_failed = false;
        for key in &keys {
            if tx
                .send(WsMessage::Text(key.subscribe_frame()))
                .await
                .is_err()
            {
                send_failed = true;
                break;
            }
            if this
                .update(cx, |svc, cx| svc.spawn_backfill(key, cx))
                .is_err()
            {
                return;
            }
        }
        if send_failed {
            attempts = attempts.saturating_add(1);
            continue;
        }

        // Main pump: write `Cmd`s, read bar frames. Exit on any error / EOF
        // and the outer loop reconnects.
        let mut should_reconnect = false;
        loop {
            futures::select! {
                cmd = cmd_rx.next() => {
                    let Some(cmd) = cmd else { return; };
                    match cmd {
                        Cmd::Subscribe(key) => {
                            if tx.send(WsMessage::Text(key.subscribe_frame())).await.is_err() {
                                break;
                            }
                        }
                        Cmd::Unsubscribe(key) => {
                            // Best-effort: if the socket is dying we don't
                            // care, the next reconnect's `active_keys` won't
                            // include this key.
                            if tx.send(WsMessage::Text(key.unsubscribe_frame())).await.is_err() {
                                break;
                            }
                        }
                        Cmd::Reset => {
                            should_reconnect = true;
                            break;
                        }
                    }
                }
                msg = rx.next() => {
                    let Some(msg) = msg else { break; };
                    match msg {
                        WsMessage::Text(text) => {
                            if let Some((key, candle, is_closed, version)) = parse_bar_frame(&text) {
                                if this.update(cx, |svc, cx| {
                                    svc.handle_bar_frame(key, candle, is_closed, version, cx);
                                }).is_err() {
                                    return;
                                }
                            }
                        }
                        WsMessage::Binary(_) => {}
                    }
                }
            }
        }

        // Inner loop exited. Drop the sink so the underlying socket closes,
        // then reconnect.
        drop(tx);
        drop(rx);
        if should_reconnect {
            // Reset is planned; status will be Connecting at top of next iter.
            // attempts stays 0 so no backoff.
            log::info!("centoflow ws reset");
        } else {
            attempts = attempts.saturating_add(1);
            log::info!("centoflow ws disconnected, retrying (attempt {attempts})");
        }
    }
}

/// 1, 2, 4, 8, 16, 30, 30, ... — capped at 30s.
fn backoff_seconds(attempts: u32) -> u64 {
    let shift = attempts.saturating_sub(1).min(5);
    (1u64 << shift).min(30)
}

fn stream_url(cfg: &CentoflowConfig) -> String {
    match &cfg.token {
        Some(t) => format!("{}/v1/stream?token={t}", cfg.ws_base()),
        None => format!("{}/v1/stream", cfg.ws_base()),
    }
}

// ---------------------------------------------------------------------------
// REST backfill
// ---------------------------------------------------------------------------

/// Fetch up to `limit` candles for `(symbol, tf)`. With `to = None` the server
/// returns the most recent `limit` bars (initial/reconnect backfill); with
/// `to = Some(ms)` it returns the `limit` bars ending at that time (older-page
/// paging).
pub(crate) async fn fetch_candles(
    client: &reqwest::Client,
    cfg: &CentoflowConfig,
    symbol: &str,
    tf: Timeframe,
    session: Session,
    limit: usize,
    to: Option<i64>,
) -> anyhow::Result<Vec<Candle>> {
    let mut url = format!(
        "{}/v1/candles?symbol={}&tf={}&limit={}&session={}&adjusted=true",
        cfg.base_url,
        symbol,
        tf.as_str(),
        limit,
        session.as_str(),
    );
    if let Some(to_ms) = to {
        url.push_str(&format!("&to={to_ms}"));
    }
    let mut req = client.get(&url);
    if let Some(token) = &cfg.token {
        req = req.bearer_auth(token);
    }
    let resp = req.send().await?;
    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!("centoflow /v1/candles returned HTTP {status}");
    }
    let parsed: CandlesResponse = resp.json().await?;
    Ok(parsed
        .candles
        .into_iter()
        .map(RawCandle::into_candle)
        .collect())
}

#[derive(Debug, Deserialize)]
struct CandlesResponse {
    #[serde(default)]
    candles: Vec<RawCandle>,
}

/// Server candle JSON: `{ "t":openMs, "T":closeMs, "o","h","l","c","v","vw","n" }`.
/// `vw` (VWAP) and `n` (trade count) are optional — older bars / pre-VWAP-feature
/// servers may omit them.
#[derive(Debug, Deserialize, Default)]
struct RawCandle {
    t: i64,
    #[serde(rename = "T", default)]
    t_close: i64,
    o: f64,
    h: f64,
    l: f64,
    c: f64,
    v: f64,
    #[serde(default)]
    vw: Option<f64>,
    #[serde(default)]
    n: Option<i32>,
}

impl RawCandle {
    fn into_candle(self) -> Candle {
        Candle::new_full(
            self.t, self.t_close, self.o, self.h, self.l, self.c, self.v, self.vw, self.n,
        )
    }
}

// ---------------------------------------------------------------------------
// WS message parsing — multiplexed protocol now carries symbol/tf/session in
// every bar push so the client can route to the right `SubKey`.
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct BarFrame {
    #[serde(rename = "type")]
    typ: String,
    #[serde(default)]
    symbol: String,
    #[serde(default)]
    tf: String,
    #[serde(default)]
    session: String,
    #[serde(default)]
    candle: Option<RawCandle>,
    #[serde(default)]
    is_closed: bool,
    /// Per-subscription monotonic version cursor (`hub.Update.Version` on the
    /// server). Missing/older servers send 0; the client treats a constant
    /// stream of 0s as "no gap detection" without false positives.
    #[serde(default, rename = "v")]
    version: u64,
}

/// Parse a server frame into `(key, candle, is_closed, version)`. Non-bar
/// frames (status, error, future types) return `None` and are silently
/// skipped.
fn parse_bar_frame(text: &str) -> Option<(SubKey, Candle, bool, u64)> {
    let frame: BarFrame = serde_json::from_str(text)
        .map_err(|e| log::debug!("ws frame parse failed: {e}"))
        .ok()?;
    if frame.typ != "bar" {
        return None;
    }
    let tf = Timeframe::from_str(&frame.tf)?;
    let session = Session::from_str(&frame.session).unwrap_or(DEFAULT_SESSION);
    let candle = frame.candle?.into_candle();
    Some((
        SubKey::new(&frame.symbol, tf, session),
        candle,
        frame.is_closed,
        frame.version,
    ))
}
