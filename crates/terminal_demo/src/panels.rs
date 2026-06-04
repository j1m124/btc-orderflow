use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable, Global,
    InteractiveElement as _, IntoElement, MouseButton, ParentElement as _, Render, ScrollHandle,
    SharedString, StatefulInteractiveElement as _, Styled as _, Task, WeakEntity, Window, div,
    prelude::FluentBuilder as _,
};
use gpui_component::{
    ActiveTheme as _, WindowExt as _,
    dock::{
        DockArea, DockEvent, Panel, PanelControl, PanelEvent, PanelInfo, PanelState, PanelView,
        TabPanel, register_panel,
    },
    input::InputState,
    text::TextViewState,
};
use serde::{Deserialize, Serialize};

pub mod ai_chat;
pub mod chart;
pub mod details;
pub mod economic_calendar;
pub mod execution;
pub mod filings;
pub mod geopolitics;
pub mod insider;
pub mod news;
pub mod notifications;
pub mod portfolio;
pub mod position;
pub mod screener;
pub mod signal;
pub mod smart_money;
pub mod trump;
pub mod watchlist;

pub use chart::{
    ChangeChartSession, ChangeChartTimeframe, GoToLatest, ResetChartScale,
};

/// Minimum interval between chart re-paints driven by WS ticks. A live feed can
/// push bar updates rapidly on an active market; without throttling the chart's
/// expensive paint path was effectively eating the frame budget. 50 ms = 20 Hz,
/// which is more than enough for a developing bar to look alive without choking
/// the UI.
const CHART_TICK_INTERVAL_MS: i64 = 50;

// Convenience re-export for callers that still use the old `PanelKind` name.
pub type PanelKind = Kind;
pub const PANEL_KINDS: &[Kind] = Kind::ALL;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    Watchlist,
    Chart,
    Details,
    Portfolio,
    Notification,
    SmartMoney,
    AiChat,
    EconomicCalendar,
    Position,
    Execution,
    Trump,
    Screener,
    Geopolitics,
    Signal,
    SignalDetail,
    Filings,
    Insider,
    News,
}

impl Kind {
    pub const ALL: &'static [Kind] = &[
        Kind::Watchlist,
        Kind::Chart,
        Kind::Details,
        Kind::Portfolio,
        Kind::Notification,
        Kind::SmartMoney,
        Kind::AiChat,
        Kind::EconomicCalendar,
        Kind::Position,
        Kind::Execution,
        Kind::Trump,
        Kind::Screener,
        Kind::Geopolitics,
        Kind::Signal,
        Kind::SignalDetail,
        Kind::Filings,
        Kind::Insider,
        Kind::News,
    ];

    pub fn id(self) -> &'static str {
        match self {
            Kind::Watchlist => "Watchlist",
            Kind::Chart => "Chart",
            Kind::Details => "Details",
            Kind::Portfolio => "Portfolio",
            Kind::Notification => "Notification",
            Kind::SmartMoney => "SmartMoney",
            Kind::AiChat => "AiChat",
            Kind::EconomicCalendar => "EconomicCalendar",
            Kind::Position => "Position",
            Kind::Execution => "Execution",
            Kind::Trump => "Trump",
            Kind::Screener => "Screener",
            Kind::Geopolitics => "Geopolitics",
            Kind::Signal => "Signal",
            Kind::SignalDetail => "SignalDetail",
            Kind::Filings => "Filings",
            Kind::Insider => "Insider",
            Kind::News => "News",
        }
    }

    pub fn display(self) -> &'static str {
        match self {
            Kind::SmartMoney => "Smart Money",
            Kind::AiChat => "AI Chat",
            Kind::EconomicCalendar => "Economic Calendar",
            Kind::Trump => "Trump Tracker",
            Kind::SignalDetail => "Signal Detail",
            Kind::Filings => "SEC Filings",
            Kind::Insider => "Insider Trades",
            Kind::News => "News",
            other => other.id(),
        }
    }

    /// Whether this kind is allowed in a given mode. Drives the +Panel
    /// menu's filter list and the mode's initial layout.
    pub fn allowed_in_mode(self, mode: crate::persistence::Mode) -> bool {
        use crate::persistence::Mode;
        match mode {
            Mode::Charting => matches!(
                self,
                Kind::Chart | Kind::Watchlist | Kind::Details
            ),
            Mode::Signal => matches!(self, Kind::Signal | Kind::SignalDetail),
            Mode::Research => matches!(
                self,
                Kind::Watchlist
                    | Kind::SmartMoney
                    | Kind::EconomicCalendar
                    | Kind::Filings
                    | Kind::Screener
                    | Kind::Geopolitics
                    | Kind::Trump
            ),
            Mode::Portfolio => matches!(self, Kind::Portfolio),
            // Free Layout accepts everything dockable. Singletons are still
            // toolbar-managed and hidden from the +Panel menu by callers.
            Mode::FreeLayout => true,
        }
    }

    pub fn from_id(id: &str) -> Option<Kind> {
        Self::ALL.iter().copied().find(|k| k.id() == id)
    }

    /// Singleton kinds may only have one instance live at a time. The toolbar
    /// toggles and the +Panel menu both consult this to avoid duplicates.
    pub fn is_singleton(self) -> bool {
        matches!(self, Kind::AiChat | Kind::Position | Kind::Execution)
    }
}

