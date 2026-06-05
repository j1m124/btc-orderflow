//! Orderbook ladder panel. Vertical price ladder:
//!
//! ```text
//!   ASKS  (highest price at top, best ask at bottom)
//!   ──── last-trade strip ────
//!   BIDS  (best bid at top, lowest at bottom)
//! ```
//!
//! Per-row layout: `[price | bar area (cum-depth bar behind, per-row qty
//! bar on top) | qty number | cumulative-qty number]`. Bars are
//! right-anchored: the per-row bar scales to the per-side visible-max qty;
//! the faint cumulative bar behind it scales to the per-side total
//! cumulative depth.
//!
//! Header carries a bucket-width selector (tick / $1 / $5 / $10 / $25),
//! a size-mode dropdown (COIN / USD) and a Center button. Center is a
//! sticky toggle — engaged at mount, on bucket / symbol change, and by
//! manual click. While engaged, every render snaps the spread row to the
//! viewport middle; it disengages when the user scrolls.
//!
//! Ladder is rendered through gpui-component's `v_virtual_list` so tick
//! bucket (up to ~400 raw levels) only pays the layout cost of items
//! actually on screen. Item heights are fixed and known up front, so the
//! sticky-center math reduces to a single absolute offset computed from a
//! prefix-sum over `item_sizes` — no `bounds_for_item` round-trip, no
//! delta-vs-absolute jiggle.
//!
//! The center row no longer shows spread / mid — it shows the last-trade
//! price colored by aggression side (▲ buy / ▼ sell). The OB panel
//! subscribes to the Trades channel for the same symbol; the service
//! refcounts on `SubKey` so having the Trades panel open at the same time
//! costs one WS sub total.

use std::collections::BTreeMap;
use std::rc::Rc;

use gpui::{
    Action, Context, FocusHandle, Hsla, IntoElement, ParentElement as _, Pixels, SharedString,
    Size, Styled as _, Window, div, point, px, relative, size,
};
use gpui_component::{
    ActiveTheme as _, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    menu::DropdownMenu as _,
    v_flex,
    v_virtual_list,
};
use serde::{Deserialize, Serialize};

use super::{ContentPanel, OrderbookState};
use crate::services::market_data::{BookLevel, MarketDataServiceHandle, Trade};

/// Switch the orderbook panel's Size / Sum columns between coin qty and
/// USD notional (`qty * row_price`). Carries the mode id ("coin" / "usd");
/// the handler on `ContentPanel` parses it back to a [`OrderbookSizeMode`].
/// Dispatched from the panel header's size-mode dropdown, scoped to the
/// panel's focus so multiple Orderbook panels stay independent.
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = client, no_json)]
pub struct ChangeOrderbookSizeMode(pub SharedString);

/// Whether the Size / Sum columns show coin quantity or USD notional.
/// `Usd` multiplies each row's qty by the row's price.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum OrderbookSizeMode {
    Coin,
    Usd,
}

impl Default for OrderbookSizeMode {
    fn default() -> Self {
        OrderbookSizeMode::Coin
    }
}

impl OrderbookSizeMode {
    pub const ALL: &'static [OrderbookSizeMode] =
        &[OrderbookSizeMode::Coin, OrderbookSizeMode::Usd];

    pub fn id(self) -> &'static str {
        match self {
            OrderbookSizeMode::Coin => "coin",
            OrderbookSizeMode::Usd => "usd",
        }
    }

    pub fn from_id(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|m| m.id() == s)
    }

    pub fn label(self) -> &'static str {
        match self {
            OrderbookSizeMode::Coin => "COIN",
            OrderbookSizeMode::Usd => "USD",
        }
    }
}

/// Top-N levels per side the server forwards on the WS Book channel.
/// 200 is enough to populate roughly a $20-wide ladder at BTC's tight book
/// without flooding the wire when book churn is heavy.
pub const WS_DEPTH: u16 = 200;

