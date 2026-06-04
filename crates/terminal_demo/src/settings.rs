//! Settings dialog. Three sections — General, Theme, Keymap — laid out as a
//! left sidebar with a content pane. State lives in a `SettingsView` entity
//! held by the dialog; clicking a sidebar item updates the entity's `tab`
//! and `cx.notify()` repaints the content.
//!
//! The dialog is opened from the sidebar bottom rail
//! ([`crate::sidebar`]) — see [`open`].

use chrono_tz::Tz;
use gpui::{
    AppContext as _, Context, DismissEvent, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, StatefulInteractiveElement as _, Styled as _,
    Subscription, Window, div, px,
};
use gpui_component::{
    ActiveTheme as _, Disableable as _, IconName, Sizable as _, StyledExt as _, Theme,
    WindowExt as _,
    button::{Button, ButtonVariants as _},
    dialog::{self, DialogButtonProps},
    h_flex,
    input::{Input, InputEvent, InputState},
    popover::Popover,
    separator::Separator,
    switch::Switch,
    v_flex,
};

use crate::persistence::{self, CalendarPrefs, ChartPrefs};
use crate::prefs::{self, CalendarPrefsGlobal, ChartPrefsGlobal, TZ_PRESETS, UserTz};
use crate::themes;
use crate::top_bar::SetTheme;

// ---------------------------------------------------------------------------
// Font size — moved here from top_bar so the General section owns its
// behaviour. The `Theme` global is gpui-component's; we mutate `font_size`
// directly and persist it via the existing `persistence::save_font_size` path.
// ---------------------------------------------------------------------------

const FONT_SIZE_MIN: f32 = 10.0;
const FONT_SIZE_MAX: f32 = 28.0;
const FONT_SIZE_DEFAULT: f32 = 16.0;

fn adjust_font_size(delta: f32, window: &mut Window, cx: &mut gpui::App) {
    let current: f32 = cx.global::<Theme>().font_size.into();
    let next = (current + delta).clamp(FONT_SIZE_MIN, FONT_SIZE_MAX);
    if (next - current).abs() < 0.01 {
        return;
    }
    apply_font_size(next, window, cx);
}

fn apply_font_size(value: f32, window: &mut Window, cx: &mut gpui::App) {
    cx.global_mut::<Theme>().font_size = px(value);
    window.refresh();
    if let Err(err) = persistence::save_font_size(value) {
        log::warn!("save font size failed: {err:?}");
    }
}

// ---------------------------------------------------------------------------
// Chart pref bounds — match the slider ranges; values arriving from a stale
// config are clamped before being installed into the global on load.
// ---------------------------------------------------------------------------

const CANDLES_MIN: f32 = 10.0;
const CANDLES_MAX: f32 = 500.0;
const RIGHT_BUFFER_MIN: f32 = 0.0;
const RIGHT_BUFFER_MAX: f32 = 0.8;
const Y_PAD_MIN: f32 = 0.0;
const Y_PAD_MAX: f32 = 0.25;

#[derive(Copy, Clone, PartialEq, Eq)]
enum Tab {
    General,
    Theme,
    Keymap,
}

impl Tab {
    fn label(self) -> &'static str {
        match self {
            Tab::General => "General",
            Tab::Theme => "Theme",
            Tab::Keymap => "Keymap",
        }
    }
}

/// Filter for the theme grid. `All` is the default so the user sees the full
/// catalogue immediately; switching to Light/Dark hides the other half.
#[derive(Copy, Clone, PartialEq, Eq)]
enum ThemeFilter {
    All,
    Light,
    Dark,
}

impl ThemeFilter {
    fn label(self) -> &'static str {
        match self {
            ThemeFilter::All => "All",
            ThemeFilter::Light => "Light",
            ThemeFilter::Dark => "Dark",
        }
    }
}

pub struct SettingsView {
    tab: Tab,
    theme_filter: ThemeFilter,
    /// Search-bar input backing the Timezone dropdown's filter. Owned here
    /// (not inside `timezone_row`) so the InputState persists across renders
    /// — re-creating it every render would reset the cursor + typed text on
    /// every keystroke.
    tz_query: Entity<InputState>,
    /// Hold to keep the subscription alive — drops when SettingsView drops.
    _tz_query_sub: Subscription,
}

