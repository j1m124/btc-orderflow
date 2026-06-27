//! [`ChartState`] — the chart panel's model: candles, view window, y-range,
//! drawing-interaction anchors, indicators, and the footprint / liquidation /
//! open-interest read-through caches, plus every state-mutation method. Fields
//! and view-facing helpers are `pub(super)` so [`super::view`] can read and
//! drive the state; `panels.rs` only ever touches it through public methods.

use gpui::{Bounds, Hsla, Pixels, Point, SharedString, Window};

use super::coords::screen_to_index;
use super::drawing::{CreatingDrawing, EditDrag, TextEditing};
use super::footprint::{FootprintParams, RenderKind};
use crate::indicators::{
    ComputeCtx, IndicatorInstance, IndicatorKind, IndicatorOutput, InstanceId, Placement,
    palette_color_for,
};
use crate::persistence::VolumeUnit;
use crate::services::market_data::{self, Candle, Timeframe};

// ============================================================================
// Chart
// ============================================================================

/// `Candle` is owned by [`crate::services::market_data`] so the live service
/// can populate it directly — no translation layer between WS events and the
/// chart's render path.

// Default candles per viewport — now user-settable via Settings → General →
// Chart. The const remains as the seed value for the atomic in `prefs.rs`;
// runtime reads go through `crate::prefs::chart_default_view()`.
pub(super) const CHART_MIN_VIEW: f32 = 4.0;
/// Maximum candles visible at once — the hard zoom-out limit. Past ~1px per
/// candle the view is already aggregated (see the dense paint path), so showing
/// more adds no detail; this also keeps the whole buffer (up to 5,000 bars)
/// from being crammed on screen. Users pan to reach older history, not zoom.
/// Capped to the buffer length when fewer bars are loaded.
pub(super) const CHART_MAX_VIEW: f32 = 1000.0;
// Right-edge buffer ratio — now user-settable via Settings → General → Chart.
// Reads at runtime through `crate::prefs::chart_right_buffer()`.
/// Symmetric left buffer: lets the user pan/zoom-out past bar 0 into empty
/// space. Required so wheel-zoom-out can keep its right-edge anchor invariant
/// even when nearing the historical edge.
const CHART_LEFT_BUFFER_RATIO: f32 = 0.50;
/// Minimum vertical motion in pixels before a canvas drag starts panning Y.
/// Pure horizontal drags below this threshold leave `y_auto` alone so casual
/// time-scrubbing keeps the price axis auto-fitting.
pub(super) const Y_FREEZE_DEADZONE_PX: f32 = 4.0;
/// Pixels of wheel `delta_y` per one zoom unit — the divisor in the
/// exponential `factor = exp(-delta_y / SCROLL_ZOOM_RATE)` used by the
/// canvas, x-axis, and y-axis scroll handlers. 120 matches the historical
/// "one mouse-wheel notch" on Windows/Mac; lower values make the wheel
/// zoom more aggressive, higher values dampen it.
pub(super) const SCROLL_ZOOM_RATE: f32 = 240.0;

/// Per-drag state for canvas 2D panning. X always pans from `start_view_start`;
/// Y panning is "lazy" — `y_freeze` stays `None` until the drag accumulates
/// `Y_FREEZE_DEADZONE_PX` of vertical motion from `start_pos`. Once it trips,
/// we snapshot the price range at that instant and translate from that
/// baseline so the chart doesn't jump at threshold cross.
#[derive(Clone, Copy)]
pub(super) struct CanvasDrag {
    pub(super) start_pos: Point<Pixels>,
    pub(super) start_view_start: f32,
    pub(super) y_freeze: Option<(Point<Pixels>, f64, f64)>,
}

// `Tool` and `DrawingId` are imported from `crate::drawings::{tool, service}` —
// the chart no longer owns the active tool (it's a workspace-global state).