/// Fixed height of each price-level row (content + 1px bottom border).
const ROW_H: Pixels = px(15.);
/// Fixed height of the last-trade strip in the middle of the ladder.
const LAST_STRIP_H: Pixels = px(22.);

/// Bucket-width choice for the ladder. `Tick` means "render every distinct
/// price level" (no aggregation); the dollar variants bin raw levels by
/// `(price / w).floor() * w`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum OrderbookBucket {
    Tick,
    Dollar1,
    Dollar5,
    Dollar10,
    Dollar25,
}

impl Default for OrderbookBucket {
    fn default() -> Self {
        OrderbookBucket::Dollar1
    }
}

impl OrderbookBucket {
    pub const ALL: &'static [OrderbookBucket] = &[
        OrderbookBucket::Tick,
        OrderbookBucket::Dollar1,
        OrderbookBucket::Dollar5,
        OrderbookBucket::Dollar10,
        OrderbookBucket::Dollar25,
    ];

    /// Persistence id ("tick", "1", "5", "10", "25"). Stable across builds —
    /// changing these breaks persisted prefs.
    pub fn id(self) -> &'static str {
        match self {
            OrderbookBucket::Tick => "tick",
            OrderbookBucket::Dollar1 => "1",
            OrderbookBucket::Dollar5 => "5",
            OrderbookBucket::Dollar10 => "10",
            OrderbookBucket::Dollar25 => "25",
        }
    }

    pub fn from_id(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|b| b.id() == s)
    }

    pub fn label(self) -> &'static str {
        match self {
            OrderbookBucket::Tick => "tick",
            OrderbookBucket::Dollar1 => "$1",
            OrderbookBucket::Dollar5 => "$5",
            OrderbookBucket::Dollar10 => "$10",
            OrderbookBucket::Dollar25 => "$25",
        }
    }

    /// Bucket width as a numeric value, or `None` in tick mode (caller emits
    /// raw levels directly).
    pub fn width(self) -> Option<f64> {
        match self {
            OrderbookBucket::Tick => None,
            OrderbookBucket::Dollar1 => Some(1.0),
            OrderbookBucket::Dollar5 => Some(5.0),
            OrderbookBucket::Dollar10 => Some(10.0),
            OrderbookBucket::Dollar25 => Some(25.0),
        }
    }
}

/// One displayed row. `key` is the bucket's `(price / w).floor() * w` (or
/// the raw price in tick mode); `qty` is the aggregated size at that level;
/// `cum` is the running cumulative from best→deep on this side.
struct DisplayRow {
    key: f64,
    qty: f64,
    cum: f64,
}

fn bucketize(raw: &[BookLevel], bucket: OrderbookBucket) -> Vec<DisplayRow> {
    match bucket.width() {
        None => raw
            .iter()
            .map(|l| DisplayRow {
                key: l.price,
                qty: l.size,
                cum: 0.0,
            })
            .collect(),
        Some(w) => {
            let mut acc: BTreeMap<i64, f64> = BTreeMap::new();
            for l in raw {
                let idx = (l.price / w).floor() as i64;
                *acc.entry(idx).or_insert(0.0) += l.size;
            }
            acc.into_iter()
                .map(|(idx, qty)| DisplayRow {
                    key: idx as f64 * w,
                    qty,
                    cum: 0.0,
                })
                .collect()
        }
    }
}

/// One virtualized item — either a ladder row (with side colour params
/// baked in) or the centre last-trade strip. Owned by an `Rc<Vec<…>>` and
/// indexed by the virtual_list render closure.
enum RowItem {
    Row {
        key: f64,
        qty: f64,
        cum: f64,
        max_qty: f64,
        max_cum: f64,
        tint: Hsla,
    },
    LastStrip {
        text: SharedString,
        color: Hsla,
        bg: Hsla,
    },
}

