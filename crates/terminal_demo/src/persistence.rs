use std::collections::BTreeMap;

use anyhow::Result;
use gpui_component::dock::DockAreaState;
use serde::{Deserialize, Serialize, de::DeserializeOwned};

// ---------------------------------------------------------------------------
// Generic JSON helpers
// ---------------------------------------------------------------------------
//
// Each persistence entry is a single JSON blob keyed by a stable string. All
// storage goes through `web_sys::window().local_storage()` — the crate only
// targets wasm so there's no longer a filesystem fallback.

fn read_storage_blob(key: &str) -> Option<String> {
    web_sys::window()?.local_storage().ok()??.get_item(key).ok()?
}

fn write_storage_blob(key: &str, value: &str) -> Result<()> {
    let window = web_sys::window().ok_or_else(|| anyhow::anyhow!("no window"))?;
    let storage = window
        .local_storage()
        .map_err(|_| anyhow::anyhow!("localStorage unavailable"))?
        .ok_or_else(|| anyhow::anyhow!("localStorage unavailable"))?;
    storage
        .set_item(key, value)
        .map_err(|_| anyhow::anyhow!("localStorage write failed"))?;
    Ok(())
}

fn remove_storage_blob(key: &str) -> Result<()> {
    let window = web_sys::window().ok_or_else(|| anyhow::anyhow!("no window"))?;
    let storage = window
        .local_storage()
        .map_err(|_| anyhow::anyhow!("localStorage unavailable"))?
        .ok_or_else(|| anyhow::anyhow!("localStorage unavailable"))?;
    storage
        .remove_item(key)
        .map_err(|_| anyhow::anyhow!("localStorage remove failed"))?;
    Ok(())
}

fn load_json<T: DeserializeOwned + Default>(key: &str) -> T {
    read_storage_blob(key)
        .as_deref()
        .and_then(|json| serde_json::from_str(json).ok())
        .unwrap_or_default()
}

fn save_json<T: Serialize>(key: &str, value: &T) -> Result<()> {
    let json = serde_json::to_string(value)?;
    write_storage_blob(key, &json)
}

fn load_json_opt<T: DeserializeOwned>(key: &str) -> Option<T> {
    read_storage_blob(key)
        .as_deref()
        .and_then(|json| serde_json::from_str(json).ok())
}

fn clear_key(key: &str) -> Result<()> {
    remove_storage_blob(key)
}

// localStorage keys. v2 schema is mode-based; v1 keys are listed only so we
// can purge them on first run after upgrade.
const CHARTING_KEY: &str = "terminal_demo.mode.charting.v2";
const SIGNAL_KEY: &str = "terminal_demo.mode.signal.v2";
const RESEARCH_KEY: &str = "terminal_demo.mode.research.v2";
const PORTFOLIO_KEY: &str = "terminal_demo.mode.portfolio.v2";
const FREE_KEY: &str = "terminal_demo.mode.free.v2";
const CURRENT_MODE_KEY: &str = "terminal_demo.current_mode.v2";
const LAYOUTS_KEY: &str = "terminal_demo.layouts.v2";
const WATCHLIST_KEY: &str = "terminal_demo.watchlist.v2";
const AI_CHAT_KEY: &str = "terminal_demo.ai_chat.v1";
const RECENTS_KEY: &str = "terminal_demo.recents.v1";
const DRAWINGS_KEY: &str = "terminal_demo.drawings.v1";
const FONT_SIZE_KEY: &str = "terminal_demo.font_size.v1";
const THEME_KEY: &str = "terminal_demo.theme.v1";
const DIALOG_ANIM_KEY: &str = "terminal_demo.dialog_animations.v1";

/// Centoflow auth blob. A hosted login page can write the JWT here (same-origin
/// localStorage) before navigating into the app; the app reads it at startup.
const AUTH_KEY: &str = "centoflow.auth.v1";

