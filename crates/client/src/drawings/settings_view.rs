//! Floating per-drawing settings window. Mounted by the workspace when
//! the strip's gear button fires. Exposes the dials that wouldn't fit
//! comfortably on the strip itself — visibility, the per-timeframe
//! filter, shape-specific knobs (ray extend-left, text font-size), and the
//! full FRVP volume-profile form.
//!
//! The body is a single declarative [`SettingsForm`] — same framework the
//! indicator settings use. Common controls (Visible / Visible-on) live in a
//! "General" group; FRVP drawings add POC / VA groups (so the form switches
//! to the sidebar layout, matching the VRVP indicator).
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
use gpui_component::{ActiveTheme as _, v_flex};

use crate::services::market_data::Timeframe;
use crate::settings_form::{
    DropdownOption, Field, MultiCheckItem, NumberOpts, SettingsForm, SettingsGroup,
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

/// Discrete font-size choices offered for Text drawings.
const FONT_SIZE_CHOICES: &[f32] = &[10.0, 12.0, 14.0, 16.0, 20.0, 24.0];

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
        let fg = cx.theme().foreground;
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

        // Whole body is one declarative form — see module docs. Mirrors
        // `indicator_settings.rs`: header + divider + form body, wrapped in
        // a single scroll container.
        let form = build_drawing_form(symbol.clone(), id, &snap);
        let form_body = form.render(window, cx);

        let body = v_flex()
            .w_full()
            .p_4()
            .gap_3()
            .child(
                v_flex()
                    .gap_1()
                    .child(div().text_sm().text_color(fg).child(header_label))
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(muted)
                            .child(SharedString::from(origin_text)),
                    ),
            )
            .child(div().h(px(1.)).bg(border))
            .child(form_body);

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
                    .child(body),
            )
            .into_any_element()
    }
}

