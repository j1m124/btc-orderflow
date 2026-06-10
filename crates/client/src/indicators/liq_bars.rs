//! Liquidation bars — sub-pane two-sided histogram. Long-liq (forced sells)
//! plot downward in bearish red; short-liq (forced buys) plot upward in
//! bullish green. Bar height ∝ qty or USD notional, decided by the chart's
//! `VolumeUnit` toggle (Coin/USD) — same dropdown that drives Volume /
//! Volume Delta / CVD, so flipping the unit re-renders without re-sub.
//!
//! Data source: per-bar aggregation cells from the gateway's
//! `LiquidationBars { tf }` subscription, threaded through `ComputeCtx`. The
//! indicator carries no bucket/window params of its own — the tf is the
//! chart's tf, and the sub-management refcounts on `(symbol, tf)` so all
//! `liq_bars` instances on the same chart share one wire subscription.

use std::any::Any;

use gpui::{Hsla, SharedString, WeakEntity};
use serde::{Deserialize, Serialize};

use super::instance::InstanceId;
use super::kind::{ComputeCtx, IndicatorKind, PaneKind};
use super::output::{IndicatorOutput, ValueReadout};
use crate::panels::ContentPanel;
use crate::persistence::VolumeUnit;
use crate::services::market_data::Candle;
use crate::settings_form::{
    DropdownOption, Field, IndicatorTarget, NumberOpts, SettingsForm, SettingsGroup,
};

/// Sub-pane y-axis scaling mode.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum LiqBarsScale {
    /// Fit to visible bars: y-range = (-max_long, +max_short). Default.
    Auto,
    /// Hard cap shared across both sides — useful for comparing absolute
    /// magnitudes across charts. Unit matches the chart's `VolumeUnit`.
    Fixed { max: f64 },
}

impl Default for LiqBarsScale {
    fn default() -> Self {
        LiqBarsScale::Auto
    }
}

/// Per-instance params. Color overrides + cumulative-line overlay are
/// deferred to Phase 11 — v1 reads bull/bear straight off the theme.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct LiquidationBarsParams {
    #[serde(default)]
    pub scale: LiqBarsScale,
    /// Optional long-liq color override (deferred — no UI in v1).
    #[serde(default)]
    pub long_color: Option<Hsla>,
    /// Optional short-liq color override (deferred — no UI in v1).
    #[serde(default)]
    pub short_color: Option<Hsla>,
    /// Show running net (short_quote_qty − long_quote_qty) overlay line.
    /// Deferred from v1; the paint arm reads this flag but only draws when
    /// it's true, and the settings UI has no toggle yet (Phase 11).
    #[serde(default)]
    pub show_cumulative: bool,
    #[serde(default)]
    pub cumulative_color: Option<Hsla>,
}

