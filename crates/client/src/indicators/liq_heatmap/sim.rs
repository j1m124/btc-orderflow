//! Liquidation-heatmap forward simulation — the pure, unit-testable core.
//!
//! Coinglass-style *predictive* model: it estimates where leveraged positions
//! would be force-closed (price "magnets"), **not** a histogram of realized
//! `forceOrder` events. Per candle, at the chart's timeframe:
//!
//! 1. **Consume** — zero every $5 price bucket the candle's `[low, high]` swept
//!    (the magnet got hit). This runs *before* placement so a position entered
//!    on candle `i` can't be liquidated by candle `i`'s own wick ("place, then
//!    consume from the NEXT candle").
//! 2. **Place** — when `ΔOI > 0`, add new leveraged notional. ΔOI drives the
//!    magnitude; taker delta drives the long/short split. Each side is spread
//!    equally across the leverage tiers `[10, 25, 50, 100]×`; the per-tier
//!    liquidation price (VWAP entry, maintenance margin `mmr`) picks the bucket.
//! 3. **Snapshot** — the running per-bucket state becomes that candle's column.
//!
//! Magnitude is stored in **coin (contract) units** — `ΔOI × split_fraction`,
//! *not* `× mark` — so the render layer reuses the orderbook heatmap's coin
//! colour ramp and Coin/USD text toggle verbatim (USD = coin × the bucket's
//! liquidation price, which is exactly where the position sits). The shape is
//! identical to the USD form up to the ~constant mark factor.
//!
//! Pure logic, no gpui — the render layer (`panels/chart/paint/liq_heatmap.rs`)
//! calls [`simulate`] then buckets the columns into a texture. Unit-tested at
//! the bottom of this file.

use std::collections::BTreeMap;

use crate::services::market_data::{Candle, MarkPriceBar, OpenInterestBar};

/// Default price-bucket width in dollars (the orderbook heatmap's $5 grain).
/// Now a *default* — the bucket ("tick size") is user-selectable per instance
/// via [`SimParams::bucket`].
pub const DEFAULT_PRICE_BUCKET: f64 = 5.0;

/// Leverage levels the heatmap can model, low → high. The user toggles a
/// subset (carried as a parallel `[bool; N_LEVERAGE]` on [`SimParams`]); the
/// sim spreads each side's notional equally across the **active** levels — each
/// active level-side gets `1 / active_count` of the side's notional.
pub const AVAILABLE_LEVERAGE: [f64; 6] = [5.0, 10.0, 25.0, 50.0, 75.0, 100.0];

/// Number of toggleable leverage levels — the width of the selection array.
pub const N_LEVERAGE: usize = AVAILABLE_LEVERAGE.len();

/// Default active selection: 50×, 75×, 100× (indices 3, 4, 5 of
/// [`AVAILABLE_LEVERAGE`]). Keep in sync with the array above if its order
/// changes.
pub const DEFAULT_LEVERAGE_SELECTED: [bool; N_LEVERAGE] =
    [false, false, false, true, true, true];

/// Tunable sim inputs carried on the indicator's params.
#[derive(Clone, Copy, Debug)]
pub struct SimParams {
    /// Maintenance-margin rate (fraction, e.g. `0.004` = 0.4%). Not negligible
    /// for tight bands: at 100× the raw `1/L = 1.0%` band moves to ~`0.6%` with
    /// a 0.4% MMR — a 40% shift.
    pub mmr: f64,
    /// Warm-up lookback in ms. The sim starts this far left of the emission
    /// window so positions opened before the visible range still magnetize.
    pub lookback_ms: i64,
    /// Price-bucket width in dollars (the heatmap's "tick size"). Coarser
    /// buckets merge nearby magnets into fewer, fatter rows; finer buckets keep
    /// them distinct. The render layer must use the same value.
    pub bucket: f64,
    /// Active leverage selection, parallel to [`AVAILABLE_LEVERAGE`]. The sim
    /// places notional only at the toggled-on levels; an all-`false` selection
    /// places nothing.
    pub tiers: [bool; N_LEVERAGE],
}

