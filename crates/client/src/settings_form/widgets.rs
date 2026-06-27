//! Field-renderer primitives + row/sidebar layout helpers. Each renderer
//! takes the typed get/set closures the `Field` carries and returns an
//! `AnyElement` that the form's content pane drops into a row.
//!
//! Stateful widgets (NumberInput, ColorPicker, TextInput) are kept alive
//! across re-renders via `window.use_keyed_state` keyed on the field's
//! row-key. The keyed state owns the widget's `Entity<...>` plus any
//! `Subscription`s that feed the captured setter closure when the user
//! edits the value.
//!
//! Tooltip-ⓘ pattern: the label column hosts a small `IconName::Info`
//! Button (no border, x-small) right next to the text. Hover shows the
//! field's description as a `gpui_component::tooltip`.

use std::rc::Rc;

use gpui::{
    AnyElement, App, Anchor, AppContext as _, Entity, Hsla, IntoElement, ParentElement as _,
    SharedString, Styled as _, Subscription, Window, div, px,
};
use gpui_component::{
    ActiveTheme as _, IconName, Sizable as _,
    button::{Button, ButtonVariants as _},
    checkbox::Checkbox,
    color_picker::{ColorPicker, ColorPickerEvent, ColorPickerState},
    h_flex,
    input::{Input, InputEvent, InputState, NumberInput, NumberInputEvent, StepAction},
    menu::{DropdownMenu as _, PopupMenuItem},
    slider::{Slider, SliderEvent, SliderState, SliderValue},
    switch::Switch,
    v_flex,
};

use crate::indicators::{COLOR_PALETTE_SIZE, palette_color_for};

use super::field::{DropdownOption, MultiCheckItem, NumberOpts};

const LABEL_WIDTH: gpui::Pixels = px(120.);
const ROW_GAP: gpui::Pixels = px(12.);

pub(super) fn muted_message(text: &'static str, cx: &mut App) -> impl IntoElement {
    div()
        .p_4()
        .text_sm()
        .text_color(cx.theme().muted_foreground)
        .child(SharedString::from(text))
}

pub(super) fn label_with_tooltip(
    label: SharedString,
    description: Option<SharedString>,
    row_key: SharedString,
    cx: &mut App,
) -> AnyElement {
    let muted = cx.theme().muted_foreground;
    let mut row = h_flex()
        .w(LABEL_WIDTH)
        .gap_1()
        .items_center()
        .child(div().text_sm().text_color(muted).child(label));
    if let Some(desc) = description {
        let info_id = SharedString::from(format!("{}-info", row_key));
        let info_btn = Button::new(info_id)
            .icon(IconName::Info)
            .ghost()
            .xsmall()
            .tooltip(desc);
        row = row.child(info_btn);
    }
    row.into_any_element()
}

pub(super) fn row(label: AnyElement, control: AnyElement, _cx: &mut App) -> impl IntoElement {
    h_flex()
        .w_full()
        .gap(ROW_GAP)
        .items_center()
        .child(label)
        .child(div().flex_1().min_w_0().child(control))
}

// ───────────────────────────── dropdown ─────────────────────────────

pub(super) fn render_dropdown(
    row_key: SharedString,
    options: Vec<DropdownOption>,
    get: Rc<dyn Fn(&App) -> SharedString>,
    set: Rc<dyn Fn(SharedString, &mut App)>,
    cx: &mut App,
) -> AnyElement {
    let current = get(cx);
    let current_label = options
        .iter()
        .find(|opt| opt.value == current)
        .map(|opt| opt.label.clone())
        .unwrap_or_else(|| current.clone());
    let btn_id = SharedString::from(format!("{}-dropdown", row_key));
    let opts_for_menu = options.clone();
    Button::new(btn_id)
        .label(current_label)
        .small()
        .outline()
        .dropdown_caret(true)
        .dropdown_menu_with_anchor(Anchor::TopLeft, move |mut menu, _, _| {
            let current = current.clone();
            for opt in opts_for_menu.iter() {
                let value = opt.value.clone();
                let label = opt.label.clone();
                let checked = value == current;
                let set = set.clone();
                menu = menu.item(
                    PopupMenuItem::new(label)
                        .checked(checked)
                        .on_click(move |_ev, _w, cx| {
                            set(value.clone(), cx);
                        }),
                );
            }
            menu
        })
        .into_any_element()
}

// ───────────────────────────── number ─────────────────────────────