impl SettingsView {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let tz_query = cx.new(|cx| {
            InputState::new(window, cx).placeholder("Filter timezones…")
        });
        // Re-render the settings view on every keystroke so the popover's
        // content closure re-runs and the filtered city list updates.
        let sub = cx.subscribe(&tz_query, |_, _input, ev: &InputEvent, cx| {
            if matches!(ev, InputEvent::Change) {
                cx.notify();
            }
        });
        Self {
            tab: Tab::General,
            theme_filter: ThemeFilter::All,
            tz_query,
            _tz_query_sub: sub,
        }
    }

    fn set_tab(&mut self, tab: Tab, cx: &mut Context<Self>) {
        if self.tab == tab {
            return;
        }
        self.tab = tab;
        cx.notify();
    }

    fn set_theme_filter(&mut self, filter: ThemeFilter, cx: &mut Context<Self>) {
        if self.theme_filter == filter {
            return;
        }
        self.theme_filter = filter;
        cx.notify();
    }
}

/// Entry point invoked from the sidebar's Settings button (and from the old
/// `top_bar::open_settings_dialog` shim, for callers that haven't migrated).
pub fn open(window: &mut Window, cx: &mut gpui::App) {
    let view = cx.new(|cx| SettingsView::new(window, cx));
    window.open_dialog(cx, move |dialog, _, _| {
        // Fixed size: the dialog itself doesn't grow with content — the
        // inner panel scrolls. Width fits the keymap table without wrapping
        // the action column; height shows ~9 setting rows before scrolling.
        dialog
            .max_w(px(900.))
            .button_props(DialogButtonProps::default().ok_text("Done"))
            .child(view.clone())
    });
}

impl Render for SettingsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let border = theme.border;

        let tabs = h_flex()
            .w_full()
            .px_3()
            .pt_3()
            .pb_0()
            .gap_1()
            .border_b_1()
            .border_color(border)
            .child(tab_item(Tab::General, self.tab, cx))
            .child(tab_item(Tab::Theme, self.tab, cx))
            .child(tab_item(Tab::Keymap, self.tab, cx));

        let tz_query = self.tz_query.clone();
        let content = match self.tab {
            Tab::General => render_general(&tz_query, cx).into_any_element(),
            Tab::Theme => render_theme(self.theme_filter, cx).into_any_element(),
            Tab::Keymap => render_keymap(cx).into_any_element(),
        };

        // Outer column is fixed-height; inner content scrolls. `justify_start`
        // pins content to the top instead of the dialog's default centre.
        v_flex()
            .w_full()
            .h(px(560.))
            .child(tabs)
            .child(
                div()
                    .id("settings-scroll")
                    .flex_1()
                    .w_full()
                    .min_h_0()
                    .py_3()
                    .overflow_y_scroll()
                    .child(content),
            )
    }
}

fn tab_item(tab: Tab, active: Tab, cx: &mut Context<SettingsView>) -> impl IntoElement {
    let theme = cx.theme();
    let is_active = tab == active;
    // Active tab gets an accent underline (the bottom border bleeds over the
    // tab strip's own border below) so the selection reads cleanly without a
    // filled background fighting the dialog chrome.
    let underline = if is_active {
        theme.primary
    } else {
        gpui::transparent_black()
    };
    let fg = if is_active {
        theme.foreground
    } else {
        theme.muted_foreground
    };
    div()
        .px_3()
        .py_2()
        .border_b_2()
        .border_color(underline)
        .text_color(fg)
        .text_sm()
        .font_semibold()
        .cursor_pointer()
        .child(SharedString::from(tab.label()))
        .on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener(move |this, _, _, cx| this.set_tab(tab, cx)),
        )
}

// ---------------------------------------------------------------------------
// General
// ---------------------------------------------------------------------------

fn render_general(
    tz_query: &Entity<InputState>,
    cx: &mut Context<SettingsView>,
) -> impl IntoElement {
    let theme = cx.theme();
    let muted = theme.muted_foreground;
    v_flex()
        .gap_4()
        .child(section_header("Appearance", muted))
        .child(font_size_row(cx))
        .child(placeholder_row("Font family", "System default", muted))
        .child(animations_row(cx))
        .child(div().px_4().child(Separator::horizontal()))
        .child(section_header("Locale", muted))
        .child(placeholder_row("Language", "English (default)", muted))
        .child(timezone_row(tz_query, cx))
        .child(div().px_4().child(Separator::horizontal()))
        .child(section_header("Chart", muted))
        .child(candles_row(cx))
        .child(right_buffer_row(cx))
        .child(y_padding_row(cx))
        .child(session_markers_row(cx))
        .child(div().px_4().child(Separator::horizontal()))
        .child(section_header("Calendar", muted))
        .child(invert_macro_colors_row(cx))
        .child(div().px_4().child(Separator::horizontal()))
        .child(section_header("Layout", muted))
        .child(layout_reset_row(cx))
        .child(div().px_4().child(Separator::horizontal()))
        .child(reset_all_row(cx))
}