pub fn render(
    state: &mut OrderbookState,
    focus: FocusHandle,
    _window: &mut Window,
    cx: &mut Context<ContentPanel>,
) -> impl IntoElement {
    let theme = cx.theme();
    let bullish = theme.chart_bullish;
    let bearish = theme.chart_bearish;
    let border = theme.border;
    let fg = theme.foreground;

    let symbol = state.symbol.clone();
    let bucket = state.bucket;
    let size_mode = state.size_mode;
    let sticky_center = state.sticky_center;
    let scroll = state.scroll.clone();

    let service = cx.global::<MarketDataServiceHandle>().0.clone();
    let (raw_bids, raw_asks): (Vec<BookLevel>, Vec<BookLevel>) = service
        .read(cx)
        .book_snapshot(symbol.as_ref(), WS_DEPTH)
        .map(|(b, a)| (b.to_vec(), a.to_vec()))
        .unwrap_or_default();
    let last_trade: Option<Trade> = service
        .read(cx)
        .trades_snapshot(symbol.as_ref())
        .and_then(|s| s.last().cloned());

    let mut bid_rows = bucketize(&raw_bids, bucket);
    bid_rows.sort_by(|a, b| b.key.partial_cmp(&a.key).unwrap_or(std::cmp::Ordering::Equal));
    let mut ask_rows = bucketize(&raw_asks, bucket);
    ask_rows.sort_by(|a, b| b.key.partial_cmp(&a.key).unwrap_or(std::cmp::Ordering::Equal));

    // Cumulative depth grows best → deep on each side.
    {
        let mut acc = 0.0;
        for r in bid_rows.iter_mut() {
            acc += r.qty;
            r.cum = acc;
        }
    }
    {
        let mut acc = 0.0;
        for r in ask_rows.iter_mut().rev() {
            acc += r.qty;
            r.cum = acc;
        }
    }

    let max_bid_qty = bid_rows.iter().map(|r| r.qty).fold(0.0_f64, f64::max);
    let max_ask_qty = ask_rows.iter().map(|r| r.qty).fold(0.0_f64, f64::max);
    let max_bid_cum = bid_rows.last().map(|r| r.cum).unwrap_or(0.0);
    let max_ask_cum = ask_rows.first().map(|r| r.cum).unwrap_or(0.0);

    let spread_idx = ask_rows.len();
    let (last_text, last_color) = match &last_trade {
        Some(t) => {
            let is_buy = !t.is_buyer_maker;
            let glyph = if is_buy { "\u{25b2}" } else { "\u{25bc}" };
            let color = if is_buy { bullish } else { bearish };
            (SharedString::from(format!("{} {:.2}", glyph, t.price)), color)
        }
        None => (SharedString::from("\u{2014}"), fg),
    };
    let last_bg = Hsla { a: 0.12, ..last_color };

    // Flatten the ladder into a single Vec<RowItem> in DOM order
    // (asks top→bottom, last-strip middle, bids top→bottom). Indices into
    // this vec are also indices into the parallel `item_sizes` Vec.
    let mut items: Vec<RowItem> = Vec::with_capacity(ask_rows.len() + 1 + bid_rows.len());
    for r in &ask_rows {
        items.push(RowItem::Row {
            key: r.key,
            qty: r.qty,
            cum: r.cum,
            max_qty: max_ask_qty,
            max_cum: max_ask_cum,
            tint: bearish,
        });
    }
    items.push(RowItem::LastStrip {
        text: last_text,
        color: last_color,
        bg: last_bg,
    });
    for r in &bid_rows {
        items.push(RowItem::Row {
            key: r.key,
            qty: r.qty,
            cum: r.cum,
            max_qty: max_bid_qty,
            max_cum: max_bid_cum,
            tint: bullish,
        });
    }

    // Parallel item-sizes vec: the last-strip is its own taller item, all
    // others are uniform `ROW_H`. v_virtual_list ignores width.
    let item_sizes: Vec<Size<Pixels>> = items
        .iter()
        .map(|it| match it {
            RowItem::LastStrip { .. } => size(px(0.), LAST_STRIP_H),
            RowItem::Row { .. } => size(px(0.), ROW_H),
        })
        .collect();
    let item_sizes_rc = Rc::new(item_sizes.clone());
    let items_rc = Rc::new(items);

    let center_btn = {
        let btn = Button::new("ob-center")
            .label(SharedString::from("Center"))
            .small()
            .on_click(cx.listener(move |this, _, _, cx| {
                this.request_orderbook_recenter(cx);
            }));
        if sticky_center { btn.primary() } else { btn.ghost() }
    };
    let header = h_flex()
        .px_2()
        .py_1()
        .gap_2()
        .items_center()
        .child(
            div()
                .text_size(px(11.))
                .text_color(fg)
                .child(SharedString::from(format!("Orderbook \u{2014} {}", symbol))),
        )
        .child(div().flex_1())
        .child(size_mode_dropdown(size_mode, focus.clone()))
        .child(bucket_selector(bucket, cx))
        .child(center_btn);

    let (size_label, sum_label) = match size_mode {
        OrderbookSizeMode::Coin => ("Size", "Sum"),
        OrderbookSizeMode::Usd => ("Size ($)", "Sum ($)"),
    };
    let column_header = h_flex()
        .px_2()
        .py(px(1.))
        .gap_1()
        .text_size(px(10.))
        .text_color(fg)
        .border_b_1()
        .border_color(border)
        .child(div().w(px(80.)).text_right().child("Price"))
        .child(div().flex_1().min_w_0().child(""))
        .child(div().w(px(70.)).text_right().child(size_label))
        .child(div().w(px(80.)).text_right().child(sum_label));

    let render_items = items_rc.clone();
    let ladder = v_virtual_list(
        cx.entity(),
        SharedString::from("ob-virtual"),
        item_sizes_rc,
        move |_panel, range, _window, _cx| {
            range
                .map(|i| render_item(&render_items[i], fg, border, size_mode))
                .collect::<Vec<_>>()
        },
    )
    .track_scroll(&scroll)
    .flex_1()
    .min_h_0();

    // Sticky centering. With known item heights we can compute the centered
    // absolute offset directly: `natural_top(spread_idx)` is the prefix sum
    // of preceding item heights, and the target offset that puts the strip
    // at viewport-mid is `(viewport.h - strip_h)/2 - natural_top`.
    //
    // User-scroll detection: compare `scroll.offset().y` against the value
    // we wrote last paint — a mismatch means the wheel / drag moved the
    // offset out from under us, so we release sticky.
    //
    // On the very first paint the scroll handle's bounds haven't been
    // populated yet (zero height) — leave sticky on; the next paint after
    // layout will apply the offset.
    if state.sticky_center {
        let cur = scroll.offset();
        let user_scrolled = state
            .last_set_offset_y
            .map_or(false, |last| (cur.y - last).abs() > px(0.5));
        if user_scrolled {
            state.sticky_center = false;
            state.last_set_offset_y = None;
        } else {
            let viewport_h = scroll.bounds().size.height;
            if viewport_h > px(0.) {
                let natural_top: Pixels = item_sizes
                    .iter()
                    .take(spread_idx)
                    .map(|s| s.height)
                    .fold(px(0.), |a, h| a + h);
                let desired_y = (viewport_h - LAST_STRIP_H) / 2.0 - natural_top;
                if (desired_y - cur.y).abs() > px(0.5) {
                    scroll.set_offset(point(cur.x, desired_y));
                }
                state.last_set_offset_y = Some(desired_y);
            }
        }
    }

    v_flex()
        .size_full()
        .child(header)
        .child(column_header)
        .child(ladder)
}

