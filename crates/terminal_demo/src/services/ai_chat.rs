//! AI chat service — owns sessions and the streaming Anthropic transport.
//!
//! The server proxies Anthropic's Messages API at `POST /v1/ai/chat` (see
//! `centoflow-server/AI_CHAT_DESIGN.md`). This module:
//!
//! * Holds the model allowlist fetched from `GET /v1/ai/models` (with a
//!   hardcoded Sonnet fallback when the fetch fails / hasn't completed yet).
//! * Persists sessions to localStorage / disk, migrating model ids from the
//!   old enum-kebab-case form to Anthropic ids on load AND migrating the
//!   pre-tool-use flat `text: String` schema into `blocks: Vec<ContentBlock>`.
//! * `send_active` POSTs the full conversation history, immediately appends
//!   an empty assistant bubble, then streams Anthropic SSE deltas into it.
//!   Updates are coalesced (~50ms) so paints stay bounded.
//! * Implements the agentic loop: when a stream ends with
//!   `stop_reason="tool_use"`, the queued tool_use blocks are executed via
//!   the registered [`ToolDispatch`] global, results are appended as a new
//!   user-role tool_result message, and the next iteration is spawned —
//!   up to [`MAX_TOOL_ITERATIONS`] times per user turn.

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;
use std::time::Duration;

use futures::StreamExt as _;
use gpui::{App, AppContext as _, Context, Entity, EventEmitter, Global, Task};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

use crate::net::{CentoflowConfig, HttpClient};
use crate::persistence;

// ---------------------------------------------------------------------------
// Anthropic model metadata (server-driven, with Sonnet fallback)
// ---------------------------------------------------------------------------

/// Display metadata for one Anthropic model. Fetched from `GET /v1/ai/models`
/// at startup. The `id` is the wire-stable model identifier; `label`/`short`
/// drive the dropdown and the per-session sidebar badge respectively.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnthropicModel {
    pub id: String,
    pub label: String,
    pub short: String,
}

/// Hardcoded Sonnet entry returned by `available_models()` until the server
/// fetch completes (or as the only entry if the fetch fails). Keeps the
/// chat panel functional even when `/v1/ai/models` is unreachable.
fn sonnet_fallback_slice() -> &'static [AnthropicModel] {
    static FALLBACK: OnceLock<Vec<AnthropicModel>> = OnceLock::new();
    FALLBACK.get_or_init(|| {
        vec![AnthropicModel {
            id: "claude-sonnet-4-6".into(),
            label: "Sonnet 4.6".into(),
            short: "Sonnet".into(),
        }]
    })
}

/// Wire-stable default model id used when the user has never picked one.
const DEFAULT_MODEL_ID: &str = "claude-sonnet-4-6";

/// Maximum number of `tool_use` round-trips per user turn. After this many
/// iterations the loop hard-stops with an error bubble — protects against
/// runaway tool-call loops and Anthropic quota burn. Sized for the expanded
/// tool surface (8 drawing + 3 mutation + get_candles) so chained analyses
/// like "compare AAPL and MSFT then mark a long entry" don't hit the cap.
const MAX_TOOL_ITERATIONS: u32 = 20;

/// Maps pre-server-driven persisted values (the old kebab-case enum tags)
/// onto current Anthropic model ids. New deployments hit the `other` arm.
fn migrate_model_id(stored: &str) -> String {
    match stored {
        "opus-47" => "claude-opus-4-7".to_string(),
        "sonnet-46" => "claude-sonnet-4-6".to_string(),
        "haiku-45" => "claude-haiku-4-5-20251001".to_string(),
        "" => DEFAULT_MODEL_ID.to_string(),
        other => other.to_string(),
    }
}

/// Best-effort short label when a stored model id isn't in the fetched
/// allowlist (e.g. server removed a model the user previously selected).
fn short_from_id(id: &str) -> String {
    for fam in ["opus", "sonnet", "haiku"] {
        if id.contains(fam) {
            let mut c = fam.chars();
            return c.next().unwrap().to_uppercase().chain(c).collect();
        }
    }
    "Model".to_string()
}

// ---------------------------------------------------------------------------
// Session schema — content is a list of typed blocks (Anthropic Messages API
// shape) so a single assistant turn can carry mixed text + tool_use.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Speaker {
    User,
    Assistant,
}

/// One content block within a `ChatMsg`. Mirrors Anthropic's wire shape:
/// `{ "type": "...", ... }` — `#[serde(tag = "type")]` makes the round-trip
/// to/from Anthropic JSON trivial (we send these straight through).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    /// Plain text — the bread and butter of every message.
    Text { text: String },
    /// Assistant-originated tool invocation. `id` is what the matching
    /// `ToolResult.tool_use_id` references on the next user turn.
    ToolUse {
        id: String,
        name: String,
        input: JsonValue,
    },
    /// User-originated tool result feeding back into the next assistant
    /// turn. Anthropic requires every prior `ToolUse.id` to have a matching
    /// `ToolResult.tool_use_id` in the conversation history.
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        is_error: bool,
    },
}

impl ContentBlock {
    pub fn text(s: impl Into<String>) -> Self {
        Self::Text { text: s.into() }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChatMsg {
    pub role: Speaker,
    /// Canonical content. Populated directly from new sessions; backfilled
    /// from legacy `text` during load (see `ChatMsg::normalize`).
    #[serde(default)]
    pub blocks: Vec<ContentBlock>,
    /// Legacy field — pre-tool_use sessions stored content as a flat
    /// string here. Read on load, drained into `blocks`, then cleared.
    /// Never written back (`skip_serializing_if`).
    #[serde(
        default,
        rename = "text",
        skip_serializing_if = "String::is_empty"
    )]
    legacy_text: String,
    /// True when this assistant message failed (network error, upstream
    /// error, cancellation, or tool-loop overflow). Renders with
    /// destructive styling + a Retry button. `#[serde(default)]` keeps
    /// old persisted sessions loadable.
    #[serde(default)]
    pub error: bool,
}

impl ChatMsg {
    /// Promote any pre-migration `legacy_text` into a single `Text` block.
    /// Idempotent. Called once per message during session load.
    fn normalize(&mut self) {
        if !self.legacy_text.is_empty() && self.blocks.is_empty() {
            self.blocks.push(ContentBlock::Text {
                text: std::mem::take(&mut self.legacy_text),
            });
        } else {
            self.legacy_text.clear();
        }
    }

    /// Joined plain text from every `Text` block. Used for title generation,
    /// token estimates, and any code path that doesn't care about tool
    /// blocks.
    pub fn text(&self) -> String {
        let mut out = String::new();
        for b in &self.blocks {
            if let ContentBlock::Text { text } = b {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(text);
            }
        }
        out
    }

