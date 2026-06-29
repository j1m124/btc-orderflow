//! Orderbook liquidity heatmap — a render layer painted *behind* the main
//! chart (candles paint on top). x = time (shared candle axis), y = price
//! (shared price axis), colour = resting book size at that (price, time) cell.
//!
//! Unlike the indicator framework (whose `ComputeCtx` carries no book data),
//! this owns its data path end to end: the market-data service samples the live
//! book into a per-`(symbol, depth)` time-series, and [`HeatmapLayer`] turns the
//! visible slice of that series into one GPU texture blitted via
//! `window.paint_image`. See `docs/ORDERBOOK_HEATMAP.md` for the full design.
//!
//! ## Grid model
//! Price is bucketed into fixed **50-tick ($5) rows** (not a user setting — see
//! [`PRICE_BUCKET`]); the snapped band is rendered at pixel-row resolution so
//! each bucket reads as a crisp horizontal band. Time is forward-filled: every
//! book snapshot fills its full column span (until the next snapshot, or the
//! right edge for the latest) so a 1 s sample spans seconds of screen width when
//! zoomed in, instead of a single 1-texel strip.
//!
//! ## Why a cached texture (not per-frame rebuild)
//! gpui's sprite atlas keys on `RenderImage.id` and only releases a tile on an
//! explicit `window.drop_image`. A fresh `RenderImage` every frame would leak
//! atlas tiles. So [`HeatmapLayer`] keeps the built `Arc<RenderImage>` and
//! rebuilds only when the data, the visible window (beyond a margin), the price
//! band, the texel dims, or the settings change — dropping the previous tile on
//! each rebuild. Between rebuilds the paint pass re-maps the texture's fixed
//! data-rect onto the *current* view transform, so pans/zooms stay smooth even
//! though the bitmap is stale. When cells are large enough on screen the paint
//! pass also overlays the per-cell book size as text (rebuild keeps a compact
//! logical-cell value table for this — see [`HeatmapValues`]).

use std::sync::Arc;

use gpui::{
    App, BorderStyle, Bounds, Corners, Edges, Hsla, PaintQuad, Pixels, Point, RenderImage, Rgba,
    Window, hsla, point, px, size,
};
use image::{Frame, RgbaImage};

use super::super::{index_to_screen, price_to_screen, time_to_idx};
use crate::persistence::VolumeUnit;
use crate::services::market_data::{BookSnapshotEntry, Candle};

/// Book depth the heatmap subscribes + samples at. A generous level-count cap —
/// the *span* is bounded server-side: the forwarder intersects this depth with a
/// ±$5000 price band (`BOOK_BAND_USD`), dropping the phantom far-from-mid tail
/// before it ever reaches the wire, so both the live stream and the persisted
/// history arrive pre-bounded and the client renders the full extent as-is. The
/// live 1 s sampler buckets each (already band-bounded) sample to `$5` before
/// storing (see `sample_books`), so this depth doesn't blow up client memory.
pub const HEATMAP_DEPTH: u16 = 10000;

/// Fixed price-bucket width in dollars: **50 ticks × $0.10** (BTCUSDT-perp tick).
/// Deliberately not a user setting — one stable row size keeps the heatmap
/// legible and the in-cell values readable.
const PRICE_BUCKET: f64 = 5.0;

/// Fraction of the visible span the built texture extends beyond each edge, so
/// a small pan/zoom between throttled rebuilds doesn't expose an unbuilt edge.
const MARGIN_FRAC: f64 = 0.25;

/// Upper bound on texel grid dimensions. Caps the per-rebuild buffer at
/// 2048×2048×4 = 16 MB and the aggregation cost; the GPU stretches whatever
/// resolution we build to the on-screen rect (bilinear), so undersizing only
/// softens detail, it never clips data.
const MAX_TEXELS: usize = 2048;
const MIN_TEXELS: usize = 16;

/// Texel **columns** to build per *visible* candle. The heatmap is per-candle
/// (each candle is forward-filled into a run of identical columns), so building
/// at full screen-pixel width is mostly wasted texels — the dominant per-rebuild
/// cost. We instead build at ~this many texels per visible candle (capped at the
/// screen width) and let the GPU stretch horizontally; with the ~25% build
/// margin that lands around 2 texels per actual candle column, enough to keep
/// candle boundaries from bilinear-merging. Shrinking `cols` is what makes both
/// the per-tick patch and a zoom-triggered full rebuild cheap.
const CANDLE_TEXELS: usize = 3;

/// Upper bound on the texture's **row** count (vertical resolution). With the
/// **lazy-y** band (the texture covers only the visible price range + pad, not
/// the full book extent — see [`Y_BAND_PAD_FRAC`]), a moderate zoom needs only a
/// few hundred buckets, so this cap is generous enough for a crisp 1-texel-per-
/// bucket blit there and only bites when zoomed all the way out (where blur is
/// expected). It still bounds the per-rebuild cell count + the `RenderImage`
/// re-upload.
const MAX_BLIT_ROWS: usize = 1024;

/// Texels per $5 price bucket in the (non-lazy) y direction. >1 so each bucket
/// is a solid block of identical texels and bilinear upscaling only blends at
/// bucket boundaries — keeps bands crisp instead of gradient-blurred. Capped by
/// [`MAX_TEXELS`], so for wide extents (n_buckets > MAX_TEXELS/this) it tapers
/// back toward 1.
const VERTICAL_OVERSAMPLE: usize = 8;

/// In-cell text shows only when a cell is at least this many screen pixels wide
/// and tall — below this the numbers are unreadable / overlapping, so they
/// auto-hide. Cell height is uniform (every bucket is $5), so the height gate is
/// a single early-out per rebuilt frame. The height gate is well above the 10 px
/// font so text drops out *early* as the y axis zooms out (cells shorten),
/// before the numbers get cramped — rather than clinging on until they clip.
const MIN_CELL_W_FOR_TEXT: f32 = 40.0;
const MIN_CELL_H_FOR_TEXT: f32 = 20.0;

/// When a (uniform-height) cell is at least this tall on screen, paint the
/// visible cells as solid quads with hard edges instead of blitting the
/// bilinear-stretched texture — gives crisp cell boundaries. This is *only* a
/// "is the hard edge perceptible?" floor: below ~3 px a $5 row's edge is within a
/// pixel of the bilinear blit's, so the blit is an indistinguishable, cheaper
/// fallback. The per-frame quad *work* is bounded separately by
/// [`MAX_CRISP_QUADS`] (the count cap is the real storm guard), so this floor can
/// sit low without risking the mid-zoom quad storm — keeping the crisp path alive
/// as the y axis zooms out, instead of popping to a soft blit at the first nudge
/// below the old 6 px floor (which on a tall canvas already blitted ~1.5 px-soft).
const MIN_CELL_H_FOR_CRISP: f32 = 3.0;

/// Upper bound on the crisp-quad path's per-frame work, counted as
/// `visible buckets × samples`. Above this the (crisp lazy-y) blit is used
/// instead — visually seamless with the quads — so a zoom can't spend a whole
/// frame emitting thousands of `paint_quad`s. Lowered alongside the higher
/// [`MIN_CELL_H_FOR_CRISP`]: together they confine quads to genuinely zoomed-in
/// views (few cells), killing the mid-zoom quad storm that grew gpui's instance
/// buffer and tanked the frame rate.
const MAX_CRISP_QUADS: usize = 6_000;

/// Don't retain the logical-cell value table (for text) past these counts — at
/// that point cells are far too small to label, so the table would never be
/// drawn and only wastes memory. `TEXT_MAX_SAMPLES` (time) is the real gate;
/// the bucket cap tracks the texel ceiling because the non-lazy y band now spans
/// the full book extent (many buckets, most off-screen). At the worst case
/// (`MAX_TEXELS` buckets × `TEXT_MAX_SAMPLES` samples × 4 B) the table is ~2 MB.
const TEXT_MAX_SAMPLES: usize = 240;
const TEXT_MAX_BUCKETS: usize = MAX_TEXELS;

/// Minimum wall-clock gap between two texture rebuilds. The chart re-renders at
/// up to ~20 Hz (the 50 ms tick loop) and every render calls `refresh`, so
/// without this a continuous pan/zoom or the 1 s data tick would rebuild the
/// whole texture 20×/s. Between rebuilds the paint pass remaps the cached
/// texture onto the live view, so panning stays smooth on a stale bitmap; this
/// just caps the rebuild rate. The tick loop guarantees a trailing render, so a
/// throttle-skipped rebuild always fires within one interval.
const MIN_REBUILD_INTERVAL_MS: i64 = 140;

/// When the visible window holds far more 1 s samples than there are texel
/// columns, processing every sample is wasted work (many collapse into one
/// column). Aim for at most this many processed samples per column; striding
/// keeps the aggregation bounded when zoomed way out.
const MAX_SAMPLES_PER_COL: usize = 3;

