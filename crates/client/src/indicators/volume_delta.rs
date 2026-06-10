//! Volume Delta + Cumulative Volume Delta (CVD). Per-bar delta is the
//! signed buy-vs-sell taker volume on each candle:
//!     delta = 2 * taker_buy_vol - volume
//! (taker_buy_vol = base-asset volume traded with the *taker on the buy
//! side*; sell-side aggression is `volume - taker_buy_vol`, so the signed
//! delta collapses to `2*taker_buy_vol - volume`.)
//!
//! Two render modes selectable in settings:
//!   * Histogram — per-bar signed bars, zero baseline
//!   * CVD       — running cumulative sum, drawn as a line
//!
//! Pane-only. The output shape is `IndicatorOutput::Macd` because that's
//! the existing output variant whose paint already draws "signed histogram
//! anchored at zero + an optional line on top" — reusing it keeps the new
//! kind a pure params/compute change with no paint-pipeline edits. Unused
//! series (e.g., histogram in CVD mode) are emitted as all-`None` and the
//! existing painters skip cleanly.

use std::any::Any;

use gpui::{SharedString, WeakEntity};
use serde::{Deserialize, Serialize};

use super::instance::InstanceId;
use super::kind::{ComputeCtx, IndicatorKind, PaneKind};
use super::output::{IndicatorOutput, ValueReadout};
use crate::panels::ContentPanel;
use crate::persistence::VolumeUnit;
use crate::services::market_data::Candle;
use crate::settings_form::{DropdownOption, Field, IndicatorTarget, SettingsForm, SettingsGroup};

