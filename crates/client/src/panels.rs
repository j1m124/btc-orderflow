use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable, Global,
    InteractiveElement as _, IntoElement, MouseButton, ParentElement as _, Render,
    SharedString, StatefulInteractiveElement as _, Styled as _, Task, WeakEntity, Window, div,
    prelude::FluentBuilder as _,
};
use gpui_component::{
    ActiveTheme as _,
    dock::{
        DockArea, DockEvent, Panel, PanelControl, PanelEvent, PanelInfo, PanelState, PanelView,
        TabPanel, register_panel,
    },
    input::{InputEvent, InputState},
};
use serde::{Deserialize, Serialize};

pub mod chart;
pub mod orderbook;
pub mod trades;
pub mod watchlist;

pub use chart::{
    ChangeChartRender, ChangeChartTimeframe, ChartRenderSettingsView, GoToLatest,
    OpenChartRenderSettings, ResetChartScale, ToggleChartRenderVisible,
};
pub use orderbook::{ChangeOrderbookBucket, ChangeOrderbookSizeMode};
pub use trades::ChangeTradesSizeMode;

/// Minimum interval between chart re-paints driven by tick events. 50ms = 20Hz.
const CHART_TICK_INTERVAL_MS: i64 = 50;

pub type PanelKind = Kind;
pub const PANEL_KINDS: &[Kind] = Kind::ALL;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    Watchlist,
    Chart,
    Trades,
    Orderbook,
}

impl Kind {
    pub const ALL: &'static [Kind] = &[
        Kind::Watchlist,
        Kind::Chart,
        Kind::Trades,
        Kind::Orderbook,
    ];

    pub fn id(self) -> &'static str {
        match self {
            Kind::Watchlist => "Watchlist",
            Kind::Chart => "Chart",
            Kind::Trades => "Trades",
            Kind::Orderbook => "Orderbook",
        }
    }

    pub fn display(self) -> &'static str {
        self.id()
    }

    pub fn from_id(id: &str) -> Option<Kind> {
        Self::ALL.iter().copied().find(|k| k.id() == id)
    }
}

/// Global tracker for the most recently focused [`TabPanel`]. Lets the
/// "+ Panel" menu drop new panels into the pane the user last interacted with.
#[derive(Default)]
pub struct LastFocusedTabPanel(pub Rc<RefCell<Option<WeakEntity<TabPanel>>>>);
impl Global for LastFocusedTabPanel {}

/// Global tracker for the most recently focused Chart panel. Drives the
/// watchlist's row-click handler so symbol switches land on whichever chart
/// the user last touched.
#[derive(Default)]
pub struct LastFocusedChart(pub Rc<RefCell<Option<WeakEntity<ContentPanel>>>>);
impl Global for LastFocusedChart {}

pub fn init(cx: &mut App) {
    cx.set_global(LastFocusedTabPanel::default());
    cx.set_global(LastFocusedChart::default());
    cx.bind_keys([
        gpui::KeyBinding::new(
            "delete",
            crate::drawings::actions::DeleteSelectedDrawing,
            Some("Chart"),
        ),
        gpui::KeyBinding::new(
            "backspace",
            crate::drawings::actions::DeleteSelectedDrawing,
            Some("Chart"),
        ),
    ]);
    for kind in Kind::ALL {
        let kind = *kind;
        register_panel(
            cx,
            kind.id(),
            move |_dock_area, _state, info, window, cx| {
                Box::new(cx.new(|cx| ContentPanel::new_restored(kind, info, window, cx)))
            },
        );
    }
}

pub fn build_kind(kind: Kind, window: &mut Window, cx: &mut App) -> Arc<dyn PanelView> {
    Arc::new(cx.new(|cx| ContentPanel::new(kind, window, cx)))
}

fn live_snapshot(
    symbol: &str,
    tf: crate::services::market_data::Timeframe,
    cx: &App,
) -> Vec<crate::services::market_data::Candle> {
    let handle = cx
        .global::<crate::services::market_data::MarketDataServiceHandle>()
        .0
        .clone();
    handle
        .read(cx)
        .snapshot(symbol, tf)
        .map(|s| s.to_vec())
        .unwrap_or_default()
}

fn ensure_chart_sub(
    symbol: &str,
    tf: crate::services::market_data::Timeframe,
    cx: &mut Context<ContentPanel>,
) -> crate::services::market_data::SubscriptionHandle {
    let handle = cx
        .global::<crate::services::market_data::MarketDataServiceHandle>()
        .0
        .clone();
    handle.update(cx, |svc, cx| svc.ensure(symbol, tf, cx))
}

#[derive(Serialize, Deserialize)]
struct ChartPrefs {
    symbol: String,
    tf: String,
}

fn chart_prefs_from_info(
    info: &PanelInfo,
) -> Option<(SharedString, crate::services::market_data::Timeframe)> {
    let PanelInfo::Panel(value) = info else {
        return None;
    };
    let prefs: ChartPrefs = serde_json::from_value(value.clone()).ok()?;
    let tf = crate::services::market_data::Timeframe::from_str(&prefs.tf)?;
    Some((SharedString::from(prefs.symbol), tf))
}

#[derive(Serialize, Deserialize)]
struct TradesPrefs {
    symbol: String,
    /// Min USD notional. `None` (or missing) → no filter. Stored as a raw
    /// number rather than a preset id so user-typed thresholds persist as
    /// typed. Older persisted state predates this field and loads cleanly
    /// (`#[serde(default)]` → `None`).
    #[serde(default)]
    min_usd: Option<f64>,
    /// Size column display mode (`"coin" / "usd"`). Optional for backward
    /// compatibility — missing → `Coin`.
    #[serde(default)]
    size_mode: Option<String>,
}

fn trades_prefs_from_info(
    info: &PanelInfo,
) -> Option<(SharedString, Option<f64>, trades::TradesSizeMode)> {
    let PanelInfo::Panel(value) = info else {
        return None;
    };
    let prefs: TradesPrefs = serde_json::from_value(value.clone()).ok()?;
    let size_mode = prefs
        .size_mode
        .as_deref()
        .and_then(trades::TradesSizeMode::from_id)
        .unwrap_or_default();
    Some((SharedString::from(prefs.symbol), prefs.min_usd, size_mode))
}