/// Padding added to each side of the **visible price range** when a full rebuild
/// derives the texture's price band (lazy-y), as a fraction of that range. The
/// padded band is *held* across cheap incremental patches and across small Y
/// auto-fit wiggles (the chart re-fits the price axis every frame), so we only
/// full-rebuild when the visible range grows past the held band (see
/// [`HeatmapLayer::refresh`]). Bigger pad ⇒ longer runs between rebuilds, at the
/// cost of a taller (more off-screen) texture and a slightly coarser blit. The
/// band is intentionally **not** the full book extent — off-screen liquidity is
/// clipped so the on-screen band renders crisp at 1 texel/bucket.
const Y_BAND_PAD_FRAC: f64 = 0.35;

/// Rebuild when the visible price range shrinks below this fraction of the held
/// band — i.e. the user zoomed the Y axis in far enough that the held (wider)
/// band would render the visible slice with too few texels (blurry). Re-centres
/// the band tight on the new range to restore crispness. Above this the held
/// band is reused (the pad absorbs ordinary auto-fit wiggle).
const Y_SHRINK_REBUILD_FRAC: f64 = 0.5;

/// Force a full rebuild at least this often even when patches would suffice.
/// Patches are byte-equivalent to a full build with the held band, so this is
/// pure insurance: any latent patch-path bug self-heals within the interval.
const SELF_HEAL_MS: i64 = 10_000;

/// Max candle closes a single patch will replay before deferring to a full
/// rebuild. A patch recomputes the trailing columns from the previous live
/// candle to the current one; if many candles closed at once (e.g. the tab was
/// backgrounded), a full rebuild is cheaper than a long trailing recompute.
const MAX_PATCH_CANDLES: i64 = 4;

/// Default low / peak of the colour range (coin units) when none is persisted.
const DEFAULT_COLOR_LO: f64 = 1.0;
const DEFAULT_COLOR_PEAK: f64 = 100.0;

/// Domain of the colour-range slider (coin units). Log scale, so `MIN` must be
/// > 0. The range covers tiny resting orders up to very large walls.
pub const COLOR_RANGE_MIN: f64 = 1.0;
pub const COLOR_RANGE_MAX: f64 = 10_000.0;

/// Colour gradient that maps a normalised heat value `t ∈ [0,1]` to RGB.
/// Selected per-instance in [`HeatmapSettings`]; honoured by **both** the
/// orderbook and liquidation heatmaps (the colorize / crisp-cell / text /
/// profile paths all sample through it). Stored by token (`as_str`) so old
/// persisted blobs without the field default to [`Colormap::Heat`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum Colormap {
    /// The original ramp: blue → cyan → green → yellow → red.
    #[default]
    Heat,
    /// Perceptually-uniform black → purple → red → orange → near-white.
    Inferno,
    /// Black → purple → magenta → orange → cream.
    Magma,
    /// Indigo → magenta → orange → yellow.
    Plasma,
    /// Dark purple → blue → teal → green → yellow.
    Viridis,
    /// Bright "improved rainbow" — purple → blue → teal → green → orange → red.
    Turbo,
    /// Simple dark → light grey.
    Grayscale,
}

impl Colormap {
    /// All variants in picker order. `Heat` first (the default / legacy look).
    pub const ALL: [Colormap; 7] = [
        Colormap::Heat,
        Colormap::Inferno,
        Colormap::Magma,
        Colormap::Plasma,
        Colormap::Viridis,
        Colormap::Turbo,
        Colormap::Grayscale,
    ];

