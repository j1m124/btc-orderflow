//! News panel. Watchlist-scoped feed rendered as fixed-height cards
//! (thumbnail + headline + source · ticker · relative time). Click a row
//! opens a dialog with the article URL + Ask AI.
//!
//! Auto-refresh: panel spawns a 60s heartbeat task on mount that nudges
//! [`NewsService::reload`]. The task is held in `Option<Task<()>>` so it's
//! cancelled when the panel is dropped — no background fetches when no
//! News panel is open.

use std::rc::Rc;
use std::time::Duration;

use chrono::{DateTime, TimeZone as _, Utc};
use gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, MouseButton, ParentElement as _, Pixels, Render,
    SharedString, Size, StatefulInteractiveElement as _, Styled as _, Subscription, Task, Window,
    div, img, px, size,
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
use crate::services::news::{
    NewsArticle, NewsServiceEvent, NewsServiceHandle, NewsState,
};

const CARD_HEIGHT_PX: f32 = 96.0;
const THUMB_SIZE_PX: f32 = 64.0;
/// Auto-refresh interval. Server's per-ticker cache TTL is 60s so calling
/// at this cadence costs nothing extra when the cache is warm.
const AUTO_REFRESH_MS: u64 = 60_000;

#[derive(Clone)]
struct NewsRow {
    title: SharedString,
    source_line: SharedString,
    relative_time: SharedString,
    ticker_chip: Option<SharedString>,
    /// Cached lowercase of every ticker on the article for the substring
    /// filter — articles are multi-ticker so we can't rely on a single
    /// field like Filings/Insider do.
    tickers_lower: String,
    image_url: Option<SharedString>,
    row_id: SharedString,
    raw: NewsArticle,
}

pub struct NewsPanel {
    focus_handle: FocusHandle,
    parent_tab_panel: Option<gpui::WeakEntity<TabPanel>>,
    service: Entity<crate::services::news::NewsService>,
    scroll_handle: VirtualListScrollHandle,
    ticker_filter: Entity<InputState>,
    cached_rows: Rc<Vec<NewsRow>>,
    cached_status: Option<SharedString>,
    _service_subscription: Subscription,
    _ticker_filter_subscription: Subscription,
    /// 60s heartbeat. Dropped with the panel → fetch loop stops.
    _auto_refresh: Task<()>,
}

