//! SEC 8-K filings panel. Watchlist-style table (px 11 header / 13 rows,
//! `flex_1 + min_w_0 + ellipsis` columns, hover row). Row list is rendered
//! through `v_virtual_list` so 500+ filings scroll without an FPS hit —
//! only the visible window is built each frame.

use std::rc::Rc;

use chrono::TimeZone as _;
use gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, MouseButton, ParentElement as _, Pixels, Render,
    SharedString, Size, StatefulInteractiveElement as _, Styled as _, Subscription, Window, div,
    px, size,
};
use gpui_component::{
    ActiveTheme as _, Sizable as _, VirtualListScrollHandle, WindowExt as _,
    button::{Button, ButtonVariants as _},
    dialog::DialogButtonProps,
    dock::{Panel, PanelEvent, TabPanel},
    h_flex,
    input::{Input, InputEvent, InputState},
    separator::Separator,
    v_flex, v_virtual_list,
};

use crate::panels::{Kind, LastFocusedTabPanel};
use crate::prefs;
use crate::services::filings::{
    Filing, FilingsEvent, FilingsServiceHandle, FilingsState,
};
use crate::top_bar::AskAi;

/// Uniform row height for the virtual list. 28px matches the row's
/// `py_1` + `text_size(px(13))` natural height; keeping it fixed lets
/// VirtualList allocate `item_sizes` cheaply.
const ROW_HEIGHT_PX: f32 = 28.0;

/// Pre-formatted strings for a single visible row. We materialize these once
/// per service `Changed` rather than per-render so the hot scroll path inside
/// `v_virtual_list`'s closure only indexes into Vecs and clones cheap
/// `SharedString`s — no `format!()` / `to_string()` / `to_lowercase()` per
/// visible row per frame. `ticker_lower` is the lowercase variant used by the
/// substring filter so we don't allocate a new String per filing per render.
#[derive(Clone)]
struct FilingRow {
    ticker: SharedString,
    ticker_lower: String,
    date_label: SharedString,
    form_type: SharedString,
    title: SharedString,
    url: SharedString,
    row_id: SharedString,
}

pub struct FilingsPanel {
    focus_handle: FocusHandle,
    parent_tab_panel: Option<gpui::WeakEntity<TabPanel>>,
    service: Entity<crate::services::filings::FilingsService>,
    scroll_handle: VirtualListScrollHandle,
    /// Substring filter on the ticker column. Owned on the panel so cursor
    /// and typed text survive panel re-renders — recreating the InputState
    /// every render would reset state on each keystroke.
    ticker_filter: Entity<InputState>,
    /// Cached display rows, rebuilt only when the service emits `Changed`.
    /// Holding it as `Rc` keeps the virtual-list closure cheap to spawn
    /// each render (it clones the Rc rather than re-allocating the Vec)
    /// and lets render skip the previous per-frame `Vec<Filing>` clone /
    /// per-row `format!()` work.
    cached_rows: Rc<Vec<FilingRow>>,
    cached_status: Option<SharedString>,
    _service_subscription: Subscription,
    _ticker_filter_subscription: Subscription,
}

