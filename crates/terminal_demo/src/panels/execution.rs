use gpui::{
    App, Context, Entity, IntoElement, ParentElement as _, SharedString, Styled as _, Window, div,
};
use gpui_component::{
    ActiveTheme as _, Sizable as _, StyledExt as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputState},
    notification::Notification,
    v_flex,
};

use super::{ContentPanel, ExecutionInputs};

// ============================================================================
// Execution
// ============================================================================

pub fn render(
    inputs: &ExecutionInputs,
    _window: &mut Window,
    cx: &mut Context<ContentPanel>,
) -> impl IntoElement {
    let theme = cx.theme();
    let muted = theme.muted_foreground;
    let bullish = theme.chart_bullish;
    let bearish = theme.chart_bearish;

    let field = |label: &'static str, input: &Entity<InputState>| {
        v_flex()
            .gap_1()
            .child(div().text_xs().text_color(muted).child(label))
            .child(Input::new(input).small())
    };

    let symbol = inputs.symbol.clone();
    let qty = inputs.quantity.clone();
    let limit = inputs.limit.clone();

    let buy = {
        let symbol = symbol.clone();
        let qty = qty.clone();
        let limit = limit.clone();
        Button::new("buy")
            .label("BUY")
            .small()
            .primary()
            .on_click(move |_, window, cx| place_order("BUY", &symbol, &qty, &limit, window, cx))
    };
    let sell = {
        let symbol = symbol.clone();
        let qty = qty.clone();
        let limit = limit.clone();
        Button::new("sell")
            .label("SELL")
            .small()
            .primary()
            .on_click(move |_, window, cx| place_order("SELL", &symbol, &qty, &limit, window, cx))
    };

    v_flex()
        .w_full()
        .p_3()
        .gap_3()
        .child(div().text_sm().font_semibold().child("Quick Order"))
        .child(field("Symbol", &inputs.symbol))
        .child(field("Quantity", &inputs.quantity))
        .child(field("Limit Price", &inputs.limit))
        .child(
            h_flex()
                .gap_2()
                .child(div().bg(bullish).rounded(gpui::px(4.)).child(buy))
                .child(div().bg(bearish).rounded(gpui::px(4.)).child(sell)),
        )
}

fn place_order(
    side: &'static str,
    symbol: &Entity<InputState>,
    qty: &Entity<InputState>,
    limit: &Entity<InputState>,
    window: &mut Window,
    cx: &mut App,
) {
    let symbol_str = symbol.read(cx).value();
    let qty_str = qty.read(cx).value();
    let limit_str = limit.read(cx).value();

    if symbol_str.trim().is_empty() || qty_str.trim().is_empty() {
        window.push_notification(
            Notification::warning("Symbol and quantity are required").title("Order rejected"),
            cx,
        );
        return;
    }

    let summary = SharedString::from(format!(
        "{side} {qty_str} {sym} @ {limit_str}",
        sym = symbol_str.trim(),
        qty_str = qty_str.trim(),
        limit_str = limit_str.trim(),
    ));
    window.push_notification(Notification::success(summary).title("Order placed"), cx);
}

