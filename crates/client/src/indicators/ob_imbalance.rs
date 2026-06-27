//! Order-book imbalance — sub-pane signed histogram of the resting-liquidity
//! skew within a depth band around mid. Positive (bid-heavy / support) tints
//! bullish, negative (ask-heavy / resistance) bearish. The value is the
//! normalized ratio `(BidVol − AskVol) / (BidVol + AskVol)` ∈ [−1, +1], summing
//! resting size (BTC) within `±depth%` of mid — scale-invariant, so the chart's
//! Coin/USD toggle doesn't change it.
//!
//! Data source: the chart's book time-series (the same one the heatmap replays —
//! server `book_snapshots` at 1-minute grain plus the live 1s sampler for the
//! forming tail), reduced to a compact per-snapshot [`BookImbalanceSample`] in
//! `panels.rs` and threaded through `ComputeCtx.book_imbalance`. Per bar we take
//! the **last snapshot in the bar** (book state at bar close). Bars with no
//! snapshot in range (sub-1m timeframes, or off the loaded book window) are
//! left blank.
//!
//! Depth is chosen from a fixed preset set ([`OB_DEPTHS_PCT`]) shared with the
//! bar-stat OB rows, so the (potentially deep) raw book is reduced exactly once
//! per snapshot for all consumers.

use std::any::Any;

use gpui::{SharedString, WeakEntity};
use serde::{Deserialize, Serialize};

use super::instance::InstanceId;
use super::kind::{ComputeCtx, IndicatorKind, PaneKind};
use super::output::{IndicatorOutput, Series, ValueReadout};
use crate::panels::ContentPanel;
use crate::services::market_data::{BookSnapshotEntry, Candle};
use crate::settings_form::{
    DropdownOption, Field, IndicatorTarget, SettingsForm, SettingsGroup,
};

/// Number of canonical depth presets.
pub const OB_N: usize = 5;

/// Canonical depth bands, as a percentage of mid price. **Ascending** — the
/// reducer relies on the ordering to accumulate cumulative band volume in a
/// single pass per side. The deepest (2%) stays well inside the server's
/// ±$5000 book band (~±5% at $100k), so every preset is fully captured.
pub const OB_DEPTHS_PCT: [f64; OB_N] = [0.1, 0.25, 0.5, 1.0, 2.0];

/// Compact per-snapshot imbalance, one ratio per [`OB_DEPTHS_PCT`] entry.
/// `f32::NAN` marks a depth with no resting size on either side (so the join
/// surfaces it as `None`). Reducing the deep book to this fixed-width row keeps
/// the chart-state cache cheap regardless of book depth.
#[derive(Clone, Copy, Debug)]
pub struct BookImbalanceSample {
    pub ts_ms: i64,
    pub imb: [f32; OB_N],
}

/// Human label for a depth preset (`0.1` → "0.1%", `1.0` → "1%").
pub fn ob_depth_label(d: f64) -> String {
    if d.fract() == 0.0 {
        format!("{}%", d as i64)
    } else {
        format!("{d}%")
    }
}

/// Reduce a book time-series (best-first per side) to per-snapshot imbalance at
/// every canonical depth. Single forward pass per side accumulating cumulative
/// band volume, snapshotting the running sum at each (ascending) depth
/// threshold. Snapshots with no top-of-book are dropped.
pub fn reduce_book_imbalance(series: &[BookSnapshotEntry]) -> Vec<BookImbalanceSample> {
    let mut out = Vec::with_capacity(series.len());
    for s in series {
        let (Some(best_bid), Some(best_ask)) = (s.bids.first(), s.asks.first()) else {
            continue;
        };
        if best_bid.price <= 0.0 || best_ask.price <= 0.0 {
            continue;
        }
        let mid = 0.5 * (best_bid.price + best_ask.price);
        let mut imb = [f32::NAN; OB_N];
        let mut bi = 0usize;
        let mut bacc = 0.0f64;
        let mut ai = 0usize;
        let mut aacc = 0.0f64;
        for (k, &d) in OB_DEPTHS_PCT.iter().enumerate() {
            let lo = mid * (1.0 - d / 100.0);
            let hi = mid * (1.0 + d / 100.0);
            while bi < s.bids.len() && s.bids[bi].price >= lo {
                bacc += s.bids[bi].size;
                bi += 1;
            }
            while ai < s.asks.len() && s.asks[ai].price <= hi {
                aacc += s.asks[ai].size;
                ai += 1;
            }
            let tot = bacc + aacc;
            if tot > 0.0 {
                imb[k] = ((bacc - aacc) / tot) as f32;
            }
        }
        out.push(BookImbalanceSample { ts_ms: s.ts_ms, imb });
    }
    out
}

