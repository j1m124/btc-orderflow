//! Screenshot capture + preview dialog.
//!
//! Flow: top-bar button → capture the full app canvas FIRST (so the dialog
//! never occludes itself) → preview dialog with [Copy] [Save] in the footer.
//! Save triggers a browser download (lands in the browser's configured
//! download directory — the web platform offers no path picker); Copy writes
//! a PNG `ClipboardItem`. Both actions close the dialog and toast.
//!
//! Capture rides on `HtmlCanvasElement::toBlob`, which snapshots the canvas
//! bitmap at call time. On WebGPU backends the last-presented frame is
//! readable; on a WebGL2 fallback without `preserveDrawingBuffer` the read
//! can come back blank — that surfaces as a blank preview, not an error.

use std::sync::Arc;

use futures::channel::oneshot;
use gpui::{
    App, Image, ImageFormat, ImageSource, ParentElement as _, SharedString, Styled as _, Window,
    div, img, px,
};
use gpui_component::{
    IconName, Sizable as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    notification::Notification,
};
use wasm_bindgen::{JsCast as _, JsValue, closure::Closure};
use wasm_bindgen_futures::JsFuture;

/// Selector for the gpui render canvas. gpui_web creates an anonymous
/// `<canvas>` appended directly to `<body>` at boot (it does NOT use the
/// static `#canvas` in `www/index.html` — that one is loading-shell
/// decoration removed by `main.js` after `run()`). A direct-child selector
/// is unambiguous in both the pre- and post-removal DOM.
const CANVAS_SELECTOR: &str = "body > canvas";

/// Entry point from the top bar. Captures first, then opens the preview;
/// failures toast instead of opening a dialog.
pub fn open(window: &mut Window, cx: &mut App) {
    window
        .spawn(cx, async move |cx| {
            let captured = capture_canvas_png().await;
            cx.update(|window, cx| match captured {
                Ok(png) => open_preview(Arc::new(png), window, cx),
                Err(err) => {
                    log::error!("screenshot capture failed: {err}");
                    notify_error(window, cx, "Screenshot failed", &err);
                }
            })
            .ok();
        })
        .detach();
}

// ---------------------------------------------------------------------------
// Capture
// ---------------------------------------------------------------------------

/// Snapshot the app canvas into PNG bytes via `toBlob`.
async fn capture_canvas_png() -> Result<Vec<u8>, String> {
    let document = web_sys::window()
        .and_then(|w| w.document())
        .ok_or("no document")?;
    let canvas: web_sys::HtmlCanvasElement = document
        .query_selector(CANVAS_SELECTOR)
        .map_err(|e| format!("selector failed: {e:?}"))?
        .ok_or_else(|| format!("canvas not found via `{CANVAS_SELECTOR}`"))?
        .dyn_into()
        .map_err(|_| format!("`{CANVAS_SELECTOR}` matched a non-canvas"))?;

    let (tx, rx) = oneshot::channel::<Option<web_sys::Blob>>();
    // `once_into_js` hands the closure to the JS GC; it frees itself after
    // the single invocation, so no Rust-side handle has to outlive the call.
    let cb = Closure::once_into_js(move |blob: JsValue| {
        let _ = tx.send(blob.dyn_into::<web_sys::Blob>().ok());
    });
    canvas
        .to_blob(cb.unchecked_ref())
        .map_err(|e| format!("toBlob failed: {e:?}"))?;

    let blob = rx
        .await
        .map_err(|_| "capture callback never fired".to_string())?
        .ok_or("canvas produced no image")?;
    let buf = JsFuture::from(blob.array_buffer())
        .await
        .map_err(|e| format!("blob read failed: {e:?}"))?;
    let bytes = js_sys::Uint8Array::new(&buf).to_vec();
    if bytes.is_empty() {
        return Err("canvas produced an empty image".into());
    }
    Ok(bytes)
}

// ---------------------------------------------------------------------------
// Preview dialog
// ---------------------------------------------------------------------------

fn open_preview(png: Arc<Vec<u8>>, window: &mut Window, cx: &mut App) {
    let image = Arc::new(Image::from_bytes(ImageFormat::Png, png.as_ref().clone()));
    // Aspect ratio (h/w) straight from the PNG header so the preview box
    // matches the capture exactly — no letterboxing dead space.
    let aspect = png_dimensions(&png).map(|(w, h)| h as f32 / w as f32);

    window.open_dialog(cx, move |dialog, window, _cx| {
        let png_for_copy = png.clone();
        let png_for_save = png.clone();
        // 80% of the viewport wide; preview height follows the screenshot's
        // own aspect ratio, leaving the header/footer their natural room.
        let viewport = window.viewport_size();
        let dialog_w = viewport.width * 0.8;
        let content_w = dialog_w - px(32.); // px_4 padding either side
        let aspect = aspect.unwrap_or(viewport.height / viewport.width);
        // Footer + paddings around the preview, roughly (no header).
        let chrome_h = px(80.);
        // Cap the preview so the dialog never overflows a short window
        // (ObjectFit::Contain letterboxes the sides in that edge case).
        let preview_h = (content_w * aspect).min(viewport.height - chrome_h - px(16.));
        // The Dialog default anchors its top at height/10, which dumps a
        // dialog this tall toward the bottom — center it explicitly.
        let margin_top = ((viewport.height - preview_h - chrome_h) / 2.).max(px(8.));
        dialog
            .w(dialog_w)
            .margin_top(margin_top)
            .child(
                div().px_4().pt_4().child(
                    // ObjectFit::Contain is the img default, so the
                    // capture letterboxes into this box undistorted.
                    img(ImageSource::Image(image.clone()))
                        .w_full()
                        .h(preview_h)
                        .rounded(px(4.)),
                ),
            )
            .footer(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .px_4()
                    .pb_4()
                    .child(
                        Button::new("screenshot-copy")
                            .outline()
                            .small()
                            .icon(IconName::Copy)
                            .label("Copy")
                            .on_click(move |_, window, cx| {
                                copy_to_clipboard(png_for_copy.clone(), window, cx);
                                window.close_dialog(cx);
                            }),
                    )
                    .child(
                        Button::new("screenshot-save")
                            .primary()
                            .small()
                            .label("Save")
                            .on_click(move |_, window, cx| {
                                match save_download(&png_for_save) {
                                    Ok(filename) => notify_success(
                                        window,
                                        cx,
                                        "Screenshot saved",
                                        &format!("Downloading {filename}"),
                                    ),
                                    Err(err) => {
                                        log::error!("screenshot save failed: {err}");
                                        notify_error(window, cx, "Save failed", &err);
                                    }
                                }
                                window.close_dialog(cx);
                            }),
                    ),
            )
    });
}