#[derive(Serialize, Deserialize)]
struct OrderbookPrefs {
    symbol: String,
    /// Bucket id ("tick", "1", "5", "10", "25"). String rather than f64 so
    /// "tick" (raw, no bucketing) is representable distinctly from "$0.10".
    bucket: String,
    /// Size column display mode (`"coin" / "usd"`). Optional for backward
    /// compatibility — missing → `Coin`.
    #[serde(default)]
    size_mode: Option<String>,
}

fn orderbook_prefs_from_info(
    info: &PanelInfo,
) -> Option<(
    SharedString,
    orderbook::OrderbookBucket,
    orderbook::OrderbookSizeMode,
)> {
    let PanelInfo::Panel(value) = info else {
        return None;
    };
    let prefs: OrderbookPrefs = serde_json::from_value(value.clone()).ok()?;
    let bucket = orderbook::OrderbookBucket::from_id(&prefs.bucket)?;
    let size_mode = prefs
        .size_mode
        .as_deref()
        .and_then(orderbook::OrderbookSizeMode::from_id)
        .unwrap_or_default();
    Some((SharedString::from(prefs.symbol), bucket, size_mode))
}

/// Per-panel state for an Orderbook ContentPanel.
///
/// `sticky_center` is a TOGGLE: while true, every render snaps the spread
/// row to the middle of the viewport, so as the inside market moves the
/// ladder follows it. The flag is turned ON at mount, on bucket change,
/// on symbol change, and by the Center button. It's turned OFF
/// automatically when the user scrolls — render detects scroll by
/// comparing the live `scroll.offset().y` against the value we wrote
/// last (`last_set_offset_y`); a mismatch means the wheel / drag moved
/// the offset out from under us.
///
/// `_trades_sub_handle` is held only to power the repurposed spread row's
/// last-trade-price marker — render reads the latest trade from the
/// service's per-symbol trades buffer via the panel's own `TradeEvent`
/// subscription.
pub struct OrderbookState {
    pub symbol: SharedString,
    pub bucket: orderbook::OrderbookBucket,
    pub size_mode: orderbook::OrderbookSizeMode,
    pub scroll: gpui_component::VirtualListScrollHandle,
    pub sticky_center: bool,
    pub last_set_offset_y: Option<gpui::Pixels>,
    _sub_handle: crate::services::market_data::SubscriptionHandle,
    _trades_sub_handle: crate::services::market_data::SubscriptionHandle,
}

/// Sanity ceiling on the trades panel's persist buffer. The panel isn't
/// scrollable so anything beyond the viewport is invisible; this cap only
/// guards against runaway growth on a `min_usd = 0` (no filter) tape over
/// a long session. ~5000 × ~50B/Trade ≈ 250 KB per panel.
const TRADES_PERSIST_CAP: usize = 5_000;

fn default_symbol(cx: &App) -> SharedString {
    cx.global::<crate::services::symbols::SymbolsServiceHandle>()
        .0
        .read(cx)
        .default_symbol()
        .unwrap_or_else(|| SharedString::from(chart::ChartState::default_symbol()))
}

#[derive(Clone)]
pub struct DockAreaHandle(pub WeakEntity<DockArea>);
impl Global for DockAreaHandle {}

fn request_layout_save(cx: &mut Context<ContentPanel>) {
    let dock = cx
        .try_global::<DockAreaHandle>()
        .and_then(|h| h.0.upgrade());
    if let Some(dock) = dock {
        dock.update(cx, |_, cx| cx.emit(DockEvent::LayoutChanged));
    }
}

pub struct ContentPanel {
    kind: Kind,
    focus_handle: FocusHandle,
    parent_tab_panel: Option<WeakEntity<TabPanel>>,
    pub(crate) chart_state: Option<chart::ChartState>,
    _chart_tick_flush: Option<Task<()>>,
    _chart_clock_tick: Option<Task<()>>,
    _tz_subscription: Option<gpui::Subscription>,
    chart_sub_handles: Vec<crate::services::market_data::SubscriptionHandle>,
    /// Footprint subscription for the chart's active render kind. Allocated
    /// lazily when render kind enters Cluster / Profile; dropped on the way
    /// out. The key is tracked separately so the lifecycle helper can detect
    /// (symbol, tf, bucket) drift and drop+reopen exactly once.
    chart_footprint_sub: Option<crate::services::market_data::SubscriptionHandle>,
    /// `(symbol, tf, bucket_bits)` — bucket bits are `f64::to_bits` so the
    /// key supports `Eq`. Any drift triggers drop+reopen of the sub.
    chart_footprint_key:
        Option<(SharedString, crate::services::market_data::Timeframe, u64)>,
    watchlist_sub_handles: HashMap<SharedString, crate::services::market_data::SubscriptionHandle>,
    pub(crate) trades_symbol: Option<SharedString>,
    pub(crate) trades_min_usd: Option<Option<f64>>,
    pub(crate) trades_size_mode: Option<trades::TradesSizeMode>,
    pub(crate) trades_filter_input: Option<Entity<InputState>>,
    pub(crate) trades_persist: Option<VecDeque<crate::services::market_data::Trade>>,
    _trades_sub_handle: Option<crate::services::market_data::SubscriptionHandle>,
    _trades_input_subscription: Option<gpui::Subscription>,
    pub(crate) orderbook_state: Option<OrderbookState>,
    /// Monotonic counter bumped by the trades + book event handlers.
    /// Subscribing only via `cx.notify()` empirically wasn't enough to mark
    /// the panel entity dirty under gpui_web — the working `chart` path
    /// also writes to `chart_state` on every tick, and that mutation is
    /// what actually queues a re-render. We mirror that by mutating this
    /// field on every relevant event so the dirty flag is always set.
    tick_seq: u64,
}

impl ContentPanel {
    pub fn new(kind: Kind, window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self::new_inner(kind, None, None, None, window, cx)
    }

    pub fn new_restored(
        kind: Kind,
        info: &PanelInfo,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new_inner(
            kind,
            chart_prefs_from_info(info),
            trades_prefs_from_info(info),
            orderbook_prefs_from_info(info),
            window,
            cx,
        )
    }

