use std::sync::Arc;
use std::time::Duration;

use gpui::{
    AnyView, App, AppContext as _, Axis, Context, DismissEvent, Entity, FocusHandle,
    Focusable as _, InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
    Styled as _, Task, Window, div, px,
};
use gpui_component::{
    ActiveTheme as _, Placement, Root, Sizable as _, WindowExt as _,
    dock::{
        DockArea, DockAreaState, DockEvent, DockItem, PanelInfo, PanelState, PanelView,
    },
};

use crate::bottom_bar::BottomBar;
use crate::floating_code_editor::{FloatingCodeEditor, ToggleFloatingCodeEditor};
use crate::floating_window::FloatingWindow;
use crate::indicator_picker::{
    IndicatorPickerEvent, IndicatorPickerIntent, IndicatorPickerState, OpenIndicatorPicker,
};
use crate::indicator_settings::{IndicatorSettingsView, OpenIndicatorSettings};
use crate::panels::{
    self, ContentPanel, CurrentModeGlobal, Kind, LastFocusedChart, LastFocusedTabPanel,
};
use crate::persistence::{self, ChartLayout, Mode, ModeState};
use crate::services::ai_chat::AiChatServiceHandle;
use crate::services::signal::SignalServiceHandle;
use crate::services::watchlist::WatchlistServiceHandle;
use crate::sidebar::{Sidebar, SwitchMode};
use crate::symbol_picker::{OpenSymbolPicker, PickerEvent, PickerIntent, SymbolPickerState};
use crate::top_bar::{
    AddPanel, AddWatchlistSymbol, ApplyChartLayout, ApplyLayout, AskAi, DeleteLayout, FocusSymbol,
    ManageLayouts, RemoveWatchlistSymbol, ResetLayout, SaveLayout, SaveLayoutCurrent, SelectSignal,
    SetAiChatModel, SetTheme, ToggleAiChat, ToggleDetails, ToggleTrading, ToggleWatchlist, TopBar,
};

const LAYOUT_VERSION: usize = 3;
const DOCK_AREA_ID: &str = "main-dock";

pub struct TerminalWorkspace {
    sidebar: Entity<Sidebar>,
    top_bar: Entity<TopBar>,
    bottom_bar: Entity<BottomBar>,
    dock_area: Entity<DockArea>,
    /// Active mode. Drives layout building, top-bar shape, and which storage
    /// blob receives auto-save writes.
    mode: Mode,
    /// Last serialized layout written to storage for the active mode. Lets
    /// us skip redundant writes inside the debounced save.
    last_saved: Option<DockAreaState>,
    _save_task: Option<Task<()>>,
    focus_handle: FocusHandle,
    focused_once: bool,
    /// AI Chat singleton — global, follows the user across modes.
    ai_chat_panel: Option<Entity<ContentPanel>>,
    /// Trading singletons — per-mode. Recreated on mode entry if the mode's
    /// persisted state had `trading_open == true`.
    position_panel: Option<Entity<ContentPanel>>,
    execution_panel: Option<Entity<ContentPanel>>,
    /// Details panel (Charting only).
    details_panel: Option<Entity<ContentPanel>>,
    /// Watchlist singleton tracked here so the top-bar toggle button can
    /// find and remove the live panel reliably. `dock_to_window_edge` only
    /// updates the StackPanel views — not the `DockItem.items` tree that
    /// `find_first_kind` walks — so we can't rely on tree-walking alone.
    watchlist_panel: Option<Entity<ContentPanel>>,
    /// Mode-scoped toggle flags. Saved into the active mode's `ModeState`.
    trading_open: bool,
    details_open: bool,
    /// Chart layout selection (Charting mode only). Saved into ModeState.
    chart_layout: ChartLayout,
    /// Shared TradingView-style symbol picker. Rendered as an overlay above the
    /// dock when `is_open`. Opened by chart header buttons, the watchlist "+"
    /// button, and the workspace-scoped Cmd-K binding.
    symbol_picker: Entity<SymbolPickerState>,
    /// Workspace-global IndicatorPicker — mirrors the SymbolPicker pattern.
    /// Triggered by chart-toolbar `+ Indicator` button + Cmd-I, resolves
    /// the target ContentPanel via `LastFocusedChart`.
    indicator_picker: Entity<IndicatorPickerState>,
    /// Singleton floating panel for editing an indicator's params. The
    /// FloatingWindow + IndicatorSettingsView are spawned on first open;
    /// subsequent gear clicks retarget the existing view (no flicker).
    /// Auto-closed if the underlying chart panel or instance disappears.
    indicator_settings: Option<FloatingIndicatorSettingsSlot>,
    /// Free-Layout-only floating Code Editor. Singleton — the toggle action
    /// either focuses the existing one or spawns a new one. The window
    /// emits `DismissEvent` on close; the workspace subscribes and clears
    /// this slot. Auto-cleared on mode switch out of Free Layout.
    floating_code_editor: Option<FloatingCodeEditorSlot>,
}

/// Bundle for the floating Code Editor singleton. The window hosts the
/// editor as content; we keep a direct handle on the editor so the toggle
/// action can re-focus its input without round-tripping through the
/// `AnyView`.
struct FloatingCodeEditorSlot {
    window: Entity<FloatingWindow>,
    editor: Entity<FloatingCodeEditor>,
}

/// Bundle for the singleton floating indicator settings panel. When the
/// user clicks the gear on a second indicator we retarget the view (no
/// new window) so dragging/resizing state survives across edits.
struct FloatingIndicatorSettingsSlot {
    window: Entity<FloatingWindow>,
    view: Entity<IndicatorSettingsView>,
}

impl TerminalWorkspace {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let dock_area = cx.new(|cx| DockArea::new(DOCK_AREA_ID, Some(LAYOUT_VERSION), window, cx));
        let weak_dock = dock_area.downgrade();
        cx.set_global(panels::DockAreaHandle(weak_dock.clone()));
        // Self-reference for AI tools (and any other non-window caller)
        // that needs to read workspace state or dispatch layout changes.
        let weak_self = cx.entity().downgrade();
        cx.set_global(TerminalWorkspaceHandle(weak_self));
        // Register the AI tool dispatcher + per-turn client-context provider.
        // The bridge captures a weak dock-area reference so it can walk the
        // currently-open chart panels without a back-reference to the
        // workspace entity. Idempotent if `new` is somehow called twice —
        // `set_global` replaces the existing handle.
        crate::ai_tools::init(weak_dock.clone(), cx);

        let current = persistence::load_current_mode();
        let mode = current.mode;
        // Publish the active mode so ContentPanel::closable / zoomable see it.
        cx.set_global(CurrentModeGlobal(mode));

        // Try to restore the active mode's dock; otherwise apply the default.
        let mode_state = persistence::load_mode_state(mode);
        let (trading_open, details_open) = match &mode_state {
            Some(s) => (s.trading_open, s.details_open),
            None => default_toggles(mode),
        };
        let chart_layout = mode_state
            .as_ref()
            .map(|s| s.chart_layout)
            .unwrap_or_default();
        let loaded = mode_state
            .as_ref()
            .and_then(|s| s.dock.clone())
            .map(|dock_state| {
                dock_area
                    .update(cx, |dock, cx| dock.load(dock_state, window, cx))
                    .is_ok()
            })
            .unwrap_or(false);
        if !loaded {
            apply_default_layout(mode, chart_layout, weak_dock.clone(), window, cx);
        }

        cx.subscribe_in(
            &dock_area,
            window,
            |this, dock_area, ev: &DockEvent, window, cx| {
                if matches!(ev, DockEvent::LayoutChanged) {
                    this.schedule_save(dock_area.clone(), window, cx);
                }
            },
        )
        .detach();

        let sidebar = cx.new(|cx| Sidebar::new(mode, window, cx));
        let top_bar = cx.new(|cx| TopBar::new("terminal_demo", mode, window, cx));
        let bottom_bar = cx.new(|cx| BottomBar::new(window, cx));
        let symbol_picker = cx.new(|cx| SymbolPickerState::new(window, cx));
        // When the picker closes, reclaim focus on the workspace so the next
        // click on a panel button isn't eaten by the no-longer-rendered
        // search input.
        cx.subscribe_in(
            &symbol_picker,
            window,
            |this, _picker, ev: &PickerEvent, window, cx| match ev {
                PickerEvent::Closed => this.reclaim_focus(window, cx),
            },
        )
        .detach();
        let indicator_picker = cx.new(|cx| IndicatorPickerState::new(window, cx));
        cx.subscribe_in(
            &indicator_picker,
            window,
            |this, _picker, ev: &IndicatorPickerEvent, window, cx| match ev {
                IndicatorPickerEvent::Closed => this.reclaim_focus(window, cx),
            },
        )
        .detach();