// ---------------------------------------------------------------------------
// Actions
// ---------------------------------------------------------------------------

/// Trigger a browser download of the PNG via a transient `<a download>`.
fn save_download(png: &[u8]) -> Result<String, String> {
    let blob = make_png_blob(png)?;
    let url = web_sys::Url::create_object_url_with_blob(&blob)
        .map_err(|e| format!("object URL failed: {e:?}"))?;
    let filename = format!(
        "btc-orderflow_{}.png",
        chrono::Local::now().format("%Y-%m-%d_%H-%M-%S")
    );

    let document = web_sys::window()
        .and_then(|w| w.document())
        .ok_or("no document")?;
    let anchor: web_sys::HtmlAnchorElement = document
        .create_element("a")
        .map_err(|e| format!("anchor create failed: {e:?}"))?
        .dyn_into()
        .map_err(|_| "anchor cast failed".to_string())?;
    anchor.set_href(&url);
    anchor.set_download(&filename);
    anchor.click();
    let _ = web_sys::Url::revoke_object_url(&url);
    Ok(filename)
}

/// Write the PNG to the clipboard. The `ClipboardItem` is constructed and
/// `write()` is called synchronously inside the click gesture (Safari
/// rejects clipboard writes that start outside one); only the outcome is
/// awaited asynchronously for the toast.
fn copy_to_clipboard(png: Arc<Vec<u8>>, window: &mut Window, cx: &mut App) {
    let write_promise = (|| -> Result<js_sys::Promise, String> {
        let blob = make_png_blob(&png)?;
        let record = js_sys::Object::new();
        js_sys::Reflect::set(
            &record,
            &JsValue::from_str("image/png"),
            &js_sys::Promise::resolve(&blob),
        )
        .map_err(|e| format!("record build failed: {e:?}"))?;
        let item = web_sys::ClipboardItem::new_with_record_from_str_to_blob_promise(&record)
            .map_err(|e| format!("ClipboardItem failed: {e:?}"))?;
        let clipboard = web_sys::window().ok_or("no window")?.navigator().clipboard();
        Ok(clipboard.write(js_sys::Array::of1(&item).as_ref()))
    })();

    match write_promise {
        Ok(promise) => {
            window
                .spawn(cx, async move |cx| {
                    let result = JsFuture::from(promise).await;
                    cx.update(|window, cx| match result {
                        Ok(_) => notify_success(
                            window,
                            cx,
                            "Screenshot copied",
                            "Image copied to clipboard",
                        ),
                        Err(err) => {
                            log::error!("clipboard write rejected: {err:?}");
                            notify_error(
                                window,
                                cx,
                                "Copy failed",
                                "Your browser may not support image clipboard",
                            );
                        }
                    })
                    .ok();
                })
                .detach();
        }
        Err(err) => {
            log::error!("clipboard write setup failed: {err}");
            notify_error(
                window,
                cx,
                "Copy failed",
                "Your browser may not support image clipboard",
            );
        }
    }
}

/// Width/height from the PNG IHDR header (bytes 16..24, big-endian).
fn png_dimensions(png: &[u8]) -> Option<(u32, u32)> {
    if png.len() < 24 || &png[..8] != b"\x89PNG\r\n\x1a\n" || &png[12..16] != b"IHDR" {
        return None;
    }
    let w = u32::from_be_bytes(png[16..20].try_into().ok()?);
    let h = u32::from_be_bytes(png[20..24].try_into().ok()?);
    (w > 0 && h > 0).then_some((w, h))
}

fn make_png_blob(png: &[u8]) -> Result<web_sys::Blob, String> {
    let array = js_sys::Uint8Array::from(png);
    let parts = js_sys::Array::of1(&array);
    let options = web_sys::BlobPropertyBag::new();
    options.set_type("image/png");
    web_sys::Blob::new_with_u8_array_sequence_and_options(&parts, &options)
        .map_err(|e| format!("blob create failed: {e:?}"))
}

// ---------------------------------------------------------------------------
// Toasts — mirrors the notify helpers in `workspace.rs`.
// ---------------------------------------------------------------------------

fn notify_success(window: &mut Window, cx: &mut App, title: &str, body: &str) {
    window.push_notification(
        Notification::success(SharedString::from(body.to_string()))
            .title(SharedString::from(title.to_string())),
        cx,
    );
}

fn notify_error(window: &mut Window, cx: &mut App, title: &str, body: &str) {
    window.push_notification(
        Notification::error(SharedString::from(body.to_string()))
            .title(SharedString::from(title.to_string())),
        cx,
    );
}