/// Map of user-named layouts. BTreeMap so the menu shows them in stable
/// alphabetical order without an extra sort step.
pub type SavedLayouts = BTreeMap<String, DockAreaState>;

// ============================================================================
// One-time v1 migration: drop the old keys/files. Called from `init` so users
// upgrading don't carry around stale state forever.
// ============================================================================

pub fn purge_v1() {
    let _ = remove_storage_blob("terminal_demo.layout.v1");
    let _ = remove_storage_blob("terminal_demo.layouts.v1");
    let _ = remove_storage_blob("terminal_demo.current_layout.v1");
}

// ============================================================================
// Per-mode workspace state
// ============================================================================

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Charting,
    Signal,
    Research,
    Portfolio,
    FreeLayout,
}

impl Default for Mode {
    fn default() -> Self {
        Self::Charting
    }
}

impl Mode {
    pub const ALL: &'static [Mode] = &[
        Mode::Charting,
        Mode::Signal,
        Mode::Research,
        Mode::Portfolio,
        Mode::FreeLayout,
    ];

    pub fn id(self) -> &'static str {
        match self {
            Mode::Charting => "charting",
            Mode::Signal => "signal",
            Mode::Research => "research",
            Mode::Portfolio => "portfolio",
            Mode::FreeLayout => "free_layout",
        }
    }

    pub fn display(self) -> &'static str {
        match self {
            Mode::Charting => "Charting",
            Mode::Signal => "Signal",
            Mode::Research => "Research",
            Mode::Portfolio => "Portfolio",
            Mode::FreeLayout => "Free Layout",
        }
    }

    pub fn from_id(id: &str) -> Option<Mode> {
        Self::ALL.iter().copied().find(|m| m.id() == id)
    }

    fn key(self) -> &'static str {
        match self {
            Mode::Charting => CHARTING_KEY,
            Mode::Signal => SIGNAL_KEY,
            Mode::Research => RESEARCH_KEY,
            Mode::Portfolio => PORTFOLIO_KEY,
            Mode::FreeLayout => FREE_KEY,
        }
    }
}

/// Per-mode persisted state. `dock` is the mode's serialized DockArea; the
/// toggle flags capture mode-scoped UI choices (Trading per-mode, Details
/// shown only in Charting). AI Chat is global and lives on `CurrentMode`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ModeState {
    #[serde(default)]
    pub dock: Option<DockAreaState>,
    #[serde(default)]
    pub trading_open: bool,
    #[serde(default)]
    pub details_open: bool,
    #[serde(default)]
    pub chart_layout: ChartLayout,
}

/// Predefined chart-workspace arrangements for Charting mode. Selecting one
/// rebuilds just the chart side of the dock; the locked watchlist column
/// stays intact.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChartLayout {
    One,
    TwoStacked,
    TwoSideBySide,
    TwoByTwo,
}

impl Default for ChartLayout {
    fn default() -> Self {
        Self::One
    }
}

impl ChartLayout {
    pub const ALL: &'static [ChartLayout] = &[
        ChartLayout::One,
        ChartLayout::TwoStacked,
        ChartLayout::TwoSideBySide,
        ChartLayout::TwoByTwo,
    ];

    pub fn id(self) -> &'static str {
        match self {
            ChartLayout::One => "one",
            ChartLayout::TwoStacked => "two_stacked",
            ChartLayout::TwoSideBySide => "two_side",
            ChartLayout::TwoByTwo => "two_by_two",
        }
    }

    pub fn display(self) -> &'static str {
        match self {
            ChartLayout::One => "1 chart",
            ChartLayout::TwoStacked => "2 stacked",
            ChartLayout::TwoSideBySide => "2 side by side",
            ChartLayout::TwoByTwo => "2 × 2 grid",
        }
    }

    pub fn from_id(id: &str) -> Option<ChartLayout> {
        Self::ALL.iter().copied().find(|l| l.id() == id)
    }
}

pub fn load_mode_state(mode: Mode) -> Option<ModeState> {
    load_json_opt(mode.key())
}

