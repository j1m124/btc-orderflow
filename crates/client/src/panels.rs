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
pub mod liquidations;
pub mod orderbook;
pub mod trades;
pub mod watchlist;

pub use chart::{
    ChangeChartRender, ChangeChartTimeframe, ChangeChartVolumeUnit, ChartRenderSettingsView,
    GoToLatest, OpenChartRenderSettings, ResetChartScale, ToggleChartRenderVisible,
};
pub use liquidations::{ChangeLiquidationsSideFilter, ChangeLiquidationsSizeMode};
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
    Liquidations,
}

impl Kind {
    pub const ALL: &'static [Kind] = &[
        Kind::Watchlist,
        Kind::Chart,
        Kind::Trades,
        Kind::Orderbook,
        Kind::Liquidations,
    ];

    pub fn id(self) -> &'static str {
        match self {
            Kind::Watchlist => "Watchlist",
            Kind::Chart => "Chart",
            Kind::Trades => "Trades",
            Kind::Orderbook => "Orderbook",
            Kind::Liquidations => "Liquidations",
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
        // ESC drops the global drawing selection — hides the floating
        // settings strip + closes any open gear window. Bound on Chart so
        // input fields elsewhere still treat ESC as their own cancel.
        gpui::KeyBinding::new(
            "escape",
            crate::drawings::actions::DeselectDrawing,
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
    /// Render kind id (`"candlestick"` / `"cluster"` / `"profile"`). New in
    /// v5; older persisted state defaults to Candlestick via `serde(default)`.
    #[serde(default)]
    render_kind: Option<String>,
    /// Per-mode params. Both are written even when only one is active so
    /// switching modes after restore preserves the user's last settings
    /// in each. New in v5; older state seeds Candlestick defaults.
    #[serde(default)]
    cluster: Option<chart::FootprintParams>,
    #[serde(default)]
    profile: Option<chart::FootprintParams>,
    /// Volume display unit (`"coin"` / `"usd"`). Per-chart, not global.
    /// New in v6; older state defaults to Coin via `serde(default)`.
    #[serde(default)]
    volume_unit: Option<String>,
    /// Attached indicator instances (kind + params + presentation state).
    /// New in v7; older state defaults to an empty Vec via `serde(default)`,
    /// which combined with `ChartState::new` no longer seeding a default
    /// Volume means restored charts pre-v7 boot up with zero indicators —
    /// matches the new "fresh chart starts empty" behaviour.
    #[serde(default)]
    indicators: Vec<IndicatorPrefs>,
}

/// Serialized form of one `IndicatorInstance`. Kind reconstruction goes
/// through `crate::indicators::build_kind(kind_id, &params)`; unknown
/// `kind_id`s are silently dropped on restore (forward-compat with future
/// kinds and removal of legacy ones).
#[derive(Serialize, Deserialize, Clone)]
pub(crate) struct IndicatorPrefs {
    pub kind_id: String,
    pub params: serde_json::Value,
    pub placement: PlacementPref,
    #[serde(default)]
    pub pane_height: Option<f32>,
    #[serde(default)]
    pub colors: Vec<HslaPref>,
    #[serde(default)]
    pub hidden: bool,
    /// Persisted across reloads so drawings anchored to
    /// `PaneRef::Indicator(InstanceId)` keep their target. `None` on
    /// pre-feature blobs — restore mints a fresh id in that case.
    #[serde(default)]
    pub id: Option<u64>,
}

#[derive(Serialize, Deserialize, Clone, Copy)]
pub(crate) enum PlacementPref {
    Overlay,
    Pane,
}

impl From<crate::indicators::Placement> for PlacementPref {
    fn from(p: crate::indicators::Placement) -> Self {
        match p {
            crate::indicators::Placement::Overlay => PlacementPref::Overlay,
            crate::indicators::Placement::Pane => PlacementPref::Pane,
        }
    }
}

impl PlacementPref {
    pub fn into_placement(self) -> crate::indicators::Placement {
        match self {
            PlacementPref::Overlay => crate::indicators::Placement::Overlay,
            PlacementPref::Pane => crate::indicators::Placement::Pane,
        }
    }
}

/// HSLA as plain f32 quadruple — sidesteps depending on gpui's `Hsla`
/// serde derive (which isn't guaranteed). Lossless round-trip via
/// `gpui::hsla(h, s, l, a)`.
#[derive(Serialize, Deserialize, Clone, Copy)]
pub(crate) struct HslaPref {
    pub h: f32,
    pub s: f32,
    pub l: f32,
    pub a: f32,
}

impl From<gpui::Hsla> for HslaPref {
    fn from(c: gpui::Hsla) -> Self {
        Self {
            h: c.h,
            s: c.s,
            l: c.l,
            a: c.a,
        }
    }
}

impl HslaPref {
    pub fn into_hsla(self) -> gpui::Hsla {
        gpui::hsla(self.h, self.s, self.l, self.a)
    }
}

/// Restored chart prefs after parsing from persisted PanelInfo. Render-mode
/// fields are optional — older v3/v4 state loads cleanly with `None` for
/// each, in which case `ChartState::new` seeds the defaults.
struct ChartRestored {
    symbol: SharedString,
    tf: crate::services::market_data::Timeframe,
    render_kind: chart::RenderKind,
    cluster: chart::FootprintParams,
    profile: chart::FootprintParams,
    volume_unit: crate::persistence::VolumeUnit,
    /// May be empty — older blobs without the field deserialize to `[]`,
    /// and that's also the intentional fresh-chart default now.
    indicators: Vec<IndicatorPrefs>,
}

fn parse_volume_unit(s: &str) -> Option<crate::persistence::VolumeUnit> {
    match s {
        "coin" => Some(crate::persistence::VolumeUnit::Coin),
        "usd" => Some(crate::persistence::VolumeUnit::Usd),
        _ => None,
    }
}

fn volume_unit_id(u: crate::persistence::VolumeUnit) -> &'static str {
    match u {
        crate::persistence::VolumeUnit::Coin => "coin",
        crate::persistence::VolumeUnit::Usd => "usd",
    }
}

