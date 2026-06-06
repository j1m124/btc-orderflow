//! Trades-tape panel. Renders the most-recent prints for the panel's
//! symbol, newest-on-top, with side-tinted rows. Buy aggression
//! (`is_buyer_maker == false`) → bullish tint; sell aggression → bearish.
//!
//! Non-scrolling: visible row count is implicitly bounded by the panel's
//! height. The panel keeps its own filter-aware persist buffer (built up
//! by `ContentPanel` from `TradeEvent::Tick`), so a high min-USD threshold
//! holds the rare big prints around indefinitely instead of letting them
//! roll off the underlying service ring. Threshold is a free-form numeric
//! input in the header; empty / `0` → no filter.

use chrono::{DateTime, Local};
use gpui::{
    Action, Context, Entity, FocusHandle, InteractiveElement as _, IntoElement, ParentElement as _,
    SharedString, Styled as _, Window, div, px,
};
use gpui_component::{
    ActiveTheme as _, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputState},
    menu::DropdownMenu as _,
    v_flex,
};
use serde::{Deserialize, Serialize};

use super::ContentPanel;
use crate::services::market_data::Trade;

/// Switch the trades panel's Size column between coin qty and USD
/// notional. Carries the mode id ("coin" / "usd"); the handler on
/// `ContentPanel` parses it back to a [`TradesSizeMode`]. Dispatched from
/// the panel header's size-mode dropdown, scoped to the panel's focus so
/// it routes through *this* panel's `ContentPanel` (not whichever element
/// had focus when the menu was opened), keeping multiple Trades panels
/// independent.
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = client, no_json)]
pub struct ChangeTradesSizeMode(pub SharedString);

/// Whether the Size column shows the coin quantity or the USD notional.
/// Per-panel state — toggled via the header dropdown, persisted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TradesSizeMode {
    Coin,
    Usd,
}

impl Default for TradesSizeMode {
    fn default() -> Self {
        TradesSizeMode::Coin
    }
}

impl TradesSizeMode {
    pub const ALL: &'static [TradesSizeMode] = &[TradesSizeMode::Coin, TradesSizeMode::Usd];

    pub fn id(self) -> &'static str {
        match self {
            TradesSizeMode::Coin => "coin",
            TradesSizeMode::Usd => "usd",
        }
    }

    pub fn from_id(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|m| m.id() == s)
    }

    pub fn label(self) -> &'static str {
        match self {
            TradesSizeMode::Coin => "COIN",
            TradesSizeMode::Usd => "USD",
        }
    }
}

/// Parse the filter input value into a threshold. Empty string, whitespace,
/// or values <= 0 → `None` (no filter). Non-parseable strings also → `None`
/// so the user gets "show all" while mid-edit rather than a stale floor.
pub fn parse_min_usd(text: &str) -> Option<f64> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    // Strip a leading $, commas, and underscores so "$100,000" / "100_000"
    // parse the same as "100000".
    let cleaned: String = trimmed
        .trim_start_matches('$')
        .chars()
        .filter(|c| *c != ',' && *c != '_')
        .collect();
    cleaned.parse::<f64>().ok().filter(|v| *v > 0.0)
}

pub fn passes_min_usd(t: &Trade, min_usd: Option<f64>) -> bool {
    match min_usd {
        None => true,
        Some(floor) => t.price * t.qty >= floor,
    }
}