fn reset_all_row(_cx: &mut Context<SettingsView>) -> impl IntoElement {
    setting_row(
        "Reset all settings",
        "Restores font size, dialog animations, timezone, and chart defaults. Theme and saved layouts are not affected.",
        Button::new("reset-all-settings")
            .label("Reset all")
            .small()
            .danger()
            .on_click(|_, window, cx| reset_all_settings(window, cx)),
    )
}

/// Restore everything General owns to defaults. Theme + saved layouts are
/// intentionally left alone — those represent user choices the user almost
/// never wants nuked alongside a "fix my fiddly toggles" reset.
fn reset_all_settings(window: &mut Window, cx: &mut gpui::App) {
    // Font size
    apply_font_size(FONT_SIZE_DEFAULT, window, cx);
    // Dialog animations — default is on
    dialog::set_animations_enabled(true);
    if let Err(err) = persistence::save_dialog_animations(true) {
        log::warn!("save dialog animations failed: {err:?}");
    }
    // Timezone + chart + calendar prefs reset together
    prefs::set_tz(cx, None);
    prefs::set_chart_prefs(cx, ChartPrefs::default());
    prefs::set_calendar_prefs(cx, CalendarPrefs::default());
    window.refresh();
}

fn section_header(label: &'static str, muted: gpui::Hsla) -> impl IntoElement {
    div()
        .px_4()
        .text_xs()
        .text_color(muted)
        .child(SharedString::from(label))
}

fn font_size_row(_cx: &mut Context<SettingsView>) -> impl IntoElement {
    setting_row(
        "Font size",
        "Adjust the base UI font size. Persists across sessions.",
        h_flex()
            .gap_2()
            .items_center()
            .child(
                Button::new("font-minus")
                    .label("−")
                    .small()
                    .outline()
                    .on_click(|_, window, cx| adjust_font_size(-1.0, window, cx)),
            )
            .child(FontSizeReadout)
            .child(
                Button::new("font-plus")
                    .label("+")
                    .small()
                    .outline()
                    .on_click(|_, window, cx| adjust_font_size(1.0, window, cx)),
            )
            .child(undo_button("font-undo", "Reset to default font size", |_, window, cx| {
                apply_font_size(FONT_SIZE_DEFAULT, window, cx);
            })),
    )
}

/// Compact icon-only "reset to default" button used at the end of each
/// adjustable setting's control row. Same visual weight regardless of which
/// setting it sits next to, so the eye reads the row as "value + undo".
fn undo_button<F>(id: &'static str, tip: &'static str, on_click: F) -> impl IntoElement
where
    F: Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static,
{
    Button::new(id)
        .icon(IconName::Undo2)
        .small()
        .ghost()
        .tooltip(tip)
        .on_click(on_click)
}

#[derive(gpui::IntoElement)]
struct FontSizeReadout;

impl gpui::RenderOnce for FontSizeReadout {
    fn render(self, _window: &mut Window, cx: &mut gpui::App) -> impl IntoElement {
        let size: f32 = cx.global::<Theme>().font_size.into();
        div()
            .min_w(px(56.))
            .text_sm()
            .text_color(cx.theme().muted_foreground)
            .child(SharedString::from(format!("{size:.0} px")))
    }
}

fn animations_row(_cx: &mut Context<SettingsView>) -> impl IntoElement {
    let enabled = dialog::animations_enabled();
    setting_row(
        "Dialog animations",
        "Toggle the slide/fade-in animation when dialogs open.",
        Switch::new("dialog-animations-toggle")
            .checked(enabled)
            .label(if enabled { "On" } else { "Off" })
            .on_click(|checked, window, _cx| {
                dialog::set_animations_enabled(*checked);
                if let Err(err) = persistence::save_dialog_animations(*checked) {
                    log::warn!("save dialog animations failed: {err:?}");
                }
                window.refresh();
            }),
    )
}

