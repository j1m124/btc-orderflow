//! TradingView-style indicator picker modal. Mirrors `SymbolPickerState`:
//! one workspace-owned entity, opened via the `OpenIndicatorPicker` action
//! (toolbar "+ Indicator" button + Cmd-I shortcut), confirms a kind into
//! the resolved target chart panel.
//!
//! Layout: centered 560×520 modal with a search bar and a scrollable list.
//! When the query is empty the list groups entries under category headers
//! (Overlays / Volume / Oscillators). When the user types, results flow
//! flat under the search bar, ranked by simple fuzzy score.

use gpui::{
    Action, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, KeyDownEvent, MouseButton, ParentElement as _, Render,
    SharedString, StatefulInteractiveElement as _, Styled as _, Subscription, WeakEntity, Window,
    div, px,
};
use gpui_component::{
    ActiveTheme as _, Sizable as _, StyledExt as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputEvent, InputState},
    v_flex,
};
use serde::Deserialize;

use crate::indicators::{Category, KindEntry, kind_entries};
use crate::panels::ContentPanel;

/// Open the indicator picker. No payload: the workspace resolves the
/// target chart from `LastFocusedChart` (mirroring `OpenSymbolPicker`'s
/// pattern). Toggling re-fires while open → close.
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = btc_orderflow, no_json)]
pub struct OpenIndicatorPicker;

/// Confirm the top-ranked row. Bound on the picker; falls through to a
/// key-down handler in browsers where the binding doesn't fire because
/// focus is parked on the search input. No keyboard row navigation —
/// mouse hover is the only way to target a non-top result.
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = btc_orderflow, no_json)]
pub struct ConfirmIndicatorPick;

/// Close without confirming (Esc).
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = btc_orderflow, no_json)]
pub struct CloseIndicatorPicker;

/// Where to add the picked indicator. The workspace resolves the chart
/// panel and stuffs it in via `open(...)` before showing the modal.
#[derive(Clone)]
pub struct IndicatorPickerIntent {
    pub target: WeakEntity<ContentPanel>,
}

pub struct IndicatorPickerState {
    is_open: bool,
    intent: Option<IndicatorPickerIntent>,
    query_input: Entity<InputState>,
    focus: FocusHandle,
    _query_sub: Option<Subscription>,
}

#[derive(Clone, Debug)]
pub enum IndicatorPickerEvent {
    Closed,
}

impl EventEmitter<IndicatorPickerEvent> for IndicatorPickerState {}

