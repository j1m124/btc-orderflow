# Orderbook Heatmap — Design (v1)

A Bookmap-style liquidity heatmap rendered **behind the candles**: x = time, y = price
(shared with the candle axis), color intensity = resting book size at that
(price, time) cell. Candles/wicks paint on top.

This document is the locked v1 design from the design interview. It supersedes the
two exploratory paths in the old "orderbook heatmap deferred" note.

## What it is NOT

It is **not** an `IndicatorKind`. The indicator trait computes from
`compute(&[Candle], ComputeCtx)`, and `ComputeCtx` carries only
`volume_unit / footprint / view_time_range / liquidation_bars / open_interest`
(`indicators/kind.rs:34-68`) — **no book data, live or historical**. A heatmap needs a
time-series of book snapshots, which lives outside that context. Forcing it into the
trait would break all existing indicators.

Instead it follows the **footprint** precedent: a *render layer* that owns its own
subscription + cache + paint pass over the candle price-axis (`paint/footprint_render.rs`).

## Locked decisions

| # | Decision | Choice |
|---|----------|--------|
| 1 | Integration | Overlay **behind candles**, sharing the candle price-axis. Independent `bool` toggle, **orthogonal to `RenderKind`**, painted behind whatever the main render is. Toggle lives next to the render-mode selector. |
| 2 | Scope | **History-backed** (live + backfill), not live-only. |
| 3 | Persistence | Bump `BOOK_SNAPSHOT_DEPTH` 50 → **1000** (the REST-seed ceiling) **+ add a Timescale compression policy** on `book_snapshots`. |
| 4 | Data model | The **market-data service owns a unified book time-series**: history paged on the left, a **1s live sampler** appending on the right. Chart just reads it. |
| 5 | Grid | ~1 device-pixel resolution. **Sum across price levels, max across time.** Price-bucket width is a **user setting** (`$1/$5/$10/$25`). |
| 6 | Color | **log1p** scale; normalize to a **fixed reference auto-seeded from ~p99** of loaded data, **manually overridable** (recomputed lazily, not per-frame); **single perceptual ramp** behind candles, capped max-alpha. |
| 7 | Render | **Image-blit** via `window.paint_image` + `RenderImage` (one GPU-textured quad), **rebuilt only on change**, throttled ~2–5 Hz. **Lazy in both axes.** |
| 8 | Toggle | Independent boolean overlay, orthogonal to `RenderKind`. |
| 9 | v1 settings | Price-bucket width, color reference (auto/manual), max opacity, one built-in ramp. |

## Why depth = 1000, not 10000

`/fapi/v1/depth` caps at `limit=1000` (Binance hard max), so the maintainer's in-memory
`Book` holds ~1000 levels/side. Both `top_n(10000)` (the ladder) and a
`BOOK_SNAPSHOT_DEPTH = 10000` silently clamp to "everything the book holds" ≈ 1000.
Set the constant to **1000** with a comment that it's the REST-seed ceiling — same
outcome, honest about the limit. (The live ladder "working at 10000" was really showing
~1000.)

## Data path

```
Binance depth@100ms ─► DepthDiff broadcast ─► book maintainer (Arc<RwLock<Book>>, ~1000 levels)
                                                   │ persists top-1000 @ 1s ─► book_snapshots (compressed)
                                                   ▼
   HistoryPage query (lazy, paged left) ◄──── gateway forwarder ────► BookSnapshot/BookDelta (live)
                                                   │
                                                   ▼
   market-data service: unified book time-series per (symbol, depth)
     • left:  pages from load_older_book (HistoryPage), driven by visible window
     • right: 1s sampler appends current live `book`
                                                   │  (chart reads, like book_snapshot)
                                                   ▼
   chart heatmap paint: aggregate (visible window only) ─► BGRA buffer ─► RenderImage ─► paint_image
                                                   │
                                                   ▼  candles paint on top
```

## Server changes

