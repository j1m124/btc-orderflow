//! Per-ticker company-detail panel.
//!
//! Replaces the original hardcoded-AAPL mock. The symbol follows the
//! user's focused chart (driven by `ContentPanel.set_active` /
//! `mouse_down` writing into [`crate::services::details::DetailsService`]),
//! so clicking another chart immediately swaps Details to that ticker.
//!
//! Sections:
//! - Header: ticker · name · exchange · sector (overview)
//! - Quote block: last/change/open/prev close/day range/52w/volume — all
//!   derived client-side from the `(symbol, 1d, Regular)` market_data
//!   snapshot. Details keeps its own RAII `SubscriptionHandle` so the
//!   service holds the bars for the focused symbol even when no chart of
//!   that timeframe is open.
//! - Fundamentals: market cap / P/E / EPS / div yield / beta / industry
//!   from overview + the most-recent financials.ratios period.
//! - About: collapsible description.
//! - Dividends: most recent 4 ex-dates, "show all" expands the rest.
//! - Financials: curated headline rows × 3 most-recent periods.
//!   "Show full statement" (Phase D) opens a JSON-tree dialog.

use gpui::{
    App, Context, EventEmitter, FocusHandle, Focusable, InteractiveElement as _, IntoElement,
    MouseButton, ParentElement as _, Render, SharedString, StatefulInteractiveElement as _,
    Styled as _, Subscription, Window, div, px,
    prelude::FluentBuilder as _,
};
use gpui_component::{
    ActiveTheme as _, Sizable as _, StyledExt as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    dock::{Panel, PanelEvent, TabPanel},
    h_flex, v_flex,
};
use serde_json::Value as JsonValue;

use crate::panels::{Kind, LastFocusedTabPanel};
use crate::services::details::{
    DetailsEntry, DetailsEvent, DetailsServiceHandle, Dividend, FinancialPeriod, Overview,
    SectionState,
};
use crate::services::market_data::{
    Candle, MarketDataServiceHandle, Session, SubscriptionHandle, Timeframe,
};

/// How many dividend rows to render before "Show all".
const DIVIDEND_PREVIEW_COUNT: usize = 4;
/// How many most-recent financial periods to columnize.
const FINANCIALS_COLUMNS: usize = 3;

pub struct DetailsPanel {
    focus_handle: FocusHandle,
    parent_tab_panel: Option<gpui::WeakEntity<TabPanel>>,
    /// 1d (Regular session) subscription for the currently displayed
    /// symbol. Re-taken on every focused-symbol change; dropping it
    /// releases the service's refcount.
    quote_sub: Option<SubscriptionHandle>,
    /// Last symbol we took a quote subscription for. Lets us detect
    /// FocusedChanged transitions and rotate the handle.
    sub_symbol: Option<SharedString>,
    /// Expansion state for the About description.
    description_expanded: bool,
    /// Expansion state for the dividend list.
    dividends_expanded: bool,
    _details_subscription: Subscription,
}

impl DetailsPanel {
    pub fn new(_window: &mut Window, cx: &mut Context<Self>) -> Self {
        let service = cx.global::<DetailsServiceHandle>().0.clone();
        let _details_subscription =
            cx.subscribe(&service, |_this, _svc, _ev: &DetailsEvent, cx| {
                cx.notify();
            });
        Self {
            focus_handle: cx.focus_handle(),
            parent_tab_panel: None,
            quote_sub: None,
            sub_symbol: None,
            description_expanded: false,
            dividends_expanded: false,
            _details_subscription,
        }
    }

    fn focused_symbol(&self, cx: &App) -> Option<SharedString> {
        let svc = cx.global::<DetailsServiceHandle>().0.clone();
        svc.read(cx).focused_symbol().cloned()
    }

    /// Rotate `quote_sub` to match the focused symbol. Idempotent for the
    /// same symbol; drops the old handle (releasing the service refcount)
    /// before taking the new one.
    fn sync_quote_sub(
        &mut self,
        focused: &Option<SharedString>,
        cx: &mut Context<Self>,
    ) {
        if self.sub_symbol == *focused {
            return;
        }
        // Drop first so the service unsubscribe (if last refholder) fires
        // before we register interest on the new key.
        self.quote_sub = None;
        self.sub_symbol = focused.clone();
        if let Some(sym) = focused {
            let md = cx.global::<MarketDataServiceHandle>().0.clone();
            let handle =
                md.update(cx, |svc, cx| svc.ensure(sym.as_ref(), Timeframe::D1, Session::Regular, cx));
            self.quote_sub = Some(handle);
        }
    }

