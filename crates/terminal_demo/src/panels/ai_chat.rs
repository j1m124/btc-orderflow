use gpui::{
    AppContext as _, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, ScrollHandle, SharedString, StatefulInteractiveElement as _,
    Styled as _, Window, div, prelude::FluentBuilder as _, px,
};
use gpui_component::{
    ActiveTheme as _, IconName, Sizable as _, StyledExt as _,
    button::{Button, ButtonRounded, ButtonVariants as _},
    h_flex,
    input::{Input, InputEvent, InputState},
    menu::DropdownMenu as _,
    text::{TextView, TextViewState},
    v_flex,
};

use super::{AiChatMarkdownEntry, ContentPanel};
use crate::drawings::service::DrawingServiceHandle;
use crate::services::ai_chat::{
    AiChatEvent, AiChatService, AiChatServiceHandle, AnthropicModel, ContentBlock,
    Session, Speaker,
};
use crate::top_bar::SetAiChatModel;

/// Default model id used when no session is selected — purely for the header
/// label; the service decides the actual model when a session is created.
const DEFAULT_HEADER_MODEL_ID: &str = "claude-sonnet-4-6";

/// Suggested empty-state prompts shown in Charting mode. Each one is picked
/// to exercise a different slice of the tool surface so a fresh user can
/// see what the assistant is capable of in a single click. Clicking a
/// prompt stages it into the input bar — the user must press Send/Enter to
/// actually fire (design Q15b: stage, don't auto-send).
const QUICK_PROMPTS: &[&str] = &[
    "Switch the focused chart to NVDA on the 1h timeframe and summarize the trend.",
    "Mark the key support and resistance levels with horizontal rays and label each.",
    "Draw a Fibonacci retracement from the most recent swing low to the swing high.",
    "Sketch a long setup with entry, stop, and target as a position rectangle.",
    "Compare AAPL's 5m and 1h candles and call out any divergence.",
    "Switch to a 2x2 quad layout and load SPY, QQQ, IWM, and DIA.",
];

/// Display-side catalog of every tool the AI can call from Charting mode.
/// Shown under the quick prompts as a "what can it do?" reference so the
/// user knows the surface without having to read the source. Order matches
/// the family grouping the server's system prompt uses; the icons mirror
/// the ones rendered on tool-use chips by `tool_icon` below so the empty
/// state and live calls feel like the same visual language.
const TOOL_REFERENCE: &[(&str, &[(&str, &str, &str)])] = &[
    (
        "Data",
        &[("📊", "get_candles", "fetch any symbol / timeframe range")],
    ),
    (
        "Drawings",
        &[
            ("─", "add_horizontal_ray", "price level"),
            ("✎", "add_text", "label anchored to time + price"),
            ("╱", "add_line", "trendline between two points"),
            ("➜", "add_arrow", "directional callout"),
            ("▭", "add_rectangle", "zone / box"),
            ("𝝫", "add_fibonacci", "retracement levels"),
        ],
    ),
    (
        "Trade ideas",
        &[
            ("▲", "add_long_position", "entry / stop / target box"),
            ("▼", "add_short_position", "entry / stop / target box"),
        ],
    ),
    (
        "Chart control",
        &[
            ("🔁", "set_symbol", "switch focused chart"),
            ("⏱", "set_timeframe", "1m · 5m · 15m · 1h · 1d · …"),
            ("▦", "set_layout", "1 · 2-up · 2x2 · …"),
        ],
    ),
];

// ============================================================================
// AI Chat panel — two-column layout:
//   [ 220px sidebar (sessions) | chat area (header + messages + input) ]
//
// The hardcoded message slice is gone — the panel renders entirely off the
// service's session list. Sending a message goes through
// `AiChatService::send_active`, which appends a static stub assistant turn
// until the real Anthropic transport is wired in.
// ============================================================================

const SIDEBAR_WIDTH: f32 = 220.;
/// Width of a single message bubble (matches `max_w(px(MESSAGE_BOX_WIDTH))`
/// on the bubble div). The panel's floor is computed from this so a bubble
/// always fits at its natural width.
const MESSAGE_BOX_WIDTH: f32 = 420.;
/// Horizontal padding around the messages list — `p_3` on the scroll
/// container = 12px each side in the current theme; doubled = 24px.
const MESSAGES_HPAD: f32 = 24.;