    /// Append `delta` to the trailing `Text` block, creating one if the last
    /// block is non-text (or there are no blocks at all). This is the path
    /// every streamed `text_delta` takes.
    pub fn append_text(&mut self, delta: &str) {
        if let Some(ContentBlock::Text { text }) = self.blocks.last_mut() {
            text.push_str(delta);
        } else {
            self.blocks.push(ContentBlock::Text {
                text: delta.to_string(),
            });
        }
    }

    /// True iff this message has any actual content. An empty assistant
    /// placeholder counts as empty.
    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
            || self.blocks.iter().all(|b| match b {
                ContentBlock::Text { text } => text.is_empty(),
                _ => false,
            })
    }

    /// Tool-only assistant turns (no Text blocks) need a placeholder string
    /// so the renderer's plain-text path doesn't show a blank bubble.
    pub fn display_or_placeholder(&self) -> String {
        let t = self.text();
        if !t.is_empty() {
            return t;
        }
        if self
            .blocks
            .iter()
            .any(|b| matches!(b, ContentBlock::ToolUse { .. }))
        {
            String::from("(using tools…)")
        } else {
            String::new()
        }
    }

    /// True when every block is a `ToolResult`. These are user-role messages
    /// the agentic loop emits to feed tool outputs back to the model —
    /// conversation-protocol, not user content — so the renderer hides them.
    pub fn is_tool_result_only(&self) -> bool {
        !self.blocks.is_empty()
            && self
                .blocks
                .iter()
                .all(|b| matches!(b, ContentBlock::ToolResult { .. }))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub title: String,
    pub messages: Vec<ChatMsg>,
    /// Anthropic model id (e.g. `"claude-sonnet-4-6"`). Stored as a string —
    /// the model allowlist evolves on the server; a stale id stays in the
    /// session and the UI shows whatever short label we can derive.
    pub model: String,
    /// Last-known input bar contents — kept in memory so switching back to
    /// this session within the same app run restores the draft. Not
    /// persisted: a fresh app launch always starts with an empty input.
    #[serde(skip)]
    pub draft: String,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

impl Session {
    fn new(id: String, model: String, now_ms: i64) -> Self {
        Self {
            id,
            title: String::new(),
            messages: Vec::new(),
            model,
            draft: String::new(),
            created_at_ms: now_ms,
            updated_at_ms: now_ms,
        }
    }

    pub fn display_title(&self) -> &str {
        if self.title.is_empty() {
            "New chat"
        } else {
            &self.title
        }
    }

    pub fn is_untitled(&self) -> bool {
        self.title.is_empty()
    }

    pub fn token_estimate(&self) -> usize {
        self.messages.iter().map(|m| m.text().len()).sum::<usize>() / 4
    }
}

// ---------------------------------------------------------------------------
// Client-context payload — the slice of UI state we tell the server about
// every turn (server stitches it into the system prompt). Demo scope: list
// of open charts.
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct ClientContext {
    pub charts: Vec<ChartContext>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChartContext {
    pub symbol: String,
    pub tf: String,
    pub last_close: f64,
    pub focused: bool,
    /// Recent OHLCV history for the focused chart only. Non-focused charts
    /// keep the lightweight summary above so we don't blow the token budget
    /// with N×200-bar arrays. `None` (or empty) → AI calls `get_candles`
    /// to populate.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub candles: Vec<CandleContext>,
}

/// Compact per-candle entry sent in the per-turn context. Both `t` (epoch
/// ms, canonical for tool inputs) and `ts` (ISO 8601 UTC, ergonomic for the
/// model) are present — the AI echoes `ts` directly into drawing-tool time
/// fields and uses `t` for arithmetic / comparisons.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CandleContext {
    pub t: i64,
    pub ts: String,
    pub o: f64,
    pub h: f64,
    pub l: f64,
    pub c: f64,
    pub v: f64,
}

/// Source of `ClientContext` + `enabled_tools` at request time. Workspace
/// implements this and stores itself as a Global so the AI chat service can
/// pull fresh context per iteration without holding cross-module references.
pub trait ChatContextProvider: 'static {
    fn enabled_tools(&self, cx: &App) -> Vec<String>;
    fn client_context(&self, cx: &App) -> Option<ClientContext>;
}

pub struct ChatContextProviderHandle(pub std::rc::Rc<dyn ChatContextProvider>);
impl Global for ChatContextProviderHandle {}

// ---------------------------------------------------------------------------
// Tool dispatch — slice 2 ships an always-error stub so the agentic loop is
// exercisable end-to-end. Slice 3 replaces this with a real executor that
// reaches into DrawingService / MarketDataService.
// ---------------------------------------------------------------------------

/// Result of executing one `ToolUse` block. `is_error: true` becomes
/// Anthropic `tool_result.is_error` — the model sees it and can recover.
pub struct ToolOutcome {
    pub content: String,
    pub is_error: bool,
}

/// Trait the workspace implements to execute the AI's tool calls. The
/// executor returns a `Task<ToolOutcome>` so individual tools can be async
/// (off-chart `get_candles` hits REST) without blocking the loop. Sync tools
/// wrap their result via `Task::ready`. The contract: every input block
/// produces exactly one outcome, awaited in the same order.
///
/// Implementors get an `AsyncApp` so they can interleave `cx.update(...)`
/// blocks for sync App reads/writes with `.await` calls to background work.
pub trait ToolDispatch: 'static {
    fn execute(
        &self,
        name: String,
        input: JsonValue,
        cx: gpui::AsyncApp,
    ) -> Task<ToolOutcome>;
}

pub struct ToolDispatchHandle(pub std::rc::Rc<dyn ToolDispatch>);
impl Global for ToolDispatchHandle {}

/// Stub used when no `ToolDispatchHandle` is registered (early-startup or
/// tests). Always returns an error so the model abandons the tool path
/// instead of looping until the iteration cap fires.
struct StubDispatch;
impl ToolDispatch for StubDispatch {
    fn execute(
        &self,
        name: String,
        _input: JsonValue,
        _cx: gpui::AsyncApp,
    ) -> Task<ToolOutcome> {
        Task::ready(ToolOutcome {
            content: format!("tool '{name}' is not available in this build"),
            is_error: true,
        })
    }
}

fn dispatch_tool(
    name: String,
    input: JsonValue,
    cx: &mut gpui::AsyncApp,
) -> Task<ToolOutcome> {
    // `AsyncApp::update` returns the closure's value directly (it asserts
    // app-still-alive internally). A missing dispatcher falls back to the
    // stub so the model gets a coherent is_error rather than nothing.
    let dispatcher: std::rc::Rc<dyn ToolDispatch> = cx.update(|cx| {
        cx.try_global::<ToolDispatchHandle>()
            .map(|h| h.0.clone())
            .unwrap_or_else(|| std::rc::Rc::new(StubDispatch))
    });
    dispatcher.execute(name, input, cx.clone())
}