/// Build the unified per-drawing settings form. Every drawing gets a
/// "General" group (Visible toggle + per-timeframe filter + shape-specific
/// toggles); FRVP drawings additionally append the volume/POC/VA groups so
/// the form renders with the sidebar layout (same as the VRVP indicator).
fn build_drawing_form(symbol: SharedString, id: DrawingId, snap: &DrawingSnapshot) -> SettingsForm {
    let form_id = SharedString::from(format!("drawing-{}", id));

    // ── General group ──
    let mut general = SettingsGroup::new("General");

    // Visibility: switch reflects "visible" (= !hidden); the click always
    // means flip, so the setter just toggles regardless of the new value.
    general = general.item(Field::switch(
        "Visible",
        drawing_get_bool(symbol.clone(), id, |d| !d.hidden, true),
        drawing_toggle_hidden(symbol.clone(), id),
    ));

    // Visible-on: one checkbox per timeframe. tf_filter == None means "all
    // timeframes" (every box checked); toggling collapses back to None when
    // all are re-selected (handled service-side).
    let tf_items: Vec<MultiCheckItem> = Timeframe::ALL
        .iter()
        .map(|tf| {
            let tf = *tf;
            let tf_str = tf.as_str();
            MultiCheckItem::new(
                tf_str,
                drawing_get_bool(symbol.clone(), id, move |d| tf_active(d, tf_str), true),
                drawing_toggle_tf(symbol.clone(), id, tf),
            )
        })
        .collect();
    general = general.item(
        Field::multi_checkbox("Visible on", tf_items)
            .description("Timeframes this drawing appears on."),
    );

    // Reset-to-all only matters once a filter is active.
    {
        let sym_pred = symbol.clone();
        general = general.item(
            Field::action("", "Reset to all timeframes", drawing_reset_tf(symbol.clone(), id))
                .visible_if(move |cx| {
                    read_drawing(sym_pred.as_ref(), id, cx, |d| d.tf_filter.is_some())
                        .unwrap_or(false)
                }),
        );
    }

    // ── Shape-specific General extras ──
    if snap.ray_extend_left.is_some() {
        general = general.item(Field::switch(
            "Extend left",
            drawing_get_bool(
                symbol.clone(),
                id,
                |d| matches!(&d.shape, DrawingShape::HorizontalRay(r) if r.extend_left),
                false,
            ),
            drawing_toggle_ray(symbol.clone(), id),
        ));
    }

    if snap.text_font_size.is_some() {
        let opts = FONT_SIZE_CHOICES
            .iter()
            .map(|&s| DropdownOption::new(font_value(s), format!("{}px", s as u32)))
            .collect();
        general = general.item(Field::dropdown(
            "Font size",
            opts,
            drawing_get_str(symbol.clone(), id, |d| match &d.shape {
                DrawingShape::Text(t) => font_value(t.font_size),
                _ => SharedString::default(),
            }),
            drawing_set_font(symbol.clone(), id),
        ));
    }

    let is_frvp = snap.frvp_params.is_some();
    let mut extra_groups: Vec<SettingsGroup> = Vec::new();

    if is_frvp {
        let _ = BTCUSDT_TICK_SIZE;

        // ── visibility predicates (mirror the VRVP indicator form) ──
        let read_pred =
            |symbol: SharedString, id: DrawingId, f: fn(&VolumeProfileParams) -> bool| {
                move |cx: &App| -> bool {
                    read_frvp_params(symbol.clone(), id, cx, &f).unwrap_or(false)
                }
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
        // Delta scaling only affects pure Delta mode — VolDeltaOutline forces
        // per-row scaling internally, so the knob is meaningless there.
        let is_pure_delta_pred =
            |symbol: SharedString, id: DrawingId| -> Box<dyn Fn(&App) -> bool + 'static> {
                Box::new(move |cx| {
                    read_frvp_params(symbol.clone(), id, cx, &|p: &VolumeProfileParams| {
                        matches!(p.render_mode, VpRenderMode::Delta)
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
            let pred = is_pure_delta_pred(symbol.clone(), id);
            move |cx| pred(cx)
        });

        let width_field = Field::number(
            "Width",
            NumberOpts::int(WIDTH_PCT_MIN as i64, WIDTH_PCT_MAX as i64)
                .with_step(5.0)
                .format(|v| SharedString::from(format!("{}%", v.round() as i64))),
            getter_f64(symbol.clone(), id, |p| p.width_pct as f64),
            setter_frvp(symbol.clone(), id, |p, v: f64| {
                let nxt = v.round().clamp(WIDTH_PCT_MIN as f64, WIDTH_PCT_MAX as f64);
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
            let pred = is_delta_pred(symbol.clone(), id);
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
            let pred = is_delta_pred(symbol.clone(), id);
            move |cx| pred(cx)
        });

        general = general
            .item(bucket_field)
            .item(mode_field)
            .item(scale_field)
            .item(width_field)
            .item(anchor_field)
            .item(volume_color)
            .item(bull_color)
            .item(bear_color);

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
        .visible_if(read_pred(symbol.clone(), id, |p| p.show_poc));

        let show_va_field = Field::switch(
            "Show VA",
            getter_bool(symbol.clone(), id, |p| p.show_va),
            setter_frvp(symbol.clone(), id, |p, v: bool| p.show_va = v),
        );
        let va_pct_field = Field::number(
            "VA %",
            NumberOpts::int(VA_PERCENT_MIN as i64, VA_PERCENT_MAX as i64)
                .with_step(1.0)
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
        let va_color = make_color_field(
            "VA color",
            symbol.clone(),
            id,
            |p| p.color_va,
            |p, c| p.color_va = c,
        )
        .visible_if(read_pred(symbol.clone(), id, |p| p.show_va));

        extra_groups.push(SettingsGroup::new("POC").item(show_poc_field).item(poc_color));
        extra_groups.push(
            SettingsGroup::new("VA")
                .item(show_va_field)
                .item(va_pct_field)
                .item(show_va_hl_field)
                .item(va_color),
        );
    }

    let mut form = SettingsForm::new(form_id).group(general);
    for g in extra_groups {
        form = form.group(g);
    }
    form
}

// ─────────── common drawing read/write helpers (settings_form glue) ──────────

/// Read a field off the live drawing. Returns `None` if the service is gone
/// or the drawing was deleted between snapshot and closure call.
fn read_drawing<R>(
    symbol: &str,
    id: DrawingId,
    cx: &App,
    f: impl FnOnce(&Drawing) -> R,
) -> Option<R> {
    let handle = cx.try_global::<DrawingServiceHandle>().cloned()?;
    let svc = handle.0.read(cx);
    let d = svc.for_symbol(symbol).iter().find(|d| d.id == id)?;
    Some(f(d))
}

/// Run a mutation against the live drawing service (settings setters only
/// get `&mut App`, so they route through the global handle rather than an
/// action dispatch, which would need a `Window`).
fn with_service(cx: &mut App, f: impl FnOnce(&mut super::service::DrawingService, &mut Context<super::service::DrawingService>)) {
    if let Some(handle) = cx.try_global::<DrawingServiceHandle>().cloned() {
        handle.0.update(cx, |s, cx| f(s, cx));
    }
}

fn drawing_get_bool(
    symbol: SharedString,
    id: DrawingId,
    f: impl Fn(&Drawing) -> bool + 'static,
    default: bool,
) -> impl Fn(&App) -> bool + 'static {
    move |cx| read_drawing(symbol.as_ref(), id, cx, &f).unwrap_or(default)
}

fn drawing_get_str(
    symbol: SharedString,
    id: DrawingId,
    f: impl Fn(&Drawing) -> SharedString + 'static,
) -> impl Fn(&App) -> SharedString + 'static {
    move |cx| read_drawing(symbol.as_ref(), id, cx, &f).unwrap_or_default()
}

fn drawing_toggle_hidden(
    symbol: SharedString,
    id: DrawingId,
) -> impl Fn(bool, &mut App) + 'static {
    move |_new, cx| {
        let symbol = symbol.clone();
        with_service(cx, move |s, cx| s.toggle_hidden(symbol.as_ref(), id, cx));
    }
}

fn drawing_toggle_tf(
    symbol: SharedString,
    id: DrawingId,
    tf: Timeframe,
) -> impl Fn(bool, &mut App) + 'static {
    move |_new, cx| {
        let symbol = symbol.clone();
        with_service(cx, move |s, cx| s.toggle_tf_filter(symbol.as_ref(), id, tf, cx));
    }
}

fn drawing_reset_tf(symbol: SharedString, id: DrawingId) -> impl Fn(&mut App) + 'static {
    move |cx| {
        let symbol = symbol.clone();
        with_service(cx, move |s, cx| s.reset_tf_filter(symbol.as_ref(), id, cx));
    }
}

