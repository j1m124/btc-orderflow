//! Funding rate — sub-pane histogram (default) or line of the perpetual's
//! funding rate. Positive funding (longs pay shorts) tints bullish; negative
//! (shorts pay longs) tints bearish. Values are rendered as a **percent**
//! (rate × 100, e.g. `0.0100` = a 0.01% rate) so the axis/readout numbers are
//! human-friendly.
//!
//! Data source: the per-bar `funding_rate` carried on the gateway's
//! `MarkPrice { tf }` subscription, threaded through `ComputeCtx`. The value is
//! the live *predicted* funding where the server has captured it, falling back
//! to the *settled* 8h rate for historical bars (the server COALESCEs the two);
//! bars between settlements before the live curve started are `None` and the
//! pane simply leaves a gap. The indicator carries no params beyond render
//! style — the tf is the chart's tf, and the mark-price sub refcounts on
//! `(symbol, tf)` so all consumers share one wire sub.

use std::any::Any;

use gpui::{SharedString, WeakEntity};
use serde::{Deserialize, Serialize};

use super::instance::InstanceId;
use super::kind::{ComputeCtx, IndicatorKind, PaneKind};
use super::output::{IndicatorOutput, Series, ValueReadout};
use crate::panels::ContentPanel;
use crate::services::market_data::Candle;
use crate::settings_form::{
    DropdownOption, Field, IndicatorTarget, SettingsForm, SettingsGroup, inst_color_field,
};

/// Sub-pane render style.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FundingRenderMode {
    /// Signed histogram around the zero line (default).
    Histogram,
    /// Single-color line.
    Line,
}

impl Default for FundingRenderMode {
    fn default() -> Self {
        FundingRenderMode::Histogram
    }
}

/// Per-instance params. Render style only — histogram colors come from the
/// theme bull/bear (matching volume-as-pane / liq bars); the line color is the
/// instance's slot-0 color.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct FundingParams {
    #[serde(default)]
    pub render: FundingRenderMode,
}

impl FundingParams {
    /// Pull the funding series (in percent) from whichever output variant
    /// `compute` emitted, so `value_at` / `y_range` work for both render modes.
    fn values_of(output: &IndicatorOutput) -> Option<&Series> {
        match output {
            IndicatorOutput::Histogram { values, .. } => Some(values),
            IndicatorOutput::Line(values) => Some(values),
            _ => None,
        }
    }
}

impl IndicatorKind for FundingParams {
    fn kind_id(&self) -> &'static str {
        "funding"
    }

    fn pane_kind(&self) -> PaneKind {
        PaneKind::PaneOnly
    }

    fn label(&self) -> SharedString {
        "Funding".into()
    }

    fn compute(&self, candles: &[Candle], ctx: ComputeCtx<'_>) -> IndicatorOutput {
        let n = candles.len();
        let mut values: Series = vec![None; n];

        // Two-pointer join of the sorted mark-price slice against candle
        // open_times, pulling the per-bar funding (as percent).
        if let Some(bars) = ctx.mark_price {
            let mut j = 0usize;
            for (i, c) in candles.iter().enumerate() {
                while j < bars.len() && bars[j].open_time < c.open_time {
                    j += 1;
                }
                if j < bars.len() && bars[j].open_time == c.open_time {
                    if let Some(r) = bars[j].funding_rate {
                        values[i] = Some(r * 100.0);
                    }
                    j += 1;
                }
            }
        }

        match self.render {
            FundingRenderMode::Histogram => {
                let up: Vec<bool> = values
                    .iter()
                    .map(|v| v.map(|x| x >= 0.0).unwrap_or(true))
                    .collect();
                IndicatorOutput::Histogram { values, up }
            }
            FundingRenderMode::Line => IndicatorOutput::Line(values),
        }
    }

    fn value_at(&self, output: &IndicatorOutput, index: usize) -> ValueReadout {
        let v = Self::values_of(output)
            .and_then(|s| s.get(index).copied().flatten());
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
        // Funding straddles zero — always include the baseline so the
        // histogram split / zero line is glance-able.
        lo = lo.min(0.0);
        hi = hi.max(0.0);
        let pad = if hi > lo {
            (hi - lo) * 0.1
        } else {
            hi.abs() * 0.001 + 0.001
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
        // Render-mode edits don't change the subscription need (gated on the
        // indicator's presence by the add/remove refresh hooks), so the default
        // no-op AfterChange is fine.
        let target: IndicatorTarget<FundingParams> = IndicatorTarget::new(panel.clone(), id);
        let form_id = SharedString::from(format!("funding-{}", id));

        let render_field = Field::dropdown(
            "Style",
            vec![
                DropdownOption::new("Histogram", "Histogram"),
                DropdownOption::new("Line", "Line"),
            ],
            target.getter(SharedString::from("Histogram"), |p: &FundingParams| {
                match p.render {
                    FundingRenderMode::Histogram => SharedString::from("Histogram"),
                    FundingRenderMode::Line => SharedString::from("Line"),
                }
            }),
            target.setter(|p: &mut FundingParams, v: SharedString| {
                p.render = match v.as_ref() {
                    "Line" => FundingRenderMode::Line,
                    _ => FundingRenderMode::Histogram,
                };
            }),
        )
        .description("Histogram tints bull/bear by sign; Line draws a single-color rate.");

        // Line mode: one color off slot 0 (same picker model as the OI line).
        let is_line = target.clone();
        let line_color_field = inst_color_field("Line color", panel, id, 0)
            .description("Color of the funding line.")
            .visible_if(move |cx| {
                is_line
                    .read(cx, |p: &FundingParams| matches!(p.render, FundingRenderMode::Line))
                    .unwrap_or(false)
            });

        Some(
            SettingsForm::new(form_id).group(
                SettingsGroup::new("General")
                    .item(render_field)
                    .item(line_color_field),
            ),
        )
    }
}
