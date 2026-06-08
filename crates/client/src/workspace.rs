use std::sync::Arc;
use std::time::Duration;

use gpui::{
    AnyView, App, AppContext as _, Context, DismissEvent, Entity, FocusHandle,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
    Styled as _, Task, Window, div, px,
};
use gpui_component::{
    ActiveTheme as _, Root,
    dock::{DockArea, DockAreaState, DockEvent, DockItem, DockPlacement, PanelView},
};

use crate::bottom_bar::BottomBar;
use crate::drawings::strip_content::DrawingStripContent;
use crate::floating_code_editor::{FloatingCodeEditor, ToggleFloatingCodeEditor};
use crate::floating_strip::{FloatingStrip, FloatingStripEvent};
use crate::floating_window::FloatingWindow;
use crate::indicator_picker::{
    IndicatorPickerEvent, IndicatorPickerIntent, IndicatorPickerState, OpenIndicatorPicker,
};
use crate::indicator_settings::{IndicatorSettingsView, OpenIndicatorSettings};
use crate::panels::{
    self, ChartRenderSettingsView, ContentPanel, Kind, LastFocusedChart, OpenChartRenderSettings,
};
use crate::persistence::{self, WorkspaceState};
use crate::symbol_picker::{OpenSymbolPicker, PickerEvent, PickerIntent, SymbolPickerState};
use crate::top_bar::{
    AddPanel, AddWatchlistSymbol, ApplyLayout, DeleteLayout, FocusSymbol, ManageLayouts,
    RemoveWatchlistSymbol, ResetLayout, SaveLayout, SetTheme, TopBar,
};

const LAYOUT_VERSION: usize = 5;
const DOCK_AREA_ID: &str = "main-dock";

pub struct TerminalWorkspace {
    top_bar: Entity<TopBar>,
    bottom_bar: Entity<BottomBar>,
    dock_area: Entity<DockArea>,
    last_saved: Option<DockAreaState>,
    _save_task: Option<Task<()>>,
    focus_handle: FocusHandle,
    focused_once: bool,
    symbol_picker: Entity<SymbolPickerState>,
    indicator_picker: Entity<IndicatorPickerState>,
    indicator_settings: Option<FloatingIndicatorSettingsSlot>,
    chart_render_settings: Option<FloatingChartRenderSettingsSlot>,
    floating_code_editor: Option<FloatingCodeEditorSlot>,
    drawing_settings: Option<FloatingDrawingSettingsSlot>,
    drawing_strip: Entity<FloatingStrip>,
    /// Kept alive on the workspace so the subscription wiring in
    /// `new` can call `cx.notify` on it from outside the strip itself.
    /// The strip's content view holds the same handle via `AnyView`.
    #[allow(dead_code)]
    drawing_strip_content: Entity<DrawingStripContent>,
}

struct FloatingCodeEditorSlot {
    window: Entity<FloatingWindow>,
}

struct FloatingIndicatorSettingsSlot {
    window: Entity<FloatingWindow>,
}

struct FloatingChartRenderSettingsSlot {
    window: Entity<FloatingWindow>,
    /// View handle kept so a re-dispatch of `OpenChartRenderSettings`
    /// against a different chart can retarget the existing window
    /// instead of opening a second one (matches the indicator-settings
    /// singleton semantics).
    view: Entity<ChartRenderSettingsView>,
}

struct FloatingDrawingSettingsSlot {
    window: Entity<FloatingWindow>,
    view: Entity<crate::drawings::settings_view::DrawingSettingsView>,
}

impl TerminalWorkspace {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let dock_area = cx.new(|cx| DockArea::new(DOCK_AREA_ID, Some(LAYOUT_VERSION), window, cx));
        let weak_dock = dock_area.downgrade();
        cx.set_global(panels::DockAreaHandle(weak_dock.clone()));
        let weak_self = cx.entity().downgrade();
        cx.set_global(TerminalWorkspaceHandle(weak_self));