fn drawing_toggle_ray(symbol: SharedString, id: DrawingId) -> impl Fn(bool, &mut App) + 'static {
    move |_new, cx| {
        let symbol = symbol.clone();
        with_service(cx, move |s, cx| s.toggle_ray_extend_left(symbol.as_ref(), id, cx));
    }
}

fn drawing_set_font(
    symbol: SharedString,
    id: DrawingId,
) -> impl Fn(SharedString, &mut App) + 'static {
    move |v, cx| {
        let Ok(size_px) = v.as_ref().parse::<f32>() else {
            return;
        };
        let symbol = symbol.clone();
        with_service(cx, move |s, cx| s.set_text_font_size(symbol.as_ref(), id, size_px, cx));
    }
}

fn tf_active(d: &Drawing, tf_str: &str) -> bool {
    match &d.tf_filter {
        None => true,
        Some(set) => set.contains(tf_str),
    }
}

/// Canonical dropdown value for a font size — the integer pixel count.
fn font_value(size_px: f32) -> SharedString {
    SharedString::from((size_px.round() as i64).to_string())
}

// ─────────── FRVP read/write helpers (settings_form glue) ───────────

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
        let Some(d) = svc.for_symbol(symbol.as_ref()).iter().find(|d| d.id == id) else {
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

fn getter_f64<F>(symbol: SharedString, id: DrawingId, f: F) -> impl Fn(&App) -> f64 + 'static
where
    F: Fn(&VolumeProfileParams) -> f64 + 'static,
{
    move |cx| read_frvp_params(symbol.clone(), id, cx, &f).unwrap_or(0.0)
}

fn getter_str<F>(symbol: SharedString, id: DrawingId, f: F) -> impl Fn(&App) -> SharedString + 'static
where
    F: Fn(&VolumeProfileParams) -> SharedString + 'static,
{
    move |cx| read_frvp_params(symbol.clone(), id, cx, &f).unwrap_or_default()
}

fn getter_bool<F>(symbol: SharedString, id: DrawingId, f: F) -> impl Fn(&App) -> bool + 'static
where
    F: Fn(&VolumeProfileParams) -> bool + 'static,
{
    move |cx| read_frvp_params(symbol.clone(), id, cx, &f).unwrap_or(false)
}

fn setter_frvp<T, F>(symbol: SharedString, id: DrawingId, f: F) -> impl Fn(T, &mut App) + 'static
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
    #[allow(dead_code)]
    hidden: bool,
    #[allow(dead_code)]
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