        let mut this = Self {
            sidebar,
            top_bar,
            bottom_bar,
            dock_area,
            mode,
            last_saved: None,
            _save_task: None,
            focus_handle: cx.focus_handle(),
            focused_once: false,
            ai_chat_panel: None,
            position_panel: None,
            execution_panel: None,
            details_panel: None,
            watchlist_panel: None,
            trading_open: false,
            details_open: false,
            chart_layout,
            symbol_picker,
            indicator_picker,
            indicator_settings: None,
            floating_code_editor: None,
        };
        // Push toggle visuals before adopting / opening so the top bar paints
        // with the correct selected state on first frame.
        this.top_bar.update(cx, |bar, cx| {
            bar.set_ai_chat_open(false, cx);
            bar.set_trading_open(trading_open, cx);
            bar.set_details_open(details_open, cx);
            bar.set_chart_layout(chart_layout, cx);
        });
        this.trading_open = trading_open;
        this.details_open = details_open;
        // Adopt any singletons that came back from persisted state, then
        // open ones that the toggle flags say should be live. AI Chat is
        // intentionally never auto-opened on load — it's an ephemeral panel.
        this.adopt_existing_singletons(window, cx);
        this.apply_zone_locking(window, cx);
        if this.trading_open && this.position_panel.is_none() {
            this.open_trading_panels(window, cx);
        }
        if this.mode == Mode::Charting && this.details_open && this.details_panel.is_none() {
            this.open_details_panel(window, cx);
        }
        // Sync the watchlist toggle button to whatever the adopted dock
        // contains — the default layout includes a watchlist, but a saved
        // layout might not.
        let watchlist_present = this.watchlist_panel.is_some();
        this.top_bar
            .update(cx, |bar, cx| bar.set_watchlist_open(watchlist_present, cx));
        this
    }

    fn schedule_save(
        &mut self,
        dock_area: Entity<DockArea>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mode = self.mode;
        self._save_task = Some(cx.spawn_in(window, async move |this, window| {
            window
                .background_executor()
                .timer(Duration::from_millis(500))
                .await;
            _ = this.update_in(window, |this, _, cx| {
                // Only persist the *layout structure* — strip the AI Chat
                // singleton so it doesn't end up duplicated when the user
                // returns to this mode (AI Chat is global and re-attached on
                // entry).
                let dock_state = dock_state_without_globals(dock_area.read(cx).dump(cx));
                if Some(&dock_state) == this.last_saved.as_ref() {
                    return;
                }
                let state = ModeState {
                    dock: Some(dock_state.clone()),
                    trading_open: this.trading_open,
                    details_open: this.details_open,
                    chart_layout: this.chart_layout,
                };
                if let Err(err) = persistence::save_mode_state(mode, &state) {
                    log::warn!("save mode state failed: {err:?}");
                } else {
                    this.last_saved = Some(dock_state);
                }
            });
        }));
    }

    fn save_current_mode_pointer(&self) {
        let value = persistence::CurrentMode { mode: self.mode };
        if let Err(err) = persistence::save_current_mode(&value) {
            log::warn!("save current mode failed: {err:?}");
        }
    }

    fn on_add_panel(&mut self, action: &AddPanel, window: &mut Window, cx: &mut Context<Self>) {
        // +Panel only works in Free Layout; the constrained modes manage their
        // own structure.
        if self.mode != Mode::FreeLayout {
            return;
        }
        let Some(kind) = Kind::from_id(action.0.as_ref()) else {
            return;
        };
        if kind.is_singleton() {
            return;
        }

        let panel = panels::build_kind(kind, window, cx);
        let target = cx
            .global::<LastFocusedTabPanel>()
            .0
            .borrow()
            .clone()
            .and_then(|w| w.upgrade());

        if let Some(tab_panel) = target {
            tab_panel.update(cx, |tp, cx| tp.add_panel(panel.clone(), window, cx));
        } else {
            // Nothing focused — dock to the right edge so the new panel sits
            // in its own TabPanel alongside the existing layout instead of
            // disappearing into a left-side center add.
            self.dock_area.update(cx, |dock_area, cx| {
                dock_area.dock_to_window_edge(panel.clone(), Placement::Right, None, window, cx);
            });
        }
    }

    fn refresh_singleton_slots(&mut self, cx: &App) {
        for slot in [
            &mut self.ai_chat_panel,
            &mut self.position_panel,
            &mut self.execution_panel,
            &mut self.details_panel,
            &mut self.watchlist_panel,
        ] {
            if let Some(entity) = slot {
                if entity.read(cx).parent_tab_panel().is_none() {
                    *slot = None;
                }
            }
        }
    }

    fn reclaim_focus(&self, window: &mut Window, cx: &mut App) {
        self.focus_handle.clone().focus(window, cx);
    }

    /// Pin every tab panel inside the dock when in a constrained mode, so the
    /// user can't drag tabs out, drop in, or close. Free Layout: unpin every
    /// tab panel so docking is fully free. Deferred via `window.defer` so the
    /// freshly-built layout has time to populate `parent_tab_panel` on each
    /// child (see `pin_panel_tab` for the same trick).
    fn apply_zone_locking(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mode = self.mode;
        let dock = self.dock_area.clone();
        window.defer(cx, move |window, cx| {
            let center = dock.read(cx).center().clone();
            let mut panels: Vec<gpui::Entity<ContentPanel>> = Vec::new();
            collect_content_panels(&center, cx, &mut panels);
            let pin = !matches!(mode, Mode::FreeLayout);
            for entity in panels {
                if let Some(tp) = entity.read(cx).parent_tab_panel().and_then(|w| w.upgrade()) {
                    tp.update(cx, |tp, cx| tp.set_pinned(pin, window, cx));
                }
            }
        });
    }

    fn adopt_existing_singletons(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Drop every singleton slot *before* walking the new dock tree.
        // Forgetting `watchlist_panel` here caused the toggle to break after
        // a mode round-trip: a stale entity stayed pinned in the slot, the
        // post-`load` adoption skipped over the freshly-deserialized
        // watchlist, the first toggle no-op'd against the dead entity, and
        // the second toggle spawned a duplicate.
        self.ai_chat_panel = None;
        self.position_panel = None;
        self.execution_panel = None;
        self.details_panel = None;
        self.watchlist_panel = None;
        let center = self.dock_area.read(cx).center().clone();
        adopt_from_item(&center, cx, self);

        let mut pin = |entity: &Option<Entity<ContentPanel>>| {
            if let Some(entity) = entity {
                if let Some(tp) = entity.read(cx).parent_tab_panel().and_then(|w| w.upgrade()) {
                    tp.update(cx, |tp, cx| tp.set_pinned(true, window, cx));
                }
            }
        };
        pin(&self.ai_chat_panel);
        pin(&self.position_panel);
        pin(&self.execution_panel);
        pin(&self.details_panel);
    }

    fn on_switch_mode(&mut self, action: &SwitchMode, window: &mut Window, cx: &mut Context<Self>) {
        let Some(target) = Mode::from_id(action.0.as_ref()) else {
            return;
        };
        if target == self.mode {
            return;
        }
        // AI Chat is ephemeral: switching modes closes it. Remove from its
        // parent tab panel (if docked) and drop the slot so the target mode
        // starts with the panel closed.
        if let Some(panel) = self.ai_chat_panel.take() {
            if let Some(tp) = panel.read(cx).parent_tab_panel().and_then(|w| w.upgrade()) {
                let arc: Arc<dyn PanelView> = Arc::new(panel);
                tp.update(cx, |tp, cx| tp.remove_panel(arc, window, cx));
            }
            // Match on_toggle_ai_chat's collapse behavior so the next reopen
            // starts in chat-focus mode.
            let svc = cx.global::<AiChatServiceHandle>().0.clone();
            svc.update(cx, |svc, cx| svc.set_sidebar_collapsed(true, cx));
        }
        // Per-mode singletons are torn down — the target mode will respawn
        // them if its persisted toggle says so.
        self.position_panel = None;
        self.execution_panel = None;
        self.details_panel = None;
        // Floating Code Editor is Free-Layout-only. Drop it on any mode
        // switch (including FreeLayout → FreeLayout, which can't happen
        // here because we early-returned on equal target above).
        self.floating_code_editor = None;

        self.mode = target;
        self.last_saved = None;
        cx.set_global(CurrentModeGlobal(target));

        let mode_state = persistence::load_mode_state(target);
        let (trading_open, details_open) = match &mode_state {
            Some(s) => (s.trading_open, s.details_open),
            None => default_toggles(target),
        };
        let chart_layout = mode_state
            .as_ref()
            .map(|s| s.chart_layout)
            .unwrap_or_default();
        self.trading_open = trading_open;
        self.details_open = details_open;
        self.chart_layout = chart_layout;

        let weak_dock = self.dock_area.downgrade();
        let loaded = mode_state
            .as_ref()
            .and_then(|s| s.dock.clone())
            .map(|dock_state| {
                self.dock_area
                    .update(cx, |dock, cx| dock.load(dock_state, window, cx))
                    .is_ok()
            })
            .unwrap_or(false);
        if !loaded {
            apply_default_layout(target, chart_layout, weak_dock, window, cx);
        }
        self.adopt_existing_singletons(window, cx);
        self.apply_zone_locking(window, cx);

        // Open per-mode singletons whose toggles say they should be live.
        if self.trading_open && self.position_panel.is_none() {
            self.open_trading_panels(window, cx);
        }
        if self.mode == Mode::Charting && self.details_open && self.details_panel.is_none() {
            self.open_details_panel(window, cx);
        }

        self.sidebar.update(cx, |s, cx| s.set_current(target, cx));
        let watchlist_present = self.watchlist_panel.is_some();
        self.top_bar.update(cx, |bar, cx| {
            bar.set_mode(target, cx);
            bar.set_trading_open(self.trading_open, cx);
            bar.set_details_open(self.details_open, cx);
            bar.set_ai_chat_open(self.ai_chat_panel.is_some(), cx);
            bar.set_watchlist_open(watchlist_present, cx);
            bar.set_chart_layout(self.chart_layout, cx);
        });
        self.save_current_mode_pointer();
        self.reclaim_focus(window, cx);
        // Force a re-save of the new mode's state immediately so freshly
        // built defaults persist (otherwise we only save when the user
        // edits the layout).
        let dock = self.dock_area.clone();
        self.schedule_save(dock, window, cx);
    }

    fn on_reset_layout(&mut self, _: &ResetLayout, window: &mut Window, cx: &mut Context<Self>) {
        // Wipe just the active mode and rebuild its default.
        let mode = self.mode;
        let _ = persistence::clear_mode_state(mode);
        let weak = self.dock_area.downgrade();
        // Reset restores the default chart-layout choice too.
        self.chart_layout = ChartLayout::default();
        apply_default_layout(mode, self.chart_layout, weak, window, cx);
        self.last_saved = None;
        let (trading_open, details_open) = default_toggles(mode);
        self.trading_open = trading_open;
        self.details_open = details_open;
        // Singletons in the rebuilt default are managed by build_*_layout, so
        // just adopt and re-open the ones the defaults say should be live.
        self.position_panel = None;
        self.execution_panel = None;
        self.details_panel = None;
        self.adopt_existing_singletons(window, cx);
        self.apply_zone_locking(window, cx);
        if self.trading_open && self.position_panel.is_none() {
            self.open_trading_panels(window, cx);
        }
        if self.mode == Mode::Charting && self.details_open && self.details_panel.is_none() {
            self.open_details_panel(window, cx);
        }
        // AI Chat is global — re-attach if it had been open.
        if let Some(panel) = self.ai_chat_panel.clone() {
            if panel.read(cx).parent_tab_panel().is_none() {
                self.dock_ai_chat(window, cx);
            }
        }
        let watchlist_present = self.watchlist_panel.is_some();
        self.top_bar.update(cx, |bar, cx| {
            bar.set_trading_open(self.trading_open, cx);
            bar.set_details_open(self.details_open, cx);
            bar.set_watchlist_open(watchlist_present, cx);
            bar.set_chart_layout(self.chart_layout, cx);
        });
        self.reclaim_focus(window, cx);
    }

    fn on_toggle_ai_chat(&mut self, _: &ToggleAiChat, window: &mut Window, cx: &mut Context<Self>) {
        self.refresh_singleton_slots(cx);

        if let Some(panel) = self.ai_chat_panel.take() {
            let parent = panel.read(cx).parent_tab_panel();
            if let Some(tp) = parent.and_then(|w| w.upgrade()) {
                let arc: Arc<dyn PanelView> = Arc::new(panel);
                tp.update(cx, |tp, cx| tp.remove_panel(arc, window, cx));
                // Collapse the sessions sidebar so the next reopen starts in
                // chat-focus mode instead of restoring the wider 2-pane view.
                let svc = cx.global::<AiChatServiceHandle>().0.clone();
                svc.update(cx, |svc, cx| svc.set_sidebar_collapsed(true, cx));
                self.top_bar
                    .update(cx, |bar, cx| bar.set_ai_chat_open(false, cx));
                self.reclaim_focus(window, cx);
                return;
            }
        }

        self.open_ai_chat_panel(window, cx);
        self.top_bar
            .update(cx, |bar, cx| bar.set_ai_chat_open(true, cx));
    }

    /// Toggle handler for the Free-Layout-only floating code editor. Opens a
    /// new singleton if none exists, otherwise re-focuses the existing one.
    /// Bound on the workspace, dispatched by the "+ Panel" menu entry.
    fn on_toggle_floating_code_editor(
        &mut self,
        _: &ToggleFloatingCodeEditor,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.mode != Mode::FreeLayout {
            return;
        }
        if let Some(slot) = &self.floating_code_editor {
            // Two-line read → focus: the temp from `read(cx)` releases its
            // immutable borrow at the semicolon, freeing cx to be borrowed
            // mutably by `focus()` on the next line.
            let handle = slot.editor.read(cx).focus_handle(cx);
            handle.focus(window, cx);
            return;
        }
        let editor = cx.new(|cx| FloatingCodeEditor::new(window, cx));
        let content: AnyView = editor.clone().into();
        let win = cx.new(|cx| FloatingWindow::new("Code Editor", content, window, cx));
        // The close X (and any future programmatic close) emits DismissEvent;
        // clear our slot so the next toggle spawns a fresh editor and the
        // overlay disappears from render.
        cx.subscribe_in(
            &win,
            window,
            |this, _, _: &DismissEvent, window, cx| {
                this.close_floating_code_editor(window, cx);
            },
        )
        .detach();
        let handle = editor.read(cx).focus_handle(cx);
        handle.focus(window, cx);
        self.floating_code_editor = Some(FloatingCodeEditorSlot {
            window: win,
            editor,
        });
        cx.notify();
    }

    /// Tear down the floating code editor slot if one is open. Used by the
    /// DismissEvent subscription and by the mode-switch handler.
    fn close_floating_code_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.floating_code_editor.take().is_some() {
            self.reclaim_focus(window, cx);
            cx.notify();
        }
    }

    fn on_ask_ai(&mut self, action: &AskAi, window: &mut Window, cx: &mut Context<Self>) {
        let prompt = action.0.clone();
        let weak_self = cx.entity().downgrade();
        window.defer(cx, move |window, cx| {
            let _ = weak_self.update(cx, |this, cx| {
                this.refresh_singleton_slots(cx);
                if this.ai_chat_panel.is_none() {
                    this.open_ai_chat_panel(window, cx);
                    this.top_bar
                        .update(cx, |bar, cx| bar.set_ai_chat_open(true, cx));
                }
                let Some(panel) = this.ai_chat_panel.clone() else {
                    return;
                };

                if let Some(tp) = panel.read(cx).parent_tab_panel().and_then(|w| w.upgrade()) {
                    let arc: Arc<dyn PanelView> = Arc::new(panel.clone());
                    if let Some(ix) = tp.read(cx).index_of_panel(&arc) {
                        tp.update(cx, |tp, cx| tp.set_active_ix(ix, window, cx));
                    }
                }

                // Route the prompt through the service rather than directly
                // poking the InputState — the service appends with `\n` if
                // the active session already has draft text, and emits an
                // `InputStaged` event so the panel's subscription handler
                // mirrors the new draft into its input.
                let svc = cx.global::<AiChatServiceHandle>().0.clone();
                svc.update(cx, |s, cx| {
                    let id = match s.selected_id() {
                        Some(id) => id.to_string(),
                        // Defensive: the service auto-creates one session on
                        // first open, so this branch shouldn't normally fire.
                        None => s.create_session(cx),
                    };
                    s.stage_input(&id, prompt.as_ref(), cx);
                });

                if let Some(input) = panel.read(cx).chat_input().cloned() {
                    let handle = input.read(cx).focus_handle(cx);
                    handle.focus(window, cx);
                }
            });
        });
    }

    fn on_set_ai_chat_model(
        &mut self,
        action: &SetAiChatModel,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let model_id = action.0.as_ref().to_string();
        let svc = cx.global::<AiChatServiceHandle>().0.clone();
        svc.update(cx, |s, cx| {
            let Some(id) = s.selected_id().map(|s| s.to_string()) else {
                return;
            };
            s.set_model(&id, &model_id, cx);
        });
    }

    fn open_ai_chat_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let entity = cx.new(|cx| ContentPanel::new(Kind::AiChat, window, cx));
        self.ai_chat_panel = Some(entity);
        self.dock_ai_chat(window, cx);
    }

    fn dock_ai_chat(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entity) = self.ai_chat_panel.clone() else {
            return;
        };
        let arc: Arc<dyn PanelView> = Arc::new(entity.clone());
        let viewport_w: f32 = window.viewport_size().width.into();
        // Initial AI Chat width: ~1/3 of the viewport, floored at 420px so the
        // chat bubbles aren't squeezed on first open. The user can still drag
        // the divider to resize.
        let target_width = px((viewport_w / 3.0).max(420.0));
        self.dock_area.update(cx, |dock, cx| {
            dock.dock_to_window_edge(arc, Placement::Right, Some(target_width), window, cx);
        });
        pin_panel_tab(&entity, window, cx);
    }

    fn on_toggle_trading(
        &mut self,
        _: &ToggleTrading,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.refresh_singleton_slots(cx);

        if self.position_panel.is_some() || self.execution_panel.is_some() {
            let close_position = self.position_panel.take();
            let close_execution = self.execution_panel.take();
            for entity in [close_position, close_execution].into_iter().flatten() {
                if let Some(tp) = entity.read(cx).parent_tab_panel().and_then(|w| w.upgrade()) {
                    let arc: Arc<dyn PanelView> = Arc::new(entity);
                    tp.update(cx, |tp, cx| tp.remove_panel(arc, window, cx));
                }
            }
            self.trading_open = false;
            self.top_bar
                .update(cx, |bar, cx| bar.set_trading_open(false, cx));
            self.persist_mode_toggles(window, cx);
            self.reclaim_focus(window, cx);
            return;
        }

        self.open_trading_panels(window, cx);
        self.trading_open = true;
        self.top_bar
            .update(cx, |bar, cx| bar.set_trading_open(true, cx));
        self.persist_mode_toggles(window, cx);
    }

    fn open_trading_panels(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let viewport = window.viewport_size();
        let viewport_w: f32 = viewport.width.into();
        let viewport_h: f32 = viewport.height.into();
        // Floor must fit the Execution panel's natural content (header + 3
        // fields + button row + padding + tab strip) so the bottom row
        // doesn't need to scroll on first open.
        let bottom_height = px((viewport_h / 4.0).max(200.0));
        let exec_width = px((viewport_w / 4.0).max(180.0));

        let position = cx.new(|cx| ContentPanel::new(Kind::Position, window, cx));
        let execution = cx.new(|cx| ContentPanel::new(Kind::Execution, window, cx));
        let position_arc: Arc<dyn PanelView> = Arc::new(position.clone());
        let execution_arc: Arc<dyn PanelView> = Arc::new(execution.clone());

        let weak = self.dock_area.downgrade();
        self.dock_area.update(cx, |dock, cx| {
            let bottom_row = DockItem::split_with_sizes(
                Axis::Horizontal,
                vec![
                    DockItem::tabs(vec![position_arc], &weak, window, cx),
                    DockItem::tabs(vec![execution_arc], &weak, window, cx),
                ],
                vec![None, Some(exec_width)],
                &weak,
                window,
                cx,
            );

            let existing = dock.center().clone();
            let new_center = DockItem::split_with_sizes(
                Axis::Vertical,
                vec![existing, bottom_row],
                vec![None, Some(bottom_height)],
                &weak,
                window,
                cx,
            );
            dock.set_center(new_center, window, cx);
        });

        pin_panel_tab(&position, window, cx);
        pin_panel_tab(&execution, window, cx);
        self.position_panel = Some(position);
        self.execution_panel = Some(execution);
    }

    fn on_toggle_details(
        &mut self,
        _: &ToggleDetails,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.mode != Mode::Charting {
            return;
        }
        self.refresh_singleton_slots(cx);

        if let Some(entity) = self.details_panel.take() {
            if let Some(tp) = entity.read(cx).parent_tab_panel().and_then(|w| w.upgrade()) {
                let arc: Arc<dyn PanelView> = Arc::new(entity);
                tp.update(cx, |tp, cx| tp.remove_panel(arc, window, cx));
            }
            self.details_open = false;
            self.top_bar
                .update(cx, |bar, cx| bar.set_details_open(false, cx));
            self.persist_mode_toggles(window, cx);
            self.reclaim_focus(window, cx);
            return;
        }

        self.open_details_panel(window, cx);
        self.details_open = true;
        self.top_bar
            .update(cx, |bar, cx| bar.set_details_open(true, cx));
        self.persist_mode_toggles(window, cx);
    }

    fn open_details_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let entity = cx.new(|cx| ContentPanel::new(Kind::Details, window, cx));
        let arc: Arc<dyn PanelView> = Arc::new(entity.clone());
        let viewport_h: f32 = window.viewport_size().height.into();
        let target_h = px((viewport_h / 3.0).max(180.0));

        // Prefer splitting under the watchlist so Details lives in the right
        // column. Falls back to a window-edge dock if no watchlist is in
        // the dock (defensive — shouldn't happen in Charting).
        let center = self.dock_area.read(cx).center().clone();
        let watchlist_tp = find_first_kind(&center, cx, Kind::Watchlist)
            .and_then(|w| w.read(cx).parent_tab_panel())
            .and_then(|w| w.upgrade());
        if let Some(tp) = watchlist_tp {
            tp.update(cx, |tp, cx| {
                tp.add_panel_at(arc, Placement::Bottom, Some(target_h), window, cx);
            });
        } else {
            self.dock_area.update(cx, |dock, cx| {
                dock.dock_to_window_edge(arc, Placement::Right, Some(target_h), window, cx);
            });
        }
        pin_panel_tab(&entity, window, cx);
        self.details_panel = Some(entity);
    }

    /// Persist the active mode's toggle flags. Triggers a debounced save via
    /// the dock's existing LayoutChanged channel — that path is the single
    /// source of truth for ModeState writes.
    fn persist_mode_toggles(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let dock = self.dock_area.clone();
        self.schedule_save(dock, window, cx);
    }

    fn on_toggle_watchlist(
        &mut self,
        _: &ToggleWatchlist,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Drop the stored ref if the panel was already detached (e.g. via a
        // mode switch that rebuilt the dock).
        self.refresh_singleton_slots(cx);

        if let Some(entity) = self.watchlist_panel.take() {
            if let Some(tp) = entity.read(cx).parent_tab_panel().and_then(|w| w.upgrade()) {
                let arc: Arc<dyn PanelView> = Arc::new(entity);
                tp.update(cx, |tp, cx| tp.remove_panel(arc, window, cx));
            }
            self.top_bar
                .update(cx, |bar, cx| bar.set_watchlist_open(false, cx));
            self.reclaim_focus(window, cx);
            return;
        }

        // Open a fresh watchlist. If AI chat is on the right edge, split it
        // open on the LEFT so AI chat stays rightmost; otherwise dock to the
        // right edge directly.
        let entity = cx.new(|cx| ContentPanel::new(Kind::Watchlist, window, cx));
        let arc: Arc<dyn PanelView> = Arc::new(entity.clone());
        let viewport_w: f32 = window.viewport_size().width.into();
        let target_w = px((viewport_w / 5.0).max(220.0));

        let ai_chat_tp = self
            .ai_chat_panel
            .as_ref()
            .and_then(|p| p.read(cx).parent_tab_panel())
            .and_then(|w| w.upgrade());
        if let Some(tp) = ai_chat_tp {
            tp.update(cx, |tp, cx| {
                tp.add_panel_at(arc, Placement::Left, Some(target_w), window, cx);
            });
        } else {
            self.dock_area.update(cx, |dock, cx| {
                dock.dock_to_window_edge(arc, Placement::Right, Some(target_w), window, cx);
            });
        }
        pin_panel_tab(&entity, window, cx);
        self.watchlist_panel = Some(entity);
        self.top_bar
            .update(cx, |bar, cx| bar.set_watchlist_open(true, cx));
    }

    fn on_apply_layout(
        &mut self,
        action: &ApplyLayout,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Apply-by-name only works in Free Layout; saved layouts are scoped
        // to that mode.
        if self.mode != Mode::FreeLayout {
            return;
        }
        let id = action.0.clone();
        let layouts = persistence::load_layouts();
        let Some(state) = layouts.get(id.as_ref()).cloned() else {
            notify_warning(
                window,
                cx,
                "Layout missing",
                &format!("No saved layout named '{id}'"),
            );
            return;
        };
        let _ = self
            .dock_area
            .update(cx, |dock, cx| dock.load(state, window, cx));
        self.adopt_existing_singletons(window, cx);
        self.reclaim_focus(window, cx);
    }

    fn on_save_layout(&mut self, _: &SaveLayout, window: &mut Window, cx: &mut Context<Self>) {
        if self.mode != Mode::FreeLayout {
            notify_info(
                window,
                cx,
                "Save layout",
                "Saving named layouts is only available in Free Layout mode.",
            );
            return;
        }
        let dock_area = self.dock_area.clone();
        let weak_self = cx.entity().downgrade();
        let name_state = cx.new(|cx| {
            gpui_component::input::InputState::new(window, cx).placeholder("Layout name")
        });
        let name_handle = name_state.clone();
        window.open_dialog(cx, move |dialog, _, _| {
            let saving_state = name_handle.clone();
            let dock_area = dock_area.clone();
            let weak_self = weak_self.clone();
            dialog
                .max_w(px(420.))
                .button_props(
                    gpui_component::dialog::DialogButtonProps::default()
                        .show_cancel(true)
                        .ok_text("Save")
                        .on_ok(move |_, window, cx| {
                            let name = saving_state.read(cx).value().to_string();
                            let trimmed = name.trim();
                            if trimmed.is_empty() {
                                notify_warning(window, cx, "Save layout", "Enter a name first");
                                return false;
                            }
                            let state = dock_state_without_globals(dock_area.read(cx).dump(cx));
                            match persistence::upsert_layout(trimmed, state) {
                                Ok(()) => {
                                    notify_success(
                                        window,
                                        cx,
                                        "Layout saved",
                                        format!("Saved '{trimmed}'"),
                                    );
                                    _ = weak_self.update(cx, |this, cx| {
                                        this.refresh_top_bar_saved_layouts(cx);
                                    });
                                    true
                                }
                                Err(err) => {
                                    log::warn!("save layout failed: {err:?}");
                                    notify_error(
                                        window,
                                        cx,
                                        "Save layout",
                                        "Failed to save layout",
                                    );
                                    false
                                }
                            }
                        }),
                )
                .child(
                    gpui::div()
                        .px_4()
                        .pt_4()
                        .pb_2()
                        .child(SharedString::from("Save current layout as:")),
                )
                .child(
                    gpui::div()
                        .px_4()
                        .pb_4()
                        .child(gpui_component::input::Input::new(&name_handle).small()),
                )
        });
    }

    fn on_save_layout_current(
        &mut self,
        _: &SaveLayoutCurrent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Single Save button in Free Layout dispatches Save (Save-As). Provided
        // for parity with the old top bar's overwritable case.
        self.on_save_layout(&SaveLayout, window, cx);
    }

    fn refresh_top_bar_saved_layouts(&self, cx: &mut Context<Self>) {
        self.top_bar
            .update(cx, |top_bar, cx| top_bar.refresh_saved_layouts(cx));
    }

    fn on_manage_layouts(
        &mut self,
        _: &ManageLayouts,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let layouts = persistence::load_layouts();
        if layouts.is_empty() {
            notify_info(
                window,
                cx,
                "No saved layouts",
                "Use 'Save current layout…' to create one first.",
            );
            return;
        }

        window.open_dialog(cx, move |dialog, _, cx| {
            let layouts = layouts.clone();
            let muted = cx.theme().muted_foreground;
            let border = cx.theme().border;
            let mut body = gpui::div().px_4().pb_4().pt_2().flex().flex_col().gap_2();
            body = body.child(
                gpui::div()
                    .text_xs()
                    .text_color(muted)
                    .child("Click a layout to apply it, or × to delete."),
            );
            for name in layouts.keys() {
                let n_apply = SharedString::from(name.clone());
                let n_delete = SharedString::from(name.clone());
                body = body.child(
                    gpui::div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .px_2()
                        .py_1()
                        .border_1()
                        .border_color(border)
                        .rounded(px(4.))
                        .child(
                            gpui::div()
                                .flex_1()
                                .text_sm()
                                .child(SharedString::from(name.clone())),
                        )
                        .child(
                            gpui_component::button::Button::new(SharedString::from(format!(
                                "apply-{name}"
                            )))
                            .label("Apply")
                            .small()
                            .on_click(move |_, window, cx| {
                                window.dispatch_action(Box::new(ApplyLayout(n_apply.clone())), cx);
                                window.close_all_dialogs(cx);
                            }),
                        )
                        .child(
                            gpui_component::button::Button::new(SharedString::from(format!(
                                "delete-{name}"
                            )))
                            .label("×")
                            .small()
                            .on_click(move |_, window, cx| {
                                window
                                    .dispatch_action(Box::new(DeleteLayout(n_delete.clone())), cx);
                                window.close_all_dialogs(cx);
                            }),
                        ),
                );
            }
            dialog
                .title(SharedString::from("Saved Layouts"))
                .max_w(px(480.))
                .button_props(gpui_component::dialog::DialogButtonProps::default().ok_text("Done"))
                .child(body)
        });
    }

    fn on_set_theme(
        &mut self,
        action: &SetTheme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Apply with window refresh so the swap is visible immediately.
        let applied = crate::themes::apply_theme_by_name(action.0.as_ref(), Some(window), cx);
        if let Err(err) = crate::persistence::save_theme_name(applied.as_ref()) {
            log::warn!("save theme name failed: {err:?}");
        }
        cx.notify();
    }

    fn on_set_timezone(
        &mut self,
        action: &crate::settings::SetTimezone,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        crate::settings::apply_timezone(cx, action.0.clone());
        // Renderers (chart axis, bottom-bar clock) read the new global on
        // their next paint — force a refresh so the swap is instant.
        window.refresh();
        cx.notify();
    }

    fn on_apply_chart_layout(
        &mut self,
        action: &ApplyChartLayout,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.mode != Mode::Charting {
            return;
        }
        let Some(layout) = ChartLayout::from_id(action.0.as_ref()) else {
            return;
        };
        if layout == self.chart_layout {
            return;
        }
        self.chart_layout = layout;

        // Capture the current chart symbols before the rebuild destroys
        // their owning panels. Most-recently-focused first, then the rest
        // in dock-walk order — that ordering decides which symbols survive
        // a "more → less" transition (keep the front of the list) and
        // which slots get padded from recents on "less → more".
        let focused_chart = cx
            .global::<LastFocusedChart>()
            .0
            .borrow()
            .clone()
            .and_then(|w| w.upgrade())
            .filter(|e| e.read(cx).kind() == Kind::Chart);
        // Capture the focused chart's timeframe so the rebuild can carry
        // it across to the new layout's charts (otherwise every panel
        // would reset to the layout's default TF).
        let focused_tf = focused_chart
            .as_ref()
            .and_then(|p| p.read(cx).chart_timeframe());
        let dock_charts = collect_chart_panels(&self.dock_area.read(cx).center().clone(), cx);
        let mut preserved_symbols: Vec<SharedString> = Vec::new();
        if let Some(fp) = focused_chart.as_ref() {
            if let Some(state) = fp.read(cx).chart_state.as_ref() {
                preserved_symbols.push(state.symbol().clone());
            }
        }
        for panel in &dock_charts {
            if focused_chart.as_ref() == Some(panel) {
                continue;
            }
            if let Some(state) = panel.read(cx).chart_state.as_ref() {
                let sym = state.symbol().clone();
                if !preserved_symbols.contains(&sym) {
                    preserved_symbols.push(sym);
                }
            }
        }

        // Tear down per-mode singletons since the rebuild destroys their
        // attachments. AI Chat is global and survives the rebuild via
        // explicit detach + reattach.
        let ai_chat = self.ai_chat_panel.take();
        if let Some(panel) = ai_chat.as_ref() {
            if let Some(tp) = panel.read(cx).parent_tab_panel().and_then(|w| w.upgrade()) {
                let arc: Arc<dyn PanelView> = Arc::new(panel.clone());
                tp.update(cx, |tp, cx| tp.remove_panel(arc, window, cx));
            }
        }
        self.position_panel = None;
        self.execution_panel = None;
        self.details_panel = None;
        // Capture watchlist visibility BEFORE clearing the ref so we can
        // honor "user had it closed" after the rebuild — otherwise the
        // new default layout would silently bring it back. Also clear
        // the ref unconditionally: the layout rebuild detaches the
        // current watchlist entity, and leaving the stale ref around
        // makes `adopt_existing_singletons` skip the new one (because
        // `is_none()` is false), so the next toggle would spawn a
        // duplicate.
        let was_watchlist_open = self.watchlist_panel.is_some();
        self.watchlist_panel = None;
        self.last_saved = None;

        let weak = self.dock_area.downgrade();
        apply_default_layout(self.mode, self.chart_layout, weak, window, cx);
        self.adopt_existing_singletons(window, cx);
        self.apply_zone_locking(window, cx);

        // If the user had the watchlist closed, drop the one the default
        // layout added so the layout switch doesn't resurrect it.
        if !was_watchlist_open {
            if let Some(entity) = self.watchlist_panel.take() {
                if let Some(tp) = entity.read(cx).parent_tab_panel().and_then(|w| w.upgrade()) {
                    let arc: Arc<dyn PanelView> = Arc::new(entity);
                    tp.update(cx, |tp, cx| tp.remove_panel(arc, window, cx));
                }
            }
        }

        // Re-attach the trading + details singletons FIRST so they take
        // their place in the freshly-built layout, then dock AI Chat last
        // on the next frame. Two reasons this order matters:
        //
        // 1. Topology. `open_trading_panels` wraps the current center in
        //    a Vertical split. If AI Chat was inserted before that wrap,
        //    it ends up sandwiched in the upper horizontal stack as a
        //    sibling of the chart workspace + watchlist. The user's
        //    expectation (and the post-toggle topology) is that AI Chat
        //    sits at the dock's outer right edge spanning the whole
        //    height. Docking it after the wrap puts it there via
        //    `dock_to_window_edge`'s wrap-existing-center path.
        //
        // 2. Bounds. Both the rebuilt center and any wrap from
        //    `open_trading_panels` start with `ResizableState.bounds = 0`
        //    because they haven't prepainted. Inserting into a zero-
        //    bounds stack wedges the size math so the resize handle drag
        //    no-ops. `window.defer` runs in the same frame and doesn't
        //    fix this; `window.on_next_frame` does — it fires after the
        //    next paint, so by the time we dock AI Chat the parent
        //    stack's bounds are real.
        if self.trading_open && self.position_panel.is_none() {
            self.open_trading_panels(window, cx);
        }
        if self.details_open && self.details_panel.is_none() {
            self.open_details_panel(window, cx);
        }
        if let Some(panel) = ai_chat {
            self.ai_chat_panel = Some(panel);
            let weak_self = cx.entity().downgrade();
            window.on_next_frame(move |window, cx| {
                if let Some(this) = weak_self.upgrade() {
                    this.update(cx, |this, cx| this.dock_ai_chat(window, cx));
                }
            });
        }

        // Walk the rebuilt dock and assign symbols to its chart slots.
        // First slots take the preserved (focus-ordered) symbols; any
        // remaining slots get recents that aren't already on screen, so
        // a 1 → 4 transition fills the three new charts with the user's
        // most-recent picks rather than four identical defaults.
        let new_chart_panels = collect_chart_panels(&self.dock_area.read(cx).center().clone(), cx);
        let recents: Vec<SharedString> = cx
            .try_global::<crate::services::recents::RecentsServiceHandle>()
            .map(|h| h.0.read(cx).tickers().to_vec())
            .unwrap_or_default();
        let mut final_symbols = preserved_symbols.clone();
        for r in recents {
            if final_symbols.len() >= new_chart_panels.len() {
                break;
            }
            if !final_symbols.contains(&r) {
                final_symbols.push(r);
            }
        }
        for (panel, symbol) in new_chart_panels.iter().zip(final_symbols.iter()) {
            let target_symbol = symbol.clone();
            let target_tf = focused_tf;
            panel.update(cx, |p, cx| {
                p.switch_chart_symbol(target_symbol.as_ref(), cx);
                if let Some(tf) = target_tf {
                    p.switch_chart_timeframe(tf, cx);
                }
            });
        }

        let watchlist_present = self.watchlist_panel.is_some();
        self.top_bar.update(cx, |bar, cx| {
            bar.set_chart_layout(layout, cx);
            bar.set_watchlist_open(watchlist_present, cx);
        });
        // Force a save so the layout change persists even if no other dock
        // edit follows.
        let dock = self.dock_area.clone();
        self.schedule_save(dock, window, cx);
        self.reclaim_focus(window, cx);
    }

    fn on_focus_symbol(
        &mut self,
        action: &FocusSymbol,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let target = action.0.clone();
        // First try the explicitly-tracked focused chart.
        let chart = cx
            .global::<LastFocusedChart>()
            .0
            .borrow()
            .clone()
            .and_then(|w| w.upgrade())
            .filter(|e| e.read(cx).kind() == Kind::Chart);
        let chart =
            chart.or_else(|| find_first_chart(&self.dock_area.read(cx).center().clone(), cx));
        let Some(chart) = chart else {
            return;
        };
        chart.update(cx, |panel, cx| {
            panel.switch_chart_symbol(target.as_ref(), cx);
        });
        // Cache as last-focused so subsequent watchlist clicks reuse it.
        let weak = chart.downgrade();
        let global = cx.global::<LastFocusedChart>().0.clone();
        *global.borrow_mut() = Some(weak);
    }

    fn on_add_watchlist_symbol(
        &mut self,
        action: &AddWatchlistSymbol,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ticker = action.0.clone();
        let trimmed = ticker.trim().to_uppercase();
        if trimmed.is_empty() {
            return;
        }
        let added = cx
            .global::<WatchlistServiceHandle>()
            .0
            .clone()
            .update(cx, |svc, cx| {
                svc.add(SharedString::from(trimmed.clone()), cx)
            });
        if added {
            notify_success(
                window,
                cx,
                "Added to watchlist",
                format!("{trimmed} is now on your watchlist"),
            );
        } else {
            notify_info(
                window,
                cx,
                "Watchlist",
                &format!("{trimmed} is already on your watchlist"),
            );
        }
    }

    fn on_remove_watchlist_symbol(
        &mut self,
        action: &RemoveWatchlistSymbol,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ticker = action.0.clone();
        cx.global::<WatchlistServiceHandle>()
            .0
            .clone()
            .update(cx, |svc, cx| {
                svc.remove(ticker.as_ref(), cx);
            });
    }

    fn on_select_signal(
        &mut self,
        action: &SelectSignal,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ticker = action.0.clone();
        cx.global::<SignalServiceHandle>()
            .0
            .clone()
            .update(cx, |svc, cx| {
                svc.select(ticker.as_ref(), cx);
            });
    }

    fn on_open_symbol_picker(
        &mut self,
        action: &OpenSymbolPicker,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Toggle: re-dispatching while the picker is up closes it. Lets the
        // same shortcut (Cmd-K) and the same trigger buttons act as both
        // open and dismiss.
        if self.symbol_picker.read(cx).is_open() {
            self.symbol_picker.update(cx, |p, cx| p.close(cx));
            return;
        }
        // Modal mutual exclusion: only one picker at a time. If the
        // indicator picker is up, Cmd-K should NOT stack a second modal
        // on top. Bail without opening — user dismisses the indicator
        // picker first (Esc / Cmd-I again), then Cmd-K works.
        if self.indicator_picker.read(cx).is_open() {
            return;
        }
        let intent = match action.kind.as_ref() {
            "watchlist" => PickerIntent::AddToWatchlist,
            _ => {
                // Resolve chart target: prefer LastFocusedChart, fall back to
                // the first Chart panel in the dock. No charts -> no-op.
                let chart = cx
                    .global::<LastFocusedChart>()
                    .0
                    .borrow()
                    .clone()
                    .and_then(|w| w.upgrade())
                    .filter(|e| e.read(cx).kind() == Kind::Chart);
                let chart = chart
                    .or_else(|| find_first_chart(&self.dock_area.read(cx).center().clone(), cx));
                let Some(chart) = chart else {
                    return;
                };
                // Cache the resolved chart so subsequent shortcut presses keep
                // hitting the same panel.
                let weak = chart.downgrade();
                *cx.global::<LastFocusedChart>().0.borrow_mut() = Some(weak.clone());
                PickerIntent::SwitchChart { target: weak }
            }
        };
        let picker = self.symbol_picker.clone();
        picker.update(cx, |p, cx| p.open(intent, window, cx));
    }

    /// Open the floating settings panel for an indicator. If the panel is
    /// already up, retarget the existing view (no flicker, drag/resize
    /// state preserved). Resolves the target chart via `LastFocusedChart`.
    fn on_open_indicator_settings(
        &mut self,
        action: &OpenIndicatorSettings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let instance_id = action.0;
        let chart = cx
            .global::<LastFocusedChart>()
            .0
            .borrow()
            .clone()
            .and_then(|w| w.upgrade())
            .filter(|e| e.read(cx).kind() == Kind::Chart);
        let chart = chart
            .or_else(|| find_first_chart(&self.dock_area.read(cx).center().clone(), cx));
        let Some(chart) = chart else {
            return;
        };
        let target = chart.downgrade();
        *cx.global::<LastFocusedChart>().0.borrow_mut() = Some(target.clone());
        if let Some(slot) = self.indicator_settings.as_ref() {
            slot.view.update(cx, |v, cx| {
                v.retarget(target, instance_id, window, cx);
            });
            let focus = slot.view.read(cx).focus_handle(cx);
            focus.focus(window, cx);
            return;
        }
        let view =
            cx.new(|cx| IndicatorSettingsView::new(target, instance_id, window, cx));
        let content: AnyView = view.clone().into();
        let win = cx.new(|cx| FloatingWindow::new("Indicator Settings", content, window, cx));
        cx.subscribe_in(
            &win,
            window,
            |this, _, _: &DismissEvent, window, cx| {
                this.close_indicator_settings(window, cx);
            },
        )
        .detach();
        let focus = view.read(cx).focus_handle(cx);
        focus.focus(window, cx);
        self.indicator_settings = Some(FloatingIndicatorSettingsSlot { window: win, view });
        cx.notify();
    }

    fn close_indicator_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.indicator_settings.take().is_some() {
            self.reclaim_focus(window, cx);
            cx.notify();
        }
    }

    /// Open (or toggle-close) the indicator picker, targeting whichever
    /// chart panel is currently focused (or the first chart in the dock as
    /// a fallback). Mirrors `on_open_symbol_picker`.
    fn on_open_indicator_picker(
        &mut self,
        _: &OpenIndicatorPicker,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.indicator_picker.read(cx).is_open() {
            self.indicator_picker.update(cx, |p, cx| p.close(cx));
            return;
        }
        // Mirror the symbol picker's mutual-exclusion gate: one modal at
        // a time. If the symbol picker is up, Cmd-I is a no-op until it
        // closes.
        if self.symbol_picker.read(cx).is_open() {
            return;
        }
        let chart = cx
            .global::<LastFocusedChart>()
            .0
            .borrow()
            .clone()
            .and_then(|w| w.upgrade())
            .filter(|e| e.read(cx).kind() == Kind::Chart);
        let chart = chart
            .or_else(|| find_first_chart(&self.dock_area.read(cx).center().clone(), cx));
        let Some(chart) = chart else {
            return;
        };
        let weak = chart.downgrade();
        *cx.global::<LastFocusedChart>().0.borrow_mut() = Some(weak.clone());
        let intent = IndicatorPickerIntent { target: weak };
        let picker = self.indicator_picker.clone();
        picker.update(cx, |p, cx| p.open(intent, window, cx));
    }

    // ----- Drawing actions -----

    fn on_delete_selected_drawing(
        &mut self,
        _: &crate::drawings::actions::DeleteSelectedDrawing,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Workspace fallback for the no-context binding. The chart panel's
        // own handler (`ContentPanel::on_delete_selected_drawing`) still
        // catches it when a chart is focused; this catches it everywhere
        // else (e.g. the Objects popover).
        let svc = cx
            .global::<crate::drawings::service::DrawingServiceHandle>()
            .0
            .clone();
        svc.update(cx, |s, cx| {
            s.delete_selected(cx);
        });
    }

    fn on_set_active_tool(
        &mut self,
        action: &crate::drawings::actions::SetActiveTool,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tool) = crate::drawings::tool::Tool::from_id(action.0.as_ref()) else {
            return;
        };
        crate::drawings::tool::set_current_tool(tool, cx);
    }

    fn on_select_drawing(
        &mut self,
        action: &crate::drawings::actions::SelectDrawing,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let symbol = action.symbol.clone();
        let id = action.id;
        let svc = cx
            .global::<crate::drawings::service::DrawingServiceHandle>()
            .0
            .clone();
        svc.update(cx, |s, cx| s.set_selected(Some((symbol, id)), cx));
    }

    fn on_delete_drawing(
        &mut self,
        action: &crate::drawings::actions::DeleteDrawing,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let symbol = action.symbol.clone();
        let id = action.id;
        let svc = cx
            .global::<crate::drawings::service::DrawingServiceHandle>()
            .0
            .clone();
        svc.update(cx, |s, cx| {
            s.delete(symbol.as_ref(), id, cx);
        });
    }

    fn on_toggle_drawing_hidden(
        &mut self,
        action: &crate::drawings::actions::ToggleDrawingHidden,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let symbol = action.symbol.clone();
        let id = action.id;
        let svc = cx
            .global::<crate::drawings::service::DrawingServiceHandle>()
            .0
            .clone();
        svc.update(cx, |s, cx| s.toggle_hidden(symbol.as_ref(), id, cx));
    }

    fn on_toggle_drawing_tf_filter(
        &mut self,
        action: &crate::drawings::actions::ToggleDrawingTfFilter,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tf) = crate::services::market_data::Timeframe::from_str(action.tf.as_ref()) else {
            return;
        };
        let symbol = action.symbol.clone();
        let id = action.id;
        let svc = cx
            .global::<crate::drawings::service::DrawingServiceHandle>()
            .0
            .clone();
        svc.update(cx, |s, cx| s.toggle_tf_filter(symbol.as_ref(), id, tf, cx));
    }

    fn on_reset_drawing_tf_filter(
        &mut self,
        action: &crate::drawings::actions::ResetDrawingTfFilter,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let symbol = action.symbol.clone();
        let id = action.id;
        let svc = cx
            .global::<crate::drawings::service::DrawingServiceHandle>()
            .0
            .clone();
        svc.update(cx, |s, cx| s.reset_tf_filter(symbol.as_ref(), id, cx));
    }

    fn on_clear_chart_drawings(
        &mut self,
        _: &crate::drawings::actions::ClearChartDrawings,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Workspace fallback: when dispatched from the Objects popover (no
        // chart panel in the focus chain), resolve the focused chart's
        // symbol from `LastFocusedChart` and clear via the service. The
        // panel-scoped handler in `ContentPanel` still catches it when the
        // user dispatches from the chart's right-click menu.
        let symbol: Option<SharedString> = cx
            .try_global::<LastFocusedChart>()
            .and_then(|g| g.0.borrow().clone())
            .and_then(|w| w.upgrade())
            .and_then(|p| {
                let p = p.read(cx);
                if p.kind() == Kind::Chart {
                    p.chart_state.as_ref().map(|s| s.symbol().clone())
                } else {
                    None
                }
            });
        let Some(symbol) = symbol else { return };
        let svc = cx
            .global::<crate::drawings::service::DrawingServiceHandle>()
            .0
            .clone();
        svc.update(cx, |s, cx| {
            s.clear_symbol(symbol.as_ref(), cx);
        });
    }

    fn on_edit_horizontal_ray_text(
        &mut self,
        action: &crate::drawings::actions::EditHorizontalRayText,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use gpui_component::{
            WindowExt as _,
            dialog::{DialogButtonProps, DialogFooter, DialogHeader, DialogTitle},
            input::{Input, InputState},
            v_flex,
        };

        let symbol = action.symbol.clone();
        let id = action.id;
        let svc = cx
            .global::<crate::drawings::service::DrawingServiceHandle>()
            .0
            .clone();
        // Pull the current label so the dialog opens pre-populated. Look up
        // happens synchronously; on miss (drawing deleted between right-
        // click and dispatch) just bail.
        let existing_text: Option<String> = {
            let svc_read = svc.read(cx);
            svc_read
                .for_symbol(symbol.as_ref())
                .iter()
                .find(|d| d.id == id)
                .and_then(|d| match &d.shape {
                    crate::drawings::shapes::DrawingShape::HorizontalRay(r) => r.text.clone(),
                    _ => None,
                })
        };

        let input = cx.new(|cx| {
            let mut state = InputState::new(window, cx).placeholder("Label…");
            if let Some(t) = existing_text {
                state = state.default_value(t);
            }
            state
        });

        let input_for_dialog = input.clone();
        let svc_for_dialog = svc.clone();
        let symbol_for_dialog = symbol.clone();

        window.open_dialog(cx, move |dialog, _w, _cx| {
            let input_for_ok = input_for_dialog.clone();
            let svc_for_ok = svc_for_dialog.clone();
            let symbol_for_ok = symbol_for_dialog.clone();
            dialog
                .max_w(px(360.))
                .button_props(DialogButtonProps::default().ok_text("Save").on_ok(
                    move |_ev, _w, cx| {
                        let value = input_for_ok.read(cx).value().trim().to_string();
                        let new_text = if value.is_empty() { None } else { Some(value) };
                        svc_for_ok.update(cx, |s, cx| {
                            s.update_ray_text(symbol_for_ok.as_ref(), id, new_text, cx)
                        });
                        true
                    },
                ))
                .child(
                    v_flex()
                        .gap_4()
                        .child(
                            DialogHeader::new()
                                .px_4()
                                .pt_4()
                                .child(DialogTitle::new().child("Edit ray label")),
                        )
                        .child(
                            div()
                                .px_4()
                                .child(Input::new(&input_for_dialog).cleanable(true)),
                        )
                        .child(DialogFooter::new().px_4().pb_2()),
                )
        });
    }

    fn on_clear_all_drawings(
        &mut self,
        _: &crate::drawings::actions::ClearAllDrawings,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // No confirm dialog in v1; the workspace-wide wipe is one menu item
        // deep and trivially reversible by drawing again. If/when we add an
        // undo stack, this becomes a single-step undo. Drag in a Dialog
        // later if the user requests it.
        let svc = cx
            .global::<crate::drawings::service::DrawingServiceHandle>()
            .0
            .clone();
        svc.update(cx, |s, cx| {
            s.clear_all(cx);
        });
    }

    fn on_delete_layout(
        &mut self,
        action: &DeleteLayout,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let name = action.0.clone();
        match persistence::delete_layout(name.as_ref()) {
            Ok(()) => {
                notify_success(window, cx, "Layout deleted", format!("Deleted '{name}'"));
                self.refresh_top_bar_saved_layouts(cx);
            }
            Err(err) => {
                log::warn!("delete layout failed: {err:?}");
                notify_error(window, cx, "Delete layout", "Failed to delete layout");
            }
        }
    }

    /// Active chart layout. Read by AI tools to capture prior-state for
    /// the `set_layout` Undo path.
    pub fn chart_layout(&self) -> ChartLayout {
        self.chart_layout
    }
}