impl FilingsPanel {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let service = cx.global::<FilingsServiceHandle>().0.clone();
        // Service emits `Changed` only on state transitions, so this fires
        // once per fetch lifecycle. Two responsibilities: (a) log the
        // upstream error to the browser console (the UI only shows a
        // generic status label) and (b) rebuild the cached display rows so
        // render no longer pays the Vec<Filing> clone / format!() cost.
        let _service_subscription =
            cx.subscribe(&service, |this, svc, _ev: &FilingsEvent, cx| {
                // Clone the snapshot so we drop the entity borrow before
                // mutating `this` — holding a `&FilingsState` across the
                // self-mutation would conflict with the borrow checker.
                let snapshot = svc.read(cx).state().clone();
                if let FilingsState::Error { message, .. } = &snapshot {
                    log::error!("filings fetch failed: {message}");
                }
                this.apply_state(snapshot, cx);
                cx.notify();
            });
        let ticker_filter = cx.new(|cx| {
            InputState::new(window, cx).placeholder("Filter ticker…")
        });
        let _ticker_filter_subscription =
            cx.subscribe(&ticker_filter, |_this, _input, ev: &InputEvent, cx| {
                if matches!(ev, InputEvent::Change) {
                    cx.notify();
                }
            });
        Self {
            focus_handle: cx.focus_handle(),
            parent_tab_panel: None,
            service,
            scroll_handle: VirtualListScrollHandle::new(),
            ticker_filter,
            cached_rows: Rc::new(Vec::new()),
            cached_status: None,
            _service_subscription,
            _ticker_filter_subscription,
        }
    }

    /// Translate a fresh `FilingsState` snapshot into the cached rows +
    /// status label. Called once per service `Changed` from the
    /// subscription — *not* per render. Pre-formats every SharedString a
    /// row needs (date label, ticker badge, title text, click URL, element
    /// id) so the virtual-list closure stays allocation-free per frame.
    fn apply_state(&mut self, state: FilingsState, cx: &mut Context<Self>) {
        match state {
            FilingsState::Idle | FilingsState::Loading => {
                self.cached_status = Some(SharedString::from("Loading…"));
                // Intentionally keep the previous cached_rows so a quick
                // refresh doesn't blank the table mid-load.
            }
            FilingsState::Loaded { filings, fetched_at } => {
                // Render the fetch timestamp in the user's selected TZ
                // (matches the row date column built below).
                let offset = prefs::offset_for(cx, fetched_at.timestamp_millis());
                let local = offset.from_utc_datetime(&fetched_at.naive_utc());
                self.cached_status = Some(SharedString::from(format!(
                    "Updated {}",
                    local.format("%H:%M")
                )));
                self.cached_rows = Rc::new(Self::build_rows(&filings, cx));
            }
            FilingsState::Error { last, .. } => {
                self.cached_status = Some(SharedString::from(match &last {
                    Some((_, t)) => {
                        let offset = prefs::offset_for(cx, t.timestamp_millis());
                        let local = offset.from_utc_datetime(&t.naive_utc());
                        format!("Stale (last {})", local.format("%H:%M"))
                    }
                    None => "Failed to load".to_string(),
                }));
                if let Some((filings, _)) = last {
                    self.cached_rows = Rc::new(Self::build_rows(&filings, cx));
                }
            }
        }
    }

    fn build_rows(filings: &[Filing], cx: &Context<Self>) -> Vec<FilingRow> {
        filings
            .iter()
            .map(|f| {
                let offset = prefs::offset_for(cx, f.filed_at.timestamp_millis());
                let local = offset.from_utc_datetime(&f.filed_at.naive_utc());
                let date_label =
                    SharedString::from(local.format("%Y-%m-%d").to_string());
                let title = if f.title.is_empty() {
                    SharedString::from(format!("{} filing", f.form_type))
                } else {
                    SharedString::from(f.title.clone())
                };
                // accession_number now arrives on the wire (server change),
                // so it's a stable unique id per filing — no need to hash
                // the headline to disambiguate.
                let row_id = SharedString::from(format!(
                    "filing-{}-{}",
                    f.ticker,
                    if f.accession_number.is_empty() {
                        &f.filing_date
                    } else {
                        f.accession_number.as_str()
                    },
                ));
                FilingRow {
                    ticker_lower: f.ticker.to_lowercase(),
                    ticker: SharedString::from(f.ticker.clone()),
                    date_label,
                    form_type: SharedString::from(f.form_type.clone()),
                    title,
                    url: SharedString::from(f.url.clone()),
                    row_id,
                }
            })
            .collect()
    }

    fn mark_focused(&self, cx: &mut App) {
        let Some(tab_panel) = self.parent_tab_panel.clone() else {
            return;
        };
        let global = cx.global::<LastFocusedTabPanel>().0.clone();
        *global.borrow_mut() = Some(tab_panel);
    }

    fn is_focused(&self, cx: &App) -> bool {
        let Some(mine) = self.parent_tab_panel.as_ref() else {
            return false;
        };
        let global = cx.global::<LastFocusedTabPanel>().0.borrow();
        global
            .as_ref()
            .map(|w| w.entity_id() == mine.entity_id())
            .unwrap_or(false)
    }

    fn trigger_refresh(&mut self, cx: &mut Context<Self>) {
        self.service.update(cx, |svc, cx| svc.reload(cx));
    }
}

