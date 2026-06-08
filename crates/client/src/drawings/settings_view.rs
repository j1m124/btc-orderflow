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
    App, AppContext as _, Context, Entity, FocusHandle, Focusable, Hsla,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Window, div, px,
};
use gpui_component::{
    ActiveTheme as _, Sizable as _,
    button::{Button, ButtonVariants as _},
    color_picker::{ColorPicker, ColorPickerEvent, ColorPickerState},
    h_flex, v_flex,
};

use crate::services::market_data::Timeframe;

use super::actions::{
    ResetDrawingTfFilter, SetTextFontSize, ToggleDrawingHidden, ToggleDrawingTfFilter,
    ToggleRayExtendLeft,
};
use super::service::{DrawingId, DrawingServiceHandle};
use super::shapes::{Drawing, DrawingOrigin, DrawingShape};
use crate::volume_profile::{
    AnchorEdge, VolumeProfileParams, VpDeltaScale, VpRenderMode,
    params::{
        BTCUSDT_TICK_SIZE, BUCKET_TICKS_MAX, BUCKET_TICKS_MIN, VA_PERCENT_MAX, VA_PERCENT_MIN,
        WIDTH_PCT_MAX, WIDTH_PCT_MIN,
    },
};

/// Per-drawing settings window content. Owns nothing the strip already
/// owns — colour / width / label / lock / delete live there. This view
/// is for the lower-frequency settings that benefit from a roomier UI.
pub struct DrawingSettingsView {
    target: (SharedString, DrawingId),
    focus: FocusHandle,
    /// FRVP-only colour pickers. Five slots in the canonical order
    /// `(Volume, Bull, Bear, POC, VA)` — mirrors `VolumeProfileParams`'s
    /// `color_*` fields. Rebuilt on `retarget` so flipping from an FRVP
    /// to a non-FRVP drops the states cleanly; rebuilt to a fresh set
    /// when switching between two FRVPs so each picker's popover state
    /// starts from the new instance's persisted colour.
    frvp_color_states: Vec<Entity<ColorPickerState>>,
    /// Subscriptions parallel to `frvp_color_states`. Each closure
    /// dispatches a `set_vp_params` write that flips just the
    /// corresponding `color_*` field on the latest persisted params.
    _frvp_color_subs: Vec<Subscription>,
}

/// Canonical slot ordering for FRVP colours. Keeps the picker → param
/// field mapping unambiguous and lets `rebuild_frvp_color_states` loop
/// once instead of per-slot.
#[derive(Clone, Copy, Debug)]
enum FrvpColorSlot {
    Volume,
    Bull,
    Bear,
    Poc,
    Va,
}

impl FrvpColorSlot {
    const ALL: &'static [FrvpColorSlot] = &[
        FrvpColorSlot::Volume,
        FrvpColorSlot::Bull,
        FrvpColorSlot::Bear,
        FrvpColorSlot::Poc,
        FrvpColorSlot::Va,
    ];

    fn label(self) -> &'static str {
        match self {
            FrvpColorSlot::Volume => "Volume",
            FrvpColorSlot::Bull => "Bull",
            FrvpColorSlot::Bear => "Bear",
            FrvpColorSlot::Poc => "POC",
            FrvpColorSlot::Va => "VA",
        }
    }

    fn read(self, p: &VolumeProfileParams) -> Hsla {
        match self {
            FrvpColorSlot::Volume => p.color_volume.into_hsla(),
            FrvpColorSlot::Bull => p.color_bull.into_hsla(),
            FrvpColorSlot::Bear => p.color_bear.into_hsla(),
            FrvpColorSlot::Poc => p.color_poc.into_hsla(),
            FrvpColorSlot::Va => p.color_va.into_hsla(),
        }
    }

    fn write(self, p: &mut VolumeProfileParams, c: Hsla) {
        use crate::volume_profile::params::ColorBlob;
        let blob = ColorBlob::from_hsla(c);
        match self {
            FrvpColorSlot::Volume => p.color_volume = blob,
            FrvpColorSlot::Bull => p.color_bull = blob,
            FrvpColorSlot::Bear => p.color_bear = blob,
            FrvpColorSlot::Poc => p.color_poc = blob,
            FrvpColorSlot::Va => p.color_va = blob,
        }
    }
}