fn render_item(
    item: &RowItem,
    fg: Hsla,
    border: Hsla,
    size_mode: OrderbookSizeMode,
) -> gpui::AnyElement {
    match item {
        RowItem::Row {
            key,
            qty,
            cum,
            max_qty,
            max_cum,
            tint,
        } => render_row(*key, *qty, *cum, *max_qty, *max_cum, *tint, fg, border, size_mode),
        RowItem::LastStrip { text, color, bg } => h_flex()
            .w_full()
            .px_2()
            .h(LAST_STRIP_H)
            .items_center()
            .justify_center()
            .text_size(px(12.))
            .text_color(*color)
            .bg(*bg)
            .border_y_1()
            .border_color(border)
            .child(text.clone())
            .into_any_element(),
    }
}

fn render_row(
    key: f64,
    qty: f64,
    cum: f64,
    max_qty: f64,
    max_cum: f64,
    tint: Hsla,
    fg: Hsla,
    border: Hsla,
    size_mode: OrderbookSizeMode,
) -> gpui::AnyElement {
    let qty_frac = if max_qty > 0.0 {
        ((qty / max_qty) as f32).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let cum_frac = if max_cum > 0.0 {
        ((cum / max_cum) as f32).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let bar_color = tint;
    let cum_color = Hsla { a: 0.22, ..tint };

    let price_text = SharedString::from(format!("{:.2}", key));
    let (qty_text, cum_text) = match size_mode {
        OrderbookSizeMode::Coin => (
            SharedString::from(format_qty(qty)),
            SharedString::from(format_qty(cum)),
        ),
        OrderbookSizeMode::Usd => (
            SharedString::from(format_usd(qty * key)),
            SharedString::from(format_usd(cum * key)),
        ),
    };

    h_flex()
        .w_full()
        .px_2()
        .py(px(0.))
        .gap_1()
        .items_stretch()
        .h(ROW_H)
        .text_size(px(11.))
        .border_b_1()
        .border_color(border)
        .child(
            div()
                .w(px(80.))
                .text_right()
                .text_color(fg)
                .flex()
                .items_center()
                .justify_end()
                .child(price_text),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .h_full()
                .relative()
                .child(
                    div()
                        .absolute()
                        .top_0()
                        .right_0()
                        .w(relative(cum_frac))
                        .h_full()
                        .bg(cum_color),
                )
                .child(
                    div()
                        .absolute()
                        .top_0()
                        .right_0()
                        .w(relative(qty_frac))
                        .h_full()
                        .bg(bar_color),
                ),
        )
        .child(
            div()
                .w(px(70.))
                .text_right()
                .text_color(fg)
                .flex()
                .items_center()
                .justify_end()
                .child(qty_text),
        )
        .child(
            div()
                .w(px(80.))
                .text_right()
                .text_color(fg)
                .flex()
                .items_center()
                .justify_end()
                .child(cum_text),
        )
        .into_any_element()
}

fn size_mode_dropdown(
    current: OrderbookSizeMode,
    focus: FocusHandle,
) -> impl IntoElement {
    Button::new("ob-size-mode")
        .label(SharedString::from(current.label()))
        .xsmall()
        .ghost()
        .dropdown_menu(move |menu, _, _| {
            let mut menu = menu.action_context(focus.clone());
            for m in OrderbookSizeMode::ALL {
                menu = menu.menu(
                    SharedString::from(m.label()),
                    Box::new(ChangeOrderbookSizeMode(SharedString::from(m.id()))),
                );
            }
            menu
        })
}

fn bucket_selector(
    current: OrderbookBucket,
    cx: &mut Context<ContentPanel>,
) -> impl IntoElement {
    let mut row = h_flex().gap_1();
    for b in OrderbookBucket::ALL {
        let bucket = *b;
        let is_active = bucket == current;
        let btn_id = SharedString::from(format!("ob-bucket-{}", bucket.id()));
        let mut btn = Button::new(btn_id).label(SharedString::from(bucket.label())).xsmall();
        btn = if is_active { btn.primary() } else { btn.ghost() };
        btn = btn.on_click(cx.listener(move |this, _, _, cx| {
            this.set_orderbook_bucket(bucket, cx);
        }));
        row = row.child(btn);
    }
    row
}

/// Adaptive USD notation matching the trades panel — compact `M`/`k`
/// suffixes so a long ladder stays scannable.
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

/// Adaptive precision matching the trades panel: large prints round to whole;
/// medium use 2 dp; sub-1 use 3 dp.
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