/// Global pointer to the running `TerminalWorkspace`. Set inside
/// `TerminalWorkspace::new` so non-window callers (AI tools, tests) can read
/// workspace state and dispatch layout changes without a back-reference.
#[derive(Clone)]
pub struct TerminalWorkspaceHandle(pub gpui::WeakEntity<TerminalWorkspace>);
impl gpui::Global for TerminalWorkspaceHandle {}

impl Render for TerminalWorkspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (bg, fg) = {
            let theme = cx.theme();
            (theme.background, theme.foreground)
        };
        let sheet_layer = Root::render_sheet_layer(window, cx);
        let dialog_layer = Root::render_dialog_layer(window, cx);
        let notification_layer = Root::render_notification_layer(window, cx);

        if !self.focused_once {
            self.focused_once = true;
            self.focus_handle.clone().focus(window, cx);
        }

        // Window: sidebar | (top bar + dock + bottom bar)
        div()
            .id("terminal-workspace")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::on_add_panel))
            .on_action(cx.listener(Self::on_reset_layout))
            .on_action(cx.listener(Self::on_toggle_ai_chat))
            .on_action(cx.listener(Self::on_toggle_floating_code_editor))
            .on_action(cx.listener(Self::on_toggle_trading))
            .on_action(cx.listener(Self::on_toggle_details))
            .on_action(cx.listener(Self::on_toggle_watchlist))
            .on_action(cx.listener(Self::on_ask_ai))
            .on_action(cx.listener(Self::on_set_ai_chat_model))
            .on_action(cx.listener(Self::on_apply_layout))
            .on_action(cx.listener(Self::on_save_layout))
            .on_action(cx.listener(Self::on_save_layout_current))
            .on_action(cx.listener(Self::on_manage_layouts))
            .on_action(cx.listener(Self::on_delete_layout))
            .on_action(cx.listener(Self::on_switch_mode))
            .on_action(cx.listener(Self::on_focus_symbol))
            .on_action(cx.listener(Self::on_add_watchlist_symbol))
            .on_action(cx.listener(Self::on_remove_watchlist_symbol))
            .on_action(cx.listener(Self::on_apply_chart_layout))
            .on_action(cx.listener(Self::on_set_theme))
            .on_action(cx.listener(Self::on_set_timezone))
            .on_action(cx.listener(Self::on_select_signal))
            .on_action(cx.listener(Self::on_open_symbol_picker))
            .on_action(cx.listener(Self::on_open_indicator_picker))
            .on_action(cx.listener(Self::on_open_indicator_settings))
            .on_action(cx.listener(Self::on_delete_selected_drawing))
            .on_action(cx.listener(Self::on_set_active_tool))
            .on_action(cx.listener(Self::on_select_drawing))
            .on_action(cx.listener(Self::on_clear_all_drawings))
            .on_action(cx.listener(Self::on_clear_chart_drawings))
            .on_action(cx.listener(Self::on_delete_drawing))
            .on_action(cx.listener(Self::on_toggle_drawing_hidden))
            .on_action(cx.listener(Self::on_toggle_drawing_tf_filter))
            .on_action(cx.listener(Self::on_reset_drawing_tf_filter))
            .on_action(cx.listener(Self::on_edit_horizontal_ray_text))
            .relative()
            .size_full()
            .flex()
            .flex_row()
            .bg(bg)
            .text_color(fg)
            .child(self.sidebar.clone())
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(self.top_bar.clone())
                    .child(
                        // Wrap dock_area in a relative container so the
                        // floating Code Editor overlay can absolute-position
                        // itself against the dock viewport (above the dock,
                        // below the top bar — per the spec).
                        div()
                            .flex_1()
                            .min_h_0()
                            .relative()
                            .child(self.dock_area.clone())
                            .children(
                                self.floating_code_editor
                                    .as_ref()
                                    .filter(|_| self.mode == Mode::FreeLayout)
                                    .map(|slot| slot.window.clone()),
                            )
                            // Indicator settings panel — available in any
                            // chart-bearing mode (no FreeLayout filter).
                            .children(
                                self.indicator_settings
                                    .as_ref()
                                    .map(|slot| slot.window.clone()),
                            ),
                    )
                    .child(self.bottom_bar.clone()),
            )
            .child(self.symbol_picker.clone())
            .child(self.indicator_picker.clone())
            .children(sheet_layer)
            .children(dialog_layer)
            .children(notification_layer)
    }
}

