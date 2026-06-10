//! Floating per-drawing settings window. Mounted by the workspace when
//! the strip's gear button fires. Exposes the dials that wouldn't fit
//! comfortably on the strip itself — primarily the per-timeframe
//! visibility filter, which today only surfaces through the chart's
//! right-click submenu.
//!
//! Singleton: a second dispatch with a different `(symbol, id)`
//! retargets the existing view (no second window). View re-reads the
//! service on every render so external mutations (toggling via the
//! right-click menu, deleting from the strip, etc.) reflect live.

use std::collections::BTreeSet;

use gpui::{
    App, Context, FocusHandle, Focusable, Hsla, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, StatefulInteractiveElement as _, Styled as _,
    Window, div, px,
};
use gpui_component::{
    ActiveTheme as _, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex, v_flex,
};

use crate::services::market_data::Timeframe;
use crate::settings_form::{
    DropdownOption, Field, NumberOpts, SettingsForm, SettingsGroup,
};

use super::actions::{
    ResetDrawingTfFilter, SetTextFontSize, ToggleDrawingHidden, ToggleDrawingTfFilter,
    ToggleRayExtendLeft,
};
use super::service::{DrawingId, DrawingServiceHandle};
use super::shapes::{Drawing, DrawingOrigin, DrawingShape};
use crate::volume_profile::{
    AnchorEdge, VolumeProfileParams, VpDeltaScale, VpRenderMode,
    params::{
        BTCUSDT_TICK_SIZE, BUCKET_TICKS_MAX, BUCKET_TICKS_MIN, ColorBlob, VA_PERCENT_MAX,
        VA_PERCENT_MIN, WIDTH_PCT_MAX, WIDTH_PCT_MIN,
    },
};

/// Per-drawing settings window content. Owns nothing the strip already
/// owns — colour / width / label / lock / delete live there. This view
/// is for the lower-frequency settings that benefit from a roomier UI.
pub struct DrawingSettingsView {
    target: (SharedString, DrawingId),
    focus: FocusHandle,
}

impl DrawingSettingsView {
    pub fn new(
        symbol: SharedString,
        id: DrawingId,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            target: (symbol, id),
            focus: cx.focus_handle(),
        }
    }

    /// Re-point the window at a different drawing. Used when the user
    /// selects another drawing and clicks its gear — keeps a single
    /// window instead of a stack.
    pub fn retarget(
        &mut self,
        symbol: SharedString,
        id: DrawingId,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.target = (symbol, id);
        cx.notify();
    }

    pub fn current_target(&self) -> &(SharedString, DrawingId) {
        &self.target
    }
}

