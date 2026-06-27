//! Liquidation-heatmap render layer — a GPU-texture blit painted *behind* the
//! candles, exactly like the orderbook heatmap ([`super::heatmap`]) but fed by a
//! forward **simulation** of un-liquidated leverage instead of a book
//! time-series. x = time (shared candle axis), y = price, colour = estimated
//! coin notional of positions that would force-close at that (price, time) cell.
//!
//! The simulation lives in [`crate::indicators::liq_heatmap::sim`] (pure,
//! unit-tested). This module owns the *texture* path: derive the visible price
//! band (lazy-y, mirroring the orderbook heatmap), run the sim clipped to that
//! band, bucket each column onto the texel grid, and colour it. The colour /
//! crisp-cell / text machinery is **shared** with the orderbook heatmap via the
//! `pub(super)` primitives in [`super::heatmap`] (`flush_one`, `colorize_range`,
//! `build_values`) and the common [`HeatmapRect`] / [`HeatmapValues`] types, so
//! both heatmaps paint through the same [`super::heatmap::paint_heatmap`].
//!
//! Independent of the orderbook heatmap: its own [`LiqHeatmapLayer`] field on
//! `ChartState`, its own cache + throttle + atlas tile. Both may be on at once.
//!
//! No incremental patch path (unlike the orderbook heatmap): the sim is cheap
//! (a few ops per loaded candle) so each throttled refresh re-runs it whole.

use std::sync::Arc;

use gpui::{RenderImage, Window};
use image::{Frame, RgbaImage};

use super::heatmap::{
    HeatmapRect, HeatmapSample, HeatmapSettings, HeatmapValues, build_values, colorize_range,
    flush_one,
};
use crate::indicators::liq_heatmap::sim::{self, LiqColumn, SimParams};
use crate::services::market_data::{Candle, MarkPriceBar, OpenInterestBar};

/// Liquidation-heatmap colour-range slider domain (coin/contract units). Log
/// scale, so `MIN` must be > 0. Wider top than the orderbook heatmap because a
/// magnet bucket accumulates across the lookback.
pub const LIQ_COLOR_RANGE_MIN: f64 = 1.0;
pub const LIQ_COLOR_RANGE_MAX: f64 = 1_000_000.0;

/// Fraction of the visible span the built texture extends past each edge.
const MARGIN_FRAC: f64 = 0.25;
/// Visible-price-range padding for the lazy-y band (held across small auto-fit
/// wiggle; a view exiting it triggers a rebuild).
const Y_BAND_PAD_FRAC: f64 = 0.35;
const MAX_TEXELS: usize = 2048;
const MIN_TEXELS: usize = 16;
/// Texel columns per visible candle (the GPU stretches horizontally).
const CANDLE_TEXELS: usize = 3;
const MAX_BLIT_ROWS: usize = 1024;
/// Texels per $5 bucket so bilinear upscaling only blends at bucket edges.
const VERTICAL_OVERSAMPLE: usize = 8;
/// Minimum wall-clock gap between full rebuilds (~7 Hz).
const MIN_REBUILD_INTERVAL_MS: i64 = 140;
/// Don't retain the per-cell value table past these counts (cells too small to
/// label / quad; bounds memory).
const TEXT_MAX_SAMPLES: usize = 240;
const TEXT_MAX_BUCKETS: usize = MAX_TEXELS;

/// A built texture plus the data-space rect it covers and the inputs it was
/// built from (the rebuild key).
struct LiqCache {
    image: Arc<RenderImage>,
    lo_ms: i64,
    hi_ms: i64,
    price_lo: f64,
    price_hi: f64,
    cols: usize,
    tf_ms: i64,
    phase: i64,
    settings: HeatmapSettings,
    mmr_bits: u64,
    lookback_ms: i64,
    /// Price-bucket width ("tick size") this texture was built at — part of the
    /// rebuild key (changing it re-buckets everything).
    bucket_bits: u64,
    /// Cheap hash of the sim inputs (candle / OI / mark tails + lengths) — a
    /// change forces a rebuild on the next throttle tick.
    fingerprint: u64,
    values: Option<Arc<HeatmapValues>>,
}

/// Persistent scratch reused across rebuilds so a per-tick rebuild doesn't
/// re-allocate the grid + BGRA buffer.
#[derive(Default)]
struct LiqScratch {
    /// `grid[col * rows + row]` — column-major per-bucket values.
    grid: Vec<f32>,
    /// Row-major BGRA, the `RenderImage` layout.
    buf: Vec<u8>,
    /// Pixel-row → bucket index (row 0 = top = highest price).
    row_bucket: Vec<usize>,
    cand_sums: Vec<f32>,
    col_px: Vec<f32>,
    samples: Vec<HeatmapSample>,
}