    fn new_inner(
        kind: Kind,
        chart_prefs: Option<(SharedString, crate::services::market_data::Timeframe)>,
        trades_prefs: Option<(SharedString, Option<f64>, trades::TradesSizeMode)>,
        orderbook_prefs: Option<(
            SharedString,
            orderbook::OrderbookBucket,
            orderbook::OrderbookSizeMode,
        )>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        let mut chart_handles: Vec<crate::services::market_data::SubscriptionHandle> = Vec::new();
        let chart_state = matches!(kind, Kind::Chart).then(|| {
            let (symbol, tf) = match &chart_prefs {
                Some((s, t)) => (s.clone(), *t),
                None => {
                    let default_tf = chart::ChartState::default_timeframe();
                    let default_symbol: SharedString = cx
                        .global::<crate::services::symbols::SymbolsServiceHandle>()
                        .0
                        .read(cx)
                        .default_symbol()
                        .unwrap_or_else(|| {
                            SharedString::from(chart::ChartState::default_symbol())
                        });
                    (default_symbol, default_tf)
                }
            };
            chart_handles = vec![ensure_chart_sub(symbol.as_ref(), tf, cx)];
            let live = live_snapshot(symbol.as_ref(), tf, cx);
            chart::ChartState::new(symbol.as_ref(), tf, live)
        });
        let watchlist_handles = if matches!(kind, Kind::Watchlist) {
            let h = watchlist::initial_handles(cx);
            watchlist::subscribe(window, cx);
            h
        } else {
            HashMap::new()
        };
        if matches!(kind, Kind::Chart) {
            let symbols = cx
                .global::<crate::services::symbols::SymbolsServiceHandle>()
                .0
                .clone();
            cx.subscribe(
                &symbols,
                |_this, _svc, _ev: &crate::services::symbols::SymbolsEvent, cx| {
                    cx.notify();
                },
            )
            .detach();
            let drawings = cx
                .global::<crate::drawings::service::DrawingServiceHandle>()
                .0
                .clone();
            cx.subscribe(
                &drawings,
                |this, _svc, ev: &crate::drawings::service::DrawingEvent, cx| {
                    use crate::drawings::service::DrawingEvent::*;
                    match ev {
                        Changed { symbol } => {
                            if let Some(state) = this.chart_state.as_ref() {
                                if state.symbol().as_ref() == symbol.as_ref() {
                                    cx.notify();
                                }
                            }
                        }
                        Wiped | SelectionChanged => {
                            cx.notify();
                        }
                    }
                },
            )
            .detach();
        }
        let chart_tick_pending = Rc::new(Cell::new(false));
        let chart_tick_last_ms = Rc::new(Cell::new(0i64));
        let mut chart_tick_flush: Option<Task<()>> = None;
        let mut chart_clock_tick: Option<Task<()>> = None;
        let mut tz_subscription: Option<gpui::Subscription> = None;
        if matches!(kind, Kind::Chart) {
            let service =
                cx.global::<crate::services::market_data::MarketDataServiceHandle>()
                    .0
                    .clone();
            let pending = chart_tick_pending.clone();
            let last_ms = chart_tick_last_ms.clone();
            cx.subscribe_in(
                &service,
                window,
                move |this, _service, event: &crate::services::market_data::KlineEvent, _window, cx| {
                    use crate::services::market_data::KlineEvent::*;
                    let Some(state) = this.chart_state.as_mut() else {
                        return;
                    };
                    match event {
                        Tick { symbol, tf, candle, is_closed } => {
                            if state.symbol().as_ref() != symbol.as_ref()
                                || state.timeframe() != *tf
                            {
                                return;
                            }
                            state.apply_tick(candle.clone(), *is_closed);
                            let now_ms = chrono::Utc::now().timestamp_millis();
                            let elapsed = now_ms - last_ms.get();
                            if elapsed >= CHART_TICK_INTERVAL_MS {
                                last_ms.set(now_ms);
                                pending.set(false);
                                cx.notify();
                            } else {
                                pending.set(true);
                            }
                        }
                        Resnap { symbol, tf } => {
                            if state.symbol().as_ref() == symbol.as_ref()
                                && state.timeframe() == *tf
                            {
                                let snap = live_snapshot(symbol.as_ref(), *tf, cx);
                                state.resnap(snap);
                                cx.notify();
                            }
                        }
                        Prepended { symbol, tf, added } => {
                            if state.symbol().as_ref() == symbol.as_ref()
                                && state.timeframe() == *tf
                            {
                                let snap = live_snapshot(symbol.as_ref(), *tf, cx);
                                state.apply_prepend(snap, *added);
                                cx.notify();
                            }
                        }
                        HistoryCapped { .. } | StatusChanged { .. } => {
                            cx.notify();
                        }
                    }
                },
            )
            .detach();
            // FootprintEvent → write into ChartState's footprint_cells buffer.
            // Matched on the chart's currently-pinned `chart_footprint_key`
            // (set by `refresh_chart_footprint_sub`); events for stale subs
            // are ignored. Service's footprint snapshot is the authoritative
            // buffer — we copy it wholesale on every relevant event rather
            // than maintaining a parallel diff path here.
            cx.subscribe_in(
                &service,
                window,
                |this,
                 _service,
                 event: &crate::services::market_data::FootprintEvent,
                 _window,
                 cx| {
                    use crate::services::market_data::FootprintEvent::*;
                    let Some((key_symbol, key_tf, key_bucket_bits)) =
                        this.chart_footprint_key.clone()
                    else {
                        return;
                    };
                    let key_bucket = f64::from_bits(key_bucket_bits);
                    let matches = match event {
                        Snapshot { symbol, tf, .. }
                        | Update { symbol, tf, .. }
                        | Prepended { symbol, tf, .. }
                        | HistoryCapped { symbol, tf }
                        | Resnap { symbol, tf } => {
                            symbol.as_ref() == key_symbol.as_ref() && *tf == key_tf
                        }
                    };
                    if !matches {
                        return;
                    }
                    match event {
                        Snapshot { .. } | Update { .. } | Prepended { .. } | Resnap { .. } => {
                            let market = cx
                                .global::<
                                    crate::services::market_data::MarketDataServiceHandle,
                                >()
                                .0
                                .clone();
                            let cells = market.read(cx).footprint_cells(
                                key_symbol.as_ref(),
                                key_tf,
                                key_bucket,
                            );
                            if let Some(state) = this.chart_state.as_mut() {
                                state.set_footprint_cells(cells);
                            }
                            cx.notify();
                        }
                        HistoryCapped { .. } => {
                            cx.notify();
                        }
                    }
                },
            )
            .detach();
            let pending = chart_tick_pending.clone();
            let last_ms = chart_tick_last_ms.clone();
            chart_tick_flush = Some(cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(
                            CHART_TICK_INTERVAL_MS as u64,
                        ))
                        .await;
                    if !pending.get() {
                        continue;
                    }
                    if this
                        .update(cx, |_this, cx| {
                            pending.set(false);
                            last_ms.set(chrono::Utc::now().timestamp_millis());
                            cx.notify();
                        })
                        .is_err()
                    {
                        return;
                    }
                }
            }));
            chart_clock_tick = Some(cx.spawn(async move |this, cx| {
                loop {
                    let now_ms = chrono::Utc::now().timestamp_millis();
                    let to_next_sec = 1000 - (now_ms.rem_euclid(1000)) as u64;
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(to_next_sec + 5))
                        .await;
                    if this
                        .update(cx, |this, cx| {
                            if let Some(state) = this.chart_state.as_mut() {
                                let now_ms = chrono::Utc::now().timestamp_millis();
                                state.tick_clock(now_ms);
                            }
                            cx.notify();
                        })
                        .is_err()
                    {
                        return;
                    }
                }
            }));
            tz_subscription = Some(cx.observe_global::<crate::prefs::UserTz>(|_, cx| {
                cx.notify();
            }));
        }
        let orderbook_state = if matches!(kind, Kind::Orderbook) {
            let (sym, bucket, size_mode) = orderbook_prefs.unwrap_or_else(|| {
                (
                    default_symbol(cx),
                    orderbook::OrderbookBucket::default(),
                    orderbook::OrderbookSizeMode::default(),
                )
            });
            let book_handle = cx
                .global::<crate::services::market_data::MarketDataServiceHandle>()
                .0
                .clone()
                .update(cx, |svc, cx| {
                    svc.ensure_book(sym.as_ref(), orderbook::WS_DEPTH, cx)
                });
            // Trades subscription powers the repurposed spread row's
            // ▲/▼ last-trade strip. Refcounted on `SubKey` in the service, so
            // having the Trades panel open at the same time costs one WS sub
            // total, not two.
            let trades_handle = cx
                .global::<crate::services::market_data::MarketDataServiceHandle>()
                .0
                .clone()
                .update(cx, |svc, cx| svc.ensure_trades(sym.as_ref(), cx));
            let market = cx
                .global::<crate::services::market_data::MarketDataServiceHandle>()
                .0
                .clone();
            cx.subscribe_in(
                &market,
                window,
                |this, _svc, ev: &crate::services::market_data::BookEvent, _window, cx| {
                    use crate::services::market_data::BookEvent::*;
                    match ev {
                        Snapshot { .. }
                        | Delta { .. }
                        | HistoryPrepended { .. }
                        | HistoryCapped { .. }
                        | Resnap { .. } => {
                            this.tick_seq = this.tick_seq.wrapping_add(1);
                            cx.notify();
                        }
                    }
                },
            )
            .detach();
            cx.subscribe_in(
                &market,
                window,
                |this, _svc, ev: &crate::services::market_data::TradeEvent, _window, cx| {
                    use crate::services::market_data::TradeEvent::*;
                    // Render reads `trades_snapshot(symbol).last()` for the
                    // strip — any buffer mutation potentially changes that
                    // tail, so repaint on every variant.
                    match ev {
                        Snapshot { .. }
                        | Tick { .. }
                        | Prepended { .. }
                        | HistoryCapped { .. }
                        | Resnap { .. } => {
                            this.tick_seq = this.tick_seq.wrapping_add(1);
                            cx.notify();
                        }
                    }
                },
            )
            .detach();
            Some(OrderbookState {
                symbol: sym,
                bucket,
                size_mode,
                scroll: gpui_component::VirtualListScrollHandle::new(),
                // Mount with sticky center engaged so the spread row is
                // pinned to the viewport middle from the very first paint.
                sticky_center: true,
                last_set_offset_y: None,
                _sub_handle: book_handle,
                _trades_sub_handle: trades_handle,
            })
        } else {
            None
        };
        let (
            trades_symbol,
            trades_min_usd,
            trades_size_mode,
            trades_filter_input,
            trades_persist,
            _trades_sub_handle,
            _trades_input_subscription,
        ) = if matches!(kind, Kind::Trades) {
            let (sym, min_usd, size_mode) = trades_prefs
                .unwrap_or_else(|| (default_symbol(cx), None, trades::TradesSizeMode::default()));
            let handle = cx
                .global::<crate::services::market_data::MarketDataServiceHandle>()
                .0
                .clone()
                .update(cx, |svc, cx| svc.ensure_trades(sym.as_ref(), cx));
            let market = cx
                .global::<crate::services::market_data::MarketDataServiceHandle>()
                .0
                .clone();
            cx.subscribe_in(
                &market,
                window,
                |this, _svc, ev: &crate::services::market_data::TradeEvent, _window, cx| {
                    use crate::services::market_data::TradeEvent::*;
                    match ev {
                        Snapshot { symbol, .. } | Resnap { symbol } => {
                            if this.trades_symbol.as_deref().map_or(true, |s| s != symbol.as_ref()) {
                                return;
                            }
                            this.reseed_trades_persist(cx);
                            this.tick_seq = this.tick_seq.wrapping_add(1);
                            cx.notify();
                        }
                        Tick { symbol, trades } => {
                            if this.trades_symbol.as_deref().map_or(true, |s| s != symbol.as_ref()) {
                                return;
                            }
                            this.append_trades_persist(trades.iter().cloned(), cx);
                            this.tick_seq = this.tick_seq.wrapping_add(1);
                            cx.notify();
                        }
                        Prepended { .. } | HistoryCapped { .. } => {
                            this.tick_seq = this.tick_seq.wrapping_add(1);
                            cx.notify();
                        }
                    }
                },
            )
            .detach();
            // Header input: free-form min-USD threshold. We seed it from
            // persisted prefs (if any) and subscribe to InputEvent::Change
            // so each keystroke updates the panel threshold and reseeds
            // `persist` from whatever's currently in the service ring.
            let initial_text: SharedString = match min_usd {
                Some(v) if v > 0.0 => SharedString::from(format!("{:.0}", v)),
                _ => SharedString::default(),
            };
            let input_state =
                cx.new(|cx| InputState::new(window, cx).placeholder("Min USD"));
            if !initial_text.is_empty() {
                input_state.update(cx, |s, cx| s.set_value(initial_text, window, cx));
            }
            let input_sub = cx.subscribe_in(
                &input_state,
                window,
                |this, input, ev: &InputEvent, window, cx| {
                    if matches!(ev, InputEvent::Change) {
                        let text = input.read(cx).value();
                        this.apply_trades_min_usd_from_text(text.as_ref(), window, cx);
                    }
                },
            );
            (
                Some(sym),
                Some(min_usd),
                Some(size_mode),
                Some(input_state),
                Some(VecDeque::new()),
                Some(handle),
                Some(input_sub),
            )
        } else {
            (None, None, None, None, None, None, None)
        };
        let mut new_self = Self {
            kind,
            focus_handle,
            parent_tab_panel: None,
            chart_state,
            _chart_tick_flush: chart_tick_flush,
            _chart_clock_tick: chart_clock_tick,
            _tz_subscription: tz_subscription,
            chart_sub_handles: chart_handles,
            chart_footprint_sub: None,
            chart_footprint_key: None,
            watchlist_sub_handles: watchlist_handles,
            trades_symbol,
            trades_min_usd,
            trades_size_mode,
            trades_filter_input,
            trades_persist,
            _trades_sub_handle,
            _trades_input_subscription,
            orderbook_state,
            tick_seq: 0,
        };
        // Seed the trades persist with whatever passing prints are already
        // in the service ring at mount — otherwise the panel reads as empty
        // until the next live tick.
        if matches!(kind, Kind::Trades) {
            new_self.reseed_trades_persist(cx);
        }
        new_self
    }

    /// Parse the free-form filter input value and apply it as the panel's
    /// `min_usd` threshold. Triggers re-render + persistence save.
    pub fn apply_trades_min_usd_from_text(
        &mut self,
        text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let parsed = trades::parse_min_usd(text);
        let Some(current) = self.trades_min_usd.as_mut() else {
            return;
        };
        if *current == parsed {
            return;
        }
        *current = parsed;
        self.reseed_trades_persist(cx);
        cx.notify();
        request_layout_save(cx);
    }

    /// Rebuild the panel-local persist buffer from the service's per-symbol
    /// trade ring at the current threshold. Called on mount, on threshold
    /// change, and on service Snapshot / Resnap. Cheap — the service ring
    /// is bounded at `TRADES_BUFFER_CAP`.
    pub fn reseed_trades_persist(&mut self, cx: &mut Context<Self>) {
        let Some(symbol) = self.trades_symbol.clone() else {
            return;
        };
        let Some(persist) = self.trades_persist.as_mut() else {
            return;
        };
        let min_usd = self.trades_min_usd.unwrap_or(None);
        persist.clear();
        let market = cx
            .global::<crate::services::market_data::MarketDataServiceHandle>()
            .0
            .clone();
        if let Some(snap) = market.read(cx).trades_snapshot(symbol.as_ref()) {
            for t in snap.iter() {
                if trades::passes_min_usd(t, min_usd) {
                    persist.push_back(t.clone());
                }
            }
            while persist.len() > TRADES_PERSIST_CAP {
                persist.pop_front();
            }
        }
    }

    /// Append new live trades to the persist buffer, dropping ones below
    /// the current threshold. Called from the `TradeEvent::Tick` handler.
    pub fn append_trades_persist<I>(&mut self, trades: I, _cx: &mut Context<Self>)
    where
        I: IntoIterator<Item = crate::services::market_data::Trade>,
    {
        let min_usd = self.trades_min_usd.unwrap_or(None);
        let Some(persist) = self.trades_persist.as_mut() else {
            return;
        };
        for t in trades {
            if trades::passes_min_usd(&t, min_usd) {
                persist.push_back(t);
            }
        }
        while persist.len() > TRADES_PERSIST_CAP {
            persist.pop_front();
        }
    }

    /// Update the trades panel's Size column display mode (COIN / USD).
    /// Triggers re-render + persistence save.
    pub fn set_trades_size_mode(
        &mut self,
        mode: trades::TradesSizeMode,
        cx: &mut Context<Self>,
    ) {
        let Some(current) = self.trades_size_mode.as_mut() else {
            return;
        };
        if *current == mode {
            return;
        }
        *current = mode;
        cx.notify();
        request_layout_save(cx);
    }

    /// Update the orderbook panel's bucket choice. Triggers re-render +
    /// persistence save. Bucket changes collapse / expand row counts and
    /// shift the spread row's index, so the previous scroll offset is
    /// meaningless — re-engage sticky centering so the new mid snaps back
    /// into the middle of the viewport.
    pub fn set_orderbook_bucket(
        &mut self,
        bucket: orderbook::OrderbookBucket,
        cx: &mut Context<Self>,
    ) {
        let Some(state) = self.orderbook_state.as_mut() else {
            return;
        };
        if state.bucket == bucket {
            return;
        }
        state.bucket = bucket;
        state.sticky_center = true;
        state.last_set_offset_y = None;
        cx.notify();
        request_layout_save(cx);
    }

    /// Update the orderbook panel's Size column display mode (COIN / USD).
    /// Triggers re-render + persistence save.
    pub fn set_orderbook_size_mode(
        &mut self,
        mode: orderbook::OrderbookSizeMode,
        cx: &mut Context<Self>,
    ) {
        let Some(state) = self.orderbook_state.as_mut() else {
            return;
        };
        if state.size_mode == mode {
            return;
        }
        state.size_mode = mode;
        cx.notify();
        request_layout_save(cx);
    }

    /// Engage sticky centering. The "Center" button in the panel header
    /// drives this; while sticky is on, every render snaps the spread row
    /// to the viewport middle. Sticky turns OFF automatically when the
    /// user scrolls (detected in render).
    pub fn request_orderbook_recenter(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.orderbook_state.as_mut() else {
            return;
        };
        state.sticky_center = true;
        state.last_set_offset_y = None;
        cx.notify();
    }

    pub fn add_indicator_from_picker(
        &mut self,
        kind: Box<dyn crate::indicators::IndicatorKind>,
        cx: &mut Context<Self>,
    ) {
        let Some(state) = self.chart_state.as_mut() else {
            return;
        };
        state.add_indicator(kind);
        cx.notify();
    }

    pub fn switch_chart_symbol(&mut self, target: &str, cx: &mut Context<Self>) {
        let Some(state) = self.chart_state.as_mut() else {
            return;
        };
        let tf = state.timeframe();
        let new_handles = vec![ensure_chart_sub(target, tf, cx)];
        self.chart_sub_handles = new_handles;
        let live = live_snapshot(target, tf, cx);
        if state.switch_symbol(target, live) {
            self.refresh_chart_footprint_sub(cx);
            cx.notify();
            request_layout_save(cx);
        }
    }

    pub fn switch_chart_timeframe(
        &mut self,
        tf: crate::services::market_data::Timeframe,
        cx: &mut Context<Self>,
    ) {
        let Some(state) = self.chart_state.as_mut() else {
            return;
        };
        let symbol = state.symbol().clone();
        let new_handles = vec![ensure_chart_sub(symbol.as_ref(), tf, cx)];
        self.chart_sub_handles = new_handles;
        let live = live_snapshot(symbol.as_ref(), tf, cx);
        if state.switch_timeframe(tf, live) {
            self.refresh_chart_footprint_sub(cx);
            cx.notify();
            request_layout_save(cx);
        }
    }

    /// Apply a mutation to the active render's `FootprintParams`. The
    /// closure receives `&mut FootprintParams` and runs against whichever
    /// mode is currently active (Cluster or Profile). Returns `true` if
    /// the mutation actually ran (i.e. the active render is a footprint
    /// kind, not Candlestick).
    ///
    /// After the mutation, the footprint subscription is reconciled —
    /// `refresh_chart_footprint_sub` will detect any bucket change and
    /// drop+reopen the sub for the new bucket.
    pub fn apply_active_footprint_params<F>(&mut self, f: F, cx: &mut Context<Self>) -> bool
    where
        F: FnOnce(&mut chart::FootprintParams),
    {
        let Some(state) = self.chart_state.as_mut() else {
            return false;
        };
        let kind = state.render_kind();
        let ran = match kind {
            chart::RenderKind::Candlestick => false,
            chart::RenderKind::Cluster => {
                state.update_cluster_params(|p| {
                    f(p);
                    false
                });
                true
            }
            chart::RenderKind::Profile => {
                state.update_profile_params(|p| {
                    f(p);
                    false
                });
                true
            }
        };
        if ran {
            // Bucket drift inside refresh_chart_footprint_sub triggers the
            // drop+reopen; cosmetic-only edits (wireframe / metric / scope)
            // are no-ops at the sub layer but still need a repaint.
            self.refresh_chart_footprint_sub(cx);
            cx.notify();
            request_layout_save(cx);
        }
        ran
    }

    /// Switch the chart's render kind. If the kind actually changed, the
    /// footprint sub is re-evaluated atomically (allocated, dropped, or
    /// re-keyed) and the chart re-renders.
    pub fn switch_chart_render(&mut self, kind: chart::RenderKind, cx: &mut Context<Self>) {
        let Some(state) = self.chart_state.as_mut() else {
            return;
        };
        if state.switch_render(kind) {
            // Cells from the old mode (if any) are stale relative to the
            // new render kind; clear immediately so the fallback path
            // (candle bodies) shows while the new sub's snapshot arrives.
            state.clear_footprint_cells();
            self.refresh_chart_footprint_sub(cx);
            cx.notify();
            request_layout_save(cx);
        }
    }

    /// Reconcile `chart_footprint_sub` with the chart's current render kind
    /// + params. Idempotent — bails fast when the desired key matches the
    /// pinned one, drops the old handle and clears stale cells when the
    /// key shifts, allocates a new sub when entering a footprint mode.
    fn refresh_chart_footprint_sub(&mut self, cx: &mut Context<Self>) {
        let desired: Option<(
            SharedString,
            crate::services::market_data::Timeframe,
            f64,
        )> = self.chart_state.as_ref().and_then(|state| {
            if !state.render_kind().needs_footprint_sub() {
                return None;
            }
            let params = state.active_footprint_params()?;
            if !chart::FootprintParams::bucket_is_valid(params.bucket) {
                return None;
            }
            Some((state.symbol().clone(), state.timeframe(), params.bucket))
        });
        let desired_key = desired
            .as_ref()
            .map(|(s, tf, b)| (s.clone(), *tf, b.to_bits()));
        if desired_key == self.chart_footprint_key {
            return;
        }
        // Drop the old handle BEFORE allocating the new one so the service
        // refcount can settle to zero (and the WS Unsubscribe fire) before
        // the new Subscribe — keeps the per-(symbol, tf, bucket) sub from
        // racing itself when only the bucket changed.
        self.chart_footprint_sub = None;
        if let Some(state) = self.chart_state.as_mut() {
            state.clear_footprint_cells();
        }
        if let Some((sym, tf, bucket)) = desired {
            let handle = cx
                .global::<crate::services::market_data::MarketDataServiceHandle>()
                .0
                .clone()
                .update(cx, |svc, cx| {
                    svc.ensure_footprint(sym.as_ref(), tf, bucket, cx)
                });
            self.chart_footprint_sub = Some(handle);
            self.chart_footprint_key = Some((sym, tf, bucket.to_bits()));
        } else {
            self.chart_footprint_key = None;
        }
    }

    pub fn chart_timeframe(&self) -> Option<crate::services::market_data::Timeframe> {
        self.chart_state.as_ref().map(|s| s.timeframe())
    }

    pub fn maybe_load_older(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.chart_state.as_ref() else {
            return;
        };
        if !state.wants_older() {
            return;
        }
        let symbol = state.symbol().clone();
        let tf = state.timeframe();
        let handle = cx
            .global::<crate::services::market_data::MarketDataServiceHandle>()
            .0
            .clone();
        handle.update(cx, |svc, cx| svc.load_older(symbol.as_ref(), tf, cx));
    }

    fn on_change_chart_timeframe(
        &mut self,
        action: &ChangeChartTimeframe,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tf) = crate::services::market_data::Timeframe::from_str(action.0.as_ref()) else {
            return;
        };
        self.switch_chart_timeframe(tf, cx);
    }

    fn on_change_chart_render(
        &mut self,
        action: &ChangeChartRender,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(kind) = chart::RenderKind::from_id(action.0.as_ref()) else {
            return;
        };
        self.switch_chart_render(kind, cx);
    }

    fn on_toggle_chart_render_visible(
        &mut self,
        _: &ToggleChartRenderVisible,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(state) = self.chart_state.as_mut() else {
            return;
        };
        let v = state.render_visible();
        state.set_render_visible(!v);
        cx.notify();
    }

    fn on_change_trades_size_mode(
        &mut self,
        action: &ChangeTradesSizeMode,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(mode) = trades::TradesSizeMode::from_id(action.0.as_ref()) else {
            return;
        };
        self.set_trades_size_mode(mode, cx);
    }

    fn on_change_orderbook_size_mode(
        &mut self,
        action: &ChangeOrderbookSizeMode,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(mode) = orderbook::OrderbookSizeMode::from_id(action.0.as_ref()) else {
            return;
        };
        self.set_orderbook_size_mode(mode, cx);
    }

    fn on_change_orderbook_bucket(
        &mut self,
        action: &ChangeOrderbookBucket,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(bucket) = orderbook::OrderbookBucket::from_id(action.0.as_ref()) else {
            return;
        };
        self.set_orderbook_bucket(bucket, cx);
    }

    fn on_delete_selected_drawing(
        &mut self,
        _: &crate::drawings::actions::DeleteSelectedDrawing,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let svc = cx
            .global::<crate::drawings::service::DrawingServiceHandle>()
            .0
            .clone();
        svc.update(cx, |s, cx| {
            s.delete_selected(cx);
        });
    }

    fn on_move_indicator_pane_up(
        &mut self,
        action: &crate::panels::chart::MoveIndicatorPaneUp,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(chart) = self.chart_state.as_mut() {
            chart.move_indicator_pane(action.0, -1);
            cx.notify();
        }
    }

    fn on_move_indicator_pane_down(
        &mut self,
        action: &crate::panels::chart::MoveIndicatorPaneDown,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(chart) = self.chart_state.as_mut() {
            chart.move_indicator_pane(action.0, 1);
            cx.notify();
        }
    }

    fn on_toggle_indicator_hidden(
        &mut self,
        action: &crate::panels::chart::ToggleIndicatorHidden,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(chart) = self.chart_state.as_mut() {
            let was_hidden = chart
                .indicators()
                .iter()
                .find(|i| i.id == action.0)
                .map(|i| i.hidden)
                .unwrap_or(false);
            chart.set_indicator_hidden(action.0, !was_hidden);
            cx.notify();
        }
    }

    fn on_remove_indicator(
        &mut self,
        action: &crate::panels::chart::RemoveIndicator,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(chart) = self.chart_state.as_mut() {
            chart.remove_indicator(action.0);
            cx.notify();
        }
    }

    fn on_clear_drawings(
        &mut self,
        _: &crate::drawings::actions::ClearChartDrawings,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(state) = self.chart_state.as_ref() else {
            return;
        };
        let symbol = state.symbol().clone();
        let svc = cx
            .global::<crate::drawings::service::DrawingServiceHandle>()
            .0
            .clone();
        svc.update(cx, |s, cx| {
            s.clear_symbol(symbol.as_ref(), cx);
        });
    }

    fn on_reset_chart_scale(
        &mut self,
        _: &ResetChartScale,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(state) = self.chart_state.as_mut() else {
            return;
        };
        state.reset_scale();
        cx.notify();
    }

    fn on_go_to_latest(&mut self, _: &GoToLatest, _: &mut Window, cx: &mut Context<Self>) {
        let Some(state) = self.chart_state.as_mut() else {
            return;
        };
        state.snap_to_latest();
        cx.notify();
    }

    pub fn parent_tab_panel(&self) -> Option<WeakEntity<TabPanel>> {
        self.parent_tab_panel.clone()
    }

    pub fn kind(&self) -> Kind {
        self.kind
    }

    fn mark_focused(&self, cx: &mut App) {
        let Some(tab_panel) = self.parent_tab_panel.clone() else {
            return;
        };
        let global = cx.global::<LastFocusedTabPanel>().0.clone();
        *global.borrow_mut() = Some(tab_panel);
    }

    fn is_focused(&self, cx: &App) -> bool {
        let Some(mine) = self.parent_tab_panel.as_ref() else {
            return false;
        };
        let global = cx.global::<LastFocusedTabPanel>().0.borrow();
        global
            .as_ref()
            .map(|w| w.entity_id() == mine.entity_id())
            .unwrap_or(false)
    }
}

