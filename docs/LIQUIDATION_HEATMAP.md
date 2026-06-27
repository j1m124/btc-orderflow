# Liquidation Heatmap (predictive, Coinglass-style)

> **Status:** SHIPPED 2026-06-28 (client v0.5.0). Built from the locked design +
> plan below. As-built notes + deviations at the bottom ("Implementation status").
> Arrived at via a full design interview. This file is the implementation contract.
> Sibling reference: [`ORDERBOOK_HEATMAP.md`](./ORDERBOOK_HEATMAP.md) — the existing
> heatmap whose façade + render pipeline this feature mirrors 1:1.

## Critical framing (read first)

Coinglass's headline "liquidation heatmap" is a **forward-looking predictive model**,
*not* a picture of realized liquidations. It estimates, from open-interest + assumed
leverage, **where leveraged positions would be force-closed** — the bright bands are
clusters of positions that *haven't been liquidated yet* (price "magnets"). It consumes
essentially zero `forceOrder` data.

This feature builds **that predictive model**. The existing realized-liquidation path
(`liquidations` table, `liquidation_bars` channel, the tape panel, the `liq_bars`
indicator) is time-only bucketed with no price axis and is **not** used here — except as
an optional v2 validation overplot.

## What it is

x = time, y = price, color = estimated **un-liquidated leverage notional** at each price.
Rendered as a GPU-texture blit **behind candles**, exactly like the orderbook heatmap.
A **pure-client indicator** — **zero protocol/server change** — because all inputs are
already on the wire and already threaded into `ComputeCtx`: `candles` (carries
`taker_buy_vol` + `quote_volume`), `open_interest`, `mark_price`.

## Locked decisions (the design tree)

1. **Predictive** model (not a realized-liquidation histogram).
2. **Hybrid inputs**: ΔOI drives magnitude; taker delta drives the long/short split.
3. **Pure-client indicator** — no protocol/server/deploy change. Mirror the `ob_heatmap`
   singleton-overlay façade.
4. **Leverage tiers = Model 1**: `[10, 25, 50, 100]×`, equal weight in v1.
5. **Consumption simulation** — a forward sim where a band is cleared when price sweeps
   through it (the magnet "gets hit"). This is the defining behavior and the **only**
   removal path.
6. **Hybrid mechanics = "soft, add-only"**: add new notional only when `ΔOI > 0`; split
   it by `long_frac = 0.5 + 0.5·clamp(delta/volume, -1, 1)`; **ignore `ΔOI < 0`**
   (removal is consumption-only — avoids the double-count an OI-decrease path would cause
   against the consumption sim).
7. **Warm-up margin**: simulate from `view_start − MARGIN (~24h)` but render columns only
   for `t ≥ view_start` → correct left edge, near-free. `MARGIN` can later graduate into a
   user-facing lookback selector.
8. **Single intensity ramp** (the Coinglass look). Side is conveyed by position (longs
   liquidate *below* price, shorts *above*). Reuse the OB heatmap's two-handle log color
   slider verbatim.
9. **Coexistence: independent** — the liq heatmap gets its **own** `HeatmapLayer`; both
   heatmaps may be on simultaneously, each with its own opacity. No cross-kind coupling
   (book-walls + liq-magnets occupy different regions, so both-on is informative).
10. **Precision defaults**: entry price = **VWAP** (`quote_volume / volume`, the true
    average fill); price bucket = **$5** (reuse OB grain); maintenance margin = **flat
    ~0.4%**, configurable.

## The model (per candle, at the chart's TF)

```
delta      = 2·taker_buy_vol − volume          (taker_buy_vol None → delta 0)
ΔOI        = oi[i].close − oi[i-1].close
if ΔOI > 0:
    notional  = ΔOI × mark[i].close
    long_frac = 0.5 + 0.5·clamp(delta/volume, -1, 1)
    long_n    = notional · long_frac
    short_n   = notional · (1 − long_frac)
    entry     = quote_volume / volume                       (VWAP)
    for L in [10, 25, 50, 100]:
        long  liq = entry·(1 − 1/L + MMR)   → bucket += long_n  / 4   (below price)
        short liq = entry·(1 + 1/L − MMR)   → bucket += short_n / 4   (above price)
consume: zero every $5 bucket within [candle.low, candle.high]
         (place-then-consume-from-NEXT-candle, so an entry can't self-liquidate)
column[t] = snapshot of the running price-bucket → notional state
```