impl DrawingSettingsView {
    pub fn new(
        symbol: SharedString,
        id: DrawingId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self {
            target: (symbol, id),
            focus: cx.focus_handle(),
            frvp_color_states: Vec::new(),
            _frvp_color_subs: Vec::new(),
        };
        this.rebuild_frvp_color_states(window, cx);
        this
    }

    /// Re-point the window at a different drawing. Used when the user
    /// selects another drawing and clicks its gear — keeps a single
    /// window instead of a stack.
    pub fn retarget(
        &mut self,
        symbol: SharedString,
        id: DrawingId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.target = (symbol, id);
        self.rebuild_frvp_color_states(window, cx);
        cx.notify();
    }

    pub fn current_target(&self) -> &(SharedString, DrawingId) {
        &self.target
    }

    /// (Re)build the FRVP color picker states for the current target.
    /// Drops everything when the target isn't an FRVP — keeps the
    /// non-FRVP render path free of stale subscriptions firing into a
    /// shape that wouldn't accept the colour write anyway.
    fn rebuild_frvp_color_states(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.frvp_color_states.clear();
        self._frvp_color_subs.clear();
        let (symbol, id) = self.target.clone();
        let Some(handle) = cx.try_global::<DrawingServiceHandle>().cloned() else {
            return;
        };
        let params = {
            let svc = handle.0.read(cx);
            let Some(d) = svc.for_symbol(symbol.as_ref()).iter().find(|d| d.id == id) else {
                return;
            };
            match &d.shape {
                DrawingShape::Frvp(f) => f.params.clone(),
                _ => return,
            }
        };
        for slot in FrvpColorSlot::ALL.iter().copied() {
            let initial = slot.read(&params);
            let state =
                cx.new(|cx| ColorPickerState::new(window, cx).default_value(initial));
            let sym = symbol.clone();
            let sub = cx.subscribe(&state, move |_this, _state, ev: &ColorPickerEvent, cx| {
                let ColorPickerEvent::Change(color) = ev;
                let Some(c) = color else {
                    // Clearing the picker resets the slot to the default
                    // colour. Read the default from a fresh `Default`
                    // params struct so future palette tweaks track
                    // automatically.
                    let defaults = VolumeProfileParams::default();
                    let reset_color = slot.read(&defaults);
                    mutate_frvp_params(sym.clone(), id, cx, |p| slot.write(p, reset_color));
                    return;
                };
                let c = *c;
                mutate_frvp_params(sym.clone(), id, cx, |p| slot.write(p, c));
            });
            self.frvp_color_states.push(state);
            self._frvp_color_subs.push(sub);
        }
    }
}

