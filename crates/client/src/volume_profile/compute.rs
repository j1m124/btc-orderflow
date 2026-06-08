//! Pure aggregation: footprint cells → per-price-bucket totals + Steidlmayer
//! value-area expansion. Stateless; the caller (VRVP indicator or FRVP
//! drawing paint) owns the cached output.
//!
//! The aggregator is bucket-agnostic — it keys on the cell's
//! `price_bucket_low` directly (which the server has already pre-quantized
//! to the requested bucket size). We don't re-bucket here, so changing the
//! bucket size requires a new footprint subscription, not a recompute over
//! existing cells.
//!
//! Coverage is reported as `unique_open_times_in_range / requested_bar_count`.
//! The caller passes `range_ms = (first_bar_open_time, last_bar_open_time +
//! tf_ms)` and supplies the timeframe so we can derive both numerator and
//! denominator without pulling a Candle slice down here.

use std::collections::BTreeMap;

use super::output::{VolumeProfileOutput, VpBucket};
use super::params::VolumeProfileParams;
use crate::services::market_data::FootprintCell;

/// Compute the per-bucket profile + reference levels for `cells` whose
/// `open_time` lands in `[range_ms.0, range_ms.1)`.
///
/// `tf_ms` is the timeframe of the bars that `range_ms` spans, in
/// milliseconds. Used both to compute the requested bar count for the
/// coverage denominator AND to derive each bucket's `price_high` (which is
/// `price_low + bucket_dollars()`). A non-positive `tf_ms` collapses
/// coverage to `0.0` (defensive — the caller should never pass that).
///
/// `range_ms.0 >= range_ms.1` (degenerate FRVP click-without-drag) returns
/// an empty output. `cells` may be empty (subscription not yet warm) —
/// returns an empty output with `coverage_pct = 0.0`.
pub fn compute_volume_profile(
    cells: &[FootprintCell],
    range_ms: (i64, i64),
    tf_ms: i64,
    params: &VolumeProfileParams,
) -> VolumeProfileOutput {
    let (t_lo, t_hi) = range_ms;
    if t_hi <= t_lo || tf_ms <= 0 {
        return VolumeProfileOutput::default();
    }
    let bucket_dollars = params.bucket_dollars();
    if !bucket_dollars.is_finite() || bucket_dollars <= 0.0 {
        return VolumeProfileOutput::default();
    }

    // Aggregate per price-bucket. Bit-pattern key so f64 bucket-low values
    // collide deterministically (same trick `FootprintSubKey` uses). BTreeMap
    // so the final `buckets` Vec comes out sorted by price ascending without
    // a separate sort pass.
    let mut by_bucket: BTreeMap<u64, (f64, f64, f64)> = BTreeMap::new(); // key → (low, bid, ask)
    // Unique covered bars within the requested window — coverage numerator.
    let mut covered_bars: std::collections::HashSet<i64> = std::collections::HashSet::new();

    for cell in cells {
        if cell.open_time < t_lo || cell.open_time >= t_hi {
            continue;
        }
        covered_bars.insert(cell.open_time);
        let key = cell.price_bucket_low.to_bits();
        let entry = by_bucket
            .entry(key)
            .or_insert((cell.price_bucket_low, 0.0, 0.0));
        entry.1 += cell.bid_vol;
        entry.2 += cell.ask_vol;
    }

    if by_bucket.is_empty() {
        // No cells fell in the range — still report coverage so the caller
        // can decide whether to render a "no data yet" placeholder.
        let requested_bars = ((t_hi - t_lo) / tf_ms).max(1) as f32;
        return VolumeProfileOutput {
            coverage_pct: (covered_bars.len() as f32 / requested_bars).min(1.0),
            ..VolumeProfileOutput::default()
        };
    }

    let buckets: Vec<VpBucket> = by_bucket
        .into_values()
        .map(|(low, bid, ask)| VpBucket {
            price_low: low,
            price_high: low + bucket_dollars,
            total: bid + ask,
            delta: ask - bid,
        })
        .collect();

    let total_volume: f64 = buckets.iter().map(|b| b.total).sum();
    let poc_idx = pick_poc(&buckets);
    let poc_price = poc_idx.map(|i| buckets[i].midpoint());

    let (vah_price, val_price) = poc_idx
        .map(|i| value_area(&buckets, i, total_volume, params.va_percent))
        .unwrap_or((None, None));

    let requested_bars = ((t_hi - t_lo) / tf_ms).max(1) as f32;
    let coverage_pct = (covered_bars.len() as f32 / requested_bars).min(1.0);

    VolumeProfileOutput {
        buckets,
        poc_price,
        vah_price,
        val_price,
        total_volume,
        coverage_pct,
    }
}