    /// Stable serde / dropdown-value token.
    pub fn as_str(&self) -> &'static str {
        match self {
            Colormap::Heat => "heat",
            Colormap::Inferno => "inferno",
            Colormap::Magma => "magma",
            Colormap::Plasma => "plasma",
            Colormap::Viridis => "viridis",
            Colormap::Turbo => "turbo",
            Colormap::Grayscale => "grayscale",
        }
    }

    /// Human label for the picker.
    pub fn label(&self) -> &'static str {
        match self {
            Colormap::Heat => "Heat",
            Colormap::Inferno => "Inferno",
            Colormap::Magma => "Magma",
            Colormap::Plasma => "Plasma",
            Colormap::Viridis => "Viridis",
            Colormap::Turbo => "Turbo",
            Colormap::Grayscale => "Grayscale",
        }
    }

    /// Parse a [`Colormap::as_str`] token; unknown → [`Colormap::Heat`].
    pub fn from_token(s: &str) -> Colormap {
        Colormap::ALL
            .into_iter()
            .find(|c| c.as_str() == s)
            .unwrap_or(Colormap::Heat)
    }

    /// Gradient stops `(position, (r, g, b))`, ascending position over `[0,1]`.
    /// 5-stop perceptual approximations of the matplotlib maps (Turbo 6, grey 2).
    fn stops(&self) -> &'static [(f32, (f32, f32, f32))] {
        match self {
            Colormap::Heat => &[
                (0.00, (40.0, 60.0, 180.0)),
                (0.25, (40.0, 160.0, 200.0)),
                (0.50, (60.0, 200.0, 120.0)),
                (0.75, (230.0, 200.0, 60.0)),
                (1.00, (230.0, 70.0, 50.0)),
            ],
            Colormap::Inferno => &[
                (0.00, (0.0, 0.0, 4.0)),
                (0.25, (87.0, 16.0, 110.0)),
                (0.50, (188.0, 55.0, 84.0)),
                (0.75, (249.0, 142.0, 9.0)),
                (1.00, (252.0, 255.0, 164.0)),
            ],
            Colormap::Magma => &[
                (0.00, (0.0, 0.0, 4.0)),
                (0.25, (81.0, 18.0, 124.0)),
                (0.50, (183.0, 55.0, 121.0)),
                (0.75, (252.0, 137.0, 97.0)),
                (1.00, (252.0, 253.0, 191.0)),
            ],
            Colormap::Plasma => &[
                (0.00, (13.0, 8.0, 135.0)),
                (0.25, (126.0, 3.0, 168.0)),
                (0.50, (204.0, 71.0, 120.0)),
                (0.75, (248.0, 149.0, 64.0)),
                (1.00, (240.0, 249.0, 33.0)),
            ],
            Colormap::Viridis => &[
                (0.00, (68.0, 1.0, 84.0)),
                (0.25, (59.0, 82.0, 139.0)),
                (0.50, (33.0, 145.0, 140.0)),
                (0.75, (94.0, 201.0, 98.0)),
                (1.00, (253.0, 231.0, 37.0)),
            ],
            Colormap::Turbo => &[
                (0.00, (48.0, 18.0, 59.0)),
                (0.20, (52.0, 110.0, 235.0)),
                (0.40, (30.0, 200.0, 180.0)),
                (0.60, (150.0, 230.0, 60.0)),
                (0.80, (250.0, 165.0, 40.0)),
                (1.00, (220.0, 40.0, 30.0)),
            ],
            Colormap::Grayscale => &[
                (0.00, (28.0, 28.0, 28.0)),
                (1.00, (245.0, 245.0, 245.0)),
            ],
        }
    }

    /// Sample the gradient at `t ∈ [0,1]` → 8-bit RGB (linear interpolation
    /// between the bracketing stops).
    #[inline]
    pub fn sample(&self, t: f32) -> (u8, u8, u8) {
        let stops = self.stops();
        let t = t.clamp(0.0, 1.0);
        let mut i = 0;
        while i + 1 < stops.len() && t > stops[i + 1].0 {
            i += 1;
        }
        let (t0, c0) = stops[i];
        let (t1, c1) = stops[(i + 1).min(stops.len() - 1)];
        let f = if t1 > t0 { (t - t0) / (t1 - t0) } else { 0.0 };
        let lerp = |a: f32, b: f32| (a + (b - a) * f).round().clamp(0.0, 255.0) as u8;
        (lerp(c0.0, c1.0), lerp(c0.1, c1.1), lerp(c0.2, c1.2))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct HeatmapSettings {
    /// Cells whose book size (coin units) is below this aren't drawn at all.
    pub color_lo: f64,
    /// Book size (coin units) mapped to the top of the colour ramp. Fixed (no
    /// auto p99 fit), so the colours never breathe as you pan or zoom.
    pub color_peak: f64,
    /// Ramp alpha at full intensity (0..1). Caps how opaque the hottest cell
    /// gets so candles stay readable on top.
    pub max_opacity: f32,
    /// Colour gradient applied to the normalised heat value.
    pub colormap: Colormap,
    /// Draw the per-cell book size as centred text when cells are large enough.
    /// When off, the logical-cell value table is never retained on rebuild.
    pub show_text: bool,
    /// Stretch the latest (live) candle's column to the right edge of the
    /// canvas, so the current book reads as "now" out to the empty future area.
    /// When off, the rightmost column stops at its own candle slot.
    pub extend_right: bool,
}

impl Default for HeatmapSettings {
    fn default() -> Self {
        Self {
            color_lo: DEFAULT_COLOR_LO,
            color_peak: DEFAULT_COLOR_PEAK,
            max_opacity: 0.85,
            colormap: Colormap::Heat,
            show_text: true,
            extend_right: true,
        }
    }
}

/// One forward-filled book sample's per-bucket liquidity, valid for the time
/// span `[t0, t1)`. Retained on rebuild (when small enough — see
/// [`TEXT_MAX_SAMPLES`]) so the paint pass can label cells without re-reading
/// the service series.
#[derive(Clone)]
pub struct HeatmapSample {
    pub t0: i64,
    pub t1: i64,
    /// `sums[k]` = total resting size in bucket `k` (bucket 0 = lowest price).
    pub sums: Box<[f32]>,
}

/// Compact logical-cell value table for in-cell text. Bucket `k` spans price
/// `[price_lo + k·bucket, price_lo + (k+1)·bucket)`. `lo` / `log_lo` / `log_span`
/// mirror the texture build's colour range so the text path hides the same
/// below-low cells and picks a contrast colour matching the painted ramp.
pub struct HeatmapValues {
    pub bucket: f64,
    pub price_lo: f64,
    pub n_buckets: usize,
    pub lo: f64,
    pub log_lo: f32,
    pub log_span: f32,
    /// Ramp alpha at full intensity — mirrors `settings.max_opacity` so the
    /// crisp-cell painter matches the blit's opacity.
    pub max_opacity: f32,
    /// Colour gradient — mirrors `settings.colormap` so the crisp-cell + text
    /// paths sample the same ramp the texture was colorized with.
    pub colormap: Colormap,
    /// Whether to draw the in-cell numeric text. The value table is now retained
    /// for crisp-cell rendering regardless of this, so the text path gates on it.
    pub show_text: bool,
    /// Mirrors `settings.extend_right` — the crisp-cell painter stretches the
    /// last (live) cell to the right edge to match the texture blit.
    pub extend_right: bool,
    pub samples: Vec<HeatmapSample>,
}

/// A built texture plus the data-space rectangle it covers, the inputs it was
/// built from (the rebuild key), and the state an incremental patch needs to
/// recompute only the live column without a full rebuild.
struct HeatmapCache {
    image: Arc<RenderImage>,
    lo_ms: i64,
    hi_ms: i64,
    /// The **held** lazy-y price band: the visible range padded by
    /// [`Y_BAND_PAD_FRAC`], snapped to bucket boundaries. Also the y-rect the
    /// texture covers. Held constant across patches so the texture's row count
    /// stays fixed and trailing columns can be overwritten in place; the reuse
    /// test in [`HeatmapLayer::refresh`] full-rebuilds when the visible range
    /// exits this band.
    price_lo: f64,
    price_hi: f64,
    /// Bucket count / texel rows for the held band. With the band fixed these
    /// are invariant across patches, so the persistent `grid`/`buf` scratch
    /// stays correctly sized between rebuilds.
    n_buckets: usize,
    rows: usize,
    /// Texel-grid width (time resolution). Part of the rebuild key.
    cols: usize,
    /// Timeframe (column width) + candle-boundary phase this texture is bucketed
    /// to. Both are part of the rebuild key — a TF switch must full-rebuild.
    tf_ms: i64,
    phase: i64,
    settings: HeatmapSettings,
    /// Colour-ramp params derived from `settings` at build time, cached so a
    /// patch reproduces byte-identical colours for the trailing columns.
    lo: f64,
    log_lo: f32,
    log_span: f32,
    /// Patch cursor: the candle currently drawn extend-right (the live one) and
    /// the newest sample ts already built into the texture. A patch recomputes
    /// from `live_candle` to the new live candle; `last_ts` gates the no-op.
    live_candle: i64,
    last_ts: i64,
    /// Whether the value table (text / crisp quads) is retained for this build —
    /// drives whether a patch maintains `scratch.samples`.
    want_values: bool,
    values: Option<Arc<HeatmapValues>>,
}

/// Persistent scratch reused across rebuilds and patches so a per-tick rebuild
/// doesn't re-allocate (and re-zero) ~16 MB of `grid` + `buf`. Critically,
/// `grid`/`buf` retain the *previous* build's full content between refreshes —
/// an incremental patch overwrites only the trailing (live-candle) columns and
/// leaves the frozen columns byte-intact. `samples` is the value-table source of
/// truth (cloned into the `Arc<HeatmapValues>` snapshot the paint pass reads).
#[derive(Default)]
struct HeatmapScratch {
    /// `grid[col * rows + row]` — column-major per-bucket values.
    grid: Vec<f32>,
    /// Row-major BGRA, the `RenderImage` layout (`(row * cols + col) * 4`).
    buf: Vec<u8>,
    /// Pixel-row → bucket index (row 0 = top = highest price).
    row_bucket: Vec<usize>,
    snap_sums: Vec<f32>,
    cand_sums: Vec<f32>,
    col_px: Vec<f32>,
    samples: Vec<HeatmapSample>,
}

/// The heatmap render layer owned by `ChartState`. Holds the toggle, the
/// settings, the cached texture, and the reusable build scratch.
pub struct HeatmapLayer {
    pub enabled: bool,
    pub settings: HeatmapSettings,
    cache: Option<HeatmapCache>,
    /// Wall-clock ms of the last build *or* patch — gates the full-rebuild rate.
    last_build_ms: i64,
    /// Wall-clock ms of the last *full* rebuild — drives the [`SELF_HEAL_MS`]
    /// floor that periodically forces a full rebuild even when patches suffice.
    last_full_build_ms: i64,
    scratch: HeatmapScratch,
}

impl Default for HeatmapLayer {
    fn default() -> Self {
        Self {
            enabled: false,
            settings: HeatmapSettings::default(),
            cache: None,
            last_build_ms: 0,
            last_full_build_ms: 0,
            scratch: HeatmapScratch::default(),
        }
    }
}

impl HeatmapLayer {
    /// Rebuild the texture if needed for the given visible time + price window.
    /// `series` is the book time-series (oldest-first) from the market-data
    /// service; `(vis_lo_ms, vis_hi_ms)` is the visible time range and
    /// `(y_lo, y_hi)` the visible price range; `canvas_w` sizes the texel grid's
    /// width. The texture is **lazy on both axes**: it covers only the visible
    /// time window (+ [`MARGIN_FRAC`]) and the visible price band
    /// (+ [`Y_BAND_PAD_FRAC`]), so off-screen liquidity is clipped and the
    /// on-screen band renders crisp at ~1 texel/bucket. A small pan/zoom within
    /// the held pads is absorbed by re-stretching the cached texture at paint
    /// time; only when the view exits a pad — or the data/settings/dims change —
    /// do we rebuild.
    ///
    /// Cheap no-op when the cache still covers the view with the same
    /// data/settings/dims. On an actual rebuild the previous atlas tile is
    /// dropped so tiles don't leak.
    ///
    /// `tf_ms` is the selected timeframe — each heatmap column spans exactly one
    /// candle of that width; `anchor_ms` is any candle's open time, used to align
    /// the column boundaries to the candle grid.
    #[allow(clippy::too_many_arguments)]
    pub fn refresh(
        &mut self,
        series: &[BookSnapshotEntry],
        vis_lo_ms: i64,
        vis_hi_ms: i64,
        y_lo: f64,
        y_hi: f64,
        canvas_w: f32,
        tf_ms: i64,
        anchor_ms: i64,
        now_ms: i64,
        window: &mut Window,
    ) {
        if !self.enabled || series.is_empty() || vis_hi_ms <= vis_lo_ms || tf_ms <= 0 || y_hi <= y_lo
        {
            self.drop_cache(window);
            return;
        }

        // Build at candle resolution (~CANDLE_TEXELS texels per visible candle),
        // capped at screen width — the heatmap is per-candle, so full-pixel width
        // is mostly redundant texels. `vis_candles` is constant while the zoom is
        // fixed (a candle close shifts both edges equally), so `cols` stays stable
        // tick-to-tick and the patch path keeps applying; it only changes on a
        // zoom/resize, which full-rebuilds anyway.
        let cols_px = (canvas_w.round() as usize).clamp(MIN_TEXELS, MAX_TEXELS);
        let vis_candles = (((vis_hi_ms - vis_lo_ms) / tf_ms).max(1)) as usize;
        let cols = cols_px.min(vis_candles.saturating_mul(CANDLE_TEXELS).max(MIN_TEXELS));
        // Candle-boundary phase: anchors are TF-aligned, so this is invariant for
        // a (symbol, TF) and changes only on a TF switch.
        let phase = anchor_ms.rem_euclid(tf_ms);
        let newest_ts = series.last().map(|s| s.ts_ms).unwrap_or(0);

        // Classify this refresh against the cache: nothing changed (no-op), only
        // the live tail grew (cheap patch), or the view exited a held pad / the
        // data/settings/dims changed (full rebuild). Reuse requires the cache to
        // still cover the visible time window *and* the visible price band
        // (`price_lo/price_hi` is the held lazy-y band); the `Y_SHRINK` test also
        // forces a rebuild when the view zoomed in far enough that the held band
        // would render the slice too coarse. The pads absorb ordinary pan +
        // auto-fit-Y wiggle so those stay paint-time re-stretches.
        enum Action {
            Noop,
            Patch,
            Full,
        }
        let action = match &self.cache {
            Some(c)
                if c.lo_ms <= vis_lo_ms
                    && c.hi_ms >= vis_hi_ms
                    && c.price_lo <= y_lo
                    && c.price_hi >= y_hi
                    && (y_hi - y_lo) >= (c.price_hi - c.price_lo) * Y_SHRINK_REBUILD_FRAC
                    && c.cols == cols
                    && c.tf_ms == tf_ms
                    && c.phase == phase
                    && c.settings == self.settings =>
            {
                if c.last_ts == newest_ts {
                    Action::Noop
                } else if patchable(series, c, tf_ms, anchor_ms, now_ms, self.last_full_build_ms) {
                    Action::Patch
                } else {
                    Action::Full
                }
            }
            _ => Action::Full,
        };

        match action {
            Action::Noop => {}
            Action::Patch => {
                let patched = {
                    let cache = self.cache.as_ref().expect("patch implies a cache");
                    try_patch(series, cache, tf_ms, anchor_ms, &mut self.scratch)
                };
                if let Some(nc) = patched {
                    if let Some(oc) = self.cache.take() {
                        let _ = window.drop_image(oc.image);
                    }
                    self.cache = Some(nc);
                    self.last_build_ms = now_ms;
                } else {
                    // Patch declined mid-build (empty trailing window / dims
                    // mismatch) — fall through to a full rebuild this tick.
                    self.full_rebuild(series, vis_lo_ms, vis_hi_ms, y_lo, y_hi, cols, tf_ms, anchor_ms, phase, now_ms, window);
                }
            }
            Action::Full => {
                // Throttle full rebuilds: a continuous pan/zoom would otherwise
                // want one every render. The 50 ms tick loop guarantees a trailing
                // render, so a throttle-skipped rebuild lands within one interval;
                // meanwhile the paint pass re-stretches the stale bitmap so the
                // pan stays smooth. (Patches are exempt — they're cheap and only
                // fire when the data tick advances `newest_ts`.)
                if self.cache.is_some() && now_ms - self.last_build_ms < MIN_REBUILD_INTERVAL_MS {
                    return;
                }
                self.full_rebuild(series, vis_lo_ms, vis_hi_ms, y_lo, y_hi, cols, tf_ms, anchor_ms, phase, now_ms, window);
            }
        }
    }

    /// From-scratch rebuild for the visible window, extended past the edges by
    /// [`MARGIN_FRAC`] so a few horizontal pans reuse the texture. Drops the old
    /// atlas tile and resets the self-heal clock.
    #[allow(clippy::too_many_arguments)]
    fn full_rebuild(
        &mut self,
        series: &[BookSnapshotEntry],
        vis_lo_ms: i64,
        vis_hi_ms: i64,
        y_lo: f64,
        y_hi: f64,
        cols: usize,
        tf_ms: i64,
        anchor_ms: i64,
        phase: i64,
        now_ms: i64,
        window: &mut Window,
    ) {
        let span_ms = (vis_hi_ms - vis_lo_ms) as f64;
        let m_ms = (span_ms * MARGIN_FRAC) as i64;
        let lo_ms = vis_lo_ms - m_ms;
        let hi_ms = vis_hi_ms + m_ms;

        let built = build_full(
            series, lo_ms, hi_ms, y_lo, y_hi, cols, tf_ms, anchor_ms, phase, &self.settings,
            &mut self.scratch,
        );

        self.last_build_ms = now_ms;
        self.last_full_build_ms = now_ms;
        if let Some(oc) = self.cache.take() {
            let _ = window.drop_image(oc.image);
        }
        self.cache = built;
    }

    /// Release the cached texture's atlas tile and clear the cache.
    pub fn drop_cache(&mut self, window: &mut Window) {
        if let Some(c) = self.cache.take() {
            let _ = window.drop_image(c.image);
        }
    }

    /// The cached texture plus the data-space rectangle it covers, for the
    /// paint pass. `None` when disabled or nothing is built. Cheap — only
    /// `Arc` clones — so it can be captured into the (move) canvas paint closure.
    pub fn paint_rect(&self) -> Option<HeatmapRect> {
        if !self.enabled {
            return None;
        }
        let c = self.cache.as_ref()?;
        Some(HeatmapRect {
            image: c.image.clone(),
            lo_ms: c.lo_ms,
            hi_ms: c.hi_ms,
            price_lo: c.price_lo,
            price_hi: c.price_hi,
            values: c.values.clone(),
            // The orderbook heatmap has no profile layer.
            profile: None,
        })
    }
}

/// A built heatmap texture and the data-space rect it covers. Captured into the
/// chart's paint closure; [`paint_heatmap`] maps the rect onto the live view.
#[derive(Clone)]
pub struct HeatmapRect {
    pub image: Arc<RenderImage>,
    pub lo_ms: i64,
    pub hi_ms: i64,
    pub price_lo: f64,
    pub price_hi: f64,
    pub values: Option<Arc<HeatmapValues>>,
    /// Optional right-anchored price profile painted *in front* of candles (via
    /// [`paint_heatmap_profile`]). Only the liquidation heatmap populates it
    /// (the orderbook heatmap leaves it `None`).
    pub profile: Option<Arc<HeatmapProfile>>,
}

/// Per-price-bucket magnitude for the right-anchored **profile** — a horizontal
/// histogram drawn on the price axis (bucket `k` spans price
/// `[price_lo + k·bucket, … + bucket)`). The liquidation heatmap is its only
/// producer: each bucket holds the magnitude of the **current** (most recent)
/// sim column at that price — the live magnet snapshot, the same values
/// `extend_right` projects to the right edge — *not* an aggregate over history.
/// Because the values are exactly the heatmap's rightmost cells, bar length +
/// colour (both via the shared log ramp `lo` / `log_lo` / `log_span`) match
/// those cells exactly.
pub struct HeatmapProfile {
    pub bucket: f64,
    pub price_lo: f64,
    pub n_buckets: usize,
    /// `values[k]` = the current sim column's magnitude at bucket `k`.
    pub values: Vec<f32>,
    pub lo: f64,
    pub log_lo: f32,
    pub log_span: f32,
    pub max_opacity: f32,
    pub colormap: Colormap,
}

/// Paint the heatmap behind the candles. When the value table is present and
/// cells are tall enough on screen, the visible cells are drawn as **solid
/// quads** (hard edges — see [`paint_heatmap_cells`]); otherwise the cached
/// texture is blitted (its data-rect mapped onto the current view transform, so
/// pans/zooms between throttled rebuilds stay smooth). When `show_text` is on
/// and cells are large enough, overlays the per-cell value text on top.
#[allow(clippy::too_many_arguments)]
pub fn paint_heatmap(
    rect: &HeatmapRect,
    origin: Point<Pixels>,
    candles: &[Candle],
    start_idx: usize,
    tf_ms: i64,
    view_start: f32,
    view_size: f32,
    canvas_w: f32,
    y_axis_gap: f32,
    y_lo: f64,
    y_hi: f64,
    canvas_h: f32,
    volume_unit: VolumeUnit,
    window: &mut Window,
    cx: &mut App,
) {
    // Data-rect corners → fractional candle index → screen x. `candles` is the
    // visible slice that starts at absolute index `start_idx`, so `time_to_idx`
    // (slice-relative) is offset back into the absolute index space that
    // `index_to_screen` (and the candle paint) use.
    let x_left = index_to_screen(
        view_start,
        view_size,
        start_idx as f32 + time_to_idx(rect.lo_ms, candles, tf_ms),
        canvas_w,
        y_axis_gap,
    );
    let x_right = index_to_screen(
        view_start,
        view_size,
        start_idx as f32 + time_to_idx(rect.hi_ms, candles, tf_ms),
        canvas_w,
        y_axis_gap,
    );
    // Higher price → smaller y (top of the canvas).
    let y_top = price_to_screen(y_lo, y_hi, rect.price_hi, canvas_h);
    let y_bottom = price_to_screen(y_lo, y_hi, rect.price_lo, canvas_h);

    let w = x_right - x_left;
    let h = y_bottom - y_top;
    if w <= 0.0 || h <= 0.0 {
        return;
    }

    // Uniform on-screen cell height (every bucket is `bucket` dollars). When
    // cells are tall enough and we have the value table, draw crisp per-cell
    // quads; otherwise blit the (bilinear-stretched) texture.
    let price_span = y_hi - y_lo;
    let cell_h = if price_span > 0.0 {
        (canvas_h as f64 * rect.values.as_ref().map_or(0.0, |v| v.bucket) / price_span) as f32
    } else {
        0.0
    };
    let crisp = match &rect.values {
        // Hard-edged quads only when cells are tall enough for the seams to read
        // *and* the visible cell count keeps the per-frame quad work bounded.
        // With the bilinear blit now crisp (integer texels/bucket), the blit is
        // a seamless fallback, so capping the quad path here costs no visible
        // quality — it just stops a medium zoom (hundreds of visible buckets ×
        // candles) from emitting tens of thousands of `paint_quad`s every frame,
        // which is what dropped the frame rate on each heatmap update as the
        // auto-fitting y axis nudged `cell_h` across the threshold.
        Some(values) if cell_h >= MIN_CELL_H_FOR_CRISP => {
            let (k_lo, k_hi) = visible_bucket_range(values, y_lo, y_hi);
            k_hi.saturating_sub(k_lo).saturating_mul(values.samples.len()) <= MAX_CRISP_QUADS
        }
        _ => false,
    };

    if let (true, Some(values)) = (crisp, &rect.values) {
        paint_heatmap_cells(
            values, origin, candles, start_idx, tf_ms, view_start, view_size, canvas_w, y_axis_gap,
            y_lo, y_hi, canvas_h, window,
        );
    } else {
        let bounds = Bounds {
            origin: point(px(x_left) + origin.x, px(y_top) + origin.y),
            size: size(px(w), px(h)),
        };
        let _ = window.paint_image(bounds, Corners::default(), rect.image.clone(), 0, false);
    }

    if let Some(values) = &rect.values {
        if values.show_text {
            paint_heatmap_text(
                values, origin, candles, start_idx, tf_ms, view_start, view_size, canvas_w,
                y_axis_gap, y_lo, y_hi, canvas_h, volume_unit, window, cx,
            );
        }
    }
}

/// Paint the right-anchored magnet **profile**: one horizontal bar per visible
/// price bucket, length + colour following the same log colour ramp as the
/// heatmap (so the profile reads as a price-axis projection of the heat). Drawn
/// *in front* of candles (called after the main chart), anchored to the **right
/// edge of the plot area** — `canvas_w − y_axis_gap`, so it doesn't overlap the
/// price axis (matching VRVP's `chart_left + chart_w` anchor) — and growing
/// leftward. `width_frac` is the peak bar's length as a fraction of the plot
/// width. Below-`lo` buckets are skipped. Pure paint-time mapping over the
/// pre-aggregated [`HeatmapProfile`] — no aggregation here.
#[allow(clippy::too_many_arguments)]
pub fn paint_heatmap_profile(
    profile: &HeatmapProfile,
    origin: Point<Pixels>,
    canvas_w: f32,
    y_axis_gap: f32,
    width_frac: f32,
    y_lo: f64,
    y_hi: f64,
    canvas_h: f32,
    window: &mut Window,
) {
    if profile.bucket <= 0.0 || profile.n_buckets == 0 || y_hi <= y_lo || canvas_w <= 0.0 {
        return;
    }
    // Anchor at the right edge of the *plot* area (price axis excluded), so the
    // profile never paints over the y-axis labels.
    let anchor_x = (canvas_w - y_axis_gap).max(1.0);
    let band_w = (anchor_x * width_frac.clamp(0.0, 1.0)).max(1.0);
    let lo_f = profile.lo as f32;

    // Only buckets whose price band overlaps the visible range.
    let k_lo = (((y_lo - profile.price_lo) / profile.bucket).floor().max(0.0) as usize)
        .min(profile.n_buckets);
    let k_hi = (((y_hi - profile.price_lo) / profile.bucket).ceil() + 1.0)
        .max(0.0) as usize;
    let k_hi = k_hi.min(profile.n_buckets);

    for k in k_lo..k_hi {
        let v = profile.values[k];
        if v <= 0.0 || (v as f64) < profile.lo || v < lo_f {
            continue;
        }
        let bp_lo = profile.price_lo + k as f64 * profile.bucket;
        let bp_hi = bp_lo + profile.bucket;
        let y_top = price_to_screen(y_lo, y_hi, bp_hi, canvas_h);
        let y_bot = price_to_screen(y_lo, y_hi, bp_lo, canvas_h);
        let h = y_bot - y_top;
        if h <= 0.0 {
            continue;
        }
        // Log-normalised intensity drives both bar length and colour — same
        // mapping the texture/crisp-cell paths use, so length tracks heat.
        let norm = (((1.0 + v).ln() - profile.log_lo) / profile.log_span).clamp(0.0, 1.0);
        if norm <= 0.0 {
            continue;
        }
        let bar_w = (band_w * norm).max(1.0);
        let (r, g, bl) = profile.colormap.sample(norm);
        let a = (norm.powf(0.6) * profile.max_opacity).clamp(0.0, 1.0);
        if a <= 0.0 {
            continue;
        }
        let color: Hsla = Rgba {
            r: r as f32 / 255.0,
            g: g as f32 / 255.0,
            b: bl as f32 / 255.0,
            a,
        }
        .into();
        let bounds = Bounds {
            origin: point(px(anchor_x - bar_w) + origin.x, px(y_top) + origin.y),
            size: size(px(bar_w), px(h)),
        };
        window.paint_quad(PaintQuad {
            bounds,
            corner_radii: Corners::default(),
            background: color.into(),
            border_widths: Edges::default(),
            border_color: gpui::transparent_black(),
            border_style: BorderStyle::default(),
        });
    }
}

/// Paint each visible, lit cell as a solid quad with hard edges — the crisp
/// alternative to the bilinear texture blit. Cells tile exactly (a cell's edges
/// are shared with its neighbours), so equal-colour neighbours read seamless
/// while different-colour neighbours get a sharp boundary. Skips empty
/// (below-`lo`) and off-screen cells; the content mask clips the rest. Colour
/// matches the texture build (same log ramp + `max_opacity`).
#[allow(clippy::too_many_arguments)]
fn paint_heatmap_cells(
    values: &HeatmapValues,
    origin: Point<Pixels>,
    candles: &[Candle],
    start_idx: usize,
    tf_ms: i64,
    view_start: f32,
    view_size: f32,
    canvas_w: f32,
    y_axis_gap: f32,
    y_lo: f64,
    y_hi: f64,
    canvas_h: f32,
    window: &mut Window,
) {
    let screen_x = |t: i64| -> f32 {
        index_to_screen(
            view_start,
            view_size,
            start_idx as f32 + time_to_idx(t, candles, tf_ms),
            canvas_w,
            y_axis_gap,
        )
    };
    let lo_f = values.lo as f32;
    // Only the buckets whose price overlaps the visible band — the full-extent
    // table can carry far more buckets than are on screen.
    let (k_lo, k_hi) = visible_bucket_range(values, y_lo, y_hi);
    let last_idx = values.samples.len().saturating_sub(1);
    for (si, s) in values.samples.iter().enumerate() {
        let x0 = screen_x(s.t0);
        // The last (live) cell extends to the right edge when `extend_right`,
        // matching the texture blit's `extend_right` fill.
        let x1 = if si == last_idx && values.extend_right {
            screen_x(s.t1).max(canvas_w)
        } else {
            screen_x(s.t1)
        };
        if x1 <= 0.0 || x0 >= canvas_w || x1 - x0 <= 0.0 {
            continue;
        }
        for k in k_lo..k_hi {
            let v = s.sums[k];
            if v <= 0.0 || (v as f64) < values.lo || v < lo_f {
                continue;
            }
            let bp_lo = values.price_lo + k as f64 * values.bucket;
            let bp_hi = bp_lo + values.bucket;
            let y_top = price_to_screen(y_lo, y_hi, bp_hi, canvas_h);
            let y_bot = price_to_screen(y_lo, y_hi, bp_lo, canvas_h);
            let norm = (((1.0 + v).ln() - values.log_lo) / values.log_span).clamp(0.0, 1.0);
            let (r, g, bl) = values.colormap.sample(norm);
            let a = (norm.powf(0.6) * values.max_opacity).clamp(0.0, 1.0);
            if a <= 0.0 {
                continue;
            }
            let color: Hsla = Rgba {
                r: r as f32 / 255.0,
                g: g as f32 / 255.0,
                b: bl as f32 / 255.0,
                a,
            }
            .into();
            let bounds = Bounds {
                origin: point(px(x0) + origin.x, px(y_top) + origin.y),
                size: size(px(x1 - x0), px(y_bot - y_top)),
            };
            window.paint_quad(PaintQuad {
                bounds,
                corner_radii: Corners::default(),
                background: color.into(),
                border_widths: Edges::default(),
                border_color: gpui::transparent_black(),
                border_style: BorderStyle::default(),
            });
        }
    }
}

/// Overlay each visible cell's book size as centred text, but only where the
/// on-screen cell is large enough to read. Skips empty cells and anything off
/// the visible band. Pure paint-time mapping over the retained value table —
/// no aggregation here.
/// `[k_lo, k_hi)` bucket index range whose price spans overlap the visible band
/// `[y_lo, y_hi]`, clamped to `[0, n_buckets]`. Lets the cell/text painters skip
/// the (potentially large) off-screen part of the full-extent table.
fn visible_bucket_range(values: &HeatmapValues, y_lo: f64, y_hi: f64) -> (usize, usize) {
    if values.bucket <= 0.0 || values.n_buckets == 0 {
        return (0, 0);
    }
    let k_lo = ((y_lo - values.price_lo) / values.bucket).floor();
    let k_hi = ((y_hi - values.price_lo) / values.bucket).ceil() + 1.0;
    let k_lo = (k_lo.max(0.0) as usize).min(values.n_buckets);
    let k_hi = (k_hi.max(0.0) as usize).min(values.n_buckets);
    (k_lo, k_hi)
}

#[allow(clippy::too_many_arguments)]
fn paint_heatmap_text(
    values: &HeatmapValues,
    origin: Point<Pixels>,
    candles: &[Candle],
    start_idx: usize,
    tf_ms: i64,
    view_start: f32,
    view_size: f32,
    canvas_w: f32,
    y_axis_gap: f32,
    y_lo: f64,
    y_hi: f64,
    canvas_h: f32,
    volume_unit: VolumeUnit,
    window: &mut Window,
    cx: &mut App,
) {
    let price_span = y_hi - y_lo;
    if price_span <= 0.0 {
        return;
    }
    // Cell height is uniform (every bucket is `bucket` dollars). One early-out
    // for the whole layer if rows are too short to label.
    let cell_h = (canvas_h as f64 * values.bucket / price_span) as f32;
    if cell_h < MIN_CELL_H_FOR_TEXT {
        return;
    }

    let screen_x = |t: i64| -> f32 {
        index_to_screen(
            view_start,
            view_size,
            start_idx as f32 + time_to_idx(t, candles, tf_ms),
            canvas_w,
            y_axis_gap,
        )
    };
    // Honour the "Round cell decimals" global, like footprint/Bar-Stats cells.
    let round = crate::prefs::round_cell_decimals();
    let (k_lo, k_hi) = visible_bucket_range(values, y_lo, y_hi);

    for s in &values.samples {
        let x0 = screen_x(s.t0);
        let x1 = screen_x(s.t1);
        let w = x1 - x0;
        if w < MIN_CELL_W_FOR_TEXT || x1 <= 0.0 || x0 >= canvas_w {
            continue;
        }
        for k in k_lo..k_hi {
            let v = s.sums[k];
            if v <= 0.0 || (v as f64) < values.lo {
                continue; // empty or below the low cut — not drawn, so no label
            }
            let bp_lo = values.price_lo + k as f64 * values.bucket;
            let bp_hi = bp_lo + values.bucket;
            let y_top = price_to_screen(y_lo, y_hi, bp_hi, canvas_h);
            let y_bot = price_to_screen(y_lo, y_hi, bp_lo, canvas_h);
            let h = y_bot - y_top;
            // Contrast: dark text on bright cells, light on dark — luminance of
            // the actual ramp colour, so it adapts to the selected colormap.
            let norm = (((1.0 + v).ln() - values.log_lo) / values.log_span).clamp(0.0, 1.0);
            let (cr, cg, cb) = values.colormap.sample(norm);
            let lum = 0.299 * cr as f32 + 0.587 * cg as f32 + 0.114 * cb as f32;
            let color = if lum > 140.0 {
                hsla(0.0, 0.0, 0.0, 0.95)
            } else {
                hsla(0.0, 0.0, 1.0, 0.95)
            };
            // Colour stays coin-normalized; only the displayed number follows
            // the Coin/USD toggle (USD = coin size × the bucket midpoint price).
            let display = match volume_unit {
                VolumeUnit::Coin => v as f64,
                VolumeUnit::Usd => v as f64 * (bp_lo + values.bucket * 0.5),
            };
            super::paint_centred_text(
                window,
                cx,
                origin,
                x0,
                w,
                y_top,
                h,
                color,
                &fmt_compact(display, round),
            );
        }
    }
}

/// Compact book-size label: `2.4B` / `15M` / `1.2k` / `340` / `4.5`. The wide
/// magnitude range covers both coin sizes and USD notionals. Keeps cells
/// uncluttered. When `round` (the "Round cell decimals" global) is on, drops
/// the fractional digit and rounds to whole numbers (suffix preserved), matching
/// footprint cells and Bar Stats.
fn fmt_compact(v: f64, round: bool) -> String {
    let a = v.abs();
    if round {
        return if a >= 1e9 {
            format!("{:.0}B", (v / 1e9).round())
        } else if a >= 1e6 {
            format!("{:.0}M", (v / 1e6).round())
        } else if a >= 1000.0 {
            format!("{:.0}k", (v / 1000.0).round())
        } else {
            format!("{:.0}", v.round())
        };
    }
    if a >= 1e9 {
        format!("{:.1}B", v / 1e9)
    } else if a >= 1e6 {
        format!("{:.1}M", v / 1e6)
    } else if a >= 1000.0 {
        format!("{:.1}k", v / 1000.0)
    } else if a >= 10.0 {
        format!("{:.0}", v)
    } else {
        format!("{:.1}", v)
    }
}

/// One snapshot's per-bucket resting size within the held band, written into
/// `out` (length `n_buckets`). `bids`/`asks` are best-first, so we skip the head
/// above/below the band and break once past it — bounding work to the band
/// instead of the full (potentially thousands-deep) book.
fn bucket_snapshot(snap: &BookSnapshotEntry, pl: f64, ph: f64, b: f64, n_buckets: usize, out: &mut [f32]) {
    for s in out.iter_mut() {
        *s = 0.0;
    }
    for lvl in &snap.bids {
        if lvl.price >= ph {
            continue;
        }
        if lvl.price < pl {
            break;
        }
        if lvl.size > 0.0 {
            let k = (((lvl.price - pl) / b).floor() as usize).min(n_buckets - 1);
            out[k] += lvl.size as f32;
        }
    }
    for lvl in &snap.asks {
        if lvl.price < pl {
            continue;
        }
        if lvl.price >= ph {
            break;
        }
        if lvl.size > 0.0 {
            let k = (((lvl.price - pl) / b).floor() as usize).min(n_buckets - 1);
            out[k] += lvl.size as f32;
        }
    }
}

/// Project one candle's per-bucket sums onto its pixel rows (via `row_bucket`)
/// and write them into `grid` columns `[c0, c1)` with a max, so adjacent slot
/// rounding never lowers a value.
fn flush_candle(
    cand_sums: &[f32],
    row_bucket: &[usize],
    rows: usize,
    c0: usize,
    c1: usize,
    grid: &mut [f32],
    col_px: &mut [f32],
) {
    for (r, px_val) in col_px.iter_mut().enumerate() {
        *px_val = cand_sums[row_bucket[r]];
    }
    for col in c0..c1 {
        let base = col * rows;
        for (r, &px_val) in col_px.iter().enumerate() {
            if px_val > grid[base + r] {
                grid[base + r] = px_val;
            }
        }
    }
}

/// Flush one accumulated candle into its column block, mapping its candle-grid
/// slot to texel columns. `is_last` marks the live (extend-right) candle, which
/// stretches to the right edge. Appends the value-table sample when retained.
///
/// `pub(super)` so the sibling liquidation-heatmap layer (`paint/liq_heatmap.rs`)
/// reuses the identical candle→column projection — its sim grid is fed through
/// the same primitive, guaranteeing byte-identical column placement.
#[allow(clippy::too_many_arguments)]
pub(super) fn flush_one(
    cand_start: i64,
    is_last: bool,
    half: i64,
    tail: i64,
    lo_ms: i64,
    span_ms: f64,
    cols: usize,
    rows: usize,
    extend_right: bool,
    cand_sums: &[f32],
    row_bucket: &[usize],
    grid: &mut [f32],
    col_px: &mut [f32],
    want_values: bool,
    samples: &mut Vec<HeatmapSample>,
) {
    let col_of = |t: i64| -> i64 { (((t - lo_ms) as f64 / span_ms) * cols as f64) as i64 };
    let slot_lo = cand_start - half;
    let slot_hi = cand_start + tail;
    let c0 = col_of(slot_lo).clamp(0, cols as i64 - 1);
    let c1 = if is_last && extend_right {
        cols as i64
    } else {
        col_of(slot_hi).clamp(c0 + 1, cols as i64)
    };
    let (c0, c1) = (c0 as usize, c1 as usize);
    flush_candle(cand_sums, row_bucket, rows, c0, c1, grid, col_px);
    if want_values {
        samples.push(HeatmapSample {
            t0: slot_lo,
            t1: slot_hi,
            sums: cand_sums.to_vec().into_boxed_slice(),
        });
    }
}

/// Colour `grid` columns `[c_lo, c_hi)` into the row-major BGRA `buf`, writing
/// *every* cell in the range (lit → ramp colour, unlit → transparent) so an
/// incremental patch correctly clears cells that went dark. Returns whether any
/// cell in the range was lit. Each candle fills a run of identical columns, so
/// the colour maths (`ln` + `powf` + ramp — the bulk of the per-rebuild CPU
/// cost) is computed once per distinct column and the rest are a byte copy of
/// the left neighbour.
// `pub(super)` so the liquidation-heatmap layer shares the exact log-ramp +
// `max_opacity` mapping, keeping its texture colours consistent with the
// crisp-cell / text painters (which recompute `norm` from `log_lo`/`log_span`).
#[allow(clippy::too_many_arguments)]
pub(super) fn colorize_range(
    grid: &[f32],
    cols: usize,
    rows: usize,
    lo_f: f32,
    log_lo: f32,
    log_span: f32,
    max_opacity: f32,
    colormap: Colormap,
    c_lo: usize,
    c_hi: usize,
    buf: &mut [u8],
) -> bool {
    let mut any = false;
    for col in c_lo..c_hi {
        let base = col * rows;
        if col > 0 && grid[base..base + rows] == grid[base - rows..base] {
            // Identical to the previous column — replicate its already-written
            // bytes (lit cells *and* the cleared unlit ones) instead of recomputing.
            for row in 0..rows {
                let src = (row * cols + (col - 1)) * 4;
                buf.copy_within(src..src + 4, (row * cols + col) * 4);
            }
            continue;
        }
        for row in 0..rows {
            let idx = (row * cols + col) * 4;
            let v = grid[base + row];
            if v <= 0.0 || v < lo_f {
                buf[idx..idx + 4].fill(0); // clear (patch may overwrite a lit cell)
                continue;
            }
            let norm = (((1.0 + v).ln() - log_lo) / log_span).clamp(0.0, 1.0);
            let a = (norm.powf(0.6) * max_opacity * 255.0).round() as u8;
            if a == 0 {
                buf[idx..idx + 4].fill(0);
                continue;
            }
            let (r, g, bl) = colormap.sample(norm);
            any = true;
            // image::RgbaImage is row-major: pixel (x=col, y=row), BGRA order.
            buf[idx] = bl;
            buf[idx + 1] = g;
            buf[idx + 2] = r;
            buf[idx + 3] = a;
        }
    }
    any
}

/// Wrap the retained logical-cell `samples` into the immutable value table the
/// paint pass reads for crisp quads / in-cell text. `None` when the table isn't
/// retained (too many buckets/candles) or nothing is lit.
// `pub(super)` so the liquidation-heatmap layer wraps its sim-grid samples into
// the same immutable value table the shared paint pass reads.
#[allow(clippy::too_many_arguments)]
pub(super) fn build_values(
    want_values: bool,
    samples: &[HeatmapSample],
    b: f64,
    pl: f64,
    n_buckets: usize,
    lo: f64,
    log_lo: f32,
    log_span: f32,
    settings: &HeatmapSettings,
) -> Option<Arc<HeatmapValues>> {
    if want_values && !samples.is_empty() {
        Some(Arc::new(HeatmapValues {
            bucket: b,
            price_lo: pl,
            n_buckets,
            lo,
            log_lo,
            log_span,
            max_opacity: settings.max_opacity,
            colormap: settings.colormap,
            show_text: settings.show_text,
            extend_right: settings.extend_right,
            samples: samples.to_vec(),
        }))
    } else {
        None
    }
}

/// Whether the next refresh can be served by a cheap incremental patch instead
/// of a full rebuild: the live tail grew, only a few candles closed, and we're
/// within the [`SELF_HEAL_MS`] floor. The held band's *price* validity is checked
/// in [`HeatmapLayer::refresh`] (the Y-coverage test) — with the lazy-y band,
/// off-screen liquidity is clipped on purpose, so there's no far-data band check
/// here (the patch recomputes the live column within the held band exactly as a
/// full build would).
fn patchable(
    series: &[BookSnapshotEntry],
    cache: &HeatmapCache,
    tf_ms: i64,
    anchor_ms: i64,
    now_ms: i64,
    last_full_build_ms: i64,
) -> bool {
    if now_ms - last_full_build_ms >= SELF_HEAL_MS {
        return false; // periodic self-heal — force a full rebuild
    }
    let Some(last) = series.last() else {
        return false;
    };
    let candle_start = |t: i64| -> i64 { anchor_ms + (t - anchor_ms).div_euclid(tf_ms) * tf_ms };
    let new_live = candle_start(last.ts_ms);
    if (new_live - cache.live_candle) / tf_ms > MAX_PATCH_CANDLES {
        return false; // too much closed at once — a full rebuild is cheaper
    }
    true
}

/// Incrementally rebuild only the trailing (live-candle) columns of the cached
/// texture, reusing the held band and the persistent `grid`/`buf` scratch (whose
/// frozen columns still hold the previous build). Recomputes from the previous
/// live candle to the current one, recolours that column range, and wraps the
/// result as a fresh `RenderImage` (gpui has no partial upload — but the upload
/// is cheap; the win is skipping the O(cols·rows) CPU rebuild of frozen
/// columns). Returns `None` to defer to a full rebuild.
fn try_patch(
    series: &[BookSnapshotEntry],
    cache: &HeatmapCache,
    tf_ms: i64,
    anchor_ms: i64,
    scratch: &mut HeatmapScratch,
) -> Option<HeatmapCache> {
    let pl = cache.price_lo;
    let ph = cache.price_hi;
    let b = PRICE_BUCKET;
    let n_buckets = cache.n_buckets;
    let rows = cache.rows;
    let cols = cache.cols;
    let lo_ms = cache.lo_ms;
    let hi_ms = cache.hi_ms;
    let span_ms = (hi_ms - lo_ms) as f64;
    if span_ms <= 0.0 {
        return None;
    }
    let lo = cache.lo;
    let lo_f = lo as f32;
    let log_lo = cache.log_lo;
    let log_span = cache.log_span;
    let settings = cache.settings;
    let extend_right = settings.extend_right;
    let want_values = cache.want_values;

    // The persistent scratch must still match the held dims (it does — the band
    // is held and `cols` is part of the reuse key — but verify defensively;
    // a mismatch falls back to a full rebuild).
    if scratch.grid.len() != cols * rows
        || scratch.buf.len() != cols * rows * 4
        || scratch.row_bucket.len() != rows
        || scratch.snap_sums.len() != n_buckets
        || scratch.cand_sums.len() != n_buckets
        || scratch.col_px.len() != rows
    {
        return None;
    }

    let candle_start = |t: i64| -> i64 { anchor_ms + (t - anchor_ms).div_euclid(tf_ms) * tf_ms };
    let col_of = |t: i64| -> i64 { (((t - lo_ms) as f64 / span_ms) * cols as f64) as i64 };
    let half = tf_ms / 2;
    let tail = tf_ms - half;

    let last = series.len().checked_sub(1)?;
    let newest_ts = series[last].ts_ms;
    let new_live = candle_start(newest_ts);
    let old_live = cache.live_candle;

    // Recompute from the previous live candle's slot start to the right edge:
    // it may now be a closed (bounded) candle, and the slots from it rightward
    // (its extend-right tail last build) all need re-laying.
    let c_start = col_of(old_live - half).clamp(0, cols as i64 - 1) as usize;
    let agg_start = series.partition_point(|s| candle_start(s.ts_ms) < old_live);
    if agg_start >= series.len() {
        return None;
    }

    let HeatmapScratch {
        grid,
        buf,
        row_bucket,
        snap_sums,
        cand_sums,
        col_px,
        samples,
    } = scratch;

    // Zero the trailing grid range; frozen columns `[0, c_start)` stay intact.
    for v in &mut grid[c_start * rows..cols * rows] {
        *v = 0.0;
    }
    // Drop value-table samples for the candles we're about to recompute.
    if want_values {
        let cut = old_live - half; // == slot_lo(old_live)
        let keep = samples.partition_point(|s| s.t0 < cut);
        samples.truncate(keep);
    }

    let mut cur_candle: Option<i64> = None;
    for snap in &series[agg_start..] {
        let cs = candle_start(snap.ts_ms);
        if cs > new_live {
            break;
        }
        if cur_candle != Some(cs) {
            if let Some(prev) = cur_candle {
                flush_one(
                    prev, false, half, tail, lo_ms, span_ms, cols, rows, extend_right, cand_sums,
                    row_bucket, grid, col_px, want_values, samples,
                );
            }
            cur_candle = Some(cs);
            for s in cand_sums.iter_mut() {
                *s = 0.0;
            }
        }
        bucket_snapshot(snap, pl, ph, b, n_buckets, snap_sums);
        for k in 0..n_buckets {
            if snap_sums[k] > cand_sums[k] {
                cand_sums[k] = snap_sums[k];
            }
        }
    }
    let Some(prev) = cur_candle else {
        return None; // nothing to recompute — keep the existing cache
    };
    flush_one(
        prev,
        prev == new_live,
        half,
        tail,
        lo_ms,
        span_ms,
        cols,
        rows,
        extend_right,
        cand_sums,
        row_bucket,
        grid,
        col_px,
        want_values,
        samples,
    );

    colorize_range(
        grid,
        cols,
        rows,
        lo_f,
        log_lo,
        log_span,
        settings.max_opacity,
        settings.colormap,
        c_start,
        cols,
        buf,
    );

    let rgba = RgbaImage::from_raw(cols as u32, rows as u32, buf.clone())?;
    let image = Arc::new(RenderImage::new(vec![Frame::new(rgba)]));
    let values = build_values(want_values, samples, b, pl, n_buckets, lo, log_lo, log_span, &settings);

    Some(HeatmapCache {
        image,
        lo_ms,
        hi_ms,
        price_lo: pl,
        price_hi: ph,
        n_buckets,
        rows,
        cols,
        tf_ms,
        phase: cache.phase,
        settings,
        lo,
        log_lo,
        log_span,
        live_candle: new_live,
        last_ts: newest_ts,
        want_values,
        values,
    })
}

/// Full rebuild: aggregate the visible time-slice of `series` into the reusable
/// `scratch` BGRA grid and wrap it as a `RenderImage`. Returns `None` if no cell
/// ends up lit. Cheap per-tick updates go through [`try_patch`]; this is the
/// from-scratch path (first build, settings/dims/TF change, window slid past the
/// margin, the held band exceeded, or the [`SELF_HEAL_MS`] floor).
///
/// **Price (y) is lazy:** the band is the **visible** price range `[y_lo, y_hi]`
/// padded by [`Y_BAND_PAD_FRAC`] (held across patches and small auto-fit-Y
/// wiggle) — *not* the full book extent, so off-screen liquidity is clipped and
/// the texture is sized to ~one (oversampled) row per [`PRICE_BUCKET`] bucket of
/// the visible band (capped at [`MAX_BLIT_ROWS`]); the on-screen liquidity thus
/// renders crisp without a tall full-extent texture. **Time is bucketed by
/// candle**: the book samples are grouped into `tf_ms`-wide columns aligned to
/// the candle grid (`anchor_ms`), so each column is exactly one candle wide;
/// within a candle the per-price-bucket value is the **max** over its samples.
#[allow(clippy::too_many_arguments)]
fn build_full(
    series: &[BookSnapshotEntry],
    lo_ms: i64,
    hi_ms: i64,
    y_lo: f64,
    y_hi: f64,
    cols: usize,
    tf_ms: i64,
    anchor_ms: i64,
    phase: i64,
    settings: &HeatmapSettings,
    scratch: &mut HeatmapScratch,
) -> Option<HeatmapCache> {
    let span_ms = (hi_ms - lo_ms) as f64;
    if span_ms <= 0.0 || y_hi <= y_lo {
        return None;
    }

    // Window: include one carry-in snapshot before `lo` so the leading partial
    // candle is filled from the book state already in effect at `lo`.
    let first_in = series.partition_point(|s| s.ts_ms < lo_ms);
    let start = first_in.saturating_sub(1);
    let end = series.partition_point(|s| s.ts_ms <= hi_ms);
    if start >= end {
        return None;
    }

    // Lazy-y band: the *visible* price range padded by `Y_BAND_PAD_FRAC` (the pad
    // is the hysteresis that lets the band be held across patches + small auto-fit
    // wiggle), snapped to bucket boundaries so every row is a clean $5 cell. We do
    // NOT scan the book extent — off-screen liquidity is intentionally clipped by
    // the band (`bucket_snapshot` skips levels outside `[pl, ph)`), keeping the
    // on-screen band crisp at ~1 texel/bucket instead of a tall full-extent blit.
    let b = PRICE_BUCKET;
    let pad = (y_hi - y_lo) * Y_BAND_PAD_FRAC;
    let pl = ((y_lo - pad) / b).floor() * b;
    let ph = ((y_hi + pad) / b).floor() * b + b;
    let price_span = ph - pl;
    if price_span <= 0.0 {
        return None;
    }
    let n_buckets = ((price_span / b).round() as usize).max(1);
    // Integer texels per bucket: each bucket renders as an exact block of `K`
    // identical texels, so bilinear upscaling only blends across the 1-texel
    // boundary *between* buckets — crisp bands. Pick the largest integer
    // oversample that fits `MAX_BLIT_ROWS`; wider extents degrade to 1 texel per
    // bucket and then subsample (the blit is a coarse background — see
    // `MAX_BLIT_ROWS`; crisp cells come from the quad path, not the texture).
    let oversample = (MAX_BLIT_ROWS / n_buckets).clamp(1, VERTICAL_OVERSAMPLE);
    let rows = (n_buckets * oversample).clamp(MIN_TEXELS, MAX_BLIT_ROWS);

    // (Re)size the persistent scratch for this band. `grid` must be zeroed (the
    // flush maxes into it); `buf` is fully overwritten by `colorize_range`, so
    // only its length matters. Reusing these allocations across rebuilds avoids
    // the ~16 MB alloc+zero churn a per-tick rebuild used to pay.
    scratch.grid.clear();
    scratch.grid.resize(cols * rows, 0.0);
    scratch.buf.resize(cols * rows * 4, 0);
    scratch.row_bucket.resize(rows, 0);
    scratch.snap_sums.resize(n_buckets, 0.0);
    scratch.cand_sums.resize(n_buckets, 0.0);
    scratch.col_px.resize(rows, 0.0);
    scratch.samples.clear();

    // Pixel-row → bucket index (row 0 = top = highest price).
    for (r, slot) in scratch.row_bucket.iter_mut().enumerate() {
        let price = ph - ((r as f64 + 0.5) / rows as f64) * price_span;
        let k = ((price - pl) / b).floor();
        *slot = (k.max(0.0) as usize).min(n_buckets - 1);
    }

    // Keep the logical-cell table while the window is small enough to plausibly
    // render crisp per-cell quads (and label them). Independent of `show_text` —
    // the table feeds both the hard-edged cell painter and the text.
    let est_candles = (span_ms / tf_ms as f64) as usize + 2;
    let want_values = n_buckets <= TEXT_MAX_BUCKETS && est_candles <= TEXT_MAX_SAMPLES;

    // Candle open time covering `t`, aligned to the `anchor_ms` candle grid.
    let candle_start = |t: i64| -> i64 { anchor_ms + (t - anchor_ms).div_euclid(tf_ms) * tf_ms };
    // Candles are drawn *centred* on their index, so a column's data-time slot is
    // the candle open ± half a candle (`half + tail == tf_ms`, so columns tile
    // exactly), keeping each heatmap column under its candle body. The live
    // (last) candle extends to the right edge when `extend_right`.
    let half = tf_ms / 2;
    let tail = tf_ms - half;
    let extend_right = settings.extend_right;

    // When the window holds many more samples than columns, stride so we process
    // at most ~MAX_SAMPLES_PER_COL per column. Cap the stride at the per-candle
    // sample count so no candle is skipped (each gets ≥1 processed sample), and
    // always process the newest snapshot (`last`) so the live column is current.
    let total = end - start;
    let last = end - 1;
    let snaps_per_candle = ((tf_ms / 1000).max(1)) as usize;
    let stride = (total / cols.saturating_mul(MAX_SAMPLES_PER_COL).max(1))
        .max(1)
        .min(snaps_per_candle.max(1));
    let live_candle = candle_start(series[last].ts_ms);

    // Borrow the scratch fields disjointly for the aggregation + colour passes.
    let HeatmapScratch {
        grid,
        buf,
        row_bucket,
        snap_sums,
        cand_sums,
        col_px,
        samples,
    } = scratch;

    let mut cur_candle: Option<i64> = None;
    let mut i = start;
    loop {
        let snap = &series[i];
        let cs = candle_start(snap.ts_ms);
        if cur_candle != Some(cs) {
            if let Some(prev) = cur_candle {
                flush_one(
                    prev, false, half, tail, lo_ms, span_ms, cols, rows, extend_right, cand_sums,
                    row_bucket, grid, col_px, want_values, samples,
                );
            }
            cur_candle = Some(cs);
            for s in cand_sums.iter_mut() {
                *s = 0.0;
            }
        }
        bucket_snapshot(snap, pl, ph, b, n_buckets, snap_sums);
        for k in 0..n_buckets {
            if snap_sums[k] > cand_sums[k] {
                cand_sums[k] = snap_sums[k];
            }
        }
        if i == last {
            break;
        }
        i = (i + stride).min(last);
    }
    if let Some(prev) = cur_candle {
        flush_one(
            prev, true, half, tail, lo_ms, span_ms, cols, rows, extend_right, cand_sums, row_bucket,
            grid, col_px, want_values, samples,
        );
    }

    // Colour range (coin units): cells below `lo` aren't drawn; `peak` maps to
    // the top of the ramp. log1p so a few big walls don't wash out the rest.
    let lo = settings.color_lo.max(0.0);
    let peak = settings.color_peak.max(lo + 1e-6);
    let lo_f = lo as f32;
    let log_lo = (1.0 + lo).ln() as f32;
    let log_peak = (1.0 + peak).ln() as f32;
    let log_span = (log_peak - log_lo).max(1e-6);

    let any = colorize_range(
        grid,
        cols,
        rows,
        lo_f,
        log_lo,
        log_span,
        settings.max_opacity,
        settings.colormap,
        0,
        cols,
        buf,
    );
    if !any {
        return None;
    }

    let rgba = RgbaImage::from_raw(cols as u32, rows as u32, buf.clone())?;
    let image = Arc::new(RenderImage::new(vec![Frame::new(rgba)]));
    let values = build_values(want_values, samples, b, pl, n_buckets, lo, log_lo, log_span, settings);

    Some(HeatmapCache {
        image,
        lo_ms,
        hi_ms,
        price_lo: pl,
        price_hi: ph,
        n_buckets,
        rows,
        cols,
        tf_ms,
        phase,
        settings: *settings,
        lo,
        log_lo,
        log_span,
        live_candle,
        last_ts: series[last].ts_ms,
        want_values,
        values,
    })
}

// The colour ramp now lives on `Colormap::sample` (see the enum near the top),
// so the gradient is user-selectable per heatmap instead of a single fixed ramp.
