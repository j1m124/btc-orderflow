use crate::persistence::{ChartLayout, Mode, SavedLayouts};
use gpui::{
    Action, Context, IntoElement, ParentElement as _, Render, SharedString, Styled as _, Window,
    actions, div, px,
};
use gpui_component::{
    ActiveTheme as _, IconName, Selectable as _, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    menu::DropdownMenu as _,
};
use serde::Deserialize;

use crate::panels::{Kind, PANEL_KINDS};

actions!(
    terminal_demo,
    [
        ResetLayout,
        ToggleAiChat,
        ToggleTrading,
        ToggleDetails,
        ToggleWatchlist,
        SaveLayout,
        SaveLayoutCurrent,
        ManageLayouts
    ]
);

#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = terminal_demo, no_json)]
pub struct AddPanel(pub SharedString);

#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = terminal_demo, no_json)]
pub struct AskAi(pub SharedString);

#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = terminal_demo, no_json)]
pub struct ApplyLayout(pub SharedString);

#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = terminal_demo, no_json)]
pub struct DeleteLayout(pub SharedString);

/// Dispatched by the watchlist when a row is clicked. The workspace routes it
/// to whichever chart the user last focused (falling back to the first chart
/// in the dock if none has been touched yet). Carries the ticker.
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = terminal_demo, no_json)]
pub struct FocusSymbol(pub SharedString);

/// Dispatched by the watchlist to add a ticker. The workspace forwards to
/// the WatchlistService and pops a notification.
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = terminal_demo, no_json)]
pub struct AddWatchlistSymbol(pub SharedString);

/// Dispatched by the watchlist's per-row × button.
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = terminal_demo, no_json)]
pub struct RemoveWatchlistSymbol(pub SharedString);

/// Dispatched by the Charting mode's "Chart layout" dropdown. The string is
/// `ChartLayout::id()`.
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = terminal_demo, no_json)]
pub struct ApplyChartLayout(pub SharedString);

/// Dispatched by the Settings dialog's Theme dropdown. Carries the
/// `ThemeConfig::name` (e.g. `"Dracula"`). The workspace routes it to
/// `crate::themes::apply_theme_by_name` and persists the choice.
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = terminal_demo, no_json)]
pub struct SetTheme(pub SharedString);

/// Dispatched when the user clicks a signal row. Carries the ticker; the
/// workspace forwards to `SignalService::select` and the detail panel
/// repaints via the `SignalEvent::SelectionChanged` subscription.
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = terminal_demo, no_json)]
pub struct SelectSignal(pub SharedString);

/// Dispatched by the AI Chat panel's model dropdown. Carries the model id
/// (e.g. `claude-sonnet-4-6`). The workspace forwards to
/// `AiChatService::set_model` on the active session.
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = terminal_demo, no_json)]
pub struct SetAiChatModel(pub SharedString);

pub struct TopBar {
    title: SharedString,
    /// Active mode. Drives which controls render in the top bar.
    mode: Mode,
    /// Toggle visual state for AI Chat (global across modes).
    ai_chat_open: bool,
    /// Toggle visual state for Trading (per-mode).
    trading_open: bool,
    /// Toggle visual state for Details (Charting only).
    details_open: bool,
    /// Toggle visual state for the Watchlist panel.
    watchlist_open: bool,
    /// Currently selected chart layout (Charting only). Surfaces as the
    /// label on the layout dropdown.
    chart_layout: ChartLayout,
    /// Cached saved-layouts list used to render the Free Layout's "Saved
    /// Layouts" dropdown. Refreshed by the workspace after each save/delete.
    saved_layouts: SavedLayouts,
}

