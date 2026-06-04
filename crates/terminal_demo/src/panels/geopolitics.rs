use gpui::{
    Context, InteractiveElement as _, IntoElement, ParentElement as _, SharedString, Styled as _,
    Window, div,
};
use gpui_component::{ActiveTheme as _, StyledExt as _, h_flex, v_flex};

use super::ContentPanel;

// ============================================================================
// Geopolitics
// ============================================================================

#[derive(Clone, Copy)]
enum GeoSeverity {
    Critical,
    High,
    Medium,
    Low,
}

#[derive(Clone, Copy)]
enum GeoRegion {
    MiddleEast,
    Europe,
    AsiaPacific,
    Americas,
    Africa,
}

struct GeoEvent {
    when: &'static str,
    region: GeoRegion,
    severity: GeoSeverity,
    headline: &'static str,
    summary: &'static str,
    asset_impact: &'static [(&'static str, f64)],
}

const GEO_EVENTS: &[GeoEvent] = &[
    GeoEvent {
        when: "12m ago",
        region: GeoRegion::MiddleEast,
        severity: GeoSeverity::Critical,
        headline: "Strikes near Iranian oil terminal at Kharg Island",
        summary: "Reuters reports two explosions near loading jetties. Tankers reroute. Brent +3.4% intraday; Saudi defense cabinet convenes.",
        asset_impact: &[
            ("BRENT", 3.42),
            ("XLE", 1.85),
            ("SPX", -0.62),
            ("USDJPY", -0.41),
        ],
    },
    GeoEvent {
        when: "47m ago",
        region: GeoRegion::Europe,
        severity: GeoSeverity::High,
        headline: "EU agrees on 14th sanctions package against Russia",
        summary: "Council greenlights LNG transshipment ban via EU ports plus secondary sanctions on third-country re-exporters. Brussels signals more measures by Q3.",
        asset_impact: &[("EUR", -0.18), ("TTF-GAS", 2.10), ("STOXX600", -0.34)],
    },
    GeoEvent {
        when: "1h ago",
        region: GeoRegion::AsiaPacific,
        severity: GeoSeverity::High,
        headline: "Taiwan reports record PLA Navy incursion across median line",
        summary: "Taiwan MND says 28 PLAN vessels and 47 aircraft crossed the strait’s median line overnight. Pentagon comments expected pre-market.",
        asset_impact: &[
            ("TSM", -2.12),
            ("SOXX", -1.04),
            ("USDTWD", 0.38),
            ("XAU", 0.62),
        ],
    },
    GeoEvent {
        when: "2h ago",
        region: GeoRegion::Americas,
        severity: GeoSeverity::Medium,
        headline: "Mexico court ruling threatens to delay USMCA review",
        summary: "Constitutional ruling on energy nationalization complicates U.S.–Mexico negotiations ahead of 2026 USMCA review. Auto and energy supply-chain risk repriced.",
        asset_impact: &[("MXN", -0.58), ("EWW", -0.94), ("F", -0.41)],
    },
    GeoEvent {
        when: "3h ago",
        region: GeoRegion::Europe,
        severity: GeoSeverity::Medium,
        headline: "France downgraded to AA- by S&P; spreads widen vs Bunds",
        summary: "Cited fiscal slippage and political fragmentation. OAT-Bund 10y spread blows out to 78bp, the widest since 2012.",
        asset_impact: &[("CAC40", -0.88), ("OAT-BUND", 7.80), ("EUR", -0.27)],
    },
    GeoEvent {
        when: "5h ago",
        region: GeoRegion::AsiaPacific,
        severity: GeoSeverity::Low,
        headline: "BoJ governor Ueda signals patience on next hike",
        summary: "Speech at IMF: data-dependent, but “normalization remains the direction.” Yen weakens through 158 against the dollar.",
        asset_impact: &[("USDJPY", 0.74), ("NKY", 1.12), ("JGB10Y", 1.20)],
    },
    GeoEvent {
        when: "Yesterday",
        region: GeoRegion::Africa,
        severity: GeoSeverity::Medium,
        headline: "Niger junta orders French uranium operator to suspend ops",
        summary: "Orano-operated Arlit mine paused. Uranium spot up 4%. EDF reviews fuel-supply contingencies.",
        asset_impact: &[("URA", 4.02), ("CCJ", 3.18), ("XAU", 0.41)],
    },
    GeoEvent {
        when: "Yesterday",
        region: GeoRegion::MiddleEast,
        severity: GeoSeverity::High,
        headline: "Houthi attacks resume in Red Sea after 2-week lull",
        summary: "Two more bulkers struck near Bab el-Mandeb. CMA CGM and Maersk extend Cape of Good Hope reroutes through end of quarter.",
        asset_impact: &[("FBX-CN-EU", 8.40), ("ZIM", 5.12), ("BRENT", 1.20)],
    },
];

