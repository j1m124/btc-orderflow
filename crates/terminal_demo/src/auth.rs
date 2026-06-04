//! Runtime auth for the centoflow market-data server.
//!
//! The server validates a provider-issued JWT (Supabase/Clerk/Auth0/…); this
//! app just holds and forwards it. The token can arrive two ways:
//!
//! 1. **Hosted login → localStorage.** The (separately hosted) login page,
//!    being same-origin, writes the JWT to `localStorage["centoflow.auth.v1"]`
//!    then navigates into the app. [`crate::net::init`] reads it at startup.
//! 2. **In-app.** Call [`set_token`] (e.g. from a future sign-in dialog or a
//!    JS shim) to set it live without a reload.
//!
//! Either way the token is persisted and the live services are restarted so
//! they pick up the new credentials immediately.

use gpui::App;

use std::cell::RefCell;
use std::time::Duration;

use futures::FutureExt as _;
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use futures::stream::StreamExt as _;
use gpui::{AppContext as _, Context, Entity, Global, Task};
use wasm_bindgen::prelude::*;

use crate::net::CentoflowConfig;
use crate::persistence::{self, AuthConfig};
use crate::services::market_data::MarketDataServiceHandle;
use crate::services::symbols::SymbolsServiceHandle;

/// Whether a token is currently set.
pub fn is_authenticated(cx: &App) -> bool {
    cx.global::<CentoflowConfig>().token.is_some()
}

/// Set (or clear, with `None`) the JWT: update the live config global, persist
/// it, and restart the market-data + symbols services so they reconnect with
/// the new token. Safe to call at any time after `init`.
pub fn set_token(cx: &mut App, token: Option<String>) {
    let mut config = cx.global::<CentoflowConfig>().clone();
    config.token = token.clone();
    cx.set_global(config);

    if let Err(e) = persistence::save_auth(&AuthConfig { token }) {
        log::warn!("failed to persist auth: {e:#}");
    }

    // Restart in-flight services so the new token takes effect now (the
    // per-subscription loops re-read config on each (re)connect).
    let market_data = cx.global::<MarketDataServiceHandle>().0.clone();
    market_data.update(cx, |svc, cx| svc.reconnect_all(cx));
    let symbols = cx.global::<SymbolsServiceHandle>().0.clone();
    symbols.update(cx, |svc, cx| svc.reload(cx));
}

/// Clear the token and restart services. On web, also wipes supabase-js's own
/// session keys and navigates to `/login.html` — without the supabase-side
/// cleanup, `supabase.auth.getSession()` on the login page restores the
/// session from localStorage and bounces the user straight back into the app.
pub fn logout(cx: &mut App) {
    set_token(cx, None);
    purge_web_session_and_redirect();
}

/// On web, remove every localStorage key supabase-js owns (`sb-*`) plus the
/// app's own `centoflow.auth.v1` mirror (`set_token(None)` only overwrites it
/// with `{"token":null}`, leaving the key present), then navigate to the
/// login page. No-op on native.
#[cfg(target_family = "wasm")]
fn purge_web_session_and_redirect() {
    let Some(window) = web_sys::window() else {
        return;
    };
    if let Ok(Some(storage)) = window.local_storage() {
        // Collect first, then remove: removing during iteration shifts the
        // remaining indices and skips keys.
        let len = storage.length().unwrap_or(0);
        let mut to_remove: Vec<String> = Vec::new();
        for i in 0..len {
            if let Ok(Some(k)) = storage.key(i) {
                if k.starts_with("sb-") || k == "centoflow.auth.v1" {
                    to_remove.push(k);
                }
            }
        }
        for k in &to_remove {
            let _ = storage.remove_item(k);
        }
    }
    if let Err(e) = window.location().set_href("/login.html") {
        log::warn!("redirect to login failed: {e:?}");
    }
}

#[cfg(not(target_family = "wasm"))]
fn purge_web_session_and_redirect() {}

// ---------------------------------------------------------------------------
// Token watcher
// ---------------------------------------------------------------------------

/// Fallback poll of persisted auth. Push from JS via [`set_token_from_js`] is
/// the common path; this catches the edge case where the JS bridge missed an
/// event (e.g. wasm wasn't loaded yet when supabase-js fired) and the
/// localStorage key drifted from the in-memory token.
const POLL_INTERVAL: Duration = Duration::from_secs(5 * 60);

thread_local! {
    /// Set by `AuthWatcher::new`. Consumed by [`set_token_from_js`] to push
    /// JS-side token updates into the watcher's async loop. `None` before
    /// `init_watcher` runs, after which it stays `Some` for the app lifetime.
    static BRIDGE_TX: RefCell<Option<UnboundedSender<Option<String>>>> = const { RefCell::new(None) };
}

/// JS-side callback for supabase-js's `onAuthStateChange`. Pushes the new
/// token (or `None` on sign-out) into the watcher's loop, which applies it via
/// [`set_token`] on the next async tick. Calls before `init_watcher` runs are
/// silently dropped — the AuthWatcher's poll fallback and the startup
/// localStorage read cover that window.
#[wasm_bindgen]
pub fn set_token_from_js(token: Option<String>) {
    BRIDGE_TX.with(|cell| {
        if let Some(tx) = cell.borrow().as_ref() {
            let _ = tx.unbounded_send(token);
        }
    });
}

/// Drains JS-pushed token changes and polls persisted auth as a fallback,
/// applying any drift via [`set_token`] which cascades to the data services.
pub struct AuthWatcher {
    _task: Task<()>,
}

impl AuthWatcher {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let (tx, rx) = unbounded::<Option<String>>();
        BRIDGE_TX.with(|cell| {
            *cell.borrow_mut() = Some(tx);
        });
        let task = cx.spawn(async move |this, cx| run_watch(this, cx, rx).await);
        Self { _task: task }
    }
}

struct AuthWatcherHandle(#[allow(dead_code)] Entity<AuthWatcher>);
impl Global for AuthWatcherHandle {}

/// Spawn the auth watcher. Call once at startup, after `net::init` and the
/// services are registered (it restarts them on a token change).
pub fn init_watcher(cx: &mut App) {
    let entity = cx.new(AuthWatcher::new);
    cx.set_global(AuthWatcherHandle(entity));
}

async fn run_watch(
    this: gpui::WeakEntity<AuthWatcher>,
    cx: &mut gpui::AsyncApp,
    mut bridge_rx: UnboundedReceiver<Option<String>>,
) {
    loop {
        let timer = cx.background_executor().timer(POLL_INTERVAL);
        let mut timer = std::pin::pin!(timer.fuse());
        futures::select! {
            _ = timer.as_mut() => {
                let persisted = persistence::load_auth().token;
                let applied = this.update(cx, |_w, cx| {
                    if cx.global::<CentoflowConfig>().token == persisted {
                        return false;
                    }
                    set_token(cx, persisted.clone());
                    true
                });
                match applied {
                    Ok(true) => log::info!("auth poll: token drifted in storage; reconnecting"),
                    Ok(false) => {}
                    Err(_) => return,
                }
            }
            token_opt = bridge_rx.next() => {
                let Some(token) = token_opt else { return; }; // bridge closed
                let applied = this.update(cx, |_w, cx| {
                    if cx.global::<CentoflowConfig>().token == token {
                        return false;
                    }
                    set_token(cx, token);
                    true
                });
                match applied {
                    Ok(true) => log::info!("auth push: token updated from JS; reconnecting"),
                    Ok(false) => {}
                    Err(_) => return,
                }
            }
        }
    }
}
