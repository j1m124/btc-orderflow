use gpui::{
    AppContext as _, Context, InteractiveElement as _, IntoElement, ParentElement as _, Render,
    SharedString, StatefulInteractiveElement as _, Styled as _, Window, div, px,
};
use gpui_component::{
    ActiveTheme as _, IconName, Sizable as _, StyledExt as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    menu::ContextMenuExt as _,
    v_flex,
};

use super::ContentPanel;
use crate::services::market_data::{MarketDataServiceHandle, Timeframe};
use crate::services::watchlist::{WatchlistEvent, WatchlistServiceHandle};
use crate::top_bar::{FocusSymbol, RemoveWatchlistSymbol};

/// Carrier for the drag-and-drop reorder. The `Render` impl is the floating
/// drag preview shown under the cursor while dragging.
#[derive(Clone)]
struct DraggedRow {
    ticker: SharedString,
}

impl Render for DraggedRow {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_2()
            .py_1()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .font_semibold()
            .border_1()
            .border_color(cx.theme().border)
            .rounded(px(4.))
            .shadow_md()
            .opacity(0.9)
            .child(self.ticker.clone())
    }
}

/// Daily candles drive the Last / Change columns — last close = today's
/// (or most-recent) close; change % = (close - open) / open.
const WATCHLIST_TF: Timeframe = Timeframe::D1;

pub fn render(_window: &mut Window, cx: &mut Context<ContentPanel>) -> impl IntoElement {
    let (bullish, bearish, muted, border, hover_bg, fg, drag_border) = {
        let theme = cx.theme();
        (
            theme.chart_bullish,
            theme.chart_bearish,
            theme.muted_foreground,
            theme.border,
            theme.accent,
            theme.foreground,
            theme.drag_border,
        )
    };

    let symbols = cx
        .global::<WatchlistServiceHandle>()
        .0
        .read(cx)
        .symbols()
        .to_vec();

    let market = cx.global::<MarketDataServiceHandle>().0.clone();
    // Subscriptions are owned by the WatchlistEvent reconciliation in
    // `subscribe()` — render is read-only.

    let header = h_flex()
        .px_2()
        .py_1()
        .gap_2()
        .items_center()
        .child(
            div()
                .flex_1()
                .text_size(px(11.))
                .text_color(muted)
                .child("Watchlist"),
        )
        .child(
            Button::new("watchlist-add")
                .icon(IconName::Plus)
                .small()
                .ghost()
                .tooltip("Add ticker to watchlist")
                .on_click(|_, window, cx| {
                    window.dispatch_action(
                        Box::new(crate::symbol_picker::OpenSymbolPicker {
                            kind: SharedString::from("watchlist"),
                        }),
                        cx,
                    );
                }),
        );

    // Columns share width via `flex_1` so they spread evenly with the panel,
    // and text is left-aligned (default `text_left()`) per design.
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
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .child("Symbol"),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .text_right()
                .child("Last"),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .text_right()
                .child("Chg"),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .text_right()
                .child("Chg %"),
        );

    let rows = symbols.into_iter().map(move |sym| {
        let ticker = sym.clone();
        let snap = market.read(cx).snapshot(ticker.as_ref(), WATCHLIST_TF);
        let (last_text, abs_change_text, change_text, change_color) = match snap {
            Some(candles) if !candles.is_empty() => {
                let day = candles.last().unwrap();
                let abs = day.close - day.open;
                let pct = if day.open > 0.0 {
                    abs / day.open * 100.0
                } else {
                    0.0
                };
                let color = if pct >= 0.0 { bullish } else { bearish };
                (
                    SharedString::from(format!("{:.2}", day.close)),
                    SharedString::from(format!("{:+.2}", abs)),
                    SharedString::from(format!("{:+.2}%", pct)),
                    color,
                )
            }
            _ => (
                SharedString::from("—"),
                SharedString::from(""),
                SharedString::from(""),
                muted,
            ),
        };

        let click_ticker = ticker.clone();
        let menu_ticker = ticker.clone();
        let drop_target_ticker = ticker.clone();
        h_flex()
            .id(SharedString::from(format!("watchlist-row-{}", ticker)))
            .px_2()
            .py_1()
            .gap_2()
            .items_center()
            .text_size(px(13.))
            .border_b_1()
            .border_color(border)
            .cursor_pointer()
            .hover(|s| s.bg(hover_bg))
            // Drag this row's ticker. The render closure builds the floating
            // preview shown under the cursor while dragging.
            .on_drag(
                DraggedRow {
                    ticker: ticker.clone(),
                },
                |drag, _offset, _window, cx| {
                    cx.stop_propagation();
                    cx.new(|_| drag.clone())
                },
            )
            // Visual cue: highlight the top border while a drag-over is in
            // progress, signalling "drop here to insert before this row".
            .drag_over::<DraggedRow>(move |s, _, _, _| s.border_t_2().border_color(drag_border))
            .on_drop(move |drag: &DraggedRow, _window, cx| {
                let source = drag.ticker.clone();
                let target = drop_target_ticker.clone();
                if source == target {
                    return;
                }
                cx.global::<WatchlistServiceHandle>()
                    .0
                    .clone()
                    .update(cx, |svc, cx| {
                        svc.move_before(source.as_ref(), Some(target.as_ref()), cx);
                    });
            })
            .on_click(move |_, window, cx| {
                window.dispatch_action(Box::new(FocusSymbol(click_ticker.clone())), cx);
            })
            // Right-click → popup menu with a Remove action. Replaces the
            // per-row × button.
            .context_menu(move |menu, _, _| {
                menu.menu(
                    "Remove from watchlist",
                    Box::new(RemoveWatchlistSymbol(menu_ticker.clone())),
                )
            })
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .font_semibold()
                    .text_color(fg)
                    .child(ticker.clone()),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_right()
                    .text_color(fg)
                    .child(last_text),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_right()
                    .text_color(change_color)
                    .child(abs_change_text),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_right()
                    .text_color(change_color)
                    .child(change_text),
            )
    });

    v_flex()
        .w_full()
        .p_2()
        .gap_1()
        .child(header)
        .child(column_header)
        .children(rows)
}