    fn mark_focused(&self, cx: &mut App) {
        let Some(tab_panel) = self.parent_tab_panel.clone() else {
            return;
        };
        let global = cx.global::<LastFocusedTabPanel>().0.clone();
        *global.borrow_mut() = Some(tab_panel);
    }

    fn is_focused(&self, cx: &App) -> bool {
        let Some(mine) = self.parent_tab_panel.as_ref() else {
            return false;
        };
        let global = cx.global::<LastFocusedTabPanel>().0.borrow();
        global
            .as_ref()
            .map(|w| w.entity_id() == mine.entity_id())
            .unwrap_or(false)
    }

    fn trigger_refresh(&mut self, cx: &mut Context<Self>) {
        let Some(symbol) = self.focused_symbol(cx) else {
            return;
        };
        let svc = cx.global::<DetailsServiceHandle>().0.clone();
        svc.update(cx, |s, cx| s.reload(symbol, cx));
    }
}

impl EventEmitter<PanelEvent> for DetailsPanel {}

impl Focusable for DetailsPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Panel for DetailsPanel {
    fn panel_name(&self) -> &'static str {
        Kind::Details.id()
    }

    fn title(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        SharedString::from(Kind::Details.display())
    }

    fn on_added_to(
        &mut self,
        tab_panel: gpui::WeakEntity<TabPanel>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        self.parent_tab_panel = Some(tab_panel);
    }

    fn set_active(&mut self, active: bool, _window: &mut Window, cx: &mut Context<Self>) {
        if active {
            self.mark_focused(cx);
        }
    }
}

impl Render for DetailsPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let focused = self.focused_symbol(cx);
        // Lifecycle hook — every render reconciles the quote sub with the
        // currently focused symbol. Cheap when nothing changed.
        self.sync_quote_sub(&focused, cx);

        let ring_color = if self.is_focused(cx) {
            cx.theme().ring
        } else {
            gpui::transparent_black()
        };

        let body: gpui::AnyElement = match &focused {
            None => render_empty_state(cx).into_any_element(),
            Some(symbol) => render_symbol(symbol.clone(), self, cx).into_any_element(),
        };

        div()
            .id("details-panel-body")
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _ev, _window, cx| this.mark_focused(cx)),
            )
            .size_full()
            .border_2()
            .border_color(ring_color)
            .child(
                div()
                    .id("details-scroll")
                    .size_full()
                    .overflow_y_scroll()
                    .child(body),
            )
    }
}

// ---------------------------------------------------------------------------
// Body renderers
// ---------------------------------------------------------------------------

fn render_empty_state(cx: &Context<DetailsPanel>) -> impl IntoElement {
    let theme = cx.theme();
    let muted = theme.muted_foreground;
    v_flex()
        .p_6()
        .gap_2()
        .child(
            div()
                .text_size(px(13.))
                .text_color(muted)
                .child("Open or focus a chart to view details."),
        )
        .child(
            div()
                .text_size(px(11.))
                .text_color(muted)
                .child("This panel follows the focused chart's ticker."),
        )
}

fn render_symbol(
    symbol: SharedString,
    panel: &DetailsPanel,
    cx: &mut Context<DetailsPanel>,
) -> impl IntoElement {
    let svc = cx.global::<DetailsServiceHandle>().0.clone();
    // Borrow scope: only the entry's fields are needed in the renderer;
    // collect them up-front so we can release the service borrow before
    // mutating panel/cx via listeners.
    let entry: DetailsEntry = svc
        .read(cx)
        .entry(symbol.as_ref())
        .cloned()
        .unwrap_or_default();

    let md = cx.global::<MarketDataServiceHandle>().0.clone();
    let daily_snapshot: Vec<Candle> = md
        .read(cx)
        .snapshot(symbol.as_ref(), Timeframe::D1, Session::Regular)
        .map(|cs| cs.to_vec())
        .unwrap_or_default();

    let header = render_header(&symbol, entry.overview.loaded());
    let quote = render_quote_block(&daily_snapshot, cx);
    let fundamentals = render_fundamentals(
        entry.overview.loaded(),
        entry.financials.loaded().and_then(|p| p.first()),
        cx,
    );
    let about = render_about(entry.overview.loaded(), panel.description_expanded, cx);
    let dividends = render_dividends(
        entry.dividends.loaded(),
        panel.dividends_expanded,
        &entry.dividends,
        cx,
    );
    let financials = render_financials(&symbol, entry.financials.loaded(), &entry.financials, cx);

    let refresh_button = Button::new("details-refresh")
        .label("↻")
        .small()
        .ghost()
        .tooltip("Refresh details")
        .on_click(cx.listener(|this, _, _, cx| this.trigger_refresh(cx)));

    v_flex()
        .w_full()
        .p_3()
        .gap_4()
        .child(
            h_flex()
                .items_start()
                .gap_2()
                .child(div().flex_1().min_w_0().child(header))
                .child(refresh_button),
        )
        .child(quote)
        .child(fundamentals)
        .child(about)
        .child(dividends)
        .child(financials)
}

