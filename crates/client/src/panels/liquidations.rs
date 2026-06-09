//! Liquidations tape panel. Newest-on-top, side-tinted rows (long-liq red,
//! short-liq green). Subscribes to `MarketDataService::ensure_liquidations`
//! and renders the panel's `liquidations_persist` buffer with three header
//! controls: size-mode dropdown (COIN / USD), side-filter dropdown
//! (ALL / LONG / SHORT), and a free-form min-size threshold input.
//!
//! Side semantic: `Long` = a long position was liquidated (forced sell,
//! bearish). `Short` = a short position was liquidated (forced buy,
//! bullish). The flip from Binance's raw forced-order side happens server-
//! side at ingest — see crates/server/src/binance/parse.rs.

use chrono::{DateTime, Local};
use gpui::{
    Action, Context, Entity, FocusHandle, InteractiveElement as _, IntoElement,
    ParentElement as _, SharedString, Styled as _, Window, div, px,
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
use crate::services::market_data::{Liquidation, LiquidationSide};

/// Switch the panel's Size column between coin qty and USD notional.
/// Carries the mode id ("coin" / "usd").
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = client, no_json)]
pub struct ChangeLiquidationsSizeMode(pub SharedString);

/// Filter the panel by liquidated-position side. Carries the side id
/// ("all" / "long" / "short").
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = client, no_json)]
pub struct ChangeLiquidationsSideFilter(pub SharedString);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum LiquidationsSizeMode {
    Coin,
    Usd,
}

impl Default for LiquidationsSizeMode {
    fn default() -> Self {
        LiquidationsSizeMode::Usd
    }
}

impl LiquidationsSizeMode {
    pub const ALL: &'static [LiquidationsSizeMode] =
        &[LiquidationsSizeMode::Coin, LiquidationsSizeMode::Usd];

    pub fn id(self) -> &'static str {
        match self {
            LiquidationsSizeMode::Coin => "coin",
            LiquidationsSizeMode::Usd => "usd",
        }
    }

    pub fn from_id(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|m| m.id() == s)
    }

    pub fn label(self) -> &'static str {
        match self {
            LiquidationsSizeMode::Coin => "COIN",
            LiquidationsSizeMode::Usd => "USD",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum LiquidationsSideFilter {
    All,
    Long,
    Short,
}

impl Default for LiquidationsSideFilter {
    fn default() -> Self {
        LiquidationsSideFilter::All
    }
}

impl LiquidationsSideFilter {
    pub const ALL: &'static [LiquidationsSideFilter] = &[
        LiquidationsSideFilter::All,
        LiquidationsSideFilter::Long,
        LiquidationsSideFilter::Short,
    ];

    pub fn id(self) -> &'static str {
        match self {
            LiquidationsSideFilter::All => "all",
            LiquidationsSideFilter::Long => "long",
            LiquidationsSideFilter::Short => "short",
        }
    }

    pub fn from_id(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|m| m.id() == s)
    }

    pub fn label(self) -> &'static str {
        match self {
            LiquidationsSideFilter::All => "ALL",
            LiquidationsSideFilter::Long => "LONG",
            LiquidationsSideFilter::Short => "SHORT",
        }
    }

    pub fn matches(self, side: LiquidationSide) -> bool {
        match (self, side) {
            (LiquidationsSideFilter::All, _) => true,
            (LiquidationsSideFilter::Long, LiquidationSide::Long) => true,
            (LiquidationsSideFilter::Short, LiquidationSide::Short) => true,
            _ => false,
        }
    }
}

/// Parse the min-size input. Empty / non-parseable / <=0 → no filter.
/// Strips `$`, commas, and underscores so "$1,000_000" parses as 1e6.
/// Unit interpretation depends on the current `LiquidationsSizeMode`.
pub fn parse_min_size(text: &str) -> Option<f64> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    let cleaned: String = trimmed
        .trim_start_matches('$')
        .chars()
        .filter(|c| *c != ',' && *c != '_')
        .collect();
    cleaned.parse::<f64>().ok().filter(|v| *v > 0.0)
}