fn default_toggles(mode: Mode) -> (bool, bool) {
    match mode {
        // Portfolio: trading always on by default (built-in part of the mode).
        Mode::Portfolio => (true, false),
        // Charting: Details starts off.
        Mode::Charting => (false, false),
        _ => (false, false),
    }
}

fn apply_default_layout(
    mode: Mode,
    chart_layout: ChartLayout,
    dock_area: gpui::WeakEntity<DockArea>,
    window: &mut Window,
    cx: &mut App,
) {
    let layout = match mode {
        Mode::Charting => build_charting_layout(chart_layout, &dock_area, window, cx),
        Mode::Signal => build_signal_layout(&dock_area, window, cx),
        Mode::Research => build_research_layout(&dock_area, window, cx),
        Mode::Portfolio => build_portfolio_layout(&dock_area, window, cx),
        Mode::FreeLayout => build_free_layout(&dock_area, window, cx),
    };
    _ = dock_area.update(cx, |view, cx| {
        view.set_version(LAYOUT_VERSION, window, cx);
        view.set_center(layout, window, cx);
    });
}

fn build(kind: Kind, window: &mut Window, cx: &mut App) -> Arc<dyn PanelView> {
    panels::build_kind(kind, window, cx)
}

// ============================================================================
// Per-mode default layouts
// ============================================================================