impl EventEmitter<PanelEvent> for ContentPanel {}

impl Focusable for ContentPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Panel for ContentPanel {
    fn panel_name(&self) -> &'static str {
        self.kind.id()
    }

    fn title(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The single-tab dock path renders `title()` instead of `tab_name()`
        // (see gpui-component `tab_panel.rs::render_title_bar` — when one
        // panel is visible it skips the tab strip and falls back to the
        // panel's title). Mirror the `tab_name` lookup here so the dock
        // label stays `btc/usd@binancef` regardless of tab count.
        self.tab_name(cx)
            .unwrap_or_else(|| SharedString::from(self.kind.display()))
    }

    fn tab_name(&self, cx: &App) -> Option<SharedString> {
        let ticker: SharedString = match self.kind {
            Kind::Chart => self.chart_state.as_ref().map(|s| s.symbol().clone())?,
            Kind::Trades => self.trades_symbol.clone()?,
            Kind::Orderbook => self.orderbook_state.as_ref().map(|s| s.symbol.clone())?,
            _ => return None,
        };
        Some(
            cx.global::<crate::services::symbols::SymbolsServiceHandle>()
                .0
                .read(cx)
                .normalized_or_lower(ticker.as_ref()),
        )
    }

    fn closable(&self, _cx: &App) -> bool {
        true
    }

    fn zoomable(&self, _cx: &App) -> Option<PanelControl> {
        Some(PanelControl::Menu)
    }

    fn dump(&self, _cx: &App) -> PanelState {
        let mut state = PanelState::new(self);
        if let Some(chart) = &self.chart_state {
            if let Ok(value) = serde_json::to_value(ChartPrefs {
                symbol: chart.symbol().to_string(),
                tf: chart.timeframe().as_str().to_string(),
            }) {
                state.info = PanelInfo::panel(value);
            }
        }
        if matches!(self.kind, Kind::Trades) {
            if let Some(sym) = &self.trades_symbol {
                let min_usd = self.trades_min_usd.unwrap_or(None);
                let size_mode = self.trades_size_mode.unwrap_or_default();
                if let Ok(value) = serde_json::to_value(TradesPrefs {
                    symbol: sym.to_string(),
                    min_usd,
                    size_mode: Some(size_mode.id().to_string()),
                }) {
                    state.info = PanelInfo::panel(value);
                }
            }
        }
        if let Some(ob) = &self.orderbook_state {
            if let Ok(value) = serde_json::to_value(OrderbookPrefs {
                symbol: ob.symbol.to_string(),
                bucket: ob.bucket.id().to_string(),
                size_mode: Some(ob.size_mode.id().to_string()),
            }) {
                state.info = PanelInfo::panel(value);
            }
        }
        state
    }

    fn on_added_to(
        &mut self,
        tab_panel: WeakEntity<TabPanel>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        self.parent_tab_panel = Some(tab_panel);
    }

    fn on_removed(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        self.parent_tab_panel = None;
    }

    fn set_active(&mut self, active: bool, _window: &mut Window, cx: &mut Context<Self>) {
        if active {
            self.mark_focused(cx);
            if self.kind == Kind::Chart {
                let weak = cx.weak_entity();
                let global = cx.global::<LastFocusedChart>().0.clone();
                *global.borrow_mut() = Some(weak);
            }
        }
    }
}