/// Per-bar imbalance at `depth_idx`, aligned to `candles` (oldest-first), using
/// the **last sample within each bar** `[open_time, next_open)`. Bars with no
/// in-range sample stay `None`. Samples are ascending in `ts_ms`, so a single
/// forward scan keeps the last sample per bar.
pub fn book_imbalance_series(
    candles: &[Candle],
    samples: Option<&[BookImbalanceSample]>,
    depth_idx: usize,
) -> Series {
    let n = candles.len();
    let mut out = vec![None; n];
    let Some(samples) = samples else {
        return out;
    };
    if depth_idx >= OB_N {
        return out;
    }
    let mut si = 0usize;
    for i in 0..n {
        let open = candles[i].open_time;
        let next_open = candles.get(i + 1).map(|c| c.open_time).unwrap_or(i64::MAX);
        let mut chosen: Option<f64> = None;
        while si < samples.len() && samples[si].ts_ms < next_open {
            if samples[si].ts_ms >= open {
                let v = samples[si].imb[depth_idx];
                if v.is_finite() {
                    chosen = Some(v as f64);
                }
            }
            si += 1;
        }
        out[i] = chosen;
    }
    out
}

/// Per-instance params: which depth preset (index into [`OB_DEPTHS_PCT`]).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ObImbalanceParams {
    #[serde(default = "default_depth_idx")]
    pub depth_idx: usize,
}

fn default_depth_idx() -> usize {
    2 // 0.5%
}

impl Default for ObImbalanceParams {
    fn default() -> Self {
        Self {
            depth_idx: default_depth_idx(),
        }
    }
}

impl ObImbalanceParams {
    fn depth_idx(&self) -> usize {
        self.depth_idx.min(OB_N - 1)
    }

    fn values_of(output: &IndicatorOutput) -> Option<&Series> {
        match output {
            IndicatorOutput::Histogram { values, .. } => Some(values),
            _ => None,
        }
    }
}

impl IndicatorKind for ObImbalanceParams {
    fn kind_id(&self) -> &'static str {
        "ob_imbalance"
    }

    fn pane_kind(&self) -> PaneKind {
        PaneKind::PaneOnly
    }

    fn label(&self) -> SharedString {
        format!("OB Imbalance {}", ob_depth_label(OB_DEPTHS_PCT[self.depth_idx()])).into()
    }

    fn compute(&self, candles: &[Candle], ctx: ComputeCtx<'_>) -> IndicatorOutput {
        let values = book_imbalance_series(candles, ctx.book_imbalance, self.depth_idx());
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
        // Imbalance straddles zero — always include the baseline so the
        // histogram split is glance-able. Bounded to [-1, 1].
        lo = lo.min(0.0).max(-1.0);
        hi = hi.max(0.0).min(1.0);
        let pad = if hi > lo {
            (hi - lo) * 0.1
        } else {
            0.05
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
        panel: WeakEntity<ContentPanel>,
        id: InstanceId,
    ) -> Option<SettingsForm> {
        // Depth edits don't change the subscription need (one shared book sub
        // covers every depth), so the default no-op AfterChange is fine.
        let target: IndicatorTarget<ObImbalanceParams> = IndicatorTarget::new(panel.clone(), id);
        let form_id = SharedString::from(format!("ob-imbalance-{}", id));

        let depth_field = Field::dropdown(
            "Depth",
            OB_DEPTHS_PCT
                .iter()
                .enumerate()
                .map(|(i, d)| DropdownOption::new(i.to_string(), ob_depth_label(*d)))
                .collect(),
            target.getter(SharedString::from("2"), |p: &ObImbalanceParams| {
                SharedString::from(p.depth_idx().to_string())
            }),
            target.setter(|p: &mut ObImbalanceParams, v: SharedString| {
                if let Ok(idx) = v.as_ref().parse::<usize>() {
                    p.depth_idx = idx.min(OB_N - 1);
                }
            }),
        )
        .description("Band around mid (% of price) the imbalance is summed over.");

        Some(
            SettingsForm::new(form_id)
                .group(SettingsGroup::new("General").item(depth_field)),
        )
    }
}
