use gpui::{
    Context, Hsla, InteractiveElement as _, IntoElement, ParentElement as _, SharedString,
    StatefulInteractiveElement as _, Styled as _, Window, div, px,
};
use gpui_component::{
    ActiveTheme as _, IconName, Sizable as _, StyledExt as _,
    button::{Button, ButtonVariants as _},
    h_flex, v_flex,
};

use super::ContentPanel;
use crate::services::signal::{Signal, SignalDirection, SignalEvent, SignalServiceHandle};
use crate::top_bar::{AddWatchlistSymbol, AskAi, FocusSymbol, SelectSignal};

// Score-color thresholds. Keeps the palette mapping in one place so the
// badge, ticker label, and any future heatmap stay consistent.
const SCORE_HIGH: u8 = 80;
const SCORE_MID: u8 = 60;

pub fn render(_window: &mut Window, cx: &mut Context<ContentPanel>) -> impl IntoElement {
    let theme = cx.theme();
    let bullish = theme.chart_bullish;
    let bearish = theme.chart_bearish;
    let muted = theme.muted_foreground;
    let border = theme.border;
    let hover_bg = theme.accent;
    let fg = theme.foreground;

    let signal_svc = cx.global::<SignalServiceHandle>().0.read(cx);
    let signals = signal_svc.signals().to_vec();
    let selected_ticker = signal_svc.selected().cloned();

    let header = h_flex()
        .px_3()
        .py_2()
        .gap_2()
        .items_center()
        .border_b_1()
        .border_color(border)
        .child(
            div()
                .flex_1()
                .text_sm()
                .font_semibold()
                .text_color(fg)
                .child("Signals"),
        )
        .child(
            div()
                .text_xs()
                .text_color(muted)
                .child(SharedString::from(format!("{} setups", signals.len()))),
        );

    let theme_accent = theme.accent;
    let rows = signals.into_iter().map(move |s| {
        let is_selected = selected_ticker.as_deref() == Some(s.ticker.as_ref());
        render_row(
            s,
            hover_bg,
            muted,
            border,
            fg,
            bullish,
            bearish,
            theme_accent,
            is_selected,
        )
    });

    v_flex()
        .w_full()
        .gap_0()
        .child(header)
        .children(rows)
}

#[allow(clippy::too_many_arguments)]
fn render_row(
    s: Signal,
    hover_bg: Hsla,
    muted: Hsla,
    border: Hsla,
    fg: Hsla,
    bullish: Hsla,
    bearish: Hsla,
    selected_bg: Hsla,
    is_selected: bool,
) -> impl IntoElement {
    let row_id = SharedString::from(format!("signal-row-{}", s.ticker));
    let click_ticker = s.ticker.clone();
    let ask_prompt = SharedString::from(format!(
        "Explain the {} setup on {} ({}): {}",
        s.setup, s.ticker, s.timeframe, s.reason
    ));
    let score_color = score_color(s.score, bullish, muted, bearish);
    let direction_color = match s.direction {
        SignalDirection::Long => bullish,
        SignalDirection::Short => bearish,
    };
    let direction_label = SharedString::from(s.direction.label());
    let score_label = SharedString::from(format!("{}", s.score));

    let mut row = h_flex()
        .id(row_id)
        .px_3()
        .py_2p5()
        .gap_3()
        .items_start()
        .border_b_1()
        .border_color(border)
        .cursor_pointer()
        .hover(|st| st.bg(hover_bg))
        .on_click(move |_, window, cx| {
            window.dispatch_action(
                Box::new(SelectSignal(click_ticker.clone())),
                cx,
            );
            window.dispatch_action(Box::new(FocusSymbol(click_ticker.clone())), cx);
        });
    if is_selected {
        row = row.bg(selected_bg);
    }
    row
        .child(
            // Score badge — circular, colored by tier.
            div()
                .w(px(44.))
                .h(px(44.))
                .flex_none()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .rounded_full()
                .border_2()
                .border_color(score_color)
                .child(
                    div()
                        .text_sm()
                        .font_semibold()
                        .text_color(score_color)
                        .child(score_label),
                )
                .child(div().text_xs().text_color(muted).child("score")),
        )
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_1()
                .child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(
                            div()
                                .text_sm()
                                .font_semibold()
                                .text_color(fg)
                                .child(s.ticker.clone()),
                        )
                        .child(direction_chip(direction_label, direction_color))
                        .child(
                            div()
                                .text_xs()
                                .text_color(muted)
                                .child(s.timeframe.clone()),
                        )
                        .child(div().flex_1())
                        .child(
                            Button::new(SharedString::from(format!("ask-{}", s.ticker)))
                                .icon(IconName::Bot)
                                .xsmall()
                                .ghost()
                                .tooltip("Ask AI about this signal")
                                .on_click(move |_, window, cx| {
                                    window.dispatch_action(
                                        Box::new(AskAi(ask_prompt.clone())),
                                        cx,
                                    );
                                }),
                        ),
                )
                .child(
                    div()
                        .text_sm()
                        .text_color(fg)
                        .child(s.setup.clone()),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(muted)
                        .child(s.reason.clone()),
                ),
        )
}

