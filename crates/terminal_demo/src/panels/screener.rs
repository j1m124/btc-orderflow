use gpui::{Context, IntoElement, ParentElement as _, Styled as _, Window, div};
use gpui_component::{ActiveTheme as _, StyledExt as _, h_flex, v_flex};

use super::ContentPanel;

// ============================================================================
// Screener
// ============================================================================

#[derive(Clone, Copy)]
enum ScreenerSignal {
    Breakout,
    Oversold,
    Squeeze,
    Reversal,
    Momentum,
}

struct ScreenerRow {
    symbol: &'static str,
    sector: &'static str,
    last: f64,
    change_pct: f64,
    rel_vol: f64,
    market_cap: &'static str,
    rsi: f64,
    signal: ScreenerSignal,
}

const SCREENER_ROWS: &[ScreenerRow] = &[
    ScreenerRow {
        symbol: "NVDA",
        sector: "Semiconductors",
        last: 875.21,
        change_pct: 3.87,
        rel_vol: 2.4,
        market_cap: "$2.16T",
        rsi: 68.4,
        signal: ScreenerSignal::Breakout,
    },
    ScreenerRow {
        symbol: "META",
        sector: "Communications",
        last: 502.34,
        change_pct: 2.14,
        rel_vol: 1.8,
        market_cap: "$1.28T",
        rsi: 62.1,
        signal: ScreenerSignal::Momentum,
    },
    ScreenerRow {
        symbol: "AAPL",
        sector: "Technology",
        last: 185.32,
        change_pct: 1.23,
        rel_vol: 1.1,
        market_cap: "$2.85T",
        rsi: 54.7,
        signal: ScreenerSignal::Squeeze,
    },
    ScreenerRow {
        symbol: "AMD",
        sector: "Semiconductors",
        last: 162.85,
        change_pct: -2.41,
        rel_vol: 1.6,
        market_cap: "$263B",
        rsi: 38.2,
        signal: ScreenerSignal::Oversold,
    },
    ScreenerRow {
        symbol: "MU",
        sector: "Semiconductors",
        last: 119.40,
        change_pct: 4.62,
        rel_vol: 3.1,
        market_cap: "$132B",
        rsi: 71.5,
        signal: ScreenerSignal::Breakout,
    },
    ScreenerRow {
        symbol: "PLTR",
        sector: "Software",
        last: 24.18,
        change_pct: 5.94,
        rel_vol: 2.9,
        market_cap: "$54B",
        rsi: 74.8,
        signal: ScreenerSignal::Momentum,
    },
    ScreenerRow {
        symbol: "CRWD",
        sector: "Cybersecurity",
        last: 318.40,
        change_pct: -1.85,
        rel_vol: 1.4,
        market_cap: "$77B",
        rsi: 41.3,
        signal: ScreenerSignal::Reversal,
    },
    ScreenerRow {
        symbol: "SMCI",
        sector: "Hardware",
        last: 712.00,
        change_pct: 7.18,
        rel_vol: 4.2,
        market_cap: "$41B",
        rsi: 78.9,
        signal: ScreenerSignal::Breakout,
    },
    ScreenerRow {
        symbol: "TSLA",
        sector: "Auto",
        last: 248.50,
        change_pct: -1.89,
        rel_vol: 1.7,
        market_cap: "$789B",
        rsi: 36.4,
        signal: ScreenerSignal::Oversold,
    },
    ScreenerRow {
        symbol: "ARM",
        sector: "Semiconductors",
        last: 142.65,
        change_pct: 2.92,
        rel_vol: 2.0,
        market_cap: "$148B",
        rsi: 64.7,
        signal: ScreenerSignal::Squeeze,
    },
    ScreenerRow {
        symbol: "AVGO",
        sector: "Semiconductors",
        last: 1342.10,
        change_pct: 1.74,
        rel_vol: 1.3,
        market_cap: "$626B",
        rsi: 58.9,
        signal: ScreenerSignal::Momentum,
    },
    ScreenerRow {
        symbol: "SHOP",
        sector: "Software",
        last: 75.40,
        change_pct: -3.18,
        rel_vol: 1.5,
        market_cap: "$96B",
        rsi: 32.7,
        signal: ScreenerSignal::Reversal,
    },
];

