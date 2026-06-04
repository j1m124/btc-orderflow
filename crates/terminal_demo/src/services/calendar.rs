//! Economic-calendar service. Fetches `GET /v1/calendar` from the centoflow
//! server and exposes loading/error/data state via a gpui `Entity`. The
//! calendar panel subscribes and renders whatever state is current.
//!
//! Unlike `symbols.rs` (one-shot at startup, retried until it succeeds), this
//! service fetches on-demand — the panel triggers `reload()` when it mounts
//! and when the user clicks the refresh button. The server-side TTL cache
//! (60s) absorbs the cost of frequent reloads, so we don't add a client-side
//! debounce.

use std::time::Duration;

use chrono::{DateTime, TimeZone as _, Utc};
use gpui::{App, AppContext as _, Context, Entity, EventEmitter, Global, Task};
use serde::Deserialize;

use crate::net::{CentoflowConfig, HttpClient};

/// One macro event row, with vendor numerics already coerced and times
/// pre-parsed to chrono UTC so the renderer doesn't have to.
#[derive(Clone, Debug)]
pub struct CalendarEvent {
    pub country: String,
    pub event_name: String,
    pub event_time: DateTime<Utc>,
    /// "high" | "medium" | "low" | "holiday" | "" (server normalizes case).
    pub impact: String,
    pub actual: Option<f64>,
    pub forecast: Option<f64>,
    pub previous: Option<f64>,
    pub unit: Option<String>,
    /// Server tag. `"inverted"` means a cooler-than-forecast print is
    /// bullish (CPI, unemployment); `"normal"` means the naive
    /// `actual ≥ forecast → green` rule applies. The panel honors this
    /// only when `prefs::invert_macro_colors()` is true.
    pub color_direction: ColorDirection,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorDirection {
    Normal,
    Inverted,
}

impl ColorDirection {
    fn from_wire(s: &str) -> Self {
        match s {
            "inverted" => Self::Inverted,
            _ => Self::Normal,
        }
    }
}

/// Snapshot of the service state for the panel renderer.
#[derive(Clone, Debug)]
pub enum CalendarState {
    /// First load has not started yet.
    Idle,
    /// A fetch is in flight; no data yet (or stale data from a previous load).
    Loading,
    /// Last fetch succeeded.
    Loaded {
        events: Vec<CalendarEvent>,
        fetched_at: DateTime<Utc>,
    },
    /// Last fetch failed. `events` from any previous successful load are kept
    /// so the panel can keep rendering them while showing an error banner.
    Error {
        message: String,
        last_events: Option<Vec<CalendarEvent>>,
        last_fetched_at: Option<DateTime<Utc>>,
    },
}

#[derive(Clone, Debug)]
pub enum CalendarEvent_ {
    /// State transitioned (Loading → Loaded/Error, or refresh kicked off).
    Changed,
}

pub struct CalendarService {
    state: CalendarState,
    inflight: Option<Task<()>>,
}

impl EventEmitter<CalendarEvent_> for CalendarService {}

impl CalendarService {
    pub fn new(cx: &mut Context<Self>) -> Self {
        // Defer the initial fetch by one tick. Calling reload directly here
        // would put the service into Loading + start a fetch before any
        // panel exists; if a panel then mounts and reads state while the
        // fetch-completion update closure is running, gpui's entity slab
        // panics with "already borrowed". Spawn ensures the first reload
        // runs after construction settles.
        cx.spawn(async move |this, cx| {
            let _ = this.update(cx, |s, cx| s.reload(cx));
        })
        .detach();
        Self {
            state: CalendarState::Idle,
            inflight: None,
        }
    }

    pub fn state(&self) -> &CalendarState {
        &self.state
    }

    /// Trigger a fetch. If one is already in flight, do nothing — the server
    /// cache makes a coalescing client-side guard sufficient.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        if self.inflight.is_some() {
            return;
        }
        let client = cx.global::<HttpClient>().0.clone();
        let cfg = cx.global::<CentoflowConfig>().clone();

        // Preserve last-good data so an error doesn't blank the panel.
        let (last_events, last_fetched_at) = match &self.state {
            CalendarState::Loaded { events, fetched_at } => {
                (Some(events.clone()), Some(*fetched_at))
            }
            CalendarState::Error {
                last_events,
                last_fetched_at,
                ..
            } => (last_events.clone(), *last_fetched_at),
            _ => (None, None),
        };

        self.state = CalendarState::Loading;
        cx.emit(CalendarEvent_::Changed);
        cx.notify();

        let task = cx.spawn(async move |this, cx| {
            let result = fetch(&client, &cfg).await;
            let _ = this.update(cx, |s, cx| {
                s.inflight = None;
                match result {
                    Ok((events, fetched_at)) => {
                        s.state = CalendarState::Loaded { events, fetched_at };
                    }
                    Err(err) => {
                        s.state = CalendarState::Error {
                            message: format!("{err:#}"),
                            last_events: last_events.clone(),
                            last_fetched_at,
                        };
                    }
                }
                cx.emit(CalendarEvent_::Changed);
                cx.notify();
            });
        });
        self.inflight = Some(task);
    }
}

#[derive(Clone)]
pub struct CalendarServiceHandle(pub Entity<CalendarService>);
impl Global for CalendarServiceHandle {}

pub fn init(cx: &mut App) {
    let entity = cx.new(CalendarService::new);
    cx.set_global(CalendarServiceHandle(entity));
}

// ---------------------------------------------------------------------------
// Wire types — keep in sync with internal/api/calendar.go::CalendarEvent.
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct CalendarResponse {
    #[serde(default)]
    events: Vec<RawEvent>,
    #[serde(default)]
    fetched_at: i64,
}

#[derive(Debug, Deserialize)]
struct RawEvent {
    #[serde(default)]
    country: String,
    #[serde(default)]
    event: String,
    #[serde(default)]
    event_time: i64,
    #[serde(default)]
    impact: String,
    #[serde(default)]
    actual: Option<f64>,
    #[serde(default)]
    forecast: Option<f64>,
    #[serde(default)]
    previous: Option<f64>,
    #[serde(default)]
    unit: Option<String>,
    #[serde(default)]
    color_direction: String,
}

async fn fetch(
    client: &reqwest::Client,
    cfg: &CentoflowConfig,
) -> anyhow::Result<(Vec<CalendarEvent>, DateTime<Utc>)> {
    let url = format!("{}/v1/calendar", cfg.base_url);
    let mut req = client.get(&url);
    if let Some(token) = &cfg.token {
        req = req.bearer_auth(token);
    }
    // Loose client-side cap on wait time; the server's own timeout is 15s.
    let resp = req
        .timeout(Duration::from_secs(20))
        .send()
        .await?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("centoflow /v1/calendar returned HTTP {status}: {body}");
    }
    let parsed: CalendarResponse = resp.json().await?;
    let fetched_at = Utc
        .timestamp_millis_opt(parsed.fetched_at)
        .single()
        .unwrap_or_else(Utc::now);
    let events = parsed
        .events
        .into_iter()
        .filter_map(|e| {
            let t = Utc.timestamp_millis_opt(e.event_time).single()?;
            Some(CalendarEvent {
                country: e.country,
                event_name: e.event,
                event_time: t,
                impact: e.impact,
                actual: e.actual,
                forecast: e.forecast,
                previous: e.previous,
                unit: e.unit,
                color_direction: ColorDirection::from_wire(&e.color_direction),
            })
        })
        .collect();
    Ok((events, fetched_at))
}