        let workspace_state = persistence::load_workspace_state();
        let loaded = workspace_state
            .as_ref()
            .and_then(|s| s.dock.clone())
            .map(|dock_state| {
                dock_area
                    .update(cx, |dock, cx| dock.load(dock_state, window, cx))
                    .is_ok()
            })
            .unwrap_or(false);
        if !loaded {
            apply_default_layout(weak_dock.clone(), window, cx);
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

        let top_bar = cx.new(|cx| TopBar::new("btc_orderflow", window, cx));
        let bottom_bar = cx.new(|cx| BottomBar::new(window, cx));

        let symbol_picker = cx.new(|cx| SymbolPickerState::new(window, cx));
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

        let drawing_strip = cx.new(FloatingStrip::new);
        let drawing_strip_content = crate::drawings::strip_content::build(cx);
        drawing_strip.update(cx, |strip, cx| {
            strip.set_content(drawing_strip_content.clone().into(), cx);
            if let Some(pos) = persistence::load_drawing_strip_position() {
                strip.set_origin(gpui::point(px(pos.x), px(pos.y)), cx);
            }
        });
        cx.subscribe(&drawing_strip, |_this, _strip, ev: &FloatingStripEvent, _cx| {
            match ev {
                FloatingStripEvent::Moved(o) => {
                    let _ = persistence::save_drawing_strip_position(
                        persistence::DrawingStripPosition {
                            x: f32::from(o.x),
                            y: f32::from(o.y),
                        },
                    );
                }
            }
        })
        .detach();

        // Strip content emits `GearClicked` for the user's currently-selected
        // drawing. Forward to the workspace-level action so the open handler
        // can do the retarget-or-spawn dance like the indicator-settings
        // path does.
        cx.subscribe_in(
            &drawing_strip_content,
            window,
            |_this, _content, ev: &crate::drawings::strip_content::StripContentEvent, window, cx| {
                match ev {
                    crate::drawings::strip_content::StripContentEvent::GearClicked {
                        symbol,
                        id,
                    } => {
                        window.dispatch_action(
                            Box::new(crate::drawings::actions::OpenDrawingSettings {
                                symbol: symbol.clone(),
                                id: *id,
                            }),
                            cx,
                        );
                    }
                }
            },
        )
        .detach();

        // Mirror the global drawing-selection state into the strip's
        // visibility. SelectionChanged also fires on programmatic deselect
        // (TF-mismatch, ESC, etc.) so the strip naturally hides.
        if let Some(handle) = cx.try_global::<crate::drawings::service::DrawingServiceHandle>().cloned() {
            // Set initial visibility from the current selection.
            let initially_visible = handle.0.read(cx).selected().is_some();
            drawing_strip.update(cx, |strip, cx| {
                if initially_visible {
                    strip.show(cx);
                } else {
                    strip.hide(cx);
                }
            });
            let strip_for_sub = drawing_strip.clone();
            let content_for_sub = drawing_strip_content.clone();
            cx.subscribe(
                &handle.0,
                move |_this, svc, ev: &crate::drawings::service::DrawingEvent, cx| match ev {
                    crate::drawings::service::DrawingEvent::SelectionChanged => {
                        let has_sel = svc.read(cx).selected().is_some();
                        strip_for_sub.update(cx, |strip, cx| {
                            if has_sel {
                                strip.show(cx);
                            } else {
                                strip.hide(cx);
                            }
                        });
                        content_for_sub.update(cx, |_, cx| cx.notify());
                    }
                    crate::drawings::service::DrawingEvent::Changed { .. }
                    | crate::drawings::service::DrawingEvent::Wiped => {
                        // Style/visibility flag mutations on the selected
                        // drawing must repaint the strip's button chrome.
                        content_for_sub.update(cx, |_, cx| cx.notify());
                    }
                },
            )
            .detach();
        }

        Self {
            top_bar,
            bottom_bar,
            dock_area,
            last_saved: None,
            _save_task: None,
            focus_handle: cx.focus_handle(),
            focused_once: false,
            symbol_picker,
            indicator_picker,
            indicator_settings: None,
            chart_render_settings: None,
            floating_code_editor: None,
            drawing_settings: None,
            drawing_strip,
            drawing_strip_content,
        }
    }

