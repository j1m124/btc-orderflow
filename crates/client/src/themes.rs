//! User-selectable colour themes.
//!
//! gpui-component already ships a `ThemeRegistry` plus a JSON theme schema
//! (`ThemeSet` / `ThemeConfig`). This module:
//!
//! * embeds an extra `themes.json` bundle so the registry knows about more
//!   than just "Default Light" / "Default Dark";
//! * exposes a flat list of `(name, mode)` pairs for the settings UI;
//! * provides [`apply_theme_by_name`] which swaps the right slot on the
//!   global `Theme` and triggers a window refresh.
//!
//! Per design Q56 the user sees a single dropdown of themes — mode is bundled
//! into the theme entry itself ("Default Dark" / "Solarized Dark" / ...), so
//! there's no separate light/dark toggle.

use gpui::{App, Hsla, SharedString, Window};
use gpui_component::{Theme, ThemeMode, ThemeRegistry, try_parse_color};

/// Our own hand-tuned themes (Solarized Dark / Dracula / Nord), bundled
/// because they have chart_bullish/bearish colors picked to read well on
/// candlestick charts. Loaded *first* so they win the name-collision check
/// inside `load_themes_from_str` (vendored solarized.json would otherwise
/// overwrite Solarized Dark).
const CUSTOM_THEMES: &str = include_str!("themes/themes.json");

/// All theme JSON files vendored with our gpui-component fork. Loaded
/// after the custom set so duplicates are skipped, but the rest of the
/// bundles (Catppuccin, Tokyo Night, Gruvbox, etc.) come along for free.
/// Each entry is one `ThemeSet` — most contain a light + dark pair, a
/// couple ship a single theme.
const VENDORED_THEMES: &[&str] = &[
    include_str!("../../../vendor/gpui-component/themes/adventure.json"),
    include_str!("../../../vendor/gpui-component/themes/alduin.json"),
    include_str!("../../../vendor/gpui-component/themes/asciinema.json"),
    include_str!("../../../vendor/gpui-component/themes/ayu.json"),
    include_str!("../../../vendor/gpui-component/themes/catppuccin.json"),
    include_str!("../../../vendor/gpui-component/themes/everforest.json"),
    include_str!("../../../vendor/gpui-component/themes/fahrenheit.json"),
    include_str!("../../../vendor/gpui-component/themes/flexoki.json"),
    include_str!("../../../vendor/gpui-component/themes/gruvbox.json"),
    include_str!("../../../vendor/gpui-component/themes/harper.json"),
    include_str!("../../../vendor/gpui-component/themes/hybrid.json"),
    include_str!("../../../vendor/gpui-component/themes/jellybeans.json"),
    include_str!("../../../vendor/gpui-component/themes/kibble.json"),
    include_str!("../../../vendor/gpui-component/themes/macos-classic.json"),
    include_str!("../../../vendor/gpui-component/themes/matrix.json"),
    include_str!("../../../vendor/gpui-component/themes/mellifluous.json"),
    include_str!("../../../vendor/gpui-component/themes/molokai.json"),
    include_str!("../../../vendor/gpui-component/themes/solarized.json"),
    include_str!("../../../vendor/gpui-component/themes/spaceduck.json"),
    include_str!("../../../vendor/gpui-component/themes/tokyonight.json"),
    include_str!("../../../vendor/gpui-component/themes/twilight.json"),
];

/// Fallback theme name used when persistence is empty or points at a theme
/// no longer in the registry.
pub const DEFAULT_THEME_NAME: &str = "Default Dark";

/// Register every bundled theme with the global [`ThemeRegistry`]. Custom
/// trading-tuned themes load first; vendored bundles fill in the rest.
/// `load_themes_from_str` skips name-duplicates, so the first registration
/// of any given name wins.
pub fn init(cx: &mut App) {
    let registry = ThemeRegistry::global_mut(cx);
    if let Err(err) = registry.load_themes_from_str(CUSTOM_THEMES) {
        log::warn!("failed to load custom themes: {err:?}");
    }
    for (idx, json) in VENDORED_THEMES.iter().enumerate() {
        if let Err(err) = registry.load_themes_from_str(json) {
            log::warn!("failed to load vendored theme bundle #{idx}: {err:?}");
        }
    }
}