OI/mark bars are aligned to candles by `open_time` (two-pointer join, cf.
`indicators/liq_bars.rs:87`). Why delta-weighting: OI is created in long/short **pairs**,
so ΔOI alone is symmetric and useless; the taker (aggressor) is the leveraged-at-risk side,
so delta breaks the symmetry. MMR is **not** negligible for tight bands (100×: 1/L = 1.00%
→ band at ~0.60% with 0.4% MMR — a 40% shift).

---

## Implementation plan

Mirrors `ob_heatmap` for everything except the data source (sim grid instead of
`book_series`). The `HeatmapLayer` render machinery is data-agnostic once fed a series of
`(price-bucket → value)` columns. **File:line anchors are from a 2026-06-28 explore pass —
re-verify, drift expected.** Ordered to de-risk: prove the sim, then make it visible, then
polish.

### Phase 1 — The simulation (pure, unit-testable, no UI)
- New `indicators/liq_heatmap/sim.rs` (or inline): `simulate(candles, oi, mark, params,
  view) -> Vec<HeatmapColumn>` — one column per candle for `t ≥ view.0`.
- Implements the model above. Warm-up: start at the first candle with
  `open_time ≥ view.0 − MARGIN_MS`; push columns only once `open_time ≥ view.0`.
- **Only real correctness risk** → gate with `cargo test -p client` unit tests:
  known ΔOI+delta → expected bucket placement; a wick clears a band; warm-up populates the
  left edge. (Pure logic; no wasm needed.)

### Phase 2 — Render generalization (one shared-code fork)
`build_heatmap_image` in `panels/chart/paint/heatmap.rs` consumes book snapshots today
(book-specific forward-fill/carry-in). The sim grid is *simpler* (every candle is a dense
column, no gaps).
- **Strategy A (recommended): extract a generic core** —
  `build_image_from_columns(columns, settings) -> RenderImage`; refactor the book path to
  produce `HeatmapColumn`s and call it (a **no-behavior-change** refactor → verify OB
  heatmap renders identically). Liq feeds the same core. DRY.
- **Strategy B (fallback): parallel build method** if the book builder is too entangled to
  extract cheaply. Faster, lower risk to OB heatmap, some duplication.
- Either way: add a **second field** `liq_heatmap: HeatmapLayer` to `ChartState` (existing
  one at `state.rs:255`) — independent cache/throttle (`MIN_REBUILD_INTERVAL_MS` ≈ 140ms).

### Phase 3 — Indicator kind + registration (mechanical)
- New `indicators/liq_heatmap.rs`: `LiqHeatmapParams { mmr, lookback_ms, settings:
  HeatmapSettings }`; impl `IndicatorKind` — `kind_id "liq_heatmap"`,
  `PaneKind::OverlayOnly`, `label "Liquidation Heatmap"`, `compute → IndicatorOutput::Heatmap`
  (existing no-op marker, `output.rs:153`), `value_at → Empty`, `y_range → None`,
  `params_json`, `as_any[_mut]`, `custom_settings_view → LiqHeatmapSettingsView`.
- `indicators.rs`: add `KindEntry` in `kind_entries()` (~`:157`), `| "liq_heatmap"` in
  `is_singleton_kind()` (`:170`), deserialize arm in `build_kind()` (~`:213`).
- **Persistence is free** — `IndicatorPrefs` (`panels.rs:198`) round-trips `params_json`.

### Phase 4 — ChartState sim wiring
- Mirror OB accessors (`state.rs:1438–1467`): `liq_heatmap_instance()`,
  `has_liq_heatmap_indicator()`, `liq_heatmap_enabled()`, `liq_heatmap_params()`.
- New `refresh_liq_heatmap(...)` mirroring `refresh_heatmap` (`:1474`): sync instance params
  → `self.liq_heatmap.{enabled,settings}`; throttle via the layer; run `simulate(...)` over
  the candle/OI/mark series ChartState already gathers for `ComputeCtx`; feed columns to the
  layer. `liq_heatmap_paint_rect()` mirroring `:1522`.

### Phase 5 — Subscription gating
- Extend `refresh_chart_mark_price_sub` (`panels.rs:2194`) and the OI sub gate to include
  `has_liq_heatmap_indicator()` (candles always present; **no book sub needed** — liq does
  not touch `refresh_chart_book_sub`).
