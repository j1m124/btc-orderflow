//! Economic calendar panel. Lives alongside the other panel modules under
//! `panels/` even though it's a custom `Panel` impl (not a `ContentPanel`
//! render helper) — the directory groups everything panel-shaped in one
//! place. Visual design mirrors `panels/watchlist.rs`: tight typography
//! (px 11 header / 13 rows), responsive `flex_1 + min_w_0 + ellipsis`
//! columns, hover highlight per row. Row list runs through `v_virtual_list`
//! (à la `filings.rs`) so multi-day ranges scroll cheaply.

use std::collections::HashSet;
use std::rc::Rc;

use chrono::{Datelike, Duration, NaiveDate, TimeZone as _};
use gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, MouseButton, ParentElement as _, Pixels, Render,
    SharedString, Size, StatefulInteractiveElement as _, Styled as _, Subscription, WeakEntity,
    Window, div, px, size,
    prelude::FluentBuilder as _,
};
use gpui_component::{
    ActiveTheme as _, Sizable as _, StyledExt as _, VirtualListScrollHandle,
    button::{Button, ButtonVariants as _},
    calendar::{Calendar, CalendarEvent, CalendarState, Date},
    dock::{Panel, PanelEvent, TabPanel},
    h_flex,
    input::{Input, InputEvent, InputState},
    popover::Popover,
    v_flex, v_virtual_list,
};

use crate::panels::{Kind, LastFocusedTabPanel};
use crate::prefs;
use crate::services::calendar::{
    CalendarEvent as CalEvent, CalendarServiceHandle, CalendarState as ServiceState,
    ColorDirection,
};

/// Uniform row height for the virtual list. 28px matches the row's
/// `py_1` + `text_size(px(13))` natural height; keeping it fixed lets
/// VirtualList allocate `item_sizes` cheaply.
const ROW_HEIGHT_PX: f32 = 28.0;

/// Fixed width of the centered calendar widget. 7 weekday columns at
/// ~36px + margins fits inside this without horizontal scroll.
const CALENDAR_WIDTH_PX: f32 = 280.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Impact {
    High,
    Medium,
    Low,
    Holiday,
}

impl Impact {
    fn label(self) -> &'static str {
        match self {
            Impact::High => "High",
            Impact::Medium => "Medium",
            Impact::Low => "Low",
            Impact::Holiday => "Holiday",
        }
    }

    fn dot_color(self, theme: &gpui_component::theme::Theme) -> gpui::Hsla {
        match self {
            Impact::High => theme.chart_bearish,
            Impact::Medium => theme.warning,
            Impact::Low => theme.chart_bullish,
            Impact::Holiday => theme.muted_foreground,
        }
    }

    fn from_wire(s: &str) -> Option<Impact> {
        match s {
            "high" => Some(Impact::High),
            "medium" => Some(Impact::Medium),
            "low" => Some(Impact::Low),
            "holiday" => Some(Impact::Holiday),
            _ => None,
        }
    }

    const ALL: &'static [Impact] = &[
        Impact::High,
        Impact::Medium,
        Impact::Low,
        Impact::Holiday,
    ];
}

/// Quick time-range chips. Week math is Mon–Sun (ISO 8601). "Today" is
/// computed in the user's selected TZ via `prefs::now_in_user_tz`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RangeKind {
    Yesterday,
    Today,
    Tomorrow,
    ThisWeek,
    NextWeek,
}

impl RangeKind {
    const ALL: &'static [RangeKind] = &[
        RangeKind::Yesterday,
        RangeKind::Today,
        RangeKind::Tomorrow,
        RangeKind::ThisWeek,
        RangeKind::NextWeek,
    ];

    fn label(self) -> &'static str {
        match self {
            RangeKind::Yesterday => "Yesterday",
            RangeKind::Today => "Today",
            RangeKind::Tomorrow => "Tomorrow",
            RangeKind::ThisWeek => "This week",
            RangeKind::NextWeek => "Next week",
        }
    }

    /// Resolve to a (start, end) inclusive range in the user's TZ.
    fn bounds(self, today: NaiveDate) -> (NaiveDate, NaiveDate) {
        match self {
            RangeKind::Yesterday => {
                let d = today - Duration::days(1);
                (d, d)
            }
            RangeKind::Today => (today, today),
            RangeKind::Tomorrow => {
                let d = today + Duration::days(1);
                (d, d)
            }
            RangeKind::ThisWeek => week_mon_sun(today),
            RangeKind::NextWeek => {
                let (mon, sun) = week_mon_sun(today);
                (mon + Duration::days(7), sun + Duration::days(7))
            }
        }
    }

    fn is_single_day(self) -> bool {
        matches!(self, RangeKind::Yesterday | RangeKind::Today | RangeKind::Tomorrow)
    }
}

