// gpui_web creates a 1x1 fixed-position transparent <input> kept focused so
// it can receive IME/composition events. On mobile Chrome any focused text
// input opens the soft keyboard, so every tap pops it. Setting
// inputmode="none" tells the browser not to show a virtual keyboard while
// still allowing the element to focus for paste/IME events.
function suppressMobileKeyboard() {
  const apply = (el) => el.setAttribute('inputmode', 'none');
  document.querySelectorAll('body > input').forEach(apply);
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
    const wasm = await import('./wasm/client.js');
    await wasm.default();
    await wasm.run();

    if (appEl) appEl.remove();
  } catch (error) {
    console.error('Failed to initialize:', error);
    if (loadingEl) {
      loadingEl.innerHTML = `
        <div class="error">
          <h2>Failed to load btc-orderflow</h2>
          <p>${error.message || error}</p>
          <p style="margin-top:10px; font-size:14px;">See console for details.</p>
        </div>
      `;
    }
  }
}

init();
