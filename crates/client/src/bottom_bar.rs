use std::time::Duration;

use chrono::{DateTime, Local};
use gpui::{Context, IntoElement, ParentElement as _, Render, SharedString, Styled as _, Task, Window, div, px};
use gpui_component::{ActiveTheme as _, StyledExt as _, h_flex};

use crate::prefs;
use crate::services::market_data::{LiveStatus, MarketDataServiceHandle};

const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Short commit SHA stamped in by `build.rs` (populated from the GHA env
/// `BUILD_SHA` at image build time). Empty string when built locally.
const BUILD_SHA: &str = env!("BUILD_SHA");

pub struct BottomBar {
    /// Number of `render()` calls observed since the last FPS sample.
    frame_count: u32,
    /// Last time we re-computed `fps`. Sampling once per ~500ms keeps the
    /// readout legible without making the bar's text twitch every frame. The
    /// underlying clock here is `Local` only because we only do elapsed
    /// arithmetic on it — the displayed clock uses the user TZ separately.
    last_fps_sample: DateTime<Local>,
    /// Last computed FPS value (rendered in the bar).
    fps: f32,
    _tick_task: Task<()>,
}

impl BottomBar {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        // Tick once per second so the clock stays accurate even when nothing
        // else in the workspace causes a redraw.
        let _tick_task = cx.spawn_in(window, async move |this, window| {
            loop {
                window
                    .background_executor()
                    .timer(Duration::from_secs(1))
                    .await;
                if this.update(window, |_, cx| cx.notify()).is_err() {
                    break;
                }
            }
        });

        Self {
            frame_count: 0,
            last_fps_sample: Local::now(),
            fps: 0.0,
            _tick_task,
        }
    }
}

impl Render for BottomBar {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();

        // Each render counts as one painted frame for this view. Recompute FPS
        // every ~500ms so the readout updates at human-readable cadence.
        self.frame_count += 1;
        let now = Local::now();
        let elapsed_ms = (now - self.last_fps_sample).num_milliseconds();
        if elapsed_ms >= 500 {
            self.fps = (self.frame_count as f32) * 1000.0 / (elapsed_ms as f32);
            self.frame_count = 0;
            self.last_fps_sample = now;
        }
        // Drive the next paint so FPS keeps sampling. Without this, gpui only
        // redraws on demand and the counter would stall as soon as the user
        // stops interacting.
        window.request_animation_frame();

        let muted = theme.muted_foreground;
        let bullish = theme.chart_bullish;
        let bearish = theme.chart_bearish;
        let amber = theme.chart_5;

        // Connection health is just the live market-data stream now. Four+
        // failed reconnect attempts in a row crosses into "Disconnected"
        // — the user should know the feed is gone.
        let md = cx.global::<MarketDataServiceHandle>().0.clone();
        let (md_status, rtt_ms) = {
            let svc = md.read(cx);
            (svc.overall_status(), svc.rtt_ms())
        };

        let (dot_color, status_label): (gpui::Hsla, SharedString) = match &md_status {
            LiveStatus::Reconnecting { attempts } if *attempts >= 4 => {
                (bearish, "Disconnected".into())
            }
            LiveStatus::Connecting => (amber, "Connecting…".into()),
            LiveStatus::Reconnecting { .. } => (amber, "Reconnecting…".into()),
            LiveStatus::Connected => (bullish, "Connected".into()),
        };

        // RTT is a connected-only concept: show it only while the feed is up.
        // Before the first pong lands we show a muted "— ms" placeholder so the
        // pill width doesn't jump when the number arrives. Thresholds tuned for
        // a cross-region hop to the EU VPS: green < 100ms, amber ≤ 250ms, red
        // beyond. The leading "·" stays muted to match the "· market data" tail.
        let rtt_chip = matches!(md_status, LiveStatus::Connected).then(|| {
            let (value, color) = match rtt_ms {
                Some(ms) => {
                    let color = if ms < 100 {
                        bullish
                    } else if ms <= 250 {
                        amber
                    } else {
                        bearish
                    };
                    (SharedString::from(format!("{ms} ms")), color)
                }
                None => (SharedString::from("— ms"), muted),
            };
            h_flex()
                .gap_1()
                .items_center()
                .child(div().text_xs().text_color(muted).child("·"))
                .child(div().text_xs().font_semibold().text_color(color).child(value))
        });

        let connection = h_flex()
            .gap_1p5()
            .items_center()
            .child(div().size_2().rounded_full().bg(dot_color))
            .child(
                div()
                    .text_xs()
                    .text_color(theme.foreground)
                    .child(status_label),
            )
            .children(rtt_chip)
            .child(div().text_xs().text_color(muted).child("· market data"));

        // Display clock honours the user's timezone choice; Auto falls back
        // to OS local (matches pre-feature behaviour bit-for-bit).
        let user_now = prefs::now_in_user_tz(cx);
        let time_str = SharedString::from(user_now.format("%H:%M:%S").to_string());
        let date_str = SharedString::from(user_now.format("%a, %b %d %Y").to_string());
        let tz_str = SharedString::from(format!("UTC{}", user_now.format("%:z")));

        let clock = h_flex()
            .gap_2()
            .items_baseline()
            .child(div().text_xs().font_semibold().child(time_str))
            .child(div().text_xs().text_color(muted).child(date_str))
            .child(div().text_xs().text_color(muted).child(tz_str));

        let fps_color = if self.fps >= 50.0 {
            bullish
        } else if self.fps >= 25.0 {
            theme.chart_5
        } else {
            theme.chart_bearish
        };
        let fps_str = SharedString::from(format!("{:>4.0} fps", self.fps));

        let fps = h_flex()
            .gap_1p5()
            .items_center()
            .child(div().text_xs().text_color(muted).child("FPS"))
            .child(
                div()
                    .text_xs()
                    .font_semibold()
                    .text_color(fps_color)
                    .child(fps_str),
            );

        // Tag debug builds in the version chip so a stray un-bundled WASM
        // never gets confused with the deployed release. `debug_assertions`
        // is on for `cargo build` and off for `cargo build --release`, so
        // the suffix flips at the same boundary as the build profile.
        //
        // In release builds, append the short commit SHA when `build.rs`
        // populated `BUILD_SHA` (GHA path). Locally-built release WASM
        // (no SHA) shows plain `v<version>`.
        let version_str = if cfg!(debug_assertions) {
            format!("v{VERSION} (debug)")
        } else if BUILD_SHA.is_empty() {
            format!("v{VERSION}")
        } else {
            let short: String = BUILD_SHA.chars().take(7).collect();
            format!("v{VERSION}-{short}")
        };
        let version = div()
            .text_xs()
            .text_color(muted)
            .child(SharedString::from(version_str));

        let separator = || div().w(px(1.)).h(px(14.)).bg(theme.border);

        h_flex()
            .h(px(24.))
            .w_full()
            .px_3()
            .gap_3()
            .items_center()
            .border_t_1()
            .border_color(theme.border)
            .bg(theme.tab_bar)
            .child(connection)
            .child(div().flex_1())
            .child(clock)
            .child(separator())
            .child(fps)
            .child(separator())
            .child(version)
    }
}
