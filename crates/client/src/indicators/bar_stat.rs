//! Bar statistic row: per-bar metrics rendered as stacked text cells, with
//! optional heatmap-style color grading. Up to five rows are surfaced (in
//! fixed order): volume, signed delta, OI delta, long-liquidation total,
//! short-liquidation total. Volume and OI delta paint with a fixed blue base
//! tint; the delta row is bull/bear tinted by sign. Which rows actually
//! render is decided by the `show_*` flags in `BarStatParams`; paint divides
//! pane height by the visible row count.
//!
//! Grading modes (paint-time decision; compute always emits the full data
//! set so a mode flip is a pure paint refresh):
//!   * Off          — no fill, text on neutral background
//!   * Bar          — full-saturation bull/bear fill per cell, based on the
//!                    candle's own sign (volume → up/down candle; delta →
//!                    sign of delta itself; liq rows always full)
//!   * VisibleRange — fill intensity = `|v| / max(|v|)` across the visible
//!                    bar slice (computed in the paint pass, no compute cost)
//!   * Daily        — fill intensity = `|v| / daily_max_for_that_bar`, where
//!                    `daily_max_*` is a per-bar rolling 24h max precomputed
//!                    here in `compute()` so paint stays cheap
//!
//! Volume unit (Coin vs USD) is read off `ComputeCtx.volume_unit` to match
//! the rest of the chart's header toggle; the same toggle drives the
//! liquidation-row magnitude (qty vs USD notional).

use std::any::Any;

use gpui::{SharedString, WeakEntity};
use serde::{Deserialize, Serialize};

use super::instance::InstanceId;
use super::kind::{ComputeCtx, IndicatorKind, PaneKind, mark_close_series};
use super::output::{IndicatorOutput, Series, ValueReadout};
use crate::panels::ContentPanel;
use crate::persistence::VolumeUnit;
use crate::services::market_data::Candle;
use crate::settings_form::{
    AfterChange, DropdownOption, Field, IndicatorTarget, MultiCheckItem, SettingsForm,
    SettingsGroup,
};

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

/// Which metric rows the BarStat pane renders. New fields default
/// preserving v1 behavior (volume + delta on; new rows off) — older
/// persisted state without these fields deserializes to the same look.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BarStatParams {
    #[serde(default)]
    pub grade: BarStatGrade,
    #[serde(default = "default_true")]
    pub show_volume: bool,
    #[serde(default = "default_true")]
    pub show_delta: bool,
    #[serde(default)]
    pub show_long_liq: bool,
    #[serde(default)]
    pub show_short_liq: bool,
    /// Per-bar open-interest change (close − open) row. Rendered directly
    /// below the delta row with a fixed blue base tint (like volume).
    #[serde(default)]
    pub show_oi_delta: bool,
    /// Per-bar net long/short flow row (`sign(delta) × |ΔOI|`), bull/bear
    /// tinted by sign like the delta row.
    #[serde(default)]
    pub show_net_ls: bool,
    /// Order-book imbalance rows: one row per enabled depth preset (index into
    /// `OB_DEPTHS_PCT`). Empty = no OB rows. Each renders signed (bid-heavy
    /// green / ask-heavy red) on a fixed |ratio| intensity scale.
    #[serde(default)]
    pub ob_depths: Vec<usize>,
}

fn default_true() -> bool {
    true
}

impl Default for BarStatParams {
    fn default() -> Self {
        Self {
            grade: BarStatGrade::default(),
            show_volume: true,
            show_delta: true,
            show_long_liq: false,
            show_short_liq: false,
            show_oi_delta: false,
            show_net_ls: false,
            ob_depths: Vec::new(),
        }
    }
}

impl BarStatParams {
    /// Number of currently-visible rows. Drives per-row height in paint.
    pub fn visible_row_count(&self) -> usize {
        [
            self.show_volume,
            self.show_delta,
            self.show_oi_delta,
            self.show_net_ls,
            self.show_long_liq,
            self.show_short_liq,
        ]
        .into_iter()
        .filter(|b| *b)
        .count()
            + self.sorted_ob_depths().len()
    }

    /// Enabled OB depth presets, sorted ascending + de-duplicated + bounds-
    /// checked. The canonical order for both compute (series) and paint (rows).
    pub fn sorted_ob_depths(&self) -> Vec<usize> {
        let mut v: Vec<usize> = self
            .ob_depths
            .iter()
            .copied()
            .filter(|&i| i < super::ob_imbalance::OB_N)
            .collect();
        v.sort_unstable();
        v.dedup();
        v
    }
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
        let mut long_liq: Series = vec![None; n];
        let mut short_liq: Series = vec![None; n];
        let mut oi_delta: Series = vec![None; n];

        for c in candles {
            volume.push(Some(scale_vol(c.volume, c, unit)));
            delta.push(c.taker_buy_vol.map(|tbv| scale_vol(2.0 * tbv - c.volume, c, unit)));
        }