impl Focusable for DrawingSettingsView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for DrawingSettingsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let border = cx.theme().border;
        let (symbol, id) = self.target.clone();
        let Some(handle) = cx.try_global::<DrawingServiceHandle>().cloned() else {
            return missing_body("Drawing service unavailable", muted).into_any_element();
        };
        // Snapshot what the view actually needs out of the borrow.
        let snapshot = {
            let svc = handle.0.read(cx);
            svc.for_symbol(symbol.as_ref())
                .iter()
                .find(|d| d.id == id)
                .map(|d| DrawingSnapshot::from(d.as_ref_proxy()))
        };
        let Some(snap) = snapshot else {
            return missing_body("Drawing was removed", muted).into_any_element();
        };

        let header_label: SharedString = SharedString::from(snap.label.clone());
        let origin_text = match snap.origin {
            DrawingOrigin::User => "User-drawn",
            DrawingOrigin::Ai => "AI-drawn",
        };

        // Body content is accumulated separately and then wrapped in a
        // scrollable div, so windows whose content exceeds the FloatingWindow
        // height stay usable. Inner uses `.w_full()` (not `.size_full()`)
        // per the CLAUDE.md scroll gotcha.
        let mut root = v_flex()
            .w_full()
            .p_4()
            .gap_4()
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().foreground)
                            .child(header_label),
                    )
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(muted)
                            .child(SharedString::from(origin_text)),
                    ),
            )
            .child(div().h(px(1.)).bg(border));

        // ── Visibility row: master hide/show toggle ─────────────────────
        let symbol_for_hide = symbol.clone();
        let hide_label: SharedString = if snap.hidden { "Show drawing" } else { "Hide drawing" }.into();
        root = root.child(
            v_flex()
                .gap_2()
                .child(section_label("Visibility", muted))
                .child(
                    Button::new(("drawing-settings-toggle-hidden", id as usize))
                        .small()
                        .ghost()
                        .label(hide_label)
                        .on_click(move |_, window, cx| {
                            window.dispatch_action(
                                Box::new(ToggleDrawingHidden {
                                    symbol: symbol_for_hide.clone(),
                                    id,
                                }),
                                cx,
                            );
                        }),
                ),
        );

        // ── Visible-on row: one chip per timeframe ──────────────────────
        let tf_filter = snap.tf_filter.clone();
        let symbol_for_tf = symbol.clone();
        let mut tf_chips = h_flex().gap_1().flex_wrap();
        for tf in Timeframe::ALL {
            let tf_str = tf.as_str();
            let active = match &tf_filter {
                None => true,
                Some(set) => set.contains(tf_str),
            };
            let sym_for_chip = symbol_for_tf.clone();
            let mut chip = Button::new(SharedString::from(format!(
                "drawing-settings-tf-{}-{}",
                id, tf_str
            )))
            .xsmall()
            .label(SharedString::from(tf_str));
            chip = if active { chip.primary() } else { chip.ghost() };
            chip = chip.on_click(move |_, window, cx| {
                window.dispatch_action(
                    Box::new(ToggleDrawingTfFilter {
                        symbol: sym_for_chip.clone(),
                        id,
                        tf: SharedString::from(tf_str),
                    }),
                    cx,
                );
            });
            tf_chips = tf_chips.child(chip);
        }
        let mut tf_section = v_flex()
            .gap_2()
            .child(section_label("Visible on", muted))
            .child(tf_chips);
        if tf_filter.is_some() {
            let sym_for_reset = symbol.clone();
            tf_section = tf_section.child(
                Button::new(("drawing-settings-tf-reset", id as usize))
                    .xsmall()
                    .ghost()
                    .label("Reset to all timeframes")
                    .on_click(move |_, window, cx| {
                        window.dispatch_action(
                            Box::new(ResetDrawingTfFilter {
                                symbol: sym_for_reset.clone(),
                                id,
                            }),
                            cx,
                        );
                    }),
            );
        }
        root = root.child(tf_section);

        // ── Ray-only: extend-left toggle ────────────────────────────────
        if let Some(extend_left) = snap.ray_extend_left {
            let sym_for_ray = symbol.clone();
            let toggle_label: SharedString = if extend_left {
                "Extend left: on".into()
            } else {
                "Extend left: off".into()
            };
            let mut toggle_btn = Button::new(("drawing-settings-extend-left", id as usize))
                .small()
                .label(toggle_label)
                .on_click(move |_, window, cx| {
                    window.dispatch_action(
                        Box::new(ToggleRayExtendLeft {
                            symbol: sym_for_ray.clone(),
                            id,
                        }),
                        cx,
                    );
                });
            toggle_btn = if extend_left { toggle_btn.primary() } else { toggle_btn.ghost() };
            root = root.child(
                v_flex()
                    .gap_2()
                    .child(section_label("Horizontal ray", muted))
                    .child(toggle_btn),
            );
        }

        // ── FRVP-only: full VolumeProfile form via the standardized
        //              SettingsForm framework. Three groups
        //              (General / POC / VA) mirror the VRVP indicator's
        //              form; differences are routed through DrawingService's
        //              `set_vp_params` instead of `chart.update_indicator`.
        if snap.frvp_params.is_some() {
            let form = build_frvp_form(symbol.clone(), id);
            root = root.child(div().h(px(1.)).bg(border)).child(form.render(window, cx));
        }

        // ── Text-only: font-size chips ──────────────────────────────────
        if let Some(font_size) = snap.text_font_size {
            const FONT_SIZE_CHOICES: &[f32] = &[10.0, 12.0, 14.0, 16.0, 20.0, 24.0];
            let sym_for_font = symbol.clone();
            let mut chips = h_flex().gap_1().flex_wrap();
            for &size in FONT_SIZE_CHOICES {
                let active = (font_size - size).abs() < 0.01;
                let sym_for_chip = sym_for_font.clone();
                let mut chip = Button::new(SharedString::from(format!(
                    "drawing-settings-fontsize-{}-{}",
                    id, size as u32
                )))
                .xsmall()
                .label(SharedString::from(format!("{}px", size as u32)));
                chip = if active { chip.primary() } else { chip.ghost() };
                chip = chip.on_click(move |_, window, cx| {
                    window.dispatch_action(
                        Box::new(SetTextFontSize::with_px(sym_for_chip.clone(), id, size)),
                        cx,
                    );
                });
                chips = chips.child(chip);
            }
            root = root.child(
                v_flex()
                    .gap_2()
                    .child(section_label("Font size", muted))
                    .child(chips),
            );
        }

        v_flex()
            .id(SharedString::from(format!("drawing-settings-{}", id)))
            .size_full()
            .child(
                div()
                    .id(SharedString::from(format!("drawing-settings-scroll-{}", id)))
                    .flex_1()
                    .w_full()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(root),
            )
            .into_any_element()
    }
}