/// Global tracker for the most recently focused [`TabPanel`].
///
/// Why: gpui-component's `DockArea::add_panel` only takes a `DockPlacement` (Center/Left/Right/
/// Bottom), so it can't target a *specific* TabPanel. We track focus ourselves so the "+ Panel"
/// menu can drop new panels into whichever pane the user last clicked on.
#[derive(Default)]
pub struct LastFocusedTabPanel(pub Rc<RefCell<Option<WeakEntity<TabPanel>>>>);
impl Global for LastFocusedTabPanel {}

/// Global tracker for the most recently focused Chart `ContentPanel`. Drives
/// the watchlist's row-click handler — clicking a watchlist row switches the
/// symbol on whichever chart the user last interacted with. Stays set after
/// the user clicks into another panel (e.g. the watchlist itself), which is
/// the behaviour we want.
#[derive(Default)]
pub struct LastFocusedChart(pub Rc<RefCell<Option<WeakEntity<ContentPanel>>>>);
impl Global for LastFocusedChart {}

/// Globally tracked active mode. Read at render time by `ContentPanel`'s
/// `closable` / `zoomable` impls so constrained modes hide those controls.
/// Workspace owns the writes — sets on construction and on every mode switch.
#[derive(Default)]
pub struct CurrentModeGlobal(pub crate::persistence::Mode);
impl Global for CurrentModeGlobal {}

pub fn init(cx: &mut App) {
    cx.set_global(LastFocusedTabPanel::default());
    cx.set_global(LastFocusedChart::default());
    cx.set_global(CurrentModeGlobal::default());
    // Delete / Backspace removes the chart panel's selected drawing. Scoped
    // to the "Chart" key-context (set via `.key_context("Chart")` on the
    // chart panel's outer div) so that when a focused `Input` is on the
    // stack, the input's own backspace/delete bindings win — otherwise the
    // user can't delete characters while editing a text drawing.
    cx.bind_keys([
        // Chart-scoped: Delete / Backspace fire only when focus is inside a
        // chart panel (which sets `key_context("Chart")`). A workspace-wide
        // binding would also fire inside dialog inputs — typing in the
        // "Edit ray label" dialog would otherwise delete the selected ray.
        // The Objects popover still exposes per-drawing Delete as an
        // explicit menu item, so keyboard-from-popover isn't needed.
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
            move |_dock_area, _state, info, window, cx| match kind {
                Kind::EconomicCalendar => Box::new(
                    cx.new(|cx| crate::panels::economic_calendar::EconomicCalendarPanel::new(window, cx)),
                ),
                Kind::Filings => Box::new(
                    cx.new(|cx| crate::panels::filings::FilingsPanel::new(window, cx)),
                ),
                Kind::Insider => Box::new(
                    cx.new(|cx| crate::panels::insider::InsiderPanel::new(window, cx)),
                ),
                Kind::News => Box::new(
                    cx.new(|cx| crate::panels::news::NewsPanel::new(window, cx)),
                ),
                Kind::Details => Box::new(
                    cx.new(|cx| crate::panels::details::DetailsPanel::new(window, cx)),
                ),
                _ => Box::new(cx.new(|cx| ContentPanel::new_restored(kind, info, window, cx))),
            },
        );
    }
}

pub fn build_kind(kind: Kind, window: &mut Window, cx: &mut App) -> Arc<dyn PanelView> {
    match kind {
        Kind::EconomicCalendar => {
            Arc::new(cx.new(|cx| crate::panels::economic_calendar::EconomicCalendarPanel::new(window, cx)))
        }
        Kind::Filings => {
            Arc::new(cx.new(|cx| crate::panels::filings::FilingsPanel::new(window, cx)))
        }
        Kind::Insider => {
            Arc::new(cx.new(|cx| crate::panels::insider::InsiderPanel::new(window, cx)))
        }
        Kind::News => {
            Arc::new(cx.new(|cx| crate::panels::news::NewsPanel::new(window, cx)))
        }
        Kind::Details => {
            Arc::new(cx.new(|cx| crate::panels::details::DetailsPanel::new(window, cx)))
        }
        _ => Arc::new(cx.new(|cx| ContentPanel::new(kind, window, cx))),
    }
}

/// Read the current candle snapshot for `(symbol, tf, session)` from
/// `MarketDataService`. Returns `None` for non-live symbols (unused now that
/// all symbols are live). Empty `Some(vec)` means "subscribed but backfill not
/// done yet"; the chart starts empty and fills on the next `Resnap`.
fn live_snapshot(
    symbol: &str,
    tf: crate::services::market_data::Timeframe,
    session: crate::services::market_data::Session,
    cx: &App,
) -> Option<Vec<crate::services::market_data::Candle>> {
    if !crate::services::market_data::is_live(symbol) {
        return None;
    }
    let handle = cx
        .global::<crate::services::market_data::MarketDataServiceHandle>()
        .0
        .clone();
    Some(
        handle
            .read(cx)
            .snapshot(symbol, tf, session)
            .map(|s| s.to_vec())
            .unwrap_or_default(),
    )
}

