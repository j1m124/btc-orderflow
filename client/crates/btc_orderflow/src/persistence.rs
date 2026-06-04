use std::collections::BTreeMap;

use anyhow::Result;
use gpui_component::dock::DockAreaState;
use serde::{Deserialize, Serialize, de::DeserializeOwned};

// localStorage IO helpers. The crate is wasm-only so there's no filesystem fallback.

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

const FREE_KEY: &str = "btc_orderflow.mode.free.v3";
const CURRENT_MODE_KEY: &str = "btc_orderflow.current_mode.v3";
const LAYOUTS_KEY: &str = "btc_orderflow.layouts.v3";
const WATCHLIST_KEY: &str = "btc_orderflow.watchlist.v3";
const RECENTS_KEY: &str = "btc_orderflow.recents.v3";
const DRAWINGS_KEY: &str = "btc_orderflow.drawings.v3";
const FONT_SIZE_KEY: &str = "btc_orderflow.font_size.v3";
const THEME_KEY: &str = "btc_orderflow.theme.v3";
const DIALOG_ANIM_KEY: &str = "btc_orderflow.dialog_animations.v3";

pub type SavedLayouts = BTreeMap<String, DockAreaState>;

/// One-time migration: drop any legacy keys from the pre-fork ancestor.
pub fn purge_v1() {
    for key in [
        "terminal_demo.layout.v1",
        "terminal_demo.layouts.v1",
        "terminal_demo.current_layout.v1",
        "terminal_demo.mode.charting.v2",
        "terminal_demo.mode.signal.v2",
        "terminal_demo.mode.research.v2",
        "terminal_demo.mode.portfolio.v2",
        "terminal_demo.mode.free.v2",
        "terminal_demo.current_mode.v2",
        "terminal_demo.layouts.v2",
        "terminal_demo.watchlist.v2",
        "terminal_demo.ai_chat.v1",
        "btc_orderflow.recents.v1",
        "terminal_demo.drawings.v1",
        "terminal_demo.font_size.v1",
        "terminal_demo.theme.v1",
        "terminal_demo.dialog_animations.v1",
        "terminal_demo.general_prefs.v1",
        "centoflow.auth.v1",
    ] {
        let _ = remove_storage_blob(key);
    }
}

/// Workspace mode. Reduced to a single FreeLayout variant after the fork;
/// the enum stays so the UI code keeps its shape.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    FreeLayout,
}

impl Default for Mode {
    fn default() -> Self {
        Self::FreeLayout
    }
}

impl Mode {
    pub const ALL: &'static [Mode] = &[Mode::FreeLayout];

    pub fn id(self) -> &'static str {
        match self {
            Mode::FreeLayout => "free_layout",
        }
    }

    pub fn display(self) -> &'static str {
        match self {
            Mode::FreeLayout => "Free Layout",
        }
    }

    pub fn from_id(id: &str) -> Option<Mode> {
        Self::ALL.iter().copied().find(|m| m.id() == id)
    }

    fn key(self) -> &'static str {
        match self {
            Mode::FreeLayout => FREE_KEY,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ModeState {
    #[serde(default)]
    pub dock: Option<DockAreaState>,
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

pub fn load_watchlist() -> Option<Vec<gpui::SharedString>> {
    let raw: Option<Vec<String>> = load_json_opt(WATCHLIST_KEY);
    raw.map(|v| v.into_iter().map(gpui::SharedString::from).collect())
}

pub fn save_watchlist(symbols: &[gpui::SharedString]) -> Result<()> {
    let raw: Vec<&str> = symbols.iter().map(|s| s.as_ref()).collect();
    save_json(WATCHLIST_KEY, &raw)
}

pub fn load_recents() -> Vec<gpui::SharedString> {
    let raw: Vec<String> = load_json(RECENTS_KEY);
    raw.into_iter().map(gpui::SharedString::from).collect()
}

pub fn save_recents(tickers: &[gpui::SharedString]) -> Result<()> {
    let raw: Vec<&str> = tickers.iter().map(|s| s.as_ref()).collect();
    save_json(RECENTS_KEY, &raw)
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PersistedDrawingsDoc {
    #[serde(default)]
    pub next_id: u64,
    #[serde(default)]
    pub by_symbol: BTreeMap<String, Vec<crate::drawings::shapes::Drawing>>,
}

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
    let next_id = root.get("next_id").and_then(|v| v.as_u64()).unwrap_or(0);
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

const GENERAL_PREFS_KEY: &str = "btc_orderflow.general_prefs.v3";

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TzPref {
    #[serde(default)]
    pub iana: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ChartPrefs {
    pub default_view: f32,
    pub right_buffer: f32,
    pub y_padding: f32,
    #[serde(default = "default_session_markers")]
    pub session_markers: bool,
}

fn default_session_markers() -> bool {
    true
}

impl Default for ChartPrefs {
    fn default() -> Self {
        Self {
            default_view: 60.0,
            right_buffer: 0.40,
            y_padding: 0.05,
            session_markers: true,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct GeneralPrefs {
    #[serde(default)]
    pub tz: TzPref,
    #[serde(default)]
    pub chart: ChartPrefs,
}

pub fn load_general_prefs() -> GeneralPrefs {
    load_json(GENERAL_PREFS_KEY)
}

pub fn save_general_prefs(value: &GeneralPrefs) -> Result<()> {
    save_json(GENERAL_PREFS_KEY, value)
}