fn section_label(text: &'static str, color: Hsla) -> impl IntoElement {
    div()
        .text_size(px(11.))
        .text_color(color)
        .child(SharedString::from(text))
}

/// Mutate the FRVP's persisted params and push the result through
/// `set_vp_params`. Read-modify-write each click — drawings change at
/// human pace, so the extra read is cheap and the mutation closure stays
/// trivial. No-op if the drawing has been deleted between snapshot and
/// click.
fn mutate_frvp_params(
    symbol: SharedString,
    id: DrawingId,
    cx: &mut App,
    mutator: impl FnOnce(&mut VolumeProfileParams),
) {
    let Some(handle) = cx.try_global::<DrawingServiceHandle>().cloned() else {
        return;
    };
    let mut params = {
        let svc = handle.0.read(cx);
        let Some(d) = svc
            .for_symbol(symbol.as_ref())
            .iter()
            .find(|d| d.id == id)
        else {
            return;
        };
        let DrawingShape::Frvp(f) = &d.shape else {
            return;
        };
        f.params.clone()
    };
    mutator(&mut params);
    if !params.is_valid() {
        return;
    }
    handle.0.update(cx, |s, cx| {
        s.set_vp_params(symbol.as_ref(), id, params, cx);
    });
}

/// Build the FRVP settings form via the standardized framework.
/// Three groups: General (layout knobs + volume/bull/bear colors),
/// POC (show_poc + POC color), VA (show_va + VA % + show_va_highlight +
/// labels + VA color). Color fields gate on render mode + show_poc/show_va
/// via `visible_if`.
fn build_frvp_form(symbol: SharedString, id: DrawingId) -> SettingsForm {
    let form_id = SharedString::from(format!("frvp-{}", id));

    let read_pred = |symbol: SharedString, id: DrawingId, f: fn(&VolumeProfileParams) -> bool| {
        move |cx: &App| -> bool { read_frvp_params(symbol.clone(), id, cx, &f).unwrap_or(false) }
    };
    let is_delta_pred =
        |symbol: SharedString, id: DrawingId| -> Box<dyn Fn(&App) -> bool + 'static> {
            Box::new(move |cx| {
                read_frvp_params(symbol.clone(), id, cx, &|p: &VolumeProfileParams| {
                    !matches!(p.render_mode, VpRenderMode::Volume)
                })
                .unwrap_or(false)
            })
        };

    let bucket_field = Field::number(
        "Bucket",
        NumberOpts::int(BUCKET_TICKS_MIN as i64, BUCKET_TICKS_MAX as i64).with_step(10.0),
        getter_f64(symbol.clone(), id, |p| p.bucket_ticks as f64),
        setter_frvp(symbol.clone(), id, |p, v: f64| {
            let nxt = v
                .round()
                .clamp(BUCKET_TICKS_MIN as f64, BUCKET_TICKS_MAX as f64);
            p.bucket_ticks = nxt as u32;
        }),
    )
    .description("Price bucket size in ticks ($0.10 each).");

    let mode_field = Field::dropdown(
        "Render mode",
        VpRenderMode::ALL
            .iter()
            .map(|m| DropdownOption::new(render_mode_value(*m), m.label()))
            .collect(),
        getter_str(symbol.clone(), id, |p| {
            SharedString::from(render_mode_value(p.render_mode))
        }),
        setter_frvp(symbol.clone(), id, |p, v: SharedString| {
            if let Some(m) = render_mode_from_value(v.as_ref()) {
                p.render_mode = m;
            }
        }),
    );

    let scale_field = Field::dropdown(
        "Delta scaling",
        VpDeltaScale::ALL
            .iter()
            .map(|s| DropdownOption::new(delta_scale_value(*s), s.label()))
            .collect(),
        getter_str(symbol.clone(), id, |p| {
            SharedString::from(delta_scale_value(p.delta_scale))
        }),
        setter_frvp(symbol.clone(), id, |p, v: SharedString| {
            if let Some(s) = delta_scale_from_value(v.as_ref()) {
                p.delta_scale = s;
            }
        }),
    )
    .visible_if({
        let s = symbol.clone();
        let pred = is_delta_pred(s, id);
        move |cx| pred(cx)
    });

    let width_field = Field::number(
        "Width",
        NumberOpts::int(WIDTH_PCT_MIN as i64, WIDTH_PCT_MAX as i64)
            .with_step(5.0)
            .format(|v| SharedString::from(format!("{}%", v.round() as i64))),
        getter_f64(symbol.clone(), id, |p| p.width_pct as f64),
        setter_frvp(symbol.clone(), id, |p, v: f64| {
            let nxt = v
                .round()
                .clamp(WIDTH_PCT_MIN as f64, WIDTH_PCT_MAX as f64);
            p.width_pct = nxt as u8;
        }),
    );

    let anchor_field = Field::dropdown(
        "Anchor",
        AnchorEdge::ALL
            .iter()
            .map(|a| DropdownOption::new(anchor_value(*a), a.label()))
            .collect(),
        getter_str(symbol.clone(), id, |p| {
            SharedString::from(anchor_value(p.anchor))
        }),
        setter_frvp(symbol.clone(), id, |p, v: SharedString| {
            if let Some(a) = anchor_from_value(v.as_ref()) {
                p.anchor = a;
            }
        }),
    );

    let volume_color = make_color_field(
        "Volume color",
        symbol.clone(),
        id,
        |p| p.color_volume,
        |p, c| p.color_volume = c,
    );
    let bull_color = make_color_field(
        "Bull color",
        symbol.clone(),
        id,
        |p| p.color_bull,
        |p, c| p.color_bull = c,
    )
    .visible_if({
        let s = symbol.clone();
        let pred = is_delta_pred(s, id);
        move |cx| pred(cx)
    });
    let bear_color = make_color_field(
        "Bear color",
        symbol.clone(),
        id,
        |p| p.color_bear,
        |p, c| p.color_bear = c,
    )
    .visible_if({
        let s = symbol.clone();
        let pred = is_delta_pred(s, id);
        move |cx| pred(cx)
    });

    let show_poc_field = Field::switch(
        "Show POC",
        getter_bool(symbol.clone(), id, |p| p.show_poc),
        setter_frvp(symbol.clone(), id, |p, v: bool| p.show_poc = v),
    );
    let poc_color = make_color_field(
        "POC color",
        symbol.clone(),
        id,
        |p| p.color_poc,
        |p, c| p.color_poc = c,
    )
    .visible_if({
        let s = symbol.clone();
        read_pred(s, id, |p| p.show_poc)
    });

    let show_va_field = Field::switch(
        "Show VA",
        getter_bool(symbol.clone(), id, |p| p.show_va),
        setter_frvp(symbol.clone(), id, |p, v: bool| p.show_va = v),
    );
    let va_pct_field = Field::number(
        "VA %",
        NumberOpts::int(VA_PERCENT_MIN as i64, VA_PERCENT_MAX as i64)
            .with_step(5.0)
            .format(|v| SharedString::from(format!("{}%", v.round() as i64))),
        getter_f64(symbol.clone(), id, |p| p.va_percent as f64),
        setter_frvp(symbol.clone(), id, |p, v: f64| {
            let nxt = v
                .round()
                .clamp(VA_PERCENT_MIN as f64, VA_PERCENT_MAX as f64);
            p.va_percent = nxt as u8;
        }),
    );
    let show_va_hl_field = Field::switch(
        "Show VA highlight",
        getter_bool(symbol.clone(), id, |p| p.show_va_highlight),
        setter_frvp(symbol.clone(), id, |p, v: bool| p.show_va_highlight = v),
    );
    let labels_field = Field::switch(
        "Show labels",
        getter_bool(symbol.clone(), id, |p| p.show_labels),
        setter_frvp(symbol.clone(), id, |p, v: bool| p.show_labels = v),
    );
    let va_color = make_color_field(
        "VA color",
        symbol.clone(),
        id,
        |p| p.color_va,
        |p, c| p.color_va = c,
    )
    .visible_if({
        let s = symbol.clone();
        read_pred(s, id, |p| p.show_va)
    });

    let _ = BTCUSDT_TICK_SIZE;

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
        .group(SettingsGroup::new("POC").item(show_poc_field).item(poc_color))
        .group(
            SettingsGroup::new("VA")
                .item(show_va_field)
                .item(va_pct_field)
                .item(show_va_hl_field)
                .item(labels_field)
                .item(va_color),
        )
}