/// Register interest in `(symbol, tf, session)` and return the RAII handle.
/// The caller is expected to store it; dropping the last handle for a key
/// triggers a server-side unsubscribe + eviction of the candle buffer.
fn ensure_sub(
    symbol: &str,
    tf: crate::services::market_data::Timeframe,
    session: crate::services::market_data::Session,
    cx: &mut Context<ContentPanel>,
) -> crate::services::market_data::SubscriptionHandle {
    let handle = cx
        .global::<crate::services::market_data::MarketDataServiceHandle>()
        .0
        .clone();
    handle.update(cx, |svc, cx| svc.ensure(symbol, tf, session, cx))
}

/// Ensure the chart's primary `(symbol, tf, session)` sub plus, when the
/// primary is regular hours, an Extended companion sub. Returns the resulting
/// handles; the panel stores them so they outlive the call. The companion
/// feeds the pre/post-market price indicator overlaid on the RTH chart — it
/// has no effect on the visible buffer (the chart's KlineEvent filter ignores
/// the other session) and is shared across chart panels via the service's
/// keyed sub map.
fn ensure_chart_subs(
    symbol: &str,
    tf: crate::services::market_data::Timeframe,
    primary: crate::services::market_data::Session,
    cx: &mut Context<ContentPanel>,
) -> Vec<crate::services::market_data::SubscriptionHandle> {
    let mut handles = vec![ensure_sub(symbol, tf, primary, cx)];
    if primary == crate::services::market_data::Session::Regular {
        handles.push(ensure_sub(
            symbol,
            tf,
            crate::services::market_data::Session::Extended,
            cx,
        ));
    }
    handles
}

/// Per-chart-panel preferences persisted *inside the dock layout* (the panel's
/// `PanelInfo::Panel` JSON), so each chart panel restores its own symbol +
/// timeframe + session independently — in both the auto-saved and named
/// layouts.
#[derive(Serialize, Deserialize)]
struct ChartPrefs {
    symbol: String,
    tf: String,
    /// Wire value (`regular` / `extended`). Optional so old persisted layouts
    /// without this field deserialize cleanly and default to regular hours.
    #[serde(default)]
    session: Option<String>,
}

/// Recover a chart panel's `(symbol, timeframe, session)` from its persisted
/// `PanelInfo`, if present and still valid.
fn chart_prefs_from_info(
    info: &PanelInfo,
) -> Option<(
    SharedString,
    crate::services::market_data::Timeframe,
    crate::services::market_data::Session,
)> {
    let PanelInfo::Panel(value) = info else {
        return None;
    };
    let prefs: ChartPrefs = serde_json::from_value(value.clone()).ok()?;
    let tf = crate::services::market_data::Timeframe::from_str(&prefs.tf)?;
    let session = prefs
        .session
        .as_deref()
        .and_then(crate::services::market_data::Session::from_str)
        .unwrap_or(crate::services::market_data::DEFAULT_SESSION);
    Some((SharedString::from(prefs.symbol), tf, session))
}

/// Weak handle to the root [`DockArea`], published by the workspace at startup.
/// Lets a panel request a (debounced) layout save when its own persisted state
/// changes — leaf-panel events don't bubble to the dock, so we nudge it here.
#[derive(Clone)]
pub struct DockAreaHandle(pub WeakEntity<DockArea>);
impl Global for DockAreaHandle {}

/// Push the focused chart's ticker into [`DetailsService`] so the
/// Details panel re-renders for the new symbol. Called from every code
/// path that changes the focused chart's symbol — initial focus, tab
/// activation, body mouse-down, and in-place symbol switch.
fn push_details_focus(symbol: &str, cx: &mut Context<ContentPanel>) {
    let Some(svc) = cx
        .try_global::<crate::services::details::DetailsServiceHandle>()
        .map(|h| h.0.clone())
    else {
        return;
    };
    let sym = SharedString::from(symbol.to_string());
    svc.update(cx, |s, cx| s.set_focused_symbol(Some(sym), cx));
}