impl EventEmitter<PanelEvent> for FilingsPanel {}

impl Focusable for FilingsPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Panel for FilingsPanel {
    fn panel_name(&self) -> &'static str {
        Kind::Filings.id()
    }

    fn title(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        SharedString::from(Kind::Filings.display())
    }

    fn on_added_to(
        &mut self,
        tab_panel: gpui::WeakEntity<TabPanel>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        self.parent_tab_panel = Some(tab_panel);
    }

    fn set_active(&mut self, active: bool, _window: &mut Window, cx: &mut Context<Self>) {
        if active {
            self.mark_focused(cx);
        }
    }
}

impl Render for FilingsPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (muted, border) = {
            let theme = cx.theme();
            (theme.muted_foreground, theme.border)
        };
        let ring_color = if self.is_focused(cx) {
            cx.theme().ring
        } else {
            gpui::transparent_black()
        };

        // Reads pull from the cache populated by the service subscription
        // (see `apply_state`). No Vec<Filing> clone in render, no per-row
        // `format!()` in the hot scroll path.
        let rows_rc = self.cached_rows.clone();
        let status_label = self.cached_status.clone();
        let total_count = rows_rc.len();

        // ─── Filter (ticker substring) ─────────────────────────────────
        let query = self.ticker_filter.read(cx).value().to_string();
        let q = query.trim().to_lowercase();
        let visible_idx: Rc<Vec<usize>> = Rc::new(
            rows_rc
                .iter()
                .enumerate()
                .filter(|(_, r)| q.is_empty() || r.ticker_lower.contains(&q))
                .map(|(i, _)| i)
                .collect(),
        );
        let visible_count = visible_idx.len();
        let filter_active = !q.is_empty();
        let count_label = if filter_active {
            SharedString::from(format!("{} of {} filings", visible_count, total_count))
        } else {
            SharedString::from(format!("{} filings", total_count))
        };

        // ─── Top header (refresh + status + count) ─────────────────────
        let mut top_header = h_flex()
            .px_2()
            .py_1()
            .gap_2()
            .items_center()
            .child(
                Button::new("refresh-filings")
                    .label("↻")
                    .small()
                    .ghost()
                    .tooltip("Refresh filings")
                    .on_click(cx.listener(|this, _, _, cx| this.trigger_refresh(cx))),
            );
        if let Some(label) = status_label {
            top_header = top_header
                .child(div().text_size(px(11.)).text_color(muted).child(label));
        }
        top_header = top_header
            .child(div().flex_1())
            .child(div().text_size(px(11.)).text_color(muted).child(count_label));

        // ─── Filter row (ticker substring) ─────────────────────────────
        let filter_row = h_flex()
            .px_2()
            .py_1()
            .gap_2()
            .items_center()
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(muted)
                    .flex_shrink_0()
                    .child("Ticker:"),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .max_w(px(220.))
                    .child(Input::new(&self.ticker_filter).small()),
            );

        // ─── Column header ─────────────────────────────────────────────
        // Every cell carries `min_w_0 + overflow_hidden + whitespace_nowrap
        // + text_ellipsis` so cells whose intrinsic content is wider than
        // their declared `w(px(N))` don't push the row wider than the rest
        // of the table — flex's implicit `min-content` otherwise lets longer
        // text accumulate horizontal drift across columns.
        let column_header = h_flex()
            .px_2()
            .py_1()
            .gap_2()
            .text_size(px(11.))
            .text_color(muted)
            .border_b_1()
            .border_color(border)
            .child(
                div()
                    .w(px(80.))
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .child("Filed"),
            )
            .child(
                div()
                    .w(px(56.))
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .child("Ticker"),
            )
            .child(
                div()
                    .w(px(48.))
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .child("Form"),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .child("Headline"),
            );

        // ─── Body ──────────────────────────────────────────────────────
        let body_rows: gpui::AnyElement = if visible_count == 0 {
            let msg = if filter_active {
                "No filings match the active filter."
            } else {
                "No recent 8-Ks for tickers in your watchlist. Add tickers via the Watchlist panel."
            };
            div()
                .py_6()
                .px_2()
                .text_size(px(13.))
                .text_color(muted)
                .child(msg)
                .into_any_element()
        } else {
            // VirtualList expects `Rc<Vec<Size<Pixels>>>` — one size per
            // item. Uniform `ROW_HEIGHT_PX` keeps allocation a single
            // value × visible_count.
            let item_sizes: Rc<Vec<Size<Pixels>>> = Rc::new(
                (0..visible_count)
                    .map(|_| size(px(0.), px(ROW_HEIGHT_PX)))
                    .collect(),
            );
            // Capture `Rc` clones of the cache + the visible indices: the
            // closure only indexes into them per visible row, no
            // formatting / allocation in the scroll hot path.
            let rows_for_closure = rows_rc.clone();
            let visible_for_closure = visible_idx.clone();
            v_virtual_list(
                cx.entity().clone(),
                "filings-rows",
                item_sizes,
                move |_this, visible_range, _window, cx| {
                    let theme = cx.theme();
                    let muted = theme.muted_foreground;
                    let border = theme.border;
                    let accent = theme.accent;
                    let accent_fg = theme.accent_foreground;
                    let fg = theme.foreground;
                    let hover_bg = theme.accent;
                    visible_range
                        .map(|vi| {
                            let i = visible_for_closure[vi];
                            let r = &rows_for_closure[i];
                            // Clone the row's display strings into the click
                            // handler so the dialog opener owns them without
                            // borrowing back into the cached Rc.
                            let click_ticker = r.ticker.clone();
                            let click_form = r.form_type.clone();
                            let click_date = r.date_label.clone();
                            let click_title = r.title.clone();
                            let click_url = r.url.clone();
                            h_flex()
                                .id(r.row_id.clone())
                                .w_full()
                                .h(px(ROW_HEIGHT_PX))
                                .px_2()
                                .gap_2()
                                .items_center()
                                .text_size(px(13.))
                                .border_b_1()
                                .border_color(border)
                                .cursor_pointer()
                                .hover(|s| s.bg(hover_bg).opacity(0.95))
                                .on_click(move |_, window, cx| {
                                    open_filing_dialog(
                                        click_ticker.clone(),
                                        click_form.clone(),
                                        click_date.clone(),
                                        click_title.clone(),
                                        click_url.clone(),
                                        window,
                                        cx,
                                    );
                                })
                                .child(
                                    div()
                                        .w(px(80.))
                                        .min_w_0()
                                        .overflow_hidden()
                                        .whitespace_nowrap()
                                        .text_ellipsis()
                                        .text_size(px(11.))
                                        .text_color(muted)
                                        .child(r.date_label.clone()),
                                )
                                .child(
                                    div()
                                        .w(px(56.))
                                        .min_w_0()
                                        .overflow_hidden()
                                        .whitespace_nowrap()
                                        .text_ellipsis()
                                        .child(
                                            div()
                                                .px_1p5()
                                                .py_0p5()
                                                .rounded(px(3.))
                                                .bg(accent)
                                                .text_color(accent_fg)
                                                .text_size(px(10.))
                                                .min_w_0()
                                                .overflow_hidden()
                                                .whitespace_nowrap()
                                                .text_ellipsis()
                                                .child(r.ticker.clone()),
                                        ),
                                )
                                .child(
                                    div()
                                        .w(px(48.))
                                        .min_w_0()
                                        .overflow_hidden()
                                        .whitespace_nowrap()
                                        .text_ellipsis()
                                        .text_size(px(11.))
                                        .text_color(muted)
                                        .child(r.form_type.clone()),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .overflow_hidden()
                                        .whitespace_nowrap()
                                        .text_ellipsis()
                                        .text_color(fg)
                                        .child(r.title.clone()),
                                )
                        })
                        .collect()
                },
            )
            .track_scroll(&self.scroll_handle)
            .into_any_element()
        };

        // Sits between the action header and the ticker substring filter so
        // it's clear the substring filter narrows *within* the watchlist
        // scope, not on top of the full SEC universe.
        let scope_hint = div()
            .px_2()
            .pb_1()
            .text_size(px(11.))
            .text_color(muted)
            .child("Showing 8-K filings for tickers in your watchlist.");

        let body = v_flex()
            .size_full()
            .child(top_header)
            .child(scope_hint)
            .child(filter_row)
            .child(column_header)
            // Virtual list needs a sized parent — flex_1 + min_h_0 inside
            // the panel's size_full wrapper gives it room to actually
            // scroll. Without min_h_0 the flex child measures to its
            // intrinsic content height and overflow never triggers.
            .child(div().flex_1().min_h_0().size_full().child(body_rows));

        div()
            .id("filings-panel-body")
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _ev, _window, cx| this.mark_focused(cx)),
            )
            .size_full()
            .border_2()
            .border_color(ring_color)
            .child(body)
    }
}

