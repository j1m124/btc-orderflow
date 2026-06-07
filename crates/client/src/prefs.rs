//! Runtime mirror of `persistence::GeneralPrefs`. GPUI globals — `UserTz`
//! and `ChartPrefsGlobal` — are written here at startup and updated live by
//! the Settings dialog. Renderers read them on each paint.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use chrono::{DateTime, FixedOffset, Local, Offset as _, TimeZone as _};
use chrono_tz::Tz;
use gpui::{App, Global};

use crate::persistence::{self, ChartPrefs, GeneralPrefs, TzPref};

static DEFAULT_VIEW: AtomicU32 = AtomicU32::new(0x42700000); // 60.0_f32.to_bits()
static RIGHT_BUFFER: AtomicU32 = AtomicU32::new(0x3ECCCCCD); // 0.40_f32.to_bits()
static Y_PADDING: AtomicU32 = AtomicU32::new(0x3D4CCCCD); // 0.05_f32.to_bits()
static TRUNCATE_FOOTPRINT_DECIMALS: AtomicBool = AtomicBool::new(false);

fn store_atomic_chart_prefs(p: &ChartPrefs) {
    DEFAULT_VIEW.store(p.default_view.to_bits(), Ordering::Relaxed);
    RIGHT_BUFFER.store(p.right_buffer.to_bits(), Ordering::Relaxed);
    Y_PADDING.store(p.y_padding.to_bits(), Ordering::Relaxed);
    TRUNCATE_FOOTPRINT_DECIMALS.store(p.truncate_footprint_decimals, Ordering::Relaxed);
}

pub fn chart_default_view() -> f32 {
    f32::from_bits(DEFAULT_VIEW.load(Ordering::Relaxed))
}

pub fn chart_right_buffer() -> f32 {
    f32::from_bits(RIGHT_BUFFER.load(Ordering::Relaxed))
}

pub fn chart_y_padding() -> f32 {
    f32::from_bits(Y_PADDING.load(Ordering::Relaxed))
}

/// When true, cell-style labels (footprint cells, Bar Stats rows) render
/// their fractional component truncated away (whole numbers only). Main
/// chart prices and pane-axis indicator readouts never read this — the
/// flag is scoped to per-bar text cells. Field name retained
/// (`truncate_footprint_decimals`) for backward compat with persisted
/// blobs even though the scope is broader.
pub fn footprint_truncate_decimals() -> bool {
    TRUNCATE_FOOTPRINT_DECIMALS.load(Ordering::Relaxed)
}

#[derive(Clone, Default)]
pub struct UserTz {
    pub iana: Option<Tz>,
}

impl Global for UserTz {}

#[derive(Clone, Default)]
pub struct ChartPrefsGlobal(pub ChartPrefs);

impl Global for ChartPrefsGlobal {}

pub fn init(cx: &mut App) {
    let prefs = persistence::load_general_prefs();
    cx.set_global(UserTz {
        iana: tz_from_pref(&prefs.tz),
    });
    store_atomic_chart_prefs(&prefs.chart);
    cx.set_global(ChartPrefsGlobal(prefs.chart));
}

pub fn snapshot(cx: &App) -> GeneralPrefs {
    let tz = cx.global::<UserTz>();
    let chart = cx.global::<ChartPrefsGlobal>().0.clone();
    GeneralPrefs {
        tz: TzPref {
            iana: tz.iana.map(|t| t.name().to_string()),
        },
        chart,
    }
}

pub fn set_tz(cx: &mut App, iana: Option<Tz>) {
    cx.set_global(UserTz { iana });
    persist_current(cx);
}

pub fn set_chart_prefs(cx: &mut App, prefs: ChartPrefs) {
    store_atomic_chart_prefs(&prefs);
    cx.set_global(ChartPrefsGlobal(prefs));
    persist_current(cx);
}

fn persist_current(cx: &App) {
    let value = snapshot(cx);
    if let Err(err) = persistence::save_general_prefs(&value) {
        log::warn!("save general prefs failed: {err:?}");
    }
}

fn tz_from_pref(pref: &TzPref) -> Option<Tz> {
    pref.iana.as_deref().and_then(|name| name.parse().ok())
}