impl TopBar {
    pub fn new(
        title: impl Into<SharedString>,
        mode: Mode,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // Re-render whenever the global drawing tool changes so the "Draw"
        // button label tracks the armed tool. The active-tool indicator is
        // the only top-bar surface that needs to react to tool changes —
        // chart canvases pull the tool fresh on each mouse event.
        if let Some(handle) = cx
            .try_global::<crate::drawings::tool::DrawingToolStateHandle>()
            .cloned()
        {
            cx.subscribe(
                &handle.0,
                |_, _, _ev: &crate::drawings::tool::DrawingToolEvent, cx| {
                    cx.notify();
                },
            )
            .detach();
        }
        // Re-render on DrawingService changes so the Objects dropdown count /
        // labels stay fresh if/when we surface them on the button itself
        // (currently the menu builds fresh on open, so this is just future-
        // proofing the label).
        if let Some(handle) = cx
            .try_global::<crate::drawings::service::DrawingServiceHandle>()
            .cloned()
        {
            cx.subscribe(
                &handle.0,
                |_, _, _ev: &crate::drawings::service::DrawingEvent, cx| {
                    cx.notify();
                },
            )
            .detach();
        }
        Self {
            title: title.into(),
            mode,
            ai_chat_open: false,
            trading_open: false,
            details_open: false,
            watchlist_open: false,
            chart_layout: ChartLayout::default(),
            saved_layouts: crate::persistence::load_layouts(),
        }
    }

    pub fn set_chart_layout(&mut self, layout: ChartLayout, cx: &mut Context<Self>) {
        if self.chart_layout == layout {
            return;
        }
        self.chart_layout = layout;
        cx.notify();
    }

    pub fn set_mode(&mut self, mode: Mode, cx: &mut Context<Self>) {
        if self.mode == mode {
            return;
        }
        self.mode = mode;
        cx.notify();
    }

    pub fn set_ai_chat_open(&mut self, open: bool, cx: &mut Context<Self>) {
        if self.ai_chat_open == open {
            return;
        }
        self.ai_chat_open = open;
        cx.notify();
    }

    pub fn set_trading_open(&mut self, open: bool, cx: &mut Context<Self>) {
        if self.trading_open == open {
            return;
        }
        self.trading_open = open;
        cx.notify();
    }

    pub fn set_details_open(&mut self, open: bool, cx: &mut Context<Self>) {
        if self.details_open == open {
            return;
        }
        self.details_open = open;
        cx.notify();
    }

    pub fn set_watchlist_open(&mut self, open: bool, cx: &mut Context<Self>) {
        if self.watchlist_open == open {
            return;
        }
        self.watchlist_open = open;
        cx.notify();
    }

    /// Re-read saved layouts from persistence. Workspace calls this after
    /// every save/delete so the dropdown stays in sync without paying a disk
    /// read per render.
    pub fn refresh_saved_layouts(&mut self, cx: &mut Context<Self>) {
        self.saved_layouts = crate::persistence::load_layouts();
        cx.notify();
    }

    fn render_mode_controls(&self, cx: &mut Context<Self>) -> impl IntoElement {
        match self.mode {
            Mode::Charting => self.render_charting_controls(cx).into_any_element(),
            Mode::Signal => self.render_signal_controls(cx).into_any_element(),
            Mode::Research => self.render_research_controls(cx).into_any_element(),
            Mode::Portfolio => self.render_portfolio_controls(cx).into_any_element(),
            Mode::FreeLayout => self.render_free_layout_controls(cx).into_any_element(),
        }
    }

    fn render_signal_controls(&self, _cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .gap_2()
            .items_center()
            .child(
                Button::new("signal-tier")
                    .label("All tiers")
                    .small()
                    .ghost()
                    .tooltip("Score tier filter (coming soon)"),
            )
            .child(
                Button::new("signal-direction")
                    .label("All directions")
                    .small()
                    .ghost()
                    .tooltip("Long / short filter (coming soon)"),
            )
            .child(
                Button::new("signal-refresh")
                    .label("Refresh")
                    .small()
                    .ghost()
                    .tooltip("Re-run the signal engine (coming soon)"),
            )
    }