/// Index of the bucket with the highest `total`. Ties resolve to the
/// **higher-priced** bucket (later in the sorted Vec) — chosen because the
/// VA expansion has a slight downward bias from the lower-first walk
/// inside [`value_area`], and breaking POC ties upward keeps the two
/// directions symmetric across the common tie case (a flat profile during
/// a chop range).
fn pick_poc(buckets: &[VpBucket]) -> Option<usize> {
    if buckets.is_empty() {
        return None;
    }
    let mut best = 0usize;
    let mut best_v = buckets[0].total;
    for (i, b) in buckets.iter().enumerate().skip(1) {
        if b.total >= best_v {
            best = i;
            best_v = b.total;
        }
    }
    Some(best)
}

/// Steidlmayer expansion: start from POC, walk outward picking the larger
/// of the two adjacent buckets (or the larger of the next *two* buckets on
/// each side — the "pair lookup" variant TV uses) until the accumulated
/// volume reaches `va_percent` of `total_volume`.
///
/// Returns `(vah_price, val_price)` — the top edge of the highest included
/// bucket and the bottom edge of the lowest. Both `None` when
/// `total_volume == 0.0` (defensive; the caller already returned early for
/// empty buckets).
fn value_area(
    buckets: &[VpBucket],
    poc_idx: usize,
    total_volume: f64,
    va_percent: u8,
) -> (Option<f64>, Option<f64>) {
    if total_volume <= 0.0 || buckets.is_empty() {
        return (None, None);
    }
    let target = total_volume * (va_percent as f64 / 100.0);
    let mut accum = buckets[poc_idx].total;
    let mut hi = poc_idx; // grows upward (toward higher price)
    let mut lo = poc_idx; // grows downward (toward lower price)

    while accum < target && (hi + 1 < buckets.len() || lo > 0) {
        // Pair lookup: peek the next two buckets in each direction; pick the
        // direction whose pair-sum is larger. Falls back to single-bucket
        // sum when only one neighbor remains, and to "the only direction
        // with anything left" when one side is exhausted.
        let up_sum = pair_sum_above(buckets, hi);
        let dn_sum = pair_sum_below(buckets, lo);
        let go_up = match (up_sum, dn_sum) {
            (Some(u), Some(d)) => u >= d, // ties expand upward, mirrors POC tiebreak
            (Some(_), None) => true,
            (None, Some(_)) => false,
            (None, None) => break,
        };
        if go_up {
            // Consume one or two buckets up — same as the pair we just summed.
            hi += 1;
            accum += buckets[hi].total;
            if accum < target && hi + 1 < buckets.len() {
                hi += 1;
                accum += buckets[hi].total;
            }
        } else {
            lo -= 1;
            accum += buckets[lo].total;
            if accum < target && lo > 0 {
                lo -= 1;
                accum += buckets[lo].total;
            }
        }
    }

    (Some(buckets[hi].price_high), Some(buckets[lo].price_low))
}

fn pair_sum_above(buckets: &[VpBucket], hi: usize) -> Option<f64> {
    let a = buckets.get(hi + 1)?.total;
    let b = buckets.get(hi + 2).map(|b| b.total).unwrap_or(0.0);
    Some(a + b)
}

