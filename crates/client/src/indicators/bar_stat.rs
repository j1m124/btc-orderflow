//! Bar statistic row: per-bar total volume + signed delta rendered as a
//! two-line text cell, with optional heatmap-style color grading.
//!
//! Grading modes (paint-time decision; compute always emits the full data
//! set so a mode flip is a pure paint refresh):
//!   * Off          — no fill, text on neutral background
//!   * Bar          — full-saturation bull/bear fill per cell, based on the
//!                    candle's own sign (volume → up/down candle; delta →
//!                    sign of delta itself)
//!   * VisibleRange — fill intensity = `|v| / max(|v|)` across the visible
//!                    bar slice (computed in the paint pass, no compute cost)
//!   * Daily        — fill intensity = `|v| / daily_max_for_that_bar`, where
//!                    `daily_max_*` is a per-bar rolling 24h max precomputed
//!                    here in `compute()` so paint stays cheap
//!
//! Volume unit (Coin vs USD) is read off `ComputeCtx.volume_unit` to match
//! the rest of the chart's header toggle.

use std::any::Any;

use gpui::SharedString;
use serde::{Deserialize, Serialize};

use super::kind::{ComputeCtx, IndicatorKind, PaneKind};
use super::output::{IndicatorOutput, Series, ValueReadout};
use crate::persistence::VolumeUnit;
use crate::services::market_data::Candle;

const DAY_MS: i64 = 24 * 3600 * 1000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BarStatGrade {
    Off,
    Bar,
    VisibleRange,
    Daily,
}

impl Default for BarStatGrade {
    fn default() -> Self {
        BarStatGrade::VisibleRange
    }
}

impl BarStatGrade {
    pub const ALL: &'static [BarStatGrade] = &[
        BarStatGrade::Off,
        BarStatGrade::Bar,
        BarStatGrade::VisibleRange,
        BarStatGrade::Daily,
    ];

    pub fn label(self) -> &'static str {
        match self {
            BarStatGrade::Off => "Off",
            BarStatGrade::Bar => "Per-bar",
            BarStatGrade::VisibleRange => "Visible range",
            BarStatGrade::Daily => "Daily",
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct BarStatParams {
    pub grade: BarStatGrade,
}

impl IndicatorKind for BarStatParams {
    fn kind_id(&self) -> &'static str {
        "bar_stat"
    }

    fn pane_kind(&self) -> PaneKind {
        PaneKind::PaneOnly
    }

    fn label(&self) -> SharedString {
        "Bar Stats".into()
    }

    fn compute(&self, candles: &[Candle], ctx: ComputeCtx) -> IndicatorOutput {
        let unit = ctx.volume_unit;
        let n = candles.len();
        let mut volume: Series = Vec::with_capacity(n);
        let mut delta: Series = Vec::with_capacity(n);

        for c in candles {
            volume.push(Some(scale_vol(c.volume, c, unit)));
            delta.push(c.taker_buy_vol.map(|tbv| scale_vol(2.0 * tbv - c.volume, c, unit)));
        }

        let times: Vec<i64> = candles.iter().map(|c| c.open_time).collect();
        let daily_max_vol = rolling_daily_max_abs(&volume, &times);
        let daily_max_delta = rolling_daily_max_abs(&delta, &times);

        IndicatorOutput::BarStat {
            grade: self.grade,
            volume,
            delta,
            daily_max_vol,
            daily_max_delta,
        }
    }

    fn value_at(&self, output: &IndicatorOutput, index: usize) -> ValueReadout {
        let IndicatorOutput::BarStat { volume, delta, .. } = output else {
            return ValueReadout::Two(None, None);
        };
        ValueReadout::Two(
            volume.get(index).copied().flatten(),
            delta.get(index).copied().flatten(),
        )
    }

    fn y_range(
        &self,
        _output: &IndicatorOutput,
        _range: std::ops::Range<usize>,
    ) -> Option<(f64, f64)> {
        // The pane renders fixed-position text cells, not a scalar series
        // against a price axis — paint owns layout entirely. Return a
        // dummy [0, 1] so the auto-fit code keeps the slot visible
        // (PanePaintItem treats `None` as "no data this frame" and
        // hides the pane).
        Some((0.0, 1.0))
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

    fn color_slots(&self) -> Vec<SharedString> {
        // Bull/bear colors come from the theme; no user-configurable slot.
        Vec::new()
    }
}

fn scale_vol(raw: f64, c: &Candle, unit: VolumeUnit) -> f64 {
    match unit {
        VolumeUnit::Coin => raw,
        VolumeUnit::Usd => raw * c.close,
    }
}

/// Per-bar rolling max of `|v|` over the trailing 24-hour window keyed by
/// `open_time` (ms epoch). O(n × window) — typically a few thousand bars
/// times a window of <= 1440 (1-minute timeframe), runs once per recompute.
fn rolling_daily_max_abs(values: &[Option<f64>], times_ms: &[i64]) -> Series {
    let n = values.len();
    let mut out: Series = vec![None; n];
    if n == 0 {
        return out;
    }
    for i in 0..n {
        let t_now = times_ms[i];
        let cutoff = t_now - DAY_MS;
        let mut mx = 0.0_f64;
        let mut any = false;
        for j in (0..=i).rev() {
            if times_ms[j] < cutoff {
                break;
            }
            if let Some(v) = values[j] {
                let av = v.abs();
                if av > mx {
                    mx = av;
                }
                any = true;
            }
        }
        if any {
            out[i] = Some(mx);
        }
    }
    out
}