// ---------------------------------------------------------------------------
// Service
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub enum AiChatEvent {
    SessionsChanged,
    SelectionChanged,
    InputStaged(String),
    MessageAppended(String),
    /// A streaming assistant message grew — panel uses this to scroll to
    /// bottom on the active session as text arrives.
    StreamingDelta(String),
    /// The fetched model allowlist changed (initial load completed).
    ModelsLoaded,
    /// The session's message list was rewritten in a way that invalidates
    /// per-message render state cached by the panel (e.g. Retry popped the
    /// failed assistant message, or the agentic loop appended a tool_result
    /// turn + new placeholder). Panel should drop all of its markdown
    /// entries for this session id and rebuild lazily on next render.
    SessionReset(String),
}

pub struct AiChatService {
    sessions: Vec<Session>,
    selected_id: Option<String>,
    /// Anthropic model id used when minting a new session.
    last_used_model: String,
    sidebar_collapsed: bool,
    next_id_seq: u64,
    pending: HashSet<String>,
    /// In-flight streaming tasks, keyed by session id. Dropping a Task
    /// cancels its future (which in turn drops the reqwest response stream
    /// — TCP close → server-side r.Context() cancels → Anthropic stops
    /// generating). `cancel_active` is the only mutator besides the task
    /// completing naturally.
    pending_tasks: HashMap<String, Task<()>>,
    /// Server-driven allowlist (`GET /v1/ai/models`). Empty until the first
    /// successful fetch — callers should go through `available_models()` to
    /// get the Sonnet fallback in that case.
    available_models: Vec<AnthropicModel>,
    _models_task: Option<Task<()>>,
}

impl EventEmitter<AiChatEvent> for AiChatService {}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PersistedState {
    #[serde(default)]
    sessions: Vec<Session>,
    #[serde(default)]
    selected_id: Option<String>,
    /// Stored as a string for forward-compat with the server-driven model
    /// list. Old persisted values (enum kebab-case like `"sonnet-46"`) are
    /// migrated to canonical Anthropic ids at load time.
    #[serde(default)]
    last_used_model: String,
    #[serde(default = "default_sidebar_collapsed")]
    sidebar_collapsed: bool,
}

fn default_sidebar_collapsed() -> bool {
    true
}

impl Default for PersistedState {
    fn default() -> Self {
        Self {
            sessions: Vec::new(),
            selected_id: None,
            last_used_model: DEFAULT_MODEL_ID.to_string(),
            sidebar_collapsed: default_sidebar_collapsed(),
        }
    }
}

impl AiChatService {
    fn new(cx: &mut Context<Self>) -> Self {
        let mut persisted: PersistedState = persistence::load_ai_chat().unwrap_or_default();
        // Migrate model ids on every load — cheap, idempotent.
        persisted.last_used_model = migrate_model_id(&persisted.last_used_model);
        for s in &mut persisted.sessions {
            s.model = migrate_model_id(&s.model);
            // Promote legacy flat-string content into a single Text block.
            // Idempotent — newly-persisted sessions store `blocks` directly
            // and skip the rewrite.
            for m in &mut s.messages {
                m.normalize();
            }
        }

        let client = cx.global::<HttpClient>().0.clone();
        let models_task = cx.spawn(async move |this, cx| {
            run_fetch_models(this, cx, client).await;
        });

        let mut svc = Self {
            sessions: persisted.sessions,
            selected_id: persisted.selected_id,
            last_used_model: persisted.last_used_model,
            sidebar_collapsed: persisted.sidebar_collapsed,
            next_id_seq: 0,
            pending: HashSet::new(),
            pending_tasks: HashMap::new(),
            available_models: Vec::new(),
            _models_task: Some(models_task),
        };
        if svc.sessions.is_empty() {
            let id = svc.mint_id();
            let now = now_ms();
            svc.sessions
                .push(Session::new(id.clone(), svc.last_used_model.clone(), now));
            svc.selected_id = Some(id);
            svc.persist();
        } else if svc.selected_id.is_none()
            || !svc
                .sessions
                .iter()
                .any(|s| Some(&s.id) == svc.selected_id.as_ref())
        {
            svc.selected_id = svc
                .sessions
                .iter()
                .max_by_key(|s| s.updated_at_ms)
                .map(|s| s.id.clone());
        }
        svc
    }

    pub fn sessions(&self) -> &[Session] {
        &self.sessions
    }

    pub fn sessions_sorted(&self) -> Vec<&Session> {
        let mut v: Vec<&Session> = self.sessions.iter().collect();
        v.sort_by(|a, b| b.updated_at_ms.cmp(&a.updated_at_ms));
        v
    }

    pub fn selected_id(&self) -> Option<&str> {
        self.selected_id.as_deref()
    }

    pub fn selected(&self) -> Option<&Session> {
        let id = self.selected_id.as_ref()?;
        self.sessions.iter().find(|s| &s.id == id)
    }

    pub fn session(&self, id: &str) -> Option<&Session> {
        self.sessions.iter().find(|s| s.id == id)
    }

    pub fn last_used_model(&self) -> &str {
        &self.last_used_model
    }

    pub fn sidebar_collapsed(&self) -> bool {
        self.sidebar_collapsed
    }

    pub fn is_pending(&self, session_id: &str) -> bool {
        self.pending.contains(session_id)
    }

    /// Server-driven model list, or the Sonnet fallback when the fetch
    /// hasn't completed (or failed). Never empty.
    pub fn available_models(&self) -> &[AnthropicModel] {
        if self.available_models.is_empty() {
            sonnet_fallback_slice()
        } else {
            &self.available_models
        }
    }

    /// Display metadata for an arbitrary model id. Returns a derived
    /// best-effort label when the id isn't (or isn't yet) in the fetched
    /// allowlist — so legacy sessions still render meaningfully.
    pub fn model_meta(&self, id: &str) -> AnthropicModel {
        self.available_models()
            .iter()
            .find(|m| m.id == id)
            .cloned()
            .unwrap_or_else(|| AnthropicModel {
                id: id.to_string(),
                label: id.to_string(),
                short: short_from_id(id),
            })
    }

    pub fn toggle_sidebar(&mut self, cx: &mut Context<Self>) {
        self.sidebar_collapsed = !self.sidebar_collapsed;
        self.persist();
        cx.emit(AiChatEvent::SessionsChanged);
        cx.notify();
    }

    pub fn set_sidebar_collapsed(&mut self, collapsed: bool, cx: &mut Context<Self>) {
        if self.sidebar_collapsed == collapsed {
            return;
        }
        self.sidebar_collapsed = collapsed;
        self.persist();
        cx.emit(AiChatEvent::SessionsChanged);
        cx.notify();
    }

    pub fn create_session(&mut self, cx: &mut Context<Self>) -> String {
        let id = self.mint_id();
        let now = now_ms();
        self.sessions
            .push(Session::new(id.clone(), self.last_used_model.clone(), now));
        self.selected_id = Some(id.clone());
        self.persist();
        cx.emit(AiChatEvent::SessionsChanged);
        cx.emit(AiChatEvent::SelectionChanged);
        cx.notify();
        id
    }