/// The liquidation-heatmap render layer owned by `ChartState`. Mirror fields
/// (`enabled` / `settings`) are synced from the indicator instance each frame.
pub struct LiqHeatmapLayer {
    pub enabled: bool,
    pub settings: HeatmapSettings,
    cache: Option<LiqCache>,
    last_build_ms: i64,
    scratch: LiqScratch,
}

impl Default for LiqHeatmapLayer {
    fn default() -> Self {
        Self {
            enabled: false,
            settings: HeatmapSettings::default(),
            cache: None,
            last_build_ms: 0,
            scratch: LiqScratch::default(),
        }
    }
}

impl LiqHeatmapLayer {
    /// Rebuild the texture if needed for the given visible window. `candles` /
    /// `oi` / `mark` are the oldest-first series `ChartState` already holds.
    /// Lazy on both axes (visible time + price band, each padded). Cheap no-op
    /// when the cache still covers the view with the same data/settings/sim.
    #[allow(clippy::too_many_arguments)]
    pub fn refresh(
        &mut self,
        candles: &[Candle],
        oi: &[OpenInterestBar],
        mark: &[MarkPriceBar],
        sim_params: SimParams,
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
        if !self.enabled || candles.is_empty() || vis_hi_ms <= vis_lo_ms || tf_ms <= 0 || y_hi <= y_lo
        {
            self.drop_cache(window);
            return;
        }

        let cols_px = (canvas_w.round() as usize).clamp(MIN_TEXELS, MAX_TEXELS);
        let vis_candles = (((vis_hi_ms - vis_lo_ms) / tf_ms).max(1)) as usize;
        let cols = cols_px.min(vis_candles.saturating_mul(CANDLE_TEXELS).max(MIN_TEXELS));
        let phase = anchor_ms.rem_euclid(tf_ms);
        let fingerprint = data_fingerprint(candles, oi, mark);
        let mmr_bits = sim_params.mmr.to_bits();
        let bucket_bits = sim_params.bucket.to_bits();

        // Reuse iff the cache still covers the visible window + price band with
        // the same dims / settings / sim inputs.
        let reuse = matches!(&self.cache, Some(c)
            if c.lo_ms <= vis_lo_ms
                && c.hi_ms >= vis_hi_ms
                && c.price_lo <= y_lo
                && c.price_hi >= y_hi
                && c.cols == cols
                && c.tf_ms == tf_ms
                && c.phase == phase
                && c.settings == self.settings
                && c.mmr_bits == mmr_bits
                && c.lookback_ms == sim_params.lookback_ms
                && c.bucket_bits == bucket_bits
                && c.fingerprint == fingerprint);
        if reuse {
            return;
        }

        // Throttle full rebuilds (continuous pan/zoom or the data tick would
        // otherwise want one every render). First build (no cache) is exempt;
        // the tick loop guarantees a trailing render within one interval.
        if self.cache.is_some() && now_ms - self.last_build_ms < MIN_REBUILD_INTERVAL_MS {
            return;
        }

        let span_ms = (vis_hi_ms - vis_lo_ms) as f64;
        let m_ms = (span_ms * MARGIN_FRAC) as i64;
        let lo_ms = vis_lo_ms - m_ms;
        let hi_ms = vis_hi_ms + m_ms;

        let built = build_full(
            candles, oi, mark, sim_params, lo_ms, hi_ms, y_lo, y_hi, cols, tf_ms, phase,
            &self.settings, fingerprint, &mut self.scratch,
        );

        self.last_build_ms = now_ms;
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

    /// The cached texture + the data-rect it covers, for the (shared) paint
    /// pass. `None` when disabled or nothing is built. Cheap `Arc` clones.
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
        })
    }
}

/// Cheap hash of the sim inputs: lengths + the (mutable) tail of each series.
/// A history prepend changes a length; a live tick changes the last candle /
/// OI / mark — both flip the fingerprint and trigger a rebuild.
fn data_fingerprint(candles: &[Candle], oi: &[OpenInterestBar], mark: &[MarkPriceBar]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut mix = |x: u64| {
        h ^= x;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    };
    mix(candles.len() as u64);
    if let Some(c) = candles.last() {
        mix(c.open_time as u64);
        mix(c.high.to_bits());
        mix(c.low.to_bits());
        mix(c.close.to_bits());
        mix(c.volume.to_bits());
        mix(c.taker_buy_vol.map_or(0, f64::to_bits));
    }
    mix(oi.len() as u64);
    if let Some(o) = oi.last() {
        mix(o.open_time as u64);
        mix(o.close.to_bits());
    }
    mix(mark.len() as u64);
    if let Some(m) = mark.last() {
        mix(m.open_time as u64);
        mix(m.close.to_bits());
    }
    h
}

