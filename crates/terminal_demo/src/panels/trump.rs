use gpui::{
    Context, InteractiveElement as _, IntoElement, ParentElement as _, SharedString, Styled as _,
    Window, div,
};
use gpui_component::{ActiveTheme as _, StyledExt as _, h_flex, v_flex};

use super::ContentPanel;

// ============================================================================
// Trump Tracker
// ============================================================================

#[derive(Clone, Copy)]
enum TrumpChannel {
    TruthSocial,
    Speech,
    Press,
    Rally,
    Interview,
}

struct TrumpPost {
    when: &'static str,
    channel: TrumpChannel,
    location: &'static str,
    headline: &'static str,
    excerpt: &'static str,
    impact_tags: &'static [&'static str],
    /// Rough market lean inferred from the post — bullish/bearish/neutral risk.
    sentiment: Option<bool>,
    engagement: &'static str,
}

const TRUMP_POSTS: &[TrumpPost] = &[
    TrumpPost {
        when: "10:42",
        channel: TrumpChannel::TruthSocial,
        location: "@realDonaldTrump",
        headline: "“60% TARIFFS on Chinese EVs starting Day One. American jobs come first!”",
        excerpt: "Truth Social post promising sweeping auto tariffs if re-elected, naming BYD, Geely, and NIO as targets. Calls Detroit “a disaster waiting to be saved.”",
        impact_tags: &["TSLA", "F", "GM", "CHINA-A", "AUTO"],
        sentiment: Some(false),
        engagement: "82.4k reposts · 412k likes",
    },
    TrumpPost {
        when: "09:18",
        channel: TrumpChannel::Rally,
        location: "Erie, PA",
        headline: "Pledges to “end the EV mandate on day one” at Pennsylvania rally",
        excerpt: "60-minute speech focusing on energy policy. Promises to reopen leased federal lands for drilling and to “drill, baby, drill — bigger than ever.”",
        impact_tags: &["XOM", "CVX", "OIL", "TSLA", "RIVN"],
        sentiment: Some(true),
        engagement: "Live · 28k stream peak",
    },
    TrumpPost {
        when: "08:55",
        channel: TrumpChannel::TruthSocial,
        location: "@realDonaldTrump",
        headline: "“Powell is too late, AGAIN. Cut rates NOW or markets crash.”",
        excerpt: "Direct attack on Fed Chair ahead of FOMC. Claims the central bank is “politically captured” and demands an emergency 50bp cut.",
        impact_tags: &["FED", "DXY", "TLT", "SPX"],
        sentiment: None,
        engagement: "44.1k reposts · 198k likes",
    },
    TrumpPost {
        when: "Yesterday 22:10",
        channel: TrumpChannel::Interview,
        location: "Fox Business",
        headline: "Floats replacing income tax with universal tariff regime",
        excerpt: "In a 20-min interview, suggests a 10% across-the-board tariff could “fund the entire government.” Economists immediately push back; futures wobble.",
        impact_tags: &["WMT", "TGT", "AMZN", "USD"],
        sentiment: Some(false),
        engagement: "1.2M views · trending #1",
    },
    TrumpPost {
        when: "Yesterday 18:45",
        channel: TrumpChannel::TruthSocial,
        location: "@realDonaldTrump",
        headline: "“Bitcoin will be made in the USA. The CCP cannot have it.”",
        excerpt: "Endorses domestic mining and proposes a strategic Bitcoin reserve modeled on the SPR. Crypto markets rip 4% on the post.",
        impact_tags: &["BTC", "MARA", "RIOT", "COIN"],
        sentiment: Some(true),
        engagement: "118k reposts · 540k likes",
    },
    TrumpPost {
        when: "Yesterday 15:02",
        channel: TrumpChannel::Press,
        location: "Mar-a-Lago presser",
        headline: "Calls NATO members “delinquent” — threatens conditional defense",
        excerpt: "Says U.S. would only defend allies that meet 2% spending. European defense names spike on expectation of forced rearmament.",
        impact_tags: &["LMT", "RTX", "EU-DEF", "EUR"],
        sentiment: Some(true),
        engagement: "Pool feed · 47 outlets",
    },
    TrumpPost {
        when: "2d ago",
        channel: TrumpChannel::Speech,
        location: "CPAC keynote",
        headline: "“Day-one drilling permits — Alaska, Gulf, ANWR all reopen.”",
        excerpt: "Outlines an executive-order package to fast-track LNG export approvals and unwind the current pause. Energy ETFs gap up at the open.",
        impact_tags: &["XLE", "LNG", "OXY", "TPL"],
        sentiment: Some(true),
        engagement: "9k attendees · standing O",
    },
    TrumpPost {
        when: "2d ago",
        channel: TrumpChannel::TruthSocial,
        location: "@realDonaldTrump",
        headline: "“Pharma companies RIPPING OFF Americans. Prices coming down.”",
        excerpt: "Threatens executive action on drug pricing if Congress fails to deliver. Pharma sector sells off pre-market on policy-risk premium.",
        impact_tags: &["LLY", "PFE", "MRK", "XBI"],
        sentiment: Some(false),
        engagement: "31.7k reposts · 142k likes",
    },
];