pub fn render(
    panel: &mut ContentPanel,
    _window: &mut Window,
    cx: &mut Context<ContentPanel>,
) -> impl IntoElement {
    let input = panel
        .chat_input
        .as_ref()
        .expect("chat_input set for AiChat")
        .clone();
    let scroll = panel
        .ai_chat_scroll
        .as_ref()
        .expect("ai_chat_scroll set for AiChat")
        .clone();
    let theme = cx.theme();
    let border = theme.border;
    let muted = theme.muted_foreground;
    let fg = theme.foreground;
    let bg_user = theme.primary;
    let fg_user = theme.primary_foreground;
    let bg_assistant = theme.muted;
    let accent = theme.accent;
    let panel_bg = theme.background;
    let danger = theme.danger;
    let warning = theme.warning;

    let svc = cx.global::<AiChatServiceHandle>().0.read(cx);
    let sessions: Vec<Session> = svc.sessions_sorted().into_iter().cloned().collect();
    let selected_id = svc.selected_id().map(|s| s.to_string());
    let active = selected_id
        .as_deref()
        .and_then(|id| sessions.iter().find(|s| s.id == id))
        .cloned();
    let collapsed = svc.sidebar_collapsed();
    let pending = selected_id
        .as_deref()
        .map(|id| svc.is_pending(id))
        .unwrap_or(false);
    // Pre-resolve display metadata for each session's stored model id. This
    // is the *only* place the panel touches the service for model info — the
    // row + header renderers then work off owned data.
    let session_models: Vec<AnthropicModel> = sessions
        .iter()
        .map(|s| svc.model_meta(&s.model))
        .collect();
    let session_pending: Vec<bool> = sessions.iter().map(|s| svc.is_pending(&s.id)).collect();
    let active_model = active
        .as_ref()
        .map(|s| svc.model_meta(&s.model))
        .unwrap_or_else(|| svc.model_meta(DEFAULT_HEADER_MODEL_ID));
    let available_models: Vec<AnthropicModel> = svc.available_models().to_vec();
    // Release the read borrow before reaching into `panel` mutably below.
    let _ = svc;

    // Lazily build one TextViewState entity per assistant message in the
    // active session. The state holds the *joined* text of all Text blocks
    // (`ChatMsg::text()`) — same shape the streaming handler pushes into.
    // User messages don't need markdown (None).
    let active_markdown: Vec<Option<Entity<TextViewState>>> = match active.as_ref() {
        Some(sess) => sess
            .messages
            .iter()
            .enumerate()
            .map(|(i, m)| match m.role {
                Speaker::User => None,
                Speaker::Assistant => {
                    let key = (sess.id.clone(), i);
                    let entry =
                        panel.ai_chat_markdown.entry(key).or_insert_with(|| {
                            let text = m.text();
                            let bytes = text.len();
                            let state =
                                cx.new(|c| TextViewState::markdown(text.as_str(), c));
                            AiChatMarkdownEntry {
                                state,
                                pushed_bytes: bytes,
                            }
                        });
                    Some(entry.state.clone())
                }
            })
            .collect(),
        None => Vec::new(),
    };

    // Floor the chat content at the message bubble's width + its container
    // padding. Wrapped in an `overflow_x_scroll` div so dragging the dock
    // divider narrower than the floor scrolls horizontally rather than
    // squishing bubbles. The sessions sidebar is a *sibling* of the chat
    // (not an overlay), so opening it physically pushes the chat narrower
    // — the panel resizes naturally, matching the user's expectation that
    // collapsible sidebars move content around rather than cover it.
    let chat_floor = MESSAGE_BOX_WIDTH + MESSAGES_HPAD;

    // Quick prompts only show in Charting mode, where the AI's tool surface
    // is enabled (mirrors `AiToolsBridge::enabled_tools`). In other modes
    // the AI is a plain chat and the prompts would dangle.
    let show_quick_prompts = cx
        .try_global::<crate::panels::CurrentModeGlobal>()
        .map(|g| matches!(g.0, crate::persistence::Mode::Charting))
        .unwrap_or(false);
    let chat = render_chat_area(
        active.as_ref(),
        &active_model,
        &available_models,
        &active_markdown,
        &input,
        &scroll,
        collapsed,
        pending,
        show_quick_prompts,
        border,
        muted,
        fg,
        bg_user,
        fg_user,
        bg_assistant,
        danger,
        warning,
        accent,
    );

    let sidebar_element: Option<gpui::AnyElement> = if collapsed {
        None
    } else {
        Some(
            div()
                .h_full()
                .w(px(SIDEBAR_WIDTH))
                .flex_none()
                .bg(panel_bg)
                .child(render_sidebar(
                    &sessions,
                    &session_models,
                    &session_pending,
                    selected_id.as_deref(),
                    border,
                    muted,
                    fg,
                    accent,
                    warning,
                ))
                .into_any_element(),
        )
    };

    let chat_wrapper = div()
        .flex_1()
        .min_w_0()
        .h_full()
        .child(
            div()
                .id("ai-chat-hscroll")
                .size_full()
                .overflow_x_scroll()
                .child(div().min_w(px(chat_floor)).h_full().child(chat)),
        );

    h_flex()
        .h_full()
        .w_full()
        .children(sidebar_element)
        .child(chat_wrapper)
}

// ---------------------------------------------------------------------------
// Sidebar
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn render_sidebar(
    sessions: &[Session],
    session_models: &[AnthropicModel],
    session_pending: &[bool],
    selected_id: Option<&str>,
    border: gpui::Hsla,
    muted: gpui::Hsla,
    fg: gpui::Hsla,
    accent: gpui::Hsla,
    warning: gpui::Hsla,
) -> impl IntoElement {
    let header = h_flex()
        .px_2()
        .py_1p5()
        .gap_1()
        .items_center()
        .border_b_1()
        .border_color(border)
        .child(
            Button::new("ai-chat-toggle-sidebar")
                .icon(IconName::PanelLeftClose)
                .xsmall()
                .ghost()
                .tooltip("Collapse sidebar")
                .on_click(|_, _window, cx| {
                    let svc = cx.global::<AiChatServiceHandle>().0.clone();
                    svc.update(cx, |s, cx| s.toggle_sidebar(cx));
                }),
        )
        .child(div().flex_1())
        .child(
            Button::new("ai-chat-new-session")
                .icon(IconName::Plus)
                .label("New")
                .xsmall()
                .ghost()
                .tooltip("Start a new chat session")
                .on_click(|_, _window, cx| {
                    let svc = cx.global::<AiChatServiceHandle>().0.clone();
                    svc.update(cx, |s, cx| {
                        s.create_session(cx);
                    });
                }),
        );

    let rows = sessions
        .iter()
        .cloned()
        .zip(session_models.iter().cloned())
        .zip(session_pending.iter().copied())
        .map(move |((s, m), pending)| {
            render_session_row(s, m, pending, selected_id, muted, fg, accent, border, warning)
        });

    v_flex()
        .w(px(SIDEBAR_WIDTH))
        .h_full()
        .flex_none()
        .border_r_1()
        .border_color(border)
        .child(header)
        .child(
            div()
                .id("ai-chat-sessions")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .child(v_flex().w_full().children(rows)),
        )
}

