//! The chart panel's gpui render tree and its sub-builders (indicator chips,
//! the synthetic-render chip, the OHLC readout, the indicator list). Reads
//! [`super::state::ChartState`], drives it through mouse / scroll handlers, and
//! hands paint data to the `paint/` submodules. Decomposition of `render` into
//! smaller builders is deferred to a follow-up; for now it lives here whole.

use gpui::{
    AppContext as _, ContentMask, Context, FocusHandle, Focusable as _, Hsla,
    InteractiveElement as _, IntoElement, MouseButton, MouseDownEvent, MouseMoveEvent,
    ParentElement as _, ScrollWheelEvent, SharedString,
    StatefulInteractiveElement as _, Styled as _, Window, canvas, div, prelude::FluentBuilder as _,
    px,
};
use gpui_component::{
    ActiveTheme as _, Disableable as _, ElementExt as _, IconName, Sizable as _, StyledExt as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputState},
    menu::{ContextMenuExt as _, DropdownMenu as _},
    plot::AXIS_GAP,
    v_flex,
};

use super::actions::{
    ChangeChartRender, ChangeChartTimeframe, ChangeChartVolumeUnit, GoToLatest,
    MoveIndicatorPaneDown, MoveIndicatorPaneUp, RemoveIndicator, ResetChartScale,
    ToggleChartRenderVisible, ToggleIndicatorHidden,
};
use super::coords::{
    compute_y_axis_gap, fmt_scalar, format_price, format_readout, format_user_tz, index_to_screen,
    price_to_screen, screen_to_index, screen_to_price, snap_t,
};
use super::drawing::{
    CreatingDrawing, Drawing, EditDrag, EditHandle, POSITION_DEFAULT_WIDTH_RATIO,
    TEXT_DEFAULT_WIDTH_PX, TextEditing, apply_edit, hit_test_drawings, snap_view_to_grid,
};
use super::footprint::{FootprintParams, RenderKind};
use super::drawings_view;
use super::footprint_settings::OpenChartRenderSettings;
use super::paint::{
    DrawingColors, HeatmapRect, MainChartColors, OverlayPaintItem, PanePaintItem, paint_heatmap,
    paint_main_chart, paint_overlay_indicators, paint_sub_pane, render_drawings_overlay,
};
use super::state::{
    CHART_MAX_VIEW, CHART_MIN_VIEW, CanvasDrag, ChartState, SCROLL_ZOOM_RATE, SplitterDrag,
    Y_FREEZE_DEADZONE_PX,
};
use crate::drawings::service::{DrawingId, DrawingServiceHandle};
use crate::drawings::tool::Tool;
use crate::indicators::{IndicatorInstance, IndicatorOutput, InstanceId, Placement};
use crate::panels::{ContentPanel, LastFocusedChart};
use crate::persistence::VolumeUnit;
use crate::services::market_data::{self, Candle, Timeframe};
use crate::services::symbols::SymbolsServiceHandle;
use crate::symbol_picker::OpenSymbolPicker;

/// Render a single indicator chip. Used for both the main-pane vertical
/// list (overlay indicators only) and each sub-pane's solo chip overlay
/// (the pane's lone indicator). Layout: `[label] [● eye] [⚙ gear] [× trash]`.
///
/// Body has no `cursor_pointer` and no left-click handler — settings is
/// only reachable via the gear button or the right-click "Settings…" item.
/// A subtle hover bg tint marks the chip as an interactive surface so the
/// right-click affordance is discoverable.
fn render_indicator_chip(
    inst: &IndicatorInstance,
    output: &IndicatorOutput,
    cursor_idx: Option<usize>,
    cx: &mut Context<ContentPanel>,
) -> gpui::AnyElement {
    let id = inst.id;
    let hidden = inst.hidden;
    let is_pane = inst.placement == Placement::Pane;
    // Chip label = "Name" when crosshair is off-canvas, or
    // "Name: v1[ / v2[ / v3]]" when the crosshair is over a bar.
    // `kind.value_at` returns a typed ValueReadout that's formatted here so
    // the chip always reads cleanly regardless of how many series the
    // indicator emits.
    let base_label = inst.kind.label();
    let label: SharedString = match cursor_idx {
        Some(i) => SharedString::from(format!(
            "{}: {}",
            base_label,
            format_readout(inst.kind.value_at(output, i))
        )),
        None => base_label,
    };
    let chip_id = SharedString::from(format!("chip-{}", id));
    let eye_id = SharedString::from(format!("chip-eye-{}", id));
    let gear_id = SharedString::from(format!("chip-gear-{}", id));
    let close_id = SharedString::from(format!("chip-close-{}", id));
    // Neutral theme colors — the indicator's series color shows in the
    // pane paint itself, so the chip doesn't need to repeat it. Hidden
    // chips dim to muted_foreground; visible chips use foreground.
    let (text_color, border_color) = {
        let theme = cx.theme();
        if hidden {
            (
                theme.muted_foreground,
                Hsla {
                    a: 0.45,
                    ..theme.border
                },
            )
        } else {
            (theme.foreground, theme.border)
        }
    };
    // Eye glyph: filled circle visible, hollow circle hidden. Same width so
    // the chip doesn't reflow when the user toggles visibility.
    let eye_label: SharedString = if hidden {
        SharedString::from("\u{25CB}") // ○
    } else {
        SharedString::from("\u{25CF}") // ●
    };
    let hover_bg = {
        let muted = cx.theme().muted;
        Hsla { a: 0.30, ..muted }
    };
    h_flex()
        .id(chip_id)
        .gap_1()
        .px_2()
        .py(px(2.))
        .items_center()
        .rounded(px(4.))
        .border_1()
        .border_color(border_color)
        .text_xs()
        .text_color(text_color)
        // Occlude the chart canvas's hitbox underneath: right-clicking
        // the chip should open only the chip's own context menu, not
        // also the chart's. `gpui-component`'s `context_menu` primitive
        // works off `window.on_mouse_event` + `hitbox.is_hovered`, so
        // event-level `stop_propagation` doesn't help — `.occlude()` is
        // the supported mechanism for marking hitboxes behind this
        // element as not-hovered. Same trick suppresses ghost-hover
        // styling on the canvas while the cursor is over a chip.
        .occlude()
        // Subtle hover tint so the right-click affordance is discoverable.
        // No cursor_pointer — the body itself is not clickable; only the
        // three buttons (which set their own pointer) are.
        .hover(move |this| this.bg(hover_bg))
        // Right-click menu: Settings… (always), Hide/Show (always),
        // Move pane up/down (Pane-placed only — overlay indicators have no
        // pane order to reshuffle), Remove (always). Actions are scoped to
        // this ContentPanel via the chip's element tree.
        .context_menu(move |menu, _, _| {
            let mut m = menu
                .menu(
                    "Settings…",
                    Box::new(crate::indicator_settings::OpenIndicatorSettings(id)),
                )
                .separator()
                .menu("Hide / Show", Box::new(ToggleIndicatorHidden(id)));
            if is_pane {
                m = m
                    .menu("Move pane up", Box::new(MoveIndicatorPaneUp(id)))
                    .menu("Move pane down", Box::new(MoveIndicatorPaneDown(id)));
            }
            m.separator().menu("Remove", Box::new(RemoveIndicator(id)))
        })
        .child(div().child(label))
        .child(
            Button::new(eye_id)
                .label(eye_label)
                .xsmall()
                .ghost()
                .on_click(cx.listener(move |this, _ev, _w, cx| {
                    if let Some(chart) = this.chart_state.as_mut() {
                        let was_hidden = chart
                            .indicators()
                            .iter()
                            .find(|i| i.id == id)
                            .map(|i| i.hidden)
                            .unwrap_or(false);
                        chart.set_indicator_hidden(id, !was_hidden);
                        cx.notify();
                    }
                })),
        )
        .child(
            Button::new(gear_id)
                .label(SharedString::from("\u{2699}")) // ⚙
                .xsmall()
                .ghost()
                .on_click(move |_ev, window, cx| {
                    window.dispatch_action(
                        Box::new(crate::indicator_settings::OpenIndicatorSettings(id)),
                        cx,
                    );
                }),
        )
        .child(
            Button::new(close_id)
                .label(SharedString::from("\u{00d7}")) // ×
                .xsmall()
                .ghost()
                .on_click(cx.listener(move |this, _ev, _w, cx| {
                    if let Some(chart) = this.chart_state.as_mut() {
                        chart.remove_indicator(id);
                        cx.notify();
                    }
                })),
        )
        .into_any_element()
}

/// Build chip elements for indicators matching `pred`, in render order.
/// Used by both the main-pane vertical list (overlay-only) and the
/// sub-pane chip overlays (each pane's lone pane-placed indicator).
fn render_indicator_chips_filtered(
    state: &ChartState,
    cx: &mut Context<ContentPanel>,
    pred: impl Fn(&IndicatorInstance) -> bool,
) -> Vec<gpui::AnyElement> {
    let cursor_idx = state.cursor_bar_index();
    state
        .indicators()
        .iter()
        .zip(state.indicator_outputs.iter())
        .filter(|(inst, _)| pred(inst))
        .map(|(inst, output)| render_indicator_chip(inst, output, cursor_idx, cx))
        .collect()
}

/// Render the synthetic chip representing the chart's active render kind.
/// Pinned at the top of the indicator list, shares chip UX (label + eye +
/// gear + trash) but isn't an `IndicatorInstance` — there's no `InstanceId`
/// to carry, and the trash glyph is rendered as a non-interactive placeholder
/// (per the locked design: the only way out of a render is the header
/// dropdown). The gear opens the per-mode settings popover; for Candlestick
/// the gear is also disabled (`has_settings() == false`). The settings
/// popover wiring lands in Commit 4 — for now the gear logs the intent.
fn render_synthetic_render_chip(
    state: &ChartState,
    cx: &mut Context<ContentPanel>,
) -> gpui::AnyElement {
    let kind = state.render_kind();
    let visible = state.render_visible();
    let has_settings = kind.has_settings();
    let label = SharedString::from(kind.display_name());

    let (text_color, border_color, disabled_color) = {
        let theme = cx.theme();
        if !visible {
            (
                theme.muted_foreground,
                Hsla {
                    a: 0.45,
                    ..theme.border
                },
                Hsla {
                    a: 0.35,
                    ..theme.muted_foreground
                },
            )
        } else {
            (
                theme.foreground,
                theme.border,
                Hsla {
                    a: 0.35,
                    ..theme.muted_foreground
                },
            )
        }
    };
    let eye_label: SharedString = if visible {
        SharedString::from("\u{25CF}") // ●
    } else {
        SharedString::from("\u{25CB}") // ○
    };
    let hover_bg = {
        let muted = cx.theme().muted;
        Hsla { a: 0.30, ..muted }
    };

    let mut chip = h_flex()
        .id(SharedString::from("chip-render-synthetic"))
        .gap_1()
        .px_2()
        .py(px(2.))
        .items_center()
        .rounded(px(4.))
        .border_1()
        .border_color(border_color)
        .text_xs()
        .text_color(text_color)
        // Match the indicator chip's occlude + hover bg — keeps the
        // chip behaving like its siblings in the same vertical stack.
        .occlude()
        .hover(move |this| this.bg(hover_bg))
        .child(div().child(label));

    // OHLC + volume/delta readout (hovered candle, else latest) lives inside
    // the render chip, right after the mode label.
    if let Some(readout) = ohlc_readout(state, cx) {
        chip = chip.child(readout);
    }

    // Eye toggles `render_visible` — flipping it suppresses the main
    // render layer (paint_main_chart honours `render_visible`) without
    // touching subscriptions. Dispatches an action so chart-scoped
    // key bindings could trigger the same toggle later.
    chip = chip.child(
        Button::new("chip-render-eye")
            .label(eye_label)
            .xsmall()
            .ghost()
            .on_click(|_ev, window, cx| {
                window.dispatch_action(Box::new(ToggleChartRenderVisible), cx);
            }),
    );

    // Gear: enabled only for footprint kinds (Candlestick has no params).
    // Dispatches `OpenChartRenderSettings`; the workspace handler resolves
    // the target chart via `LastFocusedChart` and opens the singleton
    // floating settings window scoped to whichever render kind is active.
    let gear_btn = Button::new("chip-render-gear")
        .label(SharedString::from("\u{2699}")) // ⚙
        .xsmall()
        .ghost();
    chip = if has_settings {
        chip.child(gear_btn.on_click(|_ev, window, cx| {
            window.dispatch_action(Box::new(OpenChartRenderSettings), cx);
        }))
    } else {
        chip.child(gear_btn.disabled(true))
    };

    // Trash: visually present but disabled — per locked design the render
    // is always required; only the header dropdown switches kinds. Rendered
    // dimmer than the active glyphs so it reads as "informational only".
    let trash = div()
        .px_1()
        .text_color(disabled_color)
        .child(SharedString::from("\u{00d7}")); // ×
    chip = chip.child(trash);

    chip.into_any_element()
}

