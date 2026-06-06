//! Shared symbol picker. One [`SymbolPickerState`] entity is owned by the
//! workspace; chart panels and the watchlist trigger it via the workspace's
//! [`OpenSymbolPicker`] action.
//!
//! Layout: centered 560×560 modal with a search bar, an instrument-type tab
//! row (`All` + the [`InstrumentType`] variants), and a scrollable list. When
//! the query is empty the list shows a "Recent" section above the universe.

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

use crate::panels::ContentPanel;
use crate::services::recents::{RecentsEvent, RecentsServiceHandle};
use crate::services::symbols::{InstrumentType, SymbolsEvent, SymbolsServiceHandle};
use crate::top_bar::AddWatchlistSymbol;

/// One symbol shown in the picker list.
#[derive(Clone, Debug)]
pub struct SymbolItem {
    pub ticker: SharedString,
    pub name: SharedString,
    pub exchange: SharedString,
    pub instrument: InstrumentType,
}

/// What the picker should do on confirm.
#[derive(Clone)]
pub enum PickerIntent {
    /// Switch a specific chart panel to the picked ticker. Confirming closes
    /// the picker.
    SwitchChart { target: WeakEntity<ContentPanel> },
    /// Add the picked ticker to the user's watchlist. Confirming keeps the
    /// picker open so the user can add several in a row (matching the prior
    /// `open_add_dialog` UX).
    AddToWatchlist,
}

impl PickerIntent {
    fn title(&self) -> &'static str {
        match self {
            PickerIntent::SwitchChart { .. } => "Symbol Search",
            PickerIntent::AddToWatchlist => "Add to Watchlist",
        }
    }
}

/// Selected filter tab. `All` short-circuits filtering; the [`InstrumentType`]
/// variants narrow to symbols whose `instrument` matches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FilterTab {
    All,
    Class(InstrumentType),
}

impl FilterTab {
    fn matches(self, instrument: InstrumentType) -> bool {
        match self {
            FilterTab::All => true,
            FilterTab::Class(c) => c == instrument,
        }
    }
}

/// Open the shared symbol picker. Carries the intent inline:
/// `target` is the chart panel id when switching, empty when adding to the
/// watchlist. The workspace resolves the target from
/// [`crate::panels::LastFocusedChart`] / a dock walk when this is empty for
/// the chart-switch case (Cmd-K with no prior chart click).
///
/// `kind` is `"chart"` or `"watchlist"`.
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = client, no_json)]
pub struct OpenSymbolPicker {
    pub kind: SharedString,
}

/// Dispatched from inside the picker when the user confirms a row.
/// Workspace-scoped, no payload — the highlighted ticker lives on the picker
/// state.
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = client, no_json)]
pub struct ConfirmPickerSelection;

/// Dispatched on Esc inside the picker. Closes without confirming.
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = client, no_json)]
pub struct ClosePicker;

/// Dispatched on ArrowDown/ArrowUp; `delta` is +1 or -1.
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = client, no_json)]
pub struct MovePickerHighlight(pub i32);

pub struct SymbolPickerState {
    is_open: bool,
    intent: Option<PickerIntent>,
    query_input: Entity<InputState>,
    active_tab: FilterTab,
    /// Index into the currently-rendered visible list. -1 means "no row
    /// highlighted yet". Reset to 0 on every query change so Enter picks the
    /// top hit.
    highlight: i32,
    focus: FocusHandle,
    _query_sub: Option<Subscription>,
    _symbols_sub: Option<Subscription>,
    _recents_sub: Option<Subscription>,
}

/// Events emitted by [`SymbolPickerState`]. The workspace subscribes to
/// [`PickerEvent::Closed`] so it can reclaim keyboard/click focus after the
/// modal disappears — otherwise focus stays on the (now-unrendered) search
/// input and the next click on the dock can be eaten.
#[derive(Clone, Debug)]
pub enum PickerEvent {
    Closed,
}