/// Mon–Sun bounds for the ISO week containing `d`.
fn week_mon_sun(d: NaiveDate) -> (NaiveDate, NaiveDate) {
    let days_from_mon = d.weekday().num_days_from_monday() as i64;
    let mon = d - Duration::days(days_from_mon);
    (mon, mon + Duration::days(6))
}

pub struct EconomicCalendarPanel {
    focus_handle: FocusHandle,
    parent_tab_panel: Option<gpui::WeakEntity<TabPanel>>,
    calendar: Entity<CalendarState>,
    calendar_open: bool,
    impact_filter: HashSet<Impact>,
    /// `None` → no currency filter (all pass). `Some(set)` → only these
    /// currencies pass. Default is `None` to match "all checked".
    currency_filter: Option<HashSet<String>>,
    service: Entity<crate::services::calendar::CalendarService>,
    scroll_handle: VirtualListScrollHandle,
    /// Search query backing the currency popover's filter bar. Owned on the
    /// panel (not inside the popover closure) so the InputState survives
    /// across re-renders — recreating it per render would reset the cursor
    /// and typed text on every keystroke.
    currency_search: Entity<InputState>,
    /// Scroll position for the virtualized currency list. Held here so it
    /// persists when the popover content closure rebuilds.
    currency_scroll: VirtualListScrollHandle,
    _calendar_subscription: Subscription,
    _service_subscription: Subscription,
    _currency_search_subscription: Subscription,
}

impl EconomicCalendarPanel {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let today = prefs::now_in_user_tz(cx).date_naive();
        let calendar = cx.new(|cx| {
            let mut state = CalendarState::new(window, cx);
            state.set_date(today, window, cx);
            state
        });

        let _calendar_subscription =
            cx.subscribe(&calendar, |_this, _state, _ev: &CalendarEvent, cx| {
                cx.notify();
            });

        let service = cx.global::<CalendarServiceHandle>().0.clone();
        // Log fetch failures to the browser console rather than surfacing the
        // raw error text in the status label. The service only emits Changed
        // on state transitions, so this fires once per failed fetch — no
        // per-render dedup needed.
        let _service_subscription = cx.subscribe(
            &service,
            |_this, svc, _ev: &crate::services::calendar::CalendarEvent_, cx| {
                if let ServiceState::Error { message, .. } = svc.read(cx).state() {
                    log::error!("economic calendar fetch failed: {message}");
                }
                cx.notify();
            },
        );

        let mut impact_filter = HashSet::new();
        impact_filter.insert(Impact::High);
        impact_filter.insert(Impact::Medium);
        impact_filter.insert(Impact::Low);

        let currency_search = cx.new(|cx| {
            InputState::new(window, cx).placeholder("Search currency…")
        });
        let _currency_search_subscription =
            cx.subscribe(&currency_search, |_this, _input, ev: &InputEvent, cx| {
                if matches!(ev, InputEvent::Change) {
                    cx.notify();
                }
            });

