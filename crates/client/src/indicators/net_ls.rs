//! Net Long/Short — sub-pane signed histogram of per-bar net positioning
//! *flow*, derived from taker delta + open-interest change. NOT a positioning
//! survey (that would be Binance's long/short ratio); this is the standard
//! orderflow "delta + OI" inference:
//!
//! | delta (aggressor) | ΔOI | reading              | sign |
//! |-------------------|-----|----------------------|------|
//! | buy  (+)          | ↑   | new longs opening    |  +   |
//! | sell (−)          | ↑   | new shorts opening   |  −   |
//! | buy  (+)          | ↓   | shorts covering      |  +   |
//! | sell (−)          | ↓   | longs exiting        |  −   |
//!
//! Per-bar value = `sign(delta) × |ΔOI|`: magnitude is the OI change, sign is
//! the aggressor. Flat OI (pure churn) reads ≈ 0. Magnitude follows the chart's
//! Coin/USD toggle (USD via the mark price, like the OI indicator), so the
//! histogram height is comparable to the OI-Δ row.
//!
//! Data source: `ComputeCtx.open_interest` (per-bar OHLC) + `candle.taker_buy_vol`
//! (delta) + `ComputeCtx.mark_price` (USD factor). It therefore gates the same
//! shared OI + mark-price subscriptions as the OI indicator.

use std::any::Any;

use gpui::{SharedString, WeakEntity};
use serde::{Deserialize, Serialize};

use super::instance::InstanceId;
use super::kind::{ComputeCtx, IndicatorKind, PaneKind, mark_close_series};
use super::output::{IndicatorOutput, Series, ValueReadout};
use crate::panels::ContentPanel;
use crate::persistence::VolumeUnit;
use crate::services::market_data::{Candle, MarkPriceBar, OpenInterestBar};
use crate::settings_form::SettingsForm;

/// Per-bar net-positioning flow aligned to `candles` (oldest-first). Shared by
/// the Net L/S indicator and the bar-stat Net L/S row. `None` where the bar has
/// no OI sample or no taker-delta. Two-pointer join of the sorted OI slice
/// against candle open_times (same shape as the OI indicator).
pub fn net_ls_series(
    candles: &[Candle],
    open_interest: Option<&[OpenInterestBar]>,
    mark_price: Option<&[MarkPriceBar]>,
    unit: VolumeUnit,
) -> Series {
    let n = candles.len();
    let mut out = vec![None; n];
    let Some(oi) = open_interest else {
        return out;
    };
    let mark_close = mark_close_series(candles, mark_price);
    let mut j = 0usize;
    for (i, c) in candles.iter().enumerate() {
        while j < oi.len() && oi[j].open_time < c.open_time {
            j += 1;
        }
        if j < oi.len() && oi[j].open_time == c.open_time {
            if let Some(tbv) = c.taker_buy_vol {
                let delta = 2.0 * tbv - c.volume;
                let d_oi = oi[j].close - oi[j].open;
                let mag = d_oi.abs()
                    * match unit {
                        VolumeUnit::Coin => 1.0,
                        VolumeUnit::Usd => mark_close[i].unwrap_or(c.close),
                    };
                out[i] = Some(if delta > 0.0 {
                    mag
                } else if delta < 0.0 {
                    -mag
                } else {
                    0.0
                });
            }
            j += 1;
        }
    }
    out
}

/// Per-instance params. Render is a fixed signed histogram (bull/bear by sign),
/// so there are no knobs yet — the struct exists for persistence parity.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct NetLsParams {}

impl NetLsParams {
    fn values_of(output: &IndicatorOutput) -> Option<&Series> {
        match output {
            IndicatorOutput::Histogram { values, .. } => Some(values),
            _ => None,
        }
    }
}

impl IndicatorKind for NetLsParams {
    fn kind_id(&self) -> &'static str {
        "net_ls"
    }

    fn pane_kind(&self) -> PaneKind {
        PaneKind::PaneOnly
    }

    fn label(&self) -> SharedString {
        "Net L/S".into()
    }

    fn compute(&self, candles: &[Candle], ctx: ComputeCtx<'_>) -> IndicatorOutput {
        let values = net_ls_series(candles, ctx.open_interest, ctx.mark_price, ctx.volume_unit);
        let up: Vec<bool> = values
            .iter()
            .map(|v| v.map(|x| x >= 0.0).unwrap_or(true))
            .collect();
        IndicatorOutput::Histogram { values, up }
    }

    fn value_at(&self, output: &IndicatorOutput, index: usize) -> ValueReadout {
        let v = Self::values_of(output).and_then(|s| s.get(index).copied().flatten());
        ValueReadout::One(v)
    }

    fn y_range(
        &self,
        output: &IndicatorOutput,
        range: std::ops::Range<usize>,
    ) -> Option<(f64, f64)> {
        let values = Self::values_of(output)?;
        let end = range.end.min(values.len());
        let start = range.start.min(end);
        let mut lo = f64::INFINITY;
        let mut hi = f64::NEG_INFINITY;
        for v in values.iter().take(end).skip(start).flatten() {
            lo = lo.min(*v);
            hi = hi.max(*v);
        }
        if !lo.is_finite() || !hi.is_finite() {
            return None;
        }
        // Straddles zero — always include the baseline so the split is clear.
        lo = lo.min(0.0);
        hi = hi.max(0.0);
        let pad = if hi > lo {
            (hi - lo) * 0.1
        } else {
            hi.abs() * 0.001 + 1.0
        };
        Some((lo - pad, hi + pad))
    }

    fn params_json(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or(serde_json::Value::Null)
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn settings_form(
        &self,
        _panel: WeakEntity<ContentPanel>,
        _id: InstanceId,
    ) -> Option<SettingsForm> {
        None
    }
}
