use gpui::{App, AppContext as _, Application, Bounds, Entity, WindowBounds, WindowOptions, px, size};
use gpui_component::{Root, Theme};

const FONT_SIZE_MIN: f32 = 10.0;
const FONT_SIZE_MAX: f32 = 28.0;

pub mod ai_tools;
pub mod auth;
pub mod bottom_bar;
pub mod drawings;
pub mod floating_code_editor;
pub mod floating_window;
pub mod indicator_picker;
pub mod indicator_settings;
pub mod indicators;
pub mod net;
pub mod panels;
pub mod persistence;
pub mod prefs;
pub mod profile;
pub mod services;
pub mod settings;
pub mod sidebar;
pub mod symbol_picker;
pub mod themes;
pub mod top_bar;
pub mod workspace;

pub use workspace::TerminalWorkspace;

/// Run the app on a freshly-created [`Application`]. The crate only targets
/// wasm; this is what `wasm_entry::run` ends with after it leaks the
/// `Rc<AppCell>` so the runtime survives `run()` returning to JS.
pub fn run(app: Application) {
    app.run(|cx: &mut App| {
        init(cx);
        open_window(cx);
        cx.activate(true);
    });
}

fn init(cx: &mut App) {
    gpui_component::init(cx);
    // Drop any v1 persisted state on first run after the mode-system upgrade.
    // Idempotent — no-op once the v1 keys are gone.
    persistence::purge_v1();
    panels::init(cx);
    // Network stack: HttpClient must come before any service that uses it.
    net::init(cx);
    services::market_data::init(cx);
    services::symbols::init(cx);
    services::recents::init(cx);
    services::watchlist::init(cx);
    services::calendar::init(cx);
    services::filings::init(cx);
    services::insider::init(cx);
    services::news::init(cx);
    services::details::init(cx);
    drawings::init(cx);
    symbol_picker::init(cx);
    indicator_picker::init(cx);
    services::signal::init(cx);
    services::ai_chat::init(cx);
    // Watches persisted auth for an externally-refreshed token (supabase-js)
    // and applies it live. Must come after the services it restarts.
    auth::init_watcher(cx);

    // Register bundled themes (Solarized Dark, Dracula, Nord, ...) into
    // gpui-component's `ThemeRegistry`. Default Light/Dark are already
    // there from `gpui_component::init`.
    themes::init(cx);

    // Apply the user's persisted theme choice (or Default Dark on first
    // run / unknown name). Replaces the previously-hardcoded mode toggle.
    let theme_name = persistence::load_theme_name()
        .unwrap_or_else(|| themes::DEFAULT_THEME_NAME.to_string());
    themes::apply_theme_by_name(&theme_name, None, cx);

    // Apply persisted font size before any window opens so the first frame is right-sized.
    if let Some(size) = persistence::load_font_size() {
        cx.global_mut::<Theme>().font_size = px(size.clamp(FONT_SIZE_MIN, FONT_SIZE_MAX));
    }

    // Restore the user's "Dialog animations" preference. Defaults to on if
    // nothing was saved.
    if let Some(enabled) = persistence::load_dialog_animations() {
        gpui_component::dialog::set_animations_enabled(enabled);
    }

    // Install user-timezone + chart-prefs globals from disk. Must come before
    // any panel that reads them (chart paint, bottom bar clock).
    prefs::init(cx);

    install_wasm_fonts(cx);
}

fn open_window(cx: &mut App) {
    let bounds = Bounds::centered(None, size(px(1280.), px(800.)), cx);
    cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            ..Default::default()
        },
        |window, cx| {
            let workspace: Entity<TerminalWorkspace> =
                cx.new(|cx| TerminalWorkspace::new(window, cx));
            cx.new(|cx| Root::new(workspace, window, cx))
        },
    )
    .expect("failed to open window");
}

fn install_wasm_fonts(cx: &mut App) {
    use std::borrow::Cow;
    let cjk = Cow::Borrowed(include_bytes!("../../../fonts/NotoSansSC-Regular-subset.ttf").as_slice());
    let emoji = Cow::Borrowed(include_bytes!("../../../fonts/NotoEmoji-Regular.ttf").as_slice());
    let mono = Cow::Borrowed(include_bytes!("../../../fonts/JetBrainsMono-Regular.ttf").as_slice());
    cx.text_system()
        .add_fonts(vec![cjk, emoji, mono])
        .expect("failed to load fonts");
    cx.global_mut::<Theme>().font_family = "Noto Sans SC".into();
    cx.global_mut::<Theme>().mono_font_family = "JetBrains Mono".into();
}

// =====================
// WASM entry
// =====================

mod wasm_entry {
    use super::*;
    use gpui::AppCell;
    use wasm_bindgen::prelude::*;

    #[wasm_bindgen]
    pub fn run() -> Result<(), JsValue> {
        console_error_panic_hook::set_once();
        let _ = console_log::init_with_level(log::Level::Info);
        tracing_wasm::set_as_global_default();

        gpui_platform::web_init();
        let app = gpui_platform::single_threaded_web();

        // Mirror story-web's leak hack so the WASM `Rc<AppCell>` outlives `run()`.
        struct WasmApplication(std::rc::Rc<AppCell>);
        let wasm_app = unsafe { std::mem::transmute::<Application, WasmApplication>(app) };
        std::mem::forget(wasm_app.0.clone());
        let app: Application = unsafe { std::mem::transmute::<WasmApplication, Application>(wasm_app) };

        let app = app.with_assets(gpui_component_assets::Assets::new(
            "https://longbridge.github.io/gpui-component/gallery/",
        ));
        super::run(app);
        Ok(())
    }
}