impl IndicatorPickerState {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let query_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder("Search indicators\u{2026}")
        });
        let query_sub = cx.subscribe(&query_input, |_this, _input, ev: &InputEvent, cx| {
            if matches!(ev, InputEvent::Change) {
                // Re-render so the ranked list reflects the new query;
                // Enter still confirms the top hit (index 0).
                cx.notify();
            }
        });
        Self {
            is_open: false,
            intent: None,
            query_input,
            focus: cx.focus_handle(),
            _query_sub: Some(query_sub),
        }
    }

    pub fn is_open(&self) -> bool {
        self.is_open
    }

    /// Open with a resolved target. Clears the query so every open starts
    /// clean. Focuses the search input.
    pub fn open(
        &mut self,
        intent: IndicatorPickerIntent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.intent = Some(intent);
        self.is_open = true;
        self.query_input.update(cx, |input, cx| {
            input.set_value(SharedString::from(""), window, cx);
        });
        let handle = self.query_input.read(cx).focus_handle(cx);
        handle.focus(window, cx);
        cx.notify();
    }

    pub fn close(&mut self, cx: &mut Context<Self>) {
        if !self.is_open {
            return;
        }
        self.is_open = false;
        self.intent = None;
        cx.emit(IndicatorPickerEvent::Closed);
        cx.notify();
    }

    fn query(&self, cx: &Context<Self>) -> String {
        self.query_input.read(cx).value().to_string()
    }

    /// Build the rows visible at the current query. Empty query → grouped
    /// by category; non-empty → a single flat ranked list. Both paths feed
    /// `flatten()` for the top-hit Enter behavior and `render()`.
    fn visible(&self, cx: &Context<Self>) -> VisibleList {
        let query = self.query(cx);
        let trimmed = query.trim();
        let all = kind_entries();
        if trimmed.is_empty() {
            // Empty query: section by category in display order:
            // Overlays first, then Volume, then Oscillators.
            let mut groups: Vec<(Category, Vec<KindEntry>)> = Vec::new();
            for cat in [Category::Overlay, Category::Volume, Category::Oscillator] {
                let entries: Vec<KindEntry> = all
                    .iter()
                    .filter(|e| e.category == cat)
                    .map(|e| KindEntry {
                        kind_id: e.kind_id,
                        name: e.name.clone(),
                        description: e.description.clone(),
                        category: e.category,
                        spawn: e.spawn,
                    })
                    .collect();
                if !entries.is_empty() {
                    groups.push((cat, entries));
                }
            }
            VisibleList { groups }
        } else {
            // Filtering query: flat ranked list under a single virtual group
            // so render's group loop stays uniform.
            let mut ranked: Vec<(usize, KindEntry)> = all
                .into_iter()
                .filter_map(|e| score(&e, trimmed).map(|s| (s, e)))
                .collect();
            ranked.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.name.cmp(&b.1.name)));
            let groups = if ranked.is_empty() {
                Vec::new()
            } else {
                vec![(
                    Category::Overlay,
                    ranked.into_iter().map(|(_, e)| e).collect(),
                )]
            };
            VisibleList { groups }
        }
    }

    fn confirm_at(&mut self, idx: i32, window: &mut Window, cx: &mut Context<Self>) {
        let list = self.visible(cx);
        let flat = list.flatten();
        if flat.is_empty() {
            return;
        }
        let i = idx.max(0) as usize;
        let i = i.min(flat.len() - 1);
        let chosen_spawn = flat[i].spawn;
        let target = match self.intent.as_ref() {
            Some(intent) => intent.target.clone(),
            None => {
                self.close(cx);
                return;
            }
        };
        if let Some(panel) = target.upgrade() {
            panel.update(cx, |p, cx| {
                p.add_indicator_from_picker(chosen_spawn(), cx);
            });
        }
        self.close(cx);
        let _ = window;
    }

    fn on_confirm(
        &mut self,
        _: &ConfirmIndicatorPick,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Always confirms the top-ranked row. The hover-driven picker has
        // no notion of a moving keyboard cursor — click a row to pick a
        // non-top result.
        self.confirm_at(0, window, cx);
    }

    fn on_close(&mut self, _: &CloseIndicatorPicker, _window: &mut Window, cx: &mut Context<Self>) {
        self.close(cx);
    }
}

impl Focusable for IndicatorPickerState {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for IndicatorPickerState {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.is_open {
            return div().into_any_element();
        }
        let theme_bg = cx.theme().background;
        let theme_border = cx.theme().border;
        let theme_muted = cx.theme().muted_foreground;
        let theme_ring = cx.theme().ring;
        let theme_accent = cx.theme().accent;
        let theme_accent_fg = cx.theme().accent_foreground;
        let trimmed_query_empty = self.query(cx).trim().is_empty();
        let visible = self.visible(cx);
        let total = visible.flatten().len();

        let list_element: gpui::AnyElement = if total == 0 {
            let msg = if trimmed_query_empty {
                "No indicators available"
            } else {
                "No indicators match"
            };
            div()
                .flex_1()
                .min_h_0()
                .flex()
                .items_center()
                .justify_center()
                .text_sm()
                .text_color(theme_muted)
                .child(msg)
                .into_any_element()
        } else {
            let mut scroller = v_flex()
                .id("indicator-picker-list")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll();
            let mut flat_idx = 0usize;
            for (cat, entries) in &visible.groups {
                // Hide the category header when we're showing a filtered
                // result set under the search bar (single virtual group).
                if trimmed_query_empty {
                    scroller = scroller.child(section_header(cat.label(), theme_muted));
                }
                for entry in entries {
                    scroller = scroller.child(render_row(
                        entry,
                        flat_idx,
                        theme_accent,
                        theme_accent_fg,
                        theme_muted,
                        cx,
                    ));
                    flat_idx += 1;
                }
            }
            scroller.into_any_element()
        };

        let list_region = v_flex()
            .flex_1()
            .min_h_0()
            .px_2()
            .py_1()
            .child(list_element);