- Add `refresh_chart_liq_heatmap_texture(...)` mirroring `:2352`, called in the render path
  next to the existing call (`panels.rs:3165`).

### Phase 6 — Settings view (second new UI piece)
- New `panels/chart/liq_heatmap_settings.rs`: bespoke `LiqHeatmapSettingsView` hosted via
  the custom-view hook (`indicator_settings.rs:124–144`). Reuse the two-handle log color
  `SliderState` from `heatmap_settings.rs:44`; add `max_opacity` + `show_text` switches plus
  number inputs for `MMR` and `lookback margin`. Writes route via
  `IndicatorTarget<LiqHeatmapParams>`.

### Phase 7 — Paint + verify
- `view.rs` (`:783`, `:2394`): capture `state.liq_heatmap_paint_rect()` and call
  `paint_heatmap(...)` for the liq layer, sequenced **after** the book heatmap, **before**
  candles.
- Verify: `./scripts/build-wasm.sh` + `make dev` (+ `make server` + populated DB). Confirm
  bands form below price on long-heavy candles, clear when price wicks through, the left edge
  is populated (warm-up), and both heatmaps coexist independently.

## Implementation status — shipped 2026-06-28 (client v0.5.0)

All seven phases landed. The feature is a singleton overlay indicator
(`liq_heatmap`) added from the "+ Indicator" picker; its texture paints behind
the candles, after the orderbook heatmap, through the **shared**
`paint_heatmap`. Key files:

- `indicators/liq_heatmap/sim.rs` — the pure sim (`simulate`), 8 `wasm_bindgen_test`s.
- `indicators/liq_heatmap/mod.rs` — `LiqHeatmapParams` (the `IndicatorKind` façade).
- `panels/chart/paint/liq_heatmap.rs` — `LiqHeatmapLayer` (cache + throttle + texture
  build), reusing `heatmap.rs`'s now-`pub(super)` `flush_one` / `colorize_range` /
  `build_values` and the shared `HeatmapRect` / `HeatmapValues` / `HeatmapSettings`.
- `panels/chart/liq_heatmap_settings.rs` — `LiqHeatmapSettingsView` (slider + form).
- Registration in `indicators.rs`; wiring in `state.rs` (`liq_heatmap` field +
  `refresh_liq_heatmap`), `panels.rs` (OI+mark sub gates + texture refresh), `view.rs`
  (second `paint_heatmap` call).

**Deviations from the plan, with rationale:**

- **Magnitude stored in COIN (contracts), not USD.** The model line `notional = ΔOI ×
  mark` was the USD form; the layer instead stores `ΔOI × split` (coin) and the renderer
  recovers USD as **coin × the bucket's liquidation price** (exactly where the position
  sits). This is what makes decision #8 ("reuse the OB slider verbatim") and the Coin/USD
  text toggle actually work — `paint_heatmap`/text are reused byte-for-byte. The magnet
  *shape* is identical (mark is a ~constant scale factor). Mark price is still threaded
  and used as the **entry-price fallback** when a bar ships no VWAP. Colour domain is
  liq-specific (`LIQ_COLOR_RANGE_MIN/MAX` = 1 .. 1e6 coin; defaults lo 5 / peak 500),
  since magnets accumulate larger coin totals than resting book size.
