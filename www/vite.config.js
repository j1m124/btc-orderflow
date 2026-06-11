import { defineConfig } from 'vite';
import wasm from 'vite-plugin-wasm';

// VITE_BASE_PATH is set by the GitHub Pages workflow to "/<repo-name>/" so deployed asset
// URLs resolve under the project site path. Local dev keeps the default "/".
const base = process.env.VITE_BASE_PATH ?? '/';

// Where /ws gets proxied. Default = local server. Set BACKEND_TARGET to a
// wss:// URL to point the local client at the prod VPS for rapid iteration.
// Parsed into an object so Vite's `rewriteWsOrigin` derives a proper
// http(s):// Origin from `target.protocol + target.host` — passing the raw
// `wss://…` string makes Vite stamp `Origin: wss://…`, which the server's
// ALLOWED_ORIGINS check (which expects `https://…`) rejects.
const backendUrl = new URL(process.env.BACKEND_TARGET ?? 'ws://127.0.0.1:8787');
const backendTarget = {
  protocol: backendUrl.protocol === 'wss:' ? 'https:' : 'http:',
  host: backendUrl.host,
};
// Rewrite the browser's `localhost:3001` Origin to match the backend's
// allowlist. Skipped for local targets (no allowlist to satisfy). Safe
// because the target is a fixed backend we control — not a CSRF risk.
const rewriteWsOrigin = !/^(127\.0\.0\.1|localhost)(:|$)/.test(backendUrl.host);

export default defineConfig({
  base,
  plugins: [wasm()],
  build: {
    target: 'esnext',
    minify: true,
    sourcemap: false,
    rollupOptions: {
      input: { main: 'index.html' },
    },
  },
  server: {
    // Port is intentionally NOT 3000 — the sibling centoflow project also runs
    // on :3000, and sharing the port would alias localStorage and stomp this
    // app's persisted layout/theme/etc.
    port: 3001,
    open: true,
    fs: {
      strict: false,
      allow: ['..'],
    },
    headers: {
      'Cross-Origin-Embedder-Policy': 'require-corp',
      'Cross-Origin-Opener-Policy': 'same-origin',
    },
    // Proxy /ws to the Rust server on :8787 so the client can derive its
    // WS URL from `window.location` unconditionally — same code path in dev
    // (Vite on :3001) and prod (axum serves /ws and the static SPA on one
    // origin). Target is BACKEND_TARGET-overridable so we can point local
    // dev at the prod VPS without rebuilding WASM.
    proxy: {
      '/ws': {
        target: backendTarget,
        ws: true,
        changeOrigin: true,
        // Vite-builtin Origin rewrite (changeOrigin only rewrites Host).
        rewriteWsOrigin,
      },
    },
  },
  optimizeDeps: {
    exclude: ['./src/wasm'],
  },
});