pub fn render(_window: &mut Window, cx: &mut Context<ContentPanel>) -> impl IntoElement {
    let theme = cx.theme();
    let muted = theme.muted_foreground;
    let bullish = theme.chart_bullish;
    let bearish = theme.chart_bearish;
    let border = theme.border;

    let chip = |label: &'static str, active: bool| {
        let (bg, fg) = if active {
            (theme.primary, theme.primary_foreground)
        } else {
            (theme.muted, theme.muted_foreground)
        };
        div()
            .px_2()
            .py_0p5()
            .rounded(gpui::px(999.))
            .bg(bg)
            .text_color(fg)
            .text_xs()
            .child(label)
    };

    let filter_bar = v_flex()
        .px_3()
        .py_2()
        .gap_2()
        .border_b_1()
        .border_color(border)
        .child(
            h_flex()
                .gap_2()
                .items_center()
                .child(div().text_sm().font_semibold().child("Stock Screener"))
                .child(div().flex_1())
                .child(
                    div()
                        .text_xs()
                        .text_color(muted)
                        .child(format!("{} matches", SCREENER_ROWS.len())),
                ),
        )
        .child(
            h_flex()
                .gap_2()
                .flex_wrap()
                .child(div().text_xs().text_color(muted).child("Universe:"))
                .child(chip("S&P 500", true))
                .child(chip("NASDAQ 100", false))
                .child(chip("Russell 2000", false))
                .child(div().w_2())
                .child(div().text_xs().text_color(muted).child("Cap:"))
                .child(chip("Mega", true))
                .child(chip("Large", true))
                .child(chip("Mid", false)),
        )
        .child(
            h_flex()
                .gap_2()
                .flex_wrap()
                .child(div().text_xs().text_color(muted).child("Signal:"))
                .child(chip("Breakout", true))
                .child(chip("Momentum", true))
                .child(chip("Oversold", true))
                .child(chip("Squeeze", true))
                .child(chip("Reversal", false))
                .child(div().w_2())
                .child(div().text_xs().text_color(muted).child("RVol > 1.0"))
                .child(div().text_xs().text_color(muted).child("· Price > $20")),
        );

    let header_row = h_flex()
        .px_3()
        .py_1p5()
        .gap_2()
        .text_xs()
        .text_color(muted)
        .border_b_1()
        .border_color(border)
        .child(div().w(gpui::px(64.)).child("Symbol"))
        .child(div().flex_1().child("Sector"))
        .child(div().w(gpui::px(72.)).text_right().child("Last"))
        .child(div().w(gpui::px(64.)).text_right().child("Chg %"))
        .child(div().w(gpui::px(56.)).text_right().child("RVol"))
        .child(div().w(gpui::px(56.)).text_right().child("RSI"))
        .child(div().w(gpui::px(72.)).text_right().child("Cap"))
        .child(div().w(gpui::px(80.)).text_right().child("Signal"));

    let rows = v_flex().children(SCREENER_ROWS.iter().map(|r| {
        let chg_color = if r.change_pct >= 0.0 {
            bullish
        } else {
            bearish
        };
        let (signal_label, signal_color) = match r.signal {
            ScreenerSignal::Breakout => ("BREAKOUT", bullish),
            ScreenerSignal::Oversold => ("OVERSOLD", theme.chart_4),
            ScreenerSignal::Squeeze => ("SQUEEZE", theme.chart_5),
            ScreenerSignal::Reversal => ("REVERSAL", theme.chart_3),
            ScreenerSignal::Momentum => ("MOMENTUM", theme.chart_2),
        };
        let rsi_color = if r.rsi >= 70.0 {
            bullish
        } else if r.rsi <= 30.0 {
            bearish
        } else {
            theme.foreground
        };
        h_flex()
            .px_3()
            .py_1p5()
            .gap_2()
            .text_sm()
            .border_b_1()
            .border_color(border)
            .child(div().w(gpui::px(64.)).font_semibold().child(r.symbol))
            .child(div().flex_1().text_color(muted).child(r.sector))
            .child(
                div()
                    .w(gpui::px(72.))
                    .text_right()
                    .child(format!("{:.2}", r.last)),
            )
            .child(
                div()
                    .w(gpui::px(64.))
                    .text_right()
                    .text_color(chg_color)
                    .child(format!("{:+.2}%", r.change_pct)),
            )
            .child(
                div()
                    .w(gpui::px(56.))
                    .text_right()
                    .child(format!("{:.1}x", r.rel_vol)),
            )
            .child(
                div()
                    .w(gpui::px(56.))
                    .text_right()
                    .text_color(rsi_color)
                    .child(format!("{:.0}", r.rsi)),
            )
            .child(
                div()
                    .w(gpui::px(72.))
                    .text_right()
                    .text_color(muted)
                    .child(r.market_cap),
            )
            .child(
                div().w(gpui::px(80.)).text_right().child(
                    div()
                        .px_1p5()
                        .py_0p5()
                        .rounded(gpui::px(3.))
                        .text_xs()
                        .font_semibold()
                        .text_color(signal_color)
                        .border_1()
                        .border_color(signal_color)
                        .child(signal_label),
                ),
            )
    }));

    v_flex()
        .w_full()
        .child(filter_bar)
        .child(header_row)
        .child(rows)
}

// ============================================================================