#[allow(clippy::too_many_arguments)]
fn render_session_row(
    s: Session,
    model: AnthropicModel,
    pending: bool,
    selected_id: Option<&str>,
    muted: gpui::Hsla,
    fg: gpui::Hsla,
    accent: gpui::Hsla,
    border: gpui::Hsla,
    warning: gpui::Hsla,
) -> impl IntoElement {
    let is_selected = selected_id == Some(s.id.as_str());
    let row_id = SharedString::from(format!("ai-chat-row-{}", s.id));
    let select_id = s.id.clone();
    let delete_id = s.id.clone();
    let title_color = if s.is_untitled() { muted } else { fg };
    let title = SharedString::from(s.display_title().to_string());
    let model_badge = SharedString::from(model.short.clone());
    let token_label = SharedString::from(format!("{} tok", short_count(s.token_estimate())));
    let delete_btn_id = SharedString::from(format!("ai-chat-del-{}", s.id));

    let mut row = h_flex()
        .id(row_id)
        .px_3()
        .py_2()
        .gap_2()
        .items_start()
        .border_b_1()
        .border_color(border)
        .cursor_pointer()
        .hover(|st| st.bg(accent))
        .on_click(move |_, _window, cx| {
            let svc = cx.global::<AiChatServiceHandle>().0.clone();
            let id = select_id.clone();
            svc.update(cx, |s, cx| s.select(&id, cx));
        });
    if is_selected {
        row = row.bg(accent);
    }
    row.child(
        v_flex()
            .flex_1()
            .min_w_0()
            .gap_1()
            .child(
                div()
                    .text_sm()
                    .text_color(title_color)
                    .when(s.is_untitled(), |this| this.italic())
                    .child(title),
            )
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .px_1p5()
                            .py_0p5()
                            .rounded(px(3.))
                            .border_1()
                            .border_color(muted)
                            .text_xs()
                            .text_color(muted)
                            .child(model_badge),
                    )
                    .child(div().text_xs().text_color(muted).child(token_label))
                    .when(pending, |row| {
                        // Small dot + label so the user can see streaming
                        // is happening in this session even when viewing
                        // a different one (allow-concurrent decision).
                        row.child(
                            div()
                                .text_xs()
                                .text_color(warning)
                                .child("● streaming"),
                        )
                    }),
            ),
    )
    .child(
        Button::new(delete_btn_id)
            .icon(IconName::Close)
            .xsmall()
            .ghost()
            .tooltip("Delete session")
            .on_click(move |_, _window, cx| {
                let svc = cx.global::<AiChatServiceHandle>().0.clone();
                let id = delete_id.clone();
                svc.update(cx, |s, cx| s.delete(&id, cx));
            }),
    )
}

// ---------------------------------------------------------------------------
// Chat area
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn render_chat_area(
    active: Option<&Session>,
    active_model: &AnthropicModel,
    available_models: &[AnthropicModel],
    active_markdown: &[Option<Entity<TextViewState>>],
    input: &Entity<InputState>,
    scroll: &ScrollHandle,
    sidebar_collapsed: bool,
    pending: bool,
    show_quick_prompts: bool,
    border: gpui::Hsla,
    muted: gpui::Hsla,
    fg: gpui::Hsla,
    bg_user: gpui::Hsla,
    fg_user: gpui::Hsla,
    bg_assistant: gpui::Hsla,
    danger: gpui::Hsla,
    warning: gpui::Hsla,
    accent: gpui::Hsla,
) -> impl IntoElement {
    let token_count = active.map(|s| s.token_estimate()).unwrap_or(0);

    let header = render_header(
        active_model.clone(),
        available_models.to_vec(),
        token_count,
        sidebar_collapsed,
        muted,
        border,
        warning,
        danger,
    );
    let quick_prompts_input = if show_quick_prompts {
        Some(input.clone())
    } else {
        None
    };
    let messages = render_messages(
        active,
        active_markdown,
        scroll,
        quick_prompts_input,
        fg,
        muted,
        bg_user,
        fg_user,
        bg_assistant,
        danger,
        accent,
    );
    let input_bar = render_input(input, pending, muted, border);

    v_flex()
        .size_full()
        .child(header)
        .child(messages)
        .child(input_bar)
}

#[allow(clippy::too_many_arguments)]
fn render_header(
    active_model: AnthropicModel,
    available_models: Vec<AnthropicModel>,
    token_count: usize,
    sidebar_collapsed: bool,
    muted: gpui::Hsla,
    border: gpui::Hsla,
    warning: gpui::Hsla,
    danger: gpui::Hsla,
) -> impl IntoElement {
    let model_label = SharedString::from(active_model.label.clone());
    let active_id = active_model.id.clone();
    let dropdown = Button::new("ai-chat-model-select")
        .label(model_label)
        .xsmall()
        .ghost()
        .tooltip("Switch model for this session")
        .dropdown_menu(move |menu, _, _| {
            let mut menu = menu.label("Model");
            for option in &available_models {
                let prefix = if option.id == active_id { "✓ " } else { "  " };
                let label = SharedString::from(format!("{}{}", prefix, option.label));
                menu = menu.menu(
                    label,
                    Box::new(SetAiChatModel(SharedString::from(option.id.clone()))),
                );
            }
            menu
        });

    let token_label = SharedString::from(format!("{} tok", short_count(token_count)));
    // Token-count color escalation: ample slack vs. context window keeps
    // the count subtle (muted); over ~75% gets the user's attention (warning);
    // over ~95% adds an advisory note next to it (danger).
    const WARN_AT: usize = 150_000;
    const DANGER_AT: usize = 190_000;
    let (token_color, advisory) = if token_count >= DANGER_AT {
        (danger, Some("near context limit — consider starting a new chat"))
    } else if token_count >= WARN_AT {
        (warning, None)
    } else {
        (muted, None)
    };

    let mut bar = h_flex()
        .px_2()
        .py_1()
        .gap_2()
        .items_center()
        .border_b_1()
        .border_color(border);
    if sidebar_collapsed {
        bar = bar.child(
            Button::new("ai-chat-expand-sidebar")
                .icon(IconName::PanelLeftOpen)
                .xsmall()
                .ghost()
                .tooltip("Show sessions")
                .on_click(|_, _window, cx| {
                    let svc = cx.global::<AiChatServiceHandle>().0.clone();
                    svc.update(cx, |s, cx| s.toggle_sidebar(cx));
                }),
        );
    }
    bar.child(div().flex_1())
        .child(dropdown)
        .child(div().text_xs().text_color(token_color).child(token_label))
        .when(advisory.is_some(), |b| {
            b.child(
                div()
                    .text_xs()
                    .text_color(danger)
                    .child(SharedString::from(advisory.unwrap_or("").to_string())),
            )
        })
}