    pub fn select(&mut self, id: &str, cx: &mut Context<Self>) {
        if self.selected_id.as_deref() == Some(id) {
            return;
        }
        if !self.sessions.iter().any(|s| s.id == id) {
            return;
        }
        self.selected_id = Some(id.to_string());
        self.persist();
        cx.emit(AiChatEvent::SelectionChanged);
        cx.notify();
    }

    pub fn delete(&mut self, id: &str, cx: &mut Context<Self>) {
        let before = self.sessions.len();
        self.sessions.retain(|s| s.id != id);
        if self.sessions.len() == before {
            return;
        }
        if self.selected_id.as_deref() == Some(id) {
            self.selected_id = self
                .sessions
                .iter()
                .max_by_key(|s| s.updated_at_ms)
                .map(|s| s.id.clone());
        }
        if self.sessions.is_empty() {
            let new_id = self.mint_id();
            let now = now_ms();
            self.sessions
                .push(Session::new(new_id.clone(), self.last_used_model.clone(), now));
            self.selected_id = Some(new_id);
        }
        self.persist();
        cx.emit(AiChatEvent::SessionsChanged);
        cx.emit(AiChatEvent::SelectionChanged);
        cx.notify();
    }

    pub fn set_model(&mut self, id: &str, model_id: &str, cx: &mut Context<Self>) {
        let Some(s) = self.sessions.iter_mut().find(|s| s.id == id) else {
            return;
        };
        if s.model == model_id {
            return;
        }
        s.model = model_id.to_string();
        self.last_used_model = model_id.to_string();
        self.persist();
        cx.emit(AiChatEvent::SessionsChanged);
        cx.notify();
    }

    pub fn set_draft(&mut self, id: &str, draft: String) {
        if let Some(s) = self.sessions.iter_mut().find(|s| s.id == id) {
            if s.draft == draft {
                return;
            }
            s.draft = draft;
            self.persist();
        }
    }

    pub fn stage_input(&mut self, id: &str, prompt: &str, cx: &mut Context<Self>) {
        let id_owned = {
            let Some(s) = self.sessions.iter_mut().find(|s| s.id == id) else {
                return;
            };
            if s.draft.is_empty() {
                s.draft = prompt.to_string();
            } else {
                s.draft.push('\n');
                s.draft.push_str(prompt);
            }
            s.id.clone()
        };
        self.persist();
        cx.emit(AiChatEvent::InputStaged(id_owned));
        cx.notify();
    }

    /// Append the user's draft to the active session, push an immediate
    /// empty assistant bubble, and spawn the streaming task that fills it
    /// in from `POST /v1/ai/chat` (with the agentic loop for tool_use).
    ///
    /// Returns the prompt that was sent (so the panel can clear its input),
    /// or `None` if the draft was empty / the previous turn is still in
    /// flight.
    pub fn send_active(&mut self, cx: &mut Context<Self>) -> Option<String> {
        let id = self.selected_id.clone()?;
        if self.pending.contains(&id) {
            return None;
        }
        let prompt = {
            let s = self.sessions.iter_mut().find(|s| s.id == id)?;
            let draft = std::mem::take(&mut s.draft);
            let trimmed = draft.trim();
            if trimmed.is_empty() {
                return None;
            }
            let prompt = trimmed.to_string();
            s.messages.push(ChatMsg {
                role: Speaker::User,
                blocks: vec![ContentBlock::text(prompt.clone())],
                legacy_text: String::new(),
                error: false,
            });
            // Push the empty assistant placeholder — every iteration of the
            // agentic loop fills the trailing placeholder, so seeding it up
            // front means stream code never has to think about whether the
            // placeholder exists yet.
            s.messages.push(ChatMsg {
                role: Speaker::Assistant,
                blocks: Vec::new(),
                legacy_text: String::new(),
                error: false,
            });
            if s.title.is_empty() {
                s.title = truncate_title(&prompt);
            }
            s.updated_at_ms = now_ms();
            prompt
        };
        self.start_stream(&id, cx);
        Some(prompt)
    }

    /// Cancel the streaming reply for the active session, keeping whatever
    /// partial text has already arrived. The Stop button in the panel input
    /// bar dispatches this. Dropping the `Task` cancels the future, which
    /// closes the reqwest stream, which closes the TCP connection to the
    /// server, which cancels the upstream Anthropic request via
    /// `r.Context()` propagation.
    pub fn cancel_active(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.selected_id.clone() else {
            return;
        };
        let cancelled = self.pending_tasks.remove(&id);
        if cancelled.is_none() && !self.pending.contains(&id) {
            return;
        }
        drop(cancelled);
        // Mark the trailing assistant message as a cancellation — same
        // styling as an error, with explanatory body text. Keeps the
        // partial response visible (decision 8).
        if let Some(s) = self.sessions.iter_mut().find(|s| s.id == id) {
            if let Some(last) = s
                .messages
                .iter_mut()
                .rev()
                .find(|m| matches!(m.role, Speaker::Assistant))
            {
                last.append_text("\n\n[cancelled]");
                last.error = true;
            }
        }
        self.pending.remove(&id);
        self.persist();
        cx.emit(AiChatEvent::MessageAppended(id.clone()));
        cx.emit(AiChatEvent::SessionsChanged);
        cx.notify();
    }

    /// Pop the trailing failed assistant message and re-run the streaming
    /// pipeline against the remaining history. Used by the Retry button on
    /// an error bubble. No-op if the trailing assistant isn't an error.
    pub fn retry_active(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.selected_id.clone() else {
            return;
        };
        if self.pending.contains(&id) {
            return;
        }
        {
            let Some(s) = self.sessions.iter_mut().find(|s| s.id == id) else {
                return;
            };
            // Require: trailing message is a failed assistant. Otherwise
            // there's nothing to retry.
            match s.messages.last() {
                Some(m) if matches!(m.role, Speaker::Assistant) && m.error => {}
                _ => return,
            }
            s.messages.pop();
            // Re-seed the empty assistant placeholder for the next stream.
            s.messages.push(ChatMsg {
                role: Speaker::Assistant,
                blocks: Vec::new(),
                legacy_text: String::new(),
                error: false,
            });
            s.updated_at_ms = now_ms();
        }
        // Tell the panel to drop any cached markdown entries for this
        // session — message indices and per-block keys are stale after the
        // pop+push.
        cx.emit(AiChatEvent::SessionReset(id.clone()));
        self.start_stream(&id, cx);
    }