/// Sorted list of `(name, mode)` for every registered theme. Used by the
/// settings dropdown to build its menu items. Sort order follows
/// `ThemeRegistry::sorted_themes` (defaults first, then light before dark,
/// then alphabetical by name).
pub fn available_themes(cx: &App) -> Vec<(SharedString, ThemeMode)> {
    ThemeRegistry::global(cx)
        .sorted_themes()
        .into_iter()
        .map(|cfg| (cfg.name.clone(), cfg.mode))
        .collect()
}

/// A compact preview of a theme's identity — six representative colours used
/// by the Settings → Theme grid. Order goes background → foreground → border
/// → accent → chart_bullish → chart_bearish, which together tell you "is it
/// dark/light? what's the accent? what colour are wins vs. losses?" at a
/// glance without needing to see every chart colour.
#[derive(Clone)]
pub struct ThemePreview {
    pub name: SharedString,
    pub mode: ThemeMode,
    pub background: Hsla,
    pub foreground: Hsla,
    pub border: Hsla,
    pub accent: Hsla,
    pub bullish: Hsla,
    pub bearish: Hsla,
}

/// Build a preview swatch list from each registered theme's `ThemeConfig`.
/// Colours stored as hex/Tailwind strings get parsed via `try_parse_color`;
/// missing or unparseable fields fall through to neutral defaults so a
/// half-specified theme still renders six discernible cells.
pub fn theme_previews(cx: &App) -> Vec<ThemePreview> {
    let fallback_dark = gpui::Hsla {
        h: 0.0,
        s: 0.0,
        l: 0.15,
        a: 1.0,
    };
    let fallback_light = gpui::Hsla {
        h: 0.0,
        s: 0.0,
        l: 0.85,
        a: 1.0,
    };
    ThemeRegistry::global(cx)
        .sorted_themes()
        .into_iter()
        .map(|cfg| {
            let c = &cfg.colors;
            let parse = |s: &Option<SharedString>, fb: Hsla| -> Hsla {
                s.as_ref()
                    .and_then(|v| try_parse_color(v).ok())
                    .unwrap_or(fb)
            };
            let bg_fb = if cfg.mode.is_dark() {
                fallback_dark
            } else {
                fallback_light
            };
            let fg_fb = if cfg.mode.is_dark() {
                fallback_light
            } else {
                fallback_dark
            };
            ThemePreview {
                name: cfg.name.clone(),
                mode: cfg.mode,
                background: parse(&c.background, bg_fb),
                foreground: parse(&c.foreground, fg_fb),
                border: parse(&c.border, bg_fb),
                accent: parse(&c.accent, fg_fb),
                bullish: parse(&c.chart_bullish, gpui::green()),
                bearish: parse(&c.chart_bearish, gpui::red()),
            }
        })
        .collect()
}

/// Apply the theme with the given name. Looks the `ThemeConfig` up in the
/// registry, swaps it into the matching slot on the global [`Theme`] (so a
/// later `Theme::change` for the same mode reuses it), then calls
/// `Theme::change` to apply the colours and refresh `window`.
///
/// Falls back to [`DEFAULT_THEME_NAME`] if `name` isn't in the registry.
/// Returns the name that was actually applied.
pub fn apply_theme_by_name(
    name: &str,
    window: Option<&mut Window>,
    cx: &mut App,
) -> SharedString {
    let registry = ThemeRegistry::global(cx);
    let lookup = registry.themes().get(name).cloned();
    let (config, applied_name) = match lookup {
        Some(cfg) => {
            let n = cfg.name.clone();
            (cfg, n)
        }
        None => {
            // Requested theme is gone (renamed bundle, stale persistence).
            // Pull the default — its presence is guaranteed by
            // `init_default_themes` in gpui-component.
            let cfg = registry
                .themes()
                .get(DEFAULT_THEME_NAME)
                .cloned()
                .expect("default dark theme always registered");
            (cfg, SharedString::from(DEFAULT_THEME_NAME))
        }
    };

    {
        let theme = cx.global_mut::<Theme>();
        if config.mode.is_dark() {
            theme.dark_theme = config.clone();
        } else {
            theme.light_theme = config.clone();
        }
    }
    Theme::change(config.mode, window, cx);
    applied_name
}