/// Open the filing details dialog. Lives as a free function so the row's
/// `on_click` closure can call it with just `(window, cx)` — it doesn't
/// need any panel state since the click handler already captured the row's
/// display strings. The dialog has two affordances: "Open in EDGAR"
/// (forwards to `window.open_with_url_and_target`) and "Ask AI" (dispatches
/// the existing `top_bar::AskAi` action, which routes through the workspace
/// to stage the prompt on the active chat session and focus the chat input).
fn open_filing_dialog(
    ticker: SharedString,
    form_type: SharedString,
    date_label: SharedString,
    title: SharedString,
    url: SharedString,
    window: &mut Window,
    cx: &mut App,
) {
    // Pre-build the Ask-AI prompt so the dialog closure can capture a
    // single SharedString instead of all the individual fields again.
    let ask_prompt = SharedString::from(format!(
        "About {ticker} {form_type} filed {date_label}:\n\
         Headline: {title}\n\
         URL: {url}\n\n\
         Summarize the filing and any implications for the stock.",
    ));
    let dialog_title = SharedString::from(format!("{ticker} · {form_type}"));

    window.open_dialog(cx, move |dialog, _w, cx| {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let fg = theme.foreground;

        // Compact key/value row used for the metadata block at the top of
        // the dialog. Label column is fixed-width so values align.
        let kv_row = |label: &'static str, value: SharedString| {
            h_flex()
                .gap_2()
                .items_baseline()
                .child(
                    div()
                        .w(px(64.))
                        .text_size(px(11.))
                        .text_color(muted)
                        .child(label),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_size(px(13.))
                        .text_color(fg)
                        .child(value),
                )
        };

        let body = v_flex()
            .px_4()
            .pb_2()
            .pt_2()
            .gap_3()
            .child(kv_row("Ticker", ticker.clone()))
            .child(kv_row("Form", form_type.clone()))
            .child(kv_row("Filed", date_label.clone()))
            .child(Separator::horizontal())
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(muted)
                            .child("Headline"),
                    )
                    .child(
                        div()
                            .text_size(px(13.))
                            .text_color(fg)
                            .child(title.clone()),
                    ),
            )
            .child(Separator::horizontal())
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        Button::new("filing-open-edgar")
                            .label("Open in EDGAR")
                            .small()
                            .outline()
                            .on_click({
                                let url = url.clone();
                                move |_, _, _| {
                                    if let Some(w) = web_sys::window() {
                                        let _ = w.open_with_url_and_target(
                                            url.as_ref(),
                                            "_blank",
                                        );
                                    }
                                }
                            }),
                    )
                    .child(
                        Button::new("filing-ask-ai")
                            .label("Ask AI")
                            .small()
                            .primary()
                            .on_click({
                                let prompt = ask_prompt.clone();
                                move |_, window, cx| {
                                    window.dispatch_action(
                                        Box::new(AskAi(prompt.clone())),
                                        cx,
                                    );
                                    window.close_all_dialogs(cx);
                                }
                            }),
                    ),
            );

        dialog
            .title(dialog_title.clone())
            .max_w(px(560.))
            .button_props(DialogButtonProps::default().ok_text("Close"))
            .child(body)
    });
}