        Self {
            focus_handle: cx.focus_handle(),
            parent_tab_panel: None,
            calendar,
            calendar_open: true,
            impact_filter,
            currency_filter: None,
            service,
            scroll_handle: VirtualListScrollHandle::new(),
            currency_search,
            currency_scroll: VirtualListScrollHandle::new(),
            _calendar_subscription,
            _service_subscription,
            _currency_search_subscription,
        }
    }

    fn toggle_calendar(&mut self, cx: &mut Context<Self>) {
        self.calendar_open = !self.calendar_open;
        cx.notify();
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

    fn toggle_impact(&mut self, impact: Impact, cx: &mut Context<Self>) {
        if !self.impact_filter.insert(impact) {
            self.impact_filter.remove(&impact);
        }
        cx.notify();
    }

    fn toggle_currency(&mut self, currency: String, all_currencies: &[String], cx: &mut Context<Self>) {
        // First toggle: copy the "all" set into an explicit filter so removing
        // one currency excludes only that one. Subsequent toggles flip
        // membership within the explicit set.
        let set = self.currency_filter.get_or_insert_with(|| {
            all_currencies.iter().cloned().collect()
        });
        if !set.insert(currency.clone()) {
            set.remove(&currency);
        }
        // If the explicit filter matches the full universe again, drop it
        // back to None so re-opening the popover shows "all checked" cleanly
        // and a refresh introducing a new currency doesn't quietly hide it.
        if set.len() == all_currencies.len()
            && all_currencies.iter().all(|c| set.contains(c))
        {
            self.currency_filter = None;
        }
        cx.notify();
    }

    fn currency_passes(&self, country: &str) -> bool {
        match &self.currency_filter {
            None => true,
            Some(set) => set.contains(country),
        }
    }

    /// Inclusive (start, end) range in user-TZ to filter rows by. `None`
    /// means no date filter (rare — only if calendar is in an empty state).
    fn selected_range(&self, cx: &App) -> Option<(NaiveDate, NaiveDate)> {
        match self.calendar.read(cx).date() {
            Date::Single(Some(d)) => Some((d, d)),
            Date::Range(Some(start), Some(end)) => {
                if start <= end { Some((start, end)) } else { Some((end, start)) }
            }
            Date::Range(Some(start), None) => Some((start, start)),
            _ => None,
        }
    }

    fn set_range(&mut self, kind: RangeKind, window: &mut Window, cx: &mut Context<Self>) {
        let today = prefs::now_in_user_tz(cx).date_naive();
        let (start, end) = kind.bounds(today);
        self.calendar.update(cx, |state, cx| {
            if kind.is_single_day() {
                state.set_date(start, window, cx);
            } else {
                state.set_date((start, end), window, cx);
            }
        });
        cx.notify();
    }

    /// Which range chip (if any) matches the calendar's current Date.
    fn current_range_kind(&self, cx: &App) -> Option<RangeKind> {
        let today = prefs::now_in_user_tz(cx).date_naive();
        let current = self.calendar.read(cx).date();
        for &kind in RangeKind::ALL {
            let (start, end) = kind.bounds(today);
            let expected = if kind.is_single_day() {
                Date::Single(Some(start))
            } else {
                Date::Range(Some(start), Some(end))
            };
            if current == expected {
                return Some(kind);
            }
        }
        None
    }

    fn trigger_refresh(&mut self, cx: &mut Context<Self>) {
        self.service.update(cx, |svc, cx| svc.reload(cx));
    }
}

/// Project an event's UTC instant into the user's selected TZ, returning the
/// calendar date as the user perceives it. Filter buckets and the row's
/// time-of-day label both use this so they stay aligned.
fn event_date_in_user_tz(cx: &App, e: &CalEvent) -> NaiveDate {
    let offset = prefs::offset_for(cx, e.event_time.timestamp_millis());
    offset
        .from_utc_datetime(&e.event_time.naive_utc())
        .date_naive()
}

impl EventEmitter<PanelEvent> for EconomicCalendarPanel {}