/// Right-side detail panel for Signal mode. Surfaces the currently-selected
/// signal in a roomier layout — score badge, ticker + direction, full setup
/// and reason, plus a 'Add to watchlist' button. Empty state when nothing is
/// selected.
pub fn render_detail(
    _window: &mut Window,
    cx: &mut Context<ContentPanel>,
) -> impl IntoElement {
    let theme = cx.theme();
    let bullish = theme.chart_bullish;
    let bearish = theme.chart_bearish;
    let muted = theme.muted_foreground;
    let border = theme.border;
    let fg = theme.foreground;

    let selected = cx
        .global::<SignalServiceHandle>()
        .0
        .read(cx)
        .selected_signal()
        .cloned();

    let Some(s) = selected else {
        return v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .p_6()
            .child(
                div()
                    .text_sm()
                    .text_color(muted)
                    .child("Select a signal to see details."),
            );
    };

    let score_color = score_color(s.score, bullish, muted, bearish);
    let direction_color = match s.direction {
        SignalDirection::Long => bullish,
        SignalDirection::Short => bearish,
    };
    let direction_label = SharedString::from(s.direction.label());
    let watch_ticker = s.ticker.clone();

    let header = h_flex()
        .gap_3()
        .items_center()
        .px_4()
        .py_3()
        .border_b_1()
        .border_color(border)
        .child(
            div()
                .w(px(64.))
                .h(px(64.))
                .flex_none()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .rounded_full()
                .border_2()
                .border_color(score_color)
                .child(
                    div()
                        .text_lg()
                        .font_semibold()
                        .text_color(score_color)
                        .child(SharedString::from(format!("{}", s.score))),
                )
                .child(div().text_xs().text_color(muted).child("score")),
        )
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_1()
                .child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(
                            div()
                                .text_lg()
                                .font_semibold()
                                .text_color(fg)
                                .child(s.ticker.clone()),
                        )
                        .child(direction_chip(direction_label, direction_color))
                        .child(
                            div()
                                .text_xs()
                                .text_color(muted)
                                .child(s.timeframe.clone()),
                        ),
                )
                .child(
                    div()
                        .text_sm()
                        .text_color(fg)
                        .child(s.setup.clone()),
                ),
        )
        .child(
            Button::new(SharedString::from(format!("detail-watch-{}", s.ticker)))
                .icon(IconName::Plus)
                .label("Watch")
                .small()
                .ghost()
                .tooltip("Add to watchlist")
                .on_click(move |_, window, cx| {
                    window.dispatch_action(
                        Box::new(AddWatchlistSymbol(watch_ticker.clone())),
                        cx,
                    );
                }),
        );

    let reason_section = v_flex()
        .px_4()
        .py_3()
        .gap_2()
        .child(
            div()
                .text_xs()
                .font_semibold()
                .text_color(muted)
                .child("Reason"),
        )
        .child(
            div()
                .text_sm()
                .text_color(fg)
                .child(s.reason.clone()),
        );

    let meta_section = v_flex()
        .px_4()
        .py_3()
        .gap_2()
        .border_t_1()
        .border_color(border)
        .child(
            div()
                .text_xs()
                .font_semibold()
                .text_color(muted)
                .child("Engine"),
        )
        .child(meta_row("Direction", s.direction.label(), fg, muted))
        .child(meta_row("Timeframe", s.timeframe.as_ref(), fg, muted))
        .child(meta_row("Setup", s.setup.as_ref(), fg, muted))
        .child(meta_row("Score", &format!("{}/100", s.score), fg, muted));

    v_flex()
        .w_full()
        .child(header)
        .child(reason_section)
        .child(meta_section)
}

fn meta_row(label: &str, value: &str, fg: Hsla, muted: Hsla) -> impl IntoElement {
    h_flex()
        .gap_2()
        .items_baseline()
        .child(
            div()
                .w(px(96.))
                .flex_none()
                .text_xs()
                .text_color(muted)
                .child(SharedString::from(label.to_string())),
        )
        .child(
            div()
                .flex_1()
                .text_sm()
                .text_color(fg)
                .child(SharedString::from(value.to_string())),
        )
}

fn direction_chip(label: SharedString, color: Hsla) -> impl IntoElement {
    div()
        .px_1p5()
        .py_0p5()
        .rounded(px(3.))
        .border_1()
        .border_color(color)
        .text_xs()
        .font_semibold()
        .text_color(color)
        .child(label)
}

fn score_color(score: u8, high: Hsla, mid: Hsla, low: Hsla) -> Hsla {
    if score >= SCORE_HIGH {
        high
    } else if score >= SCORE_MID {
        mid
    } else {
        low
    }
}

/// Hook a Signal `ContentPanel` to the SignalService so it repaints when the
/// engine emits new data. Detached subscription — when the panel drops, the
/// weak entity makes the callback a no-op.
pub fn subscribe(cx: &mut Context<ContentPanel>) {
    let service = cx.global::<SignalServiceHandle>().0.clone();
    cx.subscribe(&service, |_this, _svc, _ev: &SignalEvent, cx| {
        cx.notify();
    })
    .detach();
}