pub fn save_mode_state(mode: Mode, state: &ModeState) -> Result<()> {
    save_json(mode.key(), state)
}

pub fn clear_mode_state(mode: Mode) -> Result<()> {
    clear_key(mode.key())
}

/// Persisted top-level mode. AI Chat open/closed is intentionally not
/// persisted — the panel always starts closed on load and on mode switch.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CurrentMode {
    #[serde(default)]
    pub mode: Mode,
}

pub fn load_current_mode() -> CurrentMode {
    load_json(CURRENT_MODE_KEY)
}

pub fn save_current_mode(value: &CurrentMode) -> Result<()> {
    save_json(CURRENT_MODE_KEY, value)
}

// ============================================================================
// Font size (unchanged, still v1)
// ============================================================================

pub fn load_font_size() -> Option<f32> {
    let storage = web_sys::window()?.local_storage().ok()??;
    let raw = storage.get_item(FONT_SIZE_KEY).ok()??;
    raw.parse().ok()
}

pub fn save_font_size(value: f32) -> Result<()> {
    let window = web_sys::window().ok_or_else(|| anyhow::anyhow!("no window"))?;
    let storage = window
        .local_storage()
        .map_err(|_| anyhow::anyhow!("localStorage unavailable"))?
        .ok_or_else(|| anyhow::anyhow!("localStorage unavailable"))?;
    storage
        .set_item(FONT_SIZE_KEY, &value.to_string())
        .map_err(|_| anyhow::anyhow!("localStorage write failed"))?;
    Ok(())
}

// ============================================================================
// Theme name. Single string; same storage flavour as font_size.
// ============================================================================

pub fn load_theme_name() -> Option<String> {
    let storage = web_sys::window()?.local_storage().ok()??;
    storage.get_item(THEME_KEY).ok().flatten()
}

pub fn save_theme_name(name: &str) -> Result<()> {
    let window = web_sys::window().ok_or_else(|| anyhow::anyhow!("no window"))?;
    let storage = window
        .local_storage()
        .map_err(|_| anyhow::anyhow!("localStorage unavailable"))?
        .ok_or_else(|| anyhow::anyhow!("localStorage unavailable"))?;
    storage
        .set_item(THEME_KEY, name)
        .map_err(|_| anyhow::anyhow!("localStorage write failed"))?;
    Ok(())
}

// ============================================================================
// Dialog animations on/off — single bool, same storage flavour as font_size.
// ============================================================================