// ─────────── FRVP read/write helpers (settings_form glue) ───────────

fn read_frvp_params<R>(
    symbol: SharedString,
    id: DrawingId,
    cx: &App,
    f: impl FnOnce(&VolumeProfileParams) -> R,
) -> Option<R> {
    let handle = cx.try_global::<DrawingServiceHandle>().cloned()?;
    let svc = handle.0.read(cx);
    let d = svc.for_symbol(symbol.as_ref()).iter().find(|d| d.id == id)?;
    let DrawingShape::Frvp(frvp) = &d.shape else {
        return None;
    };
    Some(f(&frvp.params))
}

fn getter_f64<F>(
    symbol: SharedString,
    id: DrawingId,
    f: F,
) -> impl Fn(&App) -> f64 + 'static
where
    F: Fn(&VolumeProfileParams) -> f64 + 'static,
{
    move |cx| read_frvp_params(symbol.clone(), id, cx, &f).unwrap_or(0.0)
}

fn getter_str<F>(
    symbol: SharedString,
    id: DrawingId,
    f: F,
) -> impl Fn(&App) -> SharedString + 'static
where
    F: Fn(&VolumeProfileParams) -> SharedString + 'static,
{
    move |cx| read_frvp_params(symbol.clone(), id, cx, &f).unwrap_or_default()
}