struct NumberState {
    input: Entity<InputState>,
    last_set: f64,
    _subs: Vec<Subscription>,
}

pub(super) fn render_number(
    row_key: SharedString,
    opts: NumberOpts,
    get: Rc<dyn Fn(&App) -> f64>,
    set: Rc<dyn Fn(f64, &mut App)>,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let key = SharedString::from(format!("{}-number", row_key));
    let initial = get(cx);
    let opts_init = opts.clone();
    let opts_step = opts.clone();
    let opts_change = opts.clone();
    let get_step = get.clone();
    let set_step = set.clone();
    let set_change = set.clone();
    let state = window.use_keyed_state(key, cx, move |window, cx| {
        let display = format_display(initial, &opts_init);
        let input = cx.new(|cx| InputState::new(window, cx).default_value(display.to_string()));
        let step_sub = window.subscribe(&input, cx, move |input, ev: &NumberInputEvent, window, cx| {
            let NumberInputEvent::Step(action) = ev;
            // Step from the committed param value, not the displayed text:
            // formatted fields ("70%", "100 ticks") don't parse as a bare
            // f64, so reading the display would reset us to 0.
            let cur = get_step(cx);
            let next = match action {
                StepAction::Increment => cur + opts_step.step,
                StepAction::Decrement => cur - opts_step.step,
            };
            let next = next.clamp(opts_step.min, opts_step.max);
            input.update(cx, |input, cx| {
                input.set_value(format_display(next, &opts_step), window, cx);
            });
            set_step(next, cx);
        });
        let change_sub = window.subscribe(&input, cx, move |input, ev: &InputEvent, window, cx| {
            if !matches!(ev, InputEvent::PressEnter { .. } | InputEvent::Blur) {
                return;
            }
            let raw = input.read(cx).value();
            // Lenient parse so a manual edit that keeps the unit suffix
            // ("80%", "120 ticks") still commits — the field re-formats it
            // on the next render anyway.
            let Some(value) = parse_formatted(raw.as_ref()) else {
                return;
            };
            let clamped = value.clamp(opts_change.min, opts_change.max);
            if (clamped - value).abs() > f64::EPSILON {
                input.update(cx, |input, cx| {
                    input.set_value(format_display(clamped, &opts_change), window, cx);
                });
            }
            set_change(clamped, cx);
        });
        NumberState {
            input,
            last_set: initial,
            _subs: vec![step_sub, change_sub],
        }
    });

    // If the params changed underneath us (e.g., another control mutated
    // the same value), reflect the new value in the input.
    let state_ref = state.read(cx);
    if (state_ref.last_set - initial).abs() > f64::EPSILON {
        let input = state_ref.input.clone();
        let display = format_display(initial, &opts);
        state.update(cx, |s, _| s.last_set = initial);
        input.update(cx, |input, cx| {
            input.set_value(display, window, cx);
        });
    }

    let input = state.read(cx).input.clone();
    let number = NumberInput::new(&input).small();
    match opts.suffix.clone() {
        // Unit rendered beside the field so the editable value stays a bare
        // number (the input box never shows "%", "ticks", "h", …).
        Some(unit) => h_flex()
            .w_full()
            .items_center()
            .gap_2()
            .child(div().flex_1().min_w_0().child(number))
            .child(
                div()
                    .flex_none()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(unit),
            )
            .into_any_element(),
        None => number.into_any_element(),
    }
}

fn format_display(v: f64, opts: &NumberOpts) -> SharedString {
    if let Some(fmt) = opts.format.as_ref() {
        fmt(v)
    } else if opts.step.fract() == 0.0 {
        SharedString::from((v.round() as i64).to_string())
    } else {
        SharedString::from(format!("{}", v))
    }
}

/// Recover the numeric value from a (possibly formatted) input string.
/// Number fields display with arbitrary unit affixes ("70%", "100 ticks",
/// "$10"), so a bare `parse::<f64>()` rejects them; we scan for the first
/// float-like token (optional leading `-`, digits, single decimal point).
fn parse_formatted(s: &str) -> Option<f64> {
    let mut start: Option<usize> = None;
    let mut end = s.len();
    let mut dot_seen = false;
    for (i, ch) in s.char_indices() {
        match start {
            None => {
                if ch.is_ascii_digit() || ch == '-' {
                    start = Some(i);
                } else if ch == '.' {
                    start = Some(i);
                    dot_seen = true;
                }
            }
            Some(_) => {
                if ch.is_ascii_digit() {
                    continue;
                } else if ch == '.' && !dot_seen {
                    dot_seen = true;
                } else {
                    end = i;
                    break;
                }
            }
        }
    }
    s[start?..end].parse::<f64>().ok()
}

