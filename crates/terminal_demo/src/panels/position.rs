use gpui::{Context, IntoElement, ParentElement as _, Styled as _, Window, div};
use gpui_component::{ActiveTheme as _, StyledExt as _, h_flex, v_flex};

use super::ContentPanel;

// ============================================================================
// Position
// ============================================================================

#[derive(Clone, Copy)]
struct PositionRow {
    symbol: &'static str,
    side: &'static str,
    qty: i32,
    entry: f64,
    last: f64,
}

const POSITIONS: &[PositionRow] = &[
    PositionRow {
        symbol: "AAPL",
        side: "LONG",
        qty: 200,
        entry: 180.40,
        last: 192.15,
    },
    PositionRow {
        symbol: "NVDA",
        side: "LONG",
        qty: 50,
        entry: 812.00,
        last: 875.21,
    },
    PositionRow {
        symbol: "TSLA",
        side: "SHORT",
        qty: 75,
        entry: 268.30,
        last: 248.50,
    },
    PositionRow {
        symbol: "MSFT",
        side: "LONG",
        qty: 90,
        entry: 410.20,
        last: 423.85,
    },
];

pub fn render(_window: &mut Window, cx: &mut Context<ContentPanel>) -> impl IntoElement {
    let theme = cx.theme();
    let bullish = theme.chart_bullish;
    let bearish = theme.chart_bearish;
    let muted = theme.muted_foreground;
    let border = theme.border;

    let header = h_flex()
        .px_3()
        .py_1p5()
        .gap_2()
        .text_xs()
        .text_color(muted)
        .border_b_1()
        .border_color(border)
        .child(div().w(gpui::px(72.)).child("Symbol"))
        .child(div().w(gpui::px(56.)).child("Side"))
        .child(div().w(gpui::px(56.)).text_right().child("Qty"))
        .child(div().w(gpui::px(80.)).text_right().child("Entry"))
        .child(div().w(gpui::px(80.)).text_right().child("Last"))
        .child(div().flex_1().text_right().child("P/L"));

    let rows = POSITIONS.iter().map(|p| {
        let pl = (p.last - p.entry) * p.qty as f64 * if p.side == "SHORT" { -1.0 } else { 1.0 };
        let pl_color = if pl >= 0.0 { bullish } else { bearish };
        let side_color = if p.side == "LONG" { bullish } else { bearish };
        h_flex()
            .px_3()
            .py_1()
            .gap_2()
            .text_xs()
            .border_b_1()
            .border_color(border)
            .child(div().w(gpui::px(72.)).font_semibold().child(p.symbol))
            .child(div().w(gpui::px(56.)).text_color(side_color).child(p.side))
            .child(
                div()
                    .w(gpui::px(56.))
                    .text_right()
                    .child(format!("{}", p.qty)),
            )
            .child(
                div()
                    .w(gpui::px(80.))
                    .text_right()
                    .text_color(muted)
                    .child(format!("{:.2}", p.entry)),
            )
            .child(
                div()
                    .w(gpui::px(80.))
                    .text_right()
                    .child(format!("{:.2}", p.last)),
            )
            .child(
                div()
                    .flex_1()
                    .text_right()
                    .text_color(pl_color)
                    .child(format!("{:+.2}", pl)),
            )
    });

    v_flex().w_full().child(header).children(rows)
}

// ============================================================================