    fn render_charting_controls(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let current = self.chart_layout;
        let layout_label = SharedString::from(format!("Layout: {}", current.display()));
        let layout_dropdown = Button::new("charting-layout")
            .label(layout_label)
            .small()
            .ghost()
            .tooltip("Pick a chart workspace layout")
            .dropdown_menu(move |menu, _, _| {
                let mut menu = menu.label("Chart layout");
                for layout in ChartLayout::ALL {
                    let prefix = if *layout == current { "✓ " } else { "  " };
                    let label = SharedString::from(format!("{}{}", prefix, layout.display()));
                    menu = menu.menu(
                        label,
                        Box::new(ApplyChartLayout(SharedString::from(layout.id()))),
                    );
                }
                menu
            });

        // Draw popover: lists every tool. The active tool is checkmarked. The
        // button label changes when a non-Select tool is armed so the user
        // sees they're "in drawing mode" from anywhere in the workspace.
        let active_tool = crate::drawings::tool::current_tool(cx);
        let draw_label = if active_tool.is_drawing_tool() {
            SharedString::from(format!("Draw: {}", active_tool.label()))
        } else {
            SharedString::from("Draw")
        };
        let draw_dropdown = Button::new("topbar-draw")
            .label(draw_label)
            .small()
            .ghost()
            .tooltip("Drawing tools")
            .dropdown_menu(move |menu, _, _| {
                let mut menu = menu.label("Tools");
                for tool in crate::drawings::tool::Tool::ALL {
                    let prefix = if *tool == active_tool { "✓ " } else { "  " };
                    let label = SharedString::from(format!("{}{}", prefix, tool.label()));
                    menu = menu.menu(
                        label,
                        Box::new(crate::drawings::actions::SetActiveTool(SharedString::from(
                            tool.id(),
                        ))),
                    );
                }
                menu
            });

        // Objects popover: drawings on the focused chart's symbol. Closure
        // runs fresh each time the menu opens, so it always reads the latest
        // service + focused chart. Each drawing is a submenu with Show/Hide,
        // a per-TF "Visible on" submenu, and Delete. Footer holds the two
        // scoped clear actions.
        let objects_dropdown = Button::new("topbar-objects")
            .label("Objects")
            .small()
            .ghost()
            .tooltip("Drawings on focused chart")
            .dropdown_menu(|menu, window, cx| {
                let mut menu = menu;
                let focused_symbol: Option<SharedString> = {
                    let last = cx.try_global::<crate::panels::LastFocusedChart>();
                    last.and_then(|g| g.0.borrow().clone())
                        .and_then(|w| w.upgrade())
                        .and_then(|p| p.read(cx).chart_state.as_ref().map(|s| s.symbol().clone()))
                };
                let svc = cx
                    .global::<crate::drawings::service::DrawingServiceHandle>()
                    .0
                    .clone();
                let symbol = match focused_symbol {
                    Some(s) => s,
                    None => {
                        // No focused chart: surface a workspace-wide clear so
                        // the user can still purge drawings without re-
                        // focusing a panel. Count totals first so we can hide
                        // the action when there's nothing to clear.
                        let (total_drawings, total_symbols) = {
                            let svc_read = svc.read(cx);
                            let map = svc_read.by_symbol();
                            let drawings: usize = map.values().map(|v| v.len()).sum();
                            let symbols = map.values().filter(|v| !v.is_empty()).count();
                            (drawings, symbols)
                        };
                        menu = menu.label("No focused chart");
                        if total_drawings == 0 {
                            return menu.label("  (no drawings to clear)");
                        }
                        return menu.separator().menu(
                            SharedString::from(format!(
                                "Clear all drawings ({total_drawings} on {total_symbols} symbol{})",
                                if total_symbols == 1 { "" } else { "s" },
                            )),
                            Box::new(crate::drawings::actions::ClearAllDrawings),
                        );
                    }
                };
                // Pull a Vec of (id, label, hidden, tf_filter, origin) so the
                // submenu builders can run without re-borrowing the service.
                // The `origin` tag drives the small "[AI]" prefix below.
                let drawings_meta: Vec<(
                    u64,
                    String,
                    bool,
                    Option<std::collections::BTreeSet<String>>,
                    crate::drawings::shapes::DrawingOrigin,
                )> = {
                    let svc_read = svc.read(cx);
                    svc_read
                        .for_symbol(symbol.as_ref())
                        .iter()
                        .map(|d| (d.id, d.label(), d.hidden, d.tf_filter.clone(), d.created_by))
                        .collect()
                };
                menu = menu.label(SharedString::from(format!("Drawings on {symbol}")));
                if drawings_meta.is_empty() {
                    menu = menu.label("  (none)");
                }
                for (id, label, hidden, tf_filter, origin) in drawings_meta {
                    let label = match origin {
                        crate::drawings::shapes::DrawingOrigin::Ai => format!("[AI] {label}"),
                        crate::drawings::shapes::DrawingOrigin::User => label,
                    };
                    let symbol_for_row = symbol.clone();
                    menu = menu.submenu(
                        SharedString::from(label),
                        window,
                        cx,
                        move |sub, window, cx| {
                            let mut sub = sub;
                            sub = sub.menu(
                                SharedString::from("Select"),
                                Box::new(crate::drawings::actions::SelectDrawing {
                                    symbol: symbol_for_row.clone(),
                                    id,
                                }),
                            );
                            sub = sub.menu(
                                SharedString::from(if hidden { "Show" } else { "Hide" }),
                                Box::new(crate::drawings::actions::ToggleDrawingHidden {
                                    symbol: symbol_for_row.clone(),
                                    id,
                                }),
                            );
                            // "Visible on" submenu: 5 timeframes, each
                            // checkmarked iff the filter is None (visible-all)
                            // or contains the TF.
                            let tf_filter_for_sub = tf_filter.clone();
                            let symbol_for_sub = symbol_for_row.clone();
                            sub = sub.submenu(
                                SharedString::from("Visible on"),
                                window,
                                cx,
                                move |vis, _window, _cx| {
                                    let mut vis = vis;
                                    for tf in crate::services::market_data::Timeframe::ALL {
                                        let checked = match &tf_filter_for_sub {
                                            None => true,
                                            Some(set) => set.contains(tf.as_str()),
                                        };
                                        let prefix = if checked { "✓ " } else { "  " };
                                        let label = SharedString::from(format!(
                                            "{}{}",
                                            prefix,
                                            tf.as_str()
                                        ));
                                        vis = vis.menu(
                                            label,
                                            Box::new(
                                                crate::drawings::actions::ToggleDrawingTfFilter {
                                                    symbol: symbol_for_sub.clone(),
                                                    id,
                                                    tf: SharedString::from(tf.as_str()),
                                                },
                                            ),
                                        );
                                    }
                                    vis = vis.separator().menu(
                                        SharedString::from("Visible on all"),
                                        Box::new(crate::drawings::actions::ResetDrawingTfFilter {
                                            symbol: symbol_for_sub.clone(),
                                            id,
                                        }),
                                    );
                                    vis
                                },
                            );
                            sub = sub.separator().menu(
                                SharedString::from("Delete"),
                                Box::new(crate::drawings::actions::DeleteDrawing {
                                    symbol: symbol_for_row.clone(),
                                    id,
                                }),
                            );
                            sub
                        },
                    );
                }
                menu.separator()
                    .menu(
                        SharedString::from(format!("Clear drawings on {symbol}")),
                        Box::new(crate::drawings::actions::ClearChartDrawings),
                    )
                    .menu(
                        SharedString::from("Clear all drawings (every symbol)"),
                        Box::new(crate::drawings::actions::ClearAllDrawings),
                    )
            });

        h_flex()
            .gap_2()
            .items_center()
            .child(layout_dropdown)
            .child(draw_dropdown)
            .child(objects_dropdown)
    }