pub struct ChartState {
    pub(super) symbol: SharedString,
    /// Selected chart timeframe. Drives backfill/subscription and the x-axis
    /// step picker.
    pub(super) timeframe: Timeframe,
    pub(super) candles: Vec<Candle>,
    /// Fractional left-edge index of the visible window. Fractional so pan
    /// stays smooth at sub-candle granularity even though the chart paints
    /// integer-indexed bars. May go negative (left buffer) or extend past
    /// `total` (right buffer) by the ratios above.
    pub(super) view_start: f32,
    /// Number of candles visible in the viewport (fractional for the same
    /// reason as `view_start`).
    pub(super) view_size: f32,
    /// Set on left-mouse-down on the canvas, cleared on up. While present,
    /// mouse-move pans the view in 2D.
    pub(super) drag_anchor: Option<CanvasDrag>,
    /// Last painted bounds of the chart canvas. Captured via `on_prepaint`
    /// and consumed by drag/wheel handlers to convert pixel deltas into
    /// candle-space deltas.
    pub(super) bounds: Option<Bounds<Pixels>>,
    /// When true the price (y) axis auto-fits to the visible candles each
    /// render. Flipping to false locks the axis to (`y_min`, `y_max`) so
    /// users can drag/wheel the right edge to scale price independently.
    /// Restored to true via double-click on the right axis.
    pub(super) y_auto: bool,
    /// Locked price-axis range. Only consulted when `y_auto` is false.
    pub(super) y_min: f64,
    pub(super) y_max: f64,
    /// Drag anchor for vertical-only manipulation on the right axis:
    /// `(mouse_down_position, y_min_at_down, y_max_at_down)`.
    pub(super) y_drag_anchor: Option<(Point<Pixels>, f64, f64)>,
    /// Drag anchor for horizontal-only zoom on the bottom axis:
    /// `(mouse_down_position, view_size_at_down, view_start_at_down)`.
    /// `view_start_at_down` lets the zoom keep the viewport's centre at the
    /// position it was at drag-start, instead of recomputing the centre each
    /// frame (which drifts when `clamp` adjusts `view_start`).
    pub(super) x_axis_drag_anchor: Option<(Point<Pixels>, f32, f32)>,
    /// In-progress drawing being constructed (Line / Rect / position).
    /// Local to the chart that started the click-drag; broadcast to the
    /// service only on mouse-up so other charts of the same symbol don't see
    /// a half-drawn shape.
    pub(super) creating: Option<CreatingDrawing>,
    /// Active edit drag on an existing drawing (handle or body translation).
    /// Baseline is captured from the service at drag start, then the chart
    /// emits `preview_shape` on every move and a final `update_shape` on up.
    pub(super) edit_drag: Option<EditDrag>,
    /// Inline text editor. Single instance — only one text can be edited at
    /// a time. Committed on mouse-down outside the input.
    pub(super) editing_text: Option<TextEditing>,
    /// Cursor position in canvas-relative pixel coords. `Some` while the
    /// mouse hovers the canvas, `None` after the cursor leaves. Drives the
    /// crosshair overlay (guide lines + axis labels + OHLC readout).
    pub(super) cursor: Option<(f32, f32)>,
    /// Width of the y-axis gutter in px, recomputed each render to fit the
    /// widest price label produced by the current `(y_min, y_max)` range.
    /// `Cell` for interior mutation so `render(&ChartState, …)` can refresh
    /// it without taking a mutable borrow that conflicts with `cx`. Read by
    /// hit-test helpers (`screen_to_index` etc.) so clicks and drags stay
    /// aligned with paint when the gutter resizes.
    pub(super) y_axis_gap_px: std::cell::Cell<f32>,
    /// Sticky-tail mode: when true, new bars arriving via `apply_tick`,
    /// `tick_clock`, or `resnap` advance `view_start` so the chart stays
    /// glued to the live edge. Enabled by `snap_to_latest` (the "Go to
    /// latest" action); disabled the moment the user pans the canvas
    /// horizontally. Ephemeral, never persisted.
    pub(super) sticky_to_latest: bool,
    /// Indicators attached to this chart panel. Carry over on symbol /
    /// timeframe switch (the user's analytical setup follows the chart panel,
    /// not the data). Volume is seeded by default — see `new()`.
    pub(super) indicators: Vec<IndicatorInstance>,
    /// Cached output per indicator, parallel-indexed with `indicators`.
    /// Recomputed from `candles` on `apply_tick` / `tick_clock` / `resnap`
    /// / `apply_prepend` and on add / edit / remove. Paint reads this
    /// directly — no compute happens inside the paint closure.
    pub(super) indicator_outputs: Vec<IndicatorOutput>,
    /// Active sub-pane splitter drag, if any. Set on splitter mouse-down
    /// (carrying the target instance id + a baseline of starting cursor-y
    /// and starting pane_height); read by the outer panel's mouse-move
    /// handler to update `pane_height`; cleared on mouse-up. Drag survives
    /// while the cursor stays inside the chart panel — exits past the
    /// panel edge end the drag (v1 limitation, follow-up via global drag).
    pub(super) splitter_drag: Option<SplitterDrag>,
    /// Canvas-relative x of the cursor in whichever pane (main or sub-pane)
    /// is currently hovered. Drives the cross-pane vertical crosshair guide
    /// — `cursor` and `sub_cursor` track per-pane state for the horizontal
    /// guide / readouts, but vertical guides paint across every pane at the
    /// same x. `None` when the cursor isn't over any chart pane.
    pub(super) cross_cursor_x: Option<f32>,
    /// When the cursor sits over a sub-pane, the id + canvas-relative
    /// position within that pane. Drives the hovered sub-pane's horizontal
    /// y-line + value-readout pill. `None` when the cursor is over the
    /// main pane (`cursor` carries it then) or outside the chart entirely.
    pub(super) sub_cursor: Option<(InstanceId, f32, f32)>,
    /// Last-painted bounds per sub-pane canvas, keyed by instance id.
    /// Captured via each sub-canvas's `on_prepaint` and consumed by its
    /// `on_mouse_move` to translate window-relative event coords into the
    /// canvas-relative cursor used by the cross-pane crosshair pipeline.
    pub(super) pane_bounds: std::collections::HashMap<InstanceId, Bounds<Pixels>>,
    /// Collapse state for the main-pane "Indicators (N) ▼" header chip.
    /// Ephemeral — not preserved across symbol/timeframe switches (those
    /// reconstruct `ChartState`), not persisted to local_storage. Toggled by
    /// the header chip's click handler.
    pub indicators_collapsed: bool,
    /// Active render mode (Candlestick / Footprint Cluster / Footprint
    /// Profile). Drives the paint pipeline branch in [`paint`] and, in
    /// later commits, the header dropdown + the synthesized render chip
    /// pinned at the top of the indicator list. Defaults to `Candlestick`
    /// and is preserved across symbol/timeframe switches (the render
    /// choice follows the panel, mirroring how indicators do — see
    /// [`Self::adopt_render_settings`]).
    pub(super) render_kind: RenderKind,
    /// Eye-toggle state on the render chip. False suppresses the candle /
    /// cell / profile paint (overlays + drawings still render). Ephemeral —
    /// not persisted, defaults true.
    pub(super) render_visible: bool,
    /// Persisted per-mode params for the Cluster render. Each footprint
    /// mode remembers its own settings — switching Cluster ↔ Profile does
    /// not bleed across — see the locked design in
    /// `project_footprint_v1_design`.
    pub(super) cluster_params: FootprintParams,
    /// Persisted per-mode params for the Profile render.
    pub(super) profile_params: FootprintParams,
    /// Live footprint cells for the active (symbol, tf, bucket) sub.
    /// Replaced wholesale on FootprintEvent {Snapshot, Update, Prepended};
    /// cleared when the sub is released (render switch to Candlestick,
    /// symbol/timeframe/bucket change). Empty triggers paint_main_chart's
    /// fallback to candle bodies so the chart never goes blank during the
    /// snapshot round-trip.
    pub(super) footprint_cells: Vec<crate::services::market_data::FootprintCell>,
    /// Per-bucket footprint cell cache for VP-family consumers (VRVP today,
    /// FRVP in Phase 12). Keyed by the bit-pattern of the bucket f64 — same
    /// keying `ContentPanel.footprint_subs` uses, so a chart-owned bucket
    /// and a VP-instance bucket at the same dollar value collide cleanly
    /// into one entry. Replaced wholesale per bucket by
    /// `ContentPanel`'s FootprintEvent handler; cleared per-bucket via
    /// [`Self::clear_footprint_cache_bucket`] when the sub is released.
    ///
    /// Separate from `footprint_cells` (which is the chart's own primary
    /// render bucket and gets a fast-path in the candle pane paint) because
    /// VP consumers often want a *different* bucket than the chart, and we
    /// don't want VP sub churn to disturb the chart's own paint cache.
    pub(super) footprint_cache: std::collections::HashMap<
        u64,
        Vec<crate::services::market_data::FootprintCell>,
    >,
    /// Per-bar liquidation cells for this chart's `(symbol, tf)`, sorted
    /// oldest-first. Refilled by ContentPanel from `MarketDataService::
    /// liquidation_bars` on every `LiquidationBarEvent` for the active tf.
    /// Empty when no `liq_bars` indicator is live — the sub stays
    /// unallocated and the cache never grows.
    pub(super) liquidation_bars_cache: Vec<crate::services::market_data::LiquidationBar>,
    /// Per-bar open-interest OHLC for this chart's `(symbol, tf)`, sorted
    /// oldest-first. Refilled by ContentPanel from `MarketDataService::
    /// open_interest_bars` on every `OpenInterestEvent` for the active tf.
    /// Empty when no `open_interest` indicator (and no bar_stat OI-Δ row) is
    /// live — the sub stays unallocated and the cache never grows.
    pub(super) open_interest_cache: Vec<crate::services::market_data::OpenInterestBar>,
    /// Per-bar mark-price OHLC + funding for this chart's `(symbol, tf)`, sorted
    /// oldest-first. Refilled by ContentPanel from `MarketDataService::
    /// mark_price_bars` on every `MarkPriceEvent` for the active tf. Empty when
    /// no consumer (OI indicator, bar_stat OI-Δ row, funding indicator) is live.
    pub(super) mark_price_cache: Vec<crate::services::market_data::MarkPriceBar>,
    /// Pre-reduced per-snapshot order-book imbalance samples (ascending `ts_ms`)
    /// for this chart's symbol. Refilled by ContentPanel from the shared book
    /// time-series on a ~1s throttle. Empty when no consumer (the `ob_imbalance`
    /// indicator or a bar_stat with OB rows) is live.
    pub(super) book_imbalance_cache: Vec<crate::indicators::ob_imbalance::BookImbalanceSample>,
    /// View-time-range snapshot captured at the last
    /// [`Self::recompute_indicators`] call. Drives the cheap dirty-check in
    /// [`Self::maybe_recompute_view_dependent_indicators`] — pan/zoom that
    /// doesn't actually shift the visible bar range (e.g. dragging within
    /// one bar's width) is a no-op for VRVP.
    pub(super) last_recomputed_view_range: Option<(i64, i64)>,
    /// Per-chart volume display unit. Affects this chart's volume /
    /// volume-delta / CVD indicators (threaded into `ComputeCtx`) AND its
    /// footprint paint pipeline. Surfaces as the header Coin/USD dropdown
    /// between the render-kind selector and `+ Indicator`. Carries over
    /// symbol/timeframe switches (the rendering unit is a user choice, not
    /// a per-symbol one) and persists in `ChartPrefs`.
    pub(super) volume_unit: VolumeUnit,
    /// Orderbook liquidity heatmap render layer — an independent overlay
    /// painted behind the main render, orthogonal to `render_kind`. Owns its
    /// toggle, settings, and the cached GPU texture. The book subscription +
    /// 1s sampling that feed it are owned by `ContentPanel` (lazy on enable).
    /// Carries over symbol/timeframe switches like the render settings.
    pub(super) heatmap: super::paint::HeatmapLayer,
    /// Predictive liquidation heatmap render layer — independent of `heatmap`
    /// (its own texture cache + atlas tile), driven by a forward simulation
    /// over the candle / OI / mark series this state already gathers rather than
    /// a book subscription. Both heatmaps may be on at once. Carries over
    /// symbol/timeframe switches like `heatmap`.
    pub(super) liq_heatmap: super::paint::LiqHeatmapLayer,
}

/// Baseline captured at splitter mouse-down. The outer mouse-move handler
/// computes `new_height = start_height + (current_y - start_y)` and pushes
/// through `set_indicator_pane_height` (which clamps the floor to 60px).
#[derive(Clone, Copy)]
pub(super) struct SplitterDrag {
    pub(super) instance_id: InstanceId,
    pub(super) start_y: f32,
    pub(super) start_height: f32,
}

/// Captured by `switch_symbol` / `switch_timeframe` before they tear down
/// `self` via `*self = Self::new(...)`. Adopted back onto the fresh
/// `ChartState` so the user's render choice + per-mode params survive
/// data-side changes.
#[derive(Clone, Copy)]
struct RenderSettingsSnapshot {
    kind: RenderKind,
    visible: bool,
    cluster: FootprintParams,
    profile: FootprintParams,
    volume_unit: VolumeUnit,
}