impl EventEmitter<PickerEvent> for SymbolPickerState {}

impl SymbolPickerState {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let query_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder("Search symbol\u{2026}")
        });
        let query_sub = cx.subscribe(&query_input, |this, _input, ev: &InputEvent, cx| {
            if matches!(ev, InputEvent::Change) {
                // Any text change resets the highlight to the top hit so Enter
                // picks the most-relevant match without a manual arrow key.
                this.highlight = 0;
                cx.notify();
            }
        });
        let symbols_handle = cx.global::<SymbolsServiceHandle>().0.clone();
        let symbols_sub = cx.subscribe(&symbols_handle, |_this, _svc, _ev: &SymbolsEvent, cx| {
            cx.notify();
        });
        let recents_handle = cx.global::<RecentsServiceHandle>().0.clone();
        let recents_sub = cx.subscribe(&recents_handle, |_this, _svc, _ev: &RecentsEvent, cx| {
            cx.notify();
        });
        Self {
            is_open: false,
            intent: None,
            query_input,
            active_tab: FilterTab::All,
            highlight: 0,
            focus: cx.focus_handle(),
            _query_sub: Some(query_sub),
            _symbols_sub: Some(symbols_sub),
            _recents_sub: Some(recents_sub),
        }
    }

    pub fn is_open(&self) -> bool {
        self.is_open
    }

    /// Open the picker with the given intent. Resets query, tab, and highlight
    /// so each open starts clean. Focuses the search input.
    pub fn open(&mut self, intent: PickerIntent, window: &mut Window, cx: &mut Context<Self>) {
        self.intent = Some(intent);
        self.is_open = true;
        self.active_tab = FilterTab::All;
        self.highlight = 0;
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
        cx.emit(PickerEvent::Closed);
        cx.notify();
    }

    fn query(&self, cx: &Context<Self>) -> String {
        self.query_input.read(cx).value().to_string()
    }

    fn visible_items(&self, cx: &Context<Self>) -> VisibleList {
        let universe = collect_universe(cx);
        let query = self.query(cx);
        let trimmed = query.trim();
        if trimmed.is_empty() {
            // Empty query: show recents first, then the full universe
            // (alphabetical by ticker) underneath. Universe entries already
            // in recents are filtered out to avoid duplicate rows.
            let recents = collect_recents(cx, &universe, self.active_tab);
            let recent_set: std::collections::HashSet<SharedString> =
                recents.iter().map(|s| s.ticker.clone()).collect();
            let mut rest: Vec<SymbolItem> = universe
                .into_iter()
                .filter(|s| self.active_tab.matches(s.instrument))
                .filter(|s| !recent_set.contains(&s.ticker))
                .collect();
            rest.sort_by(|a, b| a.ticker.cmp(&b.ticker));
            VisibleList { recents, rest }
        } else {
            let mut rest: Vec<(usize, SymbolItem)> = universe
                .into_iter()
                .filter(|s| self.active_tab.matches(s.instrument))
                .filter_map(|s| score(&s, trimmed).map(|n| (n, s)))
                .collect();
            // Tier ASC (best = lowest tier), tie-break alphabetically.
            rest.sort_by(|a, b| {
                a.0.cmp(&b.0).then_with(|| a.1.ticker.cmp(&b.1.ticker))
            });
            VisibleList {
                recents: Vec::new(),
                rest: rest.into_iter().map(|(_, s)| s).collect(),
            }
        }
    }

    fn confirm_at(&mut self, idx: i32, window: &mut Window, cx: &mut Context<Self>) {
        let list = self.visible_items(cx);
        let flat = list.flatten();
        if flat.is_empty() {
            return;
        }
        let i = idx.max(0) as usize;
        let i = i.min(flat.len() - 1);
        let chosen = flat[i].clone();
        self.apply(chosen, window, cx);
    }

    fn apply(&mut self, item: SymbolItem, window: &mut Window, cx: &mut Context<Self>) {
        // Push to recents first so the watchlist case (which keeps the picker
        // open) still records the pick.
        let recents = cx.global::<RecentsServiceHandle>().0.clone();
        recents.update(cx, |svc, cx| svc.push(item.ticker.clone(), cx));

        let intent = self.intent.clone();
        match intent {
            Some(PickerIntent::SwitchChart { target }) => {
                if let Some(panel) = target.upgrade() {
                    panel.update(cx, |panel, cx| {
                        panel.switch_chart_symbol(item.ticker.as_ref(), cx);
                    });
                }
                self.close(cx);
            }
            Some(PickerIntent::AddToWatchlist) => {
                window.dispatch_action(Box::new(AddWatchlistSymbol(item.ticker.clone())), cx);
                // Reset query so the user can immediately type the next
                // ticker, mirroring the old dialog's quick-add flow.
                self.query_input.update(cx, |input, cx| {
                    input.set_value(SharedString::from(""), window, cx);
                });
                self.highlight = 0;
                cx.notify();
            }
            None => self.close(cx),
        }
    }

    fn on_confirm(&mut self, _: &ConfirmPickerSelection, window: &mut Window, cx: &mut Context<Self>) {
        let idx = self.highlight;
        self.confirm_at(idx, window, cx);
    }

    fn on_close(&mut self, _: &ClosePicker, _window: &mut Window, cx: &mut Context<Self>) {
        self.close(cx);
    }

    fn on_move(&mut self, action: &MovePickerHighlight, _window: &mut Window, cx: &mut Context<Self>) {
        let list = self.visible_items(cx);
        let len = list.flatten().len() as i32;
        if len == 0 {
            return;
        }
        let next = (self.highlight + action.0).rem_euclid(len);
        self.highlight = next;
        cx.notify();
    }

}