fn placeholder_row(
    label: &'static str,
    value_label: &'static str,
    muted: gpui::Hsla,
) -> impl IntoElement {
    setting_row(
        label,
        "Coming soon.",
        h_flex()
            .gap_2()
            .items_center()
            .child(
                Button::new(SharedString::from(format!("{label}-placeholder")))
                    .label(SharedString::from(value_label))
                    .icon(IconName::ChevronDown)
                    .small()
                    .outline()
                    .disabled(true),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child("coming soon"),
            ),
    )
}

fn timezone_row(
    tz_query: &Entity<InputState>,
    cx: &mut Context<SettingsView>,
) -> impl IntoElement {
    let active = cx.global::<UserTz>().iana;
    let active_label: SharedString = match active {
        None => "Auto (system)".into(),
        Some(tz) => prefs::tz_display_label(tz.name()).into(),
    };
    // Pre-compute city labels with their current UTC offsets so the popover
    // content closure stays cheap and DST flips show up the next time the
    // dialog re-opens.
    let city_entries: Vec<(SharedString, SharedString)> = TZ_PRESETS
        .iter()
        .map(|(id, _)| {
            (
                SharedString::from(*id),
                SharedString::from(prefs::tz_display_label(id)),
            )
        })
        .collect();
    let tz_query = tz_query.clone();
    let trigger = Button::new("tz-dropdown")
        .label(active_label)
        .icon(IconName::ChevronDown)
        .small()
        .outline();
    // Popover instead of `dropdown_menu` so we can put a filter Input above
    // the list. Each row dispatches `SetTimezone` like the old menu items did
    // and emits `DismissEvent` to close the popover. The filter is
    // case-insensitive substring against either the display label or the
    // IANA id (so "shanghai" matches "Asia/Shanghai" the same as the
    // visible "Shanghai (UTC+8)" label).
    let popover = Popover::new("tz-popover")
        .trigger(trigger)
        .p_0()
        .on_open_change({
            let tz_query = tz_query.clone();
            move |open, window, cx| {
                if !*open {
                    // Reset the filter every time the popover closes so the
                    // next open starts fresh — keeping the prior query would
                    // mask entries the user doesn't expect to be filtered.
                    tz_query.update(cx, |input, cx| input.set_value("", window, cx));
                }
            }
        })
        .content(move |_, _, cx| {
            let tz_query = tz_query.clone();
            let city_entries = city_entries.clone();
            let query = tz_query.read(cx).value().to_string();
            let q = query.trim().to_lowercase();
            let matches = |label: &str, id: &str| -> bool {
                if q.is_empty() {
                    return true;
                }
                label.to_lowercase().contains(&q) || id.to_lowercase().contains(&q)
            };
            let mut list = v_flex()
                .w(px(280.))
                .child(
                    div()
                        .px_2()
                        .pt_2()
                        .pb_1()
                        .child(Input::new(&tz_query).small()),
                )
                .child(Separator::horizontal());
            let mut scroll = v_flex()
                .id("tz-list")
                .gap_0p5()
                .p_1()
                .max_h(px(320.))
                .overflow_y_scroll();
            if matches("Auto (system)", "auto") {
                scroll = scroll.child(tz_picker_row(cx, "tz-row-auto", "Auto (system)", None));
            }
            if matches("UTC", "UTC") {
                scroll = scroll.child(tz_picker_row(
                    cx,
                    "tz-row-utc",
                    "UTC",
                    Some(SharedString::from("UTC")),
                ));
            }
            for (id, label) in city_entries.iter() {
                if !matches(label, id) {
                    continue;
                }
                scroll = scroll.child(tz_picker_row(
                    cx,
                    SharedString::from(format!("tz-row-{id}")),
                    label.clone(),
                    Some(id.clone()),
                ));
            }
            list = list.child(scroll);
            list
        });
    setting_row(
        "Timezone",
        "Used for chart x-axis labels and the bottom-bar clock.",
        popover,
    )
}

