//! Visible-Range Volume Profile (VRVP) — overlay indicator that aggregates
//! per-price-bucket bid/ask volume across the chart's currently-visible bar
//! window. Shares all heavy lifting (params struct, compute, paint, settings
//! form) with the FRVP drawing tool via [`crate::volume_profile`]; this file
//! is just the [`IndicatorKind`] adapter.
//!
//! Phase 5 (this commit) wires the kind so it shows up in the picker, can be
//! added to the chart, and round-trips through persistence. `compute` returns
//! an empty [`VolumeProfileOutput`] until Phase 6 implements real aggregation;
//! the paint pipeline's stub arm draws nothing until Phase 7 lights it up.
//!
//! Pane semantics: `OverlayOnly` — VRVP draws bars anchored to the right
//! (default) edge of the candle pane, never as its own sub-pane. Per-bar
//! `y_range` is `None` so VRVP doesn't disturb the candle pane's auto-fit.

use std::any::Any;

use gpui::{Hsla, SharedString, WeakEntity};
use serde::{Deserialize, Serialize};

use super::instance::InstanceId;
use super::kind::{ComputeCtx, IndicatorKind, PaneKind};
use super::output::{IndicatorOutput, ValueReadout};
use crate::panels::ContentPanel;
use crate::services::market_data::Candle;
use crate::settings_form::{
    AfterChange, DropdownOption, Field, IndicatorTarget, NumberOpts, SettingsForm, SettingsGroup,
};
use crate::volume_profile::{
    AnchorEdge, VolumeProfileOutput, VolumeProfileParams, VpDeltaScale, VpRenderMode,
    compute_volume_profile,
    params::{
        BTCUSDT_TICK_SIZE, BUCKET_TICKS_MAX, BUCKET_TICKS_MIN, ColorBlob, VA_PERCENT_MAX,
        VA_PERCENT_MIN, WIDTH_PCT_MAX, WIDTH_PCT_MIN,
    },
};

/// One VRVP instance. Single field: the shared params struct (FRVP uses the
/// same struct inside its `DrawingShape::Frvp`). Serializing the wrapper
/// rather than `VolumeProfileParams` directly keeps the persisted JSON shape
/// extensible if the indicator ever grows VRVP-only fields.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct VrvpParams {
    pub params: VolumeProfileParams,
}