1. **`crates/server/src/ingest.rs:120`** — `BOOK_SNAPSHOT_DEPTH: usize = 50` → `1000`
   (comment: REST-seed ceiling; `/fapi/v1/depth` max). Persist cadence stays 1s
   (`BOOK_SNAPSHOT_INTERVAL`). No change to the maintainer logic — `top_n(1000)` just
   returns more rows.
2. **New migration** (`make db-migration NAME=book_snapshots_compression`):
   - `.up.sql`: `ALTER TABLE book_snapshots SET (timescaledb.compress,
     timescaledb.compress_segmentby = 'symbol', timescaledb.compress_orderby = 'ts DESC');`
     then `SELECT add_compression_policy('book_snapshots', INTERVAL '2 hours', if_not_exists => TRUE);`
   - `.down.sql`: `SELECT remove_compression_policy('book_snapshots', if_exists => TRUE);`
     then `ALTER TABLE book_snapshots SET (timescaledb.compress = false);`
   - Compression interval (2h) sits well inside the 48h retention; recent ~2h stays
     uncompressed for fast writes. Access pattern (`WHERE symbol AND ts < x ORDER BY ts
     DESC LIMIT`) matches `segmentby=symbol, orderby=ts DESC` perfectly.

Storage at depth 1000, 1s, 48h: ~5.6 GB raw → ~1 GB compressed — comfortable on the
80 GB CPX22. Existing 50-level history self-heals: it ages out within 48h, after which
all visible history is deep.

## Client changes

### Service (`crates/client/src/services/market_data.rs`)
- A unified per-`(symbol, depth)` book time-series. Reuse `book_history`
  (`Vec<BookSnapshotEntry>`, oldest-first) as the left/historical span; add a **right/live
  tail** fed by a **1s sampler** that appends the current `book` snapshot.
- **Lazy-x paging**: drive `load_older_book` from the chart's visible time window — fetch
  another `HistoryPage` when the window's left edge approaches the oldest loaded snapshot;
  stop on `HistoryCapped`. (Server caps a page at 1000 rows ≈ 16.7 min; a wide window
  needs several round-trips — acceptable, lazy.)
- Read API for the chart: a slice/iterator over entries within a requested `(lo_ms, hi_ms)`.

### Chart state (`panels/chart/state.rs`)
- `heatmap_enabled: bool` + a `HeatmapSettings { price_bucket, color_ref: Auto|Manual(f64),
  max_opacity, ... }` struct (persisted via `persistence`, mirrored to a static if the
  paint path needs lock-free reads).

### Coords (`panels/chart/coords.rs`)
- Add `ts_to_index(candles, ts_ms) -> f32`: binary-search the candle containing `ts`, then
  `index + (ts - open_time)/tf_ms`. Feeds `index_to_screen` for sub-bar x placement.

### Paint (`panels/chart/paint/heatmap.rs`, new)
- Painted **first** (behind the main render). Steps:
  1. Determine visible window: `(view_start, view_size)` → `(lo_ms, hi_ms)`; `(y_lo, y_hi)`
     → price band. **Lazy in both axes** — only touch data inside this rectangle (+margin).
  2. Aggregate to a texel grid: columns = adaptive time buckets (1s when ≥1px/col, else
     group by **max**); rows = price buckets (`sum` of levels in band). Reduction:
     **sum across price within a snapshot, max across time within a column.**
  3. Normalize: `log1p(size)` / `log1p(reference)`, reference = manual or lazily-computed
     p99 of the aggregated cells. Map to the ramp; alpha scaled by `max_opacity`.
  4. Fill a **BGRA** `Vec<u8>` (gpui atlas byte order) sized to the texel grid → `image::Frame`
     → `RenderImage::new` → `window.paint_image(bounds, Corners::default(), Arc::new(img), 0,
     false)`. GPU stretches the texture to `bounds` (bilinear default).
- **Throttle**: rebuild the `RenderImage` only when data/view/settings change, coalesced to
  ~2–5 Hz. When nothing changes, the cached textured quad re-draws for free.