fn render_header(symbol: &SharedString, overview: Option<&Overview>) -> impl IntoElement {
    let name = overview
        .map(|o| o.name.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("—");
    let exchange = overview
        .map(|o| o.exchange.as_str())
        .filter(|s| !s.is_empty());
    let sector = overview
        .map(|o| o.sector.as_str())
        .filter(|s| !s.is_empty());
    let mut tagline = name.to_string();
    if let Some(ex) = exchange {
        tagline.push_str(" · ");
        tagline.push_str(ex);
    }
    if let Some(s) = sector {
        tagline.push_str(" · ");
        tagline.push_str(s);
    }
    v_flex()
        .gap_1()
        .child(div().text_lg().font_semibold().child(symbol.clone()))
        .child(
            div()
                .text_size(px(12.))
                .text_color(gpui::transparent_black())
                .child(SharedString::from(tagline)),
        )
}

// ---------------------------------------------------------------------------
// Quote block (derived from 1d candles)
// ---------------------------------------------------------------------------

fn render_quote_block(
    daily: &[Candle],
    cx: &Context<DetailsPanel>,
) -> gpui::AnyElement {
    let theme = cx.theme();
    let muted = theme.muted_foreground;
    let fg = theme.foreground;
    let buy = gpui::rgb(0x16a34a);
    let sell = gpui::rgb(0xdc2626);

    if daily.is_empty() {
        return div()
            .text_size(px(12.))
            .text_color(muted)
            .child("Loading quote…")
            .into_any_element();
    }

    let last = daily.last().expect("non-empty above");
    let prev = if daily.len() >= 2 {
        Some(&daily[daily.len() - 2])
    } else {
        None
    };
    let change = prev.map(|p| last.close - p.close);
    let change_pct = change
        .zip(prev.map(|p| p.close))
        .filter(|(_, prev_close)| *prev_close != 0.0)
        .map(|(c, p)| c / p * 100.0);

    let (range_lo, range_hi) = daily
        .iter()
        .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), c| {
            (lo.min(c.low), hi.max(c.high))
        });
    let avg_volume: f64 = if daily.is_empty() {
        0.0
    } else {
        daily.iter().map(|c| c.volume).sum::<f64>() / daily.len() as f64
    };

    let change_color = match change {
        Some(c) if c > 0.0 => buy.into(),
        Some(c) if c < 0.0 => sell.into(),
        _ => fg,
    };
    let change_text = match (change, change_pct) {
        (Some(c), Some(p)) => format!("{:+.2}  ({:+.2}%)", c, p),
        _ => "—".to_string(),
    };

    let lhs = v_flex()
        .gap_1()
        .child(
            h_flex()
                .gap_2()
                .items_baseline()
                .child(
                    div()
                        .text_size(px(20.))
                        .font_semibold()
                        .child(format!("${:.2}", last.close)),
                )
                .child(div().text_size(px(13.)).text_color(change_color).child(change_text)),
        )
        .child(
            div()
                .text_size(px(11.))
                .text_color(muted)
                .child(format!("Open ${:.2}  ·  Prev ${:.2}", last.open, prev.map(|p| p.close).unwrap_or(last.open))),
        )
        .child(
            div()
                .text_size(px(11.))
                .text_color(muted)
                .child(format!("Day ${:.2} – ${:.2}", last.low, last.high)),
        );

    let rhs = v_flex()
        .gap_1()
        .items_end()
        .child(
            div()
                .text_size(px(11.))
                .text_color(muted)
                .child(format!("52w ${:.2} – ${:.2}", range_lo, range_hi)),
        )
        .child(
            div()
                .text_size(px(11.))
                .text_color(muted)
                .child(format!(
                    "Vol {}  ·  Avg {}",
                    fmt_int(last.volume),
                    fmt_int(avg_volume),
                )),
        );

    h_flex()
        .w_full()
        .items_start()
        .gap_4()
        .child(div().flex_1().min_w_0().child(lhs))
        .child(div().flex_shrink_0().child(rhs))
        .into_any_element()
}