impl SimParams {
    /// The effective bucket width, guarded against a non-positive value.
    #[inline]
    pub fn bucket_width(&self) -> f64 {
        if self.bucket > 0.0 {
            self.bucket
        } else {
            DEFAULT_PRICE_BUCKET
        }
    }

    /// The active leverage levels (toggles resolved against the pool), low →
    /// high. Empty when the user has deselected everything (the sim then places
    /// nothing).
    pub fn active_tiers(&self) -> Vec<f64> {
        AVAILABLE_LEVERAGE
            .iter()
            .zip(self.tiers.iter())
            .filter_map(|(l, on)| on.then_some(*l))
            .collect()
    }
}

/// One simulated column: the running un-liquidated notional per price bucket at
/// the close of `candle_start`'s candle. `buckets` is `(absolute_bucket_index,
/// coin_notional)` sorted ascending by index; absolute index `k` spans price
/// `[k·PRICE_BUCKET, (k+1)·PRICE_BUCKET)`.
#[derive(Clone, Debug)]
pub struct LiqColumn {
    pub candle_start: i64,
    pub buckets: Vec<(i64, f32)>,
}

/// Absolute price-bucket index for a price at the given bucket width.
#[inline]
fn bucket_of(price: f64, bucket: f64) -> i64 {
    (price / bucket).floor() as i64
}

/// Two-pointer align of a per-`open_time` OHLC series onto `candles`, returning
/// the matched bar's `close` (or `None` where no bar shares the candle's
/// `open_time`). Both inputs are ascending by `open_time`.
fn align_oi(candles: &[Candle], oi: &[OpenInterestBar]) -> Vec<Option<f64>> {
    let mut out = vec![None; candles.len()];
    let mut j = 0usize;
    for (i, c) in candles.iter().enumerate() {
        while j < oi.len() && oi[j].open_time < c.open_time {
            j += 1;
        }
        if j < oi.len() && oi[j].open_time == c.open_time {
            out[i] = Some(oi[j].close);
        }
    }
    out
}

fn align_mark(candles: &[Candle], mark: &[MarkPriceBar]) -> Vec<Option<f64>> {
    let mut out = vec![None; candles.len()];
    let mut j = 0usize;
    for (i, c) in candles.iter().enumerate() {
        while j < mark.len() && mark[j].open_time < c.open_time {
            j += 1;
        }
        if j < mark.len() && mark[j].open_time == c.open_time {
            out[i] = Some(mark[j].close);
        }
    }
    out
}

