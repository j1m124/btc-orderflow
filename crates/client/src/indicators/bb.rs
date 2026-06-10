//! Bollinger Bands. Middle = SMA(period, source); upper = middle + N⋅σ;
//! lower = middle − N⋅σ. Population stddev. Paint draws three lines and
//! optionally a low-alpha fill between upper and lower.

use gpui::{SharedString, WeakEntity};
use serde::{Deserialize, Serialize};

use super::instance::InstanceId;
use super::kind::{ComputeCtx, IndicatorKind, PaneKind, Source};
use super::math::{extract_source, rolling_sma, rolling_stddev};
use super::output::{IndicatorOutput, ValueReadout};
use crate::panels::ContentPanel;
use crate::services::market_data::Candle;
use crate::settings_form::{
    DropdownOption, Field, IndicatorTarget, NumberOpts, SettingsForm, SettingsGroup,
    inst_color_field,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BbParams {
    pub period: usize,
    pub stddev: f64,
    pub source: Source,
}

impl Default for BbParams {
    fn default() -> Self {
        Self {
            period: 20,
            stddev: 2.0,
            source: Source::Close,
        }
    }
}

impl IndicatorKind for BbParams {
    fn kind_id(&self) -> &'static str {
        "bb"
    }
    fn pane_kind(&self) -> PaneKind {
        PaneKind::OverlayOnly
    }
    fn label(&self) -> SharedString {
        // 0 → "BB(20, 2)"; non-integer → "BB(20, 2.5)". Matches TV's compact
        // form rather than always printing a trailing `.0`.
        let sd = if self.stddev.fract().abs() < f64::EPSILON {
            format!("{}", self.stddev as i64)
        } else {
            format!("{}", self.stddev)
        };
        format!("BB({}, {})", self.period, sd).into()
    }
    fn compute(&self, candles: &[Candle], _ctx: ComputeCtx) -> IndicatorOutput {
        let src = extract_source(candles, self.source);
        let middle = rolling_sma(&src, self.period);
        let sigma = rolling_stddev(&src, self.period);
        let n = src.len();
        let mut upper = vec![None; n];
        let mut lower = vec![None; n];
        for i in 0..n {
            if let (Some(m), Some(s)) = (middle[i], sigma[i]) {
                upper[i] = Some(m + self.stddev * s);
                lower[i] = Some(m - self.stddev * s);
            }
        }
        IndicatorOutput::Bands {
            upper,
            middle,
            lower,
        }
    }
    fn value_at(&self, output: &IndicatorOutput, index: usize) -> ValueReadout {
        match output {
            IndicatorOutput::Bands { upper, lower, .. } => ValueReadout::Two(
                upper.get(index).copied().flatten(),
                lower.get(index).copied().flatten(),
            ),
            _ => ValueReadout::Two(None, None),
        }
    }
    fn y_range(&self, output: &IndicatorOutput, range: std::ops::Range<usize>) -> Option<(f64, f64)> {
        let IndicatorOutput::Bands { upper, lower, .. } = output else {
            return None;
        };
        let lo_i = range.start.min(upper.len());
        let hi_i = range.end.min(upper.len());
        let mut min = f64::INFINITY;
        let mut max = f64::NEG_INFINITY;
        let mut any = false;
        for i in lo_i..hi_i {
            if let Some(u) = upper[i] {
                if u > max {
                    max = u;
                }
                any = true;
            }
            if let Some(l) = lower[i] {
                if l < min {
                    min = l;
                }
                any = true;
            }
        }
        any.then_some((min, max))
    }
    fn params_json(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or(serde_json::Value::Null)
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn settings_form(
        &self,
        panel: WeakEntity<ContentPanel>,
        id: InstanceId,
    ) -> Option<SettingsForm> {
        let target: IndicatorTarget<BbParams> = IndicatorTarget::new(panel.clone(), id);
        let form_id = SharedString::from(format!("bb-{}", id));
        let source_options: Vec<DropdownOption> = Source::ALL
            .iter()
            .map(|s| DropdownOption::new(source_value(*s), s.label()))
            .collect();
        Some(SettingsForm::new(form_id).group(
            SettingsGroup::new("General")
                .item(
                    Field::number(
                        "Period",
                        NumberOpts::int(2, 2000),
                        target.getter(20.0, |p: &BbParams| p.period as f64),
                        target.setter(|p: &mut BbParams, v: f64| {
                            p.period = v.round().clamp(2.0, 2000.0) as usize;
                        }),
                    )
                    .description("SMA window length for the middle band."),
                )
                .item(
                    Field::number(
                        "Std Dev",
                        NumberOpts::float(0.5, 5.0, 0.5),
                        target.getter(2.0, |p: &BbParams| p.stddev),
                        target.setter(|p: &mut BbParams, v: f64| {
                            p.stddev = v.clamp(0.5, 5.0);
                        }),
                    )
                    .description("Width of the upper/lower bands in σ."),
                )
                .item(Field::dropdown(
                    "Source",
                    source_options,
                    target.getter(SharedString::from("Close"), |p: &BbParams| {
                        SharedString::from(source_value(p.source))
                    }),
                    target.setter(|p: &mut BbParams, v: SharedString| {
                        if let Some(s) = source_from_value(v.as_ref()) {
                            p.source = s;
                        }
                    }),
                ))
                .item(inst_color_field("Line color", panel, id, 0)),
        ))
    }
}

fn source_value(s: Source) -> &'static str {
    match s {
        Source::Close => "Close",
        Source::Open => "Open",
        Source::High => "High",
        Source::Low => "Low",
        Source::Hl2 => "Hl2",
        Source::Ohlc4 => "Ohlc4",
    }
}

fn source_from_value(s: &str) -> Option<Source> {
    Some(match s {
        "Close" => Source::Close,
        "Open" => Source::Open,
        "High" => Source::High,
        "Low" => Source::Low,
        "Hl2" => Source::Hl2,
        "Ohlc4" => Source::Ohlc4,
        _ => return None,
    })
}
