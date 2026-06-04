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
    // Build both the app and the login page (multi-page).
    rollupOptions: {
      input: { main: 'index.html', login: 'login.html' },
    },
  },
  server: {
    port: 3000,
    open: true,
    fs: {
      strict: false,
      allow: ['..'],
    },
    headers: {
      'Cross-Origin-Embedder-Policy': 'require-corp',
      'Cross-Origin-Opener-Policy': 'same-origin',
    },
  },
  optimizeDeps: {
    exclude: ['./src/wasm'],
  },
});