// ---------------------------------------------------------------------------
// Fundamentals
// ---------------------------------------------------------------------------

fn render_fundamentals(
    overview: Option<&Overview>,
    latest_ratios: Option<&FinancialPeriod>,
    cx: &Context<DetailsPanel>,
) -> gpui::AnyElement {
    let theme = cx.theme();
    let muted = theme.muted_foreground;
    let fg = theme.foreground;

    let market_cap = overview
        .and_then(|o| o.market_cap)
        .map(|m| fmt_usd_compact(m))
        .unwrap_or_else(|| "—".to_string());
    let industry = overview
        .map(|o| o.industry.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("—")
        .to_string();

    let ratios = latest_ratios.and_then(|p| p.ratios.as_ref());
    let pe = ratio_lookup(ratios, &["price_earnings_ratio", "pe_ratio", "pe", "priceEarningsRatio"]);
    let eps = ratio_lookup(ratios, &["earnings_per_share", "eps", "epsBasic", "earningsPerShare"]);
    let div_yield = ratio_lookup(ratios, &["dividend_yield", "dividendYield"]);
    let beta = ratio_lookup(ratios, &["beta"]);

    let kv = |label: &'static str, value: SharedString| {
        h_flex()
            .gap_2()
            .items_baseline()
            .child(
                div()
                    .w(px(96.))
                    .text_size(px(11.))
                    .text_color(muted)
                    .child(label),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(px(13.))
                    .text_color(fg)
                    .child(value),
            )
    };

    let left = v_flex()
        .gap_1()
        .child(kv("Market Cap", SharedString::from(market_cap)))
        .child(kv("EPS (TTM)", SharedString::from(format_ratio(eps, FormatKind::Usd))))
        .child(kv("Div Yield", SharedString::from(format_ratio(div_yield, FormatKind::Percent))));

    let right = v_flex()
        .gap_1()
        .child(kv(
            "P/E (TTM)",
            SharedString::from(format_ratio(pe, FormatKind::Plain)),
        ))
        .child(kv("Beta", SharedString::from(format_ratio(beta, FormatKind::Plain))))
        .child(kv("Industry", SharedString::from(industry)));

    h_flex()
        .w_full()
        .gap_4()
        .items_start()
        .child(div().flex_1().min_w_0().child(left))
        .child(div().flex_1().min_w_0().child(right))
        .into_any_element()
}

// ---------------------------------------------------------------------------
// About
// ---------------------------------------------------------------------------

fn render_about(
    overview: Option<&Overview>,
    expanded: bool,
    cx: &Context<DetailsPanel>,
) -> gpui::AnyElement {
    let theme = cx.theme();
    let muted = theme.muted_foreground;
    let fg = theme.foreground;
    let Some(o) = overview else {
        return div().into_any_element();
    };
    if o.description.is_empty() {
        return div().into_any_element();
    }
    let collapsed_chars = 280;
    let needs_toggle = o.description.chars().count() > collapsed_chars;
    let shown_text = if expanded || !needs_toggle {
        o.description.clone()
    } else {
        let mut s: String = o.description.chars().take(collapsed_chars).collect();
        s.push_str("…");
        s
    };

    let mut block = v_flex().gap_1().child(
        div()
            .text_size(px(11.))
            .text_color(muted)
            .child("About"),
    );
    block = block.child(
        div()
            .text_size(px(13.))
            .text_color(fg)
            .child(SharedString::from(shown_text)),
    );
    if needs_toggle {
        let label = if expanded { "Show less" } else { "Show more" };
        block = block.child(
            Button::new("details-about-toggle")
                .label(label)
                .small()
                .ghost()
                .on_click(cx.listener(|this, _, _, cx| {
                    this.description_expanded = !this.description_expanded;
                    cx.notify();
                })),
        );
    }
    block.into_any_element()
}

// ---------------------------------------------------------------------------
// Dividends
// ---------------------------------------------------------------------------