/// Chart workspace (main, splittable per template) | Watchlist (right, locked).
/// Details is opened via the top-bar toggle on top of this base.
fn build_charting_layout(
    chart_layout: ChartLayout,
    dock_area: &gpui::WeakEntity<DockArea>,
    window: &mut Window,
    cx: &mut App,
) -> DockItem {
    let chart_workspace = build_chart_workspace(chart_layout, dock_area, window, cx);
    let watchlist = build(Kind::Watchlist, window, cx);
    DockItem::split_with_sizes(
        Axis::Horizontal,
        vec![
            chart_workspace,
            DockItem::tabs(vec![watchlist], dock_area, window, cx),
        ],
        vec![None, Some(px(280.))],
        dock_area,
        window,
        cx,
    )
}

/// Build just the chart side of the Charting layout — the structure varies
/// per template. Each chart is its own tab panel so it can hold its own
/// symbol/timeframe + sit in its own split slot.
fn build_chart_workspace(
    layout: ChartLayout,
    dock_area: &gpui::WeakEntity<DockArea>,
    window: &mut Window,
    cx: &mut App,
) -> DockItem {
    let chart_tabs = |window: &mut Window, cx: &mut App| {
        let chart = build(Kind::Chart, window, cx);
        DockItem::tabs(vec![chart], dock_area, window, cx)
    };
    match layout {
        ChartLayout::One => chart_tabs(window, cx),
        ChartLayout::TwoStacked => DockItem::split_with_sizes(
            Axis::Vertical,
            vec![chart_tabs(window, cx), chart_tabs(window, cx)],
            vec![None, None],
            dock_area,
            window,
            cx,
        ),
        ChartLayout::TwoSideBySide => DockItem::split_with_sizes(
            Axis::Horizontal,
            vec![chart_tabs(window, cx), chart_tabs(window, cx)],
            vec![None, None],
            dock_area,
            window,
            cx,
        ),
        ChartLayout::TwoByTwo => {
            let top = DockItem::split_with_sizes(
                Axis::Horizontal,
                vec![chart_tabs(window, cx), chart_tabs(window, cx)],
                vec![None, None],
                dock_area,
                window,
                cx,
            );
            let bottom = DockItem::split_with_sizes(
                Axis::Horizontal,
                vec![chart_tabs(window, cx), chart_tabs(window, cx)],
                vec![None, None],
                dock_area,
                window,
                cx,
            );
            DockItem::split_with_sizes(
                Axis::Vertical,
                vec![top, bottom],
                vec![None, None],
                dock_area,
                window,
                cx,
            )
        }
    }
}