/// OHLC + volume/delta readout embedded inside the render chip (it used to
/// be a hover-only pill in the top-right corner). Reads the candle under the
/// crosshair when hovering, else the latest candle — the standard "legend
/// defaults to the last bar" behaviour. Volume + delta follow the chart's
/// `VolumeUnit` toggle. Returns `None` when the chart has no candles yet, so
/// the render chip falls back to just its mode label.
fn ohlc_readout(
    state: &ChartState,
    cx: &mut Context<ContentPanel>,
) -> Option<gpui::AnyElement> {
    if state.candles.is_empty() {
        return None;
    }
    // Candle to read: the one under the crosshair if the cursor is over a
    // valid bar, otherwise the most recent candle.
    let idx = state
        .cursor
        .zip(state.bounds)
        .and_then(|((cx_px, _), bounds)| {
            let canvas_w = bounds.size.width.as_f32();
            let t = screen_to_index(
                state.view_start,
                state.view_size,
                cx_px,
                canvas_w,
                state.y_axis_gap_px.get(),
            );
            let i = t.round() as i32;
            (i >= 0 && (i as usize) < state.candles.len()).then_some(i as usize)
        })
        .unwrap_or(state.candles.len() - 1);
    let c = &state.candles[idx];

    let (muted, fg, bull, bear) = {
        let theme = cx.theme();
        (
            theme.muted_foreground,
            theme.foreground,
            theme.chart_bullish,
            theme.chart_bearish,
        )
    };

    // Volume + delta in the chart's active unit (delta = signed taker
    // buy/sell imbalance, same definition the Volume Delta indicator uses).
    let scale = |raw: f64| match state.volume_unit {
        crate::persistence::VolumeUnit::Coin => raw,
        crate::persistence::VolumeUnit::Usd => raw * c.close,
    };
    let vol = scale(c.volume);
    let delta = c.taker_buy_vol.map(|tbv| scale(2.0 * tbv - c.volume));
    let delta_color = match delta {
        Some(d) if d >= 0.0 => bull,
        Some(_) => bear,
        None => muted,
    };

    // No border/bg/padding of its own — it's a child group inside the render
    // chip; the chip supplies the pill chrome.
    Some(
        h_flex()
            .gap(px(8.0))
            .child(
                div()
                    .text_color(muted)
                    .child(SharedString::from(format_user_tz(c.open_time, cx))),
            )
            .child(div().text_color(fg).child(format!("O {}", format_price(c.open))))
            .child(div().text_color(bull).child(format!("H {}", format_price(c.high))))
            .child(div().text_color(bear).child(format!("L {}", format_price(c.low))))
            .child(div().text_color(fg).child(format!("C {}", format_price(c.close))))
            .child(div().text_color(muted).child(format!("V {}", fmt_scalar(Some(vol)))))
            .child(div().text_color(delta_color).child(format!("Δ {}", fmt_scalar(delta))))
            .into_any_element(),
    )
}

/// Render the main-pane vertical indicator list — the synthetic render
/// chip (Candlestick / Cluster / Profile, which also carries the OHLC +
/// volume/delta readout) on top, the per-overlay chips below it, and a
/// chevron-only collapse button pinned at the bottom of the stack.
/// Collapsing hides the render chip + overlay chips alike, leaving only the
/// chevron visible — pointed by the design ask to keep the canvas as
/// uncluttered as possible until the user explicitly expands.
///
/// Pane-placed indicators are NOT included here; they each get their own
/// chip rendered at the top-left of their sub-pane (`render_sub_pane_chip`).
/// Positioned by the caller — typically absolute at the main canvas's
/// top-left in the drawings-overlay layer.
fn render_main_indicator_list(
    state: &ChartState,
    cx: &mut Context<ContentPanel>,
) -> gpui::AnyElement {
    let collapsed = state.indicators_collapsed;
    let chevron = if collapsed { "\u{25BC}" } else { "\u{25B2}" }; // ▼ / ▲
    // Count of *overlay* indicators only (the things actually hidden by
    // collapse — the render chip's mode is shown in the gear-window
    // chrome regardless). Shown next to the chevron when collapsed so
    // the user knows at a glance whether expanding will reveal anything.
    let overlay_count = state
        .indicators()
        .iter()
        .filter(|i| i.placement == Placement::Overlay)
        .count();
    let toggle_label = if collapsed {
        SharedString::from(format!("{} {}", overlay_count, chevron))
    } else {
        SharedString::from(chevron)
    };
    let (theme_border, theme_muted_fg, theme_bg, hover_bg) = {
        let theme = cx.theme();
        (
            theme.border,
            theme.muted_foreground,
            theme.background,
            Hsla {
                a: 0.30,
                ..theme.muted
            },
        )
    };
    // Chevron-only toggle. Compact (one glyph, tight padding) so it reads
    // as a control rather than a chip — there's no label text per the
    // design ask.
    let toggle = h_flex()
        .id(SharedString::from("indicator-list-toggle"))
        .px(px(6.))
        .py(px(1.))
        .items_center()
        .justify_center()
        .rounded(px(4.))
        .border_1()
        .border_color(theme_border)
        .bg(theme_bg)
        .text_xs()
        .text_color(theme_muted_fg)
        .cursor_pointer()
        .hover(move |this| this.bg(hover_bg))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|this, _ev, _w, cx| {
                if let Some(chart) = this.chart_state.as_mut() {
                    chart.indicators_collapsed = !chart.indicators_collapsed;
                    cx.notify();
                }
            }),
        )
        .child(div().child(toggle_label));
    // Absolute-anchored at the main canvas's top-left (mirrors the
    // pre-move OHLC pill's `top(8) left(8)`). `items_start` so each chip
    // auto-sizes to its content rather than stretching to fill the longest.
    // When collapsed, only the chevron toggle is rendered — the render
    // chip and overlay chips are both hidden so the canvas stays clear.
    let mut stack = v_flex()
        .absolute()
        .top(px(8.0))
        .left(px(8.0))
        .gap_1()
        .items_start();
    if !collapsed {
        let render_chip = render_synthetic_render_chip(state, cx);
        let chips = render_indicator_chips_filtered(state, cx, |i| {
            i.placement == Placement::Overlay
        });
        stack = stack.child(render_chip).children(chips);
    }
    stack.child(toggle).into_any_element()
}