fn render_dividends(
    dividends: Option<&Vec<Dividend>>,
    expanded: bool,
    state: &SectionState<Vec<Dividend>>,
    cx: &Context<DetailsPanel>,
) -> gpui::AnyElement {
    let theme = cx.theme();
    let muted = theme.muted_foreground;
    let fg = theme.foreground;
    let border = theme.border;

    if state.is_loading() && dividends.is_none() {
        return div()
            .text_size(px(11.))
            .text_color(muted)
            .child("Loading dividends…")
            .into_any_element();
    }
    let Some(divs) = dividends else {
        return div().into_any_element();
    };
    if divs.is_empty() {
        return v_flex()
            .gap_1()
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(muted)
                    .child("Dividends"),
            )
            .child(
                div()
                    .text_size(px(13.))
                    .text_color(muted)
                    .child("No recent dividends."),
            )
            .into_any_element();
    }

    let preview_count = if expanded {
        divs.len()
    } else {
        DIVIDEND_PREVIEW_COUNT.min(divs.len())
    };
    let header_label = SharedString::from(format!("Dividends ({} ex-dates)", divs.len()));

    let mut block = v_flex().gap_1().child(
        div()
            .text_size(px(11.))
            .text_color(muted)
            .child(header_label),
    );

    for d in divs.iter().take(preview_count) {
        let amount = format!("${:.2}", d.cash_amount);
        let freq = d.frequency.map(freq_label).unwrap_or("");
        let row = h_flex()
            .py_0p5()
            .gap_3()
            .items_baseline()
            .border_b_1()
            .border_color(border)
            .child(
                div()
                    .w(px(96.))
                    .text_size(px(11.))
                    .text_color(muted)
                    .child(SharedString::from(d.ex_dividend_date.clone())),
            )
            .child(
                div()
                    .w(px(72.))
                    .text_size(px(13.))
                    .text_color(fg)
                    .child(SharedString::from(amount)),
            )
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(muted)
                    .child(SharedString::from(format!(
                        "{}{}",
                        freq,
                        if d.currency.is_empty() {
                            String::new()
                        } else {
                            format!(" {}", d.currency)
                        }
                    ))),
            );
        block = block.child(row);
    }

    if divs.len() > DIVIDEND_PREVIEW_COUNT {
        let label = if expanded {
            "Show preview"
        } else {
            "Show all"
        };
        block = block.child(
            Button::new("details-div-toggle")
                .label(label)
                .small()
                .ghost()
                .on_click(cx.listener(|this, _, _, cx| {
                    this.dividends_expanded = !this.dividends_expanded;
                    cx.notify();
                })),
        );
    }
    block.into_any_element()
}

// ---------------------------------------------------------------------------
// Financials
// ---------------------------------------------------------------------------

/// Headline mapping table. Each row is `(label, statement-kind, [vendor
/// key candidates])`. We try each candidate against the vendor's blob in
/// order until one resolves to a number. Vendor (Massive/Polygon) returns
/// SEC-style snake_case keys nested under `.value`, but newer endpoints
/// sometimes flatten so both forms are accepted.
const HEADLINE_ROWS: &[(&str, FinSection, &[&str])] = &[
    ("Revenue", FinSection::Income, &["revenues", "revenue", "total_revenue", "totalRevenue"]),
    ("Gross Profit", FinSection::Income, &["gross_profit", "grossProfit"]),
    ("Operating Income", FinSection::Income, &["operating_income_loss", "operatingIncome"]),
    ("Net Income", FinSection::Income, &["net_income_loss", "netIncome"]),
    (
        "FCF",
        FinSection::Cashflow,
        &[
            "net_cash_flow_from_operating_activities",
            "operating_cash_flow",
            "operatingCashFlow",
        ],
    ),
    ("Total Assets", FinSection::Balance, &["assets", "total_assets", "totalAssets"]),
    (
        "Total Liabilities",
        FinSection::Balance,
        &["liabilities", "total_liabilities", "totalLiabilities"],
    ),
    ("Equity", FinSection::Balance, &["equity", "total_equity", "totalEquity"]),
];

#[derive(Clone, Copy)]
enum FinSection {
    Income,
    Balance,
    Cashflow,
}