pub fn offset_for(cx: &App, ms: i64) -> FixedOffset {
    match cx.global::<UserTz>().iana {
        Some(tz) => tz
            .timestamp_millis_opt(ms)
            .single()
            .map(|dt| dt.offset().fix())
            .unwrap_or_else(|| FixedOffset::east_opt(0).unwrap()),
        None => Local
            .timestamp_millis_opt(ms)
            .single()
            .map(|dt| *dt.offset())
            .unwrap_or_else(|| FixedOffset::east_opt(0).unwrap()),
    }
}

pub fn now_in_user_tz(cx: &App) -> DateTime<FixedOffset> {
    let now_ms = chrono::Utc::now().timestamp_millis();
    offset_for(cx, now_ms)
        .timestamp_millis_opt(now_ms)
        .single()
        .unwrap_or_else(|| {
            FixedOffset::east_opt(0)
                .unwrap()
                .timestamp_millis_opt(now_ms)
                .single()
                .unwrap()
        })
}

pub const TZ_PRESETS: &[(&str, &str)] = &[
    ("America/New_York", "New York"),
    ("America/Chicago", "Chicago"),
    ("America/Denver", "Denver"),
    ("America/Los_Angeles", "Los Angeles"),
    ("America/Toronto", "Toronto"),
    ("America/Mexico_City", "Mexico City"),
    ("America/Sao_Paulo", "São Paulo"),
    ("America/Buenos_Aires", "Buenos Aires"),
    ("Europe/London", "London"),
    ("Europe/Paris", "Paris"),
    ("Europe/Berlin", "Berlin"),
    ("Europe/Zurich", "Zurich"),
    ("Europe/Madrid", "Madrid"),
    ("Europe/Rome", "Rome"),
    ("Europe/Amsterdam", "Amsterdam"),
    ("Europe/Stockholm", "Stockholm"),
    ("Europe/Istanbul", "Istanbul"),
    ("Europe/Moscow", "Moscow"),
    ("Africa/Cairo", "Cairo"),
    ("Africa/Johannesburg", "Johannesburg"),
    ("Asia/Dubai", "Dubai"),
    ("Asia/Tehran", "Tehran"),
    ("Asia/Kolkata", "Mumbai"),
    ("Asia/Bangkok", "Bangkok"),
    ("Asia/Jakarta", "Jakarta"),
    ("Asia/Singapore", "Singapore"),
    ("Asia/Hong_Kong", "Hong Kong"),
    ("Asia/Shanghai", "Shanghai"),
    ("Asia/Taipei", "Taipei"),
    ("Asia/Seoul", "Seoul"),
    ("Asia/Tokyo", "Tokyo"),
    ("Australia/Sydney", "Sydney"),
    ("Pacific/Auckland", "Auckland"),
];

pub fn tz_display_label(name: &str) -> String {
    if name == "Etc/GMT" || name == "UTC" {
        return "UTC".to_string();
    }
    if let Some(rest) = name.strip_prefix("Etc/GMT") {
        if let Ok(iana_off) = rest.parse::<i32>() {
            let display = -iana_off;
            return if display >= 0 {
                format!("UTC+{display}")
            } else {
                format!("UTC{display}")
            };
        }
    }
    if let Some((_, label)) = TZ_PRESETS.iter().find(|(id, _)| *id == name) {
        if let Ok(tz) = name.parse::<Tz>() {
            return format!("{label} ({})", current_utc_offset_label(tz));
        }
        return (*label).to_string();
    }
    if let Ok(tz) = name.parse::<Tz>() {
        return format!("{name} ({})", current_utc_offset_label(tz));
    }
    name.to_string()
}

pub fn current_utc_offset_label(tz: Tz) -> String {
    let now = chrono::Utc::now().naive_utc();
    let secs = tz.offset_from_utc_datetime(&now).fix().local_minus_utc();
    let abs = secs.abs();
    let hours = abs / 3600;
    let mins = (abs % 3600) / 60;
    let sign = if secs >= 0 { '+' } else { '-' };
    if mins == 0 {
        format!("UTC{sign}{hours}")
    } else {
        format!("UTC{sign}{hours}:{mins:02}")
    }
}