impl Focusable for EconomicCalendarPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Panel for EconomicCalendarPanel {
    fn panel_name(&self) -> &'static str {
        Kind::EconomicCalendar.id()
    }

    fn title(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        SharedString::from(Kind::EconomicCalendar.display())
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

impl Render for EconomicCalendarPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (muted, border, accent, accent_fg, bullish, bearish, fg, warning, hover_bg) = {
            let theme = cx.theme();
            (
                theme.muted_foreground,
                theme.border,
                theme.accent,
                theme.accent_foreground,
                theme.chart_bullish,
                theme.chart_bearish,
                theme.foreground,
                theme.warning,
                theme.accent,
            )
        };
        let ring_color = if self.is_focused(cx) {
            cx.theme().ring
        } else {
            gpui::transparent_black()
        };

        // Clone state out so we drop the entity borrow immediately. Holding
        // a `&[CalEvent]` across the rest of the render risks "already
        // borrowed" if a fetch-completion `update` lands mid-frame.
        let (events_owned, status_label): (Option<Vec<CalEvent>>, Option<SharedString>) = {
            let state = self.service.read(cx).state().clone();
            match state {
                ServiceState::Idle | ServiceState::Loading => {
                    (None, Some(SharedString::from("Loading…")))
                }
                ServiceState::Loaded { events, fetched_at } => {
                    // Render the fetch timestamp in the user's selected TZ
                    // (matches the per-event time column below). The TZ
                    // suffix is dropped so the header stays tight; the
                    // bottom-bar clock surfaces the active offset.
                    let offset = prefs::offset_for(cx, fetched_at.timestamp_millis());
                    let local = offset.from_utc_datetime(&fetched_at.naive_utc());
                    (
                        Some(events),
                        Some(SharedString::from(format!(
                            "Updated {}",
                            local.format("%H:%M")
                        ))),
                    )
                }
                ServiceState::Error {
                    last_events,
                    last_fetched_at,
                    ..
                } => {
                    // Error detail is logged to the browser console at
                    // subscribe time; the UI only surfaces a generic status
                    // so a noisy upstream error string can't bleed into the
                    // header.
                    let label = match last_fetched_at {
                        Some(t) => {
                            let offset = prefs::offset_for(cx, t.timestamp_millis());
                            let local = offset.from_utc_datetime(&t.naive_utc());
                            format!("Stale (last {})", local.format("%H:%M"))
                        }
                        None => "Failed to load".to_string(),
                    };
                    (last_events, Some(SharedString::from(label)))
                }
            }
        };
        let all_events: Vec<CalEvent> = events_owned.unwrap_or_default();

        // Currency universe — derived from the loaded events, unfiltered by
        // date/impact, sorted for a stable popover order.
        let mut all_currencies: Vec<String> = all_events
            .iter()
            .map(|e| e.country.clone())
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        all_currencies.sort();

        let selected_range = self.selected_range(cx);
        let active_range_kind = self.current_range_kind(cx);
        let multi_day = match selected_range {
            Some((s, e)) => s != e,
            None => false,
        };

        let filter = self.impact_filter.clone();
        let invert_pref = prefs::invert_macro_colors();

        // ─── Filter + sort ──────────────────────────────────────────────
        let mut rows: Vec<CalEvent> = all_events
            .iter()
            .filter(|e| {
                let event_date = event_date_in_user_tz(cx, e);
                let in_range = match selected_range {
                    Some((s, end)) => event_date >= s && event_date <= end,
                    None => true,
                };
                if !in_range {
                    return false;
                }
                let passes_impact = match Impact::from_wire(&e.impact) {
                    Some(i) => filter.contains(&i),
                    None => true,
                };
                if !passes_impact {
                    return false;
                }
                if !self.currency_passes(&e.country) {
                    return false;
                }
                true
            })
            .cloned()
            .collect();
        rows.sort_by_key(|e| e.event_time);

        let header_label = match (selected_range, active_range_kind) {
            (_, Some(kind)) => SharedString::from(format!("Events · {}", kind.label())),
            (Some((s, e)), None) if s == e => {
                SharedString::from(format!("Events · {}", s))
            }
            (Some((s, e)), None) => {
                SharedString::from(format!("Events · {} – {}", s, e))
            }
            (None, _) => SharedString::from("All events"),
        };
        let count_label = SharedString::from(format!("{} match", rows.len()));

        // ─── Top header (refresh + status + count) ──────────────────────
        let mut top_header = h_flex()
            .px_2()
            .py_1()
            .gap_2()
            .items_center()
            .child(
                Button::new("refresh-calendar")
                    .label("↻")
                    .small()
                    .ghost()
                    .tooltip("Refresh calendar")
                    .on_click(cx.listener(|this, _, _, cx| this.trigger_refresh(cx))),
            );
        if let Some(label) = status_label.clone() {
            top_header = top_header
                .child(div().text_size(px(11.)).text_color(muted).child(label));
        }
        top_header = top_header
            .child(div().flex_1())
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(muted)
                    .child(count_label.clone()),
            );

        // ─── Impact filter chips ────────────────────────────────────────
        let mut impact_chips = h_flex().gap_1p5().items_center();
        for &impact in Impact::ALL {
            let on = self.impact_filter.contains(&impact);
            let dot = impact.dot_color(cx.theme());
            let label = impact.label();
            let id = SharedString::from(format!("impact-chip-{label}"));
            impact_chips = impact_chips.child(
                h_flex()
                    .gap_1()
                    .items_center()
                    .px_2()
                    .py_0p5()
                    .rounded(px(3.))
                    .border_1()
                    .border_color(if on { dot } else { border })
                    .when(on, |s| s.bg(cx.theme().muted))
                    .child(div().size_2().rounded_full().bg(dot))
                    .child(
                        Button::new(id)
                            .label(label)
                            .xsmall()
                            .ghost()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.toggle_impact(impact, cx);
                            })),
                    ),
            );
        }

        // ─── Range chips ────────────────────────────────────────────────
        let mut range_chips = h_flex().gap_1p5().items_center();
        for &kind in RangeKind::ALL {
            let on = active_range_kind == Some(kind);
            let label = kind.label();
            let id = SharedString::from(format!("range-chip-{label}"));
            range_chips = range_chips.child(
                Button::new(id)
                    .label(label)
                    .xsmall()
                    .when(on, |b| b.primary())
                    .when(!on, |b| b.ghost())
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.set_range(kind, window, cx);
                    })),
            );
        }

        // ─── Currency popover ───────────────────────────────────────────
        let currency_button_label: SharedString = match &self.currency_filter {
            None => "Cur: All".into(),
            Some(set) if set.is_empty() => "Cur: None".into(),
            Some(set) => {
                let mut picked: Vec<&String> = set.iter().collect();
                picked.sort();
                if picked.len() <= 3 {
                    let s = picked
                        .into_iter()
                        .map(|s| s.as_str())
                        .collect::<Vec<_>>()
                        .join(",");
                    format!("Cur: {s}").into()
                } else {
                    format!("Cur: {} selected", picked.len()).into()
                }
            }
        };
        let panel_weak = cx.entity().downgrade();
        let cur_universe: Rc<Vec<String>> = Rc::new(all_currencies.clone());
        let cur_search = self.currency_search.clone();
        let cur_scroll = self.currency_scroll.clone();
        let currency_popover = Popover::new("cur-popover")
            .trigger(
                Button::new("cur-popover-trigger")
                    .label(currency_button_label)
                    .xsmall()
                    .outline(),
            )
            .p_0()
            .on_open_change({
                let cur_search = cur_search.clone();
                move |open, window, cx| {
                    if !*open {
                        cur_search
                            .update(cx, |input, cx| input.set_value("", window, cx));
                    }
                }
            })
            .content({
                let panel_weak = panel_weak.clone();
                let cur_universe = cur_universe.clone();
                let cur_search = cur_search.clone();
                let cur_scroll = cur_scroll.clone();
                move |_, _, cx| {
                    render_currency_popover(
                        panel_weak.clone(),
                        cur_universe.clone(),
                        cur_search.clone(),
                        cur_scroll.clone(),
                        cx,
                    )
                }
            });

        // ─── Calendar widget toggle + centered widget ───────────────────
        let calendar_open = self.calendar_open;
        let calendar_label: SharedString = match (active_range_kind, selected_range) {
            (Some(kind), _) => format!("Calendar · {}", kind.label()).into(),
            (None, Some((s, e))) if s == e => format!("Calendar · {}", s).into(),
            (None, Some((s, e))) => format!("Calendar · {} – {}", s, e).into(),
            (None, None) => "Calendar".into(),
        };
        let chevron = if calendar_open { "▾" } else { "▸" };
        let calendar_section = v_flex()
            .gap_1()
            .child(
                h_flex().justify_center().child(
                    Button::new("toggle-calendar")
                        .small()
                        .ghost()
                        .label(SharedString::from(format!("{chevron}  {calendar_label}")))
                        .on_click(cx.listener(|this, _, _, cx| this.toggle_calendar(cx))),
                ),
            )
            .when(calendar_open, |s| {
                s.child(
                    h_flex().w_full().justify_center().child(
                        div()
                            .w(px(CALENDAR_WIDTH_PX))
                            .child(Calendar::new(&self.calendar)),
                    ),
                )
            });

        // ─── Unified filter row ─────────────────────────────────────────
        // Each label is grouped with its chips inside its own h_flex so
        // flex_wrap moves the label and its chips as one unit — "Impact:"
        // can't end up on a different line from the impact chips.
        let range_group = h_flex()
            .gap_2()
            .items_center()
            .flex_shrink_0()
            .child(div().text_size(px(11.)).text_color(muted).child("Range:"))
            .child(range_chips);
        let impact_group = h_flex()
            .gap_2()
            .items_center()
            .flex_shrink_0()
            .child(div().text_size(px(11.)).text_color(muted).child("Impact:"))
            .child(impact_chips);
        let filter_row = h_flex()
            .px_2()
            .py_1()
            .gap_3()
            .items_center()
            .flex_wrap()
            .child(range_group)
            .child(impact_group)
            .child(currency_popover);

        // ─── Column header ──────────────────────────────────────────────
        // Every cell carries `min_w_0()` so a cell whose intrinsic content
        // is wider than its declared `w(px(N))` doesn't push the row wider
        // than the header (without it, flex's implicit `min-content` keeps
        // longer text from shrinking and cumulative drift accumulates
        // across columns). Row body uses the same set on every cell.
        let mut column_header = h_flex()
            .px_2()
            .py_1()
            .gap_2()
            .text_size(px(11.))
            .text_color(muted)
            .border_b_1()
            .border_color(border);
        if multi_day {
            column_header = column_header
                .child(
                    div()
                        .w(px(48.))
                        .min_w_0()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .child("Date"),
                )
                .child(
                    div()
                        .w(px(44.))
                        .min_w_0()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .child("Time"),
                );
        } else {
            column_header = column_header.child(
                div()
                    .w(px(48.))
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .child("Time"),
            );
        }
        column_header = column_header
            .child(
                div()
                    .w(px(36.))
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .child("Cur"),
            )
            .child(div().w(px(12.)).min_w_0().overflow_hidden().child(""))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .child("Event"),
            )
            .child(
                div()
                    .w(px(64.))
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_right()
                    .child("Actual"),
            )
            .child(
                div()
                    .w(px(64.))
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_right()
                    .child("Forecast"),
            )
            .child(
                div()
                    .w(px(64.))
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_right()
                    .child("Prev"),
            );

        // ─── Virtualized row body ───────────────────────────────────────
        let body_rows: gpui::AnyElement = if rows.is_empty() {
            div()
                .py_6()
                .px_2()
                .text_size(px(13.))
                .text_color(muted)
                .child(
                    "No events match the active range / impact / currency filter.",
                )
                .into_any_element()
        } else {
            let item_sizes: Rc<Vec<Size<Pixels>>> = Rc::new(
                (0..rows.len())
                    .map(|_| size(px(0.), px(ROW_HEIGHT_PX)))
                    .collect(),
            );
            let rows_rc: Rc<Vec<CalEvent>> = Rc::new(rows);
            let rows_for_closure = rows_rc.clone();
            v_virtual_list(
                cx.entity().clone(),
                "calendar-rows",
                item_sizes,
                move |_this, visible_range, _window, cx| {
                    // (rows below; outer VirtualList styled with
                    // overflow_x_hidden after this block so rows can't
                    // scroll horizontally past the header.)
                    let theme = cx.theme();
                    let muted = theme.muted_foreground;
                    let border = theme.border;
                    let accent = theme.accent;
                    let accent_fg = theme.accent_foreground;
                    let bullish = theme.chart_bullish;
                    let bearish = theme.chart_bearish;
                    let fg = theme.foreground;
                    let warning = theme.warning;
                    let hover_bg = theme.accent;
                    visible_range
                        .map(|i| {
                            let e = &rows_for_closure[i];
                            let impact_dot = Impact::from_wire(&e.impact)
                                .map(|i| i.dot_color(theme))
                                .unwrap_or(muted);
                            let actual_color = match (e.actual, e.forecast) {
                                (Some(av), Some(fv)) => {
                                    let beat = av >= fv;
                                    let invert = invert_pref
                                        && e.color_direction == ColorDirection::Inverted;
                                    let bullish_now = if invert { !beat } else { beat };
                                    if bullish_now { bullish } else { bearish }
                                }
                                _ => fg,
                            };
                            let unit_label = e.unit.as_deref().unwrap_or("");
                            let fmt_num = |v: Option<f64>| -> String {
                                match v {
                                    Some(n) => {
                                        if unit_label.is_empty() {
                                            format!("{:.2}", n)
                                        } else {
                                            format!("{:.2}{}", n, unit_label)
                                        }
                                    }
                                    None => "—".to_string(),
                                }
                            };
                            let offset = prefs::offset_for(cx, e.event_time.timestamp_millis());
                            let local = offset.from_utc_datetime(&e.event_time.naive_utc());
                            let time_label = local.format("%H:%M").to_string();
                            let date_label = local.format("%m-%d").to_string();
                            let cur_label = e.country.clone();
                            let title_color = if e.impact == "holiday" { warning } else { fg };
                            let row_id = SharedString::from(format!(
                                "cal-row-{}-{}-{}",
                                e.country, e.event_name, e.event_time.timestamp_millis()
                            ));
                            let mut row = h_flex()
                                .id(row_id)
                                .w_full()
                                .h(px(ROW_HEIGHT_PX))
                                .px_2()
                                .gap_2()
                                .items_center()
                                .text_size(px(13.))
                                .border_b_1()
                                .border_color(border)
                                .hover(|s| s.bg(hover_bg).opacity(0.95));
                            if multi_day {
                                row = row
                                    .child(
                                        div()
                                            .w(px(48.))
                                            .min_w_0()
                                            .overflow_hidden()
                                            .whitespace_nowrap()
                                            .text_ellipsis()
                                            .text_size(px(11.))
                                            .text_color(muted)
                                            .child(SharedString::from(date_label)),
                                    )
                                    .child(
                                        div()
                                            .w(px(44.))
                                            .min_w_0()
                                            .overflow_hidden()
                                            .whitespace_nowrap()
                                            .text_ellipsis()
                                            .text_size(px(11.))
                                            .text_color(muted)
                                            .child(SharedString::from(time_label)),
                                    );
                            } else {
                                row = row.child(
                                    div()
                                        .w(px(48.))
                                        .min_w_0()
                                        .overflow_hidden()
                                        .whitespace_nowrap()
                                        .text_ellipsis()
                                        .text_size(px(11.))
                                        .text_color(muted)
                                        .child(SharedString::from(time_label)),
                                );
                            }
                            row.child(
                                div()
                                    .w(px(36.))
                                    .min_w_0()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .child(
                                        div()
                                            .px_1p5()
                                            .py_0p5()
                                            .rounded(px(3.))
                                            .bg(accent)
                                            .text_color(accent_fg)
                                            .text_size(px(10.))
                                            .min_w_0()
                                            .overflow_hidden()
                                            .whitespace_nowrap()
                                            .text_ellipsis()
                                            .child(SharedString::from(cur_label)),
                                    ),
                            )
                            .child(
                                div()
                                    .w(px(12.))
                                    .min_w_0()
                                    .overflow_hidden()
                                    .flex()
                                    .items_center()
                                    .child(div().size_2().rounded_full().bg(impact_dot)),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .text_color(title_color)
                                    .child(SharedString::from(e.event_name.clone())),
                            )
                            .child(
                                div()
                                    .w(px(64.))
                                    .min_w_0()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .text_right()
                                    .text_color(actual_color)
                                    .font_semibold()
                                    .child(SharedString::from(fmt_num(e.actual))),
                            )
                            .child(
                                div()
                                    .w(px(64.))
                                    .min_w_0()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .text_right()
                                    .text_color(muted)
                                    .child(SharedString::from(fmt_num(e.forecast))),
                            )
                            .child(
                                div()
                                    .w(px(64.))
                                    .min_w_0()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .text_right()
                                    .text_color(muted)
                                    .child(SharedString::from(fmt_num(e.previous))),
                            )
                        })
                        .collect::<Vec<_>>()
                },
            )
            .track_scroll(&self.scroll_handle)
            .into_any_element()
        };

        // Drop unused theme tokens captured for the empty-state branch so
        // they don't trigger unused-variable warnings.
        let _ = (accent, accent_fg, bullish, bearish, fg, warning, hover_bg);

        // Pinned chrome at top, virtualized list flex-1 below. Matches the
        // `filings.rs` scroll model: outer is sized, virtual list scrolls.
        let mut body = v_flex()
            .size_full()
            .child(top_header)
            .child(calendar_section)
            .child(filter_row)
            .child(
                h_flex()
                    .items_baseline()
                    .gap_2()
                    .px_2()
                    .pt_2()
                    .border_t_1()
                    .border_color(border)
                    .child(div().text_size(px(12.)).font_semibold().child(header_label)),
            )
            .child(column_header)
            .child(div().flex_1().min_h_0().size_full().child(body_rows));
        if invert_pref {
            body = body.child(
                div()
                    .w_full()
                    .px_2()
                    .pt_1()
                    .text_size(px(10.))
                    .text_color(muted)
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .child(
                        "ⓘ inflation/unemployment use inverted color — change in Settings → Calendar",
                    ),
            );
        }

        div()
            .id("econ-calendar-panel-body")
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _ev, _window, cx| this.mark_focused(cx)),
            )
            .size_full()
            .border_2()
            .border_color(ring_color)
            .child(body)
    }
}