fn render_financials(
    ticker: &SharedString,
    periods: Option<&Vec<FinancialPeriod>>,
    state: &SectionState<Vec<FinancialPeriod>>,
    cx: &Context<DetailsPanel>,
) -> gpui::AnyElement {
    let theme = cx.theme();
    let muted = theme.muted_foreground;
    let fg = theme.foreground;
    let border = theme.border;

    if state.is_loading() && periods.is_none() {
        return div()
            .text_size(px(11.))
            .text_color(muted)
            .child("Loading financials…")
            .into_any_element();
    }
    let Some(all) = periods else {
        return div().into_any_element();
    };
    if all.is_empty() {
        return v_flex()
            .gap_1()
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(muted)
                    .child("Financials"),
            )
            .child(
                div()
                    .text_size(px(13.))
                    .text_color(muted)
                    .child("No financial statements yet."),
            )
            .into_any_element();
    }

    let cols: Vec<&FinancialPeriod> =
        all.iter().take(FINANCIALS_COLUMNS).collect();
    let header_labels: Vec<SharedString> = cols
        .iter()
        .map(|p| SharedString::from(period_short_label(p)))
        .collect();

    let label_w = 132.0f32;
    let col_w = 88.0f32;
    let header_row = {
        let mut row = h_flex()
            .gap_2()
            .py_1()
            .border_b_1()
            .border_color(border)
            .text_size(px(11.))
            .text_color(muted)
            .child(div().w(px(label_w)).child("Financials"));
        for hl in &header_labels {
            row = row.child(div().w(px(col_w)).text_size(px(11.)).child(hl.clone()));
        }
        row
    };

    let mut rows = v_flex().w_full().child(header_row);
    for (label, section, candidates) in HEADLINE_ROWS {
        let mut row = h_flex().gap_2().py_0p5().items_baseline();
        row = row.child(
            div()
                .w(px(label_w))
                .text_size(px(12.))
                .text_color(muted)
                .child(*label),
        );
        for p in &cols {
            let blob = match section {
                FinSection::Income => p.income.as_ref(),
                FinSection::Balance => p.balance.as_ref(),
                FinSection::Cashflow => p.cashflow.as_ref(),
            };
            let raw = blob.and_then(|b| statement_lookup(b, candidates));
            let label = raw
                .map(|v| fmt_usd_compact(v))
                .unwrap_or_else(|| "—".to_string());
            row = row.child(
                div()
                    .w(px(col_w))
                    .text_size(px(12.))
                    .text_color(fg)
                    .child(SharedString::from(label)),
            );
        }
        rows = rows.child(row);
    }

    // "Show full statement" buttons — each opens a dialog with all four
    // statement blobs for that period.
    let buttons = h_flex().gap_2().py_1().when(cols.len() > 0, |mut h| {
        for (i, p) in cols.iter().enumerate() {
            let label = period_short_label(p);
            let period_clone = (*p).clone();
            let ticker_clone = ticker.clone();
            h = h.child(
                Button::new(SharedString::from(format!("details-stmt-{i}")))
                    .label(format!("Statements · {label}"))
                    .small()
                    .outline()
                    .on_click(move |_, window, cx| {
                        open_statements_dialog(
                            ticker_clone.clone(),
                            period_clone.clone(),
                            window,
                            cx,
                        );
                    }),
            );
        }
        h
    });
    rows = rows.child(buttons);

    rows.into_any_element()
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn period_short_label(p: &FinancialPeriod) -> String {
    // Prefer "Q1 26" style if we have fiscal_year + fiscal_period; fall
    // back to "YYYY-MM-DD".
    if !p.fiscal_period.is_empty() && p.fiscal_year.is_some() {
        return format!(
            "{} {:02}",
            p.fiscal_period,
            p.fiscal_year.unwrap_or(0) % 100
        );
    }
    p.period_end.clone()
}

#[derive(Clone, Copy)]
enum FormatKind {
    Plain,
    Usd,
    Percent,
}

fn format_ratio(v: Option<f64>, kind: FormatKind) -> String {
    match (v, kind) {
        (None, _) => "—".to_string(),
        (Some(n), FormatKind::Plain) => format!("{:.2}", n),
        (Some(n), FormatKind::Usd) => format!("${:.2}", n),
        (Some(n), FormatKind::Percent) => {
            // Vendor sometimes reports yield as 0.0052 (i.e. 0.52%) and
            // sometimes as 0.52 already. Heuristic: anything below 1 we
            // assume is a decimal fraction.
            let pct = if n.abs() < 1.0 { n * 100.0 } else { n };
            format!("{:.2}%", pct)
        }
    }
}

/// Look up a numeric value from a vendor financials blob. Each blob is
/// typically `{financials: {income_statement: {revenues: {value: N}}}}`
/// or a flat `{revenues: N}` map; we walk recursively and accept either.
fn statement_lookup(blob: &JsonValue, candidates: &[&str]) -> Option<f64> {
    for c in candidates {
        if let Some(v) = find_value(blob, c) {
            return Some(v);
        }
    }
    None
}

