//! Open interest — sub-pane line (default) or candlesticks of the symbol's
//! total open interest. The line is a single color (color slot 0, like the
//! CVD line); candle mode draws per-bar OHLC tinted by the up/down colors.
//! The axis unit follows the chart's `VolumeUnit` toggle — Coin shows contracts,
//! USD shows `OI × candle close` (Binance's live OI endpoint has no USD
//! figure, so this is an approximation, consistent with the rest of the
//! chart's USD rendering).
//!
//! Data source: per-bar OHLC cells from the gateway's `OpenInterest { tf }`
//! subscription, threaded through `ComputeCtx`. The indicator carries no
//! params of its own beyond render style — the tf is the chart's tf, and the
//! sub refcounts on `(symbol, tf)` so all consumers share one wire sub.

use std::any::Any;

use gpui::{App, Hsla, SharedString, WeakEntity};
use gpui_component::ActiveTheme as _;
use serde::{Deserialize, Serialize};

use super::instance::InstanceId;
use super::kind::{ComputeCtx, IndicatorKind, PaneKind};
use super::output::{IndicatorOutput, Series, ValueReadout};
use crate::panels::ContentPanel;
use crate::persistence::VolumeUnit;
use crate::services::market_data::Candle;
use crate::settings_form::{
    DropdownOption, Field, IndicatorTarget, SettingsForm, SettingsGroup, inst_color_field,
};

/// Sub-pane render style.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum OiRenderMode {
    /// Single-color line of the per-bar close. Default.
    Line,
    /// Per-bar OHLC candlesticks of open interest.
    Candles,
}

impl Default for OiRenderMode {
    fn default() -> Self {
        OiRenderMode::Line
    }
}

/// Per-instance params. `up_color` / `down_color` override the rise/fall
/// colors used by both the line and candle renders; `None` falls back to the
/// theme's bullish / bearish chart colors.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct OpenInterestParams {
    #[serde(default)]
    pub render: OiRenderMode,
    #[serde(default)]
    pub up_color: Option<Hsla>,
    #[serde(default)]
    pub down_color: Option<Hsla>,
}

impl OpenInterestParams {
    /// USD conversion factor for bar `i` — the candle close, or `0.0` when
    /// the chart is in Coin mode / the bar has no candle. Centralises the
    /// "contracts × price" rule shared by `value_at` and the paint pass.
    fn unit_value(unit: VolumeUnit, raw: f64, price: Option<f64>) -> f64 {
        match unit {
            VolumeUnit::Coin => raw,
            VolumeUnit::Usd => raw * price.unwrap_or(0.0),
        }
    }
}