/// Per-row height inside the virtualized currency list. Matches the row's
/// natural height (px_2 padding + text_size 12) and keeps `item_sizes`
/// allocation a single uniform value.
const CUR_ROW_HEIGHT_PX: f32 = 26.0;
/// Max viewport height of the virtualized list. Below this, the popover
/// shrinks to the actual list height so few-item cases don't show a tall
/// empty area.
const CUR_LIST_MAX_H_PX: f32 = 320.0;

/// Build the currency multi-select popover content. Rendered inside the
/// popover's content closure (Context<PopoverState>); panel state is mutated
/// via the captured `WeakEntity<EconomicCalendarPanel>`. The list is
/// virtualized so a long currency universe stays cheap to render, and a
/// small search Input filters the visible rows.
fn render_currency_popover(
    panel: WeakEntity<EconomicCalendarPanel>,
    universe: Rc<Vec<String>>,
    search: Entity<InputState>,
    scroll_handle: VirtualListScrollHandle,
    cx: &mut Context<gpui_component::popover::PopoverState>,
) -> gpui::AnyElement {
    let theme = cx.theme();
    let muted = theme.muted_foreground;
    let border = theme.border;

    // Read current filter state through the panel handle. If the entity is
    // gone (panel closed), render an empty popover; the next click outside
    // will dismiss it.
    let Some(panel_entity) = panel.upgrade() else {
        return v_flex().w(px(240.)).into_any_element();
    };
    let filter_snapshot: Option<HashSet<String>> =
        panel_entity.read(cx).currency_filter.clone();

    let query = search.read(cx).value().to_string();
    let q = query.trim().to_lowercase();
    let filtered: Vec<String> = if q.is_empty() {
        universe.iter().cloned().collect()
    } else {
        universe
            .iter()
            .filter(|c| c.to_lowercase().contains(&q))
            .cloned()
            .collect()
    };

    let header_row = h_flex()
        .px_2()
        .py_1()
        .gap_2()
        .items_center()
        .child(
            div()
                .flex_1()
                .text_size(px(11.))
                .text_color(muted)
                .child("Currency"),
        )
        .child(
            Button::new("cur-all")
                .label("All")
                .xsmall()
                .ghost()
                .on_click({
                    let panel = panel.clone();
                    move |_, _, cx| {
                        if let Some(p) = panel.upgrade() {
                            p.update(cx, |this, cx| {
                                this.currency_filter = None;
                                cx.notify();
                            });
                        }
                    }
                }),
        )
        .child(
            Button::new("cur-none")
                .label("None")
                .xsmall()
                .ghost()
                .on_click({
                    let panel = panel.clone();
                    move |_, _, cx| {
                        if let Some(p) = panel.upgrade() {
                            p.update(cx, |this, cx| {
                                this.currency_filter = Some(HashSet::new());
                                cx.notify();
                            });
                        }
                    }
                }),
        );

    let search_row = div().px_2().py_1().child(Input::new(&search).small());

    let content: gpui::AnyElement = if universe.is_empty() {
        div()
            .px_2()
            .py_2()
            .text_size(px(11.))
            .text_color(muted)
            .child("No currencies in current data.")
            .into_any_element()
    } else if filtered.is_empty() {
        div()
            .px_2()
            .py_2()
            .text_size(px(11.))
            .text_color(muted)
            .child("No matches.")
            .into_any_element()
    } else {
        let item_sizes: Rc<Vec<Size<Pixels>>> = Rc::new(
            (0..filtered.len())
                .map(|_| size(px(0.), px(CUR_ROW_HEIGHT_PX)))
                .collect(),
        );
        let viewport_h = (filtered.len() as f32 * CUR_ROW_HEIGHT_PX)
            .min(CUR_LIST_MAX_H_PX);
        let filtered_rc: Rc<Vec<String>> = Rc::new(filtered);
        let universe_for_rows = universe.clone();
        let entity_for_list = cx.entity();
        let list = v_virtual_list(
            entity_for_list,
            "cur-virtual-list",
            item_sizes,
            {
                let panel = panel.clone();
                let filtered_rc = filtered_rc.clone();
                let universe_for_rows = universe_for_rows.clone();
                let filter_snapshot = filter_snapshot.clone();
                move |_state, visible_range, _window, cx| {
                    let theme = cx.theme();
                    let fg = theme.foreground;
                    let border = theme.border;
                    let accent = theme.accent;
                    let accent_fg = theme.accent_foreground;
                    let hover_bg = theme.accent;
                    visible_range
                        .map(|i| {
                            let c = filtered_rc[i].clone();
                            let checked = match &filter_snapshot {
                                None => true,
                                Some(set) => set.contains(&c),
                            };
                            let id = SharedString::from(format!("cur-row-{c}"));
                            let panel = panel.clone();
                            let universe_for_row = universe_for_rows.clone();
                            let label_for_click = c.clone();
                            div()
                                .id(id)
                                .w_full()
                                .h(px(CUR_ROW_HEIGHT_PX))
                                .px_2()
                                .flex()
                                .items_center()
                                .text_size(px(12.))
                                .text_color(fg)
                                .hover(|s| s.bg(hover_bg))
                                .cursor_pointer()
                                .on_click(move |_, _, cx| {
                                    if let Some(p) = panel.upgrade() {
                                        let label_clone = label_for_click.clone();
                                        let universe_clone = universe_for_row.clone();
                                        p.update(cx, |this, cx| {
                                            this.toggle_currency(
                                                label_clone,
                                                &universe_clone,
                                                cx,
                                            );
                                        });
                                    }
                                })
                                .child(
                                    h_flex()
                                        .gap_2()
                                        .items_center()
                                        .child(
                                            div()
                                                .w(px(14.))
                                                .h(px(14.))
                                                .rounded(px(2.))
                                                .border_1()
                                                .border_color(border)
                                                .when(checked, |s| s.bg(accent))
                                                .when(checked, |s| {
                                                    s.child(
                                                        div()
                                                            .size_full()
                                                            .flex()
                                                            .items_center()
                                                            .justify_center()
                                                            .text_size(px(10.))
                                                            .text_color(accent_fg)
                                                            .child("✓"),
                                                    )
                                                }),
                                        )
                                        .child(SharedString::from(c)),
                                )
                        })
                        .collect::<Vec<_>>()
                }
            },
        )
        .track_scroll(&scroll_handle);
        div()
            .id("cur-virtual-wrap")
            .w_full()
            .h(px(viewport_h))
            .child(list)
            .into_any_element()
    };

    v_flex()
        .w(px(240.))
        .child(header_row)
        .child(div().h(px(1.)).w_full().bg(border))
        .child(search_row)
        .child(div().h(px(1.)).w_full().bg(border))
        .child(content)
        .into_any_element()
}
