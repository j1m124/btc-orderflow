use crate::persistence::SavedLayouts;
use gpui::{
    Action, Context, IntoElement, ParentElement as _, Render, SharedString, Styled as _, Window,
    actions, div, px,
};
use gpui_component::{
    ActiveTheme as _, IconName, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    menu::DropdownMenu as _,
};
use serde::Deserialize;

use crate::panels::PANEL_KINDS;

actions!(client, [ResetLayout, SaveLayout, ManageLayouts]);

#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = client, no_json)]
pub struct AddPanel(pub SharedString);

#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = client, no_json)]
pub struct ApplyLayout(pub SharedString);

#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = client, no_json)]
pub struct DeleteLayout(pub SharedString);

/// Dispatched by the watchlist when a row is clicked. The workspace routes it
/// to the focused chart (falling back to the first chart in the dock).
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = client, no_json)]
pub struct FocusSymbol(pub SharedString);

#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = client, no_json)]
pub struct AddWatchlistSymbol(pub SharedString);

#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = client, no_json)]
pub struct RemoveWatchlistSymbol(pub SharedString);

/// Dispatched by the Settings dialog's Theme dropdown.
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = client, no_json)]
pub struct SetTheme(pub SharedString);

pub struct TopBar {
    title: SharedString,
    saved_layouts: SavedLayouts,
}

impl TopBar {
    pub fn new(
        title: impl Into<SharedString>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
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
            saved_layouts: crate::persistence::load_layouts(),
        }
    }

    pub fn refresh_saved_layouts(&mut self, cx: &mut Context<Self>) {
        self.saved_layouts = crate::persistence::load_layouts();
        cx.notify();
    }

    fn render_right_controls(&self) -> impl IntoElement {
        let add_menu = Button::new("add-panel")
            .label("+ Panel")
            .small()
            .ghost()
            .dropdown_menu(|menu, _, _| {
                let mut menu = menu;
                for kind in PANEL_KINDS.iter() {
                    menu = menu.menu(
                        kind.display(),
                        Box::new(AddPanel(SharedString::from(kind.id()))),
                    );
                }
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

    fn render_left_controls(&self, cx: &mut Context<Self>) -> impl IntoElement {
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
            .child(draw_dropdown)
            .child(objects_dropdown)
    }

    fn render_settings_button(&self) -> impl IntoElement {
        Button::new("topbar-settings")
            .icon(IconName::Settings)
            .small()
            .ghost()
            .tooltip("Settings")
            .on_click(|_, window, cx| {
                crate::settings::open(window, cx);
            })
    }
}

impl Render for TopBar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (border, fg, tab_bar) = {
            let theme = cx.theme();
            (theme.border, theme.foreground, theme.tab_bar)
        };
        let left_controls = self.render_left_controls(cx);
        let right_controls = self.render_right_controls();
        let settings_btn = self.render_settings_button();

        div()
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
            .child(left_controls)
            .child(div().flex_1())
            .child(right_controls)
            .child(settings_btn)
    }
}

pub fn open_settings_dialog(window: &mut Window, cx: &mut gpui::App) {
    crate::settings::open(window, cx);
}