/// Scale a delta value (`2*tbv - volume`) into the global volume unit.
/// USD multiplies by `c.close` so the histogram, y-range, and readout
/// stay in lockstep with the rest of the chart.
fn convert_delta(c: &Candle, raw_delta: f64, unit: VolumeUnit) -> f64 {
    match unit {
        VolumeUnit::Coin => raw_delta,
        VolumeUnit::Usd => raw_delta * c.close,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum VolumeDeltaMode {
    Histogram,
    Cvd,
}

impl Default for VolumeDeltaMode {
    fn default() -> Self {
        VolumeDeltaMode::Histogram
    }
}

impl VolumeDeltaMode {
    pub const ALL: &'static [VolumeDeltaMode] =
        &[VolumeDeltaMode::Histogram, VolumeDeltaMode::Cvd];

    pub fn label(self) -> &'static str {
        match self {
            VolumeDeltaMode::Histogram => "Histogram",
            VolumeDeltaMode::Cvd => "CVD",
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct VolumeDeltaParams {
    pub mode: VolumeDeltaMode,
}

impl VolumeDeltaParams {
    fn shows_histogram(&self) -> bool {
        matches!(self.mode, VolumeDeltaMode::Histogram)
    }
    fn shows_cvd(&self) -> bool {
        matches!(self.mode, VolumeDeltaMode::Cvd)
    }
}

impl IndicatorKind for VolumeDeltaParams {
    fn kind_id(&self) -> &'static str {
        "volume_delta"
    }

    fn pane_kind(&self) -> PaneKind {
        PaneKind::PaneOnly
    }

    fn label(&self) -> SharedString {
        match self.mode {
            VolumeDeltaMode::Histogram => "Volume Delta".into(),
            VolumeDeltaMode::Cvd => "CVD".into(),
        }
    }

    fn compute(&self, candles: &[Candle], ctx: ComputeCtx<'_>) -> IndicatorOutput {
        let n = candles.len();
        let none_series = vec![None; n];
        let unit = ctx.volume_unit;

        // Per-bar signed delta. `None` propagates when the source candle is
        // missing `taker_buy_vol` (e.g., an exchange that doesn't surface it
        // on the kline payload) — paint + readout already handle `None` cleanly.
        let delta: Vec<Option<f64>> = if self.shows_histogram() {
            candles
                .iter()
                .map(|c| {
                    c.taker_buy_vol
                        .map(|tbv| convert_delta(c, 2.0 * tbv - c.volume, unit))
                })
                .collect()
        } else {
            none_series.clone()
        };

        // Running cumulative delta. Anchored at the first bar with a defined
        // delta; gaps (None) hold the previous value rather than reset to 0,
        // so a transient missing field doesn't make the curve drop to baseline.
        let cvd: Vec<Option<f64>> = if self.shows_cvd() {
            let mut acc = 0.0_f64;
            let mut started = false;
            candles
                .iter()
                .map(|c| match c.taker_buy_vol {
                    Some(tbv) => {
                        acc += convert_delta(c, 2.0 * tbv - c.volume, unit);
                        started = true;
                        Some(acc)
                    }
                    None => started.then_some(acc),
                })
                .collect()
        } else {
            none_series
        };

        // Reuse the Macd shape: `histogram` = per-bar delta (sign-colored by
        // the paint pass), `macd` = CVD line, `signal` = unused (all None).
        IndicatorOutput::Macd {
            macd: cvd,
            signal: vec![None; n],
            histogram: delta,
        }
    }

    fn value_at(&self, output: &IndicatorOutput, index: usize) -> ValueReadout {
        let IndicatorOutput::Macd {
            macd, histogram, ..
        } = output
        else {
            return ValueReadout::One(None);
        };
        let cvd_v = macd.get(index).copied().flatten();
        let delta_v = histogram.get(index).copied().flatten();
        match self.mode {
            VolumeDeltaMode::Histogram => ValueReadout::One(delta_v),
            VolumeDeltaMode::Cvd => ValueReadout::One(cvd_v),
        }
    }

    fn y_range(
        &self,
        output: &IndicatorOutput,
        range: std::ops::Range<usize>,
    ) -> Option<(f64, f64)> {
        let IndicatorOutput::Macd {
            macd, histogram, ..
        } = output
        else {
            return None;
        };
        let len = macd.len();
        let lo_i = range.start.min(len);
        let hi_i = range.end.min(len);
        let mut lo_v = f64::INFINITY;
        let mut hi_v = f64::NEG_INFINITY;
        let mut any = false;
        let mut fold = |slice: &[Option<f64>]| {
            for v in slice[lo_i..hi_i].iter().filter_map(|v| *v) {
                if v < lo_v {
                    lo_v = v;
                }
                if v > hi_v {
                    hi_v = v;
                }
                any = true;
            }
        };
        if self.shows_histogram() {
            fold(histogram);
        }
        if self.shows_cvd() {
            fold(macd);
        }
        if !any {
            return None;
        }
        // Histogram modes anchor at zero so the sign is visually evident.
        // CVD-only floats freely — anchoring would crush the curve against
        // one edge when the cumulative is far from zero.
        if self.shows_histogram() {
            if lo_v > 0.0 {
                lo_v = 0.0;
            }
            if hi_v < 0.0 {
                hi_v = 0.0;
            }
        }
        Some((lo_v, hi_v))
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
        let target: IndicatorTarget<VolumeDeltaParams> = IndicatorTarget::new(panel, id);
        let form_id = SharedString::from(format!("volume-delta-{}", id));
        Some(SettingsForm::new(form_id).group(
            SettingsGroup::new("General").item(
                Field::dropdown(
                    "Mode",
                    vec![
                        DropdownOption::new("Histogram", "Histogram"),
                        DropdownOption::new("Cvd", "CVD"),
                    ],
                    target.getter(SharedString::from("Histogram"), |p: &VolumeDeltaParams| {
                        match p.mode {
                            VolumeDeltaMode::Histogram => SharedString::from("Histogram"),
                            VolumeDeltaMode::Cvd => SharedString::from("Cvd"),
                        }
                    }),
                    target.setter(|p: &mut VolumeDeltaParams, v: SharedString| {
                        p.mode = match v.as_ref() {
                            "Cvd" => VolumeDeltaMode::Cvd,
                            _ => VolumeDeltaMode::Histogram,
                        };
                    }),
                )
                .description("Histogram: per-bar signed delta. CVD: running cumulative line."),
            ),
        ))
    }
}
