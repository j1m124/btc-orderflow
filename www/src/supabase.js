// Shared Supabase client + the bridge to the WASM app's auth.
//
// The Go server validates a Supabase-issued JWT. The WASM app reads that JWT
// from localStorage["centoflow.auth.v1"] as `{"token": "<jwt>"}` (see
// crates/terminal_demo/src/persistence.rs). This module keeps that key in sync
// with the live Supabase session; a Rust watcher polls it and reconnects.
//
// Config comes from Vite env (www/.env):
//   VITE_SUPABASE_URL=https://<ref>.supabase.co
//   VITE_SUPABASE_PUBLISHABLE_KEY=sb_publishable_xxx   (the new publishable key)
import { createClient } from 'https://esm.sh/@supabase/supabase-js@2';

// Trim + coalesce: an unset GitHub/CI variable expands to "" (not undefined),
// which would slip past `??` and make createClient throw "supabaseUrl is required".
const url = (import.meta.env.VITE_SUPABASE_URL || '').trim();
const key = (import.meta.env.VITE_SUPABASE_PUBLISHABLE_KEY || '').trim();

/** True when Supabase is configured. When false, the app runs ungated (handy
 *  against a dev-mode server that doesn't require auth). */
export const isConfigured = Boolean(url && key);

if (!isConfigured) {
  console.warn(
    'Supabase not configured — set VITE_SUPABASE_URL and ' +
      'VITE_SUPABASE_PUBLISHABLE_KEY in www/.env. Running without auth.',
  );
}

/** localStorage key the WASM app reads its JWT from. */
export const AUTH_KEY = 'centoflow.auth.v1';

// Only construct the client when configured — createClient throws on an empty
// URL/key. When unconfigured this is null and the app runs ungated; both
// consumers (auth.js, login.html) guard on `isConfigured`.
export const supabase = isConfigured
  ? createClient(url, key, {
      auth: { persistSession: true, autoRefreshToken: true, detectSessionInUrl: true },
    })
  : null;

/** Mirror the Supabase session's access token into the WASM-readable key. */
export function syncToken(session) {
  if (session?.access_token) {
    localStorage.setItem(AUTH_KEY, JSON.stringify({ token: session.access_token }));
  } else {
    localStorage.removeItem(AUTH_KEY);
  }
}