pub fn render(_window: &mut Window, cx: &mut Context<ContentPanel>) -> impl IntoElement {
    let theme = cx.theme();
    let muted = theme.muted_foreground;
    let border = theme.border;
    let bullish = theme.chart_bullish;
    let bearish = theme.chart_bearish;
    let card_bg = theme.muted;

    let header = h_flex()
        .px_3()
        .py_2()
        .gap_2()
        .items_center()
        .border_b_1()
        .border_color(border)
        .child(div().size_2().rounded_full().bg(theme.chart_bearish))
        .child(div().text_sm().font_semibold().child("Trump Tracker"))
        .child(div().flex_1())
        .child(
            div()
                .text_xs()
                .text_color(muted)
                .child("Truth Social · speeches · pressers · interviews"),
        );

    let posts = v_flex()
        .w_full()
        .p_2()
        .gap_2()
        .children(TRUMP_POSTS.iter().enumerate().map(|(idx, p)| {
            let (channel_label, channel_color) = match p.channel {
                TrumpChannel::TruthSocial => ("TRUTH", theme.chart_3),
                TrumpChannel::Speech => ("SPEECH", theme.chart_5),
                TrumpChannel::Press => ("PRESS", theme.chart_4),
                TrumpChannel::Rally => ("RALLY", theme.chart_2),
                TrumpChannel::Interview => ("INTV", theme.chart_1),
            };
            let (sentiment_label, sentiment_color) = match p.sentiment {
                Some(true) => ("RISK-ON", bullish),
                Some(false) => ("RISK-OFF", bearish),
                None => ("MIXED", muted),
            };
            v_flex()
                .id(SharedString::from(format!("trump-row-{idx}")))
                .px_3()
                .py_2()
                .gap_2()
                .rounded(gpui::px(6.))
                .bg(card_bg)
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
                                .bg(channel_color)
                                .text_color(theme.background)
                                .font_semibold()
                                .child(channel_label),
                        )
                        .child(div().text_color(muted).child(p.location))
                        .child(div().flex_1())
                        .child(
                            div()
                                .font_semibold()
                                .text_color(sentiment_color)
                                .child(sentiment_label),
                        )
                        .child(div().text_color(muted).child(p.when)),
                )
                .child(
                    div()
                        .text_sm()
                        .font_semibold()
                        .text_color(theme.foreground)
                        .child(p.headline),
                )
                .child(div().text_xs().text_color(muted).child(p.excerpt))
                .child(
                    h_flex()
                        .gap_1()
                        .flex_wrap()
                        .children(p.impact_tags.iter().map(|t| {
                            div()
                                .px_1p5()
                                .py_0p5()
                                .rounded(gpui::px(3.))
                                .bg(theme.accent)
                                .text_color(theme.accent_foreground)
                                .text_xs()
                                .child(*t)
                        })),
                )
                .child(
                    h_flex()
                        .items_center()
                        .text_xs()
                        .text_color(muted)
                        .child(div().flex_1().child(p.engagement)),
                )
        }));

    v_flex().w_full().child(header).child(posts)
}

// ============================================================================