/// Signal list (main, locked) | Signal Detail (right, locked). The detail
/// pane mirrors the selected row from the list.
fn build_signal_layout(
    dock_area: &gpui::WeakEntity<DockArea>,
    window: &mut Window,
    cx: &mut App,
) -> DockItem {
    let signal = build(Kind::Signal, window, cx);
    let detail = build(Kind::SignalDetail, window, cx);
    DockItem::split_with_sizes(
        Axis::Horizontal,
        vec![
            DockItem::tabs(vec![signal], dock_area, window, cx),
            DockItem::tabs(vec![detail], dock_area, window, cx),
        ],
        vec![None, Some(px(340.))],
        dock_area,
        window,
        cx,
    )
}

/// Main reading area (splittable, research kinds) | Watchlist (right, locked).
fn build_research_layout(
    dock_area: &gpui::WeakEntity<DockArea>,
    window: &mut Window,
    cx: &mut App,
) -> DockItem {
    let smart = build(Kind::SmartMoney, window, cx);
    let calendar = build(Kind::EconomicCalendar, window, cx);
    DockItem::tabs(vec![smart, calendar], dock_area, window, cx)
}

/// Portfolio panel filling the workspace. Position+Execution arrive via the
/// always-on Trading toggle.
fn build_portfolio_layout(
    dock_area: &gpui::WeakEntity<DockArea>,
    window: &mut Window,
    cx: &mut App,
) -> DockItem {
    let portfolio = build(Kind::Portfolio, window, cx);
    DockItem::tabs(vec![portfolio], dock_area, window, cx)
}