        // Two-pointer join of sorted liquidation bars against candle
        // open_times — mirrors the join in `liq_bars.rs::compute`. The
        // slice may be absent (no subscription yet); paint treats those
        // rows as all-None and just leaves cells blank.
        if let Some(bars) = ctx.liquidation_bars {
            let mut j = 0usize;
            for (i, c) in candles.iter().enumerate() {
                while j < bars.len() && bars[j].open_time < c.open_time {
                    j += 1;
                }
                if j < bars.len() && bars[j].open_time == c.open_time {
                    let b = &bars[j];
                    long_liq[i] = Some(match unit {
                        VolumeUnit::Coin => b.long_qty,
                        VolumeUnit::Usd => b.long_quote_qty,
                    });
                    short_liq[i] = Some(match unit {
                        VolumeUnit::Coin => b.short_qty,
                        VolumeUnit::Usd => b.short_quote_qty,
                    });
                    j += 1;
                }
            }
        }

        // OI Δ: per-bar change in open interest (close − open within the bar),
        // scaled to the active unit. USD uses the mark price (canonical OI
        // notional, like the OI indicator), falling back to the candle close
        // where the mark-price sub hasn't loaded. Same two-pointer join against
        // candle open_times as the liquidation rows above.
        let mark_close = mark_close_series(candles, ctx.mark_price);
        if let Some(bars) = ctx.open_interest {
            let mut j = 0usize;
            for (i, c) in candles.iter().enumerate() {
                while j < bars.len() && bars[j].open_time < c.open_time {
                    j += 1;
                }
                if j < bars.len() && bars[j].open_time == c.open_time {
                    let b = &bars[j];
                    let d = b.close - b.open;
                    oi_delta[i] = Some(match unit {
                        VolumeUnit::Coin => d,
                        VolumeUnit::Usd => d * mark_close[i].unwrap_or(c.close),
                    });
                    j += 1;
                }
            }
        }

        // Net long/short flow (`sign(delta) × |ΔOI|`) — shares the OI + mark
        // inputs with the dedicated Net L/S indicator; magnitude follows the
        // Coin/USD unit. `None` where the bar lacks an OI sample / taker delta.
        let net_ls =
            super::net_ls::net_ls_series(candles, ctx.open_interest, ctx.mark_price, unit);

        // OB imbalance: one ratio series per enabled depth preset, read off the
        // shared book-imbalance samples (last snapshot in each bar).
        let ob_depths = self.sorted_ob_depths();
        let ob_imbalance: Vec<Series> = ob_depths
            .iter()
            .map(|&idx| {
                super::ob_imbalance::book_imbalance_series(candles, ctx.book_imbalance, idx)
            })
            .collect();
        // Daily-grade maxima for the OB rows — only needed when that grade is
        // active (computed below alongside the other rows' maxima).
        let ob_times: Vec<i64> = if matches!(self.grade, BarStatGrade::Daily) {
            candles.iter().map(|c| c.open_time).collect()
        } else {
            Vec::new()
        };
        let daily_max_ob: Vec<Series> = if matches!(self.grade, BarStatGrade::Daily) {
            ob_imbalance
                .iter()
                .map(|s| rolling_daily_max_abs(s, &ob_times))
                .collect()
        } else {
            ob_imbalance.iter().map(|_| vec![None; n]).collect()
        };

        // The rolling 24h maxima are only consumed by the `Daily` grade.
        // Computing them is O(n × window) — skip entirely for the other
        // (incl. default) grades so panning/zooming doesn't pay for series
        // paint never reads. A grade flip triggers a recompute, so `Daily`
        // gets them when it's actually selected.
        let (
            daily_max_vol,
            daily_max_delta,
            daily_max_long_liq,
            daily_max_short_liq,
            daily_max_oi_delta,
            daily_max_net_ls,
        ) = if matches!(self.grade, BarStatGrade::Daily) {
            let times: Vec<i64> = candles.iter().map(|c| c.open_time).collect();
            (
                rolling_daily_max_abs(&volume, &times),
                rolling_daily_max_abs(&delta, &times),
                rolling_daily_max_abs(&long_liq, &times),
                rolling_daily_max_abs(&short_liq, &times),
                rolling_daily_max_abs(&oi_delta, &times),
                rolling_daily_max_abs(&net_ls, &times),
            )
        } else {
            (
                vec![None; n],
                vec![None; n],
                vec![None; n],
                vec![None; n],
                vec![None; n],
                vec![None; n],
            )
        };