impl ChartState {
    /// Fallback default symbol — used by `ContentPanel::new` when no persisted
    /// chart prefs are present and the symbols service is empty. The server
    /// only supports BTCUSDT today (see `SUPPORTED_SYMBOL`).
    pub fn default_symbol() -> &'static str {
        "BTCUSDT"
    }

    /// Timeframe used for a freshly-opened chart.
    pub fn default_timeframe() -> Timeframe {
        market_data::DEFAULT_TIMEFRAME
    }

    /// Replace `self` with a fresh state for `symbol` (keeping the current
    /// timeframe), but only if `symbol` differs. Returns `true` if a switch
    /// happened (the caller can skip a redundant `cx.notify()`). Indicators
    /// carry over: the user's analytical setup follows the chart panel, not
    /// the data — see /grill-me locked design. Render kind + per-mode
    /// footprint params also carry over (the rendering mode is a user
    /// choice, not a per-symbol one).
    pub fn switch_symbol(&mut self, symbol: &str, candles: Vec<Candle>) -> bool {
        if self.symbol == symbol {
            return false;
        }
        let indicators = std::mem::take(&mut self.indicators);
        let render = self.snapshot_render_settings();
        // Move the heatmap layer (toggle + settings + cached texture) across the
        // rebuild rather than snapshotting just the flags: this carries the live
        // `Arc<RenderImage>` so its atlas tile is released by the next
        // `refresh_heatmap` (empty new-symbol series ⇒ `drop_cache`) instead of
        // leaking when the old `ChartState` drops.
        let heatmap = std::mem::take(&mut self.heatmap);
        let liq_heatmap = std::mem::take(&mut self.liq_heatmap);
        *self = Self::new(symbol, self.timeframe, candles);
        self.adopt_indicators(indicators);
        self.adopt_render_settings(render);
        self.heatmap = heatmap;
        self.liq_heatmap = liq_heatmap;
        true
    }

    /// Replace `self` with a fresh state at `tf` (keeping symbol), but only
    /// if `tf` differs. Returns `true` if a switch happened. Indicators carry
    /// over (see `switch_symbol`); render kind + per-mode footprint params
    /// also carry over.
    pub fn switch_timeframe(&mut self, tf: Timeframe, candles: Vec<Candle>) -> bool {
        if self.timeframe == tf {
            return false;
        }
        let symbol = self.symbol.clone();
        let indicators = std::mem::take(&mut self.indicators);
        let render = self.snapshot_render_settings();
        // See `switch_symbol`: carry the heatmap layer (incl. its atlas-backed
        // texture) by move so the tile is released, not leaked. Same symbol +
        // new TF ⇒ the book sub is unchanged, but the candle x-mapping differs,
        // so the next `refresh_heatmap` rebuilds (and drops the old tile).
        let heatmap = std::mem::take(&mut self.heatmap);
        let liq_heatmap = std::mem::take(&mut self.liq_heatmap);
        *self = Self::new(symbol.as_ref(), tf, candles);
        self.adopt_indicators(indicators);
        self.adopt_render_settings(render);
        self.heatmap = heatmap;
        self.liq_heatmap = liq_heatmap;
        true
    }

    // ─────────────────────────── Render mode ───────────────────────────

    /// Currently-active render kind. Defaults `Candlestick`; switched via
    /// [`Self::switch_render`].
    pub fn render_kind(&self) -> RenderKind {
        self.render_kind
    }

    /// Eye-toggle state for the render chip. False suppresses the
    /// candle/cell/profile paint (overlays + drawings still render).
    pub fn render_visible(&self) -> bool {
        self.render_visible
    }

    pub fn set_render_visible(&mut self, visible: bool) {
        self.render_visible = visible;
    }

    /// Switch the active render kind. Returns `true` if it actually changed
    /// (caller can skip a redundant `cx.notify()` / sub re-allocation).
    ///
    /// Sub-lifecycle wiring (drop the old footprint sub, allocate a new one
    /// for the entered mode) happens at the [`crate::panels::ContentPanel`]
    /// layer — same pattern as `chart_sub_handles` for the candles channel.
    /// `ChartState` is purely state here.
    pub fn switch_render(&mut self, kind: RenderKind) -> bool {
        if self.render_kind == kind {
            return false;
        }
        self.render_kind = kind;
        true
    }

    pub fn cluster_params(&self) -> &FootprintParams {
        &self.cluster_params
    }

    pub fn profile_params(&self) -> &FootprintParams {
        &self.profile_params
    }

    /// Params for `kind`, or `None` for `Candlestick` (which has no params).
    pub fn params_for(&self, kind: RenderKind) -> Option<&FootprintParams> {
        match kind {
            RenderKind::Candlestick => None,
            RenderKind::Cluster => Some(&self.cluster_params),
            RenderKind::Profile => Some(&self.profile_params),
        }
    }

    /// Params for the active render, or `None` in Candlestick mode. Used by
    /// the paint pipeline branch and (later) the settings popover.
    pub fn active_footprint_params(&self) -> Option<&FootprintParams> {
        self.params_for(self.render_kind)
    }

    /// Mutate the Cluster params in place. The closure should return `true`
    /// if it changed a field that requires the caller to re-subscribe (i.e.
    /// the `bucket`); `false` for cosmetic-only edits. Caller (typically
    /// `ContentPanel`) acts on the return value to drop+reopen the
    /// footprint sub.
    pub fn update_cluster_params<F>(&mut self, f: F) -> bool
    where
        F: FnOnce(&mut FootprintParams) -> bool,
    {
        f(&mut self.cluster_params)
    }

    pub fn update_profile_params<F>(&mut self, f: F) -> bool
    where
        F: FnOnce(&mut FootprintParams) -> bool,
    {
        f(&mut self.profile_params)
    }

    /// Snapshot the render state (kind + both per-mode params + visibility)
    /// so `switch_symbol` / `switch_timeframe` can restore it onto the
    /// freshly-constructed `ChartState`. Visibility carries over too — the
    /// user's "hidden render" choice shouldn't reset on a symbol flip.
    fn snapshot_render_settings(&self) -> RenderSettingsSnapshot {
        RenderSettingsSnapshot {
            kind: self.render_kind,
            visible: self.render_visible,
            cluster: self.cluster_params,
            profile: self.profile_params,
            volume_unit: self.volume_unit,
        }
    }

    fn adopt_render_settings(&mut self, snap: RenderSettingsSnapshot) {
        self.render_kind = snap.kind;
        self.render_visible = snap.visible;
        self.cluster_params = snap.cluster;
        self.profile_params = snap.profile;
        self.volume_unit = snap.volume_unit;
    }

    /// Direct setters used by `ContentPanel::new_restored` to seed
    /// persisted render state (`ChartPrefs.render_kind` / `cluster` /
    /// `profile`) onto a freshly-constructed `ChartState` without going
    /// through the switch_* / update_* path that may have side effects in
    /// later commits.
    pub fn seed_render(
        &mut self,
        kind: RenderKind,
        cluster: FootprintParams,
        profile: FootprintParams,
    ) {
        self.render_kind = kind;
        self.cluster_params = cluster;
        self.profile_params = profile;
    }

    /// Current volume display unit (Coin or USD). Read by `compute_ctx()`
    /// + the paint pipeline so indicators and footprint cells agree on
    /// the same unit at every render.
    pub fn volume_unit(&self) -> VolumeUnit {
        self.volume_unit
    }

    /// Set the volume unit. Caller is responsible for invoking
    /// `recompute_indicators()` + `cx.notify()` so the change takes
    /// visible effect in one frame.
    pub fn set_volume_unit(&mut self, unit: VolumeUnit) {
        self.volume_unit = unit;
    }

    /// Build the `ComputeCtx` for the current chart settings — threaded
    /// into every `IndicatorKind::compute` call so per-chart knobs (volume
    /// unit, footprint cache, viewport) flow without a global.
    ///
    /// Takes disjoint field references (rather than `&self`) so the
    /// returned ctx's lifetime is tied only to `footprint_cache`. This
    /// lets call sites mutate other fields (`self.indicators[idx]`,
    /// `self.indicator_outputs[i]`) without tripping the borrow checker —
    /// a single `&self` method would conflict.
    fn make_compute_ctx<'a>(
        volume_unit: VolumeUnit,
        footprint_cache: &'a std::collections::HashMap<
            u64,
            Vec<crate::services::market_data::FootprintCell>,
        >,
        view_time_range: Option<(i64, i64)>,
        liquidation_bars: Option<&'a [crate::services::market_data::LiquidationBar]>,
        open_interest: Option<&'a [crate::services::market_data::OpenInterestBar]>,
        mark_price: Option<&'a [crate::services::market_data::MarkPriceBar]>,
        book_imbalance: Option<&'a [crate::indicators::ob_imbalance::BookImbalanceSample]>,
    ) -> ComputeCtx<'a> {
        ComputeCtx {
            volume_unit,
            footprint: Some(crate::services::market_data::FootprintCellLookup::new(
                footprint_cache,
            )),
            view_time_range,
            liquidation_bars,
            open_interest,
            mark_price,
            book_imbalance,
        }
    }

    /// Inclusive-exclusive `(lo, hi)` open-time window in ms covering the
    /// currently-visible bar range. Used by `compute_ctx` to bound the VRVP
    /// aggregation to visible bars. Returns `None` when the chart has no
    /// candles yet or the viewport hasn't been initialized.
    pub fn view_time_range(&self) -> Option<(i64, i64)> {
        if self.candles.is_empty() {
            return None;
        }
        let tf_ms = self.timeframe.duration_ms();
        let lo_idx = self
            .view_start
            .max(0.0)
            .floor()
            .min(self.candles.len().saturating_sub(1) as f32) as usize;
        let raw_hi = (self.view_start + self.view_size).ceil() as usize;
        let hi_idx = raw_hi.min(self.candles.len()).max(lo_idx + 1);
        let lo_t = self.candles[lo_idx].open_time;
        let hi_t = self.candles[hi_idx - 1].open_time + tf_ms;
        Some((lo_t, hi_t))
    }

    /// Oldest `open_time` cached for `bucket_bits`, or `None` when the
    /// bucket has no cells loaded yet. Used by the VP history-fill loop to
    /// decide whether to request older footprint cells for the visible
    /// window.
    pub fn oldest_footprint_cell_time(&self, bucket_bits: u64) -> Option<i64> {
        self.footprint_cache
            .get(&bucket_bits)
            .and_then(|cells| cells.iter().map(|c| c.open_time).min())
    }

    /// Replace the cached footprint cells for one bucket. Called by
    /// `ContentPanel`'s FootprintEvent handler after every Snapshot /
    /// Update / Prepended / Resnap on any bucket the chart has a live sub
    /// on — VRVP's compute reads from this cache via its
    /// `params.bucket_bits()` slot.
    pub fn set_footprint_cache_bucket(
        &mut self,
        bucket_bits: u64,
        cells: Vec<crate::services::market_data::FootprintCell>,
    ) {
        self.footprint_cache.insert(bucket_bits, cells);
    }

    /// Drop the cache entry for `bucket_bits`. Called when the bucket's
    /// sub leaves the desired set (last VRVP / FRVP holding it was
    /// removed, or the chart switched symbol / timeframe).
    pub fn clear_footprint_cache_bucket(&mut self, bucket_bits: u64) {
        self.footprint_cache.remove(&bucket_bits);
    }

    /// Wipe the whole cache. Called on (symbol, tf) change — every bucket
    /// is stale at that point.
    pub fn clear_footprint_cache(&mut self) {
        self.footprint_cache.clear();
    }

    /// Replace the per-bar liquidation cache wholesale. Caller passes a
    /// vector already sorted ascending by `open_time`. Called whenever the
    /// service emits a `LiquidationBarEvent` for the chart's `(symbol, tf)`.
    pub fn set_liquidation_bars_cache(
        &mut self,
        bars: Vec<crate::services::market_data::LiquidationBar>,
    ) {
        self.liquidation_bars_cache = bars;
    }

    pub fn clear_liquidation_bars_cache(&mut self) {
        self.liquidation_bars_cache.clear();
    }

    /// Oldest `open_time` (ms) in the liquidation-bars cache, if any. Used
    /// by `ContentPanel::maybe_request_liq_bars_history` to decide whether
    /// the visible view extends past loaded coverage.
    pub fn oldest_liquidation_bar_time(&self) -> Option<i64> {
        self.liquidation_bars_cache.first().map(|b| b.open_time)
    }

    /// Replace the per-bar open-interest cache wholesale. Caller passes a
    /// vector already sorted ascending by `open_time`. Called whenever the
    /// service emits an `OpenInterestEvent` for the chart's `(symbol, tf)`.
    pub fn set_open_interest_cache(
        &mut self,
        bars: Vec<crate::services::market_data::OpenInterestBar>,
    ) {
        self.open_interest_cache = bars;
    }

    pub fn clear_open_interest_cache(&mut self) {
        self.open_interest_cache.clear();
    }

    /// Oldest `open_time` (ms) in the open-interest cache, if any. Used by
    /// `ContentPanel::maybe_request_oi_bars_history` to decide whether the
    /// visible view extends past loaded coverage.
    pub fn oldest_open_interest_time(&self) -> Option<i64> {
        self.open_interest_cache.first().map(|b| b.open_time)
    }

    /// Replace the per-bar mark-price cache wholesale. Caller passes a vector
    /// already sorted ascending by `open_time`. Called whenever the service
    /// emits a `MarkPriceEvent` for the chart's `(symbol, tf)`.
    pub fn set_mark_price_cache(
        &mut self,
        bars: Vec<crate::services::market_data::MarkPriceBar>,
    ) {
        self.mark_price_cache = bars;
    }

    pub fn clear_mark_price_cache(&mut self) {
        self.mark_price_cache.clear();
    }

    /// Oldest `open_time` (ms) in the mark-price cache, if any. Used by
    /// `ContentPanel::maybe_request_mark_price_history`.
    pub fn oldest_mark_price_time(&self) -> Option<i64> {
        self.mark_price_cache.first().map(|b| b.open_time)
    }

    /// Replace the per-snapshot book-imbalance cache wholesale (ascending
    /// `ts_ms`). Called by ContentPanel on the ~1s book-reduce throttle.
    pub fn set_book_imbalance_cache(
        &mut self,
        samples: Vec<crate::indicators::ob_imbalance::BookImbalanceSample>,
    ) {
        self.book_imbalance_cache = samples;
    }

    pub fn clear_book_imbalance_cache(&mut self) {
        self.book_imbalance_cache.clear();
    }

    /// Whether the book-imbalance cache currently holds samples. Lets
    /// ContentPanel avoid a needless clear + recompute when it's already empty.
    pub fn has_book_imbalance_cache(&self) -> bool {
        !self.book_imbalance_cache.is_empty()
    }

    /// Whether any live indicator consumes order-book imbalance — the
    /// `ob_imbalance` indicator, or a bar_stat with at least one OB depth row.
    /// Gates the shared book subscription + the per-snapshot reduce.
    pub fn wants_book_imbalance(&self) -> bool {
        self.indicators.iter().any(|i| {
            if i.kind_id == "ob_imbalance" {
                return true;
            }
            if let Some(bs) = i
                .kind
                .as_any()
                .downcast_ref::<crate::indicators::BarStatParams>()
            {
                return !bs.sorted_ob_depths().is_empty();
            }
            false
        })
    }

    /// Read-only slice into the per-bucket footprint cache. `None` when no
    /// sub holds that bucket — FRVP paint then short-circuits to "just the
    /// bracket" so the user sees the geometry while waiting for cells.
    pub fn footprint_cells_for_bucket(
        &self,
        bucket_bits: u64,
    ) -> Option<&[crate::services::market_data::FootprintCell]> {
        self.footprint_cache.get(&bucket_bits).map(|v| v.as_slice())
    }

    pub fn timeframe(&self) -> Timeframe {
        self.timeframe
    }

    /// Read-only view of the live footprint cells for the active sub.
    /// Empty when render kind is Candlestick, when no sub is currently
    /// open, or while the initial snapshot is in flight.
    pub fn footprint_cells(&self) -> &[crate::services::market_data::FootprintCell] {
        &self.footprint_cells
    }

    /// Replace the footprint buffer wholesale. Called by `ContentPanel`'s
    /// FootprintEvent handler on Snapshot / Update / Prepended. Cleared
    /// (`Vec::new`) when the sub is released — leaves the chart's render
    /// branch free to fall back to candle bodies automatically.
    pub fn set_footprint_cells(&mut self, cells: Vec<crate::services::market_data::FootprintCell>) {
        self.footprint_cells = cells;
    }

    pub fn clear_footprint_cells(&mut self) {
        self.footprint_cells.clear();
    }

    /// Reset both axes to their defaults: trailing-window viewport on x,
    /// auto-fit on y. Composition of `reset_x` + `reset_y_auto` so the
    /// context menu can do "both at once" without the user having to
    /// double-click each axis.
    pub fn reset_scale(&mut self) {
        self.reset_x();
        self.reset_y_auto();
    }

    pub fn symbol(&self) -> &SharedString {
        &self.symbol
    }

    /// Merge a WS tick into our buffer. Mirrors `MarketDataService::apply_tick`
    /// — the service is the source of truth, but each chart panel keeps its
    /// own copy so drawings stay anchored to bar indices across symbol
    /// switches. Mutates the last bar in place when `open_time` matches the
    /// tail; appends when it advances; drops out-of-order ticks.
    pub fn apply_tick(&mut self, candle: Candle, _is_closed: bool) {
        let appended = match self.candles.last_mut() {
            Some(last) if last.open_time == candle.open_time => {
                *last = candle;
                false
            }
            Some(last) if last.open_time < candle.open_time => {
                self.candles.push(candle);
                true
            }
            None => {
                self.candles.push(candle);
                true
            }
            Some(_) => {
                // Out-of-order tick (post-reconnect resync grace period). Ignore.
                false
            }
        };
        if appended && self.sticky_to_latest {
            self.view_start += 1.0;
            self.clamp();
        }
        // Every tick mutates the candle array — either the in-progress tail's
        // OHLC moves, or a new bar appends. Both shift indicator output, so we
        // recompute. Cost is sub-ms for v1 indicators × ~1000 bars.
        self.recompute_indicators();
    }

    /// Roll the chart forward to wall-clock when no live tick has arrived for
    /// the next bar yet. Each synthesized bar carries the previous close as
    /// O/H/L/C and zero volume — when a real tick lands it replaces the
    /// synthetic one through `apply_tick`'s open_time match. Returns true if
    /// any bar was appended (callers can skip a needless `cx.notify()` cost on
    /// the no-op path, though the countdown caller always notifies).
    ///
    /// Capped per call so a chart left open across off-hours doesn't fabricate
    /// thousands of empty bars — the reconnect / resnap path will fill the
    /// real gap when the user comes back.
    pub fn tick_clock(&mut self, now_ms: i64) -> bool {
        let dur = self.timeframe.duration_ms();
        if dur <= 0 {
            return false;
        }
        let Some(last) = self.candles.last().cloned() else {
            return false;
        };
        if now_ms <= last.close_time {
            return false;
        }
        const MAX_ROLL_PER_TICK: usize = 5;
        let mut prev = last;
        let mut added = 0;
        while now_ms > prev.close_time && added < MAX_ROLL_PER_TICK {
            let next_open = prev.open_time + dur;
            let next_close = next_open + dur - 1;
            let flat = Candle::new(
                next_open, next_close, prev.close, prev.close, prev.close, prev.close, 0.0,
            );
            self.candles.push(flat.clone());
            prev = flat;
            added += 1;
        }
        if added > 0 && self.sticky_to_latest {
            self.view_start += added as f32;
            self.clamp();
        }
        if added > 0 {
            self.recompute_indicators();
        }
        added > 0
    }

    /// Re-seed `candles` from a fresh snapshot (initial backfill, or post-
    /// reconnect resync). Resets the viewport only if we had no prior data —
    /// otherwise the user's pan/zoom is preserved.
    pub fn resnap(&mut self, candles: Vec<Candle>) {
        let was_empty = self.candles.is_empty();
        self.candles = candles;
        if was_empty {
            let total = self.candles.len() as f32;
            self.view_size = crate::prefs::chart_default_view().min(total).max(1.0);
            self.view_start = if total > 0.0 {
                total - self.view_size * (1.0 - crate::prefs::chart_right_buffer())
            } else {
                0.0
            };
        } else if self.sticky_to_latest {
            // Sticky mode: re-anchor to the live edge of the fresh snapshot
            // (post-reconnect catch-up). User's `view_size` is preserved.
            let total = self.candles.len() as f32;
            if total > 0.0 {
                self.view_start =
                    total - self.view_size * (1.0 - crate::prefs::chart_right_buffer());
                self.clamp();
            }
        }
        self.recompute_indicators();
    }

    /// True when the oldest loaded bar is within ~one viewport of the left edge
    /// — the cue to prefetch older history before the user hits the hard clamp.
    pub fn wants_older(&self) -> bool {
        self.view_start < self.view_size
    }

    /// Apply a prepend of `added` older bars: adopt the fresh (longer) snapshot
    /// and shift every index-anchored value right by `added` so the viewport and
    /// drawings stay on the same bars (no view reset).
    pub fn apply_prepend(&mut self, candles: Vec<Candle>, added: usize) {
        self.candles = candles;
        if added > 0 {
            self.shift_indices(added as f32);
        }
        self.recompute_indicators();
    }

    // ──────────────────────────── Indicators ────────────────────────────

    /// Read-only view of attached indicators (in render order). Chip rendering
    /// + paint pipeline iterate this; settings + picker use the mutators below.
    pub fn indicators(&self) -> &[IndicatorInstance] {
        &self.indicators
    }

    /// Integer bar index under the crosshair, or `None` when the cursor
    /// isn't over any pane (or hasn't been measured yet). Used by chip
    /// rendering to insert the indicator's value-at-cursor into the chip
    /// label. Reads from `cross_cursor_x` so sub-pane hover counts too —
    /// the bar grid is shared across panes.
    pub fn cursor_bar_index(&self) -> Option<usize> {
        let cx = self.cross_cursor_x?;
        let bounds = self.bounds?;
        let canvas_w = bounds.size.width.as_f32();
        let raw = screen_to_index(
            self.view_start,
            self.view_size,
            cx,
            canvas_w,
            self.y_axis_gap_px.get(),
        );
        if raw < 0.0 {
            return None;
        }
        let idx = raw.round() as usize;
        if idx >= self.candles.len() {
            return None;
        }
        Some(idx)
    }

    /// Cached output for instance `id`, or `None` if the id is unknown.
    pub fn indicator_output(&self, id: InstanceId) -> Option<&IndicatorOutput> {
        let idx = self.indicators.iter().position(|i| i.id == id)?;
        self.indicator_outputs.get(idx)
    }

    /// Add a freshly-spawned kind, auto-picking the next palette slot from
    /// the per-kind rotation. Returns the new instance's id.
    pub fn add_indicator(&mut self, kind: Box<dyn IndicatorKind>) -> InstanceId {
        let kind_id = kind.kind_id();
        let count = self
            .indicators
            .iter()
            .filter(|i| i.kind_id == kind_id)
            .count();
        let color = palette_color_for(count);
        let instance = IndicatorInstance::new(kind, color);
        let id = instance.id;
        let view_range = self.view_time_range();
        let liq_bars: Option<&[crate::services::market_data::LiquidationBar]> =
            (!self.liquidation_bars_cache.is_empty())
                .then(|| self.liquidation_bars_cache.as_slice());
        let oi_bars: Option<&[crate::services::market_data::OpenInterestBar]> =
            (!self.open_interest_cache.is_empty())
                .then(|| self.open_interest_cache.as_slice());
        let mark_bars: Option<&[crate::services::market_data::MarkPriceBar]> =
            (!self.mark_price_cache.is_empty())
                .then(|| self.mark_price_cache.as_slice());
        let book_imb: Option<&[crate::indicators::ob_imbalance::BookImbalanceSample]> =
            (!self.book_imbalance_cache.is_empty())
                .then(|| self.book_imbalance_cache.as_slice());
        let ctx = Self::make_compute_ctx(
            self.volume_unit,
            &self.footprint_cache,
            view_range,
            liq_bars,
            oi_bars,
            mark_bars,
            book_imb,
        );
        let output = instance.kind.compute(&self.candles, ctx);
        self.indicators.push(instance);
        self.indicator_outputs.push(output);
        id
    }

    /// Drop the instance with the given id, if it exists. No-op otherwise.
    pub fn remove_indicator(&mut self, id: InstanceId) {
        if let Some(idx) = self.indicators.iter().position(|i| i.id == id) {
            self.indicators.remove(idx);
            self.indicator_outputs.remove(idx);
            self.pane_bounds.remove(&id);
            if let Some((sub_id, _, _)) = self.sub_cursor {
                if sub_id == id {
                    self.sub_cursor = None;
                }
            }
        }
    }

    /// Mutate an instance's `kind` in place via a closure, then recompute
    /// just that one's output. Used by the settings panel for live-apply
    /// edits — the closure typically downcasts `kind.as_any_mut()` to the
    /// concrete params type and mutates fields. Returns true if `id` was
    /// found.
    pub fn update_indicator<F>(&mut self, id: InstanceId, f: F) -> bool
    where
        F: FnOnce(&mut Box<dyn IndicatorKind>),
    {
        let Some(idx) = self.indicators.iter().position(|i| i.id == id) else {
            return false;
        };
        f(&mut self.indicators[idx].kind);
        // `kind.kind_id()` might change if the closure swaps the box, but
        // for downcast-in-place edits it's stable. Refresh the mirrored
        // copy for safety.
        self.indicators[idx].kind_id = self.indicators[idx].kind.kind_id();
        // The mutation may have changed the kind's color-slot count
        // (e.g., adding or removing an MA Suite entry). Resize the
        // instance's per-slot color Vec so paint and the settings UI
        // see a consistent shape.
        self.indicators[idx].sync_colors();
        let view_range = self.view_time_range();
        let liq_bars: Option<&[crate::services::market_data::LiquidationBar]> =
            (!self.liquidation_bars_cache.is_empty())
                .then(|| self.liquidation_bars_cache.as_slice());
        let oi_bars: Option<&[crate::services::market_data::OpenInterestBar]> =
            (!self.open_interest_cache.is_empty())
                .then(|| self.open_interest_cache.as_slice());
        let mark_bars: Option<&[crate::services::market_data::MarkPriceBar]> =
            (!self.mark_price_cache.is_empty())
                .then(|| self.mark_price_cache.as_slice());
        let book_imb: Option<&[crate::indicators::ob_imbalance::BookImbalanceSample]> =
            (!self.book_imbalance_cache.is_empty())
                .then(|| self.book_imbalance_cache.as_slice());
        let ctx = Self::make_compute_ctx(
            self.volume_unit,
            &self.footprint_cache,
            view_range,
            liq_bars,
            oi_bars,
            mark_bars,
            book_imb,
        );
        let new_output = self.indicators[idx].kind.compute(&self.candles, ctx);
        self.indicator_outputs[idx] = new_output;
        true
    }

    /// Swap in a new kind box for an existing instance (used by the settings
    /// panel when params change). Preserves placement, pane_height, color,
    /// and hidden state; recomputes just that one's output. Returns true if
    /// the id was found.
    pub fn replace_indicator_kind(&mut self, id: InstanceId, kind: Box<dyn IndicatorKind>) -> bool {
        let Some(idx) = self.indicators.iter().position(|i| i.id == id) else {
            return false;
        };
        let view_range = self.view_time_range();
        let liq_bars: Option<&[crate::services::market_data::LiquidationBar]> =
            (!self.liquidation_bars_cache.is_empty())
                .then(|| self.liquidation_bars_cache.as_slice());
        let oi_bars: Option<&[crate::services::market_data::OpenInterestBar]> =
            (!self.open_interest_cache.is_empty())
                .then(|| self.open_interest_cache.as_slice());
        let mark_bars: Option<&[crate::services::market_data::MarkPriceBar]> =
            (!self.mark_price_cache.is_empty())
                .then(|| self.mark_price_cache.as_slice());
        let book_imb: Option<&[crate::indicators::ob_imbalance::BookImbalanceSample]> =
            (!self.book_imbalance_cache.is_empty())
                .then(|| self.book_imbalance_cache.as_slice());
        let ctx = Self::make_compute_ctx(
            self.volume_unit,
            &self.footprint_cache,
            view_range,
            liq_bars,
            oi_bars,
            mark_bars,
            book_imb,
        );
        let inst = &mut self.indicators[idx];
        inst.kind_id = kind.kind_id();
        inst.kind = kind;
        let new_output = inst.kind.compute(&self.candles, ctx);
        self.indicator_outputs[idx] = new_output;
        true
    }

    /// Toggle the hidden flag (eye icon / context-menu Hide). Returns the
    /// new state if the id was found.
    pub fn set_indicator_hidden(&mut self, id: InstanceId, hidden: bool) -> Option<bool> {
        let inst = self.indicators.iter_mut().find(|i| i.id == id)?;
        inst.hidden = hidden;
        Some(hidden)
    }

    /// Update the pane height (called when the user drags the splitter).
    /// Clamps to the v1 spec's 60px floor.
    pub fn set_indicator_pane_height(&mut self, id: InstanceId, h: f32) {
        if let Some(inst) = self.indicators.iter_mut().find(|i| i.id == id) {
            if inst.pane_height.is_some() {
                inst.pane_height = Some(h.max(60.0));
            }
        }
    }

    /// Toggle a hybrid-kind instance between overlay and pane placement
    /// (Volume's settings toggle). Sets/clears `pane_height` to match.
    pub fn set_indicator_placement(&mut self, id: InstanceId, placement: Placement) {
        if let Some(inst) = self.indicators.iter_mut().find(|i| i.id == id) {
            inst.placement = placement;
            inst.pane_height = match placement {
                Placement::Pane => Some(crate::indicators::default_pane_height(inst.kind_id)),
                Placement::Overlay => None,
            };
        }
    }

    /// Set the color for a specific slot on an instance. Slot 0 is the
    /// primary line; further slots match `kind.color_slots()` order
    /// (e.g., MACD slot 1 = signal line). Out-of-bounds slots are a no-op
    /// — paint reads with the same bounds so an out-of-range index would
    /// just never be drawn anyway.
    pub fn set_indicator_color(&mut self, id: InstanceId, slot: usize, color: Hsla) {
        if let Some(inst) = self.indicators.iter_mut().find(|i| i.id == id) {
            if let Some(slot_color) = inst.colors.get_mut(slot) {
                *slot_color = color;
            }
        }
    }

    /// Reorder a sub-pane indicator by `delta` positions among the pane
    /// instances (delta = -1 → move up, +1 → move down). Overlay indicators
    /// are ignored (they don't participate in pane reorder). Clamps at edges.
    pub fn move_indicator_pane(&mut self, id: InstanceId, delta: i32) {
        let Some(idx) = self.indicators.iter().position(|i| i.id == id) else {
            return;
        };
        if self.indicators[idx].placement != Placement::Pane {
            return;
        }
        // Collect the positions of all pane instances, in order. The
        // reorder happens within that subsequence — overlay indicators
        // keep their slots in the underlying Vec.
        let pane_positions: Vec<usize> = self
            .indicators
            .iter()
            .enumerate()
            .filter(|(_, i)| i.placement == Placement::Pane)
            .map(|(p, _)| p)
            .collect();
        let Some(my_rank) = pane_positions.iter().position(|p| *p == idx) else {
            return;
        };
        let new_rank = (my_rank as i32 + delta).clamp(0, pane_positions.len() as i32 - 1) as usize;
        if new_rank == my_rank {
            return;
        }
        let target_pos = pane_positions[new_rank];
        // Swap via remove + insert so any overlay indicators between idx and
        // target_pos keep their relative slot.
        let instance = self.indicators.remove(idx);
        let output = self.indicator_outputs.remove(idx);
        let insert_at = if new_rank > my_rank {
            target_pos
        } else {
            target_pos
        };
        self.indicators.insert(insert_at, instance);
        self.indicator_outputs.insert(insert_at, output);
    }

    /// Adopt a saved indicator list (used by `switch_*` to preserve the
    /// user's setup across symbol / timeframe changes). Drops
    /// whatever the freshly-constructed state seeded (e.g., default Volume),
    /// then recomputes against the new candle buffer.
    fn adopt_indicators(&mut self, indicators: Vec<IndicatorInstance>) {
        self.indicators = indicators;
        self.indicator_outputs.clear();
        self.recompute_indicators();
    }

    /// Restore indicators from persisted prefs. Each entry is rebuilt via
    /// `indicators::build_kind(kind_id, &params)`; entries whose kind_id is
    /// unknown (legacy kinds, or forward-compat blobs from a newer build)
    /// are silently dropped. Placement, pane_height, colors, and hidden
    /// are overlaid onto the freshly-spawned instance. Caller is expected
    /// to follow with `recompute_indicators()` (the panel restore path
    /// already does).
    pub(crate) fn restore_indicators(&mut self, prefs: Vec<crate::panels::IndicatorPrefs>) {
        self.indicators.clear();
        self.indicator_outputs.clear();
        for pref in prefs {
            let Some(kind) = crate::indicators::build_kind(&pref.kind_id, &pref.params) else {
                continue;
            };
            // Primary colour seeds palette derivation for any extra slots;
            // the persisted Vec then overrides slot-by-slot below.
            let primary = pref
                .colors
                .first()
                .copied()
                .map(|c| c.into_hsla())
                .unwrap_or_else(|| palette_color_for(0));
            let mut inst = match pref.id {
                Some(id) => {
                    crate::indicators::bump_next_id_past(id);
                    IndicatorInstance::new_with_id(id, kind, primary)
                }
                None => IndicatorInstance::new(kind, primary),
            };
            inst.placement = pref.placement.into_placement();
            inst.pane_height = pref.pane_height;
            inst.hidden = pref.hidden;
            // Override the auto-derived per-slot colors with the saved
            // ones where available, but keep the IndicatorInstance's slot
            // count (which tracks the live `kind.color_slots()` length) —
            // a kind that grew/shrunk its slot count between saves is
            // handled gracefully.
            for (slot, c) in pref.colors.iter().enumerate() {
                if slot < inst.colors.len() {
                    inst.colors[slot] = c.into_hsla();
                }
            }
            self.indicators.push(inst);
        }
        // Outputs are sized in lockstep with `indicators` by
        // `recompute_indicators`; leave empty here so the caller's
        // recompute call does the real work.
    }

    /// Full recompute over the current `candles`. Cheap by v1 specs (~5
    /// indicators × ~1000 bars × sub-µs per op). Called after every tick,
    /// fabrication, snapshot, prepend, or instance edit.
    pub fn recompute_indicators(&mut self) {
        if self.indicators.len() != self.indicator_outputs.len() {
            self.indicator_outputs
                .resize_with(self.indicators.len(), || IndicatorOutput::Line(Vec::new()));
        }
        let view_range = self.view_time_range();
        let liq_bars: Option<&[crate::services::market_data::LiquidationBar]> =
            (!self.liquidation_bars_cache.is_empty())
                .then(|| self.liquidation_bars_cache.as_slice());
        let oi_bars: Option<&[crate::services::market_data::OpenInterestBar]> =
            (!self.open_interest_cache.is_empty())
                .then(|| self.open_interest_cache.as_slice());
        let mark_bars: Option<&[crate::services::market_data::MarkPriceBar]> =
            (!self.mark_price_cache.is_empty())
                .then(|| self.mark_price_cache.as_slice());
        let book_imb: Option<&[crate::indicators::ob_imbalance::BookImbalanceSample]> =
            (!self.book_imbalance_cache.is_empty())
                .then(|| self.book_imbalance_cache.as_slice());
        let ctx = Self::make_compute_ctx(
            self.volume_unit,
            &self.footprint_cache,
            view_range,
            liq_bars,
            oi_bars,
            mark_bars,
            book_imb,
        );
        for (i, inst) in self.indicators.iter().enumerate() {
            self.indicator_outputs[i] = inst.kind.compute(&self.candles, ctx);
        }
        self.last_recomputed_view_range = view_range;
    }

    /// Dirty-check + conditional re-run for indicators whose compute reads
    /// from the chart's viewport (today only VRVP). Called from
    /// `ContentPanel::render` so panning / zooming refreshes the profile
    /// without scattering recompute calls through every input handler.
    /// No-op when no view-dependent instance is attached, or when the
    /// visible time range hasn't shifted since the last recompute.
    pub fn maybe_recompute_view_dependent_indicators(&mut self) {
        if !self.indicators.iter().any(|i| i.kind_id == "vrvp") {
            return;
        }
        let cur = self.view_time_range();
        if cur == self.last_recomputed_view_range {
            return;
        }
        self.recompute_indicators();
    }

    /// Shift all candle-index-space state right by `n`. Committed drawings live
    /// in the workspace [`DrawingService`](crate::drawings::service) anchored
    /// to absolute ms, so prepended bars don't require shifting them; only
    /// chart-local ephemeral state (viewport + in-flight create/edit/text)
    /// needs the adjustment.
    fn shift_indices(&mut self, n: f32) {
        self.view_start += n;
        if let Some(c) = &mut self.creating {
            c.shift_x(n);
        }
        if let Some(t) = &mut self.editing_text {
            t.anchor.0 += n;
        }
        if let Some(ed) = &mut self.edit_drag {
            ed.baseline.shift_x(n);
            ed.anchor_world.0 += n;
        }
        if let Some(d) = &mut self.drag_anchor {
            d.start_view_start += n;
        }
        if let Some(a) = &mut self.x_axis_drag_anchor {
            // `.2` is `view_start_at_down` (see field doc).
            a.2 += n;
        }
    }

    /// Build a chart for `symbol` at `timeframe`. The initial bar buffer is
    /// the snapshot from `MarketDataService` (possibly empty if backfill
    /// hasn't completed yet — `Resnap` then fills it in). Display
    /// name/exchange are resolved at render time from the symbols service,
    /// so they aren't stored here.
    pub fn new(symbol: &str, timeframe: Timeframe, candles: Vec<Candle>) -> Self {
        let total = candles.len() as f32;
        let view_size = crate::prefs::chart_default_view().min(total).max(1.0);
        // Default view: latest candle anchored at the right edge of the
        // populated zone, with `crate::prefs::chart_right_buffer() * view_size` candles
        // of empty space past it. `view_start = total - view_size * (1 -
        // right_buffer_ratio)` ⇒ right_edge = total + buffer_in_candles.
        let view_start = if total > 0.0 {
            total - view_size * (1.0 - crate::prefs::chart_right_buffer())
        } else {
            0.0
        };
        let state = Self {
            symbol: SharedString::from(symbol.to_string()),
            timeframe,
            candles,
            view_start,
            view_size,
            drag_anchor: None,
            bounds: None,
            y_auto: true,
            y_min: 0.0,
            y_max: 0.0,
            y_drag_anchor: None,
            x_axis_drag_anchor: None,
            creating: None,
            edit_drag: None,
            editing_text: None,
            cursor: None,
            y_axis_gap_px: std::cell::Cell::new(52.0),
            sticky_to_latest: false,
            indicators: Vec::new(),
            indicator_outputs: Vec::new(),
            splitter_drag: None,
            cross_cursor_x: None,
            sub_cursor: None,
            pane_bounds: std::collections::HashMap::new(),
            indicators_collapsed: false,
            render_kind: RenderKind::default(),
            render_visible: true,
            cluster_params: FootprintParams::cluster_default(),
            profile_params: FootprintParams::profile_default(),
            footprint_cells: Vec::new(),
            footprint_cache: std::collections::HashMap::new(),
            liquidation_bars_cache: Vec::new(),
            open_interest_cache: Vec::new(),
            mark_price_cache: Vec::new(),
            book_imbalance_cache: Vec::new(),
            last_recomputed_view_range: None,
            volume_unit: VolumeUnit::default(),
            heatmap: super::paint::HeatmapLayer::default(),
            liq_heatmap: super::paint::LiqHeatmapLayer::default(),
        };
        // No default indicator. Fresh charts are born empty; the user
        // adds indicators via the picker and persistence carries them
        // across reloads.
        state
    }

    pub(super) fn clamp(&mut self) {
        let total = self.candles.len() as f32;
        self.view_size = self
            .view_size
            .clamp(CHART_MIN_VIEW.min(total), CHART_MAX_VIEW.min(total));
        // Right buffer: view_start may push past `total - view_size` by
        // `view_size * RIGHT_BUFFER_RATIO`, leaving an empty zone where future
        // bars would appear. Left buffer: view_start may go negative by
        // `view_size * LEFT_BUFFER_RATIO` so wheel-zoom-out near bar 0 still
        // works with the right-edge anchor invariant.
        let max_start = total - self.view_size * (1.0 - crate::prefs::chart_right_buffer());
        let min_start = -self.view_size * CHART_LEFT_BUFFER_RATIO;
        self.view_start = self.view_start.clamp(min_start, max_start);
    }

    /// Borrowed view of the candles currently inside the viewport. Used by
    /// hot paths (`auto_y_range`, `render`) that previously cloned a `Vec`
    /// every call — at default `view_size = 60` and `y_auto = true`, the
    /// render path was hitting this 5×/frame, which under continuous
    /// repaint (the bottom-bar's animation-frame loop) added up enough to
    /// matter. Keep `Vec`-returning variants only for callers that genuinely
    /// need ownership.
    pub(super) fn visible_slice(&self) -> &[Candle] {
        let start = self.view_start.max(0.0).floor() as usize;
        let take = self.view_size.ceil() as usize;
        let end = (start + take).min(self.candles.len());
        &self.candles[start..end]
    }

    /// Borrowed slice of candles that *paint* in the current viewport,
    /// together with the absolute index of the first candle. Used by the
    /// custom candle-paint pass which needs each candle's absolute index to
    /// compute its continuous center-x via `index_to_screen`. Returns one
    /// extra candle on the right so a candle whose body is partially
    /// clipped at the right edge during pan still paints — `visible_slice`
    /// is stricter and is reserved for the y-range auto-fit scan.
    pub(super) fn paint_slice(&self) -> (usize, &[Candle]) {
        let total = self.candles.len();
        let start = self.view_start.floor().max(0.0) as usize;
        let end_target = (self.view_start + self.view_size).ceil().max(0.0) as usize + 1;
        let end = end_target.min(total);
        let start = start.min(end);
        (start, &self.candles[start..end])
    }

    /// Sampling interval (milliseconds) between consecutive candles, taken
    /// directly from the selected timeframe. Drives the x-axis step picker.
    pub(super) fn candle_interval_ms(&self) -> i64 {
        self.timeframe.duration_ms()
    }

    /// Auto-fit price range from the visible candles. Returned `(min, max)`
    /// with a small padding so candles don't touch the chart edges. Padding
    /// is user-tunable via Settings → General → Chart → Price-axis padding;
    /// default is 5%.
    fn auto_y_range(&self) -> (f64, f64) {
        let mut lo = f64::INFINITY;
        let mut hi = f64::NEG_INFINITY;
        for c in self.visible_slice() {
            lo = lo.min(c.low);
            hi = hi.max(c.high);
        }
        if !lo.is_finite() || !hi.is_finite() || hi <= lo {
            return (0.0, 1.0);
        }
        let pad = (hi - lo) * crate::prefs::chart_y_padding() as f64;
        (lo - pad, hi + pad)
    }

    /// Lock the price axis to the current auto-fit range. Called the moment
    /// the user starts manipulating the right axis so subsequent drag/wheel
    /// moves work from a stable baseline instead of fighting auto-fit.
    pub(super) fn freeze_y_if_auto(&mut self) {
        if self.y_auto {
            let (lo, hi) = self.auto_y_range();
            self.y_min = lo;
            self.y_max = hi;
            self.y_auto = false;
        }
    }

    pub(super) fn reset_y_auto(&mut self) {
        self.y_auto = true;
        self.y_drag_anchor = None;
    }

    /// Reset the time axis to the default trailing window (most recent
    /// `crate::prefs::chart_default_view()` candles, with the standard right-side buffer
    /// pushing the latest candle to ~80% of width). Used by double-click on
    /// the bottom axis.
    pub(super) fn reset_x(&mut self) {
        let total = self.candles.len() as f32;
        self.view_size = crate::prefs::chart_default_view().min(total);
        self.view_start = total - self.view_size * (1.0 - crate::prefs::chart_right_buffer());
        self.x_axis_drag_anchor = None;
    }

    /// Pan horizontally so the latest bar lands at the default trailing
    /// offset (~60% from left, with the standard right-buffer past it), and
    /// turn on sticky-tail mode so subsequent new bars keep the chart
    /// pinned to the live edge. Preserves `view_size` (the user's zoom);
    /// re-enables y-axis auto-fit. Sticky is cleared by any user canvas
    /// pan that moves `view_start`.
    pub fn snap_to_latest(&mut self) {
        let total = self.candles.len() as f32;
        if total > 0.0 {
            self.view_start = total - self.view_size * (1.0 - crate::prefs::chart_right_buffer());
            self.clamp();
        }
        self.reset_y_auto();
        self.sticky_to_latest = true;
    }

    /// True when the most recent candle has scrolled off the right edge of
    /// the viewport (`latest_idx >= view_start + view_size`). Drives the
    /// floating "Go to latest" overlay button.
    pub fn latest_off_right(&self) -> bool {
        let total = self.candles.len() as f32;
        total > 0.0 && (total - 1.0) >= self.view_start + self.view_size
    }

    /// The y range currently rendered. Reads from auto-fit when `y_auto`,
    /// otherwise the locked range. Drawings convert prices to pixels via this
    /// — and on this frame's auto-fit if relevant — so they sit visually next
    /// to the candles they were anchored to.
    pub(super) fn y_range(&self) -> (f64, f64) {
        if self.y_auto {
            self.auto_y_range()
        } else {
            (self.y_min, self.y_max)
        }
    }

    // --- Orderbook heatmap ---------------------------------------------------
    //
    // The heatmap is a singleton overlay *indicator* (`ob_heatmap`): its on/off
    // and settings live on the instance's `OrderbookHeatmapParams`, the single
    // source of truth. `HeatmapLayer` is just the texture cache; these helpers
    // read the instance and `refresh_heatmap` syncs it into the layer's mirror
    // fields before the (unchanged) rebuild path runs.

    /// The `ob_heatmap` instance, if one is attached. Singleton (the picker +
    /// add path enforce at most one), so the first match is the only match.
    fn heatmap_instance(&self) -> Option<&IndicatorInstance> {
        self.indicators.iter().find(|i| i.kind_id == "ob_heatmap")
    }

    /// Whether a heatmap instance exists at all (even if hidden). Drives the
    /// shared book subscription gate — the sub stays up while hidden (hiding is
    /// paint-only), so OB-imbalance and a re-show both keep their data.
    pub fn has_heatmap_indicator(&self) -> bool {
        self.heatmap_instance().is_some()
    }

    /// Whether the heatmap should paint this frame: an instance exists and isn't
    /// hidden. Drives the texture build/paint + history paging (idle when
    /// hidden), distinct from [`Self::has_heatmap_indicator`].
    pub fn heatmap_enabled(&self) -> bool {
        self.heatmap_instance().is_some_and(|i| !i.hidden)
    }

    /// Current heatmap settings from the instance params, or defaults when no
    /// instance is attached.
    fn heatmap_instance_settings(&self) -> super::paint::HeatmapSettings {
        self.heatmap_instance()
            .and_then(|i| {
                i.kind
                    .as_any()
                    .downcast_ref::<crate::indicators::OrderbookHeatmapParams>()
            })
            .map(|p| p.settings)
            .unwrap_or_default()
    }

    /// Rebuild the heatmap texture for the current view if needed. `series` is
    /// the book time-series (oldest-first) owned by the market-data service.
    /// No-op (beyond a possible cache drop) when the overlay is off or the
    /// canvas hasn't painted yet. Called by `ContentPanel::render` before
    /// `chart::render`, where a `&mut Window` is available for atlas eviction.
    pub fn refresh_heatmap(
        &mut self,
        series: &[crate::services::market_data::BookSnapshotEntry],
        now_ms: i64,
        window: &mut Window,
    ) {
        // Sync the layer's mirror from the instance (the source of truth) before
        // the rebuild path reads it. Settings are part of the rebuild key, so an
        // edit on the instance flows through to a texture rebuild here.
        let on = self.heatmap_enabled();
        let settings = self.heatmap_instance_settings();
        self.heatmap.enabled = on;
        self.heatmap.settings = settings;
        if !self.heatmap.enabled {
            self.heatmap.drop_cache(window);
            return;
        }
        let Some(bounds) = self.bounds else {
            return;
        };
        if self.candles.is_empty() {
            return;
        }
        let tf_ms = self.timeframe.duration_ms();
        // True visible window incl. the right-buffer empty zone — the live book
        // samples extend past the last candle, so use idx→time extrapolation
        // rather than the candle-clamped `view_time_range`.
        let lo_ms = super::drawings_view::idx_to_time(self.view_start, &self.candles, tf_ms);
        let hi_ms = super::drawings_view::idx_to_time(
            self.view_start + self.view_size,
            &self.candles,
            tf_ms,
        );
        // Visible price range (this frame's auto-fit, or the locked range) — the
        // heatmap is lazy on y: it builds only this band (+ a hysteresis pad), so
        // the on-screen liquidity renders crisp without a full-extent texture.
        let (y_lo, y_hi) = self.y_range();
        let canvas_w = f32::from(bounds.size.width);
        // Any candle's open time anchors the per-candle column grid; all loaded
        // candles share the same TF phase, so the first one is a fine reference.
        let anchor_ms = self.candles[0].open_time;
        self.heatmap.refresh(
            series, lo_ms, hi_ms, y_lo, y_hi, canvas_w, tf_ms, anchor_ms, now_ms, window,
        );
    }

    /// The built texture + its data-rect for the paint pass, or `None` when the
    /// overlay is off / unbuilt. Captured into the chart's paint closure.
    pub(super) fn heatmap_paint_rect(&self) -> Option<super::paint::HeatmapRect> {
        self.heatmap.paint_rect()
    }

    // --- Liquidation heatmap -------------------------------------------------
    //
    // The predictive liquidation heatmap is a singleton overlay *indicator*
    // (`liq_heatmap`), mirroring `ob_heatmap`: on/off + settings + sim knobs
    // live on the instance's `LiqHeatmapParams` (the single source of truth);
    // `LiqHeatmapLayer` is just the texture cache. `refresh_liq_heatmap` syncs
    // the instance into the layer's mirror fields and runs the sim. Unlike the
    // orderbook heatmap it needs **no** book subscription — the sim reads the
    // candle / OI / mark caches this state already holds.

    fn liq_heatmap_instance(&self) -> Option<&IndicatorInstance> {
        self.indicators.iter().find(|i| i.kind_id == "liq_heatmap")
    }

    /// Whether a liq-heatmap instance exists at all (even if hidden). Drives the
    /// OI + mark-price subscription gate (the sim needs both).
    pub fn has_liq_heatmap_indicator(&self) -> bool {
        self.liq_heatmap_instance().is_some()
    }

    /// Whether the liq heatmap should paint this frame: an instance exists and
    /// isn't hidden.
    pub fn liq_heatmap_enabled(&self) -> bool {
        self.liq_heatmap_instance().is_some_and(|i| !i.hidden)
    }

    /// `(sim_params, settings)` from the instance params, or defaults when no
    /// instance is attached. The sim params carry MMR, lookback, and the
    /// user-selected price-bucket ("tick size").
    fn liq_heatmap_instance_params(
        &self,
    ) -> (
        crate::indicators::liq_heatmap::sim::SimParams,
        super::paint::HeatmapSettings,
        bool,
    ) {
        self.liq_heatmap_instance()
            .and_then(|i| {
                i.kind
                    .as_any()
                    .downcast_ref::<crate::indicators::LiqHeatmapParams>()
            })
            .map(|p| (p.sim_params(), p.settings, p.show_profile))
            .unwrap_or_else(|| {
                let p = crate::indicators::LiqHeatmapParams::default();
                (p.sim_params(), p.settings, p.show_profile)
            })
    }

    /// Rebuild the liq-heatmap texture for the current view if needed. Syncs the
    /// layer's mirror from the instance, then runs the sim over the candle / OI
    /// / mark caches. No-op (beyond a possible cache drop) when off or unpainted.
    /// Called by `ContentPanel::render` before `chart::render`, where a
    /// `&mut Window` is available for atlas eviction.
    pub fn refresh_liq_heatmap(&mut self, now_ms: i64, window: &mut Window) {
        let on = self.liq_heatmap_enabled();
        let (sim_params, settings, show_profile) = self.liq_heatmap_instance_params();
        self.liq_heatmap.enabled = on;
        self.liq_heatmap.settings = settings;
        self.liq_heatmap.show_profile = show_profile;
        if !on {
            self.liq_heatmap.drop_cache(window);
            return;
        }
        let Some(bounds) = self.bounds else {
            return;
        };
        if self.candles.is_empty() {
            return;
        }
        let tf_ms = self.timeframe.duration_ms();
        let lo_ms = super::drawings_view::idx_to_time(self.view_start, &self.candles, tf_ms);
        let hi_ms = super::drawings_view::idx_to_time(
            self.view_start + self.view_size,
            &self.candles,
            tf_ms,
        );
        let (y_lo, y_hi) = self.y_range();
        let canvas_w = f32::from(bounds.size.width);
        let anchor_ms = self.candles[0].open_time;
        self.liq_heatmap.refresh(
            &self.candles,
            &self.open_interest_cache,
            &self.mark_price_cache,
            sim_params,
            lo_ms,
            hi_ms,
            y_lo,
            y_hi,
            canvas_w,
            tf_ms,
            anchor_ms,
            now_ms,
            window,
        );
    }

    /// The built liq-heatmap texture + its data-rect for the paint pass, or
    /// `None` when off / unbuilt. Captured into the chart's paint closure.
    pub(super) fn liq_heatmap_paint_rect(&self) -> Option<super::paint::HeatmapRect> {
        self.liq_heatmap.paint_rect()
    }

    /// Profile bar-width as a fraction of the plot width, from the instance
    /// params (paint-time only — not in the texture rebuild key). Defaults when
    /// no instance is attached.
    pub(super) fn liq_heatmap_profile_width_frac(&self) -> f32 {
        self.liq_heatmap_instance()
            .and_then(|i| {
                i.kind
                    .as_any()
                    .downcast_ref::<crate::indicators::LiqHeatmapParams>()
            })
            .map(|p| p.profile_width_pct)
            .unwrap_or(crate::indicators::liq_heatmap::DEFAULT_PROFILE_WIDTH_PCT)
            / 100.0
    }
}