    fn schedule_save(
        &mut self,
        dock_area: Entity<DockArea>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self._save_task = Some(cx.spawn_in(window, async move |this, window| {
            window
                .background_executor()
                .timer(Duration::from_millis(500))
                .await;
            _ = this.update_in(window, |this, _, cx| {
                let dock_state = dock_area.read(cx).dump(cx);
                if Some(&dock_state) == this.last_saved.as_ref() {
                    return;
                }
                let state = WorkspaceState {
                    dock: Some(dock_state.clone()),
                };
                if let Err(err) = persistence::save_workspace_state(&state) {
                    log::warn!("save workspace state failed: {err:?}");
                } else {
                    this.last_saved = Some(dock_state);
                }
            });
        }));
    }

    fn reclaim_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus_handle, cx);
    }

    fn on_add_panel(
        &mut self,
        action: &AddPanel,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(kind) = Kind::from_id(action.0.as_ref()) else {
            return;
        };
        let panel = panels::build_kind(kind, window, cx);
        let dock_area = self.dock_area.clone();
        dock_area.update(cx, |dock, cx| {
            dock.add_panel(panel, DockPlacement::Center, None, window, cx);
        });
    }

    fn on_reset_layout(
        &mut self,
        _: &ResetLayout,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let weak = self.dock_area.downgrade();
        apply_default_layout(weak, window, cx);
        if let Err(err) = persistence::clear_workspace_state() {
            log::warn!("clear workspace state failed: {err:?}");
        }
    }

    fn on_set_theme(
        &mut self,
        action: &SetTheme,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        crate::themes::apply_theme_by_name(action.0.as_ref(), None, cx);
        if let Err(err) = persistence::save_theme_name(action.0.as_ref()) {
            log::warn!("save theme name failed: {err:?}");
        }
    }

    fn on_save_layout(
        &mut self,
        _: &SaveLayout,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let dock_state = self.dock_area.read(cx).dump(cx);
        let name = format!(
            "Layout {}",
            chrono::Local::now().format("%Y-%m-%d %H:%M")
        );
        if let Err(err) = persistence::upsert_layout(&name, dock_state) {
            log::warn!("upsert layout failed: {err:?}");
        }
        self.top_bar.update(cx, |bar, cx| bar.refresh_saved_layouts(cx));
        notify_info(window, cx, "Layout saved", &name);
    }

    fn on_apply_layout(
        &mut self,
        action: &ApplyLayout,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let layouts = persistence::load_layouts();
        let Some(state) = layouts.get(action.0.as_ref()).cloned() else {
            return;
        };
        let _ = self
            .dock_area
            .update(cx, |dock, cx| dock.load(state, window, cx));
    }

    fn on_delete_layout(
        &mut self,
        action: &DeleteLayout,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Err(err) = persistence::delete_layout(action.0.as_ref()) {
            log::warn!("delete layout failed: {err:?}");
        }
        self.top_bar.update(cx, |bar, cx| bar.refresh_saved_layouts(cx));
    }

    fn on_manage_layouts(
        &mut self,
        _: &ManageLayouts,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        // Stub: dedicated dialog comes later. Users can delete layouts via the
        // dropdown for now.
    }

    fn on_focus_symbol(
        &mut self,
        action: &FocusSymbol,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let target = cx
            .global::<LastFocusedChart>()
            .0
            .borrow()
            .as_ref()
            .and_then(|w| w.upgrade());
        let chart = match target {
            Some(c) => c,
            None => {
                let dock = self.dock_area.read(cx);
                let center = dock.center().clone();
                let Some(c) = find_first_chart(&center, cx) else {
                    return;
                };
                c
            }
        };
        let symbol = action.0.to_string();
        chart.update(cx, |panel, cx| {
            panel.switch_chart_symbol(&symbol, cx);
        });
    }

    fn on_add_watchlist_symbol(
        &mut self,
        action: &AddWatchlistSymbol,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let svc = cx
            .global::<crate::services::watchlist::WatchlistServiceHandle>()
            .0
            .clone();
        let ticker = SharedString::from(action.0.to_string());
        let added = svc.update(cx, |s, cx| s.add(ticker.clone(), cx));
        if added {
            notify_info(window, cx, "Added to watchlist", ticker.as_ref());
        }
    }

    fn on_remove_watchlist_symbol(
        &mut self,
        action: &RemoveWatchlistSymbol,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let svc = cx
            .global::<crate::services::watchlist::WatchlistServiceHandle>()
            .0
            .clone();
        svc.update(cx, |s, cx| {
            s.remove(action.0.as_ref(), cx);
        });
    }

    fn on_open_symbol_picker(
        &mut self,
        action: &OpenSymbolPicker,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.symbol_picker.read(cx).is_open() {
            self.symbol_picker.update(cx, |p, cx| p.close(cx));
            return;
        }
        if self.indicator_picker.read(cx).is_open() {
            return;
        }
        let intent = match action.kind.as_ref() {
            "watchlist" => PickerIntent::AddToWatchlist,
            _ => {
                let chart = cx
                    .global::<LastFocusedChart>()
                    .0
                    .borrow()
                    .clone()
                    .and_then(|w| w.upgrade())
                    .filter(|e| e.read(cx).kind() == Kind::Chart);
                let chart = chart.or_else(|| {
                    find_first_chart(&self.dock_area.read(cx).center().clone(), cx)
                });
                let Some(chart) = chart else {
                    return;
                };
                let weak = chart.downgrade();
                *cx.global::<LastFocusedChart>().0.borrow_mut() = Some(weak.clone());
                PickerIntent::SwitchChart { target: weak }
            }
        };
        let picker = self.symbol_picker.clone();
        picker.update(cx, |p, cx| p.open(intent, window, cx));
    }

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
        if self.indicator_settings.is_some() {
            return;
        }
        let view = cx.new(|cx| IndicatorSettingsView::new(target, instance_id, window, cx));
        let content: AnyView = view.clone().into();
        let win =
            cx.new(|cx| FloatingWindow::new("Indicator Settings", content, window, cx));
        cx.subscribe_in(&win, window, |this, _w, _ev: &DismissEvent, _window, cx| {
            // Defer the drop: gpui_web's pointer dispatcher still holds
            // `WebWindowCallbacks` when DismissEvent fires from a click on the
            // close button, so dropping the FloatingWindow synchronously
            // panics on the next pointer move (`RefCell already borrowed`).
            let weak = cx.weak_entity();
            cx.defer(move |cx| {
                if let Some(ws) = weak.upgrade() {
                    ws.update(cx, |ws, _cx| {
                        ws.indicator_settings = None;
                    });
                }
            });
            let _ = this;
        })
        .detach();
        self.indicator_settings = Some(FloatingIndicatorSettingsSlot { window: win });
    }

    /// Open the chart-render settings panel (sibling to indicator settings,
    /// but scoped to the chart's active footprint render rather than an
    /// individual `IndicatorInstance`). Resolves the target chart via
    /// `LastFocusedChart`, falling back to the first chart in the dock so
    /// the gear is reachable even when focus has bounced elsewhere.
    /// Singleton: a second dispatch retargets the existing view instead
    /// of opening a duplicate window.
    fn on_open_chart_render_settings(
        &mut self,
        _action: &OpenChartRenderSettings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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
        if let Some(slot) = self.chart_render_settings.as_ref() {
            let view = slot.view.clone();
            view.update(cx, |v, cx| v.retarget(target, window, cx));
            return;
        }
        let view = cx.new(|cx| ChartRenderSettingsView::new(target, window, cx));
        let content: AnyView = view.clone().into();
        let win = cx.new(|cx| FloatingWindow::new("Chart Render Settings", content, window, cx));
        cx.subscribe_in(&win, window, |this, _w, _ev: &DismissEvent, _window, cx| {
            // Defer the drop: see `on_open_indicator_settings` for the
            // gpui_web RefCell-borrow panic this works around.
            let weak = cx.weak_entity();
            cx.defer(move |cx| {
                if let Some(ws) = weak.upgrade() {
                    ws.update(cx, |ws, _cx| {
                        ws.chart_render_settings = None;
                    });
                }
            });
            let _ = this;
        })
        .detach();
        self.chart_render_settings =
            Some(FloatingChartRenderSettingsSlot { window: win, view });
    }

    fn on_open_drawing_settings(
        &mut self,
        action: &crate::drawings::actions::OpenDrawingSettings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let symbol = action.symbol.clone();
        let id = action.id;
        if let Some(slot) = self.drawing_settings.as_ref() {
            let view = slot.view.clone();
            view.update(cx, |v, cx| v.retarget(symbol, id, window, cx));
            return;
        }
        let view = cx.new(|cx| {
            crate::drawings::settings_view::DrawingSettingsView::new(symbol, id, window, cx)
        });
        let content: AnyView = view.clone().into();
        let win = cx.new(|cx| FloatingWindow::new("Drawing Settings", content, window, cx));
        cx.subscribe_in(&win, window, |this, _w, _ev: &DismissEvent, _window, cx| {
            // Defer the drop: see `on_open_indicator_settings` for the
            // gpui_web RefCell-borrow panic this works around.
            let weak = cx.weak_entity();
            cx.defer(move |cx| {
                if let Some(ws) = weak.upgrade() {
                    ws.update(cx, |ws, _cx| {
                        ws.drawing_settings = None;
                    });
                }
            });
            let _ = this;
        })
        .detach();
        self.drawing_settings = Some(FloatingDrawingSettingsSlot { window: win, view });
    }

    fn on_toggle_floating_code_editor(
        &mut self,
        _: &ToggleFloatingCodeEditor,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(slot) = self.floating_code_editor.take() {
            drop(slot);
            return;
        }
        let editor = cx.new(|cx| FloatingCodeEditor::new(window, cx));
        let content: AnyView = editor.clone().into();
        let win = cx.new(|cx| FloatingWindow::new("Code Editor", content, window, cx));
        cx.subscribe_in(&win, window, |this, _w, _ev: &DismissEvent, _window, cx| {
            // See `on_open_indicator_settings` for why the drop is deferred.
            let weak = cx.weak_entity();
            cx.defer(move |cx| {
                if let Some(ws) = weak.upgrade() {
                    ws.update(cx, |ws, _cx| {
                        ws.floating_code_editor = None;
                    });
                }
            });
            let _ = this;
        })
        .detach();
        self.floating_code_editor = Some(FloatingCodeEditorSlot { window: win });
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
        // Workspace-level fallback when the action is dispatched from the
        // top-bar Objects popover (no chart panel in the focus chain). The
        // chart panel itself also has a handler — that one wins when the
        // dispatch originates from the chart's right-click menu.
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

    fn on_clear_all_drawings(
        &mut self,
        _: &crate::drawings::actions::ClearAllDrawings,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let svc = cx
            .global::<crate::drawings::service::DrawingServiceHandle>()
            .0
            .clone();
        svc.update(cx, |s, cx| {
            s.clear_all(cx);
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

    fn on_toggle_drawing_locked(
        &mut self,
        action: &crate::drawings::actions::ToggleDrawingLocked,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let symbol = action.symbol.clone();
        let id = action.id;
        let svc = cx
            .global::<crate::drawings::service::DrawingServiceHandle>()
            .0
            .clone();
        let cur = svc
            .read(cx)
            .for_symbol(symbol.as_ref())
            .iter()
            .find(|d| d.id == id)
            .map(|d| d.locked)
            .unwrap_or(false);
        svc.update(cx, |s, cx| s.set_locked(symbol.as_ref(), id, !cur, cx));
    }

    fn on_deselect_drawing(
        &mut self,
        _: &crate::drawings::actions::DeselectDrawing,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(handle) = cx
            .try_global::<crate::drawings::service::DrawingServiceHandle>()
            .cloned()
        else {
            return;
        };
        handle.0.update(cx, |s, cx| s.clear_selection(cx));
    }

    fn on_toggle_ray_extend_left(
        &mut self,
        action: &crate::drawings::actions::ToggleRayExtendLeft,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let symbol = action.symbol.clone();
        let id = action.id;
        let svc = cx
            .global::<crate::drawings::service::DrawingServiceHandle>()
            .0
            .clone();
        svc.update(cx, |s, cx| s.toggle_ray_extend_left(symbol.as_ref(), id, cx));
    }

    fn on_set_text_font_size(
        &mut self,
        action: &crate::drawings::actions::SetTextFontSize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let symbol = action.symbol.clone();
        let id = action.id;
        let size_px = action.size_px();
        let svc = cx
            .global::<crate::drawings::service::DrawingServiceHandle>()
            .0
            .clone();
        svc.update(cx, |s, cx| s.set_text_font_size(symbol.as_ref(), id, size_px, cx));
    }

    fn on_edit_drawing_label(
        &mut self,
        action: &crate::drawings::actions::EditDrawingLabel,
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
        let existing = {
            use crate::drawings::shapes::DrawingShape;
            let svc_read = svc.read(cx);
            svc_read
                .for_symbol(symbol.as_ref())
                .iter()
                .find(|d| d.id == id)
                .and_then(|d| match &d.shape {
                    DrawingShape::Line(s)
                    | DrawingShape::Rect(s)
                    | DrawingShape::Arrow(s)
                    | DrawingShape::Fibonacci(s) => s.label.clone(),
                    DrawingShape::HorizontalRay(s) => s.text.clone(),
                    DrawingShape::AnchoredVwap(s) => s.label.clone(),
                    DrawingShape::Long(p) | DrawingShape::Short(p) => p.label.clone(),
                    DrawingShape::Text(_) => None,
                })
        };

        let input = cx.new(|cx| {
            let mut state = InputState::new(window, cx).placeholder("Label…");
            if let Some(t) = existing {
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
                            s.set_label(symbol_for_ok.as_ref(), id, new_text, cx)
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
                                .child(DialogTitle::new().child("Edit label")),
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
}

#[derive(Clone)]
pub struct TerminalWorkspaceHandle(pub gpui::WeakEntity<TerminalWorkspace>);
impl gpui::Global for TerminalWorkspaceHandle {}

impl gpui::Focusable for TerminalWorkspace {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for TerminalWorkspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (bg, fg) = {
            let theme = cx.theme();
            (theme.background, theme.foreground)
        };
        let sheet_layer = Root::render_sheet_layer(window, cx);
        let dialog_layer = Root::render_dialog_layer(window, cx);
        let notification_layer = Root::render_notification_layer(window, cx);

        // Initial focus is intentionally NOT claimed during render. Claiming
        // it here queues platform work that re-enters the input dispatcher on
        // the next pointer event, panicking with `RefCell already borrowed`.
        // The workspace gains focus naturally on the user's first click.
        let _ = &self.focused_once;

        div()
            .id("workspace")
            .key_context("Workspace")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::on_add_panel))
            .on_action(cx.listener(Self::on_reset_layout))
            .on_action(cx.listener(Self::on_set_theme))
            .on_action(cx.listener(Self::on_save_layout))
            .on_action(cx.listener(Self::on_apply_layout))
            .on_action(cx.listener(Self::on_delete_layout))
            .on_action(cx.listener(Self::on_manage_layouts))
            .on_action(cx.listener(Self::on_focus_symbol))
            .on_action(cx.listener(Self::on_add_watchlist_symbol))
            .on_action(cx.listener(Self::on_remove_watchlist_symbol))
            .on_action(cx.listener(Self::on_open_symbol_picker))
            .on_action(cx.listener(Self::on_open_indicator_picker))
            .on_action(cx.listener(Self::on_open_indicator_settings))
            .on_action(cx.listener(Self::on_open_chart_render_settings))
            .on_action(cx.listener(Self::on_toggle_floating_code_editor))
            .on_action(cx.listener(Self::on_set_active_tool))
            .on_action(cx.listener(Self::on_select_drawing))
            .on_action(cx.listener(Self::on_delete_drawing))
            .on_action(cx.listener(Self::on_toggle_drawing_hidden))
            .on_action(cx.listener(Self::on_toggle_drawing_tf_filter))
            .on_action(cx.listener(Self::on_reset_drawing_tf_filter))
            .on_action(cx.listener(Self::on_clear_chart_drawings))
            .on_action(cx.listener(Self::on_clear_all_drawings))
            .on_action(cx.listener(Self::on_edit_horizontal_ray_text))
            .on_action(cx.listener(Self::on_toggle_drawing_locked))
            .on_action(cx.listener(Self::on_deselect_drawing))
            .on_action(cx.listener(Self::on_edit_drawing_label))
            .on_action(cx.listener(Self::on_open_drawing_settings))
            .on_action(cx.listener(Self::on_toggle_ray_extend_left))
            .on_action(cx.listener(Self::on_set_text_font_size))
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .bg(bg)
            .text_color(fg)
            .child(self.top_bar.clone())
            .child(
                // Relative wrapper so the dock's drag indicators + the
                // floating overlays (Code Editor, Indicator Settings) can
                // absolute-position themselves against the dock viewport.
                div()
                    .flex_1()
                    .min_h_0()
                    .relative()
                    .child(self.dock_area.clone())
                    .children(
                        self.floating_code_editor
                            .as_ref()
                            .map(|slot| slot.window.clone()),
                    )
                    .children(
                        self.indicator_settings
                            .as_ref()
                            .map(|slot| slot.window.clone()),
                    )
                    .children(
                        self.chart_render_settings
                            .as_ref()
                            .map(|slot| slot.window.clone()),
                    )
                    .children(
                        self.drawing_settings
                            .as_ref()
                            .map(|slot| slot.window.clone()),
                    )
                    .child(self.drawing_strip.clone()),
            )
            .child(self.bottom_bar.clone())
            .child(self.symbol_picker.clone())
            .child(self.indicator_picker.clone())
            .children(sheet_layer)
            .children(dialog_layer)
            .children(notification_layer)
    }
}

fn apply_default_layout(
    weak_dock: gpui::WeakEntity<DockArea>,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(dock_area) = weak_dock.upgrade() else {
        return;
    };
    let item = build_default_layout(&weak_dock, window, cx);
    dock_area.update(cx, |dock, cx| {
        dock.set_center(item, window, cx);
    });
    let _ = window;
}

fn build_default_layout(
    dock_area: &gpui::WeakEntity<DockArea>,
    window: &mut Window,
    cx: &mut App,
) -> DockItem {
    // Watchlist (left, ~240px) | Chart (right, fills).
    //
    // Two panels by default because gpui-component's TabPanel reports
    // `draggable: false` when a dock has only one panel (`is_last_panel`
    // check in tab_panel.rs). A single-panel default boots into an
    // un-draggable state, so the user can't reach the whole-window-edge
    // docking zones to add a second panel via drag. Bump LAYOUT_VERSION
    // if you change the shape so persisted single-panel layouts get reset.
    let watchlist = build(Kind::Watchlist, window, cx);
    let chart = build(Kind::Chart, window, cx);
    DockItem::split_with_sizes(
        gpui::Axis::Horizontal,
        vec![
            DockItem::tabs(vec![watchlist], dock_area, window, cx),
            DockItem::tabs(vec![chart], dock_area, window, cx),
        ],
        vec![Some(gpui::px(240.)), None],
        dock_area,
        window,
        cx,
    )
}

fn build(kind: Kind, window: &mut Window, cx: &mut App) -> Arc<dyn PanelView> {
    panels::build_kind(kind, window, cx)
}

fn find_first_chart(item: &DockItem, cx: &App) -> Option<Entity<ContentPanel>> {
    match item {
        DockItem::Split { items, .. } => items.iter().find_map(|child| find_first_chart(child, cx)),
        DockItem::Tabs { items, .. } => items.iter().find_map(|panel| chart_from(panel, cx)),
        DockItem::Panel { view, .. } => chart_from(view, cx),
        DockItem::Tiles { .. } => None,
    }
}

fn chart_from(panel: &Arc<dyn PanelView>, cx: &App) -> Option<Entity<ContentPanel>> {
    let entity = panel.view().downcast::<ContentPanel>().ok()?;
    if entity.read(cx).kind() == Kind::Chart {
        Some(entity)
    } else {
        None
    }
}

fn notify_info(window: &mut Window, cx: &mut App, title: &str, body: &str) {
    use gpui_component::{notification::Notification, WindowExt as _};
    window.push_notification(
        Notification::info(SharedString::from(body.to_string()))
            .title(SharedString::from(title.to_string())),
        cx,
    );
}