impl Focusable for SymbolPickerState {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for SymbolPickerState {
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

        let title = self
            .intent
            .as_ref()
            .map(|i| i.title())
            .unwrap_or("Symbol Search");
        let visible = self.visible_items(cx);

        // ----- List -----
        //
        // Two-section layout when the query is empty: "Recent" then "All
        // symbols", with the rest of the universe under it. On a non-empty
        // query there's a single ranked match list under no header. Rows
        // are indexed in flatten order (recents then rest) to match
        // `MovePickerHighlight` / `confirm_at`.
        let trimmed_query_empty = self.query(cx).trim().is_empty();
        let total_visible = visible.recents.len() + visible.rest.len();

        let list_element: gpui::AnyElement = if total_visible == 0 {
            let msg = if trimmed_query_empty {
                "No symbols available"
            } else {
                "No symbols match"
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
                .id("picker-list")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll();
            let mut flat_idx = 0usize;
            if trimmed_query_empty && !visible.recents.is_empty() {
                scroller = scroller.child(recents_header(theme_muted, cx));
                for item in &visible.recents {
                    scroller = scroller.child(render_row(
                        item,
                        flat_idx,
                        theme_accent,
                        theme_accent_fg,
                        theme_muted,
                        cx,
                    ));
                    flat_idx += 1;
                }
            }
            if !visible.rest.is_empty() {
                // Only show the "All symbols" header on the empty-query
                // two-section view; for a query, the ranked list flows
                // headerless under the search bar.
                if trimmed_query_empty {
                    scroller = scroller.child(section_header("All symbols", theme_muted));
                }
                for item in &visible.rest {
                    scroller = scroller.child(render_row(
                        item,
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

        // The list region — directly inside the card's v_flex so `flex_1` /
        // `min_h_0` on the scroll container actually constrain its height.
        // Wrapping it in plain `div`s would break the flex chain and the
        // overflow_y_scroll would have nothing to scroll against.
        let list_region = v_flex()
            .flex_1()
            .min_h_0()
            .px_2()
            .py_1()
            .child(list_element);

        // ----- Card -----
        let card = v_flex()
            .id("picker-card")
            .w(px(760.))
            .h(px(560.))
            .bg(theme_bg)
            .border_1()
            .border_color(theme_border)
            .rounded(px(8.))
            .shadow_lg()
            .overflow_hidden()
            // Stop clicks inside the card from closing via the backdrop.
            .on_mouse_down(MouseButton::Left, |_ev, _w, cx| cx.stop_propagation())
            .child(
                // Header bar
                h_flex()
                    .px_4()
                    .py_3()
                    .items_center()
                    .justify_between()
                    .border_b_1()
                    .border_color(theme_border)
                    .child(div().font_semibold().child(SharedString::from(title)))
                    .child(
                        Button::new("picker-close")
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

        // ----- Backdrop + centering wrapper -----
        //
        // `on_click` (full press-and-release cycle) instead of `on_mouse_down`
        // for dismissal — closing the modal on mouse-down would tear the
        // backdrop down before the matching mouse-up arrived, leaving the next
        // click cycle in an inconsistent state.
        div()
            .id("symbol-picker-backdrop")
            .key_context("SymbolPicker")
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::on_confirm))
            .on_action(cx.listener(Self::on_close))
            .on_action(cx.listener(Self::on_move))
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                // Fallback for environments where the key bindings haven't
                // fired (e.g. focus stuck on the InputState — the Input
                // context's bindings would otherwise swallow the keys).
                match ev.keystroke.key.as_str() {
                    "escape" => this.close(cx),
                    "enter" => this.confirm_at(this.highlight, window, cx),
                    "down" => this.on_move(&MovePickerHighlight(1), window, cx),
                    "up" => this.on_move(&MovePickerHighlight(-1), window, cx),
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
            // `.occlude()` so clicks on the backdrop don't bleed through to
            // panels underneath while the modal is up.
            .occlude()
            .on_click(cx.listener(|this, _ev, _w, cx| this.close(cx)))
            // Faint accent ring around the modal so it pops on dark themes.
            .child(div().border_1().border_color(theme_ring).rounded(px(8.)).child(card))
            .into_any_element()
    }
}

struct VisibleList {
    recents: Vec<SymbolItem>,
    rest: Vec<SymbolItem>,
}

impl VisibleList {
    fn flatten(&self) -> Vec<SymbolItem> {
        let mut out = Vec::with_capacity(self.recents.len() + self.rest.len());
        out.extend(self.recents.iter().cloned());
        out.extend(self.rest.iter().cloned());
        out
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

/// "Recent" header with a small Clear button on the right. Click dispatches
/// `RecentsService::clear`, which empties the list and emits `Changed`; the
/// picker's recents subscription re-renders, dropping the section to the
/// empty-state branch automatically.
fn recents_header(muted: gpui::Hsla, cx: &mut Context<SymbolPickerState>) -> impl IntoElement {
    h_flex()
        .px_2()
        .pt_2()
        .pb_1()
        .items_center()
        .justify_between()
        .child(div().text_xs().text_color(muted).child("Recent"))
        .child(
            Button::new("picker-recents-clear")
                .label("Clear")
                .xsmall()
                .ghost()
                .on_click(cx.listener(|_this, _ev, _w, cx| {
                    let svc = cx.global::<RecentsServiceHandle>().0.clone();
                    svc.update(cx, |s, cx| s.clear(cx));
                })),
        )
}

fn render_row(
    item: &SymbolItem,
    idx: usize,
    accent: gpui::Hsla,
    accent_fg: gpui::Hsla,
    muted: gpui::Hsla,
    cx: &mut Context<SymbolPickerState>,
) -> impl IntoElement {
    let exchange_chip = if item.exchange.is_empty() {
        None
    } else {
        Some(
            div()
                .px_1p5()
                .text_xs()
                .text_color(muted)
                .child(item.exchange.clone()),
        )
    };
    h_flex()
        .id(SharedString::from(format!("picker-row-{idx}-{}", item.ticker)))
        .w_full()
        .px_2()
        .py_1p5()
        .gap_3()
        .items_center()
        .rounded(px(4.))
        .hover(|s| s.bg(accent).text_color(accent_fg))
        .cursor_pointer()
        .child(div().w(px(80.)).font_semibold().child(item.ticker.clone()))
        .child(div().flex_1().min_w_0().truncate().child(item.name.clone()))
        .children(exchange_chip)
        .on_click(cx.listener(move |this, _ev, window, cx| {
            this.confirm_at(idx as i32, window, cx);
        }))
}

/// Build the full universe from the symbols service.
pub fn collect_universe(cx: &gpui::App) -> Vec<SymbolItem> {
    let handle = cx.global::<SymbolsServiceHandle>().0.clone();
    let svc = handle.read(cx);
    svc.symbols()
        .iter()
        .map(|s| SymbolItem {
            ticker: s.ticker.clone(),
            name: s.name.clone(),
            exchange: s.exchange.clone(),
            instrument: s.instrument,
        })
        .collect()
}

/// Resolve the recents list to `SymbolItem`s, filtered by the active tab.
/// Tickers no longer in the universe are dropped silently (the universe could
/// shrink between the recent's write and the next render).
fn collect_recents(
    cx: &gpui::App,
    universe: &[SymbolItem],
    active_tab: FilterTab,
) -> Vec<SymbolItem> {
    let handle = cx.global::<RecentsServiceHandle>().0.clone();
    let svc = handle.read(cx);
    let mut out: Vec<SymbolItem> = Vec::new();
    for ticker in svc.tickers().iter().take(crate::services::recents::RecentsService::DISPLAY_LIMIT) {
        if let Some(item) = universe.iter().find(|s| &s.ticker == ticker) {
            if active_tab.matches(item.instrument) {
                out.push(item.clone());
            }
        }
    }
    out
}

/// Tiered substring match score, lower = better. `None` = no match.
/// 0: exact ticker  •  1: ticker prefix  •  2: ticker substring  •
/// 3: name prefix   •  4: name substring
fn score(item: &SymbolItem, query: &str) -> Option<usize> {
    let q = query.to_lowercase();
    let t = item.ticker.to_lowercase();
    let n = item.name.to_lowercase();
    if t == q {
        return Some(0);
    }
    if t.starts_with(&q) {
        return Some(1);
    }
    if t.contains(&q) {
        return Some(2);
    }
    if n.starts_with(&q) {
        return Some(3);
    }
    if n.contains(&q) {
        return Some(4);
    }
    None
}

/// Register the picker's key bindings:
///   • `Cmd-K` / `Ctrl-K` (no context) — toggle the picker targeting the
///     focused chart. Bound explicitly per modifier instead of via
///     `secondary-k` because gpui's `secondary` resolves at compile time
///     using `cfg!(target_os = "macos")`; for wasm builds that's always
///     `false`, so on a Mac browser `secondary` silently means Ctrl.
///   • `Escape`, `Enter`, `Down`, `Up` — scoped to the `SymbolPicker`
///     context set on the modal's outer div.
pub fn init(cx: &mut gpui::App) {
    let open_chart = || OpenSymbolPicker {
        kind: SharedString::from("chart"),
    };
    cx.bind_keys([
        gpui::KeyBinding::new("cmd-k", open_chart(), None),
        gpui::KeyBinding::new("ctrl-k", open_chart(), None),
        gpui::KeyBinding::new("escape", ClosePicker, Some("SymbolPicker")),
        gpui::KeyBinding::new("enter", ConfirmPickerSelection, Some("SymbolPicker")),
        gpui::KeyBinding::new("down", MovePickerHighlight(1), Some("SymbolPicker")),
        gpui::KeyBinding::new("up", MovePickerHighlight(-1), Some("SymbolPicker")),
    ]);
}
