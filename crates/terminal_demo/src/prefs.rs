//! Runtime mirror of `persistence::GeneralPrefs`. Two GPUI globals — `UserTz`
//! and `ChartPrefsGlobal` — are written here at startup and updated live by
//! the Settings dialog. Renderers read them on each paint, so a settings
//! change reflects on the next frame without any explicit refresh plumbing.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use chrono::{DateTime, FixedOffset, Local, Offset as _, TimeZone as _};
use chrono_tz::Tz;
use gpui::{App, Global};

use crate::persistence::{self, CalendarPrefs, ChartPrefs, GeneralPrefs, TzPref};

// ---------------------------------------------------------------------------
// Hot-path chart pref readers. Chart paint / state mutators don't carry `cx`,
// so reading the GPUI global from inside them would mean threading `&App`
// through every helper. Instead we mirror the three floats into atomics that
// can be read with a single load — `set_chart_prefs` keeps both in sync.
// ---------------------------------------------------------------------------

// Initialised at startup by `init()`; reading before init returns the default.
static DEFAULT_VIEW: AtomicU32 = AtomicU32::new(0x42700000); // 60.0_f32.to_bits()
static RIGHT_BUFFER: AtomicU32 = AtomicU32::new(0x3ECCCCCD); // 0.40_f32.to_bits()
static Y_PADDING: AtomicU32 = AtomicU32::new(0x3D4CCCCD); // 0.05_f32.to_bits()
static SESSION_MARKERS: AtomicBool = AtomicBool::new(true);
static INVERT_MACRO_COLORS: AtomicBool = AtomicBool::new(true);

fn store_atomic_chart_prefs(p: &ChartPrefs) {
    DEFAULT_VIEW.store(p.default_view.to_bits(), Ordering::Relaxed);
    RIGHT_BUFFER.store(p.right_buffer.to_bits(), Ordering::Relaxed);
    Y_PADDING.store(p.y_padding.to_bits(), Ordering::Relaxed);
    SESSION_MARKERS.store(p.session_markers, Ordering::Relaxed);
}

/// Default visible candle count. Read by `ChartState::new` /
/// `snap_to_latest` / `clamp` etc., none of which carry `&App`.
pub fn chart_default_view() -> f32 {
    f32::from_bits(DEFAULT_VIEW.load(Ordering::Relaxed))
}

/// Right-edge buffer ratio (0..1). Multiplied by `view_size` to compute
/// the empty zone past the live candle in sticky mode.
pub fn chart_right_buffer() -> f32 {
    f32::from_bits(RIGHT_BUFFER.load(Ordering::Relaxed))
}

/// Vertical padding ratio (0..1) applied around the auto-fitted price range
/// in `auto_y_range`.
pub fn chart_y_padding() -> f32 {
    f32::from_bits(Y_PADDING.load(Ordering::Relaxed))
}

/// Whether to render RTH session-boundary markers (vertical dashed lines +
/// Open/Close labels) when the chart is in Extended-session mode. Hot-path
/// read by `paint_main_chart`, which doesn't carry `&App`.
pub fn chart_session_markers() -> bool {
    SESSION_MARKERS.load(Ordering::Relaxed)
}

/// Whether the calendar panel honors the server's per-event `color_direction`
/// — see `CalendarPrefs::invert_macro_colors`. Hot-path read by the calendar
/// row renderer, which doesn't carry `&App`.
pub fn invert_macro_colors() -> bool {
    INVERT_MACRO_COLORS.load(Ordering::Relaxed)
}

/// Active timezone choice. `None` ≡ Auto ≡ OS local — most renderers used
/// `chrono::Local` directly before this global existed, so the no-op case is
/// the default.
#[derive(Clone, Default)]
pub struct UserTz {
    pub iana: Option<Tz>,
}

impl Global for UserTz {}

/// Active chart defaults. Wraps `persistence::ChartPrefs` so we can attach a
/// `Global` impl without giving persistence a GPUI dependency.
#[derive(Clone, Default)]
pub struct ChartPrefsGlobal(pub ChartPrefs);

impl Global for ChartPrefsGlobal {}

/// Active calendar/macro-data display defaults.
#[derive(Clone, Default)]
pub struct CalendarPrefsGlobal(pub CalendarPrefs);

impl Global for CalendarPrefsGlobal {}