impl IndicatorKind for OpenInterestParams {
    fn kind_id(&self) -> &'static str {
        "open_interest"
    }

    fn pane_kind(&self) -> PaneKind {
        PaneKind::PaneOnly
    }

    fn label(&self) -> SharedString {
        "Open Interest".into()
    }

    fn compute(&self, candles: &[Candle], ctx: ComputeCtx<'_>) -> IndicatorOutput {
        let n = candles.len();
        let mut open: Series = vec![None; n];
        let mut high: Series = vec![None; n];
        let mut low: Series = vec![None; n];
        let mut close: Series = vec![None; n];
        // Per-bar candle close, used to convert contracts → USD at paint time
        // without re-threading the candle slice into the paint pass.
        let mut price: Series = vec![None; n];

        // Two-pointer join of the sorted OI-bar slice against candle
        // open_times — same shape as `liq_bars.rs::compute`.
        if let Some(bars) = ctx.open_interest {
            let mut j = 0usize;
            for (i, c) in candles.iter().enumerate() {
                while j < bars.len() && bars[j].open_time < c.open_time {
                    j += 1;
                }
                if j < bars.len() && bars[j].open_time == c.open_time {
                    let b = &bars[j];
                    open[i] = Some(b.open);
                    high[i] = Some(b.high);
                    low[i] = Some(b.low);
                    close[i] = Some(b.close);
                    price[i] = Some(c.close);
                    j += 1;
                }
            }
        }

        IndicatorOutput::OpenInterest {
            open,
            high,
            low,
            close,
            price,
            params: self.clone(),
            unit: ctx.volume_unit,
        }
    }

    fn value_at(&self, output: &IndicatorOutput, index: usize) -> ValueReadout {
        let IndicatorOutput::OpenInterest {
            close, price, unit, ..
        } = output
        else {
            return ValueReadout::One(None);
        };
        let v = close.get(index).copied().flatten().map(|c| {
            Self::unit_value(*unit, c, price.get(index).copied().flatten())
        });
        ValueReadout::One(v)
    }

    fn y_range(
        &self,
        output: &IndicatorOutput,
        range: std::ops::Range<usize>,
    ) -> Option<(f64, f64)> {
        let IndicatorOutput::OpenInterest {
            high, low, price, unit, ..
        } = output
        else {
            return None;
        };
        let end = range.end.min(low.len());
        let start = range.start.min(end);
        let mut lo = f64::INFINITY;
        let mut hi = f64::NEG_INFINITY;
        for i in start..end {
            let p = price.get(i).copied().flatten();
            if let Some(l) = low[i] {
                lo = lo.min(Self::unit_value(*unit, l, p));
            }
            if let Some(h) = high[i] {
                hi = hi.max(Self::unit_value(*unit, h, p));
            }
        }
        if !lo.is_finite() || !hi.is_finite() {
            return None;
        }
        // Pad so the line/candles don't kiss the pane edges. OI is a slow
        // series, so a tight fit would look flat; 8% headroom each side reads
        // the bar-to-bar moves clearly.
        let pad = if hi > lo {
            (hi - lo) * 0.08
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
        panel: WeakEntity<ContentPanel>,
        id: InstanceId,
    ) -> Option<SettingsForm> {
        // Render-mode edits don't change the subscription need (the sub is
        // gated on the indicator's presence, handled by the add/remove
        // refresh hooks), so the default no-op AfterChange is fine here.
        let target: IndicatorTarget<OpenInterestParams> = IndicatorTarget::new(panel.clone(), id);
        let form_id = SharedString::from(format!("open-interest-{}", id));

        let render_field = Field::dropdown(
            "Style",
            vec![
                DropdownOption::new("Line", "Line"),
                DropdownOption::new("Candles", "Candles"),
            ],
            target.getter(SharedString::from("Line"), |p: &OpenInterestParams| {
                match p.render {
                    OiRenderMode::Line => SharedString::from("Line"),
                    OiRenderMode::Candles => SharedString::from("Candles"),
                }
            }),
            target.setter(|p: &mut OpenInterestParams, v: SharedString| {
                p.render = match v.as_ref() {
                    "Candles" => OiRenderMode::Candles,
                    _ => OiRenderMode::Line,
                };
            }),
        )
        .description("Line draws a single-color OI close; Candles draws per-bar OHLC.");

        // Line mode: one color off slot 0 (same picker model as the CVD line).
        let is_line = target.clone();
        let line_color_field = inst_color_field("Line color", panel, id, 0)
            .description("Color of the OI line.")
            .visible_if(move |cx| {
                is_line
                    .read(cx, |p: &OpenInterestParams| matches!(p.render, OiRenderMode::Line))
                    .unwrap_or(true)
            });

        // Candle mode: rise/fall body colors. `None` falls back to the theme
        // bull/bear, so the swatch reflects the live default until overridden.
        let up_target = target.clone();
        let up_visible = target.clone();
        let up_color_field = Field::color(
            "Up color",
            move |cx: &App| {
                up_target
                    .read(cx, |p: &OpenInterestParams| p.up_color)
                    .flatten()
                    .unwrap_or_else(|| cx.theme().chart_bullish)
            },
            target.setter(|p: &mut OpenInterestParams, c: Hsla| p.up_color = Some(c)),
        )
        .description("Candle body color when OI rises bar-over-bar.")
        .visible_if(move |cx| {
            up_visible
                .read(cx, |p: &OpenInterestParams| matches!(p.render, OiRenderMode::Candles))
                .unwrap_or(false)
        });

        let down_target = target.clone();
        let down_visible = target.clone();
        let down_color_field = Field::color(
            "Down color",
            move |cx: &App| {
                down_target
                    .read(cx, |p: &OpenInterestParams| p.down_color)
                    .flatten()
                    .unwrap_or_else(|| cx.theme().chart_bearish)
            },
            target.setter(|p: &mut OpenInterestParams, c: Hsla| p.down_color = Some(c)),
        )
        .description("Candle body color when OI falls bar-over-bar.")
        .visible_if(move |cx| {
            down_visible
                .read(cx, |p: &OpenInterestParams| matches!(p.render, OiRenderMode::Candles))
                .unwrap_or(false)
        });

        Some(
            SettingsForm::new(form_id).group(
                SettingsGroup::new("General")
                    .item(render_field)
                    .item(line_color_field)
                    .item(up_color_field)
                    .item(down_color_field),
            ),
        )
    }
}