    /// Spawn the agentic loop task. Each iteration sends the current
    /// history (including any tool_results appended by prior iterations),
    /// streams the response, and either ends (`stop_reason=end_turn`) or
    /// executes the emitted tool_use blocks and continues.
    fn start_stream(&mut self, id: &str, cx: &mut Context<Self>) {
        self.pending.insert(id.to_string());
        self.persist();
        cx.emit(AiChatEvent::MessageAppended(id.to_string()));
        cx.emit(AiChatEvent::SessionsChanged);
        cx.notify();

        let client = cx.global::<HttpClient>().0.clone();
        let cfg = cx.global::<CentoflowConfig>().clone();
        let session_id = id.to_string();
        let task = cx.spawn(async move |this, cx| {
            run_loop(this, cx, client, cfg, session_id).await;
        });
        self.pending_tasks.insert(id.to_string(), task);
    }

    /// Append `text` to the trailing assistant message of `session_id`.
    /// Emits `StreamingDelta` so the panel can scroll to bottom; **does
    /// not** persist (per design decision 12: persist on terminal events
    /// only — `finish_stream` / `fail_stream`).
    fn append_delta(&mut self, session_id: &str, text: &str, cx: &mut Context<Self>) {
        if text.is_empty() {
            return;
        }
        if let Some(s) = self.sessions.iter_mut().find(|s| s.id == session_id) {
            if let Some(last) = s
                .messages
                .iter_mut()
                .rev()
                .find(|m| matches!(m.role, Speaker::Assistant))
            {
                last.append_text(text);
            }
            s.updated_at_ms = now_ms();
        }
        cx.emit(AiChatEvent::StreamingDelta(session_id.to_string()));
        cx.notify();
    }

    /// Attach a finalized `ToolUse` block to the trailing assistant
    /// message. Emits `StreamingDelta` so the chip becomes visible
    /// immediately when streaming surfaces a new tool call.
    fn append_tool_use(
        &mut self,
        session_id: &str,
        id: String,
        name: String,
        input: JsonValue,
        cx: &mut Context<Self>,
    ) {
        if let Some(s) = self.sessions.iter_mut().find(|s| s.id == session_id) {
            if let Some(last) = s
                .messages
                .iter_mut()
                .rev()
                .find(|m| matches!(m.role, Speaker::Assistant))
            {
                last.blocks
                    .push(ContentBlock::ToolUse { id, name, input });
            }
            s.updated_at_ms = now_ms();
        }
        cx.emit(AiChatEvent::StreamingDelta(session_id.to_string()));
        cx.notify();
    }

    /// Collect every `ToolUse` block on the trailing assistant message of
    /// `session_id`, returning `(tool_use_id, name, input)` triples in
    /// emission order. Run between `stream_one` ending in `tool_use` and the
    /// async dispatch loop so dispatchers can interleave `.await` without
    /// holding the entity borrow.
    fn extract_pending_tool_uses(
        &self,
        session_id: &str,
    ) -> Vec<(String, String, JsonValue)> {
        let Some(s) = self.sessions.iter().find(|s| s.id == session_id) else {
            return Vec::new();
        };
        let Some(assistant) = s
            .messages
            .iter()
            .rev()
            .find(|m| matches!(m.role, Speaker::Assistant))
        else {
            return Vec::new();
        };
        assistant
            .blocks
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolUse { id, name, input } => {
                    Some((id.clone(), name.clone(), input.clone()))
                }
                _ => None,
            })
            .collect()
    }

    /// Splice already-resolved tool outcomes onto the conversation: append a
    /// user-role message carrying matching `ToolResult` blocks, then a fresh
    /// empty assistant placeholder so the next agentic-loop iteration has
    /// somewhere to stream into. Emits `SessionReset` so the panel drops
    /// per-message render state cached against now-stale indices.
    fn apply_tool_outcomes(
        &mut self,
        session_id: &str,
        outcomes: Vec<(String, ToolOutcome)>,
        cx: &mut Context<Self>,
    ) {
        if outcomes.is_empty() {
            return;
        }
        let results: Vec<ContentBlock> = outcomes
            .into_iter()
            .map(|(tool_use_id, outcome)| ContentBlock::ToolResult {
                tool_use_id,
                content: outcome.content,
                is_error: outcome.is_error,
            })
            .collect();
        if let Some(s) = self.sessions.iter_mut().find(|s| s.id == session_id) {
            s.messages.push(ChatMsg {
                role: Speaker::User,
                blocks: results,
                legacy_text: String::new(),
                error: false,
            });
            s.messages.push(ChatMsg {
                role: Speaker::Assistant,
                blocks: Vec::new(),
                legacy_text: String::new(),
                error: false,
            });
            s.updated_at_ms = now_ms();
        }
        cx.emit(AiChatEvent::SessionReset(session_id.to_string()));
        cx.emit(AiChatEvent::SessionsChanged);
        cx.notify();
    }

    /// Mark the streaming reply for `session_id` as done. Releases `pending`
    /// and persists the final state once.
    fn finish_stream(&mut self, session_id: &str, cx: &mut Context<Self>) {
        self.pending_tasks.remove(session_id);
        if !self.pending.remove(session_id) {
            return;
        }
        self.persist();
        cx.emit(AiChatEvent::MessageAppended(session_id.to_string()));
        cx.emit(AiChatEvent::SessionsChanged);
        cx.notify();
    }

    /// Mark the trailing assistant message as a failure. Sets `error =
    /// true` and appends a brief explanation text block, then transitions
    /// to finished state. The panel renders error bubbles with destructive
    /// styling + a Retry button (see `retry_active`).
    ///
    /// 401 escalates beyond the chat panel: the JWT is no longer valid, so
    /// we clear it (mirroring the candles/stream failure path). Other
    /// services will reconnect with an empty token and surface auth state
    /// to the user.
    fn fail_stream(
        &mut self,
        session_id: &str,
        status: Option<u16>,
        err: &str,
        cx: &mut Context<Self>,
    ) {
        if let Some(s) = self.sessions.iter_mut().find(|s| s.id == session_id) {
            if let Some(last) = s
                .messages
                .iter_mut()
                .rev()
                .find(|m| matches!(m.role, Speaker::Assistant))
            {
                if last.is_empty() {
                    last.append_text(err);
                } else {
                    last.append_text("\n\n");
                    last.append_text(err);
                }
                last.error = true;
            }
        }
        if matches!(status, Some(401)) {
            crate::auth::logout(cx);
        }
        self.finish_stream(session_id, cx);
    }

    fn set_available_models(&mut self, models: Vec<AnthropicModel>, cx: &mut Context<Self>) {
        if self.available_models == models {
            return;
        }
        self.available_models = models;
        cx.emit(AiChatEvent::ModelsLoaded);
        cx.notify();
    }

    fn mint_id(&mut self) -> String {
        self.next_id_seq += 1;
        format!("s{}-{}", now_ms(), self.next_id_seq)
    }

    fn persist(&self) {
        let state = PersistedState {
            sessions: self.sessions.clone(),
            selected_id: self.selected_id.clone(),
            last_used_model: self.last_used_model.clone(),
            sidebar_collapsed: self.sidebar_collapsed,
        };
        if let Err(err) = persistence::save_ai_chat(&state) {
            log::warn!("save ai_chat failed: {err:?}");
        }
    }
}