#[allow(clippy::too_many_arguments)]
fn render_messages(
    active: Option<&Session>,
    active_markdown: &[Option<Entity<TextViewState>>],
    scroll_handle: &ScrollHandle,
    quick_prompts_input: Option<Entity<InputState>>,
    fg: gpui::Hsla,
    muted: gpui::Hsla,
    bg_user: gpui::Hsla,
    fg_user: gpui::Hsla,
    bg_assistant: gpui::Hsla,
    danger: gpui::Hsla,
    accent: gpui::Hsla,
) -> impl IntoElement {
    // The scroll container IS the flex column directly hosting bubbles, so
    // `ScrollHandle::scroll_to_item(ix)` can target an individual bubble.
    // If we wrapped the bubbles in an intermediate v_flex the scroll
    // handle would only see a single child and `scroll_to_item` would be
    // useless for "scroll to last message".
    let scroll = v_flex()
        .id("ai-chat-messages")
        .track_scroll(scroll_handle)
        .flex_1()
        .min_h_0()
        .overflow_y_scroll()
        .p_3()
        .gap_3();
    let Some(s) = active else {
        return scroll.child(
            v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .child(
                    div()
                        .text_sm()
                        .text_color(muted)
                        .child("No session selected."),
                ),
        );
    };
    if s.messages.is_empty() {
        // Charting-mode empty state shows clickable quick prompts; other
        // modes get the plain placeholder. `quick_prompts_input` is wired
        // through from the top-level render — `None` when this isn't a
        // Charting session or no input is available.
        if let Some(input) = quick_prompts_input {
            // The reference block makes the empty state much taller than
            // the previous 4-prompt list, so we switch from
            // `justify_center` to top-anchored with padding — the outer
            // scroll container handles overflow if the panel is short.
            return scroll.child(
                v_flex()
                    .w_full()
                    .items_center()
                    .gap_3()
                    .pt_6()
                    .pb_4()
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .child("Try one of these:"),
                    )
                    .children(QUICK_PROMPTS.iter().enumerate().map(|(i, prompt)| {
                        render_quick_prompt(i, prompt, input.clone(), muted, accent)
                    }))
                    .child(render_tool_reference(fg, muted)),
            );
        }
        return scroll.child(
            v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .gap_1()
                .child(
                    div()
                        .text_xs()
                        .text_color(muted)
                        .child("Type a prompt below to start."),
                ),
        );
    }
    // Pre-build a `tool_use_id → ToolResultLite` map in ONE pass over the
    // message list. Replaces the prior `find_tool_result` (O(N) per chip,
    // walking forward each time) and removes the per-frame
    // `s.messages.clone()` that the search needed. JSON parsing of
    // `ToolResult.content` happens once here instead of once per chip per
    // frame. For a long session this was the dominant cost of the AI Chat
    // panel's render path — `find_tool_result` was O(N²) over messages
    // and `serde_json::from_str` was re-running each frame.
    let tool_results: std::collections::HashMap<&str, ToolResultLite> = s
        .messages
        .iter()
        .flat_map(|m| m.blocks.iter())
        .filter_map(|b| {
            if let ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } = b
            {
                let parsed: Option<serde_json::Value> = serde_json::from_str(content).ok();
                let drawing_id = parsed
                    .as_ref()
                    .and_then(|v| v.get("drawing_id"))
                    .and_then(|v| v.as_u64());
                let symbol = parsed
                    .as_ref()
                    .and_then(|v| v.get("symbol"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let noop = parsed
                    .as_ref()
                    .and_then(|v| v.get("noop"))
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let prior = parsed.as_ref().and_then(|v| v.get("prior")).cloned();
                Some((
                    tool_use_id.as_str(),
                    ToolResultLite {
                        is_error: *is_error,
                        noop,
                        drawing_id,
                        symbol,
                        prior,
                    },
                ))
            } else {
                None
            }
        })
        .collect();

    let rows = s
        .messages
        .iter()
        .enumerate()
        .filter(|(_, m)| !m.is_tool_result_only())
        .flat_map(move |(i, m)| {
            let (bg, color, align_end) = match m.role {
                Speaker::User => (bg_user, fg_user, true),
                Speaker::Assistant => (bg_assistant, fg, false),
            };
            let is_error = matches!(m.role, Speaker::Assistant) && m.error;

            // For assistant bubbles we render the cached TextViewState
            // (markdown), so the joined text only needs to exist for the
            // user-text path and the "is there any text?" check. Cheap
            // O(N) scan over blocks; no `String` allocation for the
            // assistant case where the bubble inner is the TextView.
            let has_text = m
                .blocks
                .iter()
                .any(|b| matches!(b, ContentBlock::Text { text } if !text.is_empty()));
            let mut elements: Vec<gpui::AnyElement> = Vec::new();
            if has_text {
                let bubble_inner: gpui::AnyElement = match m.role {
                    Speaker::Assistant => match active_markdown.get(i).and_then(|e| e.as_ref()) {
                        Some(state) => TextView::new(state).selectable(true).into_any_element(),
                        None => div()
                            .child(SharedString::from(m.text()))
                            .into_any_element(),
                    },
                    Speaker::User => div()
                        .child(SharedString::from(m.text()))
                        .into_any_element(),
                };
                let bubble = div()
                    .max_w(px(420.))
                    .px_3()
                    .py_2()
                    .rounded(px(8.))
                    .bg(bg)
                    .text_color(color)
                    .text_sm()
                    .when(is_error, |d| d.border_1().border_color(danger))
                    .child(bubble_inner);
                let row = h_flex()
                    .w_full()
                    .when(align_end, |this| this.justify_end())
                    .child(bubble);
                let final_row: gpui::AnyElement = if is_error {
                    v_flex()
                        .w_full()
                        .gap_1()
                        .child(row)
                        .child(
                            h_flex().w_full().child(
                                Button::new(SharedString::from(format!("ai-chat-retry-{i}")))
                                    .label("Retry")
                                    .xsmall()
                                    .ghost()
                                    .on_click(|_, _window, cx| {
                                        let svc = cx.global::<AiChatServiceHandle>().0.clone();
                                        svc.update(cx, |s, cx| s.retry_active(cx));
                                    }),
                            ),
                        )
                        .into_any_element()
                } else {
                    row.into_any_element()
                };
                elements.push(final_row);
            }

            // Tool chips: each gets its own row aligned to the bubble
            // side (assistant: left). Result lookup is now an O(1) hash
            // hit against the pre-built map.
            if matches!(m.role, Speaker::Assistant) {
                for (block_idx, block) in m.blocks.iter().enumerate() {
                    if let ContentBlock::ToolUse { id, name, input } = block {
                        let chip = render_tool_chip(
                            i,
                            block_idx,
                            id,
                            name,
                            input,
                            tool_results.get(id.as_str()),
                            fg,
                            muted,
                            accent,
                            danger,
                        );
                        let chip_row = h_flex().w_full().child(chip);
                        elements.push(chip_row.into_any_element());
                    }
                }
            }

            elements
        });
    scroll.children(rows)
}