impl IndicatorKind for VrvpParams {
    fn kind_id(&self) -> &'static str {
        "vrvp"
    }

    fn pane_kind(&self) -> PaneKind {
        PaneKind::OverlayOnly
    }

    fn label(&self) -> SharedString {
        // Bucket size is the most useful single-glance summary — matches how
        // orderflow traders refer to a profile ("100-tick VRVP").
        format!("VRVP {}t", self.params.bucket_ticks).into()
    }

    fn compute(&self, candles: &[Candle], ctx: ComputeCtx<'_>) -> IndicatorOutput {
        // VRVP aggregates over the chart's visible bar window only — that's
        // the "visible range" in the name. Without a measured viewport the
        // window is undefined and we report empty (rather than over- or
        // under-counting against the full loaded buffer).
        let Some(range) = ctx.view_time_range else {
            return IndicatorOutput::VolumeProfile {
                output: VolumeProfileOutput::default(),
                params: self.params.clone(),
            };
        };
        // TF inferred from adjacent open_times (avoids plumbing the chart's
        // `Timeframe` through ComputeCtx for a one-call consumer). Falls to
        // 0 when fewer than 2 candles are loaded — `compute_volume_profile`
        // short-circuits in that case.
        let tf_ms = if candles.len() >= 2 {
            candles[1].open_time - candles[0].open_time
        } else {
            0
        };
        let cells = ctx
            .footprint
            .and_then(|lookup| lookup.cells_for_bucket(self.params.bucket_dollars()))
            .unwrap_or(&[]);
        let output = compute_volume_profile(cells, range, tf_ms, &self.params);
        IndicatorOutput::VolumeProfile {
            output,
            params: self.params.clone(),
        }
    }

    fn value_at(&self, _output: &IndicatorOutput, _index: usize) -> ValueReadout {
        // No crosshair interaction in v1 (per the design grilling). The chip
        // strip skips VP-shape outputs entirely.
        ValueReadout::One(None)
    }

    fn y_range(
        &self,
        _output: &IndicatorOutput,
        _range: std::ops::Range<usize>,
    ) -> Option<(f64, f64)> {
        // VRVP is price-bucket-keyed, not bar-keyed — it shouldn't pull the
        // candle pane's y-fit up or down. Returning `None` keeps the candle
        // auto-fit driven purely by visible candles.
        None
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
        let target: IndicatorTarget<VrvpParams> =
            IndicatorTarget::new(panel, id).with_after_change(AfterChange::footprint());
        let form_id = SharedString::from(format!("vrvp-{}", id));

        // ── visibility predicates ──
        let target_for_is_delta = target.clone();
        let is_delta_mode = move |cx: &gpui::App| -> bool {
            target_for_is_delta
                .read(cx, |p| !matches!(p.params.render_mode, VpRenderMode::Volume))
                .unwrap_or(false)
        };
        // Delta scaling only affects the pure Delta mode — VolDeltaOutline
        // forces per-row scaling internally, so the knob is meaningless there.
        let target_for_pure_delta = target.clone();
        let is_pure_delta_mode = move |cx: &gpui::App| -> bool {
            target_for_pure_delta
                .read(cx, |p| matches!(p.params.render_mode, VpRenderMode::Delta))
                .unwrap_or(false)
        };
        let target_for_show_poc = target.clone();
        let show_poc_pred = move |cx: &gpui::App| -> bool {
            target_for_show_poc
                .read(cx, |p| p.params.show_poc)
                .unwrap_or(true)
        };
        let target_for_show_va = target.clone();
        let show_va_pred = move |cx: &gpui::App| -> bool {
            target_for_show_va
                .read(cx, |p| p.params.show_va)
                .unwrap_or(true)
        };

        // ── General ──
        let bucket_field = Field::number(
            "Bucket",
            NumberOpts::int(BUCKET_TICKS_MIN as i64, BUCKET_TICKS_MAX as i64).with_step(10.0),
            target.getter(100.0, |p: &VrvpParams| p.params.bucket_ticks as f64),
            target.setter(|p: &mut VrvpParams, v: f64| {
                let cur = v.round().clamp(BUCKET_TICKS_MIN as f64, BUCKET_TICKS_MAX as f64);
                p.params.bucket_ticks = cur as u32;
            }),
        )
        .description("Price bucket size in ticks ($0.10 each).");

        let mode_field = Field::dropdown(
            "Render mode",
            VpRenderMode::ALL
                .iter()
                .map(|m| DropdownOption::new(render_mode_value(*m), m.label()))
                .collect(),
            target.getter(SharedString::from("volume"), |p: &VrvpParams| {
                SharedString::from(render_mode_value(p.params.render_mode))
            }),
            target.setter(|p: &mut VrvpParams, v: SharedString| {
                if let Some(m) = render_mode_from_value(v.as_ref()) {
                    p.params.render_mode = m;
                }
            }),
        );

        let scale_field = Field::dropdown(
            "Delta scaling",
            VpDeltaScale::ALL
                .iter()
                .map(|s| DropdownOption::new(delta_scale_value(*s), s.label()))
                .collect(),
            target.getter(SharedString::from("per_row"), |p: &VrvpParams| {
                SharedString::from(delta_scale_value(p.params.delta_scale))
            }),
            target.setter(|p: &mut VrvpParams, v: SharedString| {
                if let Some(s) = delta_scale_from_value(v.as_ref()) {
                    p.params.delta_scale = s;
                }
            }),
        )
        .visible_if(is_pure_delta_mode);

        let width_field = Field::slider(
            "Width",
            NumberOpts::int(WIDTH_PCT_MIN as i64, WIDTH_PCT_MAX as i64).with_step(5.0)
                .suffix("%"),
            target.getter(30.0, |p: &VrvpParams| p.params.width_pct as f64),
            target.setter(|p: &mut VrvpParams, v: f64| {
                let cur = v.round().clamp(WIDTH_PCT_MIN as f64, WIDTH_PCT_MAX as f64);
                p.params.width_pct = cur as u8;
            }),
        )
        .description("Profile width as a percentage of the chart pane.");

        let anchor_field = Field::dropdown(
            "Anchor",
            AnchorEdge::ALL
                .iter()
                .map(|a| DropdownOption::new(anchor_value(*a), a.label()))
                .collect(),
            target.getter(SharedString::from("right"), |p: &VrvpParams| {
                SharedString::from(anchor_value(p.params.anchor))
            }),
            target.setter(|p: &mut VrvpParams, v: SharedString| {
                if let Some(a) = anchor_from_value(v.as_ref()) {
                    p.params.anchor = a;
                }
            }),
        );

        let volume_color = make_color_field("Volume color", target.clone(), |p| &mut p.params.color_volume, |p| p.params.color_volume);
        let bull_color = make_color_field("Bull color", target.clone(), |p| &mut p.params.color_bull, |p| p.params.color_bull)
            .visible_if(is_delta_mode.clone());
        let bear_color = make_color_field("Bear color", target.clone(), |p| &mut p.params.color_bear, |p| p.params.color_bear)
            .visible_if(is_delta_mode);

        // ── POC group ──
        let show_poc_field = Field::switch(
            "Show POC",
            target.getter(true, |p: &VrvpParams| p.params.show_poc),
            target.setter(|p: &mut VrvpParams, v: bool| p.params.show_poc = v),
        );
        let poc_color = make_color_field("POC color", target.clone(), |p| &mut p.params.color_poc, |p| p.params.color_poc)
            .visible_if(show_poc_pred);

        // ── VA group ──
        let show_va_field = Field::switch(
            "Show VA",
            target.getter(true, |p: &VrvpParams| p.params.show_va),
            target.setter(|p: &mut VrvpParams, v: bool| p.params.show_va = v),
        );
        let va_pct_field = Field::slider(
            "VA %",
            NumberOpts::int(VA_PERCENT_MIN as i64, VA_PERCENT_MAX as i64).with_step(1.0)
                .suffix("%"),
            target.getter(70.0, |p: &VrvpParams| p.params.va_percent as f64),
            target.setter(|p: &mut VrvpParams, v: f64| {
                let cur = v.round().clamp(VA_PERCENT_MIN as f64, VA_PERCENT_MAX as f64);
                p.params.va_percent = cur as u8;
            }),
        );
        let show_va_hl_field = Field::switch(
            "Show VA highlight",
            target.getter(true, |p: &VrvpParams| p.params.show_va_highlight),
            target.setter(|p: &mut VrvpParams, v: bool| p.params.show_va_highlight = v),
        );
        let va_color = make_color_field("VA color", target.clone(), |p| &mut p.params.color_va, |p| p.params.color_va)
            .visible_if(show_va_pred);

        let _ = BTCUSDT_TICK_SIZE;

        Some(
            SettingsForm::new(form_id)
                .group(
                    SettingsGroup::new("General")
                        .item(bucket_field)
                        .item(mode_field)
                        .item(scale_field)
                        .item(width_field)
                        .item(anchor_field)
                        .item(volume_color)
                        .item(bull_color)
                        .item(bear_color),
                )
                .group(
                    SettingsGroup::new("POC")
                        .item(show_poc_field)
                        .item(poc_color),
                )
                .group(
                    SettingsGroup::new("VA")
                        .item(show_va_field)
                        .item(va_pct_field)
                        .item(show_va_hl_field)
                        .item(va_color),
                ),
        )
    }
}