/// Free Layout seed: Chart (main) + small Watchlist (right). Two TabPanels are
/// required so `TabPanel::is_last_panel` returns false from the start —
/// otherwise the single seeded panel can't be dragged or split-docked, which
/// broke "drag to dock" after Reset until the user toggled AI Chat or
/// Execution open (each of those adds a second TabPanel).
fn build_free_layout(
    dock_area: &gpui::WeakEntity<DockArea>,
    window: &mut Window,
    cx: &mut App,
) -> DockItem {
    let chart = build(Kind::Chart, window, cx);
    let watchlist = build(Kind::Watchlist, window, cx);
    DockItem::split_with_sizes(
        Axis::Horizontal,
        vec![
            DockItem::tabs(vec![chart], dock_area, window, cx),
            DockItem::tabs(vec![watchlist], dock_area, window, cx),
        ],
        vec![None, Some(px(260.))],
        dock_area,
        window,
        cx,
    )
}

fn collect_content_panels(item: &DockItem, cx: &App, out: &mut Vec<gpui::Entity<ContentPanel>>) {
    match item {
        DockItem::Split { items, .. } => {
            for child in items {
                collect_content_panels(child, cx, out);
            }
        }
        DockItem::Tabs { items, .. } => {
            for panel in items {
                if let Ok(entity) = panel.view().downcast::<ContentPanel>() {
                    out.push(entity);
                }
            }
        }
        DockItem::Panel { view, .. } => {
            if let Ok(entity) = view.view().downcast::<ContentPanel>() {
                out.push(entity);
            }
            let _ = cx;
        }
        DockItem::Tiles { .. } => {}
    }
}