        let card = v_flex()
            .id("indicator-picker-card")
            .w(px(560.))
            .h(px(520.))
            .bg(theme_bg)
            .border_1()
            .border_color(theme_border)
            .rounded(px(8.))
            .shadow_lg()
            .overflow_hidden()
            .on_mouse_down(MouseButton::Left, |_ev, _w, cx| cx.stop_propagation())
            .child(
                h_flex()
                    .px_4()
                    .py_3()
                    .items_center()
                    .justify_between()
                    .border_b_1()
                    .border_color(theme_border)
                    .child(div().font_semibold().child(SharedString::from("Add Indicator")))
                    .child(
                        Button::new("indicator-picker-close")
                            .label("\u{00d7}")
                            .small()
                            .ghost()
                            .on_click(cx.listener(|this, _ev, _w, cx| this.close(cx))),
                    ),
            )
            .child(
                div()
                    .px_4()
                    .pt_3()
                    .pb_2()
                    .child(Input::new(&self.query_input).large()),
            )
            .child(div().h(px(1.0)).w_full().bg(theme_border))
            .child(list_region);

        div()
            .id("indicator-picker-backdrop")
            .key_context("IndicatorPicker")
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::on_confirm))
            .on_action(cx.listener(Self::on_close))
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                match ev.keystroke.key.as_str() {
                    "escape" => this.close(cx),
                    "enter" => this.confirm_at(0, window, cx),
                    _ => return,
                }
                cx.stop_propagation();
            }))
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(gpui::hsla(0.0, 0.0, 0.0, 0.5))
            .occlude()
            .on_click(cx.listener(|this, _ev, _w, cx| this.close(cx)))
            .child(
                div()
                    .border_1()
                    .border_color(theme_ring)
                    .rounded(px(8.))
                    .child(card),
            )
            .into_any_element()
    }
}

struct VisibleList {
    groups: Vec<(Category, Vec<KindEntry>)>,
}

impl VisibleList {
    fn flatten(&self) -> Vec<&KindEntry> {
        self.groups.iter().flat_map(|(_, e)| e.iter()).collect()
    }
}

fn section_header(label: &'static str, muted: gpui::Hsla) -> impl IntoElement {
    div()
        .px_2()
        .pt_2()
        .pb_1()
        .text_xs()
        .text_color(muted)
        .child(label)
}

fn render_row(
    entry: &KindEntry,
    idx: usize,
    accent: gpui::Hsla,
    accent_fg: gpui::Hsla,
    muted: gpui::Hsla,
    cx: &mut Context<IndicatorPickerState>,
) -> impl IntoElement {
    // Pure hover highlight — mirrors symbol_picker::render_row. No
    // keyboard cursor: arrow keys are unbound, Enter always confirms
    // the top-ranked row (`confirm_at(0)`).
    let name = entry.name.clone();
    let desc = entry.description.clone();
    let row_id = SharedString::from(format!("indicator-row-{}", entry.kind_id));
    h_flex()
        .id(row_id)
        .w_full()
        .px_2()
        .py_2()
        .gap_3()
        .items_center()
        .rounded(px(4.))
        .hover(move |s| s.bg(accent).text_color(accent_fg))
        .cursor_pointer()
        .child(div().w(px(110.)).font_semibold().child(name))
        .child(div().flex_1().text_sm().text_color(muted).child(desc))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _ev, w, cx| {
                this.confirm_at(idx as i32, w, cx);
            }),
        )
}

/// Register keyboard shortcuts for the picker. Cmd-I / Ctrl-I to toggle;
/// Esc / Enter scoped to the `IndicatorPicker` context set on the modal's
/// outer div. No arrow-key row navigation — hover the row you want and
/// click, or hit Enter for the top match. The `cmd-i` and `ctrl-i`
/// modifiers are bound separately rather than via `secondary` because
/// gpui's `secondary` resolves at compile time using
/// `cfg!(target_os = "macos")` — for wasm builds that's always false.
pub fn init(cx: &mut gpui::App) {
    cx.bind_keys([
        gpui::KeyBinding::new("cmd-i", OpenIndicatorPicker, None),
        gpui::KeyBinding::new("ctrl-i", OpenIndicatorPicker, None),
        gpui::KeyBinding::new("escape", CloseIndicatorPicker, Some("IndicatorPicker")),
        gpui::KeyBinding::new("enter", ConfirmIndicatorPick, Some("IndicatorPicker")),
    ]);
}

/// Crude fuzzy score: lower is better. Substring match in name (tier 0),
/// prefix match in description (tier 1), substring in description (tier 2).
fn score(entry: &KindEntry, query: &str) -> Option<usize> {
    let q = query.to_lowercase();
    let name = entry.name.to_lowercase();
    let desc = entry.description.to_lowercase();
    if name.starts_with(&q) {
        return Some(0);
    }
    if name.contains(&q) {
        return Some(1);
    }
    if desc.starts_with(&q) {
        return Some(2);
    }
    if desc.contains(&q) {
        return Some(3);
    }
    None
}