/// Run the forward simulation.
///
/// - `candles` — oldest-first OHLCV bars at the chart's TF.
/// - `oi` / `mark` — oldest-first OI / mark-price bars (aligned by `open_time`).
/// - `params` — MMR + warm-up lookback.
/// - `emit_start_ms` — emit a column for every candle whose `close_time` is at
///   or after this (the texture's left edge). Earlier candles are still
///   simulated (warm-up) so the left edge is populated correctly.
/// - `band` — optional `(price_lo, price_hi)` to clip each emitted column to
///   (the render layer's visible price band; bounds snapshot memory). `None`
///   emits the full state — used by the unit tests to assert exact placement.
///
/// The running state accumulates the *full* book of magnets regardless of
/// `band`; only the emitted snapshot is clipped.
pub fn simulate(
    candles: &[Candle],
    oi: &[OpenInterestBar],
    mark: &[MarkPriceBar],
    params: SimParams,
    emit_start_ms: i64,
    band: Option<(f64, f64)>,
) -> Vec<LiqColumn> {
    if candles.is_empty() {
        return Vec::new();
    }

    let oi_close = align_oi(candles, oi);
    let mark_close = align_mark(candles, mark);
    let bucket = params.bucket_width();

    // Warm-up start: the first candle at or after `emit_start − lookback`.
    let sim_start_ms = emit_start_ms.saturating_sub(params.lookback_ms.max(0));
    let start = candles.partition_point(|c| c.open_time < sim_start_ms);

    let active_tiers = params.active_tiers();
    let n_tiers = active_tiers.len() as f64;
    // Emission band as an exclusive bucket-index range.
    let band_buckets = band.map(|(lo, hi)| (bucket_of(lo, bucket), bucket_of(hi, bucket) + 1));

    let mut state: BTreeMap<i64, f64> = BTreeMap::new();
    let mut out: Vec<LiqColumn> = Vec::new();

    for i in start..candles.len() {
        let c = &candles[i];

        // 1. Consume: zero every bucket the candle's range swept. Runs against
        //    the PRE-existing state (positions from earlier candles), so this
        //    candle's own placement (step 2) can never self-liquidate.
        if c.high >= c.low {
            let lo_b = bucket_of(c.low, bucket);
            let hi_b = bucket_of(c.high, bucket);
            let hit: Vec<i64> = state.range(lo_b..=hi_b).map(|(k, _)| *k).collect();
            for k in hit {
                state.remove(&k);
            }
        }

        // 2. Place: only on an OI increase (new leverage entering). ΔOI is the
        //    magnitude; taker delta splits long vs short.
        let oi_now = oi_close[i];
        let oi_prev = if i > 0 { oi_close[i - 1] } else { None };
        if let (Some(oi_now), Some(oi_prev)) = (oi_now, oi_prev) {
            let d_oi = oi_now - oi_prev;
            if d_oi > 0.0 && c.volume > 0.0 && !active_tiers.is_empty() {
                let delta = c.taker_buy_vol.map_or(0.0, |tb| 2.0 * tb - c.volume);
                let long_frac = 0.5 + 0.5 * (delta / c.volume).clamp(-1.0, 1.0);
                let long_n = d_oi * long_frac;
                let short_n = d_oi * (1.0 - long_frac);
                // Magnitude is stored in coin/contract units (`ΔOI × split`),
                // not `× mark` — the render layer recovers USD as coin × the
                // bucket's liquidation price (where the position sits), so the
                // orderbook heatmap's coin colour ramp + Coin/USD text toggle are
                // reused verbatim. Mark price still feeds the *entry* estimate:
                // VWAP is the true average fill, but when the bar shipped no
                // quote volume the mark close is a better proxy than the last
                // trade close.
                let entry = c.vwap.or(mark_close[i]).unwrap_or(c.close);
                let long_per = long_n / n_tiers;
                let short_per = short_n / n_tiers;
                for &l in &active_tiers {
                    // Longs liquidate below entry, shorts above; MMR widens the
                    // band toward entry (a position is closed before price
                    // reaches the raw 1/L distance). Only insert a side that
                    // actually carries notional, so a one-sided candle doesn't
                    // litter the state with zero-valued buckets.
                    if long_per > 0.0 {
                        let long_liq = entry * (1.0 - 1.0 / l + params.mmr);
                        if long_liq > 0.0 {
                            *state.entry(bucket_of(long_liq, bucket)).or_insert(0.0) += long_per;
                        }
                    }
                    if short_per > 0.0 {
                        let short_liq = entry * (1.0 + 1.0 / l - params.mmr);
                        if short_liq > 0.0 {
                            *state.entry(bucket_of(short_liq, bucket)).or_insert(0.0) += short_per;
                        }
                    }
                }
            }
        }

        // 3. Snapshot once we're in the emission window.
        if c.close_time >= emit_start_ms {
            let buckets: Vec<(i64, f32)> = match band_buckets {
                Some((lo_b, hi_b)) => state
                    .range(lo_b..hi_b)
                    .map(|(k, v)| (*k, *v as f32))
                    .collect(),
                None => state.iter().map(|(k, v)| (*k, *v as f32)).collect(),
            };
            out.push(LiqColumn {
                candle_start: c.open_time,
                buckets,
            });
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use wasm_bindgen_test::wasm_bindgen_test;

    const TF: i64 = 60_000;

    /// Build a candle with explicit VWAP + taker-buy volume. `open_time` in ms.
    fn candle(open_time: i64, o: f64, h: f64, l: f64, close: f64, vol: f64, taker_buy: f64) -> Candle {
        Candle::new_full(
            open_time,
            open_time + TF - 1,
            o,
            h,
            l,
            close,
            vol,
            // VWAP = midpoint of the bar's body for a stable test entry price.
            Some((o + close) / 2.0),
            None,
            Some(taker_buy),
        )
    }

    fn oi(open_time: i64, close: f64) -> OpenInterestBar {
        OpenInterestBar {
            open_time,
            open: close,
            high: close,
            low: close,
            close,
        }
    }

    fn mark(open_time: i64, close: f64) -> MarkPriceBar {
        MarkPriceBar {
            open_time,
            open: close,
            high: close,
            low: close,
            close,
            funding_rate: None,
        }
    }

    /// Test leverage selection: the legacy 10/25/50/100× set (indices 1–4 of
    /// `AVAILABLE_LEVERAGE`) so the four-band assertions below stay meaningful.
    const TEST_TIERS: [bool; N_LEVERAGE] = [false, true, true, true, true, false];

    fn params() -> SimParams {
        SimParams {
            mmr: 0.004,
            lookback_ms: 24 * 60 * 60 * 1000,
            bucket: 5.0,
            tiers: TEST_TIERS,
        }
    }

    /// Total notional across all buckets in a column.
    fn col_total(c: &LiqColumn) -> f32 {
        c.buckets.iter().map(|(_, v)| *v).sum()
    }

    #[wasm_bindgen_test]
    fn empty_input_yields_empty() {
        let out = simulate(&[], &[], &[], params(), 0, None);
        assert!(out.is_empty());
    }

    #[wasm_bindgen_test]
    fn no_oi_increase_places_nothing() {
        // Flat OI → no ΔOI > 0 → no magnets, but a column is still emitted.
        let candles = vec![
            candle(0, 100.0, 101.0, 99.0, 100.0, 10.0, 5.0),
            candle(TF, 100.0, 101.0, 99.0, 100.0, 10.0, 5.0),
        ];
        let ois = vec![oi(0, 1000.0), oi(TF, 1000.0)];
        let marks = vec![mark(0, 100.0), mark(TF, 100.0)];
        let out = simulate(&candles, &ois, &marks, params(), 0, None);
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|c| c.buckets.is_empty()));
    }

    #[wasm_bindgen_test]
    fn long_heavy_candle_places_bands_below_entry() {
        // Entry VWAP = 10000 (body midpoint of open=close=10000). Strong taker
        // buying (taker_buy = volume) → long_frac = 1.0 → all notional long →
        // bands strictly BELOW entry. ΔOI = 100 over one candle.
        let entry = 10_000.0;
        let candles = vec![
            candle(0, entry, entry, entry, entry, 100.0, 100.0),
            candle(TF, entry, entry, entry, entry, 100.0, 100.0),
        ];
        let ois = vec![oi(0, 1_000.0), oi(TF, 1_100.0)];
        let marks = vec![mark(0, entry), mark(TF, entry)];
        // No band clip; assert exact placement on the 2nd candle's column.
        let out = simulate(&candles, &ois, &marks, params(), 0, None);
        let last = out.last().unwrap();
        assert!(!last.buckets.is_empty());
        // Every lit bucket sits below entry (long liquidations).
        for (k, v) in &last.buckets {
            assert!(*v > 0.0);
            let price = *k as f64 * DEFAULT_PRICE_BUCKET;
            assert!(price < entry, "bucket {k} at {price} not below entry {entry}");
        }
        // Total coin notional ≈ ΔOI (100), within bucket-rounding/precision.
        assert!((col_total(last) - 100.0).abs() < 1.0);
    }

    #[wasm_bindgen_test]
    fn short_heavy_candle_places_bands_above_entry() {
        // taker_buy = 0 → long_frac = 0 → all short → bands ABOVE entry.
        let entry = 10_000.0;
        let candles = vec![
            candle(0, entry, entry, entry, entry, 100.0, 0.0),
            candle(TF, entry, entry, entry, entry, 100.0, 0.0),
        ];
        let ois = vec![oi(0, 1_000.0), oi(TF, 1_100.0)];
        let marks = vec![mark(0, entry), mark(TF, entry)];
        let out = simulate(&candles, &ois, &marks, params(), 0, None);
        let last = out.last().unwrap();
        assert!(!last.buckets.is_empty());
        for (k, _) in &last.buckets {
            let price = *k as f64 * DEFAULT_PRICE_BUCKET;
            assert!(price > entry, "bucket {k} at {price} not above entry {entry}");
        }
    }

    #[wasm_bindgen_test]
    fn wick_clears_a_band() {
        // Candle 0 = OI base (can't place: no prior bar for ΔOI). Candle 1
        // places long bands well below entry (10× → ~10% below). Candle 2 wicks
        // down THROUGH those bands → they clear.
        let entry = 10_000.0;
        // 10× long liq ≈ entry·(1 − 0.1 + 0.004) = 9040. A wick to 9000 sweeps it.
        let candles = vec![
            candle(0, entry, entry, entry, entry, 100.0, 100.0), // OI base
            candle(TF, entry, entry, entry, entry, 100.0, 100.0), // places long bands
            // Candle 2: deep wick down to 9000, OI flat (no new placement).
            candle(2 * TF, entry, entry, 9_000.0, entry, 100.0, 50.0),
        ];
        let ois = vec![oi(0, 1_000.0), oi(TF, 1_100.0), oi(2 * TF, 1_100.0)];
        let marks = vec![mark(0, entry), mark(TF, entry), mark(2 * TF, entry)];

        let out = simulate(&candles, &ois, &marks, params(), 0, None);
        assert_eq!(out.len(), 3);
        assert!(col_total(&out[1]) > 0.0, "candle 1 should have placed bands");
        // The 9040-ish 10× band is within [9000, 10000] → swept by candle 2.
        let swept_bucket = bucket_of(entry * (1.0 - 0.1 + 0.004), 5.0);
        assert!(
            out[1].buckets.iter().any(|(k, _)| *k == swept_bucket),
            "candle 1 should hold the ~9040 band before the wick"
        );
        assert!(
            !out[2].buckets.iter().any(|(k, _)| *k == swept_bucket),
            "candle 2 wick to 9000 should clear the ~9040 band"
        );
    }

    #[wasm_bindgen_test]
    fn entry_cannot_self_liquidate() {
        // A candle with a huge range that spans its own liq bands must NOT clear
        // the bands it places on the SAME candle (consume runs before place).
        let entry = 10_000.0;
        let candles = vec![
            candle(0, entry, entry, entry, entry, 100.0, 100.0),
            // Candle 1 opens with a massive range 8000..12000 AND increases OI.
            candle(TF, entry, 12_000.0, 8_000.0, entry, 100.0, 100.0),
        ];
        let ois = vec![oi(0, 1_000.0), oi(TF, 1_100.0)];
        let marks = vec![mark(0, entry), mark(TF, entry)];
        let out = simulate(&candles, &ois, &marks, params(), 0, None);
        let last = out.last().unwrap();
        // Candle 1 placed fresh bands (long, below entry, ~9040). Despite its
        // range sweeping 8000..12000, those just-placed bands survive its own
        // candle.
        assert!(col_total(last) > 0.0, "candle 1's own placement must survive");
    }

    #[wasm_bindgen_test]
    fn warmup_populates_left_edge() {
        // Place a magnet during warm-up (candle 1, before the emission window)
        // and confirm the first EMITTED column (candle 2) already carries it.
        let entry = 10_000.0;
        let candles = vec![
            candle(0, entry, entry, entry, entry, 100.0, 100.0), // OI base
            candle(TF, entry, entry, entry, entry, 100.0, 100.0), // warm-up: places
            candle(2 * TF, entry, entry, entry, entry, 0.0, 0.0), // emitted, no new placement
        ];
        let ois = vec![oi(0, 1_000.0), oi(TF, 1_100.0), oi(2 * TF, 1_100.0)];
        let marks = vec![mark(0, entry), mark(TF, entry), mark(2 * TF, entry)];
        // Emit only from candle 2 (its close_time). lookback large enough that
        // candles 0 and 1 are simulated.
        let out = simulate(&candles, &ois, &marks, params(), 2 * TF, None);
        assert_eq!(out.len(), 1, "only candle 2 is in the emission window");
        assert!(
            col_total(&out[0]) > 0.0,
            "the warm-up candle's magnet must carry into the first emitted column"
        );
    }

    #[wasm_bindgen_test]
    fn bucket_width_changes_granularity() {
        // Same long-heavy placement; a coarse bucket merges the four tiers'
        // bands into fewer rows but conserves the total notional.
        let entry = 10_000.0;
        let candles = vec![
            candle(0, entry, entry, entry, entry, 100.0, 100.0),
            candle(TF, entry, entry, entry, entry, 100.0, 100.0),
        ];
        let ois = vec![oi(0, 1_000.0), oi(TF, 1_100.0)];
        let marks = vec![mark(0, entry), mark(TF, entry)];

        let fine = SimParams { mmr: 0.004, lookback_ms: 24 * 60 * 60 * 1000, bucket: 1.0, tiers: TEST_TIERS };
        let coarse = SimParams { mmr: 0.004, lookback_ms: 24 * 60 * 60 * 1000, bucket: 1_000.0, tiers: TEST_TIERS };
        let out_fine = simulate(&candles, &ois, &marks, fine, 0, None);
        let out_coarse = simulate(&candles, &ois, &marks, coarse, 0, None);
        let cf = out_fine.last().unwrap();
        let cc = out_coarse.last().unwrap();

        // The four tier bands (≈9040/9640/9840/9940) stay distinct at $1 but all
        // fall in the single 9000-wide bucket at $1000.
        assert_eq!(cf.buckets.len(), 4);
        assert_eq!(cc.buckets.len(), 1);
        // Coarse bucket index reflects the 1000-wide grid (9000 ≤ price < 10000).
        let (k, _) = cc.buckets[0];
        assert_eq!(k as f64 * 1_000.0, 9_000.0);
        // Total notional is conserved across the regrid.
        assert!((col_total(cf) - col_total(cc)).abs() < 1e-3);
    }

    #[wasm_bindgen_test]
    fn leverage_selection_controls_bands() {
        // Long-heavy placement at a $1 bucket. Selecting only 5× yields exactly
        // one band, at ~entry·(1 − 1/5 + mmr); an empty selection places nothing.
        let entry = 10_000.0;
        let candles = vec![
            candle(0, entry, entry, entry, entry, 100.0, 100.0),
            candle(TF, entry, entry, entry, entry, 100.0, 100.0),
        ];
        let ois = vec![oi(0, 1_000.0), oi(TF, 1_100.0)];
        let marks = vec![mark(0, entry), mark(TF, entry)];

        // Only 5× (index 0) active.
        let only_5x = SimParams {
            mmr: 0.004,
            lookback_ms: 24 * 60 * 60 * 1000,
            bucket: 1.0,
            tiers: [true, false, false, false, false, false],
        };
        let out = simulate(&candles, &ois, &marks, only_5x, 0, None);
        let last = out.last().unwrap();
        assert_eq!(last.buckets.len(), 1, "a single selected tier → one band");
        let (k, _) = last.buckets[0];
        assert_eq!(
            k,
            bucket_of(entry * (1.0 - 1.0 / 5.0 + 0.004), 1.0),
            "the 5× long band sits at ~8040"
        );

        // Nothing selected → nothing placed (a column is still emitted).
        let none = SimParams {
            mmr: 0.004,
            lookback_ms: 24 * 60 * 60 * 1000,
            bucket: 1.0,
            tiers: [false; N_LEVERAGE],
        };
        let out = simulate(&candles, &ois, &marks, none, 0, None);
        assert!(
            out.last().unwrap().buckets.is_empty(),
            "an empty leverage selection places nothing"
        );
    }

    #[wasm_bindgen_test]
    fn band_clip_bounds_snapshot() {
        // The clipped snapshot only carries buckets inside the band, but the
        // running state still accumulates everything.
        let entry = 10_000.0;
        let candles = vec![
            candle(0, entry, entry, entry, entry, 100.0, 50.0), // balanced → both sides
            candle(TF, entry, entry, entry, entry, 100.0, 50.0),
        ];
        let ois = vec![oi(0, 1_000.0), oi(TF, 1_100.0)];
        let marks = vec![mark(0, entry), mark(TF, entry)];
        // Band only just below entry: should drop the short (above-entry) bands.
        let band = Some((9_000.0, entry));
        let out = simulate(&candles, &ois, &marks, params(), 0, band);
        let last = out.last().unwrap();
        for (k, _) in &last.buckets {
            let price = *k as f64 * DEFAULT_PRICE_BUCKET;
            assert!(
                (9_000.0..entry).contains(&price),
                "band-clipped bucket {price} escaped [9000, {entry})"
            );
        }
    }
}
