// App-page auth gate + token sync. Loaded before main.js so the gate decision
// happens before the WASM boots.
//
// - Requires a Supabase session; redirects to /login.html when absent.
// - Mirrors the session token into localStorage on every auth change
//   (TOKEN_REFRESHED, SIGNED_IN, SIGNED_OUT) AND pushes it straight into the
//   Rust watcher via `bridge.setToken` so a refresh applies instantly instead
//   of waiting for the watcher's 5-minute poll.
// - When Supabase isn't configured, the gate is skipped (ungated dev mode).
import { supabase, syncToken, isConfigured } from './supabase.js';
import { setToken } from './bridge.js';

const LOGIN = 'login.html';

if (isConfigured) {
  supabase.auth.onAuthStateChange((_event, session) => {
    syncToken(session);
    setToken(session?.access_token);
    if (!session) location.replace(LOGIN);
  });

  const {
    data: { session },
  } = await supabase.auth.getSession();

  if (!session) {
    location.replace(LOGIN);
  } else {
    syncToken(session);
  }
}