/// Full rebuild: run the sim over the (warm-up-extended) window, bucket each
/// column onto the texel grid, and wrap it as a `RenderImage`. Returns `None`
/// when no cell ends up lit.
#[allow(clippy::too_many_arguments)]
fn build_full(
    candles: &[Candle],
    oi: &[OpenInterestBar],
    mark: &[MarkPriceBar],
    sim_params: SimParams,
    lo_ms: i64,
    hi_ms: i64,
    y_lo: f64,
    y_hi: f64,
    cols: usize,
    tf_ms: i64,
    phase: i64,
    settings: &HeatmapSettings,
    fingerprint: u64,
    scratch: &mut LiqScratch,
) -> Option<LiqCache> {
    let span_ms = (hi_ms - lo_ms) as f64;
    if span_ms <= 0.0 || y_hi <= y_lo {
        return None;
    }

    // Lazy-y band: the visible price range padded, snapped to bucket ("tick
    // size") boundaries.
    let b = sim_params.bucket_width();
    let pad = (y_hi - y_lo) * Y_BAND_PAD_FRAC;
    let pl = ((y_lo - pad) / b).floor() * b;
    let ph = ((y_hi + pad) / b).floor() * b + b;
    let price_span = ph - pl;
    if price_span <= 0.0 {
        return None;
    }
    let n_buckets = ((price_span / b).round() as usize).max(1);

    // Run the sim, emitting one column per candle in `[lo_ms, …]`, clipped to
    // the band. The sim warms up `lookback_ms` to the left of `lo_ms`.
    let columns = sim::simulate(candles, oi, mark, sim_params, lo_ms, Some((pl, ph)));
    if columns.is_empty() {
        return None;
    }

    // Integer texels per bucket so bilinear upscaling only blends at boundaries.
    let oversample = (MAX_BLIT_ROWS / n_buckets).clamp(1, VERTICAL_OVERSAMPLE);
    let rows = (n_buckets * oversample).clamp(MIN_TEXELS, MAX_BLIT_ROWS);

    scratch.grid.clear();
    scratch.grid.resize(cols * rows, 0.0);
    scratch.buf.resize(cols * rows * 4, 0);
    scratch.row_bucket.resize(rows, 0);
    scratch.cand_sums.resize(n_buckets, 0.0);
    scratch.col_px.resize(rows, 0.0);
    scratch.samples.clear();

    // Pixel-row → bucket index (row 0 = top = highest price).
    for (r, slot) in scratch.row_bucket.iter_mut().enumerate() {
        let price = ph - ((r as f64 + 0.5) / rows as f64) * price_span;
        let k = ((price - pl) / b).floor();
        *slot = (k.max(0.0) as usize).min(n_buckets - 1);
    }

    let want_values = n_buckets <= TEXT_MAX_BUCKETS && columns.len() <= TEXT_MAX_SAMPLES;

    // Candles are drawn centred on their index, so a column's data-time slot is
    // the candle open ± half a candle (`half + tail == tf_ms`, columns tile).
    let half = tf_ms / 2;
    let tail = tf_ms - half;
    let extend_right = settings.extend_right;
    // Absolute bucket index of the band's lowest row, to map a column's
    // absolute `(bucket_index, value)` pairs into `[0, n_buckets)`.
    let base = (pl / b).floor() as i64;

    let LiqScratch {
        grid,
        buf,
        row_bucket,
        cand_sums,
        col_px,
        samples,
    } = scratch;

    let last_idx = columns.len() - 1;
    for (idx, col) in columns.iter().enumerate() {
        for s in cand_sums.iter_mut() {
            *s = 0.0;
        }
        fill_cand_sums(col, base, n_buckets, cand_sums);
        flush_one(
            col.candle_start,
            idx == last_idx,
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
    }

    // Colour range (coin units): cells below `lo` aren't drawn; `peak` maps to
    // the ramp top. log1p so a few dense magnets don't wash out the rest.
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

    Some(LiqCache {
        image,
        lo_ms,
        hi_ms,
        price_lo: pl,
        price_hi: ph,
        cols,
        tf_ms,
        phase,
        settings: *settings,
        mmr_bits: sim_params.mmr.to_bits(),
        lookback_ms: sim_params.lookback_ms,
        bucket_bits: sim_params.bucket.to_bits(),
        fingerprint,
        values,
    })
}

/// Scatter one sim column's absolute `(bucket_index, value)` pairs into the
/// band-relative `cand_sums` (length `n_buckets`, must be pre-zeroed).
fn fill_cand_sums(col: &LiqColumn, base: i64, n_buckets: usize, cand_sums: &mut [f32]) {
    for &(k_abs, v) in &col.buckets {
        let k = k_abs - base;
        if k >= 0 && (k as usize) < n_buckets {
            cand_sums[k as usize] = v;
        }
    }
}