/// One row inside the timezone-picker popover. Plain stateful div instead of
/// `Button` so the label sits left-aligned (Button hardcodes
/// `justify_center` at button.rs:475). Hover background is applied directly
/// via `.hover(...)`. Clicking dispatches `SetTimezone` and emits
/// `DismissEvent` to close the popover.
fn tz_picker_row(
    cx: &mut Context<gpui_component::popover::PopoverState>,
    id: impl Into<gpui::ElementId>,
    label: impl Into<SharedString>,
    iana: Option<SharedString>,
) -> impl IntoElement {
    let theme = cx.theme();
    let hover_bg = theme.accent;
    div()
        .id(id.into())
        .w_full()
        .px_2()
        .py_1()
        .text_sm()
        .text_color(theme.foreground)
        .rounded(px(4.0))
        .cursor_pointer()
        .hover(move |s| s.bg(hover_bg))
        .child(label.into())
        .on_click(cx.listener(move |_, _, window, cx| {
            window.dispatch_action(Box::new(SetTimezone(iana.clone())), cx);
            cx.emit(DismissEvent);
        }))
}

fn stepper_value(text: String, cx: &mut Context<SettingsView>) -> impl IntoElement {
    // Centered readout sandwiched between − / +. `min_w` reserves enough room
    // that the buttons don't jump when the digits change width (e.g. 90 → 100).
    div()
        .min_w(px(72.))
        .text_sm()
        .text_center()
        .text_color(cx.theme().muted_foreground)
        .child(SharedString::from(text))
}

fn candles_row(cx: &mut Context<SettingsView>) -> impl IntoElement {
    let value = cx.global::<ChartPrefsGlobal>().0.default_view;
    setting_row(
        "Default candle count",
        "How many candles a chart shows when it opens or after Reset scale.",
        h_flex()
            .gap_2()
            .items_center()
            .child(
                Button::new("candles-minus")
                    .label("−")
                    .small()
                    .outline()
                    .on_click(|_, _, cx| adjust_chart(cx, |p| p.default_view -= 10.0)),
            )
            .child(stepper_value(format!("{value:.0}"), cx))
            .child(
                Button::new("candles-plus")
                    .label("+")
                    .small()
                    .outline()
                    .on_click(|_, _, cx| adjust_chart(cx, |p| p.default_view += 10.0)),
            )
            .child(undo_button(
                "candles-undo",
                "Reset to default",
                |_, _, cx| adjust_chart(cx, |p| p.default_view = ChartPrefs::default().default_view),
            )),
    )
}

fn right_buffer_row(cx: &mut Context<SettingsView>) -> impl IntoElement {
    let value = cx.global::<ChartPrefsGlobal>().0.right_buffer;
    setting_row(
        "Right-edge buffer",
        "Empty space to the right of the live candle in sticky mode.",
        h_flex()
            .gap_2()
            .items_center()
            .child(
                Button::new("rbuf-minus")
                    .label("−")
                    .small()
                    .outline()
                    .on_click(|_, _, cx| adjust_chart(cx, |p| p.right_buffer -= 0.05)),
            )
            .child(stepper_value(format!("{:.0} %", value * 100.0), cx))
            .child(
                Button::new("rbuf-plus")
                    .label("+")
                    .small()
                    .outline()
                    .on_click(|_, _, cx| adjust_chart(cx, |p| p.right_buffer += 0.05)),
            )
            .child(undo_button(
                "rbuf-undo",
                "Reset to default",
                |_, _, cx| adjust_chart(cx, |p| p.right_buffer = ChartPrefs::default().right_buffer),
            )),
    )
}

fn y_padding_row(cx: &mut Context<SettingsView>) -> impl IntoElement {
    let value = cx.global::<ChartPrefsGlobal>().0.y_padding;
    setting_row(
        "Price-axis padding",
        "Vertical breathing room around the auto-fitted price range.",
        h_flex()
            .gap_2()
            .items_center()
            .child(
                Button::new("ypad-minus")
                    .label("−")
                    .small()
                    .outline()
                    .on_click(|_, _, cx| adjust_chart(cx, |p| p.y_padding -= 0.01)),
            )
            .child(stepper_value(format!("{:.0} %", value * 100.0), cx))
            .child(
                Button::new("ypad-plus")
                    .label("+")
                    .small()
                    .outline()
                    .on_click(|_, _, cx| adjust_chart(cx, |p| p.y_padding += 0.01)),
            )
            .child(undo_button(
                "ypad-undo",
                "Reset to default",
                |_, _, cx| adjust_chart(cx, |p| p.y_padding = ChartPrefs::default().y_padding),
            )),
    )
}