impl Render for ContentPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let raw_body = match self.kind {
            Kind::Watchlist => watchlist::render(window, cx).into_any_element(),
            Kind::Chart => chart::render(
                self.chart_state
                    .as_ref()
                    .expect("chart_state set for Chart"),
                self.focus_handle.clone(),
                window,
                cx,
            )
            .into_any_element(),
            Kind::Trades => {
                let symbol = self
                    .trades_symbol
                    .clone()
                    .expect("trades_symbol set for Trades");
                let size_mode = self
                    .trades_size_mode
                    .expect("trades_size_mode set for Trades");
                let input = self
                    .trades_filter_input
                    .clone()
                    .expect("trades_filter_input set for Trades");
                // SAFETY: render() borrows &self only; we copy/clone the slice
                // out for the call so the panel's persist buffer isn't held
                // across the render closure.
                let persist_vec: Vec<crate::services::market_data::Trade> = self
                    .trades_persist
                    .as_ref()
                    .map(|p| p.iter().cloned().collect())
                    .unwrap_or_default();
                trades::render(
                    symbol,
                    &persist_vec,
                    size_mode,
                    &input,
                    self.focus_handle.clone(),
                    window,
                    cx,
                )
                .into_any_element()
            }
            Kind::Orderbook => orderbook::render(
                self.orderbook_state
                    .as_mut()
                    .expect("orderbook_state set for Orderbook"),
                self.focus_handle.clone(),
                window,
                cx,
            )
            .into_any_element(),
        };
        let body = if matches!(
            self.kind,
            Kind::Chart | Kind::Trades | Kind::Orderbook
        ) {
            raw_body
        } else {
            div()
                .id(SharedString::from(format!("scroll-{}", self.kind.id())))
                .size_full()
                .overflow_y_scroll()
                .child(raw_body)
                .into_any_element()
        };
        let border_color = if self.is_focused(cx) {
            cx.theme().ring
        } else {
            gpui::transparent_black()
        };
        div()
            .id(SharedString::from(format!("panel-body-{}", self.kind.id())))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _ev, _window, cx| {
                    this.mark_focused(cx);
                    if this.kind == Kind::Chart {
                        let weak = cx.weak_entity();
                        let global = cx.global::<LastFocusedChart>().0.clone();
                        *global.borrow_mut() = Some(weak);
                    }
                }),
            )
            .when(matches!(self.kind, Kind::Chart), |this| {
                this.track_focus(&self.focus_handle)
                    .key_context("Chart")
                    .on_action(cx.listener(Self::on_change_chart_timeframe))
                    .on_action(cx.listener(Self::on_change_chart_render))
                    .on_action(cx.listener(Self::on_toggle_chart_render_visible))
                    .on_action(cx.listener(Self::on_move_indicator_pane_up))
                    .on_action(cx.listener(Self::on_move_indicator_pane_down))
                    .on_action(cx.listener(Self::on_toggle_indicator_hidden))
                    .on_action(cx.listener(Self::on_remove_indicator))
                    .on_action(cx.listener(Self::on_delete_selected_drawing))
                    .on_action(cx.listener(Self::on_clear_drawings))
                    .on_action(cx.listener(Self::on_reset_chart_scale))
                    .on_action(cx.listener(Self::on_go_to_latest))
            })
            .when(matches!(self.kind, Kind::Trades), |this| {
                this.track_focus(&self.focus_handle)
                    .key_context("Trades")
                    .on_action(cx.listener(Self::on_change_trades_size_mode))
            })
            .when(matches!(self.kind, Kind::Orderbook), |this| {
                this.track_focus(&self.focus_handle)
                    .key_context("Orderbook")
                    .on_action(cx.listener(Self::on_change_orderbook_size_mode))
                    .on_action(cx.listener(Self::on_change_orderbook_bucket))
            })
            .size_full()
            .border_2()
            .border_color(border_color)
            .child(body)
    }
}