#[derive(Clone)]
pub struct AiChatServiceHandle(pub Entity<AiChatService>);
impl Global for AiChatServiceHandle {}

pub fn init(cx: &mut App) {
    let entity = cx.new(AiChatService::new);
    cx.set_global(AiChatServiceHandle(entity));
}

// ---------------------------------------------------------------------------
// Wire types — what we send to the server's POST /v1/ai/chat
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct WireReq<'a> {
    model: &'a str,
    messages: Vec<WireMsg>,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    enabled_tools: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    client_context: Option<ClientContext>,
}

#[derive(Serialize)]
struct WireMsg {
    role: &'static str,
    /// Always block-form. Anthropic accepts either string or block array;
    /// we use the array universally so tool_use / tool_result turns don't
    /// need a different code path.
    content: Vec<ContentBlock>,
}

/// Default `max_tokens`; matches the server's silent-clamp cap so we don't
/// rely on undocumented defaults.
const MAX_OUTPUT_TOKENS: u32 = 4096;

// ---------------------------------------------------------------------------
// SSE parsing — Anthropic's stream comes through the proxy verbatim
// ---------------------------------------------------------------------------

#[derive(Deserialize, Default)]
struct SsePayload {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    index: usize,
    #[serde(default)]
    content_block: Option<SseContentBlock>,
    #[serde(default)]
    delta: Option<SseDelta>,
}

#[derive(Deserialize, Default)]
struct SseContentBlock {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    id: String,
    #[serde(default)]
    name: String,
}

#[derive(Deserialize, Default)]
struct SseDelta {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    text: String,
    #[serde(default)]
    partial_json: String,
    /// Present on `message_delta` events. `"tool_use"` triggers the next
    /// agentic-loop iteration; `"end_turn"` ends the loop.
    #[serde(default)]
    stop_reason: String,
}

/// Pop one complete SSE event (delimited by `\n\n`) from `buf`, returning the
/// concatenated `data:` lines or `None` if no complete event is buffered yet.
/// Empty `Some(String)` is possible (event/comment-only line) — callers
/// should treat it as "skip and keep popping."
fn pop_sse_event(buf: &mut Vec<u8>) -> Option<String> {
    let pos = buf.windows(2).position(|w| w == b"\n\n")?;
    let raw: Vec<u8> = buf.drain(..pos + 2).collect();
    let mut data = String::new();
    for line in raw.split(|&b| b == b'\n') {
        if let Some(rest) = line.strip_prefix(b"data: ") {
            if let Ok(s) = std::str::from_utf8(rest) {
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(s);
            }
        }
    }
    Some(data)
}

// ---------------------------------------------------------------------------
// Background tasks
// ---------------------------------------------------------------------------

async fn run_fetch_models(
    this: gpui::WeakEntity<AiChatService>,
    cx: &mut gpui::AsyncApp,
    client: reqwest::Client,
) {
    let mut attempts: u32 = 0;
    loop {
        let Ok(cfg) = this.update(cx, |_s, cx| cx.global::<CentoflowConfig>().clone()) else {
            return;
        };
        match fetch_models(&client, &cfg).await {
            Ok(models) if !models.is_empty() => {
                let _ = this.update(cx, |s, cx| s.set_available_models(models, cx));
                return;
            }
            Ok(_) => {
                log::warn!("/v1/ai/models returned an empty list; staying on Sonnet fallback");
                return;
            }
            Err(e) => {
                log::warn!("/v1/ai/models fetch: {e:#}");
                attempts = attempts.saturating_add(1);
                let shift = attempts.saturating_sub(1).min(5);
                let secs = (1u64 << shift).min(30);
                cx.background_executor()
                    .timer(Duration::from_secs(secs))
                    .await;
            }
        }
    }
}

async fn fetch_models(
    client: &reqwest::Client,
    cfg: &CentoflowConfig,
) -> anyhow::Result<Vec<AnthropicModel>> {
    #[derive(Deserialize)]
    struct Resp {
        #[serde(default)]
        models: Vec<AnthropicModel>,
    }
    let url = format!("{}/v1/ai/models", cfg.base_url);
    let mut req = client.get(&url);
    if let Some(token) = &cfg.token {
        req = req.bearer_auth(token);
    }
    let resp = req.send().await?;
    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!("/v1/ai/models returned HTTP {status}");
    }
    Ok(resp.json::<Resp>().await?.models)
}

/// Build the wire message list from the service's session history.
///
/// Anthropic requires: every assistant message containing a `tool_use` must
/// be immediately followed by a user message containing matching
/// `tool_result` blocks. Our agentic loop already enforces that structure;
/// this function just maps each `ChatMsg` straight to a `WireMsg`.
///
/// The trailing empty assistant placeholder is dropped — Anthropic doesn't
/// want to see the bubble we're about to stream *into*.
fn build_wire_messages(session: &Session) -> Vec<WireMsg> {
    let mut out: Vec<WireMsg> = Vec::with_capacity(session.messages.len());
    for (i, m) in session.messages.iter().enumerate() {
        // Skip the trailing empty assistant placeholder.
        if i + 1 == session.messages.len()
            && matches!(m.role, Speaker::Assistant)
            && m.blocks.is_empty()
        {
            continue;
        }
        // Defensive: an empty non-placeholder message would crash Anthropic
        // with "content blocks must be non-empty"; substitute a single
        // empty text block so the request stays well-formed.
        let content = if m.blocks.is_empty() {
            vec![ContentBlock::text(String::new())]
        } else {
            m.blocks.clone()
        };
        out.push(WireMsg {
            role: match m.role {
                Speaker::User => "user",
                Speaker::Assistant => "assistant",
            },
            content,
        });
    }
    out
}