/// Pre-parsed snapshot of a `ContentBlock::ToolResult`. Built once per
/// render in `render_messages` so the per-chip render doesn't reparse
/// JSON each frame. `prior` is the captured pre-mutation state that the
/// Undo button on `set_*` chips uses to dispatch the inverse action;
/// `noop` mutations skip rendering Undo entirely.
struct ToolResultLite {
    is_error: bool,
    noop: bool,
    drawing_id: Option<u64>,
    symbol: Option<String>,
    prior: Option<serde_json::Value>,
}

/// Reference block listing every tool the AI can call in Charting mode,
/// grouped by family. Read-only — purely informational. Driven entirely
/// off `TOOL_REFERENCE` so renaming a tool there propagates here without
/// any UI churn.
fn render_tool_reference(fg: gpui::Hsla, muted: gpui::Hsla) -> impl IntoElement {
    v_flex()
        .max_w(px(360.))
        .w_full()
        .mt_4()
        .gap_2()
        .child(
            div()
                .text_xs()
                .text_color(muted)
                .child("What the assistant can do:"),
        )
        .children(TOOL_REFERENCE.iter().map(|(family, tools)| {
            v_flex()
                .w_full()
                .gap_0p5()
                .child(
                    div()
                        .text_xs()
                        .text_color(muted)
                        .child(SharedString::from(format!("· {family}"))),
                )
                .children(tools.iter().map(|(icon, name, desc)| {
                    div()
                        .pl_3()
                        .text_xs()
                        .text_color(fg)
                        .child(SharedString::from(format!(
                            "{icon}  {name} — {desc}"
                        )))
                }))
        }))
}

/// One quick-prompt button. Clicking stages the prompt into the input bar
/// (no auto-send — design Q15b). The InputState pointer is cloned in once
/// per render; the click handler updates the input value directly.
fn render_quick_prompt(
    idx: usize,
    prompt: &'static str,
    input: Entity<InputState>,
    muted: gpui::Hsla,
    accent: gpui::Hsla,
) -> impl IntoElement {
    let row_id = SharedString::from(format!("ai-quick-prompt-{idx}"));
    let label = SharedString::from(prompt.to_string());
    let prompt_owned = prompt.to_string();
    div()
        .id(row_id)
        .max_w(px(360.))
        .px_3()
        .py_1p5()
        .rounded(px(6.))
        .border_1()
        .border_color(muted)
        .text_xs()
        .text_color(muted)
        .cursor_pointer()
        .hover(|st| st.bg(accent).text_color(gpui::white()))
        .child(label)
        .on_click(move |_, window, cx| {
            let value = SharedString::from(prompt_owned.clone());
            input.update(cx, |state, cx| {
                state.set_value(value, window, cx);
            });
            // InputState::set_value suppresses `InputEvent::Change`, so the
            // panel's mirror-into-service subscription doesn't run — the
            // service's `draft` stays empty and `send_active` no-ops. Push
            // the prompt into the active session's draft directly so the
            // send button (and Enter) have something to send.
            let svc = cx.global::<AiChatServiceHandle>().0.clone();
            let active_id = svc.read(cx).selected_id().map(|s| s.to_string());
            if let Some(id) = active_id {
                svc.update(cx, |s, _cx| s.set_draft(&id, prompt_owned.clone()));
            }
        })
}