impl NewsPanel {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let service = cx.global::<NewsServiceHandle>().0.clone();
        let _service_subscription =
            cx.subscribe(&service, |this, svc, _ev: &NewsServiceEvent, cx| {
                let snapshot = svc.read(cx).state().clone();
                if let NewsState::Error { message, .. } = &snapshot {
                    log::error!("news fetch failed: {message}");
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
        // Auto-refresh heartbeat. Sleeps first so the initial fetch (from
        // service construction) has time to land; subsequent loops just
        // nudge the service. The service's own inflight guard prevents
        // overlapping fetches. Holding a WeakEntity (not Entity) means the
        // task naturally stops when the service is dropped — `.update`
        // returns `Err` and we exit the loop.
        let svc_weak = service.downgrade();
        let _auto_refresh = cx.spawn(async move |_this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(AUTO_REFRESH_MS))
                    .await;
                if svc_weak
                    .update(cx, |svc, cx| svc.reload(cx))
                    .is_err()
                {
                    return;
                }
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
            _auto_refresh,
        }
    }

    fn apply_state(&mut self, state: NewsState, cx: &mut Context<Self>) {
        match state {
            NewsState::Idle | NewsState::Loading => {
                self.cached_status = Some(SharedString::from("Loading…"));
            }
            NewsState::Loaded { articles, fetched_at } => {
                let offset = prefs::offset_for(cx, fetched_at.timestamp_millis());
                let local = offset.from_utc_datetime(&fetched_at.naive_utc());
                self.cached_status = Some(SharedString::from(format!(
                    "Updated {}",
                    local.format("%H:%M")
                )));
                self.cached_rows = Rc::new(Self::build_rows(&articles, cx));
            }
            NewsState::Error { last, .. } => {
                self.cached_status = Some(SharedString::from(match &last {
                    Some((_, t)) => {
                        let offset = prefs::offset_for(cx, t.timestamp_millis());
                        let local = offset.from_utc_datetime(&t.naive_utc());
                        format!("Stale (last {})", local.format("%H:%M"))
                    }
                    None => "Failed to load".to_string(),
                }));
                if let Some((articles, _)) = last {
                    self.cached_rows = Rc::new(Self::build_rows(&articles, cx));
                }
            }
        }
    }

    fn build_rows(articles: &[NewsArticle], _cx: &Context<Self>) -> Vec<NewsRow> {
        let now = Utc::now();
        articles
            .iter()
            .enumerate()
            .map(|(i, a)| {
                let title = SharedString::from(if a.title.is_empty() {
                    "(untitled)".to_string()
                } else {
                    a.title.clone()
                });
                let source_line = SharedString::from(if a.author.is_empty() {
                    String::new()
                } else {
                    a.author.clone()
                });
                let relative_time = SharedString::from(
                    a.published_at
                        .map(|t| relative_label(t, now))
                        .unwrap_or_default(),
                );
                let ticker_chip = a.tickers.first().cloned().map(SharedString::from);
                let tickers_lower = a
                    .tickers
                    .iter()
                    .map(|t| t.to_lowercase())
                    .collect::<Vec<_>>()
                    .join(",");
                let image_url = if a.image_url.is_empty() {
                    None
                } else {
                    Some(SharedString::from(a.image_url.clone()))
                };
                let row_id = SharedString::from(format!(
                    "news-{}-{i}",
                    if a.id.is_empty() {
                        "x"
                    } else {
                        a.id.as_str()
                    }
                ));
                NewsRow {
                    title,
                    source_line,
                    relative_time,
                    ticker_chip,
                    tickers_lower,
                    image_url,
                    row_id,
                    raw: a.clone(),
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

impl EventEmitter<PanelEvent> for NewsPanel {}

impl Focusable for NewsPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Panel for NewsPanel {
    fn panel_name(&self) -> &'static str {
        Kind::News.id()
    }

    fn title(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        SharedString::from(Kind::News.display())
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

impl Render for NewsPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
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
                .filter(|(_, r)| q.is_empty() || r.tickers_lower.contains(&q))
                .map(|(i, _)| i)
                .collect(),
        );
        let visible_count = visible_idx.len();
        let filter_active = !q.is_empty();
        let count_label = if filter_active {
            SharedString::from(format!("{} of {} articles", visible_count, total_count))
        } else {
            SharedString::from(format!("{} articles", total_count))
        };

        let mut top_header = h_flex()
            .px_2()
            .py_1()
            .gap_2()
            .items_center()
            .child(
                Button::new("refresh-news")
                    .label("↻")
                    .small()
                    .ghost()
                    .tooltip("Refresh news")
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

        let body_rows: gpui::AnyElement = if visible_count == 0 {
            let msg = if filter_active {
                "No articles match the active filter."
            } else {
                "No recent news for tickers in your watchlist."
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
                    .map(|_| size(px(0.), px(CARD_HEIGHT_PX)))
                    .collect(),
            );
            let rows_for_closure = rows_rc.clone();
            let visible_for_closure = visible_idx.clone();
            v_virtual_list(
                cx.entity().clone(),
                "news-cards",
                item_sizes,
                move |_this, visible_range, _window, cx| {
                    let theme = cx.theme();
                    let muted = theme.muted_foreground;
                    let border = theme.border;
                    let accent = theme.accent;
                    let accent_fg = theme.accent_foreground;
                    let fg = theme.foreground;
                    let hover_bg = theme.accent;
                    let placeholder_bg = theme.muted;
                    visible_range
                        .map(|vi| {
                            let i = visible_for_closure[vi];
                            let r = &rows_for_closure[i];
                            let click_article = r.raw.clone();

                            let thumb: gpui::AnyElement = match r.image_url.as_ref() {
                                Some(url) => img(url.clone())
                                    .size(px(THUMB_SIZE_PX))
                                    .rounded(px(4.))
                                    .into_any_element(),
                                None => div()
                                    .size(px(THUMB_SIZE_PX))
                                    .rounded(px(4.))
                                    .bg(placeholder_bg)
                                    .into_any_element(),
                            };

                            let mut meta_row = h_flex().gap_2().items_center().text_size(px(11.));
                            if let Some(chip) = r.ticker_chip.as_ref() {
                                meta_row = meta_row.child(
                                    div()
                                        .px_1p5()
                                        .py_0p5()
                                        .rounded(px(3.))
                                        .bg(accent)
                                        .text_color(accent_fg)
                                        .text_size(px(10.))
                                        .child(chip.clone()),
                                );
                            }
                            if !r.source_line.is_empty() {
                                meta_row = meta_row.child(
                                    div().text_color(muted).child(r.source_line.clone()),
                                );
                            }
                            if !r.relative_time.is_empty() {
                                meta_row = meta_row.child(div().flex_1()).child(
                                    div().text_color(muted).child(r.relative_time.clone()),
                                );
                            }

                            h_flex()
                                .id(r.row_id.clone())
                                .w_full()
                                .h(px(CARD_HEIGHT_PX))
                                .px_2()
                                .py_2()
                                .gap_3()
                                .items_start()
                                .border_b_1()
                                .border_color(border)
                                .cursor_pointer()
                                .hover(|s| s.bg(hover_bg).opacity(0.95))
                                .on_click(move |_, window, cx| {
                                    open_news_dialog(click_article.clone(), window, cx);
                                })
                                .child(div().flex_shrink_0().child(thumb))
                                .child(
                                    v_flex()
                                        .flex_1()
                                        .min_w_0()
                                        .gap_1()
                                        .child(
                                            div()
                                                .text_size(px(13.))
                                                .text_color(fg)
                                                .min_w_0()
                                                .overflow_hidden()
                                                .child(r.title.clone()),
                                        )
                                        .child(meta_row),
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
            .child("Showing news for tickers in your watchlist. Auto-refreshes every 60s.");

        let body = v_flex()
            .size_full()
            .child(top_header)
            .child(scope_hint)
            .child(filter_row)
            .child(div().flex_1().min_h_0().size_full().child(body_rows));

        div()
            .id("news-panel-body")
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

/// "3m ago", "2h ago", "Mon", "2026-05-04". Tight rules so the relative
/// label stays terse — full timestamp is available in the dialog.
fn relative_label(t: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let delta = now.signed_duration_since(t);
    let secs = delta.num_seconds();
    if secs < 60 {
        return "now".to_string();
    }
    let mins = secs / 60;
    if mins < 60 {
        return format!("{mins}m ago");
    }
    let hours = mins / 60;
    if hours < 24 {
        return format!("{hours}h ago");
    }
    let days = hours / 24;
    if days < 7 {
        return format!("{days}d ago");
    }
    t.format("%Y-%m-%d").to_string()
}

fn open_news_dialog(article: NewsArticle, window: &mut Window, cx: &mut App) {
    use gpui_component::{
        dialog::DialogButtonProps, separator::Separator,
    };

    let title = SharedString::from(if article.title.is_empty() {
        "(untitled)".to_string()
    } else {
        article.title.clone()
    });
    let dialog_title = title.clone();
    let date_label = SharedString::from(
        article
            .published_at
            .map(|t| {
                t.with_timezone(&Utc)
                    .format("%Y-%m-%d %H:%M UTC")
                    .to_string()
            })
            .unwrap_or_else(|| "—".to_string()),
    );
    let source_label = SharedString::from(if article.author.is_empty() {
        "—".to_string()
    } else {
        article.author.clone()
    });
    let tickers_label = SharedString::from(if article.tickers.is_empty() {
        "—".to_string()
    } else {
        article.tickers.join(", ")
    });
    let description = SharedString::from(article.description.clone());

    let ask_prompt = SharedString::from(format!(
        "About this news article from {source} ({date}):\n\
         Headline: {title}\n\
         Tickers: {tickers}\n\
         URL: {url}\n\n\
         Summarize the article and discuss implications for the mentioned tickers.",
        source = source_label,
        date = date_label,
        title = title,
        tickers = tickers_label,
        url = article.article_url,
    ));

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
                        .w(px(96.))
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

        let mut body = v_flex()
            .px_4()
            .pb_2()
            .pt_2()
            .gap_3()
            .child(kv("Source", source_label.clone()))
            .child(kv("Published", date_label.clone()))
            .child(kv("Tickers", tickers_label.clone()));
        if !description.is_empty() {
            body = body.child(Separator::horizontal()).child(
                v_flex()
                    .gap_1()
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(muted)
                            .child("Excerpt"),
                    )
                    .child(
                        div()
                            .text_size(px(13.))
                            .text_color(fg)
                            .child(description.clone()),
                    ),
            );
        }
        body = body.child(Separator::horizontal()).child(
            h_flex()
                .gap_2()
                .items_center()
                .child(
                    Button::new("news-open-article")
                        .label("Open article")
                        .small()
                        .outline()
                        .disabled(article.article_url.is_empty())
                        .on_click({
                            let url = article.article_url.clone();
                            move |_, _, _| {
                                if url.is_empty() {
                                    return;
                                }
                                if let Some(w) = web_sys::window() {
                                    let _ = w.open_with_url_and_target(url.as_str(), "_blank");
                                }
                            }
                        }),
                )
                .child(
                    Button::new("news-ask-ai")
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
            .max_w(px(640.))
            .button_props(DialogButtonProps::default().ok_text("Close"))
            .child(body)
    });
}
