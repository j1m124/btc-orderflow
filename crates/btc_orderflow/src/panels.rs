use std::cell::{Cell, RefCell};
use std::collections::HashMap;
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
};
use serde::{Deserialize, Serialize};

pub mod chart;
pub mod watchlist;

pub use chart::{ChangeChartTimeframe, GoToLatest, ResetChartScale};

/// Minimum interval between chart re-paints driven by tick events. 50ms = 20Hz.
const CHART_TICK_INTERVAL_MS: i64 = 50;

pub type PanelKind = Kind;
pub const PANEL_KINDS: &[Kind] = Kind::ALL;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    Watchlist,
    Chart,
}

impl Kind {
    pub const ALL: &'static [Kind] = &[Kind::Watchlist, Kind::Chart];

    pub fn id(self) -> &'static str {
        match self {
            Kind::Watchlist => "Watchlist",
            Kind::Chart => "Chart",
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
    watchlist_sub_handles: HashMap<SharedString, crate::services::market_data::SubscriptionHandle>,
}

impl ContentPanel {
    pub fn new(kind: Kind, window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self::new_inner(kind, None, window, cx)
    }

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
        chart_prefs: Option<(SharedString, crate::services::market_data::Timeframe)>,
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
        Self {
            kind,
            focus_handle,
            parent_tab_panel: None,
            chart_state,
            _chart_tick_flush: chart_tick_flush,
            _chart_clock_tick: chart_clock_tick,
            _tz_subscription: tz_subscription,
            chart_sub_handles: chart_handles,
            watchlist_sub_handles: watchlist_handles,
        }
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
            cx.notify();
            request_layout_save(cx);
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

    fn title(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        SharedString::from(self.kind.display())
    }

    fn tab_name(&self, _cx: &App) -> Option<SharedString> {
        match self.kind {
            Kind::Chart => self.chart_state.as_ref().map(|s| s.symbol().clone()),
            _ => None,
        }
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
        };
        let body = if matches!(self.kind, Kind::Chart) {
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