/// Build the initial subscription-handle map for a watchlist panel. Called
/// from `ContentPanel::new_inner` so the panel's `watchlist_sub_handles` field
/// starts pre-populated for whatever symbols the watchlist already contains.
pub fn initial_handles(
    cx: &mut Context<ContentPanel>,
) -> std::collections::HashMap<
    SharedString,
    crate::services::market_data::SubscriptionHandle,
> {
    let symbols: Vec<SharedString> = cx
        .global::<WatchlistServiceHandle>()
        .0
        .read(cx)
        .symbols()
        .to_vec();
    let market = cx.global::<MarketDataServiceHandle>().0.clone();
    symbols
        .into_iter()
        .map(|sym| {
            let h = market.update(cx, |svc, cx| {
                svc.ensure(sym.as_ref(), WATCHLIST_TF, cx)
            });
            (sym, h)
        })
        .collect()
}

/// Subscribe a watchlist `ContentPanel` to the services it depends on so the
/// price columns stay live. Called once from `ContentPanel::new_inner` when
/// `kind == Kind::Watchlist`. The WatchlistEvent callback reconciles the
/// `watchlist_sub_handles` map against the live symbol list: added symbols
/// get a fresh handle, removed symbols lose theirs (the Drop triggers a
/// server-side unsubscribe if no other panel still wants the key).
pub fn subscribe(window: &mut Window, cx: &mut Context<ContentPanel>) {
    let watchlist = cx.global::<WatchlistServiceHandle>().0.clone();
    cx.subscribe(&watchlist, |this, _svc, _ev: &WatchlistEvent, cx| {
        reconcile_handles(this, cx);
        cx.notify();
    })
    .detach();

    let market = cx.global::<MarketDataServiceHandle>().0.clone();
    cx.subscribe_in(
        &market,
        window,
        |_this, _svc, ev: &crate::services::market_data::KlineEvent, _window, cx| {
            use crate::services::market_data::KlineEvent::*;
            match ev {
                Tick { tf, .. } | Resnap { tf, .. } | Prepended { tf, .. } => {
                    if *tf == WATCHLIST_TF {
                        cx.notify();
                    }
                }
                _ => {}
            }
        },
    )
    .detach();
}

fn reconcile_handles(this: &mut ContentPanel, cx: &mut Context<ContentPanel>) {
    let symbols: Vec<SharedString> = cx
        .global::<WatchlistServiceHandle>()
        .0
        .read(cx)
        .symbols()
        .to_vec();
    let market = cx.global::<MarketDataServiceHandle>().0.clone();
    let mut next = std::collections::HashMap::new();
    for sym in symbols {
        if let Some(existing) = this.watchlist_sub_handles.remove(&sym) {
            next.insert(sym, existing);
        } else {
            let h = market.update(cx, |svc, cx| {
                svc.ensure(sym.as_ref(), WATCHLIST_TF, cx)
            });
            next.insert(sym, h);
        }
    }
    // Whatever's left in this.watchlist_sub_handles is for removed symbols;
    // replacing the map drops those handles → server-side unsubscribe.
    this.watchlist_sub_handles = next;
}