impl Focusable for DrawingSettingsView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for DrawingSettingsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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

        // ── FRVP-only: full VolumeProfile form ──────────────────────────
        // Mirrors the VRVP form in `indicator_settings.rs::render_vrvp` but
        // writes through `DrawingService::set_vp_params` instead of going
        // through the indicator path. Colour pickers are mode-aware: the
        // Bull/Bear slots only render in Delta / Volume+Delta modes
        // because they're only consumed there.
        if let Some(params) = snap.frvp_params.clone() {
            root = root.child(frvp_section(
                symbol.clone(),
                id,
                &params,
                &self.frvp_color_states,
                muted,
                border,
            ));
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

/// The FRVP settings block — Layout / Reference levels / Reset.
/// Static (no listener state); the only mutation is via the per-button
/// click handlers, which read the latest params from the service on
/// every fire.
fn frvp_section(
    symbol: SharedString,
    id: DrawingId,
    params: &VolumeProfileParams,
    color_states: &[Entity<ColorPickerState>],
    muted: Hsla,
    border: Hsla,
) -> impl IntoElement {
    let layout = v_flex()
        .gap_2()
        .child(section_label("Layout", muted))
        .child(frvp_bucket_row(symbol.clone(), id, params, muted))
        .child(frvp_mode_row(symbol.clone(), id, params.render_mode, muted));
    let layout = if matches!(params.render_mode, VpRenderMode::Delta) {
        layout.child(frvp_delta_scale_row(
            symbol.clone(),
            id,
            params.delta_scale,
            muted,
        ))
    } else {
        layout
    };
    let layout = layout
        .child(frvp_width_row(symbol.clone(), id, params, muted))
        .child(frvp_anchor_row(symbol.clone(), id, params.anchor, muted));

    let levels = v_flex()
        .gap_2()
        .child(section_label("Reference levels", muted))
        .child(frvp_toggle_row("POC line", params.show_poc, symbol.clone(), id, muted, "poc",
            |p, v| p.show_poc = v))
        .child(frvp_toggle_row("VA lines", params.show_va, symbol.clone(), id, muted, "va",
            |p, v| p.show_va = v))
        .child(frvp_toggle_row("VA highlight", params.show_va_highlight, symbol.clone(), id, muted, "vah",
            |p, v| p.show_va_highlight = v))
        .child(frvp_toggle_row("Labels", params.show_labels, symbol.clone(), id, muted, "lbl",
            |p, v| p.show_labels = v))
        .child(frvp_va_pct_row(symbol.clone(), id, params, muted));

    let symbol_for_reset = symbol.clone();
    let reset_btn = Button::new(SharedString::from(format!("frvp-reset-{}", id)))
        .label(SharedString::from("Reset style"))
        .small()
        .ghost()
        .on_click(move |_, _, cx| {
            let sym = symbol_for_reset.clone();
            mutate_frvp_params(sym, id, cx, |p| p.reset_styles());
        });

    // Colour pickers — mode-conditional. Volume is always relevant;
    // Bull/Bear only matter for Delta + Volume+Delta-outline; POC/VA
    // are tied to the reference-level toggles below them but render
    // unconditionally for consistency (toggling the line off still
    // leaves the slot meaningful for re-enabling later).
    let mut colors = v_flex().gap_2().child(section_label("Colors", muted));
    let mode = params.render_mode;
    let show_bull_bear = matches!(mode, VpRenderMode::Delta | VpRenderMode::VolDeltaOutline);
    let show_volume = !matches!(mode, VpRenderMode::Delta);
    for (idx, slot) in FrvpColorSlot::ALL.iter().copied().enumerate() {
        let visible = match slot {
            FrvpColorSlot::Volume => show_volume,
            FrvpColorSlot::Bull | FrvpColorSlot::Bear => show_bull_bear,
            FrvpColorSlot::Poc | FrvpColorSlot::Va => true,
        };
        if !visible {
            continue;
        }
        let Some(state) = color_states.get(idx) else {
            continue;
        };
        colors = colors.child(frvp_color_row(slot.label(), state, muted));
    }

    v_flex()
        .gap_3()
        .child(div().h(px(1.)).bg(border))
        .child(section_label("Volume Profile", muted))
        .child(layout)
        .child(div().h(px(1.)).bg(border))
        .child(levels)
        .child(div().h(px(1.)).bg(border))
        .child(colors)
        .child(reset_btn)
}

fn frvp_color_row(
    label: &'static str,
    state: &Entity<ColorPickerState>,
    muted: Hsla,
) -> impl IntoElement {
    h_flex()
        .gap_3()
        .items_center()
        .child(div().w(px(90.)).text_sm().text_color(muted).child(label))
        .child(ColorPicker::new(state).small())
}

fn frvp_bucket_row(
    symbol: SharedString,
    id: DrawingId,
    params: &VolumeProfileParams,
    muted: Hsla,
) -> impl IntoElement {
    let ticks = params.bucket_ticks;
    let dollars = ticks as f64 * BTCUSDT_TICK_SIZE;
    let readout = SharedString::from(format!("{} ticks (${:.2})", ticks, dollars));
    let sym_dec = symbol.clone();
    let sym_inc = symbol.clone();
    h_flex()
        .gap_3()
        .items_center()
        .child(div().w(px(90.)).text_sm().text_color(muted).child("Bucket"))
        .child(
            Button::new(SharedString::from(format!("frvp-bkt-dec-{}", id)))
                .label(SharedString::from("\u{2212}"))
                .xsmall()
                .ghost()
                .on_click(move |_, _, cx| {
                    let sym = sym_dec.clone();
                    mutate_frvp_params(sym, id, cx, |p| {
                        let nxt = (p.bucket_ticks as i64 - 10)
                            .clamp(BUCKET_TICKS_MIN as i64, BUCKET_TICKS_MAX as i64);
                        p.bucket_ticks = nxt as u32;
                    });
                }),
        )
        .child(div().w(px(110.)).text_sm().child(readout))
        .child(
            Button::new(SharedString::from(format!("frvp-bkt-inc-{}", id)))
                .label(SharedString::from("+"))
                .xsmall()
                .ghost()
                .on_click(move |_, _, cx| {
                    let sym = sym_inc.clone();
                    mutate_frvp_params(sym, id, cx, |p| {
                        let nxt = (p.bucket_ticks as i64 + 10)
                            .clamp(BUCKET_TICKS_MIN as i64, BUCKET_TICKS_MAX as i64);
                        p.bucket_ticks = nxt as u32;
                    });
                }),
        )
}

fn frvp_mode_row(
    symbol: SharedString,
    id: DrawingId,
    current: VpRenderMode,
    muted: Hsla,
) -> impl IntoElement {
    let mut buttons = h_flex().gap_1();
    for m in VpRenderMode::ALL {
        let mode = *m;
        let is_active = mode == current;
        let sym = symbol.clone();
        let btn_id = SharedString::from(format!("frvp-mode-{}-{}", id, mode.label()));
        let mut btn = Button::new(btn_id)
            .label(SharedString::from(mode.label()))
            .xsmall();
        btn = if is_active { btn.primary() } else { btn.ghost() };
        btn = btn.on_click(move |_, _, cx| {
            let sym = sym.clone();
            mutate_frvp_params(sym, id, cx, |p| p.render_mode = mode);
        });
        buttons = buttons.child(btn);
    }
    h_flex()
        .gap_3()
        .items_center()
        .child(div().w(px(90.)).text_sm().text_color(muted).child("Mode"))
        .child(buttons)
}

fn frvp_delta_scale_row(
    symbol: SharedString,
    id: DrawingId,
    current: VpDeltaScale,
    muted: Hsla,
) -> impl IntoElement {
    let mut buttons = h_flex().gap_1();
    for s in VpDeltaScale::ALL {
        let scale = *s;
        let is_active = scale == current;
        let sym = symbol.clone();
        let btn_id = SharedString::from(format!("frvp-scale-{}-{}", id, scale.label()));
        let mut btn = Button::new(btn_id)
            .label(SharedString::from(scale.label()))
            .xsmall();
        btn = if is_active { btn.primary() } else { btn.ghost() };
        btn = btn.on_click(move |_, _, cx| {
            let sym = sym.clone();
            mutate_frvp_params(sym, id, cx, |p| p.delta_scale = scale);
        });
        buttons = buttons.child(btn);
    }
    h_flex()
        .gap_3()
        .items_center()
        .child(div().w(px(90.)).text_sm().text_color(muted).child("Scaling"))
        .child(buttons)
}

fn frvp_width_row(
    symbol: SharedString,
    id: DrawingId,
    params: &VolumeProfileParams,
    muted: Hsla,
) -> impl IntoElement {
    let readout = SharedString::from(format!("{}%", params.width_pct));
    let sym_dec = symbol.clone();
    let sym_inc = symbol.clone();
    h_flex()
        .gap_3()
        .items_center()
        .child(div().w(px(90.)).text_sm().text_color(muted).child("Width"))
        .child(
            Button::new(SharedString::from(format!("frvp-w-dec-{}", id)))
                .label(SharedString::from("\u{2212}"))
                .xsmall()
                .ghost()
                .on_click(move |_, _, cx| {
                    let sym = sym_dec.clone();
                    mutate_frvp_params(sym, id, cx, |p| {
                        let nxt = (p.width_pct as i32 - 5)
                            .clamp(WIDTH_PCT_MIN as i32, WIDTH_PCT_MAX as i32);
                        p.width_pct = nxt as u8;
                    });
                }),
        )
        .child(div().w(px(60.)).text_sm().child(readout))
        .child(
            Button::new(SharedString::from(format!("frvp-w-inc-{}", id)))
                .label(SharedString::from("+"))
                .xsmall()
                .ghost()
                .on_click(move |_, _, cx| {
                    let sym = sym_inc.clone();
                    mutate_frvp_params(sym, id, cx, |p| {
                        let nxt = (p.width_pct as i32 + 5)
                            .clamp(WIDTH_PCT_MIN as i32, WIDTH_PCT_MAX as i32);
                        p.width_pct = nxt as u8;
                    });
                }),
        )
}

fn frvp_anchor_row(
    symbol: SharedString,
    id: DrawingId,
    current: AnchorEdge,
    muted: Hsla,
) -> impl IntoElement {
    let mut buttons = h_flex().gap_1();
    for a in AnchorEdge::ALL {
        let anchor = *a;
        let is_active = anchor == current;
        let sym = symbol.clone();
        let btn_id = SharedString::from(format!("frvp-anchor-{}-{}", id, anchor.label()));
        let mut btn = Button::new(btn_id)
            .label(SharedString::from(anchor.label()))
            .xsmall();
        btn = if is_active { btn.primary() } else { btn.ghost() };
        btn = btn.on_click(move |_, _, cx| {
            let sym = sym.clone();
            mutate_frvp_params(sym, id, cx, |p| p.anchor = anchor);
        });
        buttons = buttons.child(btn);
    }
    h_flex()
        .gap_3()
        .items_center()
        .child(div().w(px(90.)).text_sm().text_color(muted).child("Anchor"))
        .child(buttons)
}

fn frvp_toggle_row(
    label: &'static str,
    current: bool,
    symbol: SharedString,
    id: DrawingId,
    muted: Hsla,
    slot: &'static str,
    write: fn(&mut VolumeProfileParams, bool),
) -> impl IntoElement {
    let btn_id = SharedString::from(format!("frvp-tog-{}-{}", id, slot));
    let next = !current;
    let mut btn = Button::new(btn_id)
        .label(SharedString::from(if current { "On" } else { "Off" }))
        .xsmall();
    btn = if current { btn.primary() } else { btn.ghost() };
    let sym = symbol.clone();
    let btn = btn.on_click(move |_, _, cx| {
        let sym = sym.clone();
        mutate_frvp_params(sym, id, cx, |p| write(p, next));
    });
    h_flex()
        .gap_3()
        .items_center()
        .child(div().w(px(90.)).text_sm().text_color(muted).child(label))
        .child(btn)
}

fn frvp_va_pct_row(
    symbol: SharedString,
    id: DrawingId,
    params: &VolumeProfileParams,
    muted: Hsla,
) -> impl IntoElement {
    let readout = SharedString::from(format!("{}%", params.va_percent));
    let sym_dec = symbol.clone();
    let sym_inc = symbol.clone();
    h_flex()
        .gap_3()
        .items_center()
        .child(div().w(px(90.)).text_sm().text_color(muted).child("VA %"))
        .child(
            Button::new(SharedString::from(format!("frvp-va-dec-{}", id)))
                .label(SharedString::from("\u{2212}"))
                .xsmall()
                .ghost()
                .on_click(move |_, _, cx| {
                    let sym = sym_dec.clone();
                    mutate_frvp_params(sym, id, cx, |p| {
                        let nxt = (p.va_percent as i32 - 5)
                            .clamp(VA_PERCENT_MIN as i32, VA_PERCENT_MAX as i32);
                        p.va_percent = nxt as u8;
                    });
                }),
        )
        .child(div().w(px(60.)).text_sm().child(readout))
        .child(
            Button::new(SharedString::from(format!("frvp-va-inc-{}", id)))
                .label(SharedString::from("+"))
                .xsmall()
                .ghost()
                .on_click(move |_, _, cx| {
                    let sym = sym_inc.clone();
                    mutate_frvp_params(sym, id, cx, |p| {
                        let nxt = (p.va_percent as i32 + 5)
                            .clamp(VA_PERCENT_MIN as i32, VA_PERCENT_MAX as i32);
                        p.va_percent = nxt as u8;
                    });
                }),
        )
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