// ---------------------------------------------------------------------------
// Per-block bubble children + tool chips
// ---------------------------------------------------------------------------

/// Render one tool_use chip. Looks up the matching ToolResult (which the
/// agentic loop always appends in the next user-role message) to decide
/// state: pending → no result yet; ok → green check + per-chip undo for
/// add_* tools; error → red label with the error text.
#[allow(clippy::too_many_arguments)]
fn render_tool_chip(
    msg_idx: usize,
    block_idx: usize,
    _tool_use_id: &str,
    name: &str,
    input: &serde_json::Value,
    result: Option<&ToolResultLite>,
    fg: gpui::Hsla,
    muted: gpui::Hsla,
    accent: gpui::Hsla,
    danger: gpui::Hsla,
) -> impl IntoElement {
    let is_error = result.map(|r| r.is_error).unwrap_or(false);
    let drawing_id = result.and_then(|r| r.drawing_id);
    let symbol: Option<String> = result.and_then(|r| r.symbol.clone());
    let summary = chip_summary(name, input);

    // Border + state glyph carry the run state (pending / ok / error);
    // the label itself uses full foreground so the action text is the
    // legible part of the chip. Previously the label rode on the same
    // tinted color as the border, which made it hard to read.
    let (border_color, state_color) = if result.is_none() {
        (muted, muted)
    } else if is_error {
        (danger, danger)
    } else {
        (accent, accent)
    };

    let icon = tool_icon(name);
    let state_glyph = match (result.is_some(), is_error) {
        (false, _) => "⏳",
        (true, false) => "✓",
        (true, true) => "⚠",
    };

    let chip_id = SharedString::from(format!("ai-chip-{msg_idx}-{block_idx}"));
    let mut chip = h_flex()
        .id(chip_id)
        .gap_1p5()
        .px_2p5()
        .py_1()
        .rounded(px(6.))
        .border_1()
        .border_color(border_color)
        .text_sm()
        .text_color(fg)
        // Icon gets bumped to `text_base` (vs the chip's `text_sm`) plus
        // semibold so the leading glyph anchors the chip and reads as
        // "this row is a tool call" at a glance.
        .child(
            div()
                .text_base()
                .font_semibold()
                .text_color(state_color)
                .child(SharedString::from(icon.to_string())),
        )
        .child(div().font_semibold().child(SharedString::from(summary)))
        .child(div().text_color(state_color).child(SharedString::from(state_glyph.to_string())));

    // Show undo button only for add_* tools that produced a drawing_id.
    if let (Some(id), Some(sym)) = (drawing_id, symbol.clone()) {
        if name.starts_with("add_") && !is_error {
            let btn_id = SharedString::from(format!("ai-chip-undo-{msg_idx}-{block_idx}"));
            chip = chip.child(
                Button::new(btn_id)
                    .icon(IconName::Close)
                    .xsmall()
                    .ghost()
                    .tooltip("Remove this AI drawing")
                    .on_click(move |_, _window, cx| {
                        let svc = cx.global::<DrawingServiceHandle>().0.clone();
                        let sym = sym.clone();
                        svc.update(cx, |s, cx| {
                            s.delete(sym.as_str(), id, cx);
                        });
                    }),
            );
        }
    }
    // Undo for chart mutations (set_symbol / set_timeframe / set_layout).
    // Only when the call wasn't a no-op, wasn't an error, and the dispatcher
    // wrote a `prior` snapshot.
    let mutation_prior = result
        .filter(|r| !r.is_error && !r.noop && name.starts_with("set_"))
        .and_then(|r| r.prior.clone());
    if let Some(prior) = mutation_prior {
        let btn_id = SharedString::from(format!("ai-chip-undo-mut-{msg_idx}-{block_idx}"));
        let tool_name = name.to_string();
        chip = chip.child(
            Button::new(btn_id)
                .icon(IconName::Undo2)
                .xsmall()
                .ghost()
                .tooltip("Undo this change")
                .on_click(move |_, window, cx| {
                    crate::ai_tools::undo_mutation(tool_name.as_str(), &prior, window, cx);
                }),
        );
    }
    chip
}

fn tool_icon(name: &str) -> &'static str {
    match name {
        "add_horizontal_ray" => "─",
        "add_text" => "✎",
        "add_line" => "╱",
        "add_arrow" => "➜",
        "add_rectangle" => "▭",
        "add_fibonacci" => "𝝫",
        "add_long_position" => "▲",
        "add_short_position" => "▼",
        "get_candles" => "📊",
        "set_symbol" => "🔁",
        "set_timeframe" => "⏱",
        "set_layout" => "▦",
        _ => "🛠",
    }
}