/// True if `l` clears the min-size floor in the active unit. The unit is the
/// panel's current `LiquidationsSizeMode` — switching modes re-interprets
/// the same threshold value against the new unit.
pub fn passes_min_size(
    l: &Liquidation,
    min: Option<f64>,
    size_mode: LiquidationsSizeMode,
) -> bool {
    match min {
        None => true,
        Some(floor) => match size_mode {
            LiquidationsSizeMode::Coin => l.qty >= floor,
            LiquidationsSizeMode::Usd => l.quote_qty >= floor,
        },
    }
}

pub fn render(
    _symbol: SharedString,
    liquidations: &[Liquidation],
    size_mode: LiquidationsSizeMode,
    side_filter: LiquidationsSideFilter,
    min_size: Option<f64>,
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
        LiquidationsSizeMode::Coin => "Size",
        LiquidationsSizeMode::Usd => "Size ($)",
    };

    let header = h_flex()
        .px_2()
        .py_1()
        .gap_2()
        .items_center()
        .child(div().flex_1())
        .child(side_filter_dropdown(side_filter, focus.clone()))
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
                .w(px(54.))
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .child("Side"),
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

    // Newest-first; cap rendered rows. 100 covers any reasonable panel
    // height (~24 px/row → 2400 px), and matches the trades panel cap.
    const RENDER_CAP: usize = 100;
    let rows = liquidations
        .iter()
        .rev()
        .filter(|l| side_filter.matches(l.side))
        .filter(|l| passes_min_size(l, min_size, size_mode))
        .take(RENDER_CAP)
        .map(move |l| {
            let tint_color = match l.side {
                LiquidationSide::Long => bearish,
                LiquidationSide::Short => bullish,
            };
            // Liquidations are punchy events — start the row tint at a
            // higher floor than the trade tape (0.10 vs 0.04) and scale up
            // by USD notional the same way. Long/short colors stay distinct
            // even at the floor so the side is glanceable.
            let usd = l.quote_qty.max(100.0);
            let lg = (usd.log10() - 2.0).clamp(0.0, 4.0) as f32;
            let row_alpha = 0.10 + (lg / 4.0) * 0.55;
            let row_bg = gpui::Hsla {
                a: row_alpha,
                ..tint_color
            };
            let time_str = format_time(l.ts_ms);
            let side_label = match l.side {
                LiquidationSide::Long => "LONG",
                LiquidationSide::Short => "SHORT",
            };
            let price_str = format!("{:.1}", l.price);
            let size_str = match size_mode {
                LiquidationsSizeMode::Coin => format_qty(l.qty),
                LiquidationsSizeMode::Usd => format_usd(l.quote_qty),
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
                        .w(px(54.))
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .text_color(tint_color)
                        .child(side_label),
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
                .id(SharedString::from("liquidations-list"))
                .flex_1()
                .min_h_0()
                .overflow_hidden()
                .child(v_flex().w_full().children(rows)),
        )
}

fn size_mode_dropdown(
    current: LiquidationsSizeMode,
    focus: FocusHandle,
) -> impl IntoElement {
    Button::new("liquidations-size-mode")
        .label(SharedString::from(current.label()))
        .xsmall()
        .ghost()
        .dropdown_menu(move |menu, _, _| {
            let mut menu = menu.action_context(focus.clone());
            for m in LiquidationsSizeMode::ALL {
                menu = menu.menu(
                    SharedString::from(m.label()),
                    Box::new(ChangeLiquidationsSizeMode(SharedString::from(m.id()))),
                );
            }
            menu
        })
}

fn side_filter_dropdown(
    current: LiquidationsSideFilter,
    focus: FocusHandle,
) -> impl IntoElement {
    Button::new("liquidations-side-filter")
        .label(SharedString::from(current.label()))
        .xsmall()
        .ghost()
        .dropdown_menu(move |menu, _, _| {
            let mut menu = menu.action_context(focus.clone());
            for m in LiquidationsSideFilter::ALL {
                menu = menu.menu(
                    SharedString::from(m.label()),
                    Box::new(ChangeLiquidationsSideFilter(SharedString::from(m.id()))),
                );
            }
            menu
        })
}

fn format_time(ts_ms: i64) -> String {
    DateTime::<Local>::from(std::time::UNIX_EPOCH + std::time::Duration::from_millis(ts_ms as u64))
        .format("%H:%M:%S")
        .to_string()
}

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
