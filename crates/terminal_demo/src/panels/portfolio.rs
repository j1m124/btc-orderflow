use gpui::{Context, IntoElement, ParentElement as _, Styled as _, Window, div};
use gpui_component::{ActiveTheme as _, StyledExt as _, h_flex, v_flex};

use super::ContentPanel;

// ============================================================================
// Portfolio
// ============================================================================

struct Holding {
    symbol: &'static str,
    shares: u32,
    cost: f64,
    last: f64,
}

const PORTFOLIO: &[Holding] = &[
    Holding {
        symbol: "AAPL",
        shares: 100,
        cost: 158.20,
        last: 185.32,
    },
    Holding {
        symbol: "MSFT",
        shares: 50,
        cost: 310.45,
        last: 378.45,
    },
    Holding {
        symbol: "NVDA",
        shares: 25,
        cost: 412.60,
        last: 875.21,
    },
    Holding {
        symbol: "GOOGL",
        shares: 80,
        cost: 138.10,
        last: 142.18,
    },
    Holding {
        symbol: "TSLA",
        shares: 40,
        cost: 275.00,
        last: 248.50,
    },
    Holding {
        symbol: "BRK.B",
        shares: 30,
        cost: 380.20,
        last: 412.65,
    },
];

pub fn render(_window: &mut Window, cx: &mut Context<ContentPanel>) -> impl IntoElement {
    let theme = cx.theme();
    let bullish = theme.chart_bullish;
    let bearish = theme.chart_bearish;
    let muted = theme.muted_foreground;
    let border = theme.border;

    let total_cost: f64 = PORTFOLIO.iter().map(|h| h.shares as f64 * h.cost).sum();
    let total_value: f64 = PORTFOLIO.iter().map(|h| h.shares as f64 * h.last).sum();
    let total_pl = total_value - total_cost;
    let total_pl_pct = (total_pl / total_cost) * 100.0;
    let total_color = if total_pl >= 0.0 { bullish } else { bearish };

    v_flex()
        .w_full()
        .p_2()
        .gap_2()
        .child(
            // Summary header
            v_flex()
                .px_3()
                .py_2()
                .gap_1()
                .rounded(gpui::px(6.))
                .bg(theme.muted)
                .child(
                    h_flex()
                        .items_baseline()
                        .gap_2()
                        .child(div().text_xs().text_color(muted).child("Total Value"))
                        .child(div().flex_1())
                        .child(
                            div()
                                .text_lg()
                                .font_semibold()
                                .child(format!("${:.2}", total_value)),
                        ),
                )
                .child(
                    h_flex()
                        .items_baseline()
                        .gap_2()
                        .text_sm()
                        .child(div().text_xs().text_color(muted).child("P/L"))
                        .child(div().flex_1())
                        .child(
                            div()
                                .text_color(total_color)
                                .child(format!("{:+.2} ({:+.2}%)", total_pl, total_pl_pct)),
                        ),
                ),
        )
        .child(
            h_flex()
                .px_2()
                .py_1()
                .text_xs()
                .text_color(muted)
                .border_b_1()
                .border_color(border)
                .child(div().w_16().child("Symbol"))
                .child(div().w_12().text_right().child("Sh"))
                .child(div().flex_1().text_right().child("Cost"))
                .child(div().flex_1().text_right().child("Last"))
                .child(div().flex_1().text_right().child("P/L")),
        )
        .children(PORTFOLIO.iter().map(|h| {
            let value = h.shares as f64 * h.last;
            let cost_basis = h.shares as f64 * h.cost;
            let pl = value - cost_basis;
            let pl_pct = (pl / cost_basis) * 100.0;
            let color = if pl >= 0.0 { bullish } else { bearish };
            h_flex()
                .px_2()
                .py_1()
                .text_sm()
                .child(div().w_16().font_semibold().child(h.symbol))
                .child(div().w_12().text_right().child(h.shares.to_string()))
                .child(
                    div()
                        .flex_1()
                        .text_right()
                        .text_color(muted)
                        .child(format!("{:.2}", h.cost)),
                )
                .child(div().flex_1().text_right().child(format!("{:.2}", h.last)))
                .child(
                    div()
                        .flex_1()
                        .text_right()
                        .text_color(color)
                        .child(format!("{:+.0} ({:+.1}%)", pl, pl_pct)),
                )
        }))
}

// ============================================================================
