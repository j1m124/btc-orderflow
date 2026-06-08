//! Output of [`super::compute_volume_profile`] — what the painter consumes.
//!
//! Buckets are sorted by `price_low` ascending. POC/VAH/VAL are `Option`
//! because they only materialize once footprint coverage of the requested
//! time range is high enough (the painter gates rendering on
//! `coverage_pct ≥ 95` per the design grilling).

/// One price-bucket aggregated across the profile's time range.
#[derive(Clone, Copy, Debug, Default)]
pub struct VpBucket {
    /// Inclusive low edge of the bucket in quote currency.
    pub price_low: f64,
    /// Exclusive high edge of the bucket. `price_high - price_low ==
    /// VolumeProfileParams::bucket_dollars()` for every bucket the
    /// aggregator emits.
    pub price_high: f64,
    /// Sum of `bid_vol + ask_vol` for all `FootprintCell`s that landed in
    /// this bucket across the range.
    pub total: f64,
    /// Sum of `ask_vol - bid_vol`. Positive = net buy-side aggression at
    /// this price across the range.
    pub delta: f64,
}

impl VpBucket {
    /// Midpoint price — used for POC labeling and for the reference-line
    /// y-position when rendering POC.
    pub fn midpoint(&self) -> f64 {
        0.5 * (self.price_low + self.price_high)
    }
}

#[derive(Clone, Debug, Default)]
pub struct VolumeProfileOutput {
    /// Sorted by `price_low` ascending. Empty if the range had no
    /// footprint coverage at all.
    pub buckets: Vec<VpBucket>,
    /// Price of the bucket with highest `total` (midpoint). `None` when
    /// `buckets` is empty or coverage hasn't crossed the render threshold.
    pub poc_price: Option<f64>,
    /// Value-area high — top edge of the highest bucket included in the
    /// Steidlmayer expansion.
    pub vah_price: Option<f64>,
    /// Value-area low — bottom edge of the lowest included bucket.
    pub val_price: Option<f64>,
    /// Sum of every bucket's `total`. Used for the per-row delta scaling
    /// denominator and for the value-area target (`va_percent × total`).
    pub total_volume: f64,
    /// Fraction of the requested bar range that has at least one footprint
    /// cell. `0.0` = no data; `1.0` = every bar covered. The painter
    /// suppresses POC/VAH/VAL when this is below 0.95.
    pub coverage_pct: f32,
}