pub fn render(
    state: &ChartState,
    focus: FocusHandle,
    _window: &mut Window,
    cx: &mut Context<ContentPanel>,
) -> impl IntoElement {
    // Extract every theme colour we need as Hsla (Copy) up front so the
    // `&Theme` borrow ends before we start chaining `cx.listener` calls below.
    // Otherwise the closure constructions further down trip E0500 against any
    // later reference to `theme.*`.
    let (
        theme_background,
        theme_border,
        theme_muted_foreground,
        theme_chart_bullish,
        theme_chart_bearish,
        theme_chart_5,
        theme_foreground,
        theme_ring,
    ) = {
        let theme = cx.theme();
        (
            theme.background,
            theme.border,
            theme.muted_foreground,
            theme.chart_bullish,
            theme.chart_bearish,
            theme.chart_5,
            theme.foreground,
            theme.ring,
        )
    };

    // Snapshot of the active drawing tool — drives the FRVP-on-sub-pane
    // not-allowed cursor and the early-return guard in sub-pane
    // mouse-down. Re-read on every render so a tool change picks up
    // next paint (ContentPanel subscribes to DrawingToolEvent so the
    // change triggers a notify even when the user hasn't moved the
    // mouse).
    let active_tool = crate::drawings::tool::current_tool(cx);

    // LIVE / Reconnecting / Disconnected badge in the header. Mirrors the
    // market-data service's connection state.
    let (badge_color, badge_label): (Hsla, &'static str) = {
        let svc = cx
            .global::<market_data::MarketDataServiceHandle>()
            .0
            .clone();
        let status = svc
            .read(cx)
            .status(state.symbol.as_ref(), state.timeframe());
        use market_data::LiveStatus::*;
        match status {
            Connected => (theme_chart_bullish, "LIVE"),
            Connecting => (theme_chart_5, "Connecting…"),
            Reconnecting { attempts } if attempts >= 4 => (theme_chart_bearish, "Disconnected"),
            Reconnecting { .. } => (theme_chart_5, "Reconnecting…"),
        }
    };

    // Single `y_range` + `visible_slice` snapshot threaded through the rest
    // of render. Previously the render path called `state.y_range()` three
    // times and `state.visible()` (a cloning variant) once; each y_range call
    // re-scanned the visible window when y_auto was on. With the bottom-bar's
    // animation-frame loop forcing continuous repaint, that redundancy
    // mattered.
    let (y_lo, y_hi) = state.y_range();
    // Resize the y-axis gutter to fit the widest price label this frame.
    // Stored on `state` so mouse handlers' hit-tests use the same gap as
    // paint — otherwise clicks drift after a y-range change.
    state.y_axis_gap_px.set(compute_y_axis_gap(y_lo, y_hi));

    // This symbol's display meta (name/exchange) for the header line comes
    // from the symbols service. Falls back to the bare ticker if the entry
    // hasn't been registered yet.
    let symbols_handle = cx.global::<SymbolsServiceHandle>().0.clone();
    let symbols_svc = symbols_handle.read(cx);
    let (header_name, header_exchange) = symbols_svc
        .meta(state.symbol.as_ref())
        .unwrap_or_else(|| (state.symbol.clone(), SharedString::from("")));

    // Header symbol button — opens the shared TradingView-style picker
    // (modal overlay) targeting *this* chart. Sets `LastFocusedChart` before
    // dispatching so the workspace's `OpenSymbolPicker` handler resolves to
    // this panel regardless of where the user last clicked.
    let symbol_button = Button::new("chart-symbol-open-picker")
        .label(state.symbol.clone())
        .icon(IconName::ChevronDown)
        .small()
        .ghost()
        .tooltip("Change symbol (Cmd-K)")
        .on_click(cx.listener(|this, _ev, window, cx| {
            let weak = cx.weak_entity();
            *cx.global::<LastFocusedChart>().0.borrow_mut() = Some(weak);
            window.dispatch_action(
                Box::new(OpenSymbolPicker {
                    kind: SharedString::from("chart"),
                }),
                cx,
            );
            let _ = this;
        }));

    // Timeframe-selector dropdown — same focus-scoping as the symbol selector
    // so `ChangeChartTimeframe` dispatches up through this panel.
    let tf_focus = focus.clone();
    let timeframe_btn = Button::new("chart-timeframe-select")
        .label(SharedString::from(state.timeframe().as_str()))
        .small()
        .ghost()
        .dropdown_menu(move |menu, _, _| {
            let mut menu = menu.action_context(tf_focus.clone());
            for tf in Timeframe::ALL {
                menu = menu.menu(
                    SharedString::from(tf.as_str()),
                    Box::new(ChangeChartTimeframe(SharedString::from(tf.as_str()))),
                );
            }
            menu
        });

    // Render-kind dropdown (Candlestick / Footprint Cluster / Footprint
    // Profile). Sits next to the TF selector; switching swaps the chart's
    // main render layer and triggers the footprint sub lifecycle in
    // `ContentPanel::on_change_chart_render`.
    let render_focus = focus.clone();
    let render_btn = Button::new("chart-render-select")
        .label(SharedString::from(state.render_kind().display_name()))
        .small()
        .ghost()
        .dropdown_menu(move |menu, _, _| {
            let mut menu = menu.action_context(render_focus.clone());
            for kind in [
                RenderKind::Candlestick,
                RenderKind::Cluster,
                RenderKind::Profile,
            ] {
                menu = menu.menu(
                    SharedString::from(kind.display_name()),
                    Box::new(ChangeChartRender(SharedString::from(kind.as_id()))),
                );
            }
            menu
        });

    // Per-chart volume-unit dropdown (Coin / USD). Sits between the
    // render-kind selector and `+ Indicator`. Drives this chart's
    // Volume / Volume Delta / CVD indicators (via `ComputeCtx`) and its
    // footprint paint pipeline. Per-chart rather than global so two charts
    // open side-by-side can show the same data in different units.
    let volume_unit_focus = focus.clone();
    let current_volume_unit = state.volume_unit();
    let volume_unit_label = match current_volume_unit {
        VolumeUnit::Coin => "Coin",
        VolumeUnit::Usd => "USD",
    };
    let volume_unit_btn = Button::new("chart-volume-unit-select")
        .label(SharedString::from(volume_unit_label))
        .small()
        .ghost()
        .dropdown_menu(move |menu, _, _| {
            let mut menu = menu.action_context(volume_unit_focus.clone());
            for (label, id) in [("Coin", "coin"), ("USD", "usd")] {
                menu = menu.menu(
                    SharedString::from(label),
                    Box::new(ChangeChartVolumeUnit(SharedString::from(id))),
                );
            }
            menu
        });

    // Orderbook-heatmap toggle. Independent boolean overlay, orthogonal to the
    // render-kind selector — paints resting book liquidity behind whatever the
    // main render is. First enable opens the book subscription + 1s sampler
    // lazily (see `ContentPanel::toggle_chart_heatmap`). Right-click opens its
    // settings (bucket / colour reference / opacity).
    let heatmap_on = state.heatmap_enabled();
    let heatmap_btn = {
        let btn = Button::new("chart-heatmap-toggle")
            .label(SharedString::from("Heatmap"))
            .small()
            .on_click(cx.listener(|this, _ev, window, cx| {
                this.toggle_chart_heatmap(window, cx);
            }));
        if heatmap_on { btn.primary() } else { btn.ghost() }
    };
    // Gear opens the heatmap settings floating window — only meaningful (and
    // only shown) while the overlay is on.
    let heatmap_gear = heatmap_on.then(|| {
        Button::new("chart-heatmap-settings")
            .label(SharedString::from("\u{2699}")) // ⚙
            .small()
            .ghost()
            .on_click(move |_ev, window, cx| {
                window.dispatch_action(
                    Box::new(super::heatmap_settings::OpenHeatmapSettings),
                    cx,
                );
            })
    });

    // Snapshot the candle slice the paint pass needs. Cloning is fine — at
    // default `view_size = 60` we copy ~60 `Candle`s once per render, the
    // same cost the deleted `visible_for_chart_with_y` had. The closure
    // captures these by move so the borrow doesn't escape `render`.
    let (paint_start_idx, paint_candles_slice) = state.paint_slice();
    let paint_candles: Vec<Candle> = paint_candles_slice.to_vec();
    // Captured separately so the sub-pane builders (which run after the main
    // canvas closure consumes `paint_candles`) can still read the visible-bar
    // count.
    let paint_candles_len = paint_candles.len();
    let paint_view_start = state.view_start;
    let paint_view_size = state.view_size;
    let paint_candle_interval_ms = state.candle_interval_ms();
    let paint_y_axis_gap = state.y_axis_gap_px.get();
    // Render-mode dispatch params. Footprint cells are kept in
    // `ChartState::footprint_cells`, populated by `ContentPanel`'s
    // FootprintEvent handler; when the active render is Candlestick or
    // the sub is closed, this is empty and the paint pipeline falls back
    // to candle bodies automatically.
    let paint_render_kind = state.render_kind();
    let paint_render_visible = state.render_visible();
    let paint_footprint_params: Option<FootprintParams> = state.active_footprint_params().copied();
    let paint_footprint_cells: Vec<market_data::FootprintCell> = state.footprint_cells().to_vec();
    let paint_volume_unit = state.volume_unit();
    // Orderbook heatmap texture (behind candles). `None` when the overlay is
    // off or unbuilt. The build itself ran in `ContentPanel::render` (it needs
    // a `&mut Window` for atlas eviction); here we only capture the cheap
    // Arc-backed rect into the paint closure.
    let paint_heatmap_rect: Option<HeatmapRect> = state.heatmap_paint_rect();
    // Pre-filter overlay indicators for the paint closure: skip hidden /
    // pane-placed instances, snapshot color + output so the closure stays
    // 'static. Per-render clone — `Series` is a `Vec<Option<f64>>`, so the
    // cost is comparable to `paint_candles.clone()` above.
    let paint_overlay_items: Vec<OverlayPaintItem> = state
        .indicators
        .iter()
        .zip(state.indicator_outputs.iter())
        .filter(|(i, _)| !i.hidden && i.placement == Placement::Overlay)
        .map(|(i, o)| OverlayPaintItem {
            colors: i.colors.clone(),
            output: o.clone(),
        })
        .collect();

    // Pane-placed indicators: one sub-canvas each, computed in render order.
    // We capture `(instance_id, height, PanePaintItem)` triples; the sub-pane
    // emit loop below uses these to build the splitter+canvas elements. y_lo /
    // y_hi come from `IndicatorKind::y_range` over the visible bar slice — the
    // paint closure stays trait-object-free so it can be `'static`. Hidden
    // pane indicators stay in the list (keeping their slot at full height)
    // but get `hidden: true` so `paint_sub_pane` early-returns without
    // painting anything — the chip overlay at top-left remains reachable.
    let paint_pane_items: Vec<(InstanceId, f32, PanePaintItem)> = {
        let visible_end = paint_start_idx
            .saturating_add(paint_candles.len())
            .min(state.candles.len());
        let visible_range = paint_start_idx..visible_end;
        state
            .indicators
            .iter()
            .zip(state.indicator_outputs.iter())
            .filter(|(i, _)| i.placement == Placement::Pane)
            .filter_map(|(i, o)| {
                let height = i
                    .pane_height
                    .unwrap_or_else(|| crate::indicators::default_pane_height(i.kind_id));
                if i.hidden {
                    // Placeholder y range — never read since `paint_sub_pane`
                    // early-returns on `hidden`. Keep the slot at full height
                    // so toggling visibility doesn't reflow the layout.
                    let item = PanePaintItem {
                        colors: i.colors.clone(),
                        output: o.clone(),
                        kind_id: i.kind_id,
                        y_lo: 0.0,
                        y_hi: 1.0,
                        hidden: true,
                    };
                    return Some((i.id, height, item));
                }
                // `y_range` returns `None` when no `Some(_)` data falls in the
                // visible window (early bars before the indicator has enough
                // history). For visible panes we skip the canvas paint that
                // frame, but the chip overlay is rendered against the same
                // pane element (built below), so we still keep the slot —
                // the canvas just no-ops via `hidden: true`.
                let (mut y_lo, mut y_hi) = i
                    .kind
                    .y_range(o, visible_range.clone())
                    .unwrap_or((0.0, 1.0));
                if (y_hi - y_lo).abs() < 1e-9 {
                    // Degenerate range: pad ±5% so the line/zero level sits in
                    // the middle of the pane instead of stuck at the edge.
                    let pad = y_hi.abs().max(1.0) * 0.05;
                    y_lo -= pad;
                    y_hi += pad;
                }
                let item = PanePaintItem {
                    colors: i.colors.clone(),
                    output: o.clone(),
                    kind_id: i.kind_id,
                    y_lo,
                    y_hi,
                    hidden: false,
                };
                Some((i.id, height, item))
            })
            .collect()
    };

    let main_chart_colors = MainChartColors {
        bullish: theme_chart_bullish,
        bearish: theme_chart_bearish,
        grid: Hsla {
            a: 0.30,
            ..theme_border
        },
        label: theme_muted_foreground,
        cell_text: theme_foreground,
        axis_bg: theme_background,
        axis_border: theme_border,
    };
    let entity = cx.entity();

    // Right (price) axis interaction zone — overlays the chart's reserved
    // y-label gutter. Vertical drag scales the locked y range; wheel zooms
    // it; double-click re-enables auto-fit.
    let y_axis_gap = state.y_axis_gap_px.get();
    let right_axis = div()
        .id("chart-right-axis")
        .absolute()
        .right_0()
        .top_0()
        .bottom(gpui::px(AXIS_GAP))
        .w(gpui::px(y_axis_gap))
        .cursor_ns_resize()
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|this, ev: &MouseDownEvent, _w, cx| {
                cx.stop_propagation();
                let Some(state) = this.chart_state.as_mut() else {
                    return;
                };
                if ev.click_count >= 2 {
                    state.reset_y_auto();
                    cx.notify();
                    return;
                }
                state.freeze_y_if_auto();
                state.y_drag_anchor = Some((ev.position, state.y_min, state.y_max));
                cx.notify();
            }),
        )
        .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _w, cx| {
            let Some(state) = this.chart_state.as_mut() else {
                return;
            };
            if !ev.dragging() {
                if state.y_drag_anchor.take().is_some() {
                    cx.notify();
                }
                return;
            }
            let Some((start_pos, start_lo, start_hi)) = state.y_drag_anchor else {
                return;
            };
            let Some(bounds) = state.bounds else {
                return;
            };
            let h = bounds.size.height.as_f32();
            if h <= 0.0 {
                return;
            }
            let dy = ev.position.y.as_f32() - start_pos.y.as_f32();
            // Drag down → range expands (zoom out); drag up → contracts.
            let factor = (dy / h).exp() as f64;
            let center = (start_lo + start_hi) / 2.0;
            state.y_min = center - (center - start_lo) * factor;
            state.y_max = center + (start_hi - center) * factor;
            cx.notify();
        }))
        .on_mouse_up(
            MouseButton::Left,
            cx.listener(|this, _ev, _w, cx| {
                let Some(state) = this.chart_state.as_mut() else {
                    return;
                };
                if state.y_drag_anchor.take().is_some() {
                    cx.notify();
                }
            }),
        )
        .on_scroll_wheel(cx.listener(|this, ev: &ScrollWheelEvent, w, cx| {
            cx.stop_propagation();
            let Some(state) = this.chart_state.as_mut() else {
                return;
            };
            let delta_y = ev.delta.pixel_delta(w.line_height()).y.as_f32();
            if delta_y == 0.0 {
                return;
            }
            state.freeze_y_if_auto();
            let factor = (-delta_y / SCROLL_ZOOM_RATE).exp() as f64;
            let center = (state.y_min + state.y_max) / 2.0;
            state.y_min = center - (center - state.y_min) * factor;
            state.y_max = center + (state.y_max - center) * factor;
            cx.notify();
        }));

    // Bottom (time) axis interaction zone. Horizontal drag scales view_size
    // around its centre; wheel zooms; double-click resets to the trailing
    // default window.
    let bottom_axis = div()
        .id("chart-bottom-axis")
        .absolute()
        .left_0()
        .bottom_0()
        .right(gpui::px(y_axis_gap))
        .h(gpui::px(AXIS_GAP))
        .cursor_ew_resize()
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|this, ev: &MouseDownEvent, _w, cx| {
                cx.stop_propagation();
                let Some(state) = this.chart_state.as_mut() else {
                    return;
                };
                if ev.click_count >= 2 {
                    state.reset_x();
                    cx.notify();
                    return;
                }
                state.x_axis_drag_anchor = Some((ev.position, state.view_size, state.view_start));
                cx.notify();
            }),
        )
        .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _w, cx| {
            let Some(state) = this.chart_state.as_mut() else {
                return;
            };
            if !ev.dragging() {
                if state.x_axis_drag_anchor.take().is_some() {
                    cx.notify();
                }
                return;
            }
            let Some((start_pos, start_size, start_view_start)) = state.x_axis_drag_anchor else {
                return;
            };
            let Some(bounds) = state.bounds else {
                return;
            };
            let w = bounds.size.width.as_f32();
            if w <= 0.0 {
                return;
            }
            let dx = ev.position.x.as_f32() - start_pos.x.as_f32();
            // Drag right → view widens (more candles), drag left → narrows.
            let factor = (dx / w).exp();
            // Centre is taken from the drag-start viewport so it doesn't
            // drift when `clamp` adjusts `view_start` between frames.
            // `view_size` is clamped BEFORE we derive `view_start`, so once
            // the candle width hits its min/max the drag stops shifting the
            // chart horizontally (mirrors the wheel-handler fix).
            let total = state.candles.len() as f32;
            let center_at_down = start_view_start + start_size / 2.0;
            let new_view_size =
                (start_size * factor).clamp(CHART_MIN_VIEW.min(total), CHART_MAX_VIEW.min(total));
            state.view_size = new_view_size;
            state.view_start = center_at_down - new_view_size / 2.0;
            state.clamp();
            this.maybe_load_older(cx);
            cx.notify();
        }))
        .on_mouse_up(
            MouseButton::Left,
            cx.listener(|this, _ev, _w, cx| {
                let Some(state) = this.chart_state.as_mut() else {
                    return;
                };
                if state.x_axis_drag_anchor.take().is_some() {
                    cx.notify();
                }
            }),
        )
        .on_scroll_wheel(cx.listener(|this, ev: &ScrollWheelEvent, w, cx| {
            cx.stop_propagation();
            let Some(state) = this.chart_state.as_mut() else {
                return;
            };
            let delta_y = ev.delta.pixel_delta(w.line_height()).y.as_f32();
            if delta_y == 0.0 {
                return;
            }
            let factor = (-delta_y / SCROLL_ZOOM_RATE).exp();
            // Clamp `view_size` before computing the new `view_start` so
            // hitting the min/max stops both zoom and horizontal drift — see
            // the canvas wheel handler for the longer reasoning.
            let center = state.view_start + state.view_size / 2.0;
            let total = state.candles.len() as f32;
            let new_view_size = (state.view_size * factor)
                .clamp(CHART_MIN_VIEW.min(total), CHART_MAX_VIEW.min(total));
            state.view_size = new_view_size;
            state.view_start = center - state.view_size / 2.0;
            state.clamp();
            this.maybe_load_older(cx);
            cx.notify();
        }));

    // Snapshot visible drawings + selection from the workspace
    // `DrawingService`. Each tick of render builds a view-coord `Vec<Drawing>`
    // anchored to *this* chart's candle buffer, so the paint pipeline doesn't
    // need to know about wall-clock ms. Hidden drawings and drawings whose
    // `tf_filter` excludes the current timeframe are filtered out here so
    // downstream code can iterate freely.
    let symbol_str = state.symbol.as_ref();
    let tf_str = state.timeframe.as_str();
    let (mut drawings_snapshot, styles_snapshot, selected_for_overlay) = {
        let service = cx.global::<DrawingServiceHandle>().0.clone();
        let svc = service.read(cx);
        let visible: Vec<&crate::drawings::shapes::Drawing> = svc
            .for_symbol(symbol_str)
            .iter()
            .filter(|d| d.visible_on(tf_str))
            .collect();
        let snapshot: Vec<Drawing> = visible
            .iter()
            .map(|d| drawings_view::shape_to_view(d, &state.candles, paint_candle_interval_ms))
            .collect();
        // Style map parallel to `snapshot` — keyed by drawing id so paint
        // can look up per-shape colour/width overrides without threading
        // the data into every match arm of `ViewDrawing`.
        let styles: std::collections::HashMap<u64, drawings_view::DrawingStyle> = visible
            .iter()
            .map(|d| (d.id, drawings_view::style_from_shape(&d.shape)))
            .collect();
        let sel = svc
            .selected_drawing()
            .filter(|(sym, _)| sym.as_ref() == symbol_str)
            .map(|(_, d)| d.id);
        (snapshot, styles, sel)
    };
    // Enrich FRVP entries with their precomputed `VolumeProfileOutput`.
    // Done here (not inside `shape_to_view`) so the conversion stays
    // cache-free; the chart owns the per-bucket footprint cell cache and
    // is the only natural place to dereference it. The output is fed
    // straight into the drawings overlay paint closure — no service /
    // borrow back-and-forth from inside the `'static` closure.
    if !state.candles.is_empty() {
        let tf_ms_for_frvp = paint_candle_interval_ms;
        for d in drawings_snapshot.iter_mut() {
            let Drawing::Frvp { t0, t1, params, output, .. } = d else {
                continue;
            };
            // FRVP range: convert the view-coord time anchors back to ms
            // for `compute_volume_profile` (whose API is wall-clock).
            // Already normalized in `view_to_shape` but a creating-preview
            // can still arrive with `t0 > t1`; min/max guards both.
            let lo = drawings_view::idx_to_time(t0.min(*t1), &state.candles, tf_ms_for_frvp);
            let hi = drawings_view::idx_to_time(t0.max(*t1), &state.candles, tf_ms_for_frvp);
            let bits = params.bucket_bits();
            let cells_opt = state.footprint_cells_for_bucket(bits);
            let cells: &[crate::services::market_data::FootprintCell] = cells_opt.unwrap_or(&[]);
            *output = Some(crate::volume_profile::compute_volume_profile(
                cells,
                (lo, hi),
                tf_ms_for_frvp,
                params,
            ));
        }
    }
    let creating_preview = state.creating.as_ref().map(|c| c.preview());
    // Reuse the y_range computed at the top of render — overlay anchors must
    // match the candles they sit next to.
    let (y_lo_for_overlay, y_hi_for_overlay) = (y_lo, y_hi);
    let drawing_colors = DrawingColors {
        line: theme_foreground,
        // Rect uses foreground (white in dark theme) instead of accent —
        // accent is too muted to read against the candles. Low-alpha fill
        // gives a hint of the body without obscuring the bars.
        rect_fill: Hsla {
            a: 0.08,
            ..theme_foreground
        },
        rect_border: theme_foreground,
        ring: theme_ring,
        background: theme_background,
        bullish: theme_chart_bullish,
        bearish: theme_chart_bearish,
        muted: theme_muted_foreground,
    };
    let cursor_for_overlay = state.cursor;
    // Cross-pane shared x for the main pane's vertical guide. When the
    // cursor is on the main pane this duplicates `cursor.0`; when the
    // cursor is on a sub-pane this still drives the main pane's vertical
    // line so the user can see which bar their sub-pane readout matches.
    let cross_x_for_overlay = state.cross_cursor_x;
    let candles_for_overlay = state.candles.clone();
    let drawings_overlay = render_drawings_overlay(
        drawings_snapshot.clone(),
        styles_snapshot.clone(),
        creating_preview,
        selected_for_overlay,
        state.view_start,
        state.view_size,
        y_lo_for_overlay,
        y_hi_for_overlay,
        state.y_axis_gap_px.get(),
        cursor_for_overlay,
        cross_x_for_overlay,
        drawing_colors,
        candles_for_overlay,
    )
    .into_any_element();

    // Crosshair labels + OHLC readout. All depend on (cursor, bounds) being
    // Some — i.e. mouse is hovering and we've prepainted at least once.
    let crosshair_chrome: Vec<gpui::AnyElement> =
        if let (Some((cx_px, cy_px)), Some(bounds)) = (state.cursor, state.bounds) {
            let canvas_w = bounds.size.width.as_f32();
            let canvas_h = bounds.size.height.as_f32();
            let y_axis_gap = state.y_axis_gap_px.get();
            let world_t = screen_to_index(
                state.view_start,
                state.view_size,
                cx_px,
                canvas_w,
                state.y_axis_gap_px.get(),
            );
            let candle_idx = world_t.round() as i32;
            let candle: Option<&Candle> =
                if candle_idx >= 0 && (candle_idx as usize) < state.candles.len() {
                    Some(&state.candles[candle_idx as usize])
                } else {
                    None
                };
            let world_p = screen_to_price(y_lo_for_overlay, y_hi_for_overlay, cy_px, canvas_h);

            let mut chrome: Vec<gpui::AnyElement> = Vec::new();

            // (The OHLC readout moved to the top-left chip stack — see
            // `render_ohlc_chip`. The crosshair now only paints the axis
            // labels below.)

            // Time label hugging the bottom axis at the cursor's x.
            let chart_w = canvas_w - y_axis_gap;
            if cx_px >= 0.0 && cx_px <= chart_w {
                let time_text = candle
                    .map(|c| format_user_tz(c.open_time, cx))
                    .unwrap_or_else(|| "—".to_string());
                // Width estimated for centring; we shift left so the label is
                // centred under the vertical line.
                let est_w = (time_text.len() as f32 * 7.0).max(48.0) + 12.0;
                let mut left = cx_px - est_w / 2.0;
                left = left.clamp(0.0, chart_w - est_w);
                chrome.push(
                    div()
                        .absolute()
                        .left(px(left))
                        .bottom(px(0.0))
                        .pl(px(6.0))
                        .pr(px(6.0))
                        .text_size(px(11.))
                        .text_color(theme_foreground)
                        .bg(theme_background)
                        .border_1()
                        .border_color(theme_border)
                        .rounded(px(2.0))
                        .child(SharedString::from(time_text))
                        .into_any_element(),
                );
            }

            // Price label hugging the right axis at the cursor's y. Uses a
            // fixed pixel size (not `text_xs`) so the readout stays compact
            // even when the user dials up the global font size.
            let chart_h = canvas_h - AXIS_GAP;
            if cy_px >= 0.0 && cy_px <= chart_h {
                chrome.push(
                    div()
                        .absolute()
                        .right(px(0.0))
                        .top(px((cy_px - 8.0).max(0.0)))
                        .w(px(y_axis_gap - 2.0))
                        .pl(px(4.0))
                        .pr(px(4.0))
                        .text_size(px(11.))
                        .text_color(theme_foreground)
                        .bg(theme_background)
                        .border_1()
                        .border_color(theme_border)
                        .rounded(px(2.0))
                        .child(format_price(world_p))
                        .into_any_element(),
                );
            }
            chrome
        } else {
            Vec::new()
        };

    // Live developing-bar guide: a horizontal price ray from the current
    // (still-open) bar to the right edge of the chart, a colour-coded price
    // pill on the right axis, and a "M:SS" countdown to the next bar open.
    // Only live symbols have a developing bar; for historical charts this
    // collapses to an empty vec so we don't paint a stale last-close marker.
    let live_price_chrome: Vec<gpui::AnyElement> =
        if let (Some(bounds), Some(last)) = (state.bounds, state.candles.last()) {
            let canvas_w = bounds.size.width.as_f32();
            let canvas_h = bounds.size.height.as_f32();
            let y_axis_gap = state.y_axis_gap_px.get();
            let chart_w = (canvas_w - y_axis_gap).max(0.0);
            let chart_h = (canvas_h - AXIS_GAP).max(0.0);

            let last_idx = (state.candles.len() - 1) as f32;
            let last_x = index_to_screen(
                state.view_start,
                state.view_size,
                last_idx,
                canvas_w,
                state.y_axis_gap_px.get(),
            );
            let price_y = price_to_screen(y_lo, y_hi, last.close, canvas_h);

            // Direction relative to bar open — colour matches the candle body so
            // the guide reads as "this is the current bar's close".
            let bar_color = if last.close >= last.open {
                theme_chart_bullish
            } else {
                theme_chart_bearish
            };

            let mut out: Vec<gpui::AnyElement> = Vec::new();

            // Horizontal price ray. Clamp left to the chart area; if the bar has
            // scrolled off-screen the ray still hugs the chart's right half so
            // the user can find the live price without re-anchoring.
            let line_left = last_x.clamp(0.0, chart_w);
            let line_width = (chart_w - line_left).max(0.0);
            if line_width > 0.0 && price_y >= 0.0 && price_y <= chart_h {
                out.push(
                    div()
                        .absolute()
                        .left(px(line_left))
                        .top(px(price_y - 0.5))
                        .w(px(line_width))
                        .h(px(1.0))
                        // Faded so it doesn't fight the candles/drawings under it.
                        .bg(Hsla {
                            a: 0.55,
                            ..bar_color
                        })
                        .into_any_element(),
                );
            }

            // Right-axis price pill — solid background in the bar's direction
            // colour so it's the loudest thing on the axis (this is the "live"
            // signal users want at a glance).
            let pill_top = (price_y - 8.0).clamp(0.0, (chart_h - 16.0).max(0.0));
            out.push(
                div()
                    .absolute()
                    .right(px(0.0))
                    .top(px(pill_top))
                    .w(px((y_axis_gap - 2.0).max(0.0)))
                    .pl(px(4.0))
                    .pr(px(4.0))
                    .text_size(px(11.))
                    .font_semibold()
                    .text_color(theme_background)
                    .bg(bar_color)
                    .rounded(px(2.0))
                    .child(format_price(last.close))
                    .into_any_element(),
            );

            // M:SS countdown to bar close. Clamped to ≥0 — if `close_time` has
            // already elapsed (WS-stream hasn't told us yet that the bar
            // rolled), we just show 0:00 instead of a negative number.
            let now_ms = chrono::Utc::now().timestamp_millis();
            let remaining_ms = (last.close_time - now_ms).max(0);
            let total_sec = remaining_ms / 1000;
            let mm = total_sec / 60;
            let ss = total_sec % 60;
            let cd_top = (price_y + 8.0).clamp(0.0, (chart_h - 14.0).max(0.0));
            out.push(
                div()
                    .absolute()
                    .right(px(0.0))
                    .top(px(cd_top))
                    .w(px((y_axis_gap - 2.0).max(0.0)))
                    .pl(px(4.0))
                    .pr(px(4.0))
                    .text_size(px(11.))
                    .text_color(theme_muted_foreground)
                    .bg(theme_background)
                    .border_1()
                    .border_color(theme_border)
                    .rounded(px(2.0))
                    .child(SharedString::from(format!("{}:{:02}", mm, ss)))
                    .into_any_element(),
            );

            out
        } else {
            Vec::new()
        };

    // Y-axis price pill for every visible horizontal ray. Mirrors the
    // live-price pill so each ray's exact price is readable on the axis,
    // independent of grid spacing. Painted as positioned divs (over the
    // axis chrome) so it sits above the y-axis labels.
    let ray_price_chrome: Vec<gpui::AnyElement> = if let Some(bounds) = state.bounds {
        let canvas_h = bounds.size.height.as_f32();
        let y_axis_gap = state.y_axis_gap_px.get();
        let chart_h = (canvas_h - AXIS_GAP).max(0.0);
        let mut out: Vec<gpui::AnyElement> = Vec::new();
        for d in &drawings_snapshot {
            let Drawing::HorizontalRay { anchor, .. } = d else {
                continue;
            };
            let price_y = price_to_screen(y_lo_for_overlay, y_hi_for_overlay, anchor.1, canvas_h);
            if price_y < 0.0 || price_y > chart_h {
                continue;
            }
            let pill_top = (price_y - 8.0).clamp(0.0, (chart_h - 16.0).max(0.0));
            out.push(
                div()
                    .absolute()
                    .right(px(0.0))
                    .top(px(pill_top))
                    .w(px((y_axis_gap - 2.0).max(0.0)))
                    .h(px(16.0))
                    .pl(px(4.0))
                    .pr(px(4.0))
                    .text_size(px(11.))
                    .line_height(px(16.0))
                    .text_color(theme_background)
                    .bg(theme_foreground)
                    .rounded(px(2.0))
                    .child(format_price(anchor.1))
                    .into_any_element(),
            );
        }
        out
    } else {
        Vec::new()
    };

    // Floating "Go to latest" button — appears bottom-right inside the
    // canvas when the most recent candle has scrolled off the right edge of
    // the viewport (e.g. user panned left into history, or new live ticks
    // formed past the visible window). Clicking shifts `view_start` so the
    // latest bar lands at the default trailing offset, preserving the
    // user's zoom. Anchored to the canvas, inset past the y-axis gutter
    // and the x-axis label row.
    let go_to_latest_chrome: Vec<gpui::AnyElement> =
        if state.bounds.is_some() && state.latest_off_right() {
            let y_axis_gap = state.y_axis_gap_px.get();
            // No tooltip: the button removes itself on click (latest comes back
            // into view), and the gpui-component tooltip overlay only hides on
            // hover-leave — which never fires for a vanished element, leaving a
            // sticky popup behind. The arrow icon is conventional enough for
            // "go to latest" without a label.
            let btn = Button::new("chart-go-to-latest")
                .icon(IconName::ArrowRight)
                .ghost()
                .xsmall()
                .rounded(gpui_component::button::ButtonRounded::Size(px(999.0)))
                .on_click(cx.listener(|this, _ev, _w, cx| {
                    let Some(state) = this.chart_state.as_mut() else {
                        return;
                    };
                    state.snap_to_latest();
                    cx.notify();
                }));
            vec![
                div()
                    .absolute()
                    .right(px(y_axis_gap + 12.0))
                    .bottom(px(AXIS_GAP + 12.0))
                    // Eat the mouse-down so the canvas's pan handler doesn't
                    // arm a drag underneath the button. The Button's own
                    // on_click still fires on release.
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(btn)
                    .into_any_element(),
            ]
        } else {
            Vec::new()
        };

    // Text labels live as positioned divs (not painted in the overlay) so
    // editing reuses gpui-component's `Input` widget. Each label is purely
    // visual — selection and drag happen through canvas-level hit testing
    // against an estimated bounding box. Selected text gets a small handle
    // dot at the right edge as a resize affordance.
    let text_labels: Vec<gpui::AnyElement> = if let Some(bounds) = state.bounds {
        let canvas_w = bounds.size.width.as_f32();
        let canvas_h = bounds.size.height.as_f32();
        let editing_id = state.editing_text.as_ref().and_then(|e| e.existing_id);
        let mut out: Vec<gpui::AnyElement> = Vec::new();
        for d in &drawings_snapshot {
            let Drawing::Text {
                id,
                anchor,
                width,
                text,
                font_size,
            } = d
            else {
                continue;
            };
            if editing_id == Some(*id) {
                continue;
            }
            let sx = index_to_screen(
                state.view_start,
                state.view_size,
                anchor.0,
                canvas_w,
                state.y_axis_gap_px.get(),
            );
            let sy = price_to_screen(y_lo_for_overlay, y_hi_for_overlay, anchor.1, canvas_h);
            let selected = selected_for_overlay == Some(*id);
            let border = if selected { theme_ring } else { theme_border };
            // Per-shape colour override pulled from the styles snapshot built
            // upstream alongside the drawings list. Falls back to the theme
            // foreground when the user hasn't customised the colour.
            let text_color = styles_snapshot
                .get(id)
                .and_then(|s| s.color)
                .unwrap_or(theme_foreground);
            out.push(
                div()
                    .absolute()
                    .left(px(sx))
                    .top(px(sy))
                    // Fixed pixel width — text inside wraps naturally; the
                    // div's height grows with content. No background fill so
                    // candles/grid show through; the thin border keeps the
                    // box outline visible.
                    .w(px(*width))
                    .px_1p5()
                    .py_0p5()
                    .text_size(px(*font_size))
                    .text_color(text_color)
                    .border_1()
                    .border_color(border)
                    .rounded(px(3.))
                    .child(SharedString::from(text.clone()))
                    .into_any_element(),
            );
            if selected {
                // Right-edge resize handle: small square centred on the
                // right edge near the top so it's reachable even on a
                // multi-line box.
                out.push(
                    div()
                        .absolute()
                        .left(px(sx + *width - 4.0))
                        .top(px(sy + 2.0))
                        .w(px(8.0))
                        .h(px(8.0))
                        .bg(theme_background)
                        .border_1()
                        .border_color(theme_ring)
                        .rounded(px(2.0))
                        .into_any_element(),
                );
            }
        }
        out
    } else {
        Vec::new()
    };

    // Position labels: small chips at the right edge of each position rect
    // showing the three price levels (entry / TP / SL). Rendered as divs to
    // avoid bookkeeping a text-shape cache in the paint closure.
    let position_labels: Vec<gpui::AnyElement> = if let Some(bounds) = state.bounds {
        let canvas_w = bounds.size.width.as_f32();
        let canvas_h = bounds.size.height.as_f32();
        let mut out = Vec::new();
        for d in &drawings_snapshot {
            let (t0, t1, entry, tp, sl) = match d {
                Drawing::Long {
                    t0,
                    t1,
                    entry,
                    take_profit,
                    stop_loss,
                    ..
                }
                | Drawing::Short {
                    t0,
                    t1,
                    entry,
                    take_profit,
                    stop_loss,
                    ..
                } => (*t0, *t1, *entry, *take_profit, *stop_loss),
                _ => continue,
            };
            let x0 = index_to_screen(
                state.view_start,
                state.view_size,
                t0,
                canvas_w,
                state.y_axis_gap_px.get(),
            );
            let x1 = index_to_screen(
                state.view_start,
                state.view_size,
                t1,
                canvas_w,
                state.y_axis_gap_px.get(),
            );
            let xmax = x0.max(x1);
            let y_entry = price_to_screen(y_lo_for_overlay, y_hi_for_overlay, entry, canvas_h);
            let y_tp = price_to_screen(y_lo_for_overlay, y_hi_for_overlay, tp, canvas_h);
            let y_sl = price_to_screen(y_lo_for_overlay, y_hi_for_overlay, sl, canvas_h);
            let make_label = |y: f32, text: String, color: Hsla| -> gpui::AnyElement {
                div()
                    .absolute()
                    .left(px(xmax + 4.0))
                    .top(px(y - 7.0))
                    .text_xs()
                    .text_color(color)
                    .bg(theme_background)
                    .px_1()
                    .rounded(px(2.))
                    .child(SharedString::from(text))
                    .into_any_element()
            };
            out.push(make_label(
                y_entry,
                format!("E ${}", format_price(entry)),
                theme_muted_foreground,
            ));
            out.push(make_label(
                y_tp,
                format!("TP ${}", format_price(tp)),
                theme_chart_bullish,
            ));
            out.push(make_label(
                y_sl,
                format!("SL ${}", format_price(sl)),
                theme_chart_bearish,
            ));
            // R:R: reward / risk. Sign-flipped per direction so the printed
            // ratio is a positive number when the user followed convention.
            let (reward, risk) = match d {
                Drawing::Long { .. } => (tp - entry, entry - sl),
                Drawing::Short { .. } => (entry - tp, sl - entry),
                _ => (0.0, 0.0),
            };
            if risk.abs() > 1e-6 {
                let rr = reward / risk;
                out.push(make_label(
                    y_sl + 18.0,
                    format!("R:R 1:{:.2}", rr.abs()),
                    theme_muted_foreground,
                ));
            }
        }
        out
    } else {
        Vec::new()
    };

    // Inline text editor (Input wrapped in a positioned div). Stops mouse
    // propagation so clicks inside don't fire the canvas's commit-on-click.
    let editor_overlay: Option<gpui::AnyElement> =
        if let (Some(editing), Some(bounds)) = (state.editing_text.as_ref(), state.bounds) {
            let canvas_w = bounds.size.width.as_f32();
            let canvas_h = bounds.size.height.as_f32();
            let sx = index_to_screen(
                state.view_start,
                state.view_size,
                editing.anchor.0,
                canvas_w,
                state.y_axis_gap_px.get(),
            );
            let sy = price_to_screen(
                y_lo_for_overlay,
                y_hi_for_overlay,
                editing.anchor.1,
                canvas_h,
            );
            Some(
                div()
                    .absolute()
                    .left(px(sx))
                    .top(px(sy))
                    .w(px(editing.width))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(Input::new(&editing.input).text_xs())
                    .into_any_element(),
            )
        } else {
            None
        };

    let canvas = div()
        .id("chart-canvas")
        .relative()
        .flex_1()
        .min_h_0()
        .w_full()
        // Crosshair cursor everywhere on the canvas — the chart provides its
        // own guide lines + OHLC readout, and a crosshair OS cursor reads as
        // "this is a precise readout surface" better than the old grab/hand.
        .cursor_crosshair()
        .on_prepaint({
            let entity = entity.clone();
            move |bounds, _, cx| {
                entity.update(cx, |this, cx| {
                    if let Some(state) = this.chart_state.as_mut() {
                        // Overlay chrome (live price ray, axis pill, drawing
                        // labels) is positioned in render using `state.bounds`
                        // from the previous frame. When the canvas resizes
                        // (e.g. AI Chat opens and shrinks it), render runs with
                        // the stale wider size and the ray ends up clipped or
                        // misaligned until some other event triggers another
                        // render. Notify on size change so the next frame
                        // re-renders with the fresh bounds.
                        let size_changed =
                            state.bounds.map_or(true, |prev| prev.size != bounds.size);
                        state.bounds = Some(bounds);
                        if size_changed {
                            cx.notify();
                        }
                    }
                });
            }
        })
        .on_hover({
            let entity = entity.clone();
            move |&entered, _, cx| {
                if entered {
                    return;
                }
                // Cursor left the main canvas — clear its crosshair. The
                // cross-pane vertical guide (`cross_cursor_x`) is left alone
                // here: cursor might be entering a sub-pane next, and that
                // sub-pane's mouse_move will reset cross_cursor_x. We only
                // wipe `cross_cursor_x` if the cursor isn't in a sub-pane
                // either — that means the cursor has truly left the chart.
                entity.update(cx, |this, cx| {
                    if let Some(state) = this.chart_state.as_mut() {
                        let mut changed = state.cursor.take().is_some();
                        if state.sub_cursor.is_none() && state.cross_cursor_x.take().is_some() {
                            changed = true;
                        }
                        if changed {
                            cx.notify();
                        }
                    }
                });
            }
        })
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|this, ev: &MouseDownEvent, w, cx| {
                let Some(state) = this.chart_state.as_mut() else {
                    return;
                };
                // Drain any active text editor first. Mouse-down on the
                // canvas (i.e. not on the Input itself, which stop-propagates)
                // counts as "click outside" → commit. Eat the click so the
                // current tool's dispatch doesn't also fire.
                if let Some(editing) = state.editing_text.take() {
                    let value = editing.input.read(cx).value();
                    let trimmed = value.trim();
                    if !trimmed.is_empty() {
                        let interval = state.candle_interval_ms();
                        let candles_snap = state.candles.clone();
                        let symbol = state.symbol.clone();
                        let svc = cx.global::<DrawingServiceHandle>().0.clone();
                        match editing.existing_id {
                            Some(id) => {
                                // Rewrite the existing text drawing's text in
                                // place, preserving its anchor + width.
                                let existing = svc
                                    .read(cx)
                                    .for_symbol(symbol.as_ref())
                                    .iter()
                                    .find(|d| d.id == id)
                                    .map(|d| d.shape.clone());
                                if let Some(crate::drawings::shapes::DrawingShape::Text(mut t)) =
                                    existing
                                {
                                    t.text = trimmed.to_string();
                                    let symbol2 = symbol.clone();
                                    svc.update(cx, move |s, cx| {
                                        s.update_shape(
                                            symbol2.as_ref(),
                                            id,
                                            crate::drawings::shapes::DrawingShape::Text(t),
                                            cx,
                                        );
                                    });
                                }
                            }
                            None => {
                                let view = Drawing::Text {
                                    id: 0,
                                    anchor: editing.anchor,
                                    width: editing.width,
                                    text: trimmed.to_string(),
                                    font_size: crate::drawings::shapes::default_font_size(),
                                };
                                let shape = drawings_view::view_to_shape(
                                    &view,
                                    &candles_snap,
                                    interval,
                                    None,
                                );
                                let symbol2 = symbol.clone();
                                let id = svc.update(cx, |s, cx| s.add(symbol2.clone(), shape, cx));
                                svc.update(cx, |s, cx| s.set_selected(Some((symbol2, id)), cx));
                            }
                        }
                    }
                    cx.notify();
                    return;
                }

                let Some(bounds) = state.bounds else {
                    return;
                };
                let canvas_x = ev.position.x.as_f32() - bounds.origin.x.as_f32();
                let canvas_y = ev.position.y.as_f32() - bounds.origin.y.as_f32();
                let canvas_w = bounds.size.width.as_f32();
                let canvas_h = bounds.size.height.as_f32();
                if canvas_w <= 0.0 || canvas_h <= 0.0 {
                    return;
                }
                let (y_lo, y_hi) = state.y_range();
                let world_t = snap_t(screen_to_index(
                    state.view_start,
                    state.view_size,
                    canvas_x,
                    canvas_w,
                    state.y_axis_gap_px.get(),
                ));
                let world_p = screen_to_price(y_lo, y_hi, canvas_y, canvas_h);

                // Active tool comes from the global state — set by the top
                // bar's Draw popover, mirrored across every chart.
                let active_tool = cx
                    .global::<crate::drawings::tool::DrawingToolStateHandle>()
                    .0
                    .read(cx)
                    .tool();

                match active_tool {
                    Tool::HorizontalRay | Tool::HorizontalLine => {
                        // One-click commit: a horizontal ray (and the line
                        // variant) is defined by a single (time, price)
                        // anchor — no trailing endpoint to drag — so just
                        // write it through immediately. The HorizontalLine
                        // tool sets `extend_left=true` so the stroke spans
                        // the full chart width.
                        let extend_left = matches!(active_tool, Tool::HorizontalLine);
                        let drawing = Drawing::HorizontalRay {
                            id: 0,
                            anchor: (world_t, world_p),
                            text: None,
                            extend_left,
                        };
                        let shape = drawings_view::view_to_shape(
                            &drawing,
                            &state.candles,
                            state.candle_interval_ms(),
                            None,
                        );
                        let symbol = state.symbol.clone();
                        let svc = cx.global::<DrawingServiceHandle>().0.clone();
                        let symbol2 = symbol.clone();
                        let id = svc.update(cx, |s, cx| s.add(symbol2.clone(), shape, cx));
                        svc.update(cx, |s, cx| s.set_selected(Some((symbol2, id)), cx));
                        let tool_state = cx
                            .global::<crate::drawings::tool::DrawingToolStateHandle>()
                            .0
                            .clone();
                        tool_state.update(cx, |s, cx| s.reset(cx));
                        cx.notify();
                    }
                    Tool::AnchoredVwap => {
                        // Single-click commit: an Anchored VWAP needs only a
                        // time anchor (price isn't user-chosen; the line
                        // tracks the cumulative volume-weighted price from
                        // the anchor bar forward). `world_p` rides along for
                        // shape symmetry but is unused at render.
                        let drawing = Drawing::AnchoredVwap {
                            id: 0,
                            anchor: (world_t, world_p),
                        };
                        let shape = drawings_view::view_to_shape(
                            &drawing,
                            &state.candles,
                            state.candle_interval_ms(),
                            None,
                        );
                        let symbol = state.symbol.clone();
                        let svc = cx.global::<DrawingServiceHandle>().0.clone();
                        let symbol2 = symbol.clone();
                        let id = svc.update(cx, |s, cx| s.add(symbol2.clone(), shape, cx));
                        svc.update(cx, |s, cx| s.set_selected(Some((symbol2, id)), cx));
                        let tool_state = cx
                            .global::<crate::drawings::tool::DrawingToolStateHandle>()
                            .0
                            .clone();
                        tool_state.update(cx, |s, cx| s.reset(cx));
                        cx.notify();
                    }
                    Tool::Line
                    | Tool::Arrow
                    | Tool::Rectangle
                    | Tool::Fibonacci
                    | Tool::Long
                    | Tool::Short
                    | Tool::FixedRangeVolumeProfile => {
                        // Two-click creation: first click places the anchor,
                        // mouse-move updates the trailing point (in the
                        // mouse-move handler), and the second click commits.
                        // No drag-release path — the user explicitly asked
                        // for click-click.
                        if let Some(mut creating) = state.creating.take() {
                            // Second click: snap trailing anchor to this
                            // position and commit through the service.
                            creating.set_end((world_t, world_p));
                            let drawing = creating.into_drawing(0);
                            let shape = drawings_view::view_to_shape(
                                &drawing,
                                &state.candles,
                                state.candle_interval_ms(),
                                None,
                            );
                            let symbol = state.symbol.clone();
                            let svc = cx.global::<DrawingServiceHandle>().0.clone();
                            let symbol2 = symbol.clone();
                            let id = svc.update(cx, |s, cx| s.add(symbol2.clone(), shape, cx));
                            svc.update(cx, |s, cx| s.set_selected(Some((symbol2, id)), cx));
                            // After a one-shot creation, revert to Select so
                            // the user can immediately move what they drew.
                            let tool_state = cx
                                .global::<crate::drawings::tool::DrawingToolStateHandle>()
                                .0
                                .clone();
                            tool_state.update(cx, |s, cx| s.reset(cx));
                            cx.notify();
                        } else {
                            // First click: start a new creation. Trailing
                            // anchor starts at the same point so the preview
                            // collapses to a dot until the cursor moves.
                            // Round the default width to a whole number of
                            // candle slots so the position's `t1` lands on a
                            // bar centre — otherwise the right edge would
                            // float between two candles on creation. Minimum
                            // 1 slot so a zoomed-in viewport still gives a
                            // visible position.
                            let default_width = (state.view_size * POSITION_DEFAULT_WIDTH_RATIO)
                                .round()
                                .max(1.0);
                            state.creating = CreatingDrawing::from_tool(
                                active_tool,
                                (world_t, world_p),
                                default_width,
                            );
                            cx.notify();
                        }
                    }
                    Tool::Text => {
                        let input = cx.new(|cx| {
                            // auto_grow already implies multi-line input;
                            // 1..8 rows lets the editor expand as text wraps
                            // without ballooning when the user empties it.
                            InputState::new(w, cx).placeholder("Text…").auto_grow(1, 8)
                        });
                        // Focus the new input so the user can type immediately
                        // — without this they'd have to click the field first.
                        input.focus_handle(cx).focus(w, cx);
                        state.editing_text = Some(TextEditing {
                            existing_id: None,
                            anchor: (world_t, world_p),
                            width: TEXT_DEFAULT_WIDTH_PX,
                            input,
                        });
                        // Per spec: revert to Select after starting a text.
                        let tool_state = cx
                            .global::<crate::drawings::tool::DrawingToolStateHandle>()
                            .0
                            .clone();
                        tool_state.update(cx, |s, cx| s.reset(cx));
                        cx.notify();
                    }
                    Tool::Select => {
                        // Build the hit-test snapshot from the service, in
                        // view-coords. Drawings filtered for visibility +
                        // current-TF here so hidden / out-of-filter drawings
                        // don't intercept clicks.
                        let symbol = state.symbol.clone();
                        let tf_str = state.timeframe.as_str();
                        let interval = state.candle_interval_ms();
                        let visible_drawings: Vec<Drawing> = {
                            let svc = cx.global::<DrawingServiceHandle>().0.clone();
                            let svc_read = svc.read(cx);
                            svc_read
                                .for_symbol(symbol.as_ref())
                                .iter()
                                .filter(|d| d.visible_on(tf_str))
                                .map(|d| drawings_view::shape_to_view(d, &state.candles, interval))
                                .collect()
                        };
                        if let Some((hit_id, handle)) = hit_test_drawings(
                            &visible_drawings,
                            state.view_start,
                            state.view_size,
                            y_lo,
                            y_hi,
                            canvas_w,
                            canvas_h,
                            state.y_axis_gap_px.get(),
                            canvas_x,
                            canvas_y,
                        ) {
                            let baseline =
                                visible_drawings.iter().find(|d| d.id() == hit_id).cloned();
                            if let Some(baseline) = baseline {
                                // Double-click on a text drawing → open the
                                // inline editor for it. (Use the hit
                                // drawing's existing data so the resize +
                                // existing text are preserved.)
                                if ev.click_count >= 2 {
                                    if let Drawing::Text {
                                        anchor,
                                        width,
                                        text,
                                        ..
                                    } = &baseline
                                    {
                                        let existing_text = text.clone();
                                        let existing_width = *width;
                                        let existing_anchor = *anchor;
                                        let input = cx.new(|cx| {
                                            InputState::new(w, cx)
                                                .placeholder("Text…")
                                                .auto_grow(1, 8)
                                                .default_value(existing_text)
                                        });
                                        input.focus_handle(cx).focus(w, cx);
                                        state.editing_text = Some(TextEditing {
                                            existing_id: Some(hit_id),
                                            anchor: existing_anchor,
                                            width: existing_width,
                                            input,
                                        });
                                        cx.notify();
                                        return;
                                    }
                                }
                                let symbol2 = symbol.clone();
                                let svc = cx.global::<DrawingServiceHandle>().0.clone();
                                // Locked drawings select but never enter
                                // edit-drag — the strip surfaces an unlock
                                // toggle so the user can release the lock
                                // before moving the geometry.
                                let is_locked = svc
                                    .read(cx)
                                    .for_symbol(symbol2.as_ref())
                                    .iter()
                                    .any(|d| d.id == hit_id && d.locked);
                                svc.update(cx, |s, cx| s.set_selected(Some((symbol2, hit_id)), cx));
                                if !is_locked {
                                    state.edit_drag = Some(EditDrag {
                                        id: hit_id,
                                        handle,
                                        baseline,
                                        anchor_world: (world_t, world_p),
                                        anchor_screen: (canvas_x, canvas_y),
                                        moved: false,
                                    });
                                }
                                cx.notify();
                                return;
                            }
                        }
                        // Empty canvas: deselect + begin pan.
                        let svc = cx.global::<DrawingServiceHandle>().0.clone();
                        svc.update(cx, |s, cx| s.set_selected(None, cx));
                        state.drag_anchor = Some(CanvasDrag {
                            start_pos: ev.position,
                            start_view_start: state.view_start,
                            y_freeze: None,
                        });
                        cx.notify();
                    }
                }
            }),
        )
        .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _w, cx| {
            let Some(state) = this.chart_state.as_mut() else {
                return;
            };
            let Some(bounds) = state.bounds else {
                return;
            };
            let canvas_x = ev.position.x.as_f32() - bounds.origin.x.as_f32();
            let canvas_y = ev.position.y.as_f32() - bounds.origin.y.as_f32();
            let canvas_w = bounds.size.width.as_f32();
            let canvas_h = bounds.size.height.as_f32();
            if canvas_w <= 0.0 || canvas_h <= 0.0 {
                return;
            }
            // Crosshair: capture cursor unconditionally so the guide lines
            // and OHLC readout follow the mouse during pan/edit/creation too.
            // `cross_cursor_x` mirrors x so the cross-pane vertical guide and
            // chip-value-at-cursor pipeline work from any pane; clearing
            // `sub_cursor` ensures the previous sub-pane's horizontal guide
            // doesn't linger after the cursor returns to the main canvas.
            state.cursor = Some((canvas_x, canvas_y));
            state.cross_cursor_x = Some(canvas_x);
            state.sub_cursor = None;
            cx.notify();
            let (y_lo, y_hi) = state.y_range();
            let world_t = snap_t(screen_to_index(
                state.view_start,
                state.view_size,
                canvas_x,
                canvas_w,
                state.y_axis_gap_px.get(),
            ));
            let world_p = screen_to_price(y_lo, y_hi, canvas_y, canvas_h);

            // 1. In-progress 2-click creation: trailing anchor follows the
            // cursor regardless of mouse-button state. Don't gate on
            // `ev.dragging()` — hover-tracking is the whole point.
            if let Some(creating) = state.creating.as_mut() {
                creating.set_end((world_t, world_p));
                cx.notify();
                return;
            }

            if !ev.dragging() {
                // No button held and nothing in flight: clear any stale
                // pan/edit anchor so the next mouse-down starts fresh.
                let cleared =
                    state.drag_anchor.take().is_some() || state.edit_drag.take().is_some();
                if cleared {
                    cx.notify();
                }
                return;
            }

            // 2. Edit drag on an existing drawing. The baseline (snapshot at
            // drag start) is in chart-coords; we transform it by (dt, dp) and
            // broadcast the result via `preview_shape` (no persist). Final
            // persistence + snap-to-current-TF-grid happens on mouse-up.
            if let Some(drag) = state.edit_drag.clone() {
                let dt = world_t - drag.anchor_world.0;
                let dp = world_p - drag.anchor_world.1;
                let mut edited = drag.baseline.clone();
                let mut handled = false;
                if drag.handle == EditHandle::EndpointB {
                    // Text-width resize uses pixel delta, not world delta.
                    if let Drawing::Text { width: base_w, .. } = &drag.baseline {
                        let dx_px = canvas_x - drag.anchor_screen.0;
                        if let Drawing::Text { width, .. } = &mut edited {
                            *width = (base_w + dx_px).max(40.0);
                        }
                        handled = true;
                    }
                }
                if !handled {
                    apply_edit(&mut edited, &drag.baseline, drag.handle, dt, dp);
                }
                let symbol = state.symbol.clone();
                let svc = cx.global::<DrawingServiceHandle>().0.clone();
                let prev_shape: Option<crate::drawings::shapes::DrawingShape> = svc
                    .read(cx)
                    .for_symbol(symbol.as_ref())
                    .iter()
                    .find(|d| d.id == drag.id)
                    .map(|d| d.shape.clone());
                let shape = drawings_view::view_to_shape(
                    &edited,
                    &state.candles,
                    state.candle_interval_ms(),
                    prev_shape.as_ref(),
                );
                svc.update(cx, |s, cx| {
                    s.preview_shape(symbol.as_ref(), drag.id, shape, cx)
                });
                // Mark the drag as having actually produced motion so the
                // mouse-up handler knows to snap (vs treating a click-only as
                // a no-op).
                if let Some(d) = state.edit_drag.as_mut() {
                    if dt != 0.0 || dp != 0.0 {
                        d.moved = true;
                    }
                }
                cx.notify();
                return;
            }

            // 3. Canvas pan (Select tool, empty-area drag).
            let Some(mut pan_drag) = state.drag_anchor else {
                return;
            };

            // X pan: always active from drag start.
            let dx = ev.position.x.as_f32() - pan_drag.start_pos.x.as_f32();
            let candles_per_px = state.view_size / canvas_w;
            state.view_start = pan_drag.start_view_start - dx * candles_per_px;
            state.clamp();
            // Any horizontal motion during a pan means the user has chosen
            // to leave the live edge — disable sticky-tail. Pure clicks
            // (`dx == 0`) leave sticky alone so a no-motion mouse-down
            // doesn't silently drop the mode.
            if dx != 0.0 {
                state.sticky_to_latest = false;
            }

            // Y pan: lazy — only once vertical motion crosses the deadzone.
            // On first cross, freeze auto-fit and snapshot the y range so
            // subsequent moves translate from that baseline (no jump at
            // threshold cross).
            let dy = ev.position.y.as_f32() - pan_drag.start_pos.y.as_f32();
            if dy.abs() >= Y_FREEZE_DEADZONE_PX {
                if pan_drag.y_freeze.is_none() {
                    state.freeze_y_if_auto();
                    pan_drag.y_freeze = Some((ev.position, state.y_min, state.y_max));
                }
                if let Some((freeze_pos, baseline_min, baseline_max)) = pan_drag.y_freeze {
                    if canvas_h > 0.0 {
                        let dy_from_freeze = ev.position.y.as_f32() - freeze_pos.y.as_f32();
                        let range = baseline_max - baseline_min;
                        // Drag down (dy > 0) → price range shifts up so the
                        // chart content follows the hand. y = y_max maps to
                        // the canvas top, so increasing both min and max
                        // moves visible content downward on screen.
                        let delta = dy_from_freeze as f64 * range / canvas_h as f64;
                        state.y_min = baseline_min + delta;
                        state.y_max = baseline_max + delta;
                    }
                }
            }

            state.drag_anchor = Some(pan_drag);
            this.maybe_load_older(cx);
            cx.notify();
        }))
        .on_mouse_up(
            MouseButton::Left,
            cx.listener(|this, _ev, _w, cx| {
                let Some(state) = this.chart_state.as_mut() else {
                    return;
                };
                // Drawing creation commits on the *second* mouse-down (in
                // the on_mouse_down handler), not on mouse-up — so we don't
                // touch `state.creating` here.
                // Clear edit drag. If the drag actually moved the drawing,
                // snap each anchor to the current TF's candle grid (round
                // view-coord idx to integer) and commit; otherwise just
                // flush the in-memory state to disk in case any preview
                // happened to land on a non-integer position en route.
                if let Some(drag) = state.edit_drag.take() {
                    let svc = cx.global::<DrawingServiceHandle>().0.clone();
                    if drag.moved {
                        let symbol = state.symbol.clone();
                        let interval = state.candle_interval_ms();
                        let candles_snap = state.candles.clone();
                        let snapped: Option<crate::drawings::shapes::DrawingShape> = {
                            let svc_read = svc.read(cx);
                            svc_read
                                .for_symbol(symbol.as_ref())
                                .iter()
                                .find(|d| d.id == drag.id)
                                .map(|d| {
                                    let mut view =
                                        drawings_view::shape_to_view(d, &candles_snap, interval);
                                    snap_view_to_grid(&mut view);
                                    drawings_view::view_to_shape(
                                        &view,
                                        &candles_snap,
                                        interval,
                                        Some(&d.shape),
                                    )
                                })
                        };
                        if let Some(shape) = snapped {
                            svc.update(cx, |s, cx| {
                                s.update_shape(symbol.as_ref(), drag.id, shape, cx)
                            });
                        } else {
                            svc.update(cx, |s, _cx| s.flush_persist());
                        }
                    } else {
                        svc.update(cx, |s, _cx| s.flush_persist());
                    }
                    cx.notify();
                    return;
                }
                // Clear pan anchor.
                if state.drag_anchor.take().is_some() {
                    cx.notify();
                }
            }),
        )
        // Right-click hit-test → write the target (drawing or empty) into the
        // workspace `LastChartRightClick` global so the chart's context_menu
        // builder can shape itself per-drawing or canvas-wide. We don't stop
        // propagation here so the framework's ContextMenu element still sees
        // the right-mouse-down and opens its menu.
        .on_mouse_down(
            MouseButton::Right,
            cx.listener(|this, ev: &MouseDownEvent, _w, cx| {
                let Some(state) = this.chart_state.as_ref() else {
                    return;
                };
                let symbol = state.symbol.clone();
                let target: Option<DrawingId> = (|| {
                    let bounds = state.bounds?;
                    let canvas_x = ev.position.x.as_f32() - bounds.origin.x.as_f32();
                    let canvas_y = ev.position.y.as_f32() - bounds.origin.y.as_f32();
                    let canvas_w = bounds.size.width.as_f32();
                    let canvas_h = bounds.size.height.as_f32();
                    if canvas_w <= 0.0 || canvas_h <= 0.0 {
                        return None;
                    }
                    let (y_lo, y_hi) = state.y_range();
                    let tf_str = state.timeframe.as_str();
                    let interval = state.candle_interval_ms();
                    let visible: Vec<Drawing> = {
                        let svc = cx.global::<DrawingServiceHandle>().0.clone();
                        let svc_read = svc.read(cx);
                        svc_read
                            .for_symbol(symbol.as_ref())
                            .iter()
                            .filter(|d| d.visible_on(tf_str))
                            .map(|d| drawings_view::shape_to_view(d, &state.candles, interval))
                            .collect()
                    };
                    hit_test_drawings(
                        &visible,
                        state.view_start,
                        state.view_size,
                        y_lo,
                        y_hi,
                        canvas_w,
                        canvas_h,
                        state.y_axis_gap_px.get(),
                        canvas_x,
                        canvas_y,
                    )
                    .map(|(id, _handle)| id)
                })();
                let global = cx
                    .global::<crate::drawings::LastChartRightClick>()
                    .0
                    .clone();
                *global.borrow_mut() = Some(crate::drawings::RightClickTarget {
                    symbol,
                    drawing_id: target,
                });
            }),
        )
        .on_scroll_wheel(cx.listener(|this, ev: &ScrollWheelEvent, w, cx| {
            let Some(state) = this.chart_state.as_mut() else {
                return;
            };
            let delta_y = ev.delta.pixel_delta(w.line_height()).y.as_f32();
            if delta_y == 0.0 {
                return;
            }
            // Wheel-up (positive delta_y) zooms IN (smaller view_size); wheel-down zooms out.
            let factor = (-delta_y / SCROLL_ZOOM_RATE).exp();
            // Anchor the zoom at the rightmost candle slot of the viewport
            // (visible or virtual — i.e. inside the right buffer). Holding
            // this point fixed in candle-index space means scrolling controls
            // how far into the past the chart renders, while the rightmost
            // bar stays parked where it is.
            //
            // CRITICAL: clamp `view_size` BEFORE computing the new
            // `view_start`. If we clamp after, hitting the min/max would
            // still let the unclamped product through to `view_start`, then
            // `clamp()` would snap `view_size` back without un-shifting
            // `view_start` — drifting the right edge sideways every wheel
            // tick after the candle width hits its limit.
            let right_edge = state.view_start + state.view_size;
            let total = state.candles.len() as f32;
            let new_view_size = (state.view_size * factor)
                .clamp(CHART_MIN_VIEW.min(total), CHART_MAX_VIEW.min(total));
            state.view_size = new_view_size;
            state.view_start = right_edge - state.view_size;
            state.clamp();
            this.maybe_load_older(cx);
            cx.notify();
        }))
        // Inner wrapper for the candle paint primitive. Owns the chart's
        // right-click context menu so its hitbox is registered EARLY in
        // the prepaint order — `gpui::hit_test` iterates hitboxes in
        // reverse registration order and breaks on the first `BlockMouse`
        // (occluding) hitbox it encounters. The indicator chips register a
        // `.occlude()` hitbox during their prepaint LATER in this same
        // tree, so an early-registered chart-context-menu hitbox sits
        // BEHIND the chips and gets occluded when the cursor is over a
        // chip. (Putting the context_menu on the outer chart-canvas div
        // doesn't work — its hitbox is registered LAST, in front of every
        // chip, and reverse iteration visits it before any occluding chip
        // hitbox can break the loop.)
        .child(
            div()
                .size_full()
                .child(
                    // Custom main-chart paint: continuous candle x-positions plus
                    // auto-fit grid + axis labels. Replaces `CandlestickChart`
                    // whose `ScaleBand` slot positioning made horizontal pan feel
                    // discrete.
                    canvas(|_, _, _| (), {
                        // Capture bullish/bearish before `main_chart_colors` is
                        // moved into the closure: `MainChartColors` isn't Copy
                        // and `paint_main_chart` consumes it.
                        let overlay_bullish = main_chart_colors.bullish;
                        let overlay_bearish = main_chart_colors.bearish;
                        move |bounds, _, window, cx| {
                            // Clip every paint call to the canvas's bounds.
                            // Without this, wicks of candles whose high/low
                            // sit outside the locked y range (or the chart's
                            // 10px top inset) paint past `chart_bottom` into
                            // the sub-pane below — visible as candle bleed.
                            // Mirrors what `render_drawings_overlay` does for
                            // drawing labels.
                            window.with_content_mask(Some(ContentMask { bounds }), |window| {
                                // Heatmap paints first — behind candles/grid.
                                if let Some(rect) = &paint_heatmap_rect {
                                    paint_heatmap(
                                        rect,
                                        bounds.origin,
                                        &paint_candles,
                                        paint_start_idx,
                                        paint_candle_interval_ms,
                                        paint_view_start,
                                        paint_view_size,
                                        f32::from(bounds.size.width),
                                        paint_y_axis_gap,
                                        y_lo,
                                        y_hi,
                                        f32::from(bounds.size.height),
                                        paint_volume_unit,
                                        window,
                                        cx,
                                    );
                                }
                                paint_main_chart(
                                    bounds,
                                    &paint_candles,
                                    paint_start_idx,
                                    paint_view_start,
                                    paint_view_size,
                                    y_lo,
                                    y_hi,
                                    paint_candle_interval_ms,
                                    paint_y_axis_gap,
                                    main_chart_colors,
                                    paint_render_kind,
                                    paint_render_visible,
                                    paint_footprint_params.as_ref(),
                                    &paint_footprint_cells,
                                    paint_volume_unit,
                                    window,
                                    cx,
                                );
                                // Overlay indicators paint after candles + grid but
                                // before drawings, so user-drawn lines stay on top.
                                paint_overlay_indicators(
                                    bounds,
                                    paint_start_idx,
                                    paint_candles.len(),
                                    paint_view_start,
                                    paint_view_size,
                                    y_lo,
                                    y_hi,
                                    paint_y_axis_gap,
                                    &paint_overlay_items,
                                    overlay_bullish,
                                    overlay_bearish,
                                    window,
                                );
                            });
                        }
                    })
                    .size_full(),
                )
                // Right-click → context menu shaped by the hit-test captured on
                // right-mouse-down. Drawing hit → per-drawing actions (Show/Hide,
                // Visible-on submenu, Delete) plus canvas defaults. Empty area →
                // canvas defaults only (Clear drawings on chart, Reset scale).
                // `action_context` routes dispatched actions up through this panel's
                // focus handle so multi-chart workspaces don't fight over them.
                // Hosted on the inner paint wrapper (not the outer chart-canvas
                // div) so its hitbox registers early in prepaint and indicator
                // chips with `.occlude()` can shadow it — see the wrapper's own
                // doc-comment above.
                .context_menu({
                    let focus = focus.clone();
                    move |menu, _window, cx| {
                        let mut menu = menu.action_context(focus.clone());
                        let target = cx
                            .try_global::<crate::drawings::LastChartRightClick>()
                            .and_then(|g| g.0.borrow().clone());
                        if let Some(target) = target {
                            if let Some(drawing_id) = target.drawing_id {
                                // Snapshot the drawing's `hidden` flag + shape
                                // kind so the menu builder doesn't re-borrow
                                // the service. `is_ray` gates the "Edit label"
                                // item since only horizontal rays carry a text
                                // label. Per-TF visibility lives on the
                                // floating settings window only (cleaner
                                // affordance than a deep submenu).
                                let (hidden, is_ray) = {
                                    let svc = cx
                                        .global::<crate::drawings::service::DrawingServiceHandle>()
                                        .0
                                        .clone();
                                    let svc_read = svc.read(cx);
                                    svc_read
                                        .for_symbol(target.symbol.as_ref())
                                        .iter()
                                        .find(|d| d.id == drawing_id)
                                        .map(|d| {
                                            let is_ray = matches!(
                                                &d.shape,
                                                crate::drawings::shapes::DrawingShape::HorizontalRay(_)
                                            );
                                            (d.hidden, is_ray)
                                        })
                                        .unwrap_or((false, false))
                                };
                                let sym_select = target.symbol.clone();
                                menu = menu.menu(
                                    "Select",
                                    Box::new(crate::drawings::actions::SelectDrawing {
                                        symbol: sym_select,
                                        id: drawing_id,
                                    }),
                                );
                                if is_ray {
                                    let sym_label = target.symbol.clone();
                                    menu = menu.menu(
                                        "Edit label",
                                        Box::new(crate::drawings::actions::EditHorizontalRayText {
                                            symbol: sym_label,
                                            id: drawing_id,
                                        }),
                                    );
                                }
                                let sym_hidden = target.symbol.clone();
                                menu = menu.menu(
                                    if hidden { "Show" } else { "Hide" },
                                    Box::new(crate::drawings::actions::ToggleDrawingHidden {
                                        symbol: sym_hidden,
                                        id: drawing_id,
                                    }),
                                );
                                let sym_del = target.symbol.clone();
                                menu = menu.menu(
                                    "Delete",
                                    Box::new(crate::drawings::actions::DeleteDrawing {
                                        symbol: sym_del,
                                        id: drawing_id,
                                    }),
                                );
                                menu = menu.separator();
                            }
                        }
                        menu.menu("Go to latest", Box::new(GoToLatest))
                            .menu(
                                "Clear drawings on chart",
                                Box::new(crate::drawings::actions::ClearChartDrawings),
                            )
                            .menu("Reset chart scale", Box::new(ResetChartScale))
                    }
                }),
        )
        // Drawings paint between candles and the axis interaction zones —
        // visually above the chart, but the (non-interactive) overlay
        // doesn't intercept mouse events so the canvas's own handlers stay
        // in charge of tool dispatch.
        .child(drawings_overlay)
        // Text labels render as positioned divs above lines/rects.
        .children(text_labels)
        // Position price/R:R labels — wrapped in a clip surface that
        // matches the chart canvas area (excluding both axis gutters) so
        // labels drawn at a rect's right edge don't bleed past the y-axis
        // and overpaint the price labels there.
        .child(
            div()
                .absolute()
                .left_0()
                .top_0()
                .right(px(state.y_axis_gap_px.get()))
                .bottom(px(AXIS_GAP))
                .overflow_hidden()
                .children(position_labels),
        )
        // Main-pane indicator list (header chip + collapsible overlay
        // chips) absolute-anchored at the canvas top-left. Rendered after
        // position labels so chips sit on top of any position rect that
        // happens to land at the same corner.
        .child(render_main_indicator_list(state, cx))
        // Active text editor (Input). Above labels so its caret/selection
        // chrome isn't visually clipped by a stale label.
        .children(editor_overlay)
        // Axis zones go AFTER the chart so they sit on top in z-order and
        // get hit-tested first — their handlers `cx.stop_propagation()` to
        // keep mouse-down from also arming the canvas's pan drag.
        .child(right_axis)
        .child(bottom_axis)
        // Crosshair chrome (time + price labels, OHLC readout) on top of the
        // axes so the cursor's labels sit above the chart's static labels.
        .children(crosshair_chrome)
        // Live developing-bar guide (price ray, axis pill, countdown). Last
        // so the pill sits above the static y-axis labels on the right edge.
        .children(live_price_chrome)
        // Per-ray price pills on the y-axis. After live_price_chrome so a
        // ray drawn at the live price won't completely hide the live pill.
        .children(ray_price_chrome)
        // Floating "Go to latest" button (bottom-right). Last in the
        // children chain so it z-orders above the axis chrome and any
        // pills/labels that might sit at the corner.
        .children(go_to_latest_chrome);
    // Note: the chart-wide right-click context menu lives on the inner
    // paint wrapper div above (see its doc-comment for the z-order
    // reasoning). The outer canvas div intentionally has none.

    // Build one (splitter + sub-canvas) pair per pane indicator. The
    // splitter sits ABOVE its sub-pane; dragging it up grows the sub-pane,
    // dragging down shrinks it (the main canvas's `flex_1` absorbs the
    // remainder either way). Convention matches TradingView's per-pane
    // top-edge resize handle.
    let pane_grid_color = Hsla {
        a: 0.30,
        ..theme_border
    };
    let pane_label_color = theme_muted_foreground;
    // Full-contrast text for sub-pane cells whose label sits on top of a
    // coloured fill (e.g. BarStat). Mirrors the main chart's `cell_text`.
    let pane_cell_text_color = theme_foreground;
    let pane_bullish = theme_chart_bullish;
    let pane_bearish = theme_chart_bearish;
    // Pull these out once so the per-iter closure construction below doesn't
    // touch `paint_candles` (already moved into the main canvas closure).
    let pane_visible_count = paint_candles_len;
    let pane_start_idx = paint_start_idx;
    let pane_view_start = paint_view_start;
    let pane_view_size = paint_view_size;
    let pane_y_axis_gap = paint_y_axis_gap;
    // Snapshot the cross-pane cursor x once — it's the same value passed to
    // every sub-canvas closure for the vertical guide. Per-pane `hovered_y`
    // is derived inside the loop from `state.sub_cursor.id == instance_id`.
    let pane_cross_x = state.cross_cursor_x;
    let pane_sub_cursor = state.sub_cursor;
    let pane_cursor_idx = state.cursor_bar_index();
    let mut sub_panes: Vec<gpui::AnyElement> = Vec::new();
    for (instance_id, pane_height, item) in paint_pane_items.into_iter() {
        // Snapshot per-iter to move into closures.
        let item_for_paint = item;
        // Build the sub-pane chip overlay now while we still have an
        // immutable borrow on `state` — the canvas-building closures below
        // re-borrow `state` indirectly via the entity, so the chip needs
        // to be constructed up front and consumed into the sub-pane div.
        let pane_chip: Option<gpui::AnyElement> = state
            .indicators()
            .iter()
            .zip(state.indicator_outputs.iter())
            .find(|(i, _)| i.id == instance_id)
            .map(|(i, o)| render_indicator_chip(i, o, pane_cursor_idx, cx));
        // Horizontal y-guide + value-readout pill only paint when THIS pane
        // is the hovered one. `sub_cursor` carries (id, x, y); the y is
        // canvas-relative to whichever sub-pane wrote it.
        let pane_hovered_y = match pane_sub_cursor {
            Some((id, _x, y)) if id == instance_id => Some(y),
            _ => None,
        };
        let splitter_id_str = SharedString::from(format!("pane-splitter-{}", instance_id));
        let sub_canvas_id_str = SharedString::from(format!("pane-canvas-{}", instance_id));
        sub_panes.push(
            div()
                .id(splitter_id_str)
                .flex_none()
                .h(px(4.0))
                .w_full()
                .cursor_ns_resize()
                // Slight tinted bar so the resize handle is visible against
                // the panel background. Theme border at low alpha — same
                // visual weight as the chart's grid lines.
                .bg(Hsla {
                    a: 0.45,
                    ..theme_border
                })
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, ev: &MouseDownEvent, _w, cx| {
                        let Some(state) = this.chart_state.as_mut() else {
                            return;
                        };
                        // Look up CURRENT pane_height — paint_pane_items
                        // captured a copy at render time, but a sibling
                        // splitter might've adjusted it before this drag.
                        let current_h = state
                            .indicators()
                            .iter()
                            .find(|i| i.id == instance_id)
                            .and_then(|i| i.pane_height)
                            .unwrap_or(pane_height);
                        state.splitter_drag = Some(SplitterDrag {
                            instance_id,
                            start_y: ev.position.y.as_f32(),
                            start_height: current_h,
                        });
                        cx.stop_propagation();
                    }),
                )
                .into_any_element(),
        );
        sub_panes.push(
            div()
                .id(sub_canvas_id_str)
                .flex_none()
                .relative()
                .w_full()
                .h(px(pane_height))
                // Crosshair cursor mirrors the main canvas so the hover
                // affordance reads identically across panes — except when
                // FRVP is active. FRVP only renders inside the main
                // candle pane, so painting a crosshair over an indicator
                // pane would suggest the user could click here to start a
                // bracket. `not-allowed` makes the restriction explicit;
                // the sub-pane has no mouse-down handler for FRVP so the
                // click is already a silent no-op.
                .map(|d| if matches!(active_tool, Tool::FixedRangeVolumeProfile) {
                    d.cursor_not_allowed()
                } else {
                    d.cursor_crosshair()
                })
                .on_prepaint({
                    let entity = entity.clone();
                    move |bounds, _, cx| {
                        // Stash this sub-pane's bounds so the on_mouse_move
                        // handler can convert window-relative event coords
                        // into canvas-relative cursor (x, y).
                        entity.update(cx, |this, _cx| {
                            if let Some(state) = this.chart_state.as_mut() {
                                state.pane_bounds.insert(instance_id, bounds);
                            }
                        });
                    }
                })
                .on_mouse_move(cx.listener(move |this, ev: &MouseMoveEvent, _w, cx| {
                    let Some(state) = this.chart_state.as_mut() else {
                        return;
                    };
                    let Some(bounds) = state.pane_bounds.get(&instance_id).copied() else {
                        return;
                    };
                    let canvas_x = ev.position.x.as_f32() - bounds.origin.x.as_f32();
                    let canvas_y = ev.position.y.as_f32() - bounds.origin.y.as_f32();
                    let canvas_w = bounds.size.width.as_f32();
                    let canvas_h = bounds.size.height.as_f32();
                    if canvas_w <= 0.0 || canvas_h <= 0.0 {
                        return;
                    }
                    // Sub-pane hover wipes the main-pane crosshair (only one
                    // pane is "active" at a time), then sets the cross-pane
                    // shared x + this pane's y for the horizontal guide.
                    state.cursor = None;
                    state.cross_cursor_x = Some(canvas_x);
                    state.sub_cursor = Some((instance_id, canvas_x, canvas_y));
                    cx.notify();
                }))
                .on_hover({
                    let entity = entity.clone();
                    move |&entered, _, cx| {
                        if entered {
                            return;
                        }
                        // Cursor left this sub-pane. Only clear if our id is
                        // the one currently held — moving to a sibling pane
                        // would mean the sibling already overwrote sub_cursor,
                        // and we don't want to clobber its state.
                        entity.update(cx, |this, cx| {
                            if let Some(state) = this.chart_state.as_mut() {
                                let mut changed = false;
                                if let Some((id, _, _)) = state.sub_cursor {
                                    if id == instance_id {
                                        state.sub_cursor = None;
                                        changed = true;
                                    }
                                }
                                // Cross-pane x stays alive while another pane
                                // is hovered. If neither main nor any sub-pane
                                // is hovered we let the cross x linger; the
                                // outer panel root has no on_hover wired (v1
                                // limitation — small visual artifact on exit).
                                if state.cursor.is_none()
                                    && state.sub_cursor.is_none()
                                    && state.cross_cursor_x.take().is_some()
                                {
                                    changed = true;
                                }
                                if changed {
                                    cx.notify();
                                }
                            }
                        });
                    }
                })
                .child(
                    // `canvas` the local div is bound below; use the
                    // fully-qualified path to reach the gpui paint helper.
                    gpui::canvas(
                        |_, _, _| (),
                        move |bounds, _, window, cx| {
                            paint_sub_pane(
                                bounds,
                                pane_start_idx,
                                pane_visible_count,
                                pane_view_start,
                                pane_view_size,
                                pane_y_axis_gap,
                                &item_for_paint,
                                pane_bullish,
                                pane_bearish,
                                pane_grid_color,
                                pane_label_color,
                                pane_cell_text_color,
                                pane_cross_x,
                                pane_hovered_y,
                                window,
                                cx,
                            );
                        },
                    )
                    .size_full(),
                )
                // Sub-pane chip overlay: the lone indicator's chip pinned
                // at top-left of its own pane. Doubles as the un-hide
                // affordance when the pane is muted (paint_sub_pane is a
                // no-op then, but the chip still renders).
                .children(pane_chip.map(|chip| {
                    div()
                        .absolute()
                        .top(px(4.0))
                        .left(px(4.0))
                        .child(chip)
                        .into_any_element()
                }))
                .into_any_element(),
        );
    }

    v_flex()
        .id("chart-panel-root")
        .size_full()
        // No bottom padding so the chart-canvas reaches the panel's bottom
        // edge — otherwise the panel-level padding shows below the x-axis
        // chrome as a visible gap.
        .pt_3()
        .pl_3()
        .pr_3()
        .gap_2()
        // Splitter drag handlers attach here so the cursor can stray off
        // the 4px splitter bar and still drive the resize — mouse_move on
        // the splitter alone would die the moment the cursor crossed its
        // 4px boundary. Limitation: drag also dies when the cursor exits
        // the panel root entirely (v1 — a global pointer-capture would
        // fix it but isn't needed for the common adjust gesture).
        .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _w, cx| {
            let Some(state) = this.chart_state.as_mut() else {
                return;
            };
            let Some(drag) = state.splitter_drag else {
                return;
            };
            // Splitter sits ABOVE its sub-pane. Drag up (delta_y < 0)
            // grows the pane; drag down (delta_y > 0) shrinks it.
            let delta_y = ev.position.y.as_f32() - drag.start_y;
            let new_h = drag.start_height - delta_y;
            state.set_indicator_pane_height(drag.instance_id, new_h);
            cx.notify();
        }))
        .on_mouse_up(
            MouseButton::Left,
            cx.listener(|this, _ev, _w, cx| {
                let Some(state) = this.chart_state.as_mut() else {
                    return;
                };
                if state.splitter_drag.take().is_some() {
                    cx.notify();
                    // Persist the new pane height once on release rather than
                    // on every move event — the dock's save is already
                    // debounced 500ms but emitting LayoutChanged hundreds of
                    // times per drag is still wasted work.
                    crate::panels::request_layout_save(cx);
                }
            }),
        )
        .child(
            h_flex()
                // `flex_none` pins the header row at its natural height;
                // without it the canvas's `flex_1` can compress the header
                // and the canvas paint encroaches over the symbol/timeframe
                // controls.
                .flex_none()
                .w_full()
                .gap_3()
                .items_center()
                .child(symbol_button)
                .child(timeframe_btn)
                .child(render_btn)
                .child(volume_unit_btn)
                .child(heatmap_btn)
                .children(heatmap_gear)
                // `+ Indicator` button — dispatches `OpenIndicatorPicker`
                // which the workspace resolves to this chart via the
                // `LastFocusedChart` global (already kept fresh by the
                // panel's mouse-down handler). Cmd-I / Ctrl-I is the
                // keyboard equivalent (workspace-scoped).
                .child(
                    Button::new("chart-add-indicator")
                        .label(SharedString::from("+ Indicator"))
                        .small()
                        .ghost()
                        .on_click(|_ev, window, cx| {
                            window.dispatch_action(
                                Box::new(crate::indicator_picker::OpenIndicatorPicker),
                                cx,
                            );
                        }),
                )
                // Indicator chips no longer live in the toolbar — the
                // main-pane `Indicators (N) ▼` list at the canvas's top-left
                // is the visual home for overlay-placed indicators, and
                // pane-placed indicators wear their chip on their own
                // sub-pane. The toolbar keeps only actions (+ Indicator).
                // Company name + exchange takes the leftover space and is the
                // first thing to give up width when the panel shrinks — same
                // `flex_1().min_w_0().truncate()` idiom used in symbol_picker.
                // `min_w_0` is the load-bearing bit; without it the flex item
                // refuses to shrink below its content width and pushes the
                // status badge off the right edge.
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_color(theme_muted_foreground)
                        .text_sm()
                        .child(format!("{} · {}", header_name, header_exchange)),
                )
                .child(
                    h_flex()
                        .flex_none()
                        .gap_1p5()
                        .items_center()
                        .child(div().size_2().rounded_full().bg(badge_color))
                        .child(div().text_xs().text_color(badge_color).child(badge_label)),
                ),
        )
        // Chart stack: main candle canvas (flex_1) + (splitter + sub_canvas)
        // pairs for each pane indicator. Main canvas keeps its own `flex_1`
        // so it absorbs whatever space the sub-panes (and their splitters)
        // don't claim — including resize-driven changes to pane_height.
        .child(
            v_flex()
                .flex_1()
                .min_h_0()
                .w_full()
                .child(canvas)
                .children(sub_panes),
        )
}