impl IndicatorKind for LiquidationBarsParams {
    fn kind_id(&self) -> &'static str {
        "liq_bars"
    }

    fn pane_kind(&self) -> PaneKind {
        PaneKind::PaneOnly
    }

    fn label(&self) -> SharedString {
        "Liq Bars".into()
    }

    fn compute(&self, candles: &[Candle], ctx: ComputeCtx<'_>) -> IndicatorOutput {
        let n = candles.len();
        let mut long_qty: Vec<Option<f64>> = vec![None; n];
        let mut long_quote: Vec<Option<f64>> = vec![None; n];
        let mut short_qty: Vec<Option<f64>> = vec![None; n];
        let mut short_quote: Vec<Option<f64>> = vec![None; n];

        // Join the sorted liq-bar slice against candle `open_time` via a
        // two-pointer walk. Both are sorted ascending; total cost O(n + m).
        if let Some(bars) = ctx.liquidation_bars {
            let mut j = 0usize;
            for (i, c) in candles.iter().enumerate() {
                while j < bars.len() && bars[j].open_time < c.open_time {
                    j += 1;
                }
                if j < bars.len() && bars[j].open_time == c.open_time {
                    let b = &bars[j];
                    long_qty[i] = Some(b.long_qty);
                    long_quote[i] = Some(b.long_quote_qty);
                    short_qty[i] = Some(b.short_qty);
                    short_quote[i] = Some(b.short_quote_qty);
                    j += 1;
                }
            }
        }

        IndicatorOutput::LiquidationBars {
            long_qty,
            long_quote_qty: long_quote,
            short_qty,
            short_quote_qty: short_quote,
            params: self.clone(),
            unit: ctx.volume_unit,
        }
    }

    fn value_at(&self, output: &IndicatorOutput, index: usize) -> ValueReadout {
        let IndicatorOutput::LiquidationBars {
            long_qty,
            long_quote_qty,
            short_qty,
            short_quote_qty,
            unit,
            ..
        } = output
        else {
            return ValueReadout::Two(None, None);
        };
        let (longs, shorts) = match unit {
            VolumeUnit::Coin => (long_qty, short_qty),
            VolumeUnit::Usd => (long_quote_qty, short_quote_qty),
        };
        ValueReadout::Two(
            longs.get(index).copied().flatten(),
            shorts.get(index).copied().flatten(),
        )
    }

    fn y_range(
        &self,
        output: &IndicatorOutput,
        range: std::ops::Range<usize>,
    ) -> Option<(f64, f64)> {
        let IndicatorOutput::LiquidationBars {
            long_qty,
            long_quote_qty,
            short_qty,
            short_quote_qty,
            params,
            unit,
        } = output
        else {
            return None;
        };
        // Fixed scale: use the configured cap on both sides regardless of
        // visible-bar magnitudes. Yields a stable y-axis that doesn't jump
        // as the user pans.
        if let LiqBarsScale::Fixed { max } = params.scale {
            if max > 0.0 {
                return Some((-max, max));
            }
        }
        let (longs, shorts) = match unit {
            VolumeUnit::Coin => (long_qty, short_qty),
            VolumeUnit::Usd => (long_quote_qty, short_quote_qty),
        };
        let end = range.end.min(longs.len());
        let slice_long = &longs[range.start.min(end)..end];
        let slice_short = &shorts[range.start.min(end)..end];
        let max_long = slice_long
            .iter()
            .filter_map(|v| *v)
            .fold(0.0_f64, f64::max);
        let max_short = slice_short
            .iter()
            .filter_map(|v| *v)
            .fold(0.0_f64, f64::max);
        if max_long == 0.0 && max_short == 0.0 {
            // Stable, non-degenerate range so the sub-pane still draws axes.
            return Some((-1.0, 1.0));
        }
        Some((-max_long.max(1e-9), max_short.max(1e-9)))
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
        let target: IndicatorTarget<LiquidationBarsParams> = IndicatorTarget::new(panel, id);
        let form_id = SharedString::from(format!("liq-bars-{}", id));

        let is_fixed_for_field = target.clone();

        let scale_field = Field::dropdown(
            "Scale",
            vec![
                DropdownOption::new("Auto", "Auto fit"),
                DropdownOption::new("Fixed", "Fixed cap"),
            ],
            target.getter(SharedString::from("Auto"), |p: &LiquidationBarsParams| {
                match p.scale {
                    LiqBarsScale::Auto => SharedString::from("Auto"),
                    LiqBarsScale::Fixed { .. } => SharedString::from("Fixed"),
                }
            }),
            target.setter(|p: &mut LiquidationBarsParams, v: SharedString| {
                p.scale = match v.as_ref() {
                    "Fixed" => {
                        let prev_cap = match p.scale {
                            LiqBarsScale::Fixed { max } => max,
                            _ => 0.0,
                        };
                        LiqBarsScale::Fixed { max: prev_cap }
                    }
                    _ => LiqBarsScale::Auto,
                };
            }),
        )
        .description("Auto fits to the visible bars; Fixed locks a symmetric y-range.");

        let max_field = Field::number(
            "Fixed cap",
            NumberOpts::float(0.0, 1.0e12, 1.0),
            target.getter(0.0, |p: &LiquidationBarsParams| match p.scale {
                LiqBarsScale::Fixed { max } => max,
                _ => 0.0,
            }),
            target.setter(|p: &mut LiquidationBarsParams, v: f64| {
                if let LiqBarsScale::Fixed { max } = &mut p.scale {
                    *max = v.max(0.0);
                }
            }),
        )
        .description("Symmetric y-axis cap (Coin or USD, follows the chart's unit toggle).")
        .visible_if(move |cx| {
            is_fixed_for_field
                .read(cx, |p| matches!(p.scale, LiqBarsScale::Fixed { .. }))
                .unwrap_or(false)
        });

        let cumulative_field = Field::switch(
            "Cumulative line",
            target.getter(false, |p: &LiquidationBarsParams| p.show_cumulative),
            target.setter(|p: &mut LiquidationBarsParams, v: bool| p.show_cumulative = v),
        )
        .description("Running net (short USD − long USD across visible bars).");

        Some(
            SettingsForm::new(form_id).group(
                SettingsGroup::new("General")
                    .item(scale_field)
                    .item(max_field)
                    .item(cumulative_field),
            ),
        )
    }
}