    fn render_research_controls(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        // Stubs: cross-cutting filters land in a follow-up; the controls are here
        // so the mode's top-bar shape is correct from day one.
        h_flex()
            .gap_2()
            .items_center()
            .child(
                Button::new("research-symbol")
                    .label("All symbols")
                    .small()
                    .ghost()
                    .tooltip("Filter research to a ticker (coming soon)"),
            )
            .child(
                Button::new("research-time")
                    .label("Today")
                    .small()
                    .ghost()
                    .tooltip("Time range (coming soon)"),
            )
            .child(
                Button::new("research-source")
                    .label("All sources")
                    .small()
                    .ghost()
                    .tooltip("Source / category filter (coming soon)"),
            )
            .child(div().w(px(1.)).h(px(16.)).bg(muted).opacity(0.3))
    }

    fn render_portfolio_controls(&self, _cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .gap_2()
            .items_center()
            .child(
                Button::new("portfolio-account")
                    .label("Main account")
                    .small()
                    .ghost()
                    .tooltip("Account selector (coming soon)"),
            )
            .child(
                Button::new("portfolio-period")
                    .label("Today")
                    .small()
                    .ghost()
                    .tooltip("P/L period (coming soon)"),
            )
            .child(
                Button::new("portfolio-currency")
                    .label("USD")
                    .small()
                    .ghost()
                    .tooltip("Currency / unit (coming soon)"),
            )
    }