fn getter_bool<F>(
    symbol: SharedString,
    id: DrawingId,
    f: F,
) -> impl Fn(&App) -> bool + 'static
where
    F: Fn(&VolumeProfileParams) -> bool + 'static,
{
    move |cx| read_frvp_params(symbol.clone(), id, cx, &f).unwrap_or(false)
}

fn setter_frvp<T, F>(
    symbol: SharedString,
    id: DrawingId,
    f: F,
) -> impl Fn(T, &mut App) + 'static
where
    T: 'static,
    F: Fn(&mut VolumeProfileParams, T) + 'static + Clone,
{
    move |value, cx| {
        let symbol = symbol.clone();
        let f = f.clone();
        mutate_frvp_params(symbol, id, cx, move |p| f(p, value));
    }
}

fn make_color_field<G, S>(
    label: &'static str,
    symbol: SharedString,
    id: DrawingId,
    get_field: G,
    set_field: S,
) -> Field
where
    G: Fn(&VolumeProfileParams) -> ColorBlob + 'static + Clone,
    S: Fn(&mut VolumeProfileParams, ColorBlob) + 'static + Clone,
{
    let symbol_for_get = symbol.clone();
    let get_clone = get_field.clone();
    Field::color(
        label,
        move |cx: &App| -> Hsla {
            read_frvp_params(symbol_for_get.clone(), id, cx, &get_clone)
                .unwrap_or_else(|| ColorBlob::from_hsla(gpui::hsla(0.0, 0.0, 0.5, 1.0)))
                .into_hsla()
        },
        move |color: Hsla, cx: &mut App| {
            let symbol = symbol.clone();
            let set_field = set_field.clone();
            mutate_frvp_params(symbol, id, cx, move |p| {
                set_field(p, ColorBlob::from_hsla(color));
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

fn missing_body(msg: &'static str, color: Hsla) -> impl IntoElement {
    div()
        .p_4()
        .text_sm()
        .text_color(color)
        .child(SharedString::from(msg))
}

/// View-side snapshot of the bits of a [`Drawing`] this window reads.
/// Keeping it Clone-only avoids holding the service borrow across the
/// rest of render.
struct DrawingSnapshot {
    label: String,
    hidden: bool,
    tf_filter: Option<BTreeSet<String>>,
    origin: DrawingOrigin,
    /// Per-shape extras that drive the variant-specific sections of the
    /// settings window. `None` when the selected shape doesn't expose
    /// that knob.
    ray_extend_left: Option<bool>,
    text_font_size: Option<f32>,
    /// FRVP-only: a clone of the persisted params. The settings window's
    /// controls read defaults from here on every render and write back
    /// through `DrawingService::set_vp_params`.
    frvp_params: Option<VolumeProfileParams>,
}

trait DrawingRefProxy {
    fn as_ref_proxy(&self) -> &Drawing;
}

impl DrawingRefProxy for &Drawing {
    fn as_ref_proxy(&self) -> &Drawing {
        self
    }
}

impl From<&Drawing> for DrawingSnapshot {
    fn from(d: &Drawing) -> Self {
        let ray_extend_left = match &d.shape {
            DrawingShape::HorizontalRay(r) => Some(r.extend_left),
            _ => None,
        };
        let text_font_size = match &d.shape {
            DrawingShape::Text(t) => Some(t.font_size),
            _ => None,
        };
        let frvp_params = match &d.shape {
            DrawingShape::Frvp(f) => Some(f.params.clone()),
            _ => None,
        };
        Self {
            label: d.label(),
            hidden: d.hidden,
            tf_filter: d.tf_filter.clone(),
            origin: d.created_by,
            ray_extend_left,
            text_font_size,
            frvp_params,
        }
    }
}