### View / toggle (`panels/chart/view.rs`)
- A toggle next to the render-mode selector flips `heatmap_enabled`. First enable opens the
  book subscription (footprint-style lazy sub) and kicks the visible-window paging.

### Settings (`settings_form/` + a `FloatingWindow` slot)
- v1 form: price-bucket dropdown, color-reference (Auto / manual number), max-opacity slider,
  one ramp. Mirror the footprint-settings floating window.

## Risks / gotchas

- **First use of `window.paint_image` in this codebase** (screenshot.rs uses the high-level
  `Image` *element*, not the canvas paint path). Standard gpui API (`window.rs:3797`) but
  untrodden here.
- **Atlas cache lifecycle.** The sprite atlas keys on `RenderImage.id`. Mutating the heatmap
  means building a **new** `RenderImage` (fresh id) each rebuild; ensure the previous tile is
  released so atlas tiles don't leak. Throttling rebuilds also bounds churn.
- **BGRA byte order** — fill the buffer B,G,R,A, not R,G,B,A.
- **Live-edge vs history seam.** Both are now ~1000 levels deep, so the seam is mild, but the
  live tail samples at 1s to match the persisted cadence so column widths are uniform.
- **p99 recompute** must be lazy (on data/window change), never per-frame.
- **Live tail memory** — cap the in-memory series to the visible window + margin; evict older.

## v1 scope vs deferred

**v1 ships:** overlay behind candles; history-backed (depth 1000 + compression); 1s live
sampler; pixel-grid (sum-price / max-time); log + p99-seeded overridable reference; single
ramp; image-blit render, lazy both axes, throttled; settings = bucket / reference / opacity.

**Deferred:** bid/ask color-tint split; Coin vs USD sizing toggle; multiple/custom ramps;
nearest-vs-bilinear sampling control (default bilinear); price auto-zoom to fit deep walls
(use manual y-range for now).

## Suggested implementation order

1. **Server**: depth bump + compression migration; deploy so deep history starts
   accumulating (48h to fully heal).
2. **Service**: 1s sampler + unified series + visible-window-driven paging; a read API.
3. **Coords**: `ts_to_index`.
4. **Paint**: `heatmap.rs` with the BGRA → `RenderImage` → `paint_image` pipeline (start with
   a fixed reference + one ramp, no throttle) to prove the render path.
5. **State + toggle**: wire `heatmap_enabled` and the lazy subscription.
6. **Throttle + lazy windowing + p99 reference**: optimize.
7. **Settings form**.

## Implementation status — shipped 2026-06-23

All seven steps landed. Notable deviations from the design above, with rationale:

- **Step 1 — depth + compression.** `BOOK_SNAPSHOT_DEPTH` 50→1000 (`ingest.rs`). New paired
  migration `..._book_snapshots_compression` (`compress_segmentby='symbol'`,
  `compress_orderby='ts DESC'`, `add_compression_policy(INTERVAL '2 hours')`). Not yet deployed —
  deep history heals over 48h once the server runs the migration on boot.
- **Step 3 — reused `time_to_idx`, did NOT add `ts_to_index`.** `drawings_view::time_to_idx`
  already does exactly the time→fractional-index mapping (gap-safe, neighbour-spacing, unit-tested),
  so the heatmap reuses it via a chart-facade re-export rather than duplicating the binary search.
- **Texture build runs in `ContentPanel::render`, not the paint closure.** It needs a `&mut Window`
  to evict the previous atlas tile (`window.drop_image`) — the App-only paint closure can't. The
  closure just captures the cheap `Arc`-backed `HeatmapRect` and blits. Reading the book series
  straight from the service avoids a per-frame clone of the (large) time-series.