/// One-line summary of a tool call's args. The model has full context from
/// the actual JSON; this is purely for the human eye scanning the bubble.
fn chip_summary(name: &str, input: &serde_json::Value) -> String {
    match name {
        "add_horizontal_ray" => {
            let price = input.get("price").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let label = input
                .get("label")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim();
            if label.is_empty() {
                format!("Ray @ {price:.2}")
            } else {
                format!("Ray '{label}' @ {price:.2}")
            }
        }
        "add_text" => {
            let price = input.get("price").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let text = input
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim();
            format!("Text '{text}' @ {price:.2}")
        }
        "add_line" => two_point_summary("Line", input),
        "add_arrow" => two_point_summary("Arrow", input),
        "add_rectangle" => two_point_summary("Rect", input),
        "add_fibonacci" => two_point_summary("Fib", input),
        "add_long_position" => position_summary("Long", input),
        "add_short_position" => position_summary("Short", input),
        "get_candles" => {
            let n = input.get("count").and_then(|v| v.as_u64()).unwrap_or(50);
            let sym = input
                .get("symbol")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim();
            let tf = input
                .get("tf")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim();
            match (sym.is_empty(), tf.is_empty()) {
                (true, true) => format!("Read {n} candles"),
                (false, true) => format!("Read {n} candles of {sym}"),
                (true, false) => format!("Read {n} {tf} candles"),
                (false, false) => format!("Read {n} {tf} candles of {sym}"),
            }
        }
        "set_symbol" => {
            let target = input
                .get("target_symbol")
                .and_then(|v| v.as_str())
                .unwrap_or("focused");
            let sym = input.get("symbol").and_then(|v| v.as_str()).unwrap_or("?");
            format!("Set {target} → {sym}")
        }
        "set_timeframe" => {
            let target = input
                .get("target_symbol")
                .and_then(|v| v.as_str())
                .unwrap_or("focused");
            let tf = input.get("tf").and_then(|v| v.as_str()).unwrap_or("?");
            format!("Set {target} timeframe → {tf}")
        }
        "set_layout" => {
            let layout = input
                .get("layout")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            format!("Set layout → {layout}")
        }
        _ => name.to_string(),
    }
}

fn two_point_summary(kind: &str, input: &serde_json::Value) -> String {
    let a = input.get("a_price").and_then(|v| v.as_f64()).unwrap_or(0.0);
    let b = input.get("b_price").and_then(|v| v.as_f64()).unwrap_or(0.0);
    format!("{kind} {a:.2} → {b:.2}")
}

fn position_summary(kind: &str, input: &serde_json::Value) -> String {
    let entry = input.get("entry").and_then(|v| v.as_f64()).unwrap_or(0.0);
    let tp = input
        .get("take_profit")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    let sl = input
        .get("stop_loss")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    format!("{kind} {entry:.2}→{tp:.2} (SL {sl:.2})")
}

fn render_input(
    input: &Entity<InputState>,
    pending: bool,
    muted: gpui::Hsla,
    border: gpui::Hsla,
) -> impl IntoElement {
    let send_input = input.clone();
    let clear_input = input.clone();
    // Icon-only buttons get sized as a square (size_6 at Small), so a
    // border-radius of half that height (12px+) renders as a circle.
    let circle = ButtonRounded::Size(px(999.));
    let clear_button = Button::new("ai-chat-clear")
        .icon(IconName::Delete)
        .small()
        .ghost()
        .rounded(circle)
        .tooltip("Clear prompt")
        .on_click(move |_, window, cx| {
            clear_prompt(&clear_input, window, cx);
        });
    let mut bar = h_flex()
        .px_2()
        .py_1p5()
        .gap_2()
        .items_center()
        .border_t_1()
        .border_color(border)
        .child(div().flex_1().child(Input::new(input)))
        .child(clear_button);
    if pending {
        bar = bar.child(
            div()
                .text_xs()
                .text_color(muted)
                .child("Assistant is thinking…"),
        );
        // Stop cancels the in-flight stream but keeps the partial reply
        // (see `AiChatService::cancel_active`).
        bar.child(
            Button::new("ai-chat-stop")
                .label("Stop")
                .xsmall()
                .danger()
                .on_click(|_, _window, cx| {
                    let svc = cx.global::<AiChatServiceHandle>().0.clone();
                    svc.update(cx, |s, cx| s.cancel_active(cx));
                }),
        )
    } else {
        bar.child(
            Button::new("ai-chat-send")
                .icon(IconName::ArrowUp)
                .small()
                .primary()
                .rounded(circle)
                .tooltip("Send")
                .on_click(move |_, window, cx| {
                    send_active(&send_input, window, cx);
                }),
        )
    }
}

// ---------------------------------------------------------------------------
// Subscriptions
// ---------------------------------------------------------------------------

