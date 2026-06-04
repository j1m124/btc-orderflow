use gpui::{Context, IntoElement, ParentElement as _, Styled as _, Window, div};
use gpui_component::{ActiveTheme as _, StyledExt as _, h_flex, v_flex};

use super::ContentPanel;

// ============================================================================
// Notifications
// ============================================================================

#[derive(Clone, Copy)]
enum NotifKind {
    Alert,
    Fill,
    News,
    Warn,
}

struct Notif {
    when: &'static str,
    kind: NotifKind,
    text: &'static str,
}

const NOTIFS: &[Notif] = &[
    Notif {
        when: "now",
        kind: NotifKind::Alert,
        text: "AAPL crossed above $185.00 (price alert)",
    },
    Notif {
        when: "2m",
        kind: NotifKind::Fill,
        text: "Order filled · BUY 100 NVDA @ $873.50",
    },
    Notif {
        when: "5m",
        kind: NotifKind::News,
        text: "GOOGL beats earnings by $0.12",
    },
    Notif {
        when: "12m",
        kind: NotifKind::Warn,
        text: "TSLA dropped -2% in last 30m",
    },
    Notif {
        when: "23m",
        kind: NotifKind::Alert,
        text: "BTC reached daily high $68,420",
    },
    Notif {
        when: "45m",
        kind: NotifKind::Fill,
        text: "Order partial fill · SELL 50/200 META @ $501.80",
    },
    Notif {
        when: "1h",
        kind: NotifKind::News,
        text: "FOMC minutes released — no surprises",
    },
    Notif {
        when: "2h",
        kind: NotifKind::Warn,
        text: "VIX up +8.3% — elevated volatility",
    },
];

pub fn render(_window: &mut Window, cx: &mut Context<ContentPanel>) -> impl IntoElement {
    let theme = cx.theme();
    let muted = theme.muted_foreground;
    let border = theme.border;
    v_flex().w_full().p_2().children(NOTIFS.iter().map(|n| {
        let (label, color) = match n.kind {
            NotifKind::Alert => ("ALERT", theme.info),
            NotifKind::Fill => ("FILL", theme.chart_bullish),
            NotifKind::News => ("NEWS", theme.accent),
            NotifKind::Warn => ("WARN", theme.chart_bearish),
        };
        h_flex()
            .px_2()
            .py_2()
            .gap_3()
            .items_start()
            .border_b_1()
            .border_color(border)
            .child(
                div()
                    .w_16()
                    .text_xs()
                    .font_semibold()
                    .text_color(color)
                    .child(label),
            )
            .child(div().flex_1().text_sm().child(n.text))
            .child(
                div()
                    .w_10()
                    .text_right()
                    .text_xs()
                    .text_color(muted)
                    .child(n.when),
            )
    }))
}

// ============================================================================