/// The agentic loop. Each iteration:
///   1. Snapshot the current session + per-turn context (enabled_tools +
///      client_context) under one `this.update` call.
///   2. POST to `/v1/ai/chat`, stream the response, accumulate text deltas
///      into the trailing assistant placeholder and tool_use blocks into a
///      local map.
///   3. On `stop_reason=tool_use`: ask the service to execute the tool_use
///      blocks (which appends a new user-role tool_result turn + a fresh
///      empty placeholder) and continue.
///   4. On `stop_reason=end_turn` or unknown: finish.
///
/// The loop is capped at [`MAX_TOOL_ITERATIONS`] iterations per user turn.
async fn run_loop(
    this: gpui::WeakEntity<AiChatService>,
    cx: &mut gpui::AsyncApp,
    client: reqwest::Client,
    cfg: CentoflowConfig,
    session_id: String,
) {
    for _ in 0..MAX_TOOL_ITERATIONS {
        // Snapshot history + context.
        let snapshot = this.update(cx, |s, cx| {
            let session = s.session(&session_id)?;
            let history = build_wire_messages(session);
            let model_id = session.model.clone();
            let (enabled_tools, client_context) = chat_context_for(cx);
            Some((history, model_id, enabled_tools, client_context))
        });
        let Ok(Some((history, model_id, enabled_tools, client_context))) = snapshot else {
            return;
        };

        let outcome =
            stream_one(&client, &cfg, &this, cx, &session_id, &model_id, history,
                enabled_tools, client_context).await;
        match outcome {
            StreamOutcome::EndTurn => {
                let _ = this.update(cx, |s, cx| s.finish_stream(&session_id, cx));
                return;
            }
            StreamOutcome::ToolUse => {
                // Two-phase tool execution: extract the pending tool_uses
                // under a sync entity update, then await each dispatch
                // outside the borrow so executors can hit REST + interleave
                // their own `cx.update(...)` blocks.
                let pending = this
                    .update(cx, |s, _cx| s.extract_pending_tool_uses(&session_id))
                    .unwrap_or_default();
                if pending.is_empty() {
                    // Anthropic flagged tool_use but nothing materialized —
                    // unusual, but treat as end-of-turn rather than spin.
                    let _ = this.update(cx, |s, cx| s.finish_stream(&session_id, cx));
                    return;
                }
                let mut outcomes: Vec<(String, ToolOutcome)> =
                    Vec::with_capacity(pending.len());
                for (id, name, input) in pending {
                    let outcome = dispatch_tool(name, input, cx).await;
                    outcomes.push((id, outcome));
                }
                let _ = this
                    .update(cx, |s, cx| s.apply_tool_outcomes(&session_id, outcomes, cx));
                // Loop continues — next iteration sees the appended
                // tool_result turn in history.
            }
            StreamOutcome::Failed => {
                // `fail_stream` was already called by stream_one — bail.
                return;
            }
        }
    }
    // Iteration cap hit. Mark the trailing assistant as a failure with a
    // diagnostic. The user can hit Retry, which pops the failed message
    // and re-enters the loop with the same history.
    let _ = this.update(cx, |s, cx| {
        s.fail_stream(
            &session_id,
            None,
            "Tool-use loop hit the per-turn iteration cap.",
            cx,
        )
    });
}

/// Returns the per-iteration tool surface + client context from the
/// `ChatContextProviderHandle` global, or empty/none if no provider is
/// registered (e.g. early startup / tests).
fn chat_context_for(cx: &App) -> (Vec<String>, Option<ClientContext>) {
    let Some(handle) = cx.try_global::<ChatContextProviderHandle>() else {
        return (Vec::new(), None);
    };
    let provider = handle.0.clone();
    let tools = provider.enabled_tools(cx);
    let ctx = provider.client_context(cx);
    (tools, ctx)
}

#[derive(Clone, Copy, Debug)]
enum StreamOutcome {
    EndTurn,
    ToolUse,
    Failed,
}

/// One iteration of the agentic loop: POST + stream + handle terminal
/// `message_delta.stop_reason`. Per-block state (in-flight tool_use json
/// accumulators) is local — finalized tool_use blocks are pushed into the
/// session via `append_tool_use`; text deltas via `append_delta`.
#[allow(clippy::too_many_arguments)]
async fn stream_one(
    client: &reqwest::Client,
    cfg: &CentoflowConfig,
    this: &gpui::WeakEntity<AiChatService>,
    cx: &mut gpui::AsyncApp,
    session_id: &str,
    model_id: &str,
    history: Vec<WireMsg>,
    enabled_tools: Vec<String>,
    client_context: Option<ClientContext>,
) -> StreamOutcome {
    let url = format!("{}/v1/ai/chat", cfg.base_url);
    let body = WireReq {
        model: model_id,
        messages: history,
        max_tokens: MAX_OUTPUT_TOKENS,
        enabled_tools,
        client_context,
    };
    let mut req = client
        .post(&url)
        .header("accept", "text/event-stream")
        .json(&body);
    if let Some(token) = &cfg.token {
        req = req.bearer_auth(token);
    }
    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            let err = format!("Network error: {e}");
            let _ = this.update(cx, |s, cx| s.fail_stream(session_id, None, &err, cx));
            return StreamOutcome::Failed;
        }
    };
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        let msg = if body.is_empty() {
            format!("HTTP {status}")
        } else {
            format!("HTTP {status}: {}", body.trim())
        };
        let _ = this.update(cx, |s, cx| {
            s.fail_stream(session_id, Some(status.as_u16()), &msg, cx)
        });
        return StreamOutcome::Failed;
    }

    // Per-block in-flight state. Keyed by Anthropic's `index` field on
    // content_block_start/delta/stop events.
    #[derive(Default)]
    struct InFlightTool {
        id: String,
        name: String,
        input_json: String,
    }
    let mut tools_in_flight: HashMap<usize, InFlightTool> = HashMap::new();
    let mut stop_reason: Option<String> = None;

    let mut buf: Vec<u8> = Vec::new();
    let mut pending_text = String::new();
    let mut last_flush_ms = now_ms();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(c) => c,
            Err(e) => {
                if !pending_text.is_empty() {
                    let to_flush = std::mem::take(&mut pending_text);
                    let _ = this.update(cx, |s, cx| s.append_delta(session_id, &to_flush, cx));
                }
                let err = format!("Connection lost: {e}");
                let _ = this.update(cx, |s, cx| s.fail_stream(session_id, None, &err, cx));
                return StreamOutcome::Failed;
            }
        };
        buf.extend_from_slice(&chunk);
        while let Some(data) = pop_sse_event(&mut buf) {
            if data.is_empty() {
                continue;
            }
            let Ok(payload) = serde_json::from_str::<SsePayload>(&data) else {
                continue;
            };
            match payload.kind.as_str() {
                "content_block_start" => {
                    if let Some(cb) = payload.content_block.as_ref() {
                        if cb.kind == "tool_use" {
                            tools_in_flight.insert(
                                payload.index,
                                InFlightTool {
                                    id: cb.id.clone(),
                                    name: cb.name.clone(),
                                    input_json: String::new(),
                                },
                            );
                        }
                    }
                }
                "content_block_delta" => {
                    let Some(d) = payload.delta else { continue };
                    match d.kind.as_str() {
                        "text_delta" if !d.text.is_empty() => {
                            pending_text.push_str(&d.text);
                        }
                        "input_json_delta" => {
                            if let Some(entry) = tools_in_flight.get_mut(&payload.index) {
                                entry.input_json.push_str(&d.partial_json);
                            }
                        }
                        _ => {}
                    }
                }
                "content_block_stop" => {
                    if let Some(entry) = tools_in_flight.remove(&payload.index) {
                        let input: JsonValue = serde_json::from_str(&entry.input_json)
                            .unwrap_or_else(|_| JsonValue::Object(Default::default()));
                        // Flush any pending text first so block order in the
                        // stored ChatMsg matches the order Anthropic emitted.
                        if !pending_text.is_empty() {
                            let to_flush = std::mem::take(&mut pending_text);
                            let _ = this
                                .update(cx, |s, cx| s.append_delta(session_id, &to_flush, cx));
                        }
                        let _ = this.update(cx, |s, cx| {
                            s.append_tool_use(session_id, entry.id, entry.name, input, cx)
                        });
                    }
                }
                "message_delta" => {
                    if let Some(d) = payload.delta {
                        if !d.stop_reason.is_empty() {
                            stop_reason = Some(d.stop_reason);
                        }
                    }
                }
                // message_start / message_stop / errors carry metadata we
                // don't need to react to here — the server already logs
                // usage; transport-layer errors come through the bytes
                // stream's Err arm above.
                _ => {}
            }
        }
        let cur_ms = now_ms();
        if !pending_text.is_empty() && cur_ms - last_flush_ms >= 50 {
            let to_flush = std::mem::take(&mut pending_text);
            let _ = this.update(cx, |s, cx| s.append_delta(session_id, &to_flush, cx));
            last_flush_ms = cur_ms;
        }
    }
    // Final flush of any tail text.
    if !pending_text.is_empty() {
        let _ = this.update(cx, |s, cx| s.append_delta(session_id, &pending_text, cx));
    }

    match stop_reason.as_deref() {
        Some("tool_use") => StreamOutcome::ToolUse,
        // "end_turn" / "max_tokens" / "stop_sequence" / unknown → terminate.
        _ => StreamOutcome::EndTurn,
    }
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Title from first user message — truncate at ~32 chars on a word boundary
/// if possible, otherwise hard-cut. Collapses interior whitespace.
fn truncate_title(text: &str) -> String {
    const MAX: usize = 32;
    let collapsed: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= MAX {
        return collapsed;
    }
    let mut acc = String::new();
    for (i, ch) in collapsed.chars().enumerate() {
        if i >= MAX {
            break;
        }
        acc.push(ch);
    }
    acc.push('…');
    acc
}