fn session_markers_row(cx: &mut Context<SettingsView>) -> impl IntoElement {
    let enabled = cx.global::<ChartPrefsGlobal>().0.session_markers;
    setting_row(
        "ETH session markers",
        "Show RTH Open/Close dashed lines when a chart is in Extended-hours mode.",
        Switch::new("session-markers-toggle")
            .checked(enabled)
            .label(if enabled { "On" } else { "Off" })
            .on_click(|checked, window, cx| {
                let checked = *checked;
                adjust_chart(cx, |p| p.session_markers = checked);
                window.refresh();
            }),
    )
}

// ---------------------------------------------------------------------------
// Calendar prefs
// ---------------------------------------------------------------------------

fn invert_macro_colors_row(cx: &mut Context<SettingsView>) -> impl IntoElement {
    let enabled = cx.global::<CalendarPrefsGlobal>().0.invert_macro_colors;
    // Note the trade-off explicitly so users opting out understand they're
    // accepting wrong-color rendering for inflation/unemployment events.
    setting_row(
        "Invert colors for inflation & unemployment",
        "When ON: a cooler-than-forecast inflation print (e.g. CPI under estimate) renders green, matching how macro desks read the data — cooler inflation is bullish for risk assets. When OFF: every event uses the same rule (actual ≥ forecast → green), which is correct for growth indicators like NFP/GDP but misleading for inflation and unemployment. The server tags each event with the appropriate convention; this toggle only controls whether the client honors it.",
        Switch::new("invert-macro-colors-toggle")
            .checked(enabled)
            .label(if enabled { "On" } else { "Off" })
            .on_click(|checked, window, cx| {
                let checked = *checked;
                let mut prefs = cx.global::<CalendarPrefsGlobal>().0.clone();
                prefs.invert_macro_colors = checked;
                prefs::set_calendar_prefs(cx, prefs);
                window.refresh();
            }),
    )
}

fn adjust_chart(cx: &mut gpui::App, mutate: impl FnOnce(&mut ChartPrefs)) {
    let mut prefs = cx.global::<ChartPrefsGlobal>().0.clone();
    mutate(&mut prefs);
    prefs.default_view = prefs.default_view.clamp(CANDLES_MIN, CANDLES_MAX);
    prefs.right_buffer = prefs.right_buffer.clamp(RIGHT_BUFFER_MIN, RIGHT_BUFFER_MAX);
    prefs.y_padding = prefs.y_padding.clamp(Y_PAD_MIN, Y_PAD_MAX);
    prefs::set_chart_prefs(cx, prefs);
}

fn layout_reset_row(_cx: &mut Context<SettingsView>) -> impl IntoElement {
    setting_row(
        "Reset layout",
        "Rebuilds the active mode's default layout.",
        Button::new("reset-layout")
            .label("Reset current mode")
            .small()
            .outline()
            .on_click(|_, window, cx| {
                window.dispatch_action(Box::new(crate::top_bar::ResetLayout), cx);
                window.close_dialog(cx);
            }),
    )
}

fn setting_row(
    title: &'static str,
    blurb: &'static str,
    control: impl IntoElement,
) -> impl IntoElement {
    v_flex()
        .px_4()
        .gap_1()
        .child(div().text_sm().font_semibold().child(SharedString::from(title)))
        .child(control)
        .child(
            div()
                .text_xs()
                .child(SharedString::from(blurb)),
        )
}

// ---------------------------------------------------------------------------
// Theme
// ---------------------------------------------------------------------------