fn ratio_lookup(blob: Option<&JsonValue>, candidates: &[&str]) -> Option<f64> {
    let blob = blob?;
    for c in candidates {
        if let Some(v) = find_value(blob, c) {
            return Some(v);
        }
    }
    None
}

/// Recursive search for `key` anywhere in `v`. Numbers can land as
/// `{value: N}` (vendor's "fact" shape) or as a direct number; both
/// accepted.
fn find_value(v: &JsonValue, key: &str) -> Option<f64> {
    match v {
        JsonValue::Object(map) => {
            if let Some(child) = map.get(key) {
                return extract_number(child);
            }
            for (_, child) in map {
                if let Some(n) = find_value(child, key) {
                    return Some(n);
                }
            }
            None
        }
        JsonValue::Array(arr) => {
            for child in arr {
                if let Some(n) = find_value(child, key) {
                    return Some(n);
                }
            }
            None
        }
        _ => None,
    }
}

fn extract_number(v: &JsonValue) -> Option<f64> {
    match v {
        JsonValue::Number(n) => n.as_f64(),
        JsonValue::Object(map) => map.get("value").and_then(extract_number),
        _ => None,
    }
}

fn fmt_usd_compact(n: f64) -> String {
    let abs = n.abs();
    if abs >= 1_000_000_000_000.0 {
        format!("${:.2}T", n / 1_000_000_000_000.0)
    } else if abs >= 1_000_000_000.0 {
        format!("${:.2}B", n / 1_000_000_000.0)
    } else if abs >= 1_000_000.0 {
        format!("${:.2}M", n / 1_000_000.0)
    } else if abs >= 1_000.0 {
        format!("${:.2}K", n / 1_000.0)
    } else {
        format!("${:.2}", n)
    }
}

fn fmt_int(n: f64) -> String {
    let abs = n.abs();
    if abs >= 1_000_000_000.0 {
        format!("{:.2}B", n / 1_000_000_000.0)
    } else if abs >= 1_000_000.0 {
        format!("{:.2}M", n / 1_000_000.0)
    } else if abs >= 1_000.0 {
        format!("{:.0}K", n / 1_000.0)
    } else {
        format!("{:.0}", n)
    }
}

fn freq_label(freq: i32) -> &'static str {
    match freq {
        1 => "Annual",
        2 => "Semi-annual",
        4 => "Quarterly",
        12 => "Monthly",
        _ => "",
    }
}

// ---------------------------------------------------------------------------
// "Show full statement" dialog
// ---------------------------------------------------------------------------

/// Open the full-statement viewer for a single financial period.
///
/// The dialog renders four sections — Income, Balance, Cash flow, Ratios —
/// each backed by the period's vendor JSON blob. Sections with no data
/// render a `(no data)` placeholder. The tree is fully expanded; the
/// dialog is scrollable.
fn open_statements_dialog(
    ticker: SharedString,
    period: FinancialPeriod,
    window: &mut Window,
    cx: &mut App,
) {
    use gpui_component::{
        dialog::DialogButtonProps, separator::Separator,
    };

    let dialog_title = SharedString::from(format!(
        "{} · {}",
        ticker,
        period_short_label(&period)
    ));

    window.open_dialog(cx, move |dialog, _w, cx| {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let fg = theme.foreground;

        let section = |label: &'static str, blob: Option<&JsonValue>| {
            let header = h_flex()
                .py_1()
                .gap_2()
                .items_baseline()
                .child(
                    div()
                        .text_size(px(12.))
                        .font_semibold()
                        .text_color(fg)
                        .child(label),
                )
                .child(
                    div()
                        .text_size(px(10.))
                        .text_color(muted)
                        .child(if blob.is_some() { "" } else { "(no data)" }),
                );
            let body = match blob {
                Some(v) => render_json_tree(v, 0, fg, muted).into_any_element(),
                None => div().into_any_element(),
            };
            v_flex()
                .gap_1()
                .child(header)
                .child(div().pl_2().child(body))
        };

        let body = v_flex()
            .px_4()
            .pb_2()
            .pt_2()
            .gap_3()
            .child(section("Income statement", period.income.as_ref()))
            .child(Separator::horizontal())
            .child(section("Balance sheet", period.balance.as_ref()))
            .child(Separator::horizontal())
            .child(section("Cash flow", period.cashflow.as_ref()))
            .child(Separator::horizontal())
            .child(section("Ratios", period.ratios.as_ref()));

        // The body can be tall — wrap in a scroll container with a cap so
        // the dialog stays inside the window.
        let scroll = div()
            .id("statements-scroll")
            .max_h(px(560.))
            .overflow_y_scroll()
            .child(body);

        dialog
            .title(dialog_title.clone())
            .max_w(px(720.))
            .button_props(DialogButtonProps::default().ok_text("Close"))
            .child(scroll)
    });
}