- **Atlas lifecycle.** The wgpu atlas only releases a tile on explicit `drop_image` (no per-frame
  GC), so: rebuilds drop the old tile; toggle-off drops it; symbol/TF switches **move** the
  `HeatmapLayer` across the `*self = Self::new()` rebuild (carrying the live `Arc`) so the next
  `refresh` releases it instead of leaking. **Known minor leak:** closing a chart panel while the
  heatmap is on orphans one tile (no `&mut Window` in `Drop`); reclaimed on GPU-error recovery or
  reload. Acceptable for v1.
- **Sampling is opt-in + refcounted** (`enable/disable_book_sampling`), so an orderbook-only
  session never pays to sample. The heatmap subscribes at the fixed `HEATMAP_DEPTH = 1000`; the
  orderbook panel's own (deeper) sub is a different key and is not sampled.
- **Throttle/lazy/memory.** Rebuild gated on a `(series fingerprint, window±25% margin, price band,
  texel dims, settings)` key; paint re-maps the data-rect every frame so pans stay smooth between
  rebuilds. Lazy in both axes (binary-search the time window, filter the price band). p99 computed
  only on rebuild. Live-tail memory bounded by `evict_book_history_before` (keep visible ± one span,
  throttled 2s so eviction doesn't churn the rebuild key) plus a `BOOK_HISTORY_HARD_CAP` (~6h)
  backstop — zooming the heatmap past ~6h shows only the most recent 6h.
- **Settings + persistence.** `HeatmapSettingsView` floating window (gear next to the header
  toggle): price-bucket, colour reference (auto p99 / manual), max opacity. State persists in the
  per-panel `ChartPrefs` (all `serde(default)`, so no `LAYOUT_VERSION` bump).

Key files: `services/market_data.rs` (sampler + series API + eviction), `panels/chart/paint/heatmap.rs`
(build + blit), `panels/chart/heatmap_settings.rs` (form), `panels/chart/state.rs` (`HeatmapLayer`
field + refresh), `panels/chart/view.rs` (toggle + paint wiring), `panels.rs` (sub lifecycle +
texture refresh + paging + persistence), `workspace.rs` (settings floating-window slot).

### Refinements — 2026-06-23 (post-ship)

- **Depth 1000 → 4000.** Both `BOOK_SNAPSHOT_DEPTH` (`ingest.rs`) and `HEATMAP_DEPTH` (`heatmap.rs`)
  bumped to surface liquidity further from mid. The REST seed still fills ~1000/side (Binance hard
  cap); the `depth@100ms` diff stream accumulates the maintained book deeper over time, so
  `top_n(4000)` returns whatever the book actually holds. ~4× the per-sample bytes on deep zoom-out.
- **Price bucket fixed at 50 ticks ($5), no longer a setting.** `PRICE_BUCKET = 5.0` const in
  `heatmap.rs`; the dropdown + `HeatmapSettings.price_bucket` + the `heatmap_bucket` pref are gone.
  The texture is rendered at pixel-row resolution but every row's value is its $5 bucket's sum, so
  buckets read as crisp horizontal bands. (Settings now: colour reference + max opacity only.)
- **Forward-filled columns.** Each book snapshot fills its full column span (from its timestamp to
  the next snapshot's, or the right edge for the latest), with a max-reduction where several
  snapshots land in one column (zoomed out). Fixes the "thin vertical strips with gaps" look when
  zoomed in — a 1 s sample now spans seconds of screen width. A carry-in snapshot from just before
  the window forward-fills the left edge.
- **In-cell values when zoomed in.** When an on-screen cell clears ~40 px wide × ~13 px tall, the
  paint pass overlays each cell's book size as centred text (compact `1.2k`/`340`/`4.5`, contrast
  colour chosen by cell luminance). The rebuild keeps a compact logical-cell value table
  (`HeatmapValues`, gated on ≤240 samples × ≤200 buckets so it's only retained when text could
  actually render); `paint_heatmap` now takes `&mut App` for the text shaper.
- **Auto colour grading removed → fixed range slider.** `ColorRef` (Auto p99 / Manual) and
  `percentile_nonzero` are gone. `HeatmapSettings` now carries `color_lo` + `color_peak` (coin
  units): cells below `lo` aren't drawn at all, `peak` maps to the ramp top, and the ramp is a
  fixed log map `norm = (ln1p(v) − ln1p(lo)) / (ln1p(peak) − ln1p(lo))` — colours never breathe.
  The setting is a two-handle log-scale **range `Slider`** (`gpui_component::slider`) hosted on
  `HeatmapSettingsView` (the declarative `SettingsForm` is stateless, so the stateful `SliderState`
  lives on the view and writes through `apply_heatmap_settings` on `Change`; opacity stays a form
  field). Domain `COLOR_RANGE_MIN..MAX` (1..10 000). Persistence: `heatmap_manual_ref` → `color_peak`
  (name kept for back-compat — the old reference *was* the ramp top), new `heatmap_color_lo` →
  `color_lo`. *Known v1 cost:* each drag tick is a settings change → full texture rebuild incl.
  re-aggregation; snappy zoomed-in, can lag on a deep zoom-out drag (settles on release).
- **Coin/USD toggle drives the in-cell text.** The cell numbers follow the chart's `VolumeUnit`:
  USD multiplies the coin size by the bucket midpoint price (same convention as footprint's
  `sided_volumes`). Colour normalization stays coin-based; only the displayed number converts.
  `fmt_compact` gained `M`/`B` suffixes for the USD-notional range.
- **One column per candle (timeframe-wide).** The build now groups the 1 s book samples into
  `tf_ms`-wide columns aligned to the candle grid (anchored on `candles[0].open_time`), so each
  heatmap column is exactly one candle wide instead of one 1 s sample. Within a candle the
  per-bucket value is the **max** over that candle's samples. `refresh`/`build_heatmap_image` take
  `tf_ms` + `anchor_ms`; `tf_ms` and the candle phase joined the rebuild key (a TF switch keeps the
  same book series, so the fingerprint alone wouldn't trigger a rebuild). Columns are shifted by
  −½ candle (`half`/`tail`) because candles are drawn *centred* on their index — this keeps each
  column under its candle body/gridline rather than straddling the boundary.
- **Two new toggles: "Show cell values" + "Extend latest to edge."** `HeatmapSettings` gained
  `show_text: bool` and `extend_right: bool` (both default `true`), surfaced as `Field::switch`
  rows in the settings form. `show_text` off gates `want_values` in `build_heatmap_image` so the
  logical-cell table is never even retained (→ `values: None` → the paint pass draws no text).
  `extend_right` off makes the `flush` closure stop the live candle's column at its own slot edge
  (`col_of(slot_hi)`) instead of stretching to `cols`; the right-edge fill that always happened for
  the latest candle is now opt-out. Both are in `HeatmapSettings`'s `PartialEq`, so toggling either
  triggers a texture rebuild via the existing settings-changed cache key. Persistence:
  `heatmap_show_text` + `heatmap_extend_right` (`Option<bool>`, `serde(default)`).
- **Rebuild-perf pass (fixes drag / update FPS drops).** The "throttled ~2–5 Hz" the design
  claimed was never actually implemented — every render (up to ~20 Hz via the 50 ms tick loop)
  called `refresh`, and each rebuild re-bucketed the **full** book (now thousands of levels deep,
  not 50) for every sample in the window. Three fixes: **(1) real rebuild throttle** —
  `HeatmapLayer` carries `last_build_ms`; `refresh` takes `now_ms` and skips a wanted rebuild (keeps
  the cached bitmap, which the paint pass still remaps onto the live view) until
  `MIN_REBUILD_INTERVAL_MS` (140 ms ≈ 7 Hz) elapses; the tick loop guarantees the trailing render.
  First build (no cache) is never throttled. **(2) early-break level scan** — `build_heatmap_image`
  walks `bids`/`asks` best-first and breaks once past the visible price band instead of filtering
  all N levels, so per-sample work tracks the band, not book depth. This needs a best-first
  invariant on `BookSnapshotEntry` (bids desc / asks asc): live samples already are (via
  `apply_levels`); `book_snapshot_entry_from_proto` now sorts history pages on receipt. **(3) sample
  striding** — when the window holds ≫ `cols·MAX_SAMPLES_PER_COL` samples, stride the loop (capped
  at the per-candle sample count so no candle is skipped, and the newest sample is always processed)
  to bound aggregation when zoomed way out. *Remaining lever:* `paint_heatmap_text` still shapes
  each visible label every frame — toggle "Show cell values" off (or add a shaped-line cache) if
  zoomed-in dragging is still heavy.
- **Bucketed deep storage (server) + matched live tail (client).** `book_snapshots` now stores depth
  **pre-bucketed at 50 ticks ($5)** and **`BOOK_SNAPSHOT_DEPTH` 4000 → 10000**: `persist_snapshot`
  folds the raw `top_n(10000)` (best-first, so equal buckets are adjacent — single-pass
  `bucket_sorted`) into $5 bins before the `upsert`. No migration — same `(price[], size[])` columns,
  just bucketed values; old raw rows coexist and still render (the heatmap re-buckets), aging out in
  48h. *Irreversible:* the DB no longer holds tick granularity for the heatmap path. **Ceiling
  caveat:** 10k is the cap on *accumulated active* depth, not true full depth — Binance REST seeds at
  most 1000/side, a sequence gap re-bootstraps back to the seed, and static walls beyond the seed are
  never reported by diffs. Client: **`HEATMAP_DEPTH` 4000 → 10000** to match, and the live 1 s sampler
  now **buckets each sample to $5** (`bucket_book_levels` in `sample_books`) so deep samples don't
  blow up `book_history` memory and the live tail's representation matches the paged history. The live
  wire `BookDelta` path is unchanged (still raw diffs, filtered to the requested top-N, batched
  100 ms; +5 s full resync now carries up to 10k levels). Bucket width is `BOOK_BUCKET_USD = 5.0` on
  both sides — must stay in sync with the render-time `PRICE_BUCKET`.
- **DB cadence 1s → 1m (`BOOK_SNAPSHOT_INTERVAL`).** 60× fewer persisted rows; one stored sample per
  1m candle, so 1m+ timeframes are unaffected. The client's live 1s sampler (`BOOK_SAMPLE_INTERVAL`)
  is *deliberately* left at 1s, so only DB-backed history is coarsened — the recent tail stays crisp.
  **Known limitation:** `build_heatmap_image` groups by candle and only fills candles that contain a
  stored sample, so at *sub*-1m timeframes (and on gap-minutes around reconnects) the paged history
  has **blank columns** between samples rather than carry-forward bands. A forward-fill pass (carry
  the last book across empty candles, gated on a per-candle "had data" flag so genuinely-empty price
  regions aren't painted) would make sub-1m / gappy history render continuously — deferred.
- **Y axis no longer lazy.** Supersedes the design's "lazy in both axes." `build_heatmap_image` now
  derives the price band from the **full extent of resting liquidity** in the windowed snapshots
  (cheap: bucketed sides are best-first, so per-snapshot extremes are first/last entries — O(1)) and
  sizes the texture to **`VERTICAL_OVERSAMPLE` (8) texels per $5 bucket** (capped at `MAX_TEXELS`) —
  several identical texels per bucket so bilinear upscaling only blends at bucket boundaries, keeping
  bands crisp; one-texel-per-bucket looked soft/gradient-blurred. Consequences: vertical
  pan/zoom never rebuilds or clips — it's a pure paint-time re-stretch (price + rows dropped from the
  rebuild key and from `refresh`'s args; only time/cols/tf/phase/fingerprint/settings remain); the
  whole book height is always present, so scrolling y reveals deep liquidity instantly. Pairs with the
  bucketed storage — a $5-bucketed snapshot has only a few hundred entries, so full-extent aggregation
  is cheap. Trade-offs: with deep (10k) books the extent can be wide, so the near-mid region occupies
  a smaller vertical slice when zoomed out (each visible bucket still gets ≥1 texel up to a ~$10k span,
  beyond which rows cap and downsample); in-cell text is suppressed once the extent exceeds
  `TEXT_MAX_BUCKETS` (= `MAX_TEXELS`) buckets. `TEXT_MAX_BUCKETS` was raised from 200 → `MAX_TEXELS`
  so text still shows at the now-wider bucket counts (the `TEXT_MAX_SAMPLES` time gate is the real cap;
  worst-case table ~2 MB).
- **Hard-edged cells (crisp boundaries).** `paint_image` is bilinear-only (gpui exposes no
  nearest-neighbour sampler), so the stretched texture always blends across cell edges — soft
  boundaries. Fix: when the value table is present and the uniform on-screen cell height ≥
  `MIN_CELL_H_FOR_CRISP` (2.5 px), `paint_heatmap` paints the **visible lit cells as solid
  `PaintQuad`s** (`paint_heatmap_cells`) instead of blitting — cells tile exactly, so equal-colour
  neighbours read seamless and different-colour neighbours get a sharp edge. Below the threshold (or
  no table → zoomed out) it falls back to the texture blit. The value table is now retained
  independent of `show_text` (it feeds both the cell painter and the text), so `HeatmapValues` gained
  `max_opacity` / `show_text` / `extend_right` (the cell colour + alpha match the texture build; the
  last cell honours `extend_right`). Both the cell and text painters iterate only `visible_bucket_range`
  (the full-extent table can hold ≫ on-screen buckets). Quad count is bounded by the table's
  `TEXT_MAX_SAMPLES` column cap × visible buckets; if it ever feels heavy, raise `MIN_CELL_H_FOR_CRISP`.
- **±$2500 price band on the book — server-side, both paths.** The depth bumps above exposed a
  latent bug: `BOOK_SNAPSHOT_DEPTH` / `HEATMAP_DEPTH` count **levels**, not price, and a real
  Binance book carries sparse, economically-dead resting orders far from mid (a $1k bid, a $105k
  ask) that the diff stream never removes. So `top_n(10000)` spanned **$100k+**, and the non-lazy-y
  "full extent" derivation (above) flattened the real near-mid liquidity into a single texel row —
  the heatmap "rendered but wrong". Fix is **entirely server-side** (the client is unchanged — it
  still derives the band from the full extent of whatever it receives, which is now pre-bounded):
  a new `Book::top_n_within_band(n, band)` (`binance/book.rs`) intersects the level count with a
  ±`band` window around mid. `BOOK_BAND_USD` (`ingest.rs`) = `BOOK_SNAPSHOT_DEPTH` × `BOOK_BUCKET_USD`
  = 500 × $5 = **±$2500/side**. Two call paths use it: `persist_snapshot` reads the whole book within
  the band (`n = usize::MAX`) before bucketing (history rows now ≤500 buckets/side, the phantom tail
  gone); the live forwarder (`gateway/session.rs`, all three `top_n` sites — initial snapshot, the
  per-batch delta-filter window, and the periodic resync) bounds the live stream the same way. The
  band is an **intersection** with each subscription's depth, so the orderbook ladder (depth 1000,
  always near mid) is unaffected — only the heatmap's depth-10000 sub is actually band-bounded.
  `BOOK_SNAPSHOT_DEPTH` is now read as a **$5-bucket count** (500 ⇒ $2500), not a raw-level count.
  *Trade-off:* "scroll-y reveals deep liquidity" is now capped at ±$2500; walls beyond that are no
  longer stored or streamed. *Backlog:* the server change bounds only **new** rows — phantom rows
  already written (since the depth-10000 deploy) keep their $100k span until they age out under the
  14-day retention, so they must be deleted (`DELETE FROM book_snapshots WHERE array_length(bid_prices,1)
  > 510 OR array_length(ask_prices,1) > 510`) to clean the rendered history immediately.