- **Consume runs BEFORE place each candle** (not the pseudocode's place→consume order).
  Equivalent for realistic leverage (bands land outside a single candle's range), and it
  makes "an entry can't self-liquidate" exact: a candle's own placement is never swept by
  its own `[low, high]`.
- **Strategy B (parallel build), not A (extract+refactor the book builder).** The book
  `build_full` is entangled (carry-in, striding, best-first level scan, patch path); the
  sim grid is a dense column per candle, so a separate build is simpler and zero-risk to
  the OB heatmap. Shared code is the low-level colour/flush/value primitives only.
- **No incremental patch path.** The sim is cheap (a few ops per loaded candle), so each
  throttled (≈140 ms) refresh re-runs it whole. A `(window, band, dims, settings, mmr,
  lookback, data-fingerprint)` key gates the rebuild.
- **MMR + lookback are user-facing** number inputs in the settings view (the plan listed
  them as such); tiers/weights stay fixed (`[10,25,50,100]×`, equal).
- **Price bucket ("tick size") is user-selectable** (post-ship addition — decision #10 had
  locked it at $5). A `bucket` field on `LiqHeatmapParams` (stored in **dollars**) flows
  into `SimParams.bucket` (the sim buckets liq prices at this width) and the render band,
  and it's in the texture rebuild key. The settings view exposes it as a **free-form "Tick
  size" number input in ticks** (1 tick = `TICK_SIZE` = $0.10, the BTCUSDT increment;
  clamped `MIN_BUCKET_TICKS`=1 .. `MAX_BUCKET_TICKS`=100000, default 50 ticks = $5) — the
  getter/setter convert ticks↔dollars so the sim/render stay in dollars. Coarser merges
  nearby magnets into fatter rows.
- **Magnet "profile" toggle** (post-ship addition). A `show_profile` bool on
  `LiqHeatmapParams` (default off, in the texture rebuild key) draws a right-anchored
  horizontal histogram: estimated liq notional per price bucket across the built columns.
  Aggregated inline in the layer's `build_full` (one cheap pass over the sparse per-column
  bucket lists) into a `HeatmapProfile` carried on the shared `HeatmapRect` (the orderbook
  heatmap leaves it `None`). Painted by `paint_heatmap_profile` **in front of** candles
  (after `paint_main_chart`, before overlay indicators). Key choices:
  - **Per-bucket value is the PEAK** any single column reached, not a sum — magnets persist
    across columns, so a sum would scale a level by its on-screen dwell instead of its
    strength. The peak also keeps each bar on the **same value scale as the heatmap cells**,
    so the shared log colour ramp (`lo`/`log_lo`/`log_span`) colours bar `k` identically to
    the hottest cell at that price (bar length + colour both come from that ramp).
  - **Anchored at `canvas_w − y_axis_gap`** (the plot's right edge, price axis excluded —
    matching VRVP's `chart_left + chart_w`), so it never overlaps the y-axis labels.
  - **Width is user-settable**: `profile_width_pct` on `LiqHeatmapParams` (peak bar = this %
    of plot width; `MIN/MAX_PROFILE_WIDTH_PCT` 2..50, default `DEFAULT_PROFILE_WIDTH_PCT`
    16%). Paint-time only — **not** in the rebuild key (read via
    `ChartState::liq_heatmap_profile_width_frac`, passed straight to the painter).
  - Settings: a "Show profile" switch + a "Profile width" **slider** (`%`, `visible_if`
    show_profile) in the settings view.
- **Selectable colour map** (post-ship, shared with the orderbook heatmap). The single
  fixed `ramp()` was replaced by a `Colormap` enum (`heatmap.rs`) with `sample(t)` — variants
  Heat (default / legacy), Inferno, Magma, Plasma, Viridis, Turbo, Grayscale (5-stop
  perceptual approximations; Turbo 6, grey 2). `colormap` is a field on the **shared**
  `HeatmapSettings` (`#[serde(default)]` → old blobs get Heat), so it's in the texture
  rebuild key and **both** heatmaps honour it (the OB heatmap defaults to `Heat`, but the
  liq heatmap overrides its default to **`Plasma`** in `default_liq_settings()` — its dark
  indigo base reads better for sparse predictive magnets); it threads through `colorize_range`
  (texture), `HeatmapValues` (crisp-cell + text — text contrast now derives from the actual
  ramp luminance) and `HeatmapProfile` (profile bars). Exposed as a "Colour map" dropdown in
  *both* the OB and liq settings views.

**Not yet validated against a populated DB** — verified by `cargo test -p client
--target wasm32-unknown-unknown` (sim) + `./scripts/build-wasm.sh` + `make check-client`.
A live `make server` + `make dev` smoke (bands form below price on long-heavy candles,
clear on a wick-through, left edge populated, coexists with the OB heatmap) is the
remaining manual check.

## Deferred to v2
Aging window (drop positions older than N) · long/short tint toggle · configurable
tiers + weights · lookback selector UI · tiered Binance MMR brackets · realized
`forceOrder` overplot as model validation · USD-weighted magnitude (currently coin).

## Related
- `docs/ORDERBOOK_HEATMAP.md` — façade pattern, `HeatmapLayer`, color slider, the render
  pipeline this reuses.
- Memory: `project_liq_heatmap_design`, `project_orderbook_heatmap_deferred`,
  `project_mark_price_funding` (this was the "deferred liq heatmap" referenced there),
  `project_net_ls_ob_imbalance` (pure-client derived-indicator pattern).