pub fn render(
    _symbol: SharedString,
    trades: &[Trade],
    size_mode: TradesSizeMode,
    filter_input: &Entity<InputState>,
    focus: FocusHandle,
    _window: &mut Window,
    cx: &mut Context<ContentPanel>,
) -> impl IntoElement {
    let theme = cx.theme();
    let bullish = theme.chart_bullish;
    let bearish = theme.chart_bearish;
    let border = theme.border;
    let fg = theme.foreground;

    let size_header_label = match size_mode {
        TradesSizeMode::Coin => "Size",
        TradesSizeMode::Usd => "Size ($)",
    };
    // Header carries only the controls (size mode + min-USD filter) — the
    // panel identifier sits in the dock tab name via `Panel::tab_name`, so a
    // body header label would just duplicate it.
    let header = h_flex()
        .px_2()
        .py_1()
        .gap_2()
        .items_center()
        .child(div().flex_1())
        .child(size_mode_dropdown(size_mode, focus.clone()))
        .child(
            div()
                .w(px(110.))
                .child(Input::new(filter_input).xsmall().cleanable(true)),
        );

    let column_header = h_flex()
        .px_2()
        .py_1()
        .gap_2()
        .text_size(px(11.))
        .text_color(fg)
        .border_b_1()
        .border_color(border)
        .child(
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .child("Time"),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .text_right()
                .child("Price"),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .text_right()
                .child(size_header_label),
        );

    // Walk persist newest-first and cap at RENDER_CAP. The persist buffer
    // grows up to 5000 entries (so threshold-rebuild has depth), but only
    // the first ~200 newest are ever on-screen; building DOM for the rest
    // burns frame budget for no visible payoff. ~200 rows × ~24 px > 4500 px
    // tall — well above any realistic panel height.
    const RENDER_CAP: usize = 100;
    let rows = trades.iter().rev().take(RENDER_CAP).map(move |t| {
        let is_buy = !t.is_buyer_maker;
        let tint_color = if is_buy { bullish } else { bearish };
        let usd = t.price * t.qty;
        let lg = (usd.max(100.0).log10() - 2.0).clamp(0.0, 4.0) as f32;
        let row_alpha = 0.04 + (lg / 4.0) * 0.51;
        let row_bg = gpui::Hsla {
            a: row_alpha,
            ..tint_color
        };
        let time_str = format_time(t.ts_ms);
        let price_str = format!("{:.1}", t.price);
        let size_str = match size_mode {
            TradesSizeMode::Coin => format_qty(t.qty),
            TradesSizeMode::Usd => format_usd(usd),
        };
        h_flex()
            .px_2()
            .py(px(1.))
            .gap_2()
            .items_center()
            .text_size(px(12.))
            .text_color(fg)
            .bg(row_bg)
            .border_b_1()
            .border_color(border)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .child(SharedString::from(time_str)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_right()
                    .child(SharedString::from(price_str)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_right()
                    .child(SharedString::from(size_str)),
            )
    });

    v_flex()
        .size_full()
        .child(header)
        .child(column_header)
        .child(
            div()
                .id(SharedString::from("trades-list"))
                .flex_1()
                .min_h_0()
                .overflow_hidden()
                .child(v_flex().w_full().children(rows)),
        )
}

fn size_mode_dropdown(current: TradesSizeMode, focus: FocusHandle) -> impl IntoElement {
    Button::new("trades-size-mode")
        .label(SharedString::from(current.label()))
        .xsmall()
        .ghost()
        .dropdown_menu(move |menu, _, _| {
            let mut menu = menu.action_context(focus.clone());
            for m in TradesSizeMode::ALL {
                menu = menu.menu(
                    SharedString::from(m.label()),
                    Box::new(ChangeTradesSizeMode(SharedString::from(m.id()))),
                );
            }
            menu
        })
}

fn format_time(ts_ms: i64) -> String {
    DateTime::<Local>::from(std::time::UNIX_EPOCH + std::time::Duration::from_millis(ts_ms as u64))
        .format("%H:%M:%S%.3f")
        .to_string()
}

/// Adaptive USD notation: large notionals use compact `M`/`k` suffixes so a
/// long tape stays scannable. `>=$1M` → 1-dp millions; `>=$100k` → integer
/// thousands; `>=$1k` → 1-dp thousands; below $1k → raw integer.
fn format_usd(usd: f64) -> String {
    let abs = usd.abs();
    if abs >= 1_000_000.0 {
        format!("{:.1}M", usd / 1_000_000.0)
    } else if abs >= 100_000.0 {
        format!("{:.0}k", usd / 1_000.0)
    } else if abs >= 1_000.0 {
        format!("{:.1}k", usd / 1_000.0)
    } else {
        format!("{:.0}", usd)
    }
}

/// Adaptive precision: large prints round to whole / 1-dp; sub-1 prints get
/// 3-dp to keep small BTC sizes legible without padding everything to 8 dp.
fn format_qty(q: f64) -> String {
    let abs = q.abs();
    if abs >= 100.0 {
        format!("{:.0}", q)
    } else if abs >= 1.0 {
        format!("{:.2}", q)
    } else {
        format!("{:.3}", q)
    }
}
