// Thin shim between supabase-js and the Rust auth watcher.
//
// `auth.js` runs before the wasm bundle finishes loading, so it can't import
// the Rust export directly. `main.js` registers the export here once the wasm
// is ready; `auth.js` calls `setToken` from `onAuthStateChange`, which either
// dispatches straight through (post-init) or no-ops (pre-init — the watcher's
// startup localStorage read and the 5-minute poll fallback cover that window).

let _setTokenImpl = null;

/** Called by main.js after `wasm.default()` resolves. */
export function _registerSetToken(fn) {
  _setTokenImpl = fn;
}

/** Push the current access token (or null on sign-out) to the Rust watcher. */
export function setToken(token) {
  if (_setTokenImpl) {
    _setTokenImpl(token ?? undefined);
  }
}