    fn render_free_layout_controls(&self, _cx: &mut Context<Self>) -> impl IntoElement {
        let add_menu = Button::new("add-panel")
            .label("+ Panel")
            .small()
            .ghost()
            .dropdown_menu(|menu, _, _| {
                let mut menu = menu;
                for kind in PANEL_KINDS.iter().filter(|k| !k.is_singleton()) {
                    if !kind.allowed_in_mode(Mode::FreeLayout) {
                        continue;
                    }
                    menu = menu.menu(
                        kind.display(),
                        Box::new(AddPanel(SharedString::from(kind.id()))),
                    );
                }
                // Floating overlays live outside PANEL_KINDS — they're not
                // dockable. Group them under a separator so it's clear the
                // entries below don't drop into the dock area.
                menu = menu.separator().label("Floating");
                menu = menu.menu(
                    "Code Editor",
                    Box::new(crate::floating_code_editor::ToggleFloatingCodeEditor),
                );
                menu
            });

        let saved_names: Vec<SharedString> = self
            .saved_layouts
            .keys()
            .map(|name| SharedString::from(name.clone()))
            .collect();
        let layout_menu = Button::new("layout")
            .label("Layouts")
            .small()
            .ghost()
            .dropdown_menu(move |menu, _, _| {
                let mut menu = menu
                    .menu("Save current layout…", Box::new(SaveLayout))
                    .menu("Manage layouts…", Box::new(ManageLayouts));
                if !saved_names.is_empty() {
                    menu = menu.separator().label("Saved");
                    for name in &saved_names {
                        menu = menu.menu(name.clone(), Box::new(ApplyLayout(name.clone())));
                    }
                }
                menu
            });

        h_flex()
            .gap_2()
            .items_center()
            .child(add_menu)
            .child(layout_menu)
    }
}

impl Render for TopBar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (border, fg, muted_fg, tab_bar) = {
            let theme = cx.theme();
            (
                theme.border,
                theme.foreground,
                theme.muted_foreground,
                theme.tab_bar,
            )
        };
        let mode_label = SharedString::from(self.mode.display());

        let mode_controls = self.render_mode_controls(cx);

        let trading_btn = {
            let is_open = self.trading_open;
            Button::new("trading")
                .icon(IconName::SquareTerminal)
                .small()
                .ghost()
                .selected(is_open)
                .tooltip("Toggle trading panels")
                .on_click(|_, window, cx| {
                    window.dispatch_action(Box::new(ToggleTrading), cx);
                })
        };

        let watchlist_btn = {
            let is_open = self.watchlist_open;
            Button::new("watchlist")
                .icon(IconName::Star)
                .small()
                .ghost()
                .selected(is_open)
                .tooltip("Toggle watchlist")
                .on_click(|_, window, cx| {
                    window.dispatch_action(Box::new(ToggleWatchlist), cx);
                })
        };

        let ai_chat_btn = {
            let is_open = self.ai_chat_open;
            Button::new("ai-chat")
                .icon(IconName::Bot)
                .small()
                .ghost()
                .selected(is_open)
                .tooltip("Toggle AI chat panel")
                .on_click(|_, window, cx| {
                    window.dispatch_action(Box::new(ToggleAiChat), cx);
                })
        };

        let mut row = div()
            .h(px(36.))
            .w_full()
            .flex()
            .flex_row()
            .items_center()
            .px_3()
            .gap_2()
            .border_b_1()
            .border_color(border)
            .bg(tab_bar)
            .child(div().text_sm().text_color(fg).child(self.title.clone()))
            .child(div().text_xs().text_color(muted_fg).child(mode_label))
            .child(div().flex_1())
            .child(mode_controls);

        // Watchlist toggle is scoped to Charting — other modes don't host
        // a watchlist panel by convention.
        if matches!(self.mode, Mode::Charting) {
            row = row.child(watchlist_btn);
        }
        row.child(trading_btn).child(ai_chat_btn)
    }
}

/// Compatibility shim: opens the multi-section Settings dialog. The actual
/// layout lives in [`crate::settings::open`]; this exists so callers that
/// still reach for `top_bar::open_settings_dialog` keep working.
pub fn open_settings_dialog(window: &mut Window, cx: &mut gpui::App) {
    crate::settings::open(window, cx);
}

/// Suppress unused-import warnings until Kind is wired into top-bar filters.
#[allow(dead_code)]
fn _kind_marker() -> Option<Kind> {
    None
}