fn find_first_kind(item: &DockItem, cx: &App, kind: Kind) -> Option<Entity<ContentPanel>> {
    match item {
        DockItem::Split { items, .. } => items
            .iter()
            .find_map(|child| find_first_kind(child, cx, kind)),
        DockItem::Tabs { items, .. } => items.iter().find_map(|panel| {
            let entity = panel.view().downcast::<ContentPanel>().ok()?;
            if entity.read(cx).kind() == kind {
                Some(entity)
            } else {
                None
            }
        }),
        DockItem::Panel { view, .. } => {
            let entity = view.view().downcast::<ContentPanel>().ok()?;
            if entity.read(cx).kind() == kind {
                Some(entity)
            } else {
                None
            }
        }
        DockItem::Tiles { .. } => None,
    }
}

fn find_first_chart(item: &DockItem, cx: &App) -> Option<Entity<ContentPanel>> {
    match item {
        DockItem::Split { items, .. } => items.iter().find_map(|child| find_first_chart(child, cx)),
        DockItem::Tabs { items, .. } => items.iter().find_map(|panel| chart_from(panel, cx)),
        DockItem::Panel { view, .. } => chart_from(view, cx),
        DockItem::Tiles { .. } => None,
    }
}

/// Collect every Chart `ContentPanel` reachable from `item` in dock-walk
/// order (depth-first, in declaration order — i.e. roughly left-to-right and
/// top-to-bottom for splits). Used by the chart-layout switch to map old
/// symbols onto new chart slots.
pub(crate) fn collect_chart_panels(item: &DockItem, cx: &App) -> Vec<Entity<ContentPanel>> {
    let mut out = Vec::new();
    fn walk(item: &DockItem, cx: &App, out: &mut Vec<Entity<ContentPanel>>) {
        match item {
            DockItem::Split { items, .. } => {
                for child in items {
                    walk(child, cx, out);
                }
            }
            DockItem::Tabs { items, .. } => {
                for panel in items {
                    if let Some(chart) = chart_from(panel, cx) {
                        out.push(chart);
                    }
                }
            }
            DockItem::Panel { view, .. } => {
                if let Some(chart) = chart_from(view, cx) {
                    out.push(chart);
                }
            }
            DockItem::Tiles { .. } => {}
        }
    }
    walk(item, cx, &mut out);
    out
}

fn chart_from(panel: &Arc<dyn PanelView>, cx: &App) -> Option<Entity<ContentPanel>> {
    let entity = panel.view().downcast::<ContentPanel>().ok()?;
    if entity.read(cx).kind() == Kind::Chart {
        Some(entity)
    } else {
        None
    }
}

fn adopt_from_item(item: &DockItem, cx: &App, ws: &mut TerminalWorkspace) {
    match item {
        DockItem::Split { items, .. } => {
            for child in items {
                adopt_from_item(child, cx, ws);
            }
        }
        DockItem::Tabs { items, .. } => {
            for panel in items {
                adopt_panel(panel, cx, ws);
            }
        }
        DockItem::Panel { view, .. } => adopt_panel(view, cx, ws),
        DockItem::Tiles { .. } => {}
    }
}

fn notify_success(window: &mut Window, cx: &mut App, title: &str, body: String) {
    window.push_notification(
        gpui_component::notification::Notification::success(SharedString::from(body)).title(title),
        cx,
    );
}

fn notify_error(window: &mut Window, cx: &mut App, title: &str, body: &str) {
    window.push_notification(
        gpui_component::notification::Notification::error(SharedString::from(body)).title(title),
        cx,
    );
}

fn notify_warning(window: &mut Window, cx: &mut App, title: &str, body: &str) {
    window.push_notification(
        gpui_component::notification::Notification::warning(SharedString::from(body)).title(title),
        cx,
    );
}

fn notify_info(window: &mut Window, cx: &mut App, title: &str, body: &str) {
    window.push_notification(
        gpui_component::notification::Notification::info(SharedString::from(body)).title(title),
        cx,
    );
}

fn adopt_panel(panel: &Arc<dyn PanelView>, cx: &App, ws: &mut TerminalWorkspace) {
    let Ok(entity) = panel.view().downcast::<ContentPanel>() else {
        return;
    };
    let kind = entity.read(cx).kind();
    match kind {
        Kind::AiChat => {
            if ws.ai_chat_panel.is_none() {
                ws.ai_chat_panel = Some(entity);
            }
        }
        Kind::Position => {
            if ws.position_panel.is_none() {
                ws.position_panel = Some(entity);
            }
        }
        Kind::Execution => {
            if ws.execution_panel.is_none() {
                ws.execution_panel = Some(entity);
            }
        }
        Kind::Details if ws.mode == Mode::Charting => {
            if ws.details_panel.is_none() {
                ws.details_panel = Some(entity);
            }
        }
        Kind::Watchlist => {
            if ws.watchlist_panel.is_none() {
                ws.watchlist_panel = Some(entity);
            }
        }
        _ => {}
    }
}

fn pin_panel_tab(
    entity: &Entity<ContentPanel>,
    window: &mut Window,
    cx: &mut Context<TerminalWorkspace>,
) {
    let weak = entity.downgrade();
    window.defer(cx, move |window, cx| {
        let Some(entity) = weak.upgrade() else { return };
        let Some(tp) = entity.read(cx).parent_tab_panel().and_then(|w| w.upgrade()) else {
            return;
        };
        tp.update(cx, |tp, cx| tp.set_pinned(true, window, cx));
    });
}

/// Drop globally-managed singletons (today: AI Chat) from a serialized layout
/// before persisting. AI Chat is re-attached on mode entry, so leaving it in
/// the per-mode blob would cause duplicates.
fn prune_globals_from_state(state: PanelState) -> Option<PanelState> {
    if state.children.is_empty()
        && matches!(state.info, PanelInfo::Panel(_))
        && Kind::from_id(&state.panel_name) == Some(Kind::AiChat)
    {
        return None;
    }

    let original_sizes: Option<Vec<gpui::Pixels>> = match &state.info {
        PanelInfo::Stack { sizes, .. } => Some(sizes.clone()),
        _ => None,
    };
    let original_active_ix = match &state.info {
        PanelInfo::Tabs { active_index } => Some(*active_index),
        _ => None,
    };

    let mut kept_children = Vec::new();
    let mut kept_sizes = Vec::new();
    for (i, child) in state.children.into_iter().enumerate() {
        if let Some(pruned) = prune_globals_from_state(child) {
            kept_children.push(pruned);
            if let Some(sizes) = &original_sizes {
                if let Some(s) = sizes.get(i) {
                    kept_sizes.push(*s);
                }
            }
        }
    }

    if matches!(state.info, PanelInfo::Stack { .. } | PanelInfo::Tabs { .. })
        && kept_children.is_empty()
    {
        return None;
    }

    let info = match state.info {
        PanelInfo::Stack { axis, .. } => PanelInfo::Stack {
            sizes: kept_sizes,
            axis,
        },
        PanelInfo::Tabs { .. } => {
            let max_ix = kept_children.len().saturating_sub(1);
            PanelInfo::Tabs {
                active_index: original_active_ix.unwrap_or(0).min(max_ix),
            }
        }
        other => other,
    };

    Some(PanelState {
        panel_name: state.panel_name,
        children: kept_children,
        info,
    })
}

fn dock_state_without_globals(mut state: DockAreaState) -> DockAreaState {
    if !panel_tree_has_global(&state.center) {
        return state;
    }
    if let Some(pruned) = prune_globals_from_state(state.center.clone()) {
        state.center = pruned;
    }
    state
}

fn panel_tree_has_global(state: &PanelState) -> bool {
    if matches!(state.info, PanelInfo::Panel(_))
        && Kind::from_id(&state.panel_name) == Some(Kind::AiChat)
    {
        return true;
    }
    state.children.iter().any(panel_tree_has_global)
}