/// Read persisted prefs and install both globals. Called once from `lib::init`.
pub fn init(cx: &mut App) {
    let prefs = persistence::load_general_prefs();
    cx.set_global(UserTz {
        iana: tz_from_pref(&prefs.tz),
    });
    store_atomic_chart_prefs(&prefs.chart);
    cx.set_global(ChartPrefsGlobal(prefs.chart));
    INVERT_MACRO_COLORS.store(prefs.calendar.invert_macro_colors, Ordering::Relaxed);
    cx.set_global(CalendarPrefsGlobal(prefs.calendar));
}

/// Snapshot of the in-memory globals as a `GeneralPrefs` for persistence.
pub fn snapshot(cx: &App) -> GeneralPrefs {
    let tz = cx.global::<UserTz>();
    let chart = cx.global::<ChartPrefsGlobal>().0.clone();
    let calendar = cx.global::<CalendarPrefsGlobal>().0.clone();
    GeneralPrefs {
        tz: TzPref {
            iana: tz.iana.map(|t| t.name().to_string()),
        },
        chart,
        calendar,
    }
}

/// Replace the active TZ and persist. The renderers read `UserTz` lazily so
/// the next frame picks up the change.
pub fn set_tz(cx: &mut App, iana: Option<Tz>) {
    cx.set_global(UserTz { iana });
    persist_current(cx);
}

/// Replace the active chart prefs and persist.
pub fn set_chart_prefs(cx: &mut App, prefs: ChartPrefs) {
    store_atomic_chart_prefs(&prefs);
    cx.set_global(ChartPrefsGlobal(prefs));
    persist_current(cx);
}

/// Replace the active calendar prefs and persist. The atomic mirror is
/// updated in lockstep so the next paint of the calendar panel honors the
/// new setting without explicit refresh plumbing.
pub fn set_calendar_prefs(cx: &mut App, prefs: CalendarPrefs) {
    INVERT_MACRO_COLORS.store(prefs.invert_macro_colors, Ordering::Relaxed);
    cx.set_global(CalendarPrefsGlobal(prefs));
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

// ---------------------------------------------------------------------------
// Time conversion helpers — the user-tz-aware replacements for chrono::Local.
// Renderers call these with the current `App` to get a wall-clock offset that
// honours the user's setting.
// ---------------------------------------------------------------------------

/// UTC offset to use for displaying an instant. With `UserTz::None` this is
/// the OS-local offset at that instant (Local's behaviour); with a chosen
/// zone it's that zone's offset, DST-correct.
pub fn offset_for(cx: &App, ms: i64) -> FixedOffset {
    match cx.global::<UserTz>().iana {
        // chrono_tz returns its own `TzOffset`; `.fix()` collapses it to the
        // local-minus-utc `FixedOffset` callers expect (and matches Local's
        // shape exactly, so the no-op replacement is bit-identical).
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

/// "Now" rendered in the active TZ. Used by the bottom-bar clock.
pub fn now_in_user_tz(cx: &App) -> DateTime<FixedOffset> {
    let now_ms = chrono::Utc::now().timestamp_millis();
    offset_for(cx, now_ms)
        .timestamp_millis_opt(now_ms)
        .single()
        .unwrap_or_else(|| {
            // Fallback for unrepresentable instants — shouldn't trigger for "now".
            FixedOffset::east_opt(0)
                .unwrap()
                .timestamp_millis_opt(now_ms)
                .single()
                .unwrap()
        })
}

/// IANA presets surfaced in the Settings → General → Timezone dropdown.
/// Ordering: US-first (most users), then west-to-east across LatAm → Europe →
/// Middle East → Asia → Oceania so the list traces a single sweep around the
/// globe instead of jumping. The label is the city; the current UTC offset
/// gets appended at render time by `tz_display_label`, so DST flips show up
/// without needing to edit this table.
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

/// Convert an IANA name to the label shown on the dropdown trigger and inside
/// the preset menu. City presets get suffixed with the *current* UTC offset
/// (DST-aware, so New York reads "UTC-5" in winter and "UTC-4" in summer);
/// `Etc/GMT±N` zones get rendered as plain "UTC±N".
pub fn tz_display_label(name: &str) -> String {
    if name == "Etc/GMT" || name == "UTC" {
        return "UTC".to_string();
    }
    if let Some(rest) = name.strip_prefix("Etc/GMT") {
        // `Etc/GMT-N` ⇒ UTC+N; `Etc/GMT+N` ⇒ UTC-N. Parse the signed offset
        // and flip it to display.
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

/// "UTC±N" / "UTC±N:30" for the zone's *current* offset. Sub-hour zones
/// (India, Nepal, Newfoundland) keep the minutes component so the label is
/// honest about the actual shift.
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