#[cfg(test)]
mod tests {
    use super::*;
    use wasm_bindgen_test::wasm_bindgen_test;

    #[wasm_bindgen_test]
    fn pop_sse_event_handles_chunked_arrival() {
        // Simulate Anthropic SSE arriving as two chunks split mid-event.
        let mut buf: Vec<u8> = Vec::new();
        buf.extend_from_slice(b"event: message_start\n");
        buf.extend_from_slice(b"data: {\"type\":\"message_start\"}\n\n");
        buf.extend_from_slice(b"event: content_block_delta\n");
        buf.extend_from_slice(b"data: {\"type\":\"content_block_de");
        let first = pop_sse_event(&mut buf).expect("first event");
        assert_eq!(first, "{\"type\":\"message_start\"}");
        // Second event isn't complete yet.
        assert!(pop_sse_event(&mut buf).is_none());
        // Append the rest of the second event + a complete third.
        buf.extend_from_slice(b"lta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n");
        let second = pop_sse_event(&mut buf).expect("second event");
        assert!(second.contains("\"text_delta\""));
    }

    #[wasm_bindgen_test]
    fn pop_sse_event_returns_empty_for_eventline_only() {
        let mut buf: Vec<u8> = Vec::new();
        buf.extend_from_slice(b"event: keep-alive\n\n");
        let got = pop_sse_event(&mut buf).expect("popped");
        assert_eq!(got, "");
    }

    #[wasm_bindgen_test]
    fn migrate_model_id_maps_legacy_tags() {
        assert_eq!(migrate_model_id("sonnet-46"), "claude-sonnet-4-6");
        assert_eq!(migrate_model_id("opus-47"), "claude-opus-4-7");
        assert_eq!(migrate_model_id("haiku-45"), "claude-haiku-4-5-20251001");
        assert_eq!(migrate_model_id("claude-sonnet-4-6"), "claude-sonnet-4-6");
        assert_eq!(migrate_model_id(""), "claude-sonnet-4-6");
    }

    #[wasm_bindgen_test]
    fn short_from_id_picks_family() {
        assert_eq!(short_from_id("claude-opus-4-7"), "Opus");
        assert_eq!(short_from_id("claude-sonnet-4-6"), "Sonnet");
        assert_eq!(short_from_id("claude-haiku-4-5-20251001"), "Haiku");
        assert_eq!(short_from_id("something-else"), "Model");
    }

    #[wasm_bindgen_test]
    fn chat_msg_legacy_text_promotes_to_block() {
        let json = r#"{"role":"user","text":"hello","error":false}"#;
        let mut m: ChatMsg = serde_json::from_str(json).expect("decode legacy");
        m.normalize();
        assert_eq!(m.blocks.len(), 1);
        assert_eq!(m.text(), "hello");
        // Re-serialize: blocks present, top-level legacy `text` field gone.
        // Parsing the re-serialized form as a generic JSON object lets us
        // assert on the top-level keys without false matches on nested
        // `text` fields inside content blocks.
        let back = serde_json::to_string(&m).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&back).unwrap();
        let obj = parsed.as_object().unwrap();
        assert!(!obj.contains_key("text"), "legacy text key leaked: {back}");
        assert!(obj.contains_key("blocks"));
    }

    #[wasm_bindgen_test]
    fn chat_msg_append_text_extends_trailing_block() {
        let mut m = ChatMsg {
            role: Speaker::Assistant,
            blocks: vec![ContentBlock::text("hi")],
            legacy_text: String::new(),
            error: false,
        };
        m.append_text(" there");
        assert_eq!(m.text(), "hi there");
        // Inserting tool_use then more text starts a fresh block.
        m.blocks.push(ContentBlock::ToolUse {
            id: "t1".into(),
            name: "add_horizontal_ray".into(),
            input: serde_json::json!({"price": 100.0}),
        });
        m.append_text("done");
        // text() joins Text blocks with newline.
        assert_eq!(m.text(), "hi there\ndone");
    }

    #[wasm_bindgen_test]
    fn build_wire_messages_drops_trailing_placeholder() {
        let session = Session {
            id: "s".into(),
            title: "".into(),
            messages: vec![
                ChatMsg {
                    role: Speaker::User,
                    blocks: vec![ContentBlock::text("hi")],
                    legacy_text: String::new(),
                    error: false,
                },
                ChatMsg {
                    role: Speaker::Assistant,
                    blocks: Vec::new(),
                    legacy_text: String::new(),
                    error: false,
                },
            ],
            model: "claude-sonnet-4-6".into(),
            draft: String::new(),
            created_at_ms: 0,
            updated_at_ms: 0,
        };
        let wire = build_wire_messages(&session);
        assert_eq!(wire.len(), 1);
        assert_eq!(wire[0].role, "user");
    }
}