        IndicatorOutput::BarStat {
            grade: self.grade,
            show_volume: self.show_volume,
            show_delta: self.show_delta,
            show_long_liq: self.show_long_liq,
            show_short_liq: self.show_short_liq,
            show_oi_delta: self.show_oi_delta,
            show_net_ls: self.show_net_ls,
            volume,
            delta,
            long_liq,
            short_liq,
            oi_delta,
            net_ls,
            ob_depths,
            ob_imbalance,
            daily_max_ob,
            daily_max_vol,
            daily_max_delta,
            daily_max_long_liq,
            daily_max_short_liq,
            daily_max_oi_delta,
            daily_max_net_ls,
        }
    }

    fn value_at(&self, _output: &IndicatorOutput, _index: usize) -> ValueReadout {
        // No crosshair chip readout — every Bar Stats row (vol / delta / liq /
        // OI Δ / net L/S / OB imbalance) is read directly off its cell, so a
        // chip readout would be redundant and ambiguous.
        ValueReadout::Empty
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

    fn settings_form(
        &self,
        panel: WeakEntity<ContentPanel>,
        id: InstanceId,
    ) -> Option<SettingsForm> {
        // Toggling long_liq / short_liq drives the shared LiquidationBars sub,
        // OI Δ + Net L/S drive the shared OpenInterest + MarkPrice subs, and the
        // OB rows drive the shared book sub — refresh all on every mutation
        // (idempotent for grade / other-row toggles).
        let target: IndicatorTarget<BarStatParams> =
            IndicatorTarget::new(panel, id).with_after_change(AfterChange::bar_stat());

        let grade_field = Field::dropdown(
            "Color grading",
            BarStatGrade::ALL
                .iter()
                .map(|g| DropdownOption::new(grade_value(*g), g.label()))
                .collect(),
            target.getter(SharedString::from("VisibleRange"), |p: &BarStatParams| {
                SharedString::from(grade_value(p.grade))
            }),
            target.setter(|p: &mut BarStatParams, v: SharedString| {
                p.grade = grade_from_value(v.as_ref()).unwrap_or_default();
            }),
        )
        .description("Scales each cell's tint by visible-range or trailing-24h max.");

        let items = vec![
            MultiCheckItem::new(
                "Volume",
                target.getter(true, |p: &BarStatParams| p.show_volume),
                target.setter(|p: &mut BarStatParams, v: bool| p.show_volume = v),
            ),
            MultiCheckItem::new(
                "Delta",
                target.getter(true, |p: &BarStatParams| p.show_delta),
                target.setter(|p: &mut BarStatParams, v: bool| p.show_delta = v),
            ),
            MultiCheckItem::new(
                "OI Δ",
                target.getter(false, |p: &BarStatParams| p.show_oi_delta),
                target.setter(|p: &mut BarStatParams, v: bool| p.show_oi_delta = v),
            )
            .description("Per-bar change in open interest (close − open)."),
            MultiCheckItem::new(
                "Net L/S",
                target.getter(false, |p: &BarStatParams| p.show_net_ls),
                target.setter(|p: &mut BarStatParams, v: bool| p.show_net_ls = v),
            )
            .description("Net positioning flow: sign(delta) × |ΔOI|."),
            MultiCheckItem::new(
                "Long Liq",
                target.getter(false, |p: &BarStatParams| p.show_long_liq),
                target.setter(|p: &mut BarStatParams, v: bool| p.show_long_liq = v),
            ),
            MultiCheckItem::new(
                "Short Liq",
                target.getter(false, |p: &BarStatParams| p.show_short_liq),
                target.setter(|p: &mut BarStatParams, v: bool| p.show_short_liq = v),
            ),
        ];

        let rows_field = Field::multi_checkbox("Show rows", items)
            .description("Rows render top-to-bottom in this order.");

        // OB-imbalance depth rows: one checkbox per preset; checking adds the
        // depth index to `ob_depths` (one bar-stat row per checked depth).
        use super::ob_imbalance::{OB_DEPTHS_PCT, ob_depth_label};
        let ob_items: Vec<MultiCheckItem> = OB_DEPTHS_PCT
            .iter()
            .enumerate()
            .map(|(idx, d)| {
                MultiCheckItem::new(
                    ob_depth_label(*d),
                    target.getter(false, move |p: &BarStatParams| p.ob_depths.contains(&idx)),
                    target.setter(move |p: &mut BarStatParams, v: bool| {
                        if v {
                            if !p.ob_depths.contains(&idx) {
                                p.ob_depths.push(idx);
                            }
                        } else {
                            p.ob_depths.retain(|&i| i != idx);
                        }
                    }),
                )
            })
            .collect();
        let ob_field = Field::multi_checkbox("OB imbalance depths", ob_items)
            .description("One imbalance row per checked depth (% from mid).");

        let form_id = SharedString::from(format!("bar-stat-{}", id));
        Some(
            SettingsForm::new(form_id).group(
                SettingsGroup::new("General")
                    .item(grade_field)
                    .item(rows_field)
                    .item(ob_field),
            ),
        )
    }
}

fn grade_value(g: BarStatGrade) -> &'static str {
    match g {
        BarStatGrade::Off => "Off",
        BarStatGrade::Bar => "Bar",
        BarStatGrade::VisibleRange => "VisibleRange",
        BarStatGrade::Daily => "Daily",
    }
}

fn grade_from_value(s: &str) -> Option<BarStatGrade> {
    Some(match s {
        "Off" => BarStatGrade::Off,
        "Bar" => BarStatGrade::Bar,
        "VisibleRange" => BarStatGrade::VisibleRange,
        "Daily" => BarStatGrade::Daily,
        _ => return None,
    })
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
