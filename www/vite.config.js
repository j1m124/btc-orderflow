import { defineConfig } from 'vite';
import wasm from 'vite-plugin-wasm';

// VITE_BASE_PATH is set by the GitHub Pages workflow to "/<repo-name>/" so deployed asset
// URLs resolve under the project site path. Local dev keeps the default "/".
const base = process.env.VITE_BASE_PATH ?? '/';

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
    // origin).
    proxy: {
      '/ws': {
        target: 'ws://127.0.0.1:8787',
        ws: true,
        changeOrigin: true,
      },
    },
  },
  optimizeDeps: {
    exclude: ['./src/wasm'],
  },
});