/// Map a USD notional to a row-tint alpha that keeps small orders faint and
/// makes large ones pop. The trades tape and liquidations tape both tint rows
/// by size, so sharing this keeps their grading curves identical.
///
/// The notional is clamped to `[$100, $5M]` and log-normalized to `[0, 1]`
/// (≈4.7 decades), then run through a gamma ease-in (`t²`) and scaled into
/// `[floor, ceil]`. The square curve is the point: a plain linear-over-log
/// ramp spends too much of its alpha range on the common small prints
/// (a $5k order ends up nearly as tinted as a $500k one), so the squaring
/// pushes the low/mid end toward `floor` and reserves the strong tints near
/// `ceil` for the rare whale orders the user actually wants to spot. Widening
/// the top of the range to $5M (vs the old $1M clamp) lets multi-million
/// prints separate from the merely-large instead of all flattening to the
/// same ceiling.
pub fn size_tint_alpha(usd: f64, floor: f32, ceil: f32) -> f32 {
    // $100 → 0.0, $5M → 1.0 on a log scale.
    let t = (((usd.max(100.0).log10() - 2.0) / 4.7) as f32).clamp(0.0, 1.0);
    floor + t * t * (ceil - floor)
}

fn chart_prefs_from_info(info: &PanelInfo) -> Option<ChartRestored> {
    let PanelInfo::Panel(value) = info else {
        return None;
    };
    let prefs: ChartPrefs = serde_json::from_value(value.clone()).ok()?;
    let tf = crate::services::market_data::Timeframe::from_str(&prefs.tf)?;
    let render_kind = prefs
        .render_kind
        .as_deref()
        .and_then(chart::RenderKind::from_id)
        .unwrap_or_default();
    let cluster = prefs
        .cluster
        .unwrap_or_else(chart::FootprintParams::cluster_default);
    let profile = prefs
        .profile
        .unwrap_or_else(chart::FootprintParams::profile_default);
    let volume_unit = prefs
        .volume_unit
        .as_deref()
        .and_then(parse_volume_unit)
        .unwrap_or_default();
    Some(ChartRestored {
        symbol: SharedString::from(prefs.symbol),
        tf,
        render_kind,
        cluster,
        profile,
        volume_unit,
        indicators: prefs.indicators,
    })
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

#[derive(Serialize, Deserialize)]
struct LiquidationsPrefs {
    symbol: String,
    #[serde(default)]
    min_size: Option<f64>,
    #[serde(default)]
    size_mode: Option<String>,
    #[serde(default)]
    side_filter: Option<String>,
}

fn liquidations_prefs_from_info(
    info: &PanelInfo,
) -> Option<(
    SharedString,
    Option<f64>,
    liquidations::LiquidationsSizeMode,
    liquidations::LiquidationsSideFilter,
)> {
    let PanelInfo::Panel(value) = info else {
        return None;
    };
    let prefs: LiquidationsPrefs = serde_json::from_value(value.clone()).ok()?;
    let size_mode = prefs
        .size_mode
        .as_deref()
        .and_then(liquidations::LiquidationsSizeMode::from_id)
        .unwrap_or_default();
    let side_filter = prefs
        .side_filter
        .as_deref()
        .and_then(liquidations::LiquidationsSideFilter::from_id)
        .unwrap_or_default();
    Some((
        SharedString::from(prefs.symbol),
        prefs.min_size,
        size_mode,
        side_filter,
    ))
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

/// Same role as `TRADES_PERSIST_CAP` for the liquidations tape — a min-size
/// threshold can hold rare big prints in the buffer indefinitely; capped so
/// long sessions don't accumulate unbounded.
const LIQUIDATIONS_PERSIST_CAP: usize = 5_000;

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

pub(crate) fn request_layout_save(cx: &mut Context<ContentPanel>) {
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
    /// Recomputes indicators (volume / volume-delta convert their values
    /// per the global `volume_unit`) and repaints when the user changes
    /// any chart-wide setting (volume unit, price decimals, …).
    _chart_prefs_subscription: Option<gpui::Subscription>,
    chart_sub_handles: Vec<crate::services::market_data::SubscriptionHandle>,
    /// Live footprint subscriptions for the chart panel, keyed by the
    /// bucket size's `f64::to_bits` (matches `FootprintSubKey`'s keying).
    /// One entry per **distinct bucket** in use:
    ///   * the chart's own render bucket (if Cluster / Profile mode), AND
    ///   * one per VRVP indicator instance (Phase 5+) and FRVP drawing
    ///     (Phase 12+) whose bucket differs from the chart's.
    /// Refcounting at the `MarketDataService` level dedupes when the same
    /// bucket is requested by multiple owners — see `ensure_footprint`.
    footprint_subs: HashMap<u64, crate::services::market_data::SubscriptionHandle>,
    /// Per-bucket throttle for VP-driven footprint history paging — the
    /// last time (ms) we asked the service for older cells on that bucket.
    /// `MarketDataService::load_older_footprint` already dedupes against
    /// an in-flight set, but throttling here keeps the panel from spamming
    /// requests every paint frame the moment a response arrives. 200ms
    /// per the design grilling.
    vp_history_last_request_ms: HashMap<u64, i64>,
    /// Throttle for the liquidation-bars history-fill loop. Single slot
    /// (the bar indicator's sub is keyed only on `(symbol, tf)`, so there's
    /// only ever one in-flight target at a time on this chart). Same 200ms
    /// cadence as the footprint variant.
    liq_bars_history_last_request_ms: Option<i64>,
    /// `(symbol, tf, bucket_bits)` for the **chart's own** footprint render
    /// (Cluster / Profile modes). The FootprintEvent subscription matches
    /// on this to know which sub's cells to copy into `state.footprint_cells`
    /// for the chart's render path. VP-only buckets are tracked solely via
    /// `footprint_subs` and read on demand by the VP compute / paint code.
    chart_footprint_key:
        Option<(SharedString, crate::services::market_data::Timeframe, u64)>,
    /// `(symbol, tf, handle)` for the chart's liquidation-bars subscription.
    /// Allocated lazily when at least one `liq_bars` instance is on the
    /// chart; dropped when the last instance is removed or (symbol, tf)
    /// changes. `MarketDataService::ensure_liquidation_bars` refcounts so
    /// repeated reconciles are cheap.
    chart_liq_bars_sub: Option<(
        SharedString,
        crate::services::market_data::Timeframe,
        crate::services::market_data::SubscriptionHandle,
    )>,
    watchlist_sub_handles: HashMap<SharedString, crate::services::market_data::SubscriptionHandle>,
    pub(crate) trades_symbol: Option<SharedString>,
    pub(crate) trades_min_usd: Option<Option<f64>>,
    pub(crate) trades_size_mode: Option<trades::TradesSizeMode>,
    pub(crate) trades_filter_input: Option<Entity<InputState>>,
    pub(crate) trades_persist: Option<VecDeque<crate::services::market_data::Trade>>,
    _trades_sub_handle: Option<crate::services::market_data::SubscriptionHandle>,
    _trades_input_subscription: Option<gpui::Subscription>,
    pub(crate) orderbook_state: Option<OrderbookState>,
    pub(crate) liquidations_symbol: Option<SharedString>,
    pub(crate) liquidations_min_size: Option<Option<f64>>,
    pub(crate) liquidations_size_mode: Option<liquidations::LiquidationsSizeMode>,
    pub(crate) liquidations_side_filter: Option<liquidations::LiquidationsSideFilter>,
    pub(crate) liquidations_filter_input: Option<Entity<InputState>>,
    pub(crate) liquidations_persist:
        Option<VecDeque<crate::services::market_data::Liquidation>>,
    _liquidations_sub_handle: Option<crate::services::market_data::SubscriptionHandle>,
    _liquidations_input_subscription: Option<gpui::Subscription>,
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
        Self::new_inner(kind, None, None, None, None, window, cx)
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
            liquidations_prefs_from_info(info),
            window,
            cx,
        )
    }

    fn new_inner(
        kind: Kind,
        chart_prefs: Option<ChartRestored>,
        trades_prefs: Option<(SharedString, Option<f64>, trades::TradesSizeMode)>,
        orderbook_prefs: Option<(
            SharedString,
            orderbook::OrderbookBucket,
            orderbook::OrderbookSizeMode,
        )>,
        liquidations_prefs: Option<(
            SharedString,
            Option<f64>,
            liquidations::LiquidationsSizeMode,
            liquidations::LiquidationsSideFilter,
        )>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        let mut chart_handles: Vec<crate::services::market_data::SubscriptionHandle> = Vec::new();
        let chart_state = matches!(kind, Kind::Chart).then(|| {
            let (symbol, tf, render_seed, indicator_seed) = match &chart_prefs {
                Some(restored) => (
                    restored.symbol.clone(),
                    restored.tf,
                    Some((
                        restored.render_kind,
                        restored.cluster,
                        restored.profile,
                        restored.volume_unit,
                    )),
                    restored.indicators.clone(),
                ),
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
                    (default_symbol, default_tf, None, Vec::new())
                }
            };
            chart_handles = vec![ensure_chart_sub(symbol.as_ref(), tf, cx)];
            let live = live_snapshot(symbol.as_ref(), tf, cx);
            let mut state = chart::ChartState::new(symbol.as_ref(), tf, live);
            if let Some((kind, cluster, profile, volume_unit)) = render_seed {
                state.seed_render(kind, cluster, profile);
                state.set_volume_unit(volume_unit);
            }
            if !indicator_seed.is_empty() {
                state.restore_indicators(indicator_seed);
            }
            // Re-run indicator math against the (possibly) restored volume
            // unit + indicator list so the chart doesn't show one stale frame
            // before settling.
            state.recompute_indicators();
            state
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
                                    // A drawing on our symbol changed —
                                    // refresh footprint subs in case an
                                    // FRVP was added / removed / had its
                                    // bucket edited. Idempotent: same
                                    // desired set ⇒ no churn.
                                    this.refresh_chart_footprint_sub(cx);
                                    cx.notify();
                                }
                            }
                        }
                        Wiped => {
                            // Drawings wiped across every symbol — every
                            // FRVP just disappeared, so any FRVP-only
                            // footprint sub should be released.
                            this.refresh_chart_footprint_sub(cx);
                            cx.notify();
                        }
                        SelectionChanged => {
                            cx.notify();
                        }
                    }
                },
            )
            .detach();
            // Tool-state changes drive the FRVP-on-sub-pane not-allowed
            // cursor. Without this subscription the cursor only flips
            // after the user moves the mouse — the gesture works either
            // way, but the affordance lags.
            let tool_handle = cx
                .global::<crate::drawings::tool::DrawingToolStateHandle>()
                .0
                .clone();
            cx.subscribe(
                &tool_handle,
                |_this, _tool, _ev: &crate::drawings::tool::DrawingToolEvent, cx| {
                    cx.notify();
                },
            )
            .detach();
        }
        let chart_tick_pending = Rc::new(Cell::new(false));
        let chart_tick_last_ms = Rc::new(Cell::new(0i64));
        let mut chart_tick_flush: Option<Task<()>> = None;
        let mut chart_clock_tick: Option<Task<()>> = None;
        let mut tz_subscription: Option<gpui::Subscription> = None;
        let mut chart_prefs_subscription: Option<gpui::Subscription> = None;
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
                    // The chart's identity gates everything — events for a
                    // (symbol, tf) the chart isn't watching are dropped.
                    let Some((chart_symbol, chart_tf)) = this
                        .chart_state
                        .as_ref()
                        .map(|s| (s.symbol().clone(), s.timeframe()))
                    else {
                        return;
                    };
                    let matches = match event {
                        Snapshot { symbol, tf, .. }
                        | Update { symbol, tf, .. }
                        | Prepended { symbol, tf, .. }
                        | HistoryCapped { symbol, tf }
                        | Resnap { symbol, tf } => {
                            symbol.as_ref() == chart_symbol.as_ref() && *tf == chart_tf
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
                            // Snapshot every active bucket up-front so the
                            // borrow on `this` (via `footprint_subs.keys`)
                            // is released before we take `chart_state.as_mut`.
                            let bucket_bits_list: Vec<u64> =
                                this.footprint_subs.keys().copied().collect();
                            let primary_key = this.chart_footprint_key.clone();
                            let cells_by_bucket: Vec<(u64, Vec<_>)> = bucket_bits_list
                                .into_iter()
                                .map(|bits| {
                                    let bucket = f64::from_bits(bits);
                                    let cells = market.read(cx).footprint_cells(
                                        chart_symbol.as_ref(),
                                        chart_tf,
                                        bucket,
                                    );
                                    (bits, cells)
                                })
                                .collect();
                            // The chart's own render path still goes through
                            // `footprint_cells` (not the per-bucket cache),
                            // so refresh it separately for whichever bucket
                            // the chart is currently displaying.
                            let primary_cells = primary_key.and_then(|(s, t, bits)| {
                                (s.as_ref() == chart_symbol.as_ref() && t == chart_tf).then(
                                    || {
                                        let bucket = f64::from_bits(bits);
                                        market.read(cx).footprint_cells(
                                            chart_symbol.as_ref(),
                                            chart_tf,
                                            bucket,
                                        )
                                    },
                                )
                            });
                            if let Some(state) = this.chart_state.as_mut() {
                                for (bits, cells) in cells_by_bucket {
                                    state.set_footprint_cache_bucket(bits, cells);
                                }
                                if let Some(cells) = primary_cells {
                                    state.set_footprint_cells(cells);
                                }
                                // VRVP outputs depend on the cache; force a
                                // recompute now so the next paint draws the
                                // refreshed profile rather than stale data.
                                state.recompute_indicators();
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
            // LiquidationBarEvent → copy the per-bar cells into ChartState's
            // liquidation_bars_cache. Same gating as the FootprintEvent
            // subscription: events for a (symbol, tf) the chart isn't
            // watching are dropped.
            cx.subscribe_in(
                &service,
                window,
                |this,
                 _service,
                 event: &crate::services::market_data::LiquidationBarEvent,
                 _window,
                 cx| {
                    use crate::services::market_data::LiquidationBarEvent::*;
                    let Some((chart_symbol, chart_tf)) = this
                        .chart_state
                        .as_ref()
                        .map(|s| (s.symbol().clone(), s.timeframe()))
                    else {
                        return;
                    };
                    let matches = match event {
                        Snapshot { symbol, tf, .. }
                        | Update { symbol, tf, .. }
                        | Prepended { symbol, tf, .. }
                        | HistoryCapped { symbol, tf }
                        | Resnap { symbol, tf } => {
                            symbol.as_ref() == chart_symbol.as_ref() && *tf == chart_tf
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
                            let bars = market
                                .read(cx)
                                .liquidation_bars(chart_symbol.as_ref(), chart_tf);
                            if let Some(state) = this.chart_state.as_mut() {
                                state.set_liquidation_bars_cache(bars);
                                state.recompute_indicators();
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
            // Repaint on any global chart-prefs change (price decimals,
            // default view, …). The volume-unit toggle is now per-chart
            // (header dropdown → `set_chart_volume_unit`), so this
            // subscription no longer needs to recompute indicators.
            chart_prefs_subscription = Some(cx.observe_global::<crate::prefs::ChartPrefsGlobal>(
                |_this, cx| {
                    cx.notify();
                },
            ));
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
        let (
            liquidations_symbol,
            liquidations_min_size,
            liquidations_size_mode,
            liquidations_side_filter,
            liquidations_filter_input,
            liquidations_persist,
            _liquidations_sub_handle,
            _liquidations_input_subscription,
        ) = if matches!(kind, Kind::Liquidations) {
            let (sym, min_size, size_mode, side_filter) = liquidations_prefs.unwrap_or_else(|| {
                (
                    default_symbol(cx),
                    None,
                    liquidations::LiquidationsSizeMode::default(),
                    liquidations::LiquidationsSideFilter::default(),
                )
            });
            let handle = cx
                .global::<crate::services::market_data::MarketDataServiceHandle>()
                .0
                .clone()
                .update(cx, |svc, cx| svc.ensure_liquidations(sym.as_ref(), cx));
            let market = cx
                .global::<crate::services::market_data::MarketDataServiceHandle>()
                .0
                .clone();
            cx.subscribe_in(
                &market,
                window,
                |this, _svc, ev: &crate::services::market_data::LiquidationEvent, _window, cx| {
                    use crate::services::market_data::LiquidationEvent::*;
                    match ev {
                        Snapshot { symbol, .. } | Resnap { symbol } => {
                            if this
                                .liquidations_symbol
                                .as_deref()
                                .map_or(true, |s| s != symbol.as_ref())
                            {
                                return;
                            }
                            this.reseed_liquidations_persist(cx);
                            this.tick_seq = this.tick_seq.wrapping_add(1);
                            cx.notify();
                        }
                        Tick { symbol, liquidations } => {
                            if this
                                .liquidations_symbol
                                .as_deref()
                                .map_or(true, |s| s != symbol.as_ref())
                            {
                                return;
                            }
                            this.append_liquidations_persist(liquidations.iter().cloned(), cx);
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
            let initial_text: SharedString = match min_size {
                Some(v) if v > 0.0 => SharedString::from(format!("{:.0}", v)),
                _ => SharedString::default(),
            };
            let input_state =
                cx.new(|cx| InputState::new(window, cx).placeholder("Min size"));
            if !initial_text.is_empty() {
                input_state.update(cx, |s, cx| s.set_value(initial_text, window, cx));
            }
            let input_sub = cx.subscribe_in(
                &input_state,
                window,
                |this, input, ev: &InputEvent, window, cx| {
                    if matches!(ev, InputEvent::Change) {
                        let text = input.read(cx).value();
                        this.apply_liquidations_min_size_from_text(text.as_ref(), window, cx);
                    }
                },
            );
            (
                Some(sym),
                Some(min_size),
                Some(size_mode),
                Some(side_filter),
                Some(input_state),
                Some(VecDeque::new()),
                Some(handle),
                Some(input_sub),
            )
        } else {
            (None, None, None, None, None, None, None, None)
        };
        let mut new_self = Self {
            kind,
            focus_handle,
            parent_tab_panel: None,
            chart_state,
            _chart_tick_flush: chart_tick_flush,
            _chart_clock_tick: chart_clock_tick,
            _tz_subscription: tz_subscription,
            _chart_prefs_subscription: chart_prefs_subscription,
            chart_sub_handles: chart_handles,
            footprint_subs: HashMap::new(),
            vp_history_last_request_ms: HashMap::new(),
            liq_bars_history_last_request_ms: None,
            chart_footprint_key: None,
            chart_liq_bars_sub: None,
            watchlist_sub_handles: watchlist_handles,
            trades_symbol,
            trades_min_usd,
            trades_size_mode,
            trades_filter_input,
            trades_persist,
            _trades_sub_handle,
            _trades_input_subscription,
            orderbook_state,
            liquidations_symbol,
            liquidations_min_size,
            liquidations_size_mode,
            liquidations_side_filter,
            liquidations_filter_input,
            liquidations_persist,
            _liquidations_sub_handle,
            _liquidations_input_subscription,
            tick_seq: 0,
        };
        // Seed the trades persist with whatever passing prints are already
        // in the service ring at mount — otherwise the panel reads as empty
        // until the next live tick.
        if matches!(kind, Kind::Trades) {
            new_self.reseed_trades_persist(cx);
        }
        if matches!(kind, Kind::Liquidations) {
            new_self.reseed_liquidations_persist(cx);
        }
        // Allocate the footprint sub for a restored chart whose persisted
        // render kind is Cluster / Profile. No-op for Candlestick (the
        // refresh helper short-circuits when needs_footprint_sub() is false).
        if matches!(kind, Kind::Chart) {
            new_self.refresh_chart_footprint_sub(cx);
            new_self.refresh_chart_liq_bars_sub(cx);
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

    // --- Liquidations panel helpers ---

    pub fn apply_liquidations_min_size_from_text(
        &mut self,
        text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let parsed = liquidations::parse_min_size(text);
        let Some(current) = self.liquidations_min_size.as_mut() else {
            return;
        };
        if *current == parsed {
            return;
        }
        *current = parsed;
        self.reseed_liquidations_persist(cx);
        cx.notify();
        request_layout_save(cx);
    }

    pub fn reseed_liquidations_persist(&mut self, cx: &mut Context<Self>) {
        let Some(symbol) = self.liquidations_symbol.clone() else {
            return;
        };
        let Some(persist) = self.liquidations_persist.as_mut() else {
            return;
        };
        let min_size = self.liquidations_min_size.unwrap_or(None);
        let size_mode = self.liquidations_size_mode.unwrap_or_default();
        persist.clear();
        let market = cx
            .global::<crate::services::market_data::MarketDataServiceHandle>()
            .0
            .clone();
        if let Some(snap) = market.read(cx).liquidations(symbol.as_ref()) {
            for l in snap.iter() {
                if liquidations::passes_min_size(l, min_size, size_mode) {
                    persist.push_back(l.clone());
                }
            }
            while persist.len() > LIQUIDATIONS_PERSIST_CAP {
                persist.pop_front();
            }
        }
    }

    pub fn append_liquidations_persist<I>(&mut self, liqs: I, _cx: &mut Context<Self>)
    where
        I: IntoIterator<Item = crate::services::market_data::Liquidation>,
    {
        let min_size = self.liquidations_min_size.unwrap_or(None);
        let size_mode = self.liquidations_size_mode.unwrap_or_default();
        let Some(persist) = self.liquidations_persist.as_mut() else {
            return;
        };
        for l in liqs {
            if liquidations::passes_min_size(&l, min_size, size_mode) {
                persist.push_back(l);
            }
        }
        while persist.len() > LIQUIDATIONS_PERSIST_CAP {
            persist.pop_front();
        }
    }

    pub fn set_liquidations_size_mode(
        &mut self,
        mode: liquidations::LiquidationsSizeMode,
        cx: &mut Context<Self>,
    ) {
        let Some(current) = self.liquidations_size_mode.as_mut() else {
            return;
        };
        if *current == mode {
            return;
        }
        *current = mode;
        // Size mode change re-interprets the threshold against a new unit —
        // reseed so the filter applies under the new interpretation.
        self.reseed_liquidations_persist(cx);
        cx.notify();
        request_layout_save(cx);
    }

    pub fn set_liquidations_side_filter(
        &mut self,
        filter: liquidations::LiquidationsSideFilter,
        cx: &mut Context<Self>,
    ) {
        let Some(current) = self.liquidations_side_filter.as_mut() else {
            return;
        };
        if *current == filter {
            return;
        }
        *current = filter;
        // Side filter doesn't change the persist buffer (filter is applied
        // at render time only) — just re-render and persist.
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
        // VRVP joins the desired-bucket set; refresh so its sub is
        // allocated. Cheap no-op for non-VP kinds (the desired set is
        // unchanged).
        self.refresh_chart_footprint_sub(cx);
            self.refresh_chart_liq_bars_sub(cx);
        cx.notify();
        request_layout_save(cx);
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
            self.refresh_chart_liq_bars_sub(cx);
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
            // The selected drawing might have a tf_filter that excludes the
            // new TF — clear the global selection so the floating settings
            // strip closes instead of orphaning over a now-invisible
            // drawing. Mirrors the chart's per-TF visibility rule.
            if let Some(handle) = cx
                .try_global::<crate::drawings::service::DrawingServiceHandle>()
                .cloned()
            {
                let tf_str = tf.as_str();
                let should_clear = handle
                    .0
                    .read(cx)
                    .selected_drawing()
                    .map(|(_, d)| !d.visible_on(tf_str))
                    .unwrap_or(false);
                if should_clear {
                    handle.0.update(cx, |s, cx| s.clear_selection(cx));
                }
            }
            self.refresh_chart_footprint_sub(cx);
            self.refresh_chart_liq_bars_sub(cx);
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
            self.refresh_chart_liq_bars_sub(cx);
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
            self.refresh_chart_liq_bars_sub(cx);
            cx.notify();
            request_layout_save(cx);
        }
    }

    /// Reconcile `footprint_subs` with every bucket the chart panel needs
    /// live cells for: the chart's own render bucket (when Cluster /
    /// Profile mode active) plus — in later phases — one entry per VRVP
    /// indicator instance and FRVP drawing whose bucket differs from the
    /// chart's. Idempotent: same desired set ⇒ no churn.
    ///
    /// Order matters when a bucket changes: drop the *old* handle (so the
    /// service refcount settles to zero and a WS `Unsubscribe` fires)
    /// **before** allocating the new one. Otherwise two subs at the same
    /// `(symbol, tf, bucket)` would race when the user shifts the chart's
    /// bucket through a value also held by a VP instance.
    pub(crate) fn refresh_chart_footprint_sub(&mut self, cx: &mut Context<Self>) {
        let Some((symbol, tf)) = self.chart_state.as_ref().map(|s| (s.symbol().clone(), s.timeframe()))
        else {
            // No chart on this panel — make sure no subs leak.
            if !self.footprint_subs.is_empty() {
                self.footprint_subs.clear();
            }
            self.chart_footprint_key = None;
            return;
        };

        // The chart's own render-driven bucket (the one whose cells get
        // copied into `state.footprint_cells` for the candle-pane render).
        let chart_bucket: Option<f64> = self.chart_state.as_ref().and_then(|state| {
            if !state.render_kind().needs_footprint_sub() {
                return None;
            }
            let params = state.active_footprint_params()?;
            chart::FootprintParams::bucket_is_valid(params.bucket).then_some(params.bucket)
        });

        // Desired bucket set across all consumers: the chart's render-mode
        // bucket plus one entry per VRVP instance (FRVP joins this list in
        // Phase 12). Duplicates collapse via the bit-pattern key.
        let mut desired: HashMap<u64, f64> = HashMap::new();
        if let Some(b) = chart_bucket {
            desired.insert(b.to_bits(), b);
        }
        if let Some(state) = self.chart_state.as_ref() {
            for inst in state.indicators() {
                if let Some(vrvp) = inst
                    .kind
                    .as_any()
                    .downcast_ref::<crate::indicators::VrvpParams>()
                {
                    let b = vrvp.params.bucket_dollars();
                    if b.is_finite() && b > 0.0 {
                        desired.insert(b.to_bits(), b);
                    }
                }
            }
        }
        // FRVP drawings on the same symbol also need their bucket subscribed —
        // the shared `volume_profile::compute_volume_profile` reads from the
        // same per-bucket cache that VRVP uses. Read the service once;
        // drawings carry no liveness concept, so every persisted FRVP on
        // this symbol contributes its bucket regardless of whether the
        // bracket is currently inside the viewport.
        if let Some(handle) = cx.try_global::<crate::drawings::service::DrawingServiceHandle>().cloned() {
            let svc = handle.0.read(cx);
            for d in svc.for_symbol(symbol.as_ref()) {
                if let crate::drawings::shapes::DrawingShape::Frvp(s) = &d.shape {
                    let b = s.params.bucket_dollars();
                    if b.is_finite() && b > 0.0 {
                        desired.insert(b.to_bits(), b);
                    }
                }
            }
        }

        // Drop any sub whose bucket left the desired set, plus its cached
        // cells — otherwise VRVP would keep reading stale aggregates after
        // the user changes the bucket / removes the instance.
        let to_drop: Vec<u64> = self
            .footprint_subs
            .keys()
            .copied()
            .filter(|k| !desired.contains_key(k))
            .collect();
        for k in to_drop {
            // Explicit drop before any later allocate to let the refcount
            // settle. (Same intent as the old single-sub path.)
            self.footprint_subs.remove(&k);
            if let Some(state) = self.chart_state.as_mut() {
                state.clear_footprint_cache_bucket(k);
            }
        }

        // Allocate any sub whose bucket newly appeared in the desired set.
        let market = cx
            .global::<crate::services::market_data::MarketDataServiceHandle>()
            .0
            .clone();
        for (bits, bucket) in desired.iter() {
            if self.footprint_subs.contains_key(bits) {
                continue;
            }
            let handle = market.clone().update(cx, |svc, cx| {
                svc.ensure_footprint(symbol.as_ref(), tf, *bucket, cx)
            });
            self.footprint_subs.insert(*bits, handle);
        }

        // Update the chart-render key separately. The FootprintEvent
        // subscription reads this to know which sub's snapshot it should
        // copy into `state.footprint_cells`. VP-only buckets are NOT
        // surfaced here — VP code reads them directly via the lookup cache.
        let new_chart_key = chart_bucket.map(|b| (symbol.clone(), tf, b.to_bits()));
        if new_chart_key != self.chart_footprint_key {
            if let Some(state) = self.chart_state.as_mut() {
                // Clear cells when the chart's bucket itself shifts (entered/
                // left Cluster/Profile mode, or the user changed the bucket
                // input). VP-only sub churn doesn't trigger this.
                state.clear_footprint_cells();
            }
            self.chart_footprint_key = new_chart_key;
        }
    }

    /// Reconcile the chart's liquidation-bars subscription against current
    /// indicator state. Liq_bars has only one keying dimension (the chart's
    /// `(symbol, tf)`), so this is a single-slot allocate / drop rather than
    /// the multi-bucket diff that footprint subs do.
    pub(crate) fn refresh_chart_liq_bars_sub(&mut self, cx: &mut Context<Self>) {
        let want = self.chart_state.as_ref().and_then(|state| {
            // Active when there's a dedicated liq_bars instance, OR a
            // bar_stat instance with a liquidation row enabled. The
            // bar_stat consumer reads the same per-bar series via
            // `ComputeCtx.liquidation_bars`, so a single refcounted sub
            // covers both.
            let any_live = state.indicators().iter().any(|i| {
                if i.kind_id == "liq_bars" {
                    return true;
                }
                if let Some(bs) = i
                    .kind
                    .as_any()
                    .downcast_ref::<crate::indicators::BarStatParams>()
                {
                    return bs.show_long_liq || bs.show_short_liq;
                }
                false
            });
            any_live.then(|| (state.symbol().clone(), state.timeframe()))
        });
        let cur = self
            .chart_liq_bars_sub
            .as_ref()
            .map(|(s, t, _)| (s.clone(), *t));
        if want == cur {
            return;
        }
        // Drop the old sub (if any) before allocating so refcounts settle —
        // same intent as the footprint refresh path.
        if cur.is_some() {
            self.chart_liq_bars_sub = None;
            if let Some(state) = self.chart_state.as_mut() {
                state.clear_liquidation_bars_cache();
                state.recompute_indicators();
            }
        }
        if let Some((symbol, tf)) = want {
            let market = cx
                .global::<crate::services::market_data::MarketDataServiceHandle>()
                .0
                .clone();
            let handle = market.clone().update(cx, |svc, cx| {
                svc.ensure_liquidation_bars(symbol.as_ref(), tf, cx)
            });
            // Seed cache from whatever the service already has.
            let seeded = market
                .read(cx)
                .liquidation_bars(symbol.as_ref(), tf);
            if let Some(state) = self.chart_state.as_mut() {
                state.set_liquidation_bars_cache(seeded);
                state.recompute_indicators();
            }
            self.chart_liq_bars_sub = Some((symbol, tf, handle));
        }
    }

    /// For each live VRVP, request older footprint cells if the visible
    /// window extends past the oldest loaded cell for the instance's
    /// bucket. Throttled per-bucket via `vp_history_last_request_ms` to
    /// avoid hammering the WS during sustained pan into history; the
    /// service-side `footprint_history_in_flight` set is the second line
    /// of defense (a duplicate request slot-collides and no-ops).
    fn maybe_request_vp_history(&mut self, cx: &mut Context<Self>) {
        let Some((symbol, tf)) = self
            .chart_state
            .as_ref()
            .map(|s| (s.symbol().clone(), s.timeframe()))
        else {
            return;
        };
        let view_lo = match self.chart_state.as_ref().and_then(|s| s.view_time_range()) {
            Some((lo, _)) => lo,
            None => return,
        };
        // Collect the (bucket_bits, bucket_dollars) pairs to query so we
        // release the `&self.chart_state` immutable borrow before the
        // service call below (which mutably borrows `cx.global`).
        // VRVP wants coverage up to the *viewport* left edge; FRVP wants
        // coverage up to its own `a_time`. Either trigger backfills the
        // same bucket — `wanted_lo[bits] = min(over all consumers)` so we
        // only fire one request per bucket per throttle window.
        let mut wanted_lo: HashMap<u64, (f64, i64)> = HashMap::new();
        if let Some(state) = self.chart_state.as_ref() {
            for inst in state.indicators() {
                if let Some(vrvp) = inst
                    .kind
                    .as_any()
                    .downcast_ref::<crate::indicators::VrvpParams>()
                {
                    let bucket = vrvp.params.bucket_dollars();
                    if !bucket.is_finite() || bucket <= 0.0 {
                        continue;
                    }
                    let bits = bucket.to_bits();
                    // Skip if we've fetched recently or have enough coverage
                    // already.
                    let oldest_loaded = state.oldest_footprint_cell_time(bits);
                    if let Some(oldest) = oldest_loaded {
                        if oldest <= view_lo {
                            continue; // Have everything we need.
                        }
                    }
                    wanted_lo
                        .entry(bits)
                        .and_modify(|(_, lo)| *lo = (*lo).min(view_lo))
                        .or_insert((bucket, view_lo));
                }
            }
        }
        // FRVPs on this symbol: each one wants cells back to its `a_time`,
        // regardless of the viewport (an FRVP can sit fully off-screen and
        // still need its history loaded so it stays accurate when the user
        // scrolls back to it).
        if let Some(handle) = cx.try_global::<crate::drawings::service::DrawingServiceHandle>().cloned() {
            let svc = handle.0.read(cx);
            for d in svc.for_symbol(symbol.as_ref()) {
                let crate::drawings::shapes::DrawingShape::Frvp(s) = &d.shape else {
                    continue;
                };
                let bucket = s.params.bucket_dollars();
                if !bucket.is_finite() || bucket <= 0.0 {
                    continue;
                }
                let bits = bucket.to_bits();
                let need_lo = s.a_time.min(s.b_time);
                if let Some(state) = self.chart_state.as_ref() {
                    if let Some(oldest) = state.oldest_footprint_cell_time(bits) {
                        if oldest <= need_lo {
                            continue;
                        }
                    }
                }
                wanted_lo
                    .entry(bits)
                    .and_modify(|(_, lo)| *lo = (*lo).min(need_lo))
                    .or_insert((bucket, need_lo));
            }
        }
        let wanted: Vec<(u64, f64)> = wanted_lo
            .into_iter()
            .map(|(bits, (bucket, _lo))| (bits, bucket))
            .collect();
        if wanted.is_empty() {
            return;
        }
        const THROTTLE_MS: i64 = 200;
        let now_ms = chrono::Utc::now().timestamp_millis();
        let market = cx
            .global::<crate::services::market_data::MarketDataServiceHandle>()
            .0
            .clone();
        for (bits, bucket) in wanted {
            let last = self.vp_history_last_request_ms.get(&bits).copied().unwrap_or(0);
            if now_ms - last < THROTTLE_MS {
                continue;
            }
            self.vp_history_last_request_ms.insert(bits, now_ms);
            market.update(cx, |svc, cx| {
                svc.load_older_footprint(symbol.as_ref(), tf, bucket, cx)
            });
        }
    }

    /// Liquidation-bars history fill — fires `load_older_liquidation_bars`
    /// when the visible view extends past the oldest loaded bar. Single
    /// 200ms throttle (only one sub per chart). No-op when no `liq_bars`
    /// instance is live on the chart or coverage already reaches the view.
    fn maybe_request_liq_bars_history(&mut self, cx: &mut Context<Self>) {
        let Some((symbol, tf)) = self
            .chart_state
            .as_ref()
            .map(|s| (s.symbol().clone(), s.timeframe()))
        else {
            return;
        };
        if self.chart_liq_bars_sub.is_none() {
            return;
        }
        let view_lo = match self.chart_state.as_ref().and_then(|s| s.view_time_range()) {
            Some((lo, _)) => lo,
            None => return,
        };
        // Oldest loaded bar in the cache; coverage already reaches view if
        // it's <= view_lo. Cache is sorted ascending by open_time, so the
        // first entry is the oldest.
        if let Some(state) = self.chart_state.as_ref() {
            if let Some(oldest) = state.oldest_liquidation_bar_time() {
                if oldest <= view_lo {
                    return;
                }
            }
        }
        const THROTTLE_MS: i64 = 200;
        let now_ms = chrono::Utc::now().timestamp_millis();
        if let Some(last) = self.liq_bars_history_last_request_ms {
            if now_ms - last < THROTTLE_MS {
                return;
            }
        }
        self.liq_bars_history_last_request_ms = Some(now_ms);
        let market = cx
            .global::<crate::services::market_data::MarketDataServiceHandle>()
            .0
            .clone();
        market.update(cx, |svc, cx| {
            svc.load_older_liquidation_bars(symbol.as_ref(), tf, cx)
        });
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

    fn on_change_chart_volume_unit(
        &mut self,
        action: &ChangeChartVolumeUnit,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(unit) = parse_volume_unit(action.0.as_ref()) else {
            return;
        };
        self.set_chart_volume_unit(unit, cx);
    }

    /// Apply a volume-unit choice to the chart and propagate: recompute
    /// indicators so Volume / Volume Delta / CVD reflect the new unit,
    /// repaint so the footprint paint pipeline picks it up too, and persist
    /// the choice via the dock-area layout-changed signal.
    pub fn set_chart_volume_unit(
        &mut self,
        unit: crate::persistence::VolumeUnit,
        cx: &mut Context<Self>,
    ) {
        let Some(state) = self.chart_state.as_mut() else {
            return;
        };
        if state.volume_unit() == unit {
            return;
        }
        state.set_volume_unit(unit);
        state.recompute_indicators();
        cx.notify();
        request_layout_save(cx);
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

    fn on_change_liquidations_size_mode(
        &mut self,
        action: &liquidations::ChangeLiquidationsSizeMode,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(mode) = liquidations::LiquidationsSizeMode::from_id(action.0.as_ref()) else {
            return;
        };
        self.set_liquidations_size_mode(mode, cx);
    }

    fn on_change_liquidations_side_filter(
        &mut self,
        action: &liquidations::ChangeLiquidationsSideFilter,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(filter) = liquidations::LiquidationsSideFilter::from_id(action.0.as_ref()) else {
            return;
        };
        self.set_liquidations_side_filter(filter, cx);
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
            request_layout_save(cx);
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
            request_layout_save(cx);
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
            request_layout_save(cx);
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
            // If the removed instance was VRVP, its bucket may have left
            // the desired set — refresh drops the sub + cache entry.
            self.refresh_chart_footprint_sub(cx);
            self.refresh_chart_liq_bars_sub(cx);
            cx.notify();
            request_layout_save(cx);
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
            Kind::Liquidations => self.liquidations_symbol.clone()?,
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
            let indicators = chart
                .indicators()
                .iter()
                .map(|inst| IndicatorPrefs {
                    kind_id: inst.kind_id.to_string(),
                    params: inst.kind.params_json(),
                    placement: inst.placement.into(),
                    pane_height: inst.pane_height,
                    colors: inst.colors.iter().copied().map(HslaPref::from).collect(),
                    hidden: inst.hidden,
                    id: Some(inst.id),
                })
                .collect();
            if let Ok(value) = serde_json::to_value(ChartPrefs {
                symbol: chart.symbol().to_string(),
                tf: chart.timeframe().as_str().to_string(),
                render_kind: Some(chart.render_kind().as_id().to_string()),
                cluster: Some(*chart.cluster_params()),
                profile: Some(*chart.profile_params()),
                volume_unit: Some(volume_unit_id(chart.volume_unit()).to_string()),
                indicators,
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
        if matches!(self.kind, Kind::Liquidations) {
            if let Some(sym) = &self.liquidations_symbol {
                let min_size = self.liquidations_min_size.unwrap_or(None);
                let size_mode = self.liquidations_size_mode.unwrap_or_default();
                let side_filter = self.liquidations_side_filter.unwrap_or_default();
                if let Ok(value) = serde_json::to_value(LiquidationsPrefs {
                    symbol: sym.to_string(),
                    min_size,
                    size_mode: Some(size_mode.id().to_string()),
                    side_filter: Some(side_filter.id().to_string()),
                }) {
                    state.info = PanelInfo::panel(value);
                }
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
            Kind::Chart => {
                // View-dependent indicators (VRVP) cache the visible time
                // range at their last compute; if the user has panned /
                // zoomed since, the cached output is stale. This lazy
                // recompute keeps VRVP's range tracking continuous without
                // sprinkling refresh calls through every pan/zoom site. The
                // check is O(indicators) + a couple of comparisons, and
                // short-circuits to a no-op when there's no VP instance.
                if let Some(chart) = self.chart_state.as_mut() {
                    chart.maybe_recompute_view_dependent_indicators();
                }
                self.maybe_request_vp_history(cx);
                self.maybe_request_liq_bars_history(cx);
                chart::render(
                    self.chart_state
                        .as_ref()
                        .expect("chart_state set for Chart"),
                    self.focus_handle.clone(),
                    window,
                    cx,
                )
                .into_any_element()
            }
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
            Kind::Liquidations => {
                let symbol = self
                    .liquidations_symbol
                    .clone()
                    .expect("liquidations_symbol set for Liquidations");
                let size_mode = self
                    .liquidations_size_mode
                    .expect("liquidations_size_mode set for Liquidations");
                let side_filter = self
                    .liquidations_side_filter
                    .expect("liquidations_side_filter set for Liquidations");
                let min_size = self.liquidations_min_size.unwrap_or(None);
                let input = self
                    .liquidations_filter_input
                    .clone()
                    .expect("liquidations_filter_input set for Liquidations");
                let persist_vec: Vec<crate::services::market_data::Liquidation> = self
                    .liquidations_persist
                    .as_ref()
                    .map(|p| p.iter().cloned().collect())
                    .unwrap_or_default();
                liquidations::render(
                    symbol,
                    &persist_vec,
                    size_mode,
                    side_filter,
                    min_size,
                    &input,
                    self.focus_handle.clone(),
                    window,
                    cx,
                )
                .into_any_element()
            }
        };
        let body = if matches!(
            self.kind,
            Kind::Chart | Kind::Trades | Kind::Orderbook | Kind::Liquidations
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
                    .on_action(cx.listener(Self::on_change_chart_volume_unit))
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
            .when(matches!(self.kind, Kind::Liquidations), |this| {
                this.track_focus(&self.focus_handle)
                    .key_context("Liquidations")
                    .on_action(cx.listener(Self::on_change_liquidations_size_mode))
                    .on_action(cx.listener(Self::on_change_liquidations_side_filter))
            })
            .size_full()
            .border_2()
            .border_color(border_color)
            .child(body)
    }
}