/// Ask the workspace to re-save the layout (it debounces + diffs), capturing
/// updated panel state. No-op until the dock handle is published.
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
    chat_input: Option<Entity<InputState>>,
    /// Tracks which AI Chat session the InputState currently mirrors. Used by
    /// the AiChatEvent subscription to know when to swap the input value on
    /// session change. `Some` only when `kind == Kind::AiChat`.
    displayed_session_id: Option<String>,
    /// Scroll handle for the AI Chat messages list. Set only when
    /// `kind == Kind::AiChat`. The MessageAppended subscription uses it to
    /// force the view to the bottom on every new message (user prompt or
    /// stub assistant response), regardless of the current scroll position.
    ai_chat_scroll: Option<ScrollHandle>,
    /// Execution-panel inputs. Set only when `kind == Kind::Execution`.
    exec_inputs: Option<ExecutionInputs>,
    /// Chart-panel viewport state (symbol, candles, pan/zoom). Set only when
    /// `kind == Kind::Chart`. `pub(crate)` so the `chart` submodule's
    /// `cx.listener` closures can `as_mut()` it directly.
    pub(crate) chart_state: Option<chart::ChartState>,
    /// Heartbeat task that flushes a deferred chart paint when WS ticks
    /// arrive faster than `CHART_TICK_INTERVAL_MS`. Held here so it's
    /// dropped (cancelled) when the panel goes away. The flag + timestamp
    /// it watches live inside the closure captures, not on `Self`.
    _chart_tick_flush: Option<Task<()>>,
    /// 1Hz wall-clock heartbeat — only spawned for chart panels. Notifies on
    /// every fire so the live-price countdown re-renders each second, and
    /// rolls a flat continuation bar forward when no real tick has arrived
    /// for the next bucket yet (fixes "no new bars during a price-quiet
    /// minute"). Dropped with the panel.
    _chart_clock_tick: Option<Task<()>>,
    /// Observes [`crate::prefs::UserTz`] so chart panels repaint with new
    /// x-axis time labels the instant the user picks a timezone in Settings.
    /// Without this the chart waits for the next 1Hz clock tick or some
    /// unrelated state change before the swap shows up. Only `Some` for chart
    /// panels.
    _tz_subscription: Option<gpui::Subscription>,
    /// Per-(session_id, message_index) markdown-render state for assistant
    /// bubbles. Created lazily in render; pushed-to from the StreamingDelta
    /// subscription. Set only when `kind == Kind::AiChat`. The state holds
    /// the *joined* text of all `ContentBlock::Text` blocks in the message
    /// — `ChatMsg::text()` is the source of truth. Tool-use chips render
    /// below the markdown text in the same bubble.
    pub(crate) ai_chat_markdown: HashMap<(String, usize), AiChatMarkdownEntry>,
    /// Live market-data subscription handles owned by chart panels. Replaced
    /// (old handles dropped → server-side unsubscribe) on every symbol /
    /// timeframe / session change. Empty for non-chart kinds.
    chart_sub_handles: Vec<crate::services::market_data::SubscriptionHandle>,
    /// Live market-data subscription handles for watchlist rows, keyed by
    /// ticker. Reconciled by the WatchlistEvent subscription when symbols are
    /// added or removed. Empty for non-watchlist kinds.
    watchlist_sub_handles: HashMap<SharedString, crate::services::market_data::SubscriptionHandle>,
}

/// One assistant message's live markdown-render handle. `pushed_bytes`
/// tracks how much of `ChatMsg::text()` has already been fed to `state`
/// so the streaming subscription pushes only the tail per delta.
pub(crate) struct AiChatMarkdownEntry {
    pub state: Entity<TextViewState>,
    pub pushed_bytes: usize,
}

#[derive(Clone)]
pub struct ExecutionInputs {
    pub symbol: Entity<InputState>,
    pub quantity: Entity<InputState>,
    pub limit: Entity<InputState>,
}

impl ContentPanel {
    pub fn new(kind: Kind, window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self::new_inner(kind, None, window, cx)
    }

    /// Like [`Self::new`] but restores persisted per-panel state (currently a
    /// chart's symbol + timeframe) from the dock layout's `PanelInfo`.
    pub fn new_restored(
        kind: Kind,
        info: &PanelInfo,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new_inner(kind, chart_prefs_from_info(info), window, cx)
    }

