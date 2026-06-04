use gpui::{Context, IntoElement, ParentElement as _, Styled as _, Window, div};
use gpui_component::{ActiveTheme as _, StyledExt as _, h_flex, v_flex};

use super::ContentPanel;

// ============================================================================
// Smart Money
// ============================================================================

#[derive(Clone, Copy)]
enum FlowKind {
    Insider,
    Whale,
    Option,
}

struct Flow {
    kind: FlowKind,
    actor: &'static str,
    action: &'static str,
    detail: &'static str,
    notional: &'static str,
    bullish: Option<bool>,
}

const FLOWS: &[Flow] = &[
    Flow {
        kind: FlowKind::Insider,
        actor: "Tim Cook (CEO)",
        action: "SOLD",
        detail: "50,000 AAPL @ $184.21",
        notional: "-$9.21M",
        bullish: Some(false),
    },
    Flow {
        kind: FlowKind::Whale,
        actor: "0x4f3a…b21c",
        action: "MOVED",
        detail: "1,250 BTC → Coinbase",
        notional: " $86.5M",
        bullish: None,
    },
    Flow {
        kind: FlowKind::Option,
        actor: "Unusual Sweep",
        action: "CALL",
        detail: "AAPL Mar 200C · 12,000 ct",
        notional: " $3.4M",
        bullish: Some(true),
    },
    Flow {
        kind: FlowKind::Insider,
        actor: "Elon Musk (CEO)",
        action: "FILED",
        detail: "Form 4 · 100K TSLA buy",
        notional: "+$24.85M",
        bullish: Some(true),
    },
    Flow {
        kind: FlowKind::Option,
        actor: "Block Trade",
        action: "PUT",
        detail: "QQQ May 420P · 8,500 ct",
        notional: " $2.1M",
        bullish: Some(false),
    },
    Flow {
        kind: FlowKind::Whale,
        actor: "0x7a91…ff04",
        action: "DUMPED",
        detail: "42,000 ETH → Binance",
        notional: " $144M",
        bullish: Some(false),
    },
    Flow {
        kind: FlowKind::Insider,
        actor: "Satya Nadella (CEO)",
        action: "BOUGHT",
        detail: "5,000 MSFT @ $377.10",
        notional: "+$1.89M",
        bullish: Some(true),
    },
    Flow {
        kind: FlowKind::Option,
        actor: "Sweep",
        action: "CALL",
        detail: "NVDA Apr 900C · 18,000 ct",
        notional: " $5.8M",
        bullish: Some(true),
    },
    Flow {
        kind: FlowKind::Whale,
        actor: "0xc04e…118d",
        action: "ACCUM",
        detail: "920 BTC over 6h",
        notional: " $63.6M",
        bullish: Some(true),
    },
    Flow {
        kind: FlowKind::Insider,
        actor: "Andy Jassy (CEO)",
        action: "SOLD",
        detail: "12,500 AMZN @ $178.40",
        notional: "-$2.23M",
        bullish: Some(false),
    },
];

pub fn render(_window: &mut Window, cx: &mut Context<ContentPanel>) -> impl IntoElement {
    let theme = cx.theme();
    let muted = theme.muted_foreground;
    let bullish = theme.chart_bullish;
    let bearish = theme.chart_bearish;
    let border = theme.border;
    v_flex().w_full().p_2().children(FLOWS.iter().map(|f| {
        let (kind_label, kind_color) = match f.kind {
            FlowKind::Insider => ("INSIDER", theme.chart_4),
            FlowKind::Whale => ("WHALE", theme.chart_5),
            FlowKind::Option => ("OPT", theme.chart_2),
        };
        let notional_color = match f.bullish {
            Some(true) => bullish,
            Some(false) => bearish,
            None => muted,
        };
        v_flex()
            .px_2()
            .py_2()
            .gap_1()
            .border_b_1()
            .border_color(border)
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .w_16()
                            .text_xs()
                            .font_semibold()
                            .text_color(kind_color)
                            .child(kind_label),
                    )
                    .child(div().text_sm().font_semibold().child(f.actor))
                    .child(div().flex_1())
                    .child(
                        div()
                            .text_sm()
                            .font_semibold()
                            .text_color(notional_color)
                            .child(f.notional),
                    ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .text_xs()
                    .text_color(muted)
                    .child(div().w_16().child(f.action))
                    .child(div().flex_1().child(f.detail)),
            )
    }))
}

// ============================================================================