pub fn render(_window: &mut Window, cx: &mut Context<ContentPanel>) -> impl IntoElement {
    let theme = cx.theme();
    let muted = theme.muted_foreground;
    let bullish = theme.chart_bullish;
    let bearish = theme.chart_bearish;
    let border = theme.border;

    let region_label_color = |r: GeoRegion| -> (&'static str, gpui::Hsla) {
        match r {
            GeoRegion::MiddleEast => ("MIDDLE EAST", theme.chart_3),
            GeoRegion::Europe => ("EUROPE", theme.chart_2),
            GeoRegion::AsiaPacific => ("ASIA-PACIFIC", theme.chart_5),
            GeoRegion::Americas => ("AMERICAS", theme.chart_1),
            GeoRegion::Africa => ("AFRICA", theme.chart_4),
        }
    };

    let severity_label_color = |s: GeoSeverity| -> (&'static str, gpui::Hsla) {
        match s {
            GeoSeverity::Critical => ("CRITICAL", theme.chart_bearish),
            GeoSeverity::High => ("HIGH", theme.chart_4),
            GeoSeverity::Medium => ("MEDIUM", theme.chart_5),
            GeoSeverity::Low => ("LOW", theme.chart_bullish),
        }
    };

    let header = h_flex()
        .px_3()
        .py_2()
        .items_center()
        .gap_2()
        .border_b_1()
        .border_color(border)
        .child(div().size_2().rounded_full().bg(theme.chart_4))
        .child(div().text_sm().font_semibold().child("Geopolitical Watch"))
        .child(div().flex_1())
        .child(
            div()
                .text_xs()
                .text_color(muted)
                .child("Live · macro & cross-asset impact"),
        );

    let events = v_flex()
        .w_full()
        .p_2()
        .gap_2()
        .children(GEO_EVENTS.iter().enumerate().map(|(idx, e)| {
            let (region_label, region_color) = region_label_color(e.region);
            let (sev_label, sev_color) = severity_label_color(e.severity);
            v_flex()
                .id(SharedString::from(format!("geo-row-{idx}")))
                .px_3()
                .py_2()
                .gap_2()
                .rounded(gpui::px(6.))
                .border_1()
                .border_color(border)
                .child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .text_xs()
                        .child(
                            div()
                                .px_1p5()
                                .py_0p5()
                                .rounded(gpui::px(3.))
                                .bg(sev_color)
                                .text_color(theme.background)
                                .font_semibold()
                                .child(sev_label),
                        )
                        .child(
                            div()
                                .px_1p5()
                                .py_0p5()
                                .rounded(gpui::px(3.))
                                .border_1()
                                .border_color(region_color)
                                .text_color(region_color)
                                .font_semibold()
                                .child(region_label),
                        )
                        .child(div().flex_1())
                        .child(div().text_color(muted).child(e.when)),
                )
                .child(
                    div()
                        .text_sm()
                        .font_semibold()
                        .text_color(theme.foreground)
                        .child(e.headline),
                )
                .child(div().text_xs().text_color(muted).child(e.summary))
                .child(
                    h_flex()
                        .gap_2()
                        .flex_wrap()
                        .children(e.asset_impact.iter().map(|(asset, pct)| {
                            let color = if *pct >= 0.0 { bullish } else { bearish };
                            h_flex()
                                .px_1p5()
                                .py_0p5()
                                .gap_1()
                                .rounded(gpui::px(3.))
                                .bg(theme.muted)
                                .text_xs()
                                .child(
                                    div()
                                        .text_color(theme.foreground)
                                        .font_semibold()
                                        .child(*asset),
                                )
                                .child(div().text_color(color).child(format!("{:+.2}%", pct)))
                        })),
                )
        }));

    v_flex().w_full().child(header).child(events)
}

// ============================================================================