fn pair_sum_below(buckets: &[VpBucket], lo: usize) -> Option<f64> {
    if lo == 0 {
        return None;
    }
    let a = buckets[lo - 1].total;
    let b = if lo >= 2 { buckets[lo - 2].total } else { 0.0 };
    Some(a + b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::volume_profile::params::VolumeProfileParams;
    use wasm_bindgen_test::wasm_bindgen_test;

    fn cell(open_time: i64, bucket_low: f64, bid: f64, ask: f64) -> FootprintCell {
        FootprintCell {
            open_time,
            price_bucket_low: bucket_low,
            bid_vol: bid,
            ask_vol: ask,
        }
    }

    fn params() -> VolumeProfileParams {
        VolumeProfileParams::default()
    }

    #[wasm_bindgen_test]
    fn empty_input_yields_empty_output() {
        let out = compute_volume_profile(&[], (0, 60_000), 60_000, &params());
        assert!(out.buckets.is_empty());
        assert_eq!(out.poc_price, None);
        assert_eq!(out.coverage_pct, 0.0);
    }

    #[wasm_bindgen_test]
    fn degenerate_range_yields_empty() {
        let cells = vec![cell(0, 100.0, 1.0, 1.0)];
        let out = compute_volume_profile(&cells, (0, 0), 60_000, &params());
        assert!(out.buckets.is_empty());
        assert_eq!(out.coverage_pct, 0.0);
    }

    #[wasm_bindgen_test]
    fn cells_outside_range_excluded() {
        let cells = vec![
            cell(-1, 100.0, 5.0, 5.0), // before range
            cell(0, 100.0, 1.0, 2.0),
            cell(60_000, 100.0, 5.0, 5.0), // at exclusive upper bound
        ];
        let out = compute_volume_profile(&cells, (0, 60_000), 60_000, &params());
        assert_eq!(out.buckets.len(), 1);
        assert_eq!(out.buckets[0].total, 3.0);
        assert_eq!(out.buckets[0].delta, 1.0);
    }

    #[wasm_bindgen_test]
    fn single_bucket_poc_at_that_bucket() {
        let cells = vec![cell(0, 200.0, 3.0, 7.0)];
        let p = params();
        let out = compute_volume_profile(&cells, (0, 60_000), 60_000, &p);
        assert_eq!(out.buckets.len(), 1);
        let b = &out.buckets[0];
        assert_eq!(b.price_low, 200.0);
        assert!((b.price_high - (200.0 + p.bucket_dollars())).abs() < 1e-9);
        assert_eq!(b.total, 10.0);
        assert_eq!(b.delta, 4.0);
        assert_eq!(out.poc_price, Some(b.midpoint()));
        assert_eq!(out.vah_price, Some(b.price_high));
        assert_eq!(out.val_price, Some(b.price_low));
    }

    #[wasm_bindgen_test]
    fn poc_at_max_total_bucket() {
        // Three buckets, middle has most volume.
        let cells = vec![
            cell(0, 100.0, 1.0, 1.0),  // total 2
            cell(0, 110.0, 5.0, 5.0),  // total 10  ← POC
            cell(0, 120.0, 1.0, 2.0),  // total 3
        ];
        let out = compute_volume_profile(&cells, (0, 60_000), 60_000, &params());
        assert_eq!(out.buckets.len(), 3);
        let poc = out.poc_price.expect("poc");
        let mid_bucket_mid = out.buckets[1].midpoint();
        assert!((poc - mid_bucket_mid).abs() < 1e-9);
    }

    #[wasm_bindgen_test]
    fn value_area_expansion_70pct() {
        // Five buckets totalling 100. POC at idx 2 with vol 40. Target = 70.
        // buckets: 100→5, 110→10, 120→40 (POC), 130→30, 140→15
        // pair-above from POC: (30 + 15) = 45 ; pair-below: (10 + 5) = 15
        // Step 1: pair-above larger → consume bucket[3]; accum = 40 + 30 = 70.
        //   70 ≥ 70 → done. (Second consumption only fires if still under
        //   target, so bucket[4] is *not* eaten.)
        // VAH = top of bucket 3 = 130 + 10 = 140
        // VAL = bottom of bucket 2 = 120
        let cells = vec![
            cell(0, 100.0, 2.0, 3.0),
            cell(0, 110.0, 4.0, 6.0),
            cell(0, 120.0, 20.0, 20.0),
            cell(0, 130.0, 10.0, 20.0),
            cell(0, 140.0, 5.0, 10.0),
        ];
        let p = params(); // va_percent 70, bucket 100 ticks = $10
        let out = compute_volume_profile(&cells, (0, 60_000), 60_000, &p);
        assert_eq!(out.total_volume, 100.0);
        assert_eq!(out.poc_price, Some(125.0));
        assert_eq!(out.vah_price, Some(140.0));
        assert_eq!(out.val_price, Some(120.0));
    }

    #[wasm_bindgen_test]
    fn value_area_expands_both_sides_when_needed() {
        // Wider profile where the VA must reach both directions. Total 100,
        // POC at 60, target 70. After consuming the POC (60), need +10 more
        // — pair-above (3+1)=4 vs pair-below (5+2)=7 → expand down by 1
        // to bucket[1] (vol 5), accum 65 < 70, second consume → bucket[0]
        // (vol 2), accum 67 < 70. Next iter: only upward left,
        // pair-above (3+1)=4, consume bucket[3], accum 70 ≥ 70 → done.
        // VAH = top of bucket 3 = 130+10 = 140; VAL = bottom of bucket 0 = 100.
        let cells = vec![
            cell(0, 100.0, 1.0, 1.0),   // 2
            cell(0, 110.0, 2.5, 2.5),   // 5
            cell(0, 120.0, 30.0, 30.0), // 60 (POC)
            cell(0, 130.0, 1.5, 1.5),   // 3
            cell(0, 140.0, 0.5, 0.5),   // 1
            cell(0, 150.0, 14.5, 14.5), // 29 — far above; out of reach until pair sum favors it
        ];
        let out = compute_volume_profile(&cells, (0, 60_000), 60_000, &params());
        assert_eq!(out.total_volume, 100.0);
        assert_eq!(out.poc_price, Some(125.0));
        // VAL must extend below POC.
        assert!(out.val_price.unwrap() <= 120.0);
        // VAH must extend at least one bucket above POC.
        assert!(out.vah_price.unwrap() >= 140.0);
    }

    #[wasm_bindgen_test]
    fn coverage_fraction_reported() {
        // 5 bars in the window, cells touch 3 distinct open_times → 60% coverage.
        let tf = 60_000;
        let cells = vec![
            cell(0, 100.0, 1.0, 1.0),
            cell(tf, 100.0, 1.0, 1.0),
            cell(2 * tf, 100.0, 1.0, 1.0),
        ];
        let out = compute_volume_profile(&cells, (0, 5 * tf), tf, &params());
        assert!((out.coverage_pct - 0.6).abs() < 1e-6);
    }

    #[wasm_bindgen_test]
    fn coverage_caps_at_one() {
        // More distinct open_times than the window expects shouldn't go above 1.0.
        let tf = 60_000;
        let cells = (0..10)
            .map(|i| cell(i * tf, 100.0, 1.0, 1.0))
            .collect::<Vec<_>>();
        let out = compute_volume_profile(&cells, (0, 5 * tf), tf, &params());
        assert!((out.coverage_pct - 1.0).abs() < 1e-6);
    }

    #[wasm_bindgen_test]
    fn tied_pocs_resolve_to_higher_price() {
        let cells = vec![
            cell(0, 100.0, 5.0, 5.0), // total 10
            cell(0, 110.0, 5.0, 5.0), // total 10 — ties; should win
            cell(0, 120.0, 2.0, 2.0),
        ];
        let out = compute_volume_profile(&cells, (0, 60_000), 60_000, &params());
        assert_eq!(out.poc_price, Some(115.0)); // midpoint of bucket [110, 120)
    }
}