/// Render a JSON value as a nested tree.
///
/// Numbers that look like dollar amounts (>= 1K) are rendered with
/// USD-compact formatting; smaller numbers and ratios stay as-is. Strings,
/// bools, and nulls render verbatim. Objects/arrays nest with a left
/// padding step per level.
fn render_json_tree(
    v: &JsonValue,
    depth: usize,
    fg: gpui::Hsla,
    muted: gpui::Hsla,
) -> gpui::AnyElement {
    let indent = (depth as f32) * 12.0;
    match v {
        JsonValue::Object(map) => {
            let mut block = v_flex().gap_0p5();
            for (k, child) in map {
                let row = h_flex()
                    .gap_2()
                    .items_baseline()
                    .pl(px(indent))
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(muted)
                            .child(SharedString::from(k.clone())),
                    )
                    .child(render_json_inline_or_nested(child, depth + 1, fg, muted));
                block = block.child(row);
            }
            block.into_any_element()
        }
        JsonValue::Array(arr) => {
            let mut block = v_flex().gap_0p5();
            for (i, child) in arr.iter().enumerate() {
                let row = h_flex()
                    .gap_2()
                    .items_baseline()
                    .pl(px(indent))
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(muted)
                            .child(SharedString::from(format!("[{i}]"))),
                    )
                    .child(render_json_inline_or_nested(child, depth + 1, fg, muted));
                block = block.child(row);
            }
            block.into_any_element()
        }
        _ => div()
            .pl(px(indent))
            .text_size(px(12.))
            .text_color(fg)
            .child(SharedString::from(format_scalar(v)))
            .into_any_element(),
    }
}

/// Decide whether to inline a value (scalar or `{value: N}` "fact" wrapper)
/// next to its label, or break it into a nested tree below.
fn render_json_inline_or_nested(
    v: &JsonValue,
    depth: usize,
    fg: gpui::Hsla,
    muted: gpui::Hsla,
) -> gpui::AnyElement {
    if let Some(scalar) = inline_scalar(v) {
        return div()
            .text_size(px(12.))
            .text_color(fg)
            .child(SharedString::from(scalar))
            .into_any_element();
    }
    // Otherwise descend on a new line.
    div()
        .flex_1()
        .min_w_0()
        .child(render_json_tree(v, depth, fg, muted))
        .into_any_element()
}

/// Inline a value that's small enough to render alongside its label:
/// scalars become themselves; vendor "fact" wrappers `{value: N, unit: …}`
/// collapse to the numeric value (drop the unit) for readability.
fn inline_scalar(v: &JsonValue) -> Option<String> {
    match v {
        JsonValue::Null | JsonValue::Bool(_) | JsonValue::Number(_) | JsonValue::String(_) => {
            Some(format_scalar(v))
        }
        JsonValue::Object(map) => {
            // Vendor "fact" shape: {value: <number>, unit: "USD", label: "…"}.
            // Only inline if `value` is a scalar and the map has no other
            // structural keys.
            if let Some(value) = map.get("value") {
                if matches!(
                    value,
                    JsonValue::Null
                        | JsonValue::Bool(_)
                        | JsonValue::Number(_)
                        | JsonValue::String(_)
                ) && map.iter().all(|(k, child)| {
                    k == "value"
                        || matches!(
                            child,
                            JsonValue::Null
                                | JsonValue::Bool(_)
                                | JsonValue::Number(_)
                                | JsonValue::String(_)
                        )
                }) {
                    return Some(format_scalar(value));
                }
            }
            None
        }
        JsonValue::Array(_) => None,
    }
}

fn format_scalar(v: &JsonValue) -> String {
    match v {
        JsonValue::Null => "—".to_string(),
        JsonValue::Bool(b) => b.to_string(),
        JsonValue::String(s) => s.clone(),
        JsonValue::Number(n) => {
            if let Some(f) = n.as_f64() {
                if f.abs() >= 1_000.0 {
                    fmt_usd_compact(f)
                } else if f.fract() == 0.0 {
                    format!("{}", f as i64)
                } else {
                    format!("{:.4}", f)
                }
            } else {
                n.to_string()
            }
        }
        _ => String::new(),
    }
}
