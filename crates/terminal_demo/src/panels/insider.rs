//! Form 4 insider transactions panel. Watchlist-scoped, dense 6-column
//! table rendered through `v_virtual_list` so hundreds of trades scroll
//! without an FPS hit. Mirrors `panels/filings.rs` structure; the
//! main differences are the columns (code/shares/value instead of headline)
//! and the colored P/S badge.

use std::rc::Rc;

use chrono::TimeZone as _;
use gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, MouseButton, ParentElement as _, Pixels, Render,
    SharedString, Size, StatefulInteractiveElement as _, Styled as _, Subscription, Window, div,
    px, size,
};
use gpui_component::{
    ActiveTheme as _, Disableable as _, Sizable as _, VirtualListScrollHandle, WindowExt as _,
    button::{Button, ButtonVariants as _},
    dock::{Panel, PanelEvent, TabPanel},
    h_flex,
    input::{Input, InputEvent, InputState},
    v_flex, v_virtual_list,
};

use crate::panels::{Kind, LastFocusedTabPanel};
use crate::prefs;
use crate::services::insider::{
    InsiderEvent, InsiderServiceHandle, InsiderState, InsiderTrade,
};

const ROW_HEIGHT_PX: f32 = 28.0;

/// Pre-formatted strings for one visible row. Built once per service
/// `Changed` (not per render) so the hot scroll closure only clones
/// SharedStrings — no `format!` / `to_string` per visible row per frame.
#[derive(Clone)]
struct InsiderRow {
    ticker: SharedString,
    ticker_lower: String,
    date_label: SharedString,
    filer: SharedString,
    code: SharedString,
    code_kind: CodeKind,
    shares_label: SharedString,
    value_label: SharedString,
    row_id: SharedString,
    /// Owned copies for the click handler so the dialog opener doesn't
    /// reborrow into the cached Rc.
    raw: InsiderTrade,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CodeKind {
    Buy,
    Sell,
    Other,
}

impl CodeKind {
    fn from_code(s: &str) -> Self {
        match s.trim().to_ascii_uppercase().as_str() {
            "P" | "A" => CodeKind::Buy,
            "S" | "D" => CodeKind::Sell,
            _ => CodeKind::Other,
        }
    }
}

pub struct InsiderPanel {
    focus_handle: FocusHandle,
    parent_tab_panel: Option<gpui::WeakEntity<TabPanel>>,
    service: Entity<crate::services::insider::InsiderService>,
    scroll_handle: VirtualListScrollHandle,
    ticker_filter: Entity<InputState>,
    cached_rows: Rc<Vec<InsiderRow>>,
    cached_status: Option<SharedString>,
    _service_subscription: Subscription,
    _ticker_filter_subscription: Subscription,
}

impl InsiderPanel {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let service = cx.global::<InsiderServiceHandle>().0.clone();
        let _service_subscription =
            cx.subscribe(&service, |this, svc, _ev: &InsiderEvent, cx| {
                let snapshot = svc.read(cx).state().clone();
                if let InsiderState::Error { message, .. } = &snapshot {
                    log::error!("insider fetch failed: {message}");
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

    fn apply_state(&mut self, state: InsiderState, cx: &mut Context<Self>) {
        match state {
            InsiderState::Idle | InsiderState::Loading => {
                self.cached_status = Some(SharedString::from("Loading…"));
            }
            InsiderState::Loaded { trades, fetched_at } => {
                let offset = prefs::offset_for(cx, fetched_at.timestamp_millis());
                let local = offset.from_utc_datetime(&fetched_at.naive_utc());
                self.cached_status = Some(SharedString::from(format!(
                    "Updated {}",
                    local.format("%H:%M")
                )));
                self.cached_rows = Rc::new(Self::build_rows(&trades, cx));
            }
            InsiderState::Error { last, .. } => {
                self.cached_status = Some(SharedString::from(match &last {
                    Some((_, t)) => {
                        let offset = prefs::offset_for(cx, t.timestamp_millis());
                        let local = offset.from_utc_datetime(&t.naive_utc());
                        format!("Stale (last {})", local.format("%H:%M"))
                    }
                    None => "Failed to load".to_string(),
                }));
                if let Some((trades, _)) = last {
                    self.cached_rows = Rc::new(Self::build_rows(&trades, cx));
                }
            }
        }
    }

    fn build_rows(trades: &[InsiderTrade], cx: &Context<Self>) -> Vec<InsiderRow> {
        trades
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let date_label = match t.transaction_at {
                    Some(dt) => {
                        let offset = prefs::offset_for(cx, dt.timestamp_millis());
                        let local = offset.from_utc_datetime(&dt.naive_utc());
                        SharedString::from(local.format("%Y-%m-%d").to_string())
                    }
                    None => SharedString::from(t.transaction_date.clone()),
                };
                let code_kind = CodeKind::from_code(&t.transaction_code);
                let code = SharedString::from(if t.transaction_code.is_empty() {
                    "—".to_string()
                } else {
                    t.transaction_code.clone()
                });
                let shares_label = SharedString::from(
                    t.shares.map(format_shares).unwrap_or_else(|| "—".into()),
                );
                let value_label = SharedString::from(match t.value.or_else(|| {
                    match (t.shares, t.price) {
                        (Some(s), Some(p)) => Some(s * p),
                        _ => None,
                    }
                }) {
                    Some(v) => format_usd_compact(v),
                    None => "—".into(),
                });
                let filer = SharedString::from(if t.filer_name.is_empty() {
                    "—".to_string()
                } else {
                    t.filer_name.clone()
                });
                let row_id = SharedString::from(format!(
                    "insider-{}-{}-{i}",
                    t.ticker,
                    if t.accession_number.is_empty() {
                        "x"
                    } else {
                        t.accession_number.as_str()
                    },
                ));
                InsiderRow {
                    ticker_lower: t.ticker.to_lowercase(),
                    ticker: SharedString::from(t.ticker.clone()),
                    date_label,
                    filer,
                    code,
                    code_kind,
                    shares_label,
                    value_label,
                    row_id,
                    raw: t.clone(),
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

impl EventEmitter<PanelEvent> for InsiderPanel {}

impl Focusable for InsiderPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Panel for InsiderPanel {
    fn panel_name(&self) -> &'static str {
        Kind::Insider.id()
    }

    fn title(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        SharedString::from(Kind::Insider.display())
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

impl Render for InsiderPanel {
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

        let rows_rc = self.cached_rows.clone();
        let status_label = self.cached_status.clone();
        let total_count = rows_rc.len();

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
            SharedString::from(format!("{} of {} trades", visible_count, total_count))
        } else {
            SharedString::from(format!("{} trades", total_count))
        };

        let mut top_header = h_flex()
            .px_2()
            .py_1()
            .gap_2()
            .items_center()
            .child(
                Button::new("refresh-insider")
                    .label("↻")
                    .small()
                    .ghost()
                    .tooltip("Refresh insider trades")
                    .on_click(cx.listener(|this, _, _, cx| this.trigger_refresh(cx))),
            );
        if let Some(label) = status_label {
            top_header = top_header
                .child(div().text_size(px(11.)).text_color(muted).child(label));
        }
        top_header = top_header
            .child(div().flex_1())
            .child(div().text_size(px(11.)).text_color(muted).child(count_label));

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

        // Column header — every cell carries min_w_0 + ellipsis so longer
        // strings can't push the row wider than its declared width.
        let column_header = h_flex()
            .px_2()
            .py_1()
            .gap_2()
            .text_size(px(11.))
            .text_color(muted)
            .border_b_1()
            .border_color(border)
            .child(col_header(72., "Date"))
            .child(col_header(56., "Ticker"))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .child("Insider"),
            )
            .child(col_header(40., "Code"))
            .child(col_header(80., "Shares"))
            .child(col_header(80., "Value"));

        let body_rows: gpui::AnyElement = if visible_count == 0 {
            let msg = if filter_active {
                "No insider trades match the active filter."
            } else {
                "No recent insider transactions for tickers in your watchlist."
            };
            div()
                .py_6()
                .px_2()
                .text_size(px(13.))
                .text_color(muted)
                .child(msg)
                .into_any_element()
        } else {
            let item_sizes: Rc<Vec<Size<Pixels>>> = Rc::new(
                (0..visible_count)
                    .map(|_| size(px(0.), px(ROW_HEIGHT_PX)))
                    .collect(),
            );
            let rows_for_closure = rows_rc.clone();
            let visible_for_closure = visible_idx.clone();
            v_virtual_list(
                cx.entity().clone(),
                "insider-rows",
                item_sizes,
                move |_this, visible_range, _window, cx| {
                    let theme = cx.theme();
                    let muted = theme.muted_foreground;
                    let border = theme.border;
                    let accent = theme.accent;
                    let accent_fg = theme.accent_foreground;
                    let fg = theme.foreground;
                    let hover_bg = theme.accent;
                    let buy_color = gpui::rgb(0x16a34a); // green-600
                    let sell_color = gpui::rgb(0xdc2626); // red-600
                    visible_range
                        .map(|vi| {
                            let i = visible_for_closure[vi];
                            let r = &rows_for_closure[i];
                            let code_color = match r.code_kind {
                                CodeKind::Buy => buy_color.into(),
                                CodeKind::Sell => sell_color.into(),
                                CodeKind::Other => muted,
                            };
                            let click_trade = r.raw.clone();
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
                                    open_insider_dialog(click_trade.clone(), window, cx);
                                })
                                .child(
                                    cell(72.)
                                        .text_size(px(11.))
                                        .text_color(muted)
                                        .child(r.date_label.clone()),
                                )
                                .child(cell(56.).child(
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
                                ))
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .overflow_hidden()
                                        .whitespace_nowrap()
                                        .text_ellipsis()
                                        .text_color(fg)
                                        .child(r.filer.clone()),
                                )
                                .child(
                                    cell(40.)
                                        .text_color(code_color)
                                        .child(r.code.clone()),
                                )
                                .child(
                                    cell(80.)
                                        .text_size(px(11.))
                                        .text_color(fg)
                                        .child(r.shares_label.clone()),
                                )
                                .child(
                                    cell(80.)
                                        .text_size(px(11.))
                                        .text_color(fg)
                                        .child(r.value_label.clone()),
                                )
                        })
                        .collect()
                },
            )
            .track_scroll(&self.scroll_handle)
            .into_any_element()
        };

        let scope_hint = div()
            .px_2()
            .pb_1()
            .text_size(px(11.))
            .text_color(muted)
            .child("Showing Form 4 insider transactions for tickers in your watchlist.");

        let body = v_flex()
            .size_full()
            .child(top_header)
            .child(scope_hint)
            .child(filter_row)
            .child(column_header)
            .child(div().flex_1().min_h_0().size_full().child(body_rows));

        div()
            .id("insider-panel-body")
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

fn col_header(width_px: f32, label: &'static str) -> gpui::Div {
    div()
        .w(px(width_px))
        .min_w_0()
        .overflow_hidden()
        .whitespace_nowrap()
        .text_ellipsis()
        .child(label)
}

fn cell(width_px: f32) -> gpui::Div {
    div()
        .w(px(width_px))
        .min_w_0()
        .overflow_hidden()
        .whitespace_nowrap()
        .text_ellipsis()
}

/// Compact share count: 1234567 → "1.23M", 4567 → "4.57K", 800 → "800".
fn format_shares(n: f64) -> String {
    let abs = n.abs();
    if abs >= 1_000_000.0 {
        format!("{:.2}M", n / 1_000_000.0)
    } else if abs >= 1_000.0 {
        format!("{:.2}K", n / 1_000.0)
    } else {
        format!("{:.0}", n)
    }
}

/// Compact $-value: 12_345_678 → "$12.3M", 4567 → "$4.57K".
fn format_usd_compact(n: f64) -> String {
    let abs = n.abs();
    if abs >= 1_000_000_000.0 {
        format!("${:.2}B", n / 1_000_000_000.0)
    } else if abs >= 1_000_000.0 {
        format!("${:.2}M", n / 1_000_000.0)
    } else if abs >= 1_000.0 {
        format!("${:.2}K", n / 1_000.0)
    } else {
        format!("${:.0}", n)
    }
}

/// Dialog body for clicked row — kept in this file alongside the panel so
/// the prompt strings + formatting stay together. Phase D will reshape if
/// we move dialogs into a shared helper module.
fn open_insider_dialog(trade: InsiderTrade, window: &mut Window, cx: &mut App) {
    use gpui_component::{
        dialog::DialogButtonProps, separator::Separator,
    };

    let date_label = SharedString::from(if trade.transaction_date.is_empty() {
        "—".to_string()
    } else {
        trade.transaction_date.clone()
    });
    let action_label = match CodeKind::from_code(&trade.transaction_code) {
        CodeKind::Buy => "bought",
        CodeKind::Sell => "sold",
        CodeKind::Other => "transacted",
    };
    let shares_text = trade
        .shares
        .map(format_shares)
        .unwrap_or_else(|| "an unknown amount of".into());
    let price_text = trade
        .price
        .map(|p| format!("${:.2}", p))
        .unwrap_or_else(|| "an undisclosed price".into());
    let filer = if trade.filer_name.is_empty() {
        "an insider".to_string()
    } else {
        trade.filer_name.clone()
    };
    let ask_prompt = SharedString::from(format!(
        "About {ticker} Form 4 filed {date}:\n\
         {filer}{title} {action} {shares} shares at {price}.\n\
         URL: {url}\n\n\
         Summarize the transaction and any implications for the stock.",
        ticker = trade.ticker,
        date = date_label,
        filer = filer,
        title = if trade.insider_title.is_empty() {
            String::new()
        } else {
            format!(" ({})", trade.insider_title)
        },
        action = action_label,
        shares = shares_text,
        price = price_text,
        url = trade.filing_url,
    ));
    let dialog_title = SharedString::from(format!("{} · Form 4", trade.ticker));

    window.open_dialog(cx, move |dialog, _w, cx| {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let fg = theme.foreground;

        let kv = |label: &'static str, value: SharedString| {
            h_flex()
                .gap_2()
                .items_baseline()
                .child(
                    div()
                        .w(px(112.))
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

        let shares_str = trade
            .shares
            .map(format_shares)
            .unwrap_or_else(|| "—".into());
        let price_str = trade
            .price
            .map(|p| format!("${:.2}", p))
            .unwrap_or_else(|| "—".into());
        let value_str = trade
            .value
            .or_else(|| match (trade.shares, trade.price) {
                (Some(s), Some(p)) => Some(s * p),
                _ => None,
            })
            .map(format_usd_compact)
            .unwrap_or_else(|| "—".into());
        let after_str = trade
            .shares_owned_after
            .map(format_shares)
            .unwrap_or_else(|| "—".into());
        let code_str = if trade.transaction_code.is_empty() {
            "—".to_string()
        } else {
            trade.transaction_code.clone()
        };
        let title_str = if trade.insider_title.is_empty() {
            "—".to_string()
        } else {
            trade.insider_title.clone()
        };

        let body = v_flex()
            .px_4()
            .pb_2()
            .pt_2()
            .gap_3()
            .child(kv("Ticker", SharedString::from(trade.ticker.clone())))
            .child(kv("Filed", date_label.clone()))
            .child(kv("Insider", SharedString::from(filer.clone())))
            .child(kv("Title", SharedString::from(title_str)))
            .child(kv("Code", SharedString::from(code_str)))
            .child(kv("Shares", SharedString::from(shares_str)))
            .child(kv("Price", SharedString::from(price_str)))
            .child(kv("Value", SharedString::from(value_str)))
            .child(kv("After", SharedString::from(after_str)))
            .child(Separator::horizontal())
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        Button::new("insider-open-edgar")
                            .label("Open filing")
                            .small()
                            .outline()
                            .disabled(trade.filing_url.is_empty())
                            .on_click({
                                let url = trade.filing_url.clone();
                                move |_, _, _| {
                                    if url.is_empty() {
                                        return;
                                    }
                                    if let Some(w) = web_sys::window() {
                                        let _ = w.open_with_url_and_target(
                                            url.as_str(),
                                            "_blank",
                                        );
                                    }
                                }
                            }),
                    )
                    .child(
                        Button::new("insider-ask-ai")
                            .label("Ask AI")
                            .small()
                            .primary()
                            .on_click({
                                let prompt = ask_prompt.clone();
                                move |_, window, cx| {
                                    window.dispatch_action(
                                        Box::new(crate::top_bar::AskAi(prompt.clone())),
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
