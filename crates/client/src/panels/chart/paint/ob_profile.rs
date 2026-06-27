//! Orderbook Profile render — the live order book drawn as right-anchored
//! horizontal bars on the chart's price axis, painted *in front* of candles
//! (under overlay indicators + drawings). The façade indicator
//! [`crate::indicators::ob_profile`] owns the params; this module reads the live
//! book snapshot fresh from the market-data service each frame and bins it onto
//! the visible price band.
//!
//! No texture / cache (unlike the heatmaps): the live snapshot is cheap to
//! bucketize over just the visible band, so each frame re-bins straight into
//! quads. Each side (bids below mid, asks above) scales to its own largest
//! visible bucket and shares the right-edge anchor; the two never overlap in
//! price, so a single anchor + band reads cleanly.

use std::collections::BTreeMap;

use gpui::{App, Hsla, Pixels, Point, SharedString, Window};

use super::super::price_to_screen;
use super::fill_rect;
use super::heatmap::HEATMAP_DEPTH;
use crate::persistence::VolumeUnit;
use crate::services::market_data::MarketDataServiceHandle;

/// Captured params for the orderbook-profile paint pass. Cheap to clone into the
/// chart's `'static` paint closure (a symbol handle + a few scalars + 2 colors);
/// the book itself is read fresh from the service inside the closure.
#[derive(Clone)]
pub struct ObProfilePaintParams {
    pub symbol: SharedString,
    /// Price-bucket width in dollars.
    pub bucket_w: f64,
    /// Longest bar's width as a fraction of the plot width.
    pub width_frac: f32,
    pub color_bid: Hsla,
    pub color_ask: Hsla,
}

/// Coin → display magnitude for a bucket: weights by the bucket mid price when
/// the chart's unit is USD, else the raw coin quantity.
#[inline]
fn bucket_mag(usd: bool, w: f64, k: i64, qty: f64) -> f64 {
    if usd {
        qty * ((k as f64 + 0.5) * w)
    } else {
        qty
    }
}

/// Paint the live order book as right-anchored horizontal bars on the price
/// axis. Reads `book_snapshot` fresh from the service, drops crossed levels,
/// bins each side onto buckets overlapping the visible price band, and scales
/// each side independently to its own largest visible bucket.
#[allow(clippy::too_many_arguments)]
pub fn paint_ob_profile(
    params: &ObProfilePaintParams,
    origin: Point<Pixels>,
    canvas_w: f32,
    canvas_h: f32,
    y_axis_gap: f32,
    y_lo: f64,
    y_hi: f64,
    volume_unit: VolumeUnit,
    window: &mut Window,
    cx: &mut App,
) {
    let w = params.bucket_w;
    if w <= 0.0 || y_hi <= y_lo || canvas_w <= 0.0 {
        return;
    }
    // Visible-bucket index span: bucket k spans price [k·w, (k+1)·w).
    let k_min = (y_lo / w).floor() as i64;
    let k_max = (y_hi / w).floor() as i64;

    // Read the live book and bin it — borrowing the service only for this block
    // (the BTreeMaps own f64s, so the borrow ends before we paint).
    let service = cx.global::<MarketDataServiceHandle>().0.clone();
    let mut bid_buckets: BTreeMap<i64, f64> = BTreeMap::new();
    let mut ask_buckets: BTreeMap<i64, f64> = BTreeMap::new();
    {
        let svc = service.read(cx);
        let Some((bids, asks)) = svc.book_snapshot(params.symbol.as_ref(), HEATMAP_DEPTH) else {
            return;
        };
        // Best-first ordering (bids descending, asks ascending) lets us drop
        // crossed levels against the opposing best and stop the scan once we
        // leave the visible band.
        let best_bid = bids.first().map(|l| l.price);
        let best_ask = asks.first().map(|l| l.price);
        for l in bids {
            if let Some(a) = best_ask {
                if l.price >= a {
                    continue; // crossed — a bid at/above best ask
                }
            }
            let k = (l.price / w).floor() as i64;
            if k > k_max {
                continue; // above the visible band (near best bid)
            }
            if k < k_min {
                break; // descending — everything past here is below the band
            }
            *bid_buckets.entry(k).or_insert(0.0) += l.size;
        }
        for l in asks {
            if let Some(b) = best_bid {
                if l.price <= b {
                    continue; // crossed — an ask at/below best bid
                }
            }
            let k = (l.price / w).floor() as i64;
            if k < k_min {
                continue; // below the visible band (near best ask)
            }
            if k > k_max {
                break; // ascending — everything past here is above the band
            }
            *ask_buckets.entry(k).or_insert(0.0) += l.size;
        }
    }

    if bid_buckets.is_empty() && ask_buckets.is_empty() {
        return;
    }

    let usd = matches!(volume_unit, VolumeUnit::Usd);
    let side_max = |buckets: &BTreeMap<i64, f64>| -> f64 {
        buckets
            .iter()
            .map(|(&k, &q)| bucket_mag(usd, w, k, q))
            .fold(0.0_f64, f64::max)
    };
    let max_bid = side_max(&bid_buckets);
    let max_ask = side_max(&ask_buckets);

    // Anchor at the plot's right edge (price axis excluded) so bars never paint
    // over the y-axis labels.
    let anchor_x = (canvas_w - y_axis_gap).max(1.0);
    let band_w = (anchor_x * params.width_frac.clamp(0.0, 1.0)).max(1.0);

    paint_side(
        &bid_buckets,
        max_bid,
        usd,
        w,
        anchor_x,
        band_w,
        y_lo,
        y_hi,
        canvas_h,
        params.color_bid,
        origin,
        window,
    );
    paint_side(
        &ask_buckets,
        max_ask,
        usd,
        w,
        anchor_x,
        band_w,
        y_lo,
        y_hi,
        canvas_h,
        params.color_ask,
        origin,
        window,
    );
}

/// Draw one side's bucket bars, right-anchored at `anchor_x`, each scaled to
/// `max` (the side's largest visible bucket).
#[allow(clippy::too_many_arguments)]
fn paint_side(
    buckets: &BTreeMap<i64, f64>,
    max: f64,
    usd: bool,
    w: f64,
    anchor_x: f32,
    band_w: f32,
    y_lo: f64,
    y_hi: f64,
    canvas_h: f32,
    color: Hsla,
    origin: Point<Pixels>,
    window: &mut Window,
) {
    if max <= 0.0 {
        return;
    }
    for (&k, &qty) in buckets {
        let m = bucket_mag(usd, w, k, qty);
        if m <= 0.0 {
            continue;
        }
        let bp_lo = k as f64 * w;
        let bp_hi = bp_lo + w;
        // Higher price → smaller y, so `bp_hi` is the top edge.
        let y_top = price_to_screen(y_lo, y_hi, bp_hi, canvas_h);
        let y_bot = price_to_screen(y_lo, y_hi, bp_lo, canvas_h);
        let h = y_bot - y_top;
        if h <= 0.0 {
            continue;
        }
        let frac = ((m / max) as f32).clamp(0.0, 1.0);
        let bar_w = (band_w * frac).max(1.0);
        fill_rect(window, origin, anchor_x - bar_w, bar_w, y_top, h, color);
    }
}