fn make_color_field<F, G>(
    label: &'static str,
    target: IndicatorTarget<VrvpParams>,
    set_field: F,
    get_field: G,
) -> Field
where
    F: Fn(&mut VrvpParams) -> &mut ColorBlob + 'static + Clone,
    G: Fn(&VrvpParams) -> ColorBlob + 'static + Clone,
{
    let get_clone = get_field.clone();
    let get_default = get_field.clone();
    let target_for_get = target.clone();
    let target_for_set = target;
    Field::color(
        label,
        move |cx: &gpui::App| -> Hsla {
            target_for_get
                .read(cx, |p| get_clone(p).into_hsla())
                .unwrap_or_else(|| get_default(&VrvpParams::default()).into_hsla())
        },
        move |color: Hsla, cx: &mut gpui::App| {
            let set_field = set_field.clone();
            target_for_set.write(cx, move |p| {
                *set_field(p) = ColorBlob::from_hsla(color);
            });
        },
    )
}

fn render_mode_value(m: VpRenderMode) -> &'static str {
    match m {
        VpRenderMode::Volume => "volume",
        VpRenderMode::Delta => "delta",
        VpRenderMode::VolDeltaOutline => "vol_delta_outline",
    }
}

fn render_mode_from_value(s: &str) -> Option<VpRenderMode> {
    Some(match s {
        "volume" => VpRenderMode::Volume,
        "delta" => VpRenderMode::Delta,
        "vol_delta_outline" => VpRenderMode::VolDeltaOutline,
        _ => return None,
    })
}

fn delta_scale_value(s: VpDeltaScale) -> &'static str {
    match s {
        VpDeltaScale::PerRow => "per_row",
        VpDeltaScale::WholeProfile => "whole_profile",
    }
}

fn delta_scale_from_value(s: &str) -> Option<VpDeltaScale> {
    Some(match s {
        "per_row" => VpDeltaScale::PerRow,
        "whole_profile" => VpDeltaScale::WholeProfile,
        _ => return None,
    })
}

fn anchor_value(a: AnchorEdge) -> &'static str {
    match a {
        AnchorEdge::Right => "right",
        AnchorEdge::Left => "left",
    }
}

fn anchor_from_value(s: &str) -> Option<AnchorEdge> {
    Some(match s {
        "right" => AnchorEdge::Right,
        "left" => AnchorEdge::Left,
        _ => return None,
    })
}