/// Wires the AI Chat panel to the `AiChatService` and to its own `InputState`:
///
/// * `AiChatEvent::SelectionChanged` → reload the input with the newly active
///   session's draft, update tracked `displayed_session_id`.
/// * `AiChatEvent::InputStaged(id)` → if the staged session is what we're
///   showing, mirror the new draft into the input (used by external `AskAi`).
/// * `AiChatEvent::SessionsChanged` / `MessageAppended` → repaint.
/// * `InputEvent::Change` → persist the input value as the current session's
///   draft (no service emit; that would loop).
/// * `InputEvent::PressEnter` (non-shift) → send.
///
/// Returns the initially-selected session id so the caller can seed
/// `ContentPanel::displayed_session_id`.
pub fn subscribe(
    input: &Entity<InputState>,
    scroll: &ScrollHandle,
    window: &mut Window,
    cx: &mut Context<ContentPanel>,
) -> Option<String> {
    let service = cx.global::<AiChatServiceHandle>().0.clone();
    let initial_id = service.read(cx).selected_id().map(|s| s.to_string());

    let scroll_handle = scroll.clone();
    cx.subscribe_in(
        &service,
        window,
        move |this, svc, ev: &AiChatEvent, window, cx| {
            match ev {
                AiChatEvent::SelectionChanged => {
                    let new_id = svc.read(cx).selected_id().map(|s| s.to_string());
                    if new_id != this.displayed_session_id {
                        let draft = new_id
                            .as_deref()
                            .and_then(|id| svc.read(cx).session(id))
                            .map(|s| s.draft.clone())
                            .unwrap_or_default();
                        if let Some(input) = this.chat_input.clone() {
                            input.update(cx, |state, cx| {
                                state.set_value(SharedString::from(draft), window, cx);
                            });
                        }
                        this.displayed_session_id = new_id.clone();
                        // Snap to bottom so the user sees the most recent
                        // turn of the freshly-selected thread.
                        if let Some(id) = new_id {
                            scroll_to_last(&scroll_handle, svc, &id, cx);
                        }
                    }
                }
                AiChatEvent::InputStaged(id) => {
                    if this.displayed_session_id.as_deref() == Some(id.as_str()) {
                        let draft = svc
                            .read(cx)
                            .session(id)
                            .map(|s| s.draft.clone())
                            .unwrap_or_default();
                        if let Some(input) = this.chat_input.clone() {
                            input.update(cx, |state, cx| {
                                state.set_value(SharedString::from(draft), window, cx);
                            });
                        }
                    }
                }
                AiChatEvent::MessageAppended(id) => {
                    // Scroll to the just-appended bubble on every turn —
                    // user prompt or assistant reply — regardless of where
                    // the user currently is in the conversation.
                    // `scroll_to_item` stores an active-item request that
                    // gpui processes during the next prepaint, after the
                    // new bubble's bounds are known — so this works
                    // immediately rather than lagging one message behind.
                    if this.displayed_session_id.as_deref() == Some(id.as_str()) {
                        scroll_to_last(&scroll_handle, svc, id, cx);
                    }
                }
                AiChatEvent::StreamingDelta(id) => {
                    // Push the newly-streamed tail into the trailing
                    // assistant message's TextViewState so markdown re-
                    // renders incrementally. Joined text from all Text
                    // blocks is the source of truth (matches the cache
                    // init in render).
                    let (last_idx, full_text) = {
                        let svc_read = svc.read(cx);
                        let Some(sess) = svc_read.session(id) else {
                            return;
                        };
                        let Some(idx) = sess
                            .messages
                            .iter()
                            .rposition(|m| matches!(m.role, Speaker::Assistant))
                        else {
                            return;
                        };
                        (idx, sess.messages[idx].text())
                    };
                    let key = (id.clone(), last_idx);
                    if let Some(entry) = this.ai_chat_markdown.get_mut(&key) {
                        if full_text.len() > entry.pushed_bytes {
                            let tail = full_text[entry.pushed_bytes..].to_string();
                            entry.state.update(cx, |state, cx| {
                                state.push_str(&tail, cx);
                            });
                            entry.pushed_bytes = full_text.len();
                        }
                    }
                    if this.displayed_session_id.as_deref() == Some(id.as_str()) {
                        scroll_to_last(&scroll_handle, svc, id, cx);
                    }
                }
                AiChatEvent::SessionReset(id) => {
                    // Retry or a tool_use round popped + reshaped messages;
                    // every cached markdown entry for this session is now
                    // stale. Drop them all and let render rebuild lazily
                    // from current message text.
                    this.ai_chat_markdown.retain(|(s, _), _| s != id);
                }
                AiChatEvent::SessionsChanged | AiChatEvent::ModelsLoaded => {}
            }
            cx.notify();
        },
    )
    .detach();

    // Mirror input bar edits into the active session's `draft`, and treat
    // Enter (no shift) as Send.
    cx.subscribe_in(
        input,
        window,
        |this, input_entity, ev: &InputEvent, window, cx| match ev {
            InputEvent::Change => {
                let Some(id) = this.displayed_session_id.clone() else {
                    return;
                };
                let value = input_entity.read(cx).value().to_string();
                let svc = cx.global::<AiChatServiceHandle>().0.clone();
                svc.update(cx, |s, _cx| s.set_draft(&id, value));
            }
            InputEvent::PressEnter { secondary } => {
                if *secondary {
                    return;
                }
                send_active(input_entity, window, cx);
            }
            _ => {}
        },
    )
    .detach();

    initial_id
}

/// Ask the messages-list scroll handle to scroll the last bubble into view.
/// `scroll_to_item` is processed during the next prepaint — by then the new
/// bubble's bounds are known, so this works on the message that *just*
/// landed (no lag-by-one). The bubbles must be direct children of the
/// scroll element for the index to mean anything; see `render_messages`.
fn scroll_to_last(
    scroll: &ScrollHandle,
    svc: &Entity<AiChatService>,
    session_id: &str,
    cx: &Context<ContentPanel>,
) {
    let count = svc
        .read(cx)
        .session(session_id)
        .map(|s| s.messages.len())
        .unwrap_or(0);
    if count > 0 {
        scroll.scroll_to_item(count - 1);
    }
}

/// Format a token count compactly: `42`, `1.2k`, `12k`. Lossy on purpose —
/// the sidebar row only has ~30px for it.
fn short_count(n: usize) -> String {
    if n < 1_000 {
        n.to_string()
    } else if n < 10_000 {
        format!("{:.1}k", n as f32 / 1_000.)
    } else {
        format!("{}k", n / 1_000)
    }
}

/// Send the active session's draft. Clears the input on success.
fn send_active(input: &Entity<InputState>, window: &mut Window, cx: &mut gpui::App) {
    let svc = cx.global::<AiChatServiceHandle>().0.clone();
    let sent = svc.update(cx, |s, cx| s.send_active(cx).is_some());
    if sent {
        input.update(cx, |state, cx| {
            state.set_value(SharedString::from(""), window, cx);
        });
    }
}

/// Clear the input bar. The `InputEvent::Change` subscription mirrors the
/// empty value into the active session's draft, so the cleared state also
/// persists across panel re-renders.
fn clear_prompt(input: &Entity<InputState>, window: &mut Window, cx: &mut gpui::App) {
    input.update(cx, |state, cx| {
        state.set_value(SharedString::from(""), window, cx);
    });
}