fn render_theme(
    active_filter: ThemeFilter,
    cx: &mut Context<SettingsView>,
) -> impl IntoElement {
    let theme = cx.theme();
    let muted = theme.muted_foreground;
    let primary = theme.primary;
    let border = theme.border;
    let active_name: SharedString = if theme.mode.is_dark() {
        theme.dark_theme.name.clone()
    } else {
        theme.light_theme.name.clone()
    };

    let all_previews = themes::theme_previews(cx);
    let total = all_previews.len();
    let previews: Vec<themes::ThemePreview> = all_previews
        .into_iter()
        .filter(|p| match active_filter {
            ThemeFilter::All => true,
            ThemeFilter::Light => !p.mode.is_dark(),
            ThemeFilter::Dark => p.mode.is_dark(),
        })
        .collect();
    let count = previews.len();

    // Fixed 3-column grid. Cards use `flex_1` so each row shares the
    // available content width equally — that's how every card stays the
    // same size regardless of window width AND avoids overflowing the
    // dialog's rounded border. Short last rows get empty flex_1 placeholders
    // so their visible cards keep the column-1 / column-2 widths instead of
    // stretching to fill.
    const COLS: usize = 3;
    let mut grid = v_flex().px_4().gap_3();
    let mut idx = 0;
    while idx < count {
        let mut row = h_flex().w_full().gap_3();
        for col in 0..COLS {
            if let Some(p) = previews.get(idx + col) {
                row = row.child(theme_card(p, &active_name, primary, border));
            } else {
                row = row.child(div().flex_1().min_w(px(0.)));
            }
        }
        grid = grid.child(row);
        idx += COLS;
    }

    let filter_row = h_flex()
        .px_4()
        .gap_1()
        .child(filter_chip(ThemeFilter::All, active_filter, cx))
        .child(filter_chip(ThemeFilter::Light, active_filter, cx))
        .child(filter_chip(ThemeFilter::Dark, active_filter, cx));

    v_flex()
        .gap_3()
        .child(setting_row(
            "Colour scheme",
            "Each card's strip previews six theme colours, left → right: \
             background, foreground, border, accent, bullish (gains), bearish (losses).",
            div(),
        ))
        .child(filter_row)
        .child(grid)
        .child(
            div().px_4().text_xs().text_color(muted).child(SharedString::from(format!(
                "Showing {count} of {total} themes.",
            ))),
        )
}

fn filter_chip(
    filter: ThemeFilter,
    active: ThemeFilter,
    cx: &mut Context<SettingsView>,
) -> impl IntoElement {
    let theme = cx.theme();
    let is_active = filter == active;
    let bg = if is_active {
        theme.accent
    } else {
        gpui::transparent_black()
    };
    let fg = if is_active {
        theme.accent_foreground
    } else {
        theme.muted_foreground
    };
    div()
        .id(gpui::ElementId::Name(SharedString::from(format!(
            "theme-filter-{}",
            filter.label()
        ))))
        .px_3()
        .py_1()
        .rounded(px(6.))
        .text_xs()
        .font_semibold()
        .bg(bg)
        .text_color(fg)
        .cursor_pointer()
        .child(SharedString::from(filter.label()))
        .on_click(cx.listener(move |this, _, _, cx| this.set_theme_filter(filter, cx)))
}

/// One swatch card. A 6-cell colour strip at the top renders background /
/// foreground / border / accent / bullish / bearish so the palette identity
/// is visible at a glance; below sits the name + mode chip. All cards share
/// the same fixed dimensions so the grid reads as a uniform tile field.
fn theme_card(
    p: &themes::ThemePreview,
    active_name: &SharedString,
    primary: gpui::Hsla,
    border_color: gpui::Hsla,
) -> impl IntoElement {
    const CARD_H: f32 = 96.0;
    const SWATCH_H: f32 = 32.0;

    let is_active = p.name == *active_name;
    let outline = if is_active { primary } else { border_color };
    let name_for_action = p.name.clone();

    div()
        .id(gpui::ElementId::Name(p.name.clone()))
        .flex_1()
        .min_w(px(0.))
        .h(px(CARD_H))
        .rounded(px(8.))
        .border_2()
        .border_color(outline)
        .bg(p.background)
        .text_color(p.foreground)
        .overflow_hidden()
        .cursor_pointer()
        .child(
            // 6-cell palette strip — background / foreground / border /
            // accent / bullish / bearish, equal width.
            h_flex()
                .w_full()
                .h(px(SWATCH_H))
                .child(div().flex_1().h_full().bg(p.background))
                .child(div().flex_1().h_full().bg(p.foreground))
                .child(div().flex_1().h_full().bg(p.border))
                .child(div().flex_1().h_full().bg(p.accent))
                .child(div().flex_1().h_full().bg(p.bullish))
                .child(div().flex_1().h_full().bg(p.bearish)),
        )
        .child(
            div()
                .px_3()
                .py_2()
                .text_sm()
                .font_semibold()
                .child(p.name.clone()),
        )
        .on_click(move |_, window, cx| {
            window.dispatch_action(Box::new(SetTheme(name_for_action.clone())), cx);
        })
}