pub fn load_dialog_animations() -> Option<bool> {
    let storage = web_sys::window()?.local_storage().ok()??;
    let raw = storage.get_item(DIALOG_ANIM_KEY).ok()??;
    match raw.as_str() {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

pub fn save_dialog_animations(enabled: bool) -> Result<()> {
    let window = web_sys::window().ok_or_else(|| anyhow::anyhow!("no window"))?;
    let storage = window
        .local_storage()
        .map_err(|_| anyhow::anyhow!("localStorage unavailable"))?
        .ok_or_else(|| anyhow::anyhow!("localStorage unavailable"))?;
    storage
        .set_item(DIALOG_ANIM_KEY, if enabled { "true" } else { "false" })
        .map_err(|_| anyhow::anyhow!("localStorage write failed"))?;
    Ok(())
}

// ============================================================================
// Named user layouts (Free Layout only)
// ============================================================================

pub fn load_layouts() -> SavedLayouts {
    load_json(LAYOUTS_KEY)
}

pub fn save_layouts(layouts: &SavedLayouts) -> Result<()> {
    save_json(LAYOUTS_KEY, layouts)
}

pub fn upsert_layout(name: &str, state: DockAreaState) -> Result<()> {
    let mut layouts = load_layouts();
    layouts.insert(name.to_string(), state);
    save_layouts(&layouts)
}

pub fn delete_layout(name: &str) -> Result<()> {
    let mut layouts = load_layouts();
    if layouts.remove(name).is_some() {
        save_layouts(&layouts)?;
    }
    Ok(())
}

// ============================================================================
// User-managed watchlist (list of tickers)
// ============================================================================

pub fn load_watchlist() -> Option<Vec<gpui::SharedString>> {
    let raw: Option<Vec<String>> = load_json_opt(WATCHLIST_KEY);
    raw.map(|v| v.into_iter().map(gpui::SharedString::from).collect())
}

pub fn save_watchlist(symbols: &[gpui::SharedString]) -> Result<()> {
    let raw: Vec<&str> = symbols.iter().map(|s| s.as_ref()).collect();
    save_json(WATCHLIST_KEY, &raw)
}

// ============================================================================
// AI chat state — sessions, selection, last-used model. The wire schema lives
// in `services::ai_chat`; persistence stays generic so it doesn't need to
// learn the model.
// ============================================================================

pub fn load_ai_chat<T: DeserializeOwned>() -> Option<T> {
    load_json_opt(AI_CHAT_KEY)
}

pub fn save_ai_chat<T: Serialize>(value: &T) -> Result<()> {
    save_json(AI_CHAT_KEY, value)
}

// ============================================================================
// Recent symbols (shared symbol picker)
// ============================================================================
//
// Stored on its own key so a LAYOUT_VERSION bump (or mode-state wipe) doesn't
// erase the user's pick history.

pub fn load_recents() -> Vec<gpui::SharedString> {
    let raw: Vec<String> = load_json(RECENTS_KEY);
    raw.into_iter().map(gpui::SharedString::from).collect()
}

pub fn save_recents(tickers: &[gpui::SharedString]) -> Result<()> {
    let raw: Vec<&str> = tickers.iter().map(|s| s.as_ref()).collect();
    save_json(RECENTS_KEY, &raw)
}

// ============================================================================
// Drawings (chart annotations). Independent of layout so a mode-state wipe
// doesn't lose user trendlines / boxes / positions.
// ============================================================================

/// Top-level persisted document. `next_id` is the monotonic source carried
/// across reloads; `by_symbol` is the per-ticker drawing list.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PersistedDrawingsDoc {
    #[serde(default)]
    pub next_id: u64,
    #[serde(default)]
    pub by_symbol: BTreeMap<String, Vec<crate::drawings::shapes::Drawing>>,
}

/// Forward-compatible load: we deserialize into a `serde_json::Value` first so
/// any shape variants from a newer binary (e.g. `fib_retracement`) are
/// skipped with a warning instead of failing the whole load.
pub fn load_drawings() -> PersistedDrawingsDoc {
    let raw: Option<String> = read_drawings_blob();
    let Some(raw) = raw else {
        return PersistedDrawingsDoc::default();
    };
    let root: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(err) => {
            log::warn!("drawings: failed to parse blob, starting empty: {err:?}");
            return PersistedDrawingsDoc::default();
        }
    };
    let next_id = root
        .get("next_id")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let mut by_symbol: BTreeMap<String, Vec<crate::drawings::shapes::Drawing>> = BTreeMap::new();
    if let Some(map) = root.get("by_symbol").and_then(|v| v.as_object()) {
        for (sym, arr) in map {
            let Some(arr) = arr.as_array() else {
                continue;
            };
            let mut out = Vec::with_capacity(arr.len());
            for item in arr {
                match serde_json::from_value::<crate::drawings::shapes::Drawing>(item.clone()) {
                    Ok(d) => out.push(d),
                    Err(err) => {
                        log::warn!(
                            "drawings: skipping unknown/invalid drawing on {sym}: {err:?}"
                        );
                    }
                }
            }
            if !out.is_empty() {
                by_symbol.insert(sym.clone(), out);
            }
        }
    }
    PersistedDrawingsDoc { next_id, by_symbol }
}

pub fn save_drawings(doc: &PersistedDrawingsDoc) -> Result<()> {
    save_json(DRAWINGS_KEY, doc)
}

fn read_drawings_blob() -> Option<String> {
    read_storage_blob(DRAWINGS_KEY)
}