// ───────────────────────────── slider ─────────────────────────────

struct SliderFieldState {
    state: Entity<SliderState>,
    last_set: f64,
    _sub: Subscription,
}

pub(super) fn render_slider(
    row_key: SharedString,
    opts: NumberOpts,
    get: Rc<dyn Fn(&App) -> f64>,
    set: Rc<dyn Fn(f64, &mut App)>,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let key = SharedString::from(format!("{}-slider", row_key));
    let initial = get(cx);
    let (min, max, step) = (opts.min as f32, opts.max as f32, opts.step.max(f64::EPSILON) as f32);
    let set_for_state = set.clone();
    let slider = window.use_keyed_state(key, cx, move |window, cx| {
        let state = cx.new(|_| {
            SliderState::new()
                .min(min)
                .max(max)
                .step(step)
                .default_value(initial as f32)
        });
        let setter = set_for_state.clone();
        let sub = window.subscribe(&state, cx, move |_state, ev: &SliderEvent, _w, cx| {
            let SliderEvent::Change(value) = ev;
            setter(value.end() as f64, cx);
        });
        SliderFieldState {
            state,
            last_set: initial,
            _sub: sub,
        }
    });

    // Mirror an external param change (another control / reset) back into the
    // thumb position so the slider tracks the source of truth.
    let snapshot = slider.read(cx);
    let state_entity = snapshot.state.clone();
    if (snapshot.last_set - initial).abs() > f64::EPSILON {
        slider.update(cx, |s, _| s.last_set = initial);
        state_entity.update(cx, |s, cx| s.set_value(initial as f32, window, cx));
    }

    // Readout reflects the live thumb value during a drag (the entity updates
    // before the param round-trips), so it never lags the handle.
    let current = match state_entity.read(cx).value() {
        SliderValue::Single(v) => v as f64,
        SliderValue::Range(_, end) => end as f64,
    };
    let readout = value_with_unit(current, &opts);

    h_flex()
        .w_full()
        .items_center()
        .gap_3()
        .child(div().flex_1().min_w_0().child(Slider::new(&state_entity)))
        .child(
            div()
                .flex_none()
                .min_w(px(44.))
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child(readout),
        )
        .into_any_element()
}

/// Numeric display plus its unit suffix as one string (e.g. `85%`, `30 ticks`).
/// Symbolic units (`%`) butt against the number; word units (`ticks`) get a
/// space.
fn value_with_unit(v: f64, opts: &NumberOpts) -> SharedString {
    let num = format_display(v, opts);
    match opts.suffix.as_ref() {
        Some(unit) => {
            let sep = match unit.chars().next() {
                Some(c) if c.is_alphanumeric() => " ",
                _ => "",
            };
            SharedString::from(format!("{}{}{}", num, sep, unit))
        }
        None => num,
    }
}

// ───────────────────────────── switch / checkbox ─────────────────────────────

pub(super) fn render_switch(
    row_key: SharedString,
    get: Rc<dyn Fn(&App) -> bool>,
    set: Rc<dyn Fn(bool, &mut App)>,
    cx: &mut App,
) -> AnyElement {
    let checked = get(cx);
    let switch_id = SharedString::from(format!("{}-switch", row_key));
    Switch::new(switch_id)
        .checked(checked)
        .small()
        .on_click(move |new_checked, _w, cx| {
            set(*new_checked, cx);
        })
        .into_any_element()
}

pub(super) fn render_checkbox(
    row_key: SharedString,
    get: Rc<dyn Fn(&App) -> bool>,
    set: Rc<dyn Fn(bool, &mut App)>,
    _cx: &mut App,
) -> AnyElement {
    let cb_id = SharedString::from(format!("{}-checkbox", row_key));
    Checkbox::new(cb_id)
        .checked(get(_cx))
        .on_click(move |new_checked, _w, cx| {
            set(*new_checked, cx);
        })
        .into_any_element()
}

