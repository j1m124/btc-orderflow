//! HTTP + WebSocket networking primitives.
//!
//! `HttpClient` is one shared `reqwest::Client` as a gpui `Global` — every
//! service pools connections through it. On wasm, reqwest transparently
//! routes through the browser's `fetch()`; we use the default builder
//! because the timeout/connect-timeout setters don't compile against the
//! wasm backend.
//!
//! `CentoflowConfig` holds the market-data server's base URL + optional JWT.
//! It's a `Global` so a login flow can replace it at runtime (the
//! market-data subscription loop re-reads it on every (re)connect).
//!
//! `ws_open` connects a `web_sys::WebSocket` (via `ws_stream_wasm`) and returns
//! the duplex stream. Callers drive the protocol themselves — sending the
//! centoflow `{"action":"subscribe",…}` / `unsubscribe` frames and reading bar
//! pushes — because the multiplexed `MarketDataService` needs to interleave
//! writes (new subs from chart panels) and reads (server-pushed bars) on the
//! same socket for the lifetime of the connection.

use gpui::{App, Global};
use ws_stream_wasm::WsStream;

pub struct HttpClient(pub reqwest::Client);
impl Global for HttpClient {}

/// Market-data server connection config. Cloned by the subscription loop on
/// each connect so changes (e.g. a token set after login) take effect on the
/// next reconnect.
#[derive(Clone)]
pub struct CentoflowConfig {
    /// HTTP base, e.g. `http://localhost:8080` (no trailing slash).
    pub base_url: String,
    /// JWT bearer token, if the server requires auth.
    pub token: Option<String>,
}
impl Global for CentoflowConfig {}

impl CentoflowConfig {
    /// Read from compile-time env (`CENTOFLOW_BASE_URL`, `CENTOFLOW_TOKEN`),
    /// falling back to a local dev server. `option_env!` is used so the values
    /// bake into the WASM bundle (no runtime env on the web).
    fn from_env() -> Self {
        // `.filter(non-empty)`: an unset build var is baked as Some("") (not
        // None), which would otherwise yield base_url="" and make every request
        // a relative URL ("relative URL without a base").
        let base_url = option_env!("CENTOFLOW_BASE_URL")
            .filter(|s| !s.is_empty())
            .unwrap_or("http://localhost:8080")
            .trim_end_matches('/')
            .to_string();
        let token = option_env!("CENTOFLOW_TOKEN")
            .filter(|t| !t.is_empty())
            .map(|t| t.to_string());
        Self { base_url, token }
    }

    /// WebSocket base derived from `base_url` (http→ws, https→wss).
    pub fn ws_base(&self) -> String {
        if let Some(rest) = self.base_url.strip_prefix("https://") {
            format!("wss://{rest}")
        } else if let Some(rest) = self.base_url.strip_prefix("http://") {
            format!("ws://{rest}")
        } else {
            self.base_url.clone()
        }
    }
}

pub fn init(cx: &mut App) {
    cx.set_global(HttpClient(reqwest::Client::new()));

    // Start from compile-time env defaults, then overlay a persisted token
    // (set by a previous in-app sign-in, or written to localStorage by a hosted
    // login page). A persisted token wins over the env default.
    let mut config = CentoflowConfig::from_env();
    if let Some(token) = crate::persistence::load_auth().token {
        config.token = Some(token);
    }
    cx.set_global(config);
}

// ---------------------------------------------------------------------------
// WebSocket
// ---------------------------------------------------------------------------

/// Open a WS to `url`. Returns the duplex stream; the caller writes subscribe /
/// unsubscribe frames and reads bar pushes for the connection's lifetime.
pub async fn ws_open(url: &str) -> anyhow::Result<WsStream> {
    use ws_stream_wasm::WsMeta;
    let (_meta, ws) = WsMeta::connect(url, None)
        .await
        .map_err(|e| anyhow::anyhow!("ws connect failed: {e}"))?;
    Ok(ws)
}