// ============================================================================
// Centoflow auth (JWT)
// ============================================================================

/// Persisted auth state for the centoflow market-data server. `token` is the
/// JWT bearer; `None` means signed out (fall back to any compile-time default).
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct AuthConfig {
    #[serde(default)]
    pub token: Option<String>,
}

pub fn load_auth() -> AuthConfig {
    load_json(AUTH_KEY)
}

pub fn save_auth(value: &AuthConfig) -> Result<()> {
    save_json(AUTH_KEY, value)
}

// ============================================================================
// General preferences (timezone + chart defaults + planned placeholders).
// One JSON blob so adding new general settings doesn't require a new key per
// field. Bumping `v1` is the migration story when the schema becomes lossy.
// ============================================================================

const GENERAL_PREFS_KEY: &str = "terminal_demo.general_prefs.v1";

/// Timezone selection. `None` (the default) means "use the OS-local zone" —
/// matches pre-existing behaviour. A `Some` value carries an IANA name like
/// `"America/New_York"` which the UI translates to `chrono_tz::Tz`.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TzPref {
    #[serde(default)]
    pub iana: Option<String>,
}

/// Per-user chart defaults. These replace the previously-hardcoded constants
/// (`CHART_DEFAULT_VIEW`, `CHART_RIGHT_BUFFER_RATIO`, the 0.05 vertical pad in
/// `auto_y_range`). All fields are bounded by the UI sliders so an out-of-range
/// value coming back from a stale config doesn't break the chart.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ChartPrefs {
    /// Default visible candle count. 10..=500.
    pub default_view: f32,
    /// Empty space to the right of the live candle in sticky mode, as a
    /// fraction of `view_size`. 0.0..=0.8.
    pub right_buffer: f32,
    /// Vertical padding around the auto-fitted price range, as a fraction of
    /// (hi-lo). 0.0..=0.25.
    pub y_padding: f32,
    /// Render RTH-open/close vertical markers in Extended-session mode. On by
    /// default; users can hide them from Settings → General → Chart. Optional
    /// so layouts saved before this field was added deserialize cleanly.
    #[serde(default = "default_session_markers")]
    pub session_markers: bool,
}

fn default_session_markers() -> bool {
    true
}

impl Default for ChartPrefs {
    fn default() -> Self {
        // These mirror the original hardcoded constants so a user with no
        // saved prefs gets the exact pre-feature chart behaviour.
        Self {
            default_view: 60.0,
            right_buffer: 0.40,
            y_padding: 0.05,
            session_markers: true,
        }
    }
}

/// Per-user calendar/macro-data display defaults.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct CalendarPrefs {
    /// When ON, the calendar panel honors the server's `color_direction` field
    /// — inflation and unemployment prints render with inverted color (cooler
    /// than forecast = green). When OFF, every event uses the naive
    /// `actual >= forecast → green` rule, which is correct for growth
    /// indicators (NFP, GDP) but misleading for inflation/unemployment.
    ///
    /// Default: ON. Correctness for macro events outweighs the surprise
    /// factor for users seeing the inversion for the first time — a small
    /// `(i)` tooltip in the panel header explains the behavior.
    #[serde(default = "default_invert_macro_colors")]
    pub invert_macro_colors: bool,
}

fn default_invert_macro_colors() -> bool {
    true
}

impl Default for CalendarPrefs {
    fn default() -> Self {
        Self {
            invert_macro_colors: true,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct GeneralPrefs {
    #[serde(default)]
    pub tz: TzPref,
    #[serde(default)]
    pub chart: ChartPrefs,
    #[serde(default)]
    pub calendar: CalendarPrefs,
}

pub fn load_general_prefs() -> GeneralPrefs {
    load_json(GENERAL_PREFS_KEY)
}

pub fn save_general_prefs(value: &GeneralPrefs) -> Result<()> {
    save_json(GENERAL_PREFS_KEY, value)
}