// ---------------------------------------------------------------------------
// Keymap — static reference. Build a small table of (scope, keys, action),
// rendered as a two-column grid. Cmd vs Ctrl is picked per OS so the labels
// match what actually fires.
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
const CMD: &str = "⌘";
#[cfg(not(target_os = "macos"))]
const CMD: &str = "Ctrl";

struct Shortcut {
    scope: &'static str,
    keys: String,
    action: &'static str,
}

fn shortcuts() -> Vec<Shortcut> {
    vec![
        Shortcut {
            scope: "Workspace",
            keys: format!("{CMD} + K"),
            action: "Open symbol picker",
        },
        Shortcut {
            scope: "Workspace",
            keys: format!("{CMD} + I"),
            action: "Open indicator picker",
        },
        Shortcut {
            scope: "Symbol picker",
            keys: "Enter".into(),
            action: "Confirm selection",
        },
        Shortcut {
            scope: "Symbol picker",
            keys: "Esc".into(),
            action: "Close picker",
        },
        Shortcut {
            scope: "Indicator picker",
            keys: "Enter".into(),
            action: "Add selected indicator",
        },
        Shortcut {
            scope: "Indicator picker",
            keys: "Esc".into(),
            action: "Close picker",
        },
        Shortcut {
            scope: "Chart",
            keys: "Delete / Backspace".into(),
            action: "Delete selected drawing",
        },
    ]
}

fn render_keymap(cx: &mut Context<SettingsView>) -> impl IntoElement {
    let theme = cx.theme();
    let muted = theme.muted_foreground;
    let border = theme.border;

    const KEYS_W: f32 = 200.0;

    // Group shortcuts by scope. Iteration order over the input slice is
    // preserved so the existing ordering (Workspace → Symbol picker → Chart)
    // is the group order on screen.
    let mut groups: Vec<(&'static str, Vec<Shortcut>)> = Vec::new();
    for s in shortcuts() {
        match groups.iter_mut().find(|(scope, _)| *scope == s.scope) {
            Some((_, v)) => v.push(s),
            None => groups.push((s.scope, vec![s])),
        }
    }

    let header_row = || {
        h_flex()
            .py_1p5()
            .px_3()
            .border_b_1()
            .border_color(border)
            .items_center()
            .child(
                div()
                    .w(px(KEYS_W))
                    .flex_none()
                    .text_xs()
                    .text_color(muted)
                    .child(SharedString::from("Shortcut")),
            )
            .child(
                div()
                    .flex_1()
                    .text_xs()
                    .text_color(muted)
                    .child(SharedString::from("Action")),
            )
    };

    let mut sections = v_flex().gap_4();
    for (scope, shortcuts_in_scope) in groups {
        let mut rows = v_flex().child(header_row());
        for s in shortcuts_in_scope {
            rows = rows.child(
                h_flex()
                    .py_1p5()
                    .px_3()
                    .border_b_1()
                    .border_color(border)
                    .items_center()
                    .child(
                        div()
                            .w(px(KEYS_W))
                            .flex_none()
                            .text_sm()
                            .font_semibold()
                            .child(SharedString::from(s.keys)),
                    )
                    .child(div().flex_1().text_sm().child(SharedString::from(s.action))),
            );
        }
        sections = sections.child(
            v_flex()
                .gap_2()
                .child(
                    div()
                        .px_4()
                        .text_xs()
                        .text_color(muted)
                        .child(SharedString::from(scope)),
                )
                .child(div().px_4().child(rows)),
        );
    }

    v_flex()
        .gap_3()
        .child(setting_row(
            "Keyboard shortcuts",
            "Reference only. Customisation is on the roadmap.",
            div(),
        ))
        .child(sections)
}

// ---------------------------------------------------------------------------
// Actions — dispatched by the Timezone dropdown. Workspace handles them.
// ---------------------------------------------------------------------------

#[derive(gpui::Action, Clone, PartialEq, Eq, serde::Deserialize)]
#[action(namespace = terminal_demo, no_json)]
pub struct SetTimezone(pub Option<SharedString>);

/// Apply a timezone choice. `None` ≡ Auto; `Some("UTC"|...)` parses via
/// `chrono_tz`. Called from the workspace's action handler.
pub fn apply_timezone(cx: &mut gpui::App, iana: Option<SharedString>) {
    let parsed: Option<Tz> = iana.as_deref().and_then(|s| s.parse().ok());
    prefs::set_tz(cx, parsed);
}