pub(super) fn render_multi_checkbox(
    row_key: SharedString,
    items: &[MultiCheckItem],
    cx: &mut App,
) -> AnyElement {
    let mut wrap = h_flex().w_full().gap_3().flex_wrap();
    for (ix, item) in items.iter().enumerate() {
        let id = SharedString::from(format!("{}-mc-{}", row_key, ix));
        let checked = (item.get)(cx);
        let set = item.set.clone();
        let mut cb = Checkbox::new(id)
            .label(item.label.clone())
            .checked(checked)
            .on_click(move |new_checked, _w, cx| {
                set(*new_checked, cx);
            });
        if let Some(desc) = item.description.clone() {
            cb = cb.tooltip(desc);
        }
        wrap = wrap.child(cb);
    }
    wrap.into_any_element()
}

// ───────────────────────────── color ─────────────────────────────

struct ColorState {
    state: Entity<ColorPickerState>,
    _sub: Subscription,
}

pub(super) fn render_color(
    row_key: SharedString,
    get: Rc<dyn Fn(&App) -> Hsla>,
    set: Rc<dyn Fn(Hsla, &mut App)>,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let key = SharedString::from(format!("{}-color", row_key));
    let initial = get(cx);
    let set_for_init = set.clone();
    let color_state = window.use_keyed_state(key, cx, move |window, cx| {
        let state = cx.new(|cx| ColorPickerState::new(window, cx).default_value(initial));
        let setter = set_for_init.clone();
        let sub = window.subscribe(&state, cx, move |_state, ev: &ColorPickerEvent, _w, cx| {
            if let ColorPickerEvent::Change(Some(color)) = ev {
                setter(*color, cx);
            }
        });
        ColorState { state, _sub: sub }
    });

    // Sync any external value change back into the picker so the swatch
    // mirrors `params.color` even when the picker isn't the one that set it.
    let state_entity = color_state.read(cx).state.clone();
    let current = state_entity.read(cx).value();
    if current != Some(initial) {
        state_entity.update(cx, |s, cx| s.set_value(initial, window, cx));
    }

    ColorPicker::new(&state_entity)
        .small()
        .featured_colors(featured_palette())
        .into_any_element()
}

pub fn featured_palette() -> Vec<Hsla> {
    (0..COLOR_PALETTE_SIZE).map(palette_color_for).collect()
}

// ───────────────────────────── text ─────────────────────────────

struct TextState {
    input: Entity<InputState>,
    last_set: SharedString,
    _sub: Subscription,
}

pub(super) fn render_text(
    row_key: SharedString,
    get: Rc<dyn Fn(&App) -> SharedString>,
    set: Rc<dyn Fn(SharedString, &mut App)>,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let key = SharedString::from(format!("{}-text", row_key));
    let initial = get(cx);
    let initial_for_state = initial.clone();
    let setter_for_state = set.clone();
    let state = window.use_keyed_state(key, cx, move |window, cx| {
        let initial = initial_for_state.clone();
        let input =
            cx.new(|cx| InputState::new(window, cx).default_value(initial.to_string()));
        let setter = setter_for_state.clone();
        let sub = window.subscribe(&input, cx, move |input, ev: &InputEvent, _w, cx| {
            if !matches!(ev, InputEvent::Blur | InputEvent::PressEnter { .. }) {
                return;
            }
            let value = input.read(cx).value();
            setter(SharedString::from(value.to_string()), cx);
        });
        TextState {
            input,
            last_set: initial,
            _sub: sub,
        }
    });

    let snapshot = state.read(cx);
    if snapshot.last_set != initial {
        let input = snapshot.input.clone();
        state.update(cx, |s, _| s.last_set = initial.clone());
        input.update(cx, |input, cx| {
            input.set_value(initial.clone(), window, cx);
        });
    }

    let input = state.read(cx).input.clone();
    Input::new(&input).small().into_any_element()
}

// ───────────────────────────── action ─────────────────────────────

pub(super) fn render_action(
    row_key: SharedString,
    button_label: SharedString,
    on_click: Rc<dyn Fn(&mut App)>,
    _cx: &mut App,
) -> AnyElement {
    let btn_id = SharedString::from(format!("{}-action", row_key));
    Button::new(btn_id)
        .label(button_label)
        .small()
        .ghost()
        .on_click(move |_ev, _w, cx| on_click(cx))
        .into_any_element()
}

// ───────────────────────────── sidebar entry helper ─────────────────────────────

#[allow(dead_code)]
pub(super) fn placeholder() -> impl IntoElement {
    v_flex().w_full()
}
