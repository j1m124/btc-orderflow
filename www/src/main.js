// gpui_web creates a 1x1 fixed-position transparent <input> element and keeps it focused so
// it can receive IME/composition events. On mobile Chrome (and other touch browsers) any
// focused text input opens the soft keyboard — so every tap pops the keyboard.
//
// Setting inputmode="none" on that element tells the browser not to show a virtual keyboard
// while still allowing the element to be focused for paste/IME events. We watch for the
// element being added (gpui creates it during window init, after our app code runs) and
// also catch it again if gpui ever recreates it.
function suppressMobileKeyboard() {
  const apply = (el) => el.setAttribute('inputmode', 'none');
  // Catch any inputs already in the DOM.
  document.querySelectorAll('body > input').forEach(apply);
  // Catch future ones.
  const observer = new MutationObserver((mutations) => {
    for (const m of mutations) {
      for (const node of m.addedNodes) {
        if (node.tagName === 'INPUT' && node.parentElement === document.body) {
          apply(node);
        }
      }
    }
  });
  observer.observe(document.body, { childList: true });
}

async function init() {
  const loadingEl = document.getElementById('loading');
  const appEl = document.getElementById('app');

  try {
    suppressMobileKeyboard();
    const wasm = await import('./wasm/terminal_demo.js');
    await wasm.default();
    // Wire the auth bridge so auth.js's onAuthStateChange pushes straight to
    // Rust instead of waiting for the 5-minute fallback poll.
    const { _registerSetToken } = await import('./bridge.js');
    _registerSetToken(wasm.set_token_from_js);
    await wasm.run();

    if (appEl) appEl.remove();
  } catch (error) {
    console.error('Failed to initialize:', error);
    if (loadingEl) {
      loadingEl.innerHTML = `
        <div class="error">
          <h2>Failed to load terminal_demo</h2>
          <p>${error.message || error}</p>
          <p style="margin-top:10px; font-size:14px;">See console for details.</p>
        </div>
      `;
    }
  }
}

init();