    fn new_inner(
        kind: Kind,
        chart_prefs: Option<(
            SharedString,
            crate::services::market_data::Timeframe,
            crate::services::market_data::Session,
        )>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        // Only the AI chat panel has a real input; other kinds don't pay the InputState cost.
        // `auto_grow` wraps long prompts and grows the input up to 4 rows before
        // it starts internal scrolling — capped low so a long pre-filled
        // prompt doesn't squeeze the message area in a short panel.
        let chat_input = matches!(kind, Kind::AiChat).then(|| {
            cx.new(|cx| {
                InputState::new(window, cx)
                    .auto_grow(1, 4)
                    .placeholder("Ask anything…")
            })
        });
        let exec_inputs = matches!(kind, Kind::Execution).then(|| ExecutionInputs {
            symbol: cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("AAPL")
                    .default_value("AAPL")
            }),
            quantity: cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("100")
                    .default_value("100")
            }),
            limit: cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("0.00")
                    .default_value("192.15")
            }),
        });
        let mut chart_handles: Vec<crate::services::market_data::SubscriptionHandle> = Vec::new();
        let chart_state = matches!(kind, Kind::Chart).then(|| {
            // Restored prefs win; otherwise default timeframe + the server's
            // first symbol (static fallback until `/v1/symbols` loads). Pull the
            // live snapshot *now* so the chart paints real data on first frame;
            // an empty Vec just yields an empty chart until `Resnap` fills it.
            let (symbol, tf, session) = match &chart_prefs {
                Some((s, t, sn)) => (s.clone(), *t, *sn),
                None => {
                    let default_tf = chart::ChartState::default_timeframe();
                    let default_session = chart::ChartState::default_session();
                    let default_symbol: SharedString = cx
                        .global::<crate::services::symbols::SymbolsServiceHandle>()
                        .0
                        .read(cx)
                        .default_symbol()
                        .unwrap_or_else(|| {
                            SharedString::from(chart::ChartState::default_symbol())
                        });
                    (default_symbol, default_tf, default_session)
                }
            };
            chart_handles = ensure_chart_subs(symbol.as_ref(), tf, session, cx);
            let live = live_snapshot(symbol.as_ref(), tf, session, cx);
            chart::ChartState::new(symbol.as_ref(), tf, session, live)
        });
        // Watchlist panels re-render on watchlist mutations + market-data ticks
        // for the rows they show. `initial_handles` seeds the panel's
        // subscription handle map with the current watchlist; subscribe()
        // reconciles on WatchlistEvent.
        let watchlist_handles = if matches!(kind, Kind::Watchlist) {
            let h = watchlist::initial_handles(cx);
            watchlist::subscribe(window, cx);
            h
        } else {
            HashMap::new()
        };
        // Signal panel + SignalDetail repaint on engine updates and selection
        // changes.
        if matches!(kind, Kind::Signal | Kind::SignalDetail) {
            signal::subscribe(cx);
        }
        // Chart panels re-render when the symbols universe loads so the
        // header switches from the static fallback to the server list. The
        // shared symbol picker has its own subscription for its list items.
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
        }
        // Cross-chart sync: when another chart of the same symbol commits an
        // edit, this chart's snapshot needs to refresh. Filter by symbol so
        // mutations on unrelated symbols don't trigger spurious re-renders.
        // SelectionChanged + Wiped always re-render — they're rare and
        // workspace-wide anyway.
        if matches!(kind, Kind::Chart) {
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
        // Chart panels subscribe to `MarketDataService` for live ticks
        // (mutate-last / append on `Tick`) and full re-snapshots after
        // reconnect (`Resnap`). Filter by current (symbol, tf) so an update for
        // another chart's selection doesn't disturb this one. StatusChanged
        // just triggers a re-render so the LIVE / Reconnecting badge updates.
        //
        // `chart_tick_pending` + `chart_tick_last_ms` coalesce Tick notifies
        // so the chart paints at most ~20 Hz regardless of how chatty the
        // upstream stream is. The candle buffer is updated every tick; only
        // `cx.notify` is rate-limited. See `CHART_TICK_INTERVAL_MS`.
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
                move |this, _service, event: &crate::services::market_data::KlineEvent, window, cx| {
                    use crate::services::market_data::KlineEvent::*;
                    let Some(state) = this.chart_state.as_mut() else {
                        return;
                    };
                    match event {
                        Tick { symbol, tf, session, candle, is_closed } => {
                            if state.symbol().as_ref() != symbol.as_ref()
                                || state.timeframe() != *tf
                                || state.session() != *session
                            {
                                return;
                            }
                            // Always update the buffer — the developing bar
                            // must reflect the latest OHLC even if we skip
                            // this paint.
                            state.apply_tick(candle.clone(), *is_closed);
                            let now_ms = chrono::Utc::now().timestamp_millis();
                            let elapsed = now_ms - last_ms.get();
                            if elapsed >= CHART_TICK_INTERVAL_MS {
                                last_ms.set(now_ms);
                                pending.set(false);
                                cx.notify();
                            } else {
                                // Mark a paint as pending; the flush task
                                // spawned in `Self::new` checks this every
                                // `CHART_TICK_INTERVAL_MS` and notifies if
                                // set. This guarantees the latest bar
                                // surfaces even after a burst of throttled
                                // ticks stops.
                                pending.set(true);
                            }
                        }
                        Resnap { symbol, tf, session } => {
                            if state.symbol().as_ref() == symbol.as_ref()
                                && state.timeframe() == *tf
                                && state.session() == *session
                            {
                                let snap = live_snapshot(symbol.as_ref(), *tf, *session, cx)
                                    .unwrap_or_default();
                                state.resnap(snap);
                                cx.notify();
                            }
                        }
                        Prepended { symbol, tf, session, added } => {
                            // Older history landed: adopt the longer snapshot and
                            // shift the viewport + drawings right by `added` so
                            // the chart stays put on the bars the user was viewing.
                            if state.symbol().as_ref() == symbol.as_ref()
                                && state.timeframe() == *tf
                                && state.session() == *session
                            {
                                let snap = live_snapshot(symbol.as_ref(), *tf, *session, cx)
                                    .unwrap_or_default();
                                state.apply_prepend(snap, *added);
                                cx.notify();
                            }
                        }
                        HistoryCapped { symbol, tf, session } => {
                            if state.symbol().as_ref() == symbol.as_ref()
                                && state.timeframe() == *tf
                                && state.session() == *session
                            {
                                let msg = format!(
                                    "Showing the maximum {} candles of history for {} · {}.",
                                    crate::services::market_data::MAX_CANDLES,
                                    symbol,
                                    tf.as_str()
                                );
                                window.push_notification(
                                    gpui_component::notification::Notification::warning(
                                        SharedString::from(msg),
                                    )
                                    .title("History limit reached"),
                                    cx,
                                );
                            }
                        }
                        StatusChanged { .. } => {
                            cx.notify();
                        }
                    }
                },
            )
            .detach();
            // Tail-flush heartbeat — only spawned for chart panels. Wakes
            // every `CHART_TICK_INTERVAL_MS` and flushes a pending paint if
            // the subscription marked one. Held in `_chart_tick_flush` so
            // it's dropped with the panel.
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

            // 1Hz wall-clock heartbeat — drives the live-price countdown and
            // synthesises the next bar when wall-clock has crossed the
            // current bar's close_time but no real tick has landed yet.
            //
            // Sleep is aligned to the next wall-clock second boundary (+ a
            // small skew so we wake just past it, not on it). A flat
            // `Duration::from_secs(1)` would drift any time the executor
            // takes longer than 1s to deliver a wake, and once cumulative
            // drift crosses a second, the displayed countdown skips a value
            // — that's the "not counting evenly" symptom.
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
            // Repaint the chart immediately on a timezone change. Without
            // this the chart waits for the next 1Hz heartbeat (~up to a
            // second of stale labels) or, worse, never repaints if nothing
            // else triggers a notify between the user's click and a viewport
            // interaction.
            tz_subscription = Some(cx.observe_global::<crate::prefs::UserTz>(|_, cx| {
                cx.notify();
            }));
        }
        // AI Chat panels wire their InputState to the service's draft, and
        // subscribe to selection / staging events so external `Ask AI`
        // dispatches and sidebar clicks reflect immediately in the input bar.
        let (displayed_session_id, ai_chat_scroll) = if matches!(kind, Kind::AiChat) {
            let input_ref = chat_input
                .as_ref()
                .expect("chat_input created for AiChat above");
            let scroll = ScrollHandle::new();
            let id = ai_chat::subscribe(input_ref, &scroll, window, cx);
            // Seed the input with whatever draft the active session held.
            if let (Some(input), Some(id_ref)) = (chat_input.as_ref(), id.as_ref()) {
                let svc = cx
                    .global::<crate::services::ai_chat::AiChatServiceHandle>()
                    .0
                    .clone();
                let draft = svc
                    .read(cx)
                    .session(id_ref)
                    .map(|s| s.draft.clone())
                    .unwrap_or_default();
                input.update(cx, |state, cx| {
                    state.set_value(SharedString::from(draft), window, cx);
                });
            }
            (id, Some(scroll))
        } else {
            (None, None)
        };
        Self {
            kind,
            focus_handle,
            parent_tab_panel: None,
            chat_input,
            exec_inputs,
            chart_state,
            _chart_tick_flush: chart_tick_flush,
            _chart_clock_tick: chart_clock_tick,
            _tz_subscription: tz_subscription,
            displayed_session_id,
            ai_chat_scroll,
            ai_chat_markdown: HashMap::new(),
            chart_sub_handles: chart_handles,
            watchlist_sub_handles: watchlist_handles,
        }
    }

    /// Switch the chart to `target`, ensuring its (symbol, tf, session)
    /// subscription and seeding the initial snapshot. Driven by the symbol
    /// picker's confirm event and by the watchlist's FocusSymbol routing.
    /// Add an indicator to this panel's chart (no-op for non-chart kinds).
    /// Dispatched by `IndicatorPickerState::apply` when the user confirms a
    /// row. Auto-color rotation and recompute happen inside `ChartState`.
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
        let session = state.session();
        // Replace handles first so old ones drop after the new ones exist —
        // refcount-stable when both old/new resolve to the same SubKey.
        let new_handles = ensure_chart_subs(target, tf, session, cx);
        self.chart_sub_handles = new_handles;
        let live = live_snapshot(target, tf, session, cx);
        if state.switch_symbol(target, live) {
            cx.notify();
            request_layout_save(cx);
            // If this chart is the focused one, the Details panel needs to
            // follow the new symbol. Skipped when an unfocused chart's
            // symbol changes (the user's focus is still on whichever chart
            // owned the prior Details symbol).
            if self.is_focused(cx) {
                push_details_focus(target, cx);
            }
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
        let session = state.session();
        let new_handles = ensure_chart_subs(symbol.as_ref(), tf, session, cx);
        self.chart_sub_handles = new_handles;
        let live = live_snapshot(symbol.as_ref(), tf, session, cx);
        if state.switch_timeframe(tf, live) {
            cx.notify();
            request_layout_save(cx);
        }
    }

    pub fn switch_chart_session(
        &mut self,
        session: crate::services::market_data::Session,
        cx: &mut Context<Self>,
    ) {
        let Some(state) = self.chart_state.as_mut() else {
            return;
        };
        let symbol = state.symbol().clone();
        let tf = state.timeframe();
        let new_handles = ensure_chart_subs(symbol.as_ref(), tf, session, cx);
        self.chart_sub_handles = new_handles;
        let live = live_snapshot(symbol.as_ref(), tf, session, cx);
        if state.switch_session(session, live) {
            cx.notify();
            request_layout_save(cx);
        }
    }

    pub fn chart_timeframe(&self) -> Option<crate::services::market_data::Timeframe> {
        self.chart_state.as_ref().map(|s| s.timeframe())
    }

    /// If the chart has scrolled close to its oldest loaded bar, ask the service
    /// to page in older history. Cheap + fully guarded (no-ops when not near the
    /// edge, already loading, capped, or exhausted), so it's safe to call from
    /// every pan/zoom gesture.
    fn maybe_load_older(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.chart_state.as_ref() else {
            return;
        };
        if !state.wants_older() {
            return;
        }
        let symbol = state.symbol().clone();
        let tf = state.timeframe();
        let session = state.session();
        let handle = cx
            .global::<crate::services::market_data::MarketDataServiceHandle>()
            .0
            .clone();
        handle.update(cx, |svc, cx| svc.load_older(symbol.as_ref(), tf, session, cx));
    }

    fn on_change_chart_timeframe(
        &mut self,
        action: &ChangeChartTimeframe,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(state) = self.chart_state.as_mut() else {
            return;
        };
        let Some(tf) = crate::services::market_data::Timeframe::from_str(action.0.as_ref()) else {
            return;
        };
        let symbol = state.symbol().clone();
        let session = state.session();
        let new_handles = ensure_chart_subs(symbol.as_ref(), tf, session, cx);
        self.chart_sub_handles = new_handles;
        let live = live_snapshot(symbol.as_ref(), tf, session, cx);
        if state.switch_timeframe(tf, live) {
            cx.notify();
            request_layout_save(cx);
        }
    }

    fn on_change_chart_session(
        &mut self,
        action: &ChangeChartSession,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(session) =
            crate::services::market_data::Session::from_str(action.0.as_ref())
        else {
            return;
        };
        self.switch_chart_session(session, cx);
    }

    fn on_delete_selected_drawing(
        &mut self,
        _: &crate::drawings::actions::DeleteSelectedDrawing,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Selection + drawings now live on the workspace-wide DrawingService;
        // the chart-scoped binding still routes here, but the action just
        // forwards to the service.
        let svc = cx
            .global::<crate::drawings::service::DrawingServiceHandle>()
            .0
            .clone();
        svc.update(cx, |s, cx| {
            s.delete_selected(cx);
        });
    }

    // ----- Indicator chip context-menu handlers -----

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
        // Bound from the chart's right-click context menu; clears every
        // drawing on the focused chart's symbol.
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

    /// The AI Chat input. `Some` only when `kind == Kind::AiChat`. Lets the
    /// workspace prefill the prompt for the "Ask AI" flow.
    pub fn chat_input(&self) -> Option<&Entity<InputState>> {
        self.chat_input.as_ref()
    }

    fn mark_focused(&self, cx: &mut App) {
        // Singleton panels (AI Chat, Position, Execution) are host-managed:
        // they live in pinned TabPanels and shouldn't become the "+ Panel"
        // drop target, so we don't record their parent here. Using `self.kind`
        // (rather than reading the parent TabPanel) is essential — this fires
        // from inside `TabPanel::set_active_ix`'s update closure, so reading
        // the parent's `is_pinned` would double-borrow the TabPanel.
        if self.kind.is_singleton() {
            return;
        }
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

    fn title(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        SharedString::from(self.kind.display())
    }

    /// Chart tabs show the current symbol (e.g. "AAPL") rather than the
    /// generic "Chart" — when the user has 3 chart panels open, the tab
    /// strip becomes useful only if each tab names its symbol. Other kinds
    /// fall through to the default (`None` → `title()` is used).
    fn tab_name(&self, _cx: &App) -> Option<SharedString> {
        match self.kind {
            Kind::Chart => self.chart_state.as_ref().map(|s| s.symbol().clone()),
            _ => None,
        }
    }

    /// Suppress per-tab close in constrained modes. Free Layout keeps the
    /// default (closable = true). Note: TabPanel also returns false when the
    /// panel is pinned, so locked singletons stay safe even in Free Layout.
    fn closable(&self, cx: &App) -> bool {
        match cx.global::<CurrentModeGlobal>().0 {
            crate::persistence::Mode::FreeLayout => true,
            _ => false,
        }
    }

    /// Suppress zoom in constrained modes — locked layouts shouldn't blow
    /// up to fill the workspace and shadow the locked structure.
    fn zoomable(&self, _cx: &App) -> Option<PanelControl> {
        match _cx.global::<CurrentModeGlobal>().0 {
            crate::persistence::Mode::FreeLayout => Some(PanelControl::Menu),
            _ => None,
        }
    }

    /// Persist per-panel state into the dock layout. Chart panels stash their
    /// current symbol + timeframe so each restores independently; other kinds
    /// use the default (null) info.
    fn dump(&self, _cx: &App) -> PanelState {
        let mut state = PanelState::new(self);
        if let Some(chart) = &self.chart_state {
            if let Ok(value) = serde_json::to_value(ChartPrefs {
                symbol: chart.symbol().to_string(),
                tf: chart.timeframe().as_str().to_string(),
                session: Some(chart.session().as_str().to_string()),
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

    /// Clear the cached parent on detach so callers can tell the panel is no
    /// longer in the dock tree (e.g. user closed the tab). For drag-between
    /// TabPanels, on_added_to immediately re-sets the parent on the destination.
    fn on_removed(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        // If this is a Chart that owned the Details focus, drop it. The
        // Details panel will fall to its empty state until the user clicks
        // another chart. Comparing the LastFocusedChart entity_id avoids
        // stomping on a sibling chart's focus.
        if self.kind == Kind::Chart {
            let mine = cx.weak_entity().entity_id();
            let still_focused = cx
                .try_global::<LastFocusedChart>()
                .and_then(|g| g.0.borrow().as_ref().map(|w| w.entity_id()))
                == Some(mine);
            if still_focused {
                if let Some(svc) = cx
                    .try_global::<crate::services::details::DetailsServiceHandle>()
                    .map(|h| h.0.clone())
                {
                    svc.update(cx, |s, cx| s.set_focused_symbol(None, cx));
                }
            }
        }
        self.parent_tab_panel = None;
    }

    // Tab-changes also count as focus changes. on_focus_in handles body clicks; this handles
    // tab-strip clicks where the active tab swaps.
    fn set_active(&mut self, active: bool, _window: &mut Window, cx: &mut Context<Self>) {
        if active {
            self.mark_focused(cx);
            if self.kind == Kind::Chart {
                let weak = cx.weak_entity();
                let global = cx.global::<LastFocusedChart>().0.clone();
                *global.borrow_mut() = Some(weak);
                if let Some(sym) =
                    self.chart_state.as_ref().map(|s| s.symbol().clone())
                {
                    push_details_focus(sym.as_ref(), cx);
                }
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
            Kind::Portfolio => portfolio::render(window, cx).into_any_element(),
            Kind::Notification => notifications::render(window, cx).into_any_element(),
            Kind::SmartMoney => smart_money::render(window, cx).into_any_element(),
            Kind::AiChat => ai_chat::render(self, window, cx).into_any_element(),
            Kind::Position => position::render(window, cx).into_any_element(),
            Kind::Execution => execution::render(
                self.exec_inputs
                    .as_ref()
                    .expect("exec_inputs set for Execution"),
                window,
                cx,
            )
            .into_any_element(),
            Kind::Trump => trump::render(window, cx).into_any_element(),
            Kind::Signal => signal::render(window, cx).into_any_element(),
            Kind::SignalDetail => signal::render_detail(window, cx).into_any_element(),
            Kind::Screener => screener::render(window, cx).into_any_element(),
            Kind::Geopolitics => geopolitics::render(window, cx).into_any_element(),
            // EconomicCalendar, Filings, Insider, News, Details have custom
            // Panel impls — they should never reach ContentPanel::render. If
            // they do, render a placeholder rather than panicking so a
            // misrouted persisted layout entry doesn't crash the workspace.
            Kind::EconomicCalendar
            | Kind::Filings
            | Kind::Insider
            | Kind::News
            | Kind::Details => div()
                .p_4()
                .text_sm()
                .child(SharedString::from(format!(
                    "{} panel: misrouted to ContentPanel — close + re-add via +Panel",
                    self.kind.display()
                )))
                .into_any_element(),
        };
        // AiChat manages its own internal scroll region (so its input bar stays pinned at
        // the bottom). Chart fills the available space so the canvas can flex with the
        // panel (no vertical scroll). Every other kind gets a single outer scroll wrapper
        // so long lists don't get clipped when the panel shrinks.
        let body = if matches!(self.kind, Kind::AiChat | Kind::Chart) {
            raw_body
        } else {
            div()
                .id(SharedString::from(format!("scroll-{}", self.kind.id())))
                .size_full()
                .overflow_y_scroll()
                .child(raw_body)
                .into_any_element()
        };
        // Border is always 2px wide so toggling focus doesn't shift content; only the color
        // changes between transparent (unfocused) and theme.ring (focused).
        let border_color = if self.is_focused(cx) {
            cx.theme().ring
        } else {
            gpui::transparent_black()
        };
        // Click-based focus tracking (NOT track_focus / on_focus_in) — gpui's web focus
        // mechanism uses a hidden input element which makes mobile browsers pop the soft
        // keyboard on every tap. Mouse-down works the same on touch and doesn't claim
        // text-input focus.
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
                        if let Some(sym) =
                            this.chart_state.as_ref().map(|s| s.symbol().clone())
                        {
                            push_details_focus(sym.as_ref(), cx);
                        }
                    }
                }),
            )
            // `track_focus` is the focus channel the chart's timeframe-selector
            // popup uses to dispatch `ChangeChartTimeframe` back into this panel.
            // We scope it to Chart only because gpui's web focus mechanism
            // creates a hidden <input>, and broadly applied focus tracking pops
            // the soft keyboard on mobile (see CLAUDE.md).
            .when(matches!(self.kind, Kind::Chart), |this| {
                this.track_focus(&self.focus_handle)
                    // Marks the chart-panel ancestry in the key-context
                    // stack so `DeleteSelectedDrawing` only fires here (and
                    // doesn't steal Backspace from a focused text Input).
                    .key_context("Chart")
                    .on_action(cx.listener(Self::on_change_chart_timeframe))
                    .on_action(cx.listener(Self::on_change_chart_session))
                    .on_action(cx.listener(Self::on_move_indicator_pane_up))
                    .on_action(cx.listener(Self::on_move_indicator_pane_down))
                    .on_action(cx.listener(Self::on_toggle_indicator_hidden))
                    .on_action(cx.listener(Self::on_remove_indicator))
                    .on_action(cx.listener(Self::on_delete_selected_drawing))
                    .on_action(cx.listener(Self::on_clear_drawings))
                    .on_action(cx.listener(Self::on_reset_chart_scale))
                    .on_action(cx.listener(Self::on_go_to_latest))
            })
            .size_full()
            .border_2()
            .border_color(border_color)
            .child(body)
    }
}

