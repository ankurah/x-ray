//! Browser access, with an off-wasm stand-in.
//!
//! FOR: making the panel renderable without a browser. Every wasm-bindgen
//! import aborts the process with "cannot call wasm-bindgen imported functions
//! on non-wasm targets" when this crate is built for the host, so a single
//! `js_sys::Date::now()` in a feed row would make `cargo test` unable to draw
//! one card. Routing the browser touchpoints through here makes a host build
//! behave like a browser that has no window — a state every call site already
//! handles, because they all return `Option` or `Result` — so the panel renders
//! its empty states instead of dying.
//!
//! On `wasm32` each function below is the original call, nothing more. This is
//! a test seam, not a portability layer: the panel's deployment target is the
//! browser and always will be.

/// The browser window, or `None` when there is no browser.
#[cfg(target_arch = "wasm32")]
pub(crate) fn window() -> Option<web_sys::Window> { web_sys::window() }

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn window() -> Option<web_sys::Window> { None }

/// Wall-clock milliseconds since the epoch.
#[cfg(target_arch = "wasm32")]
pub(crate) fn now_ms() -> f64 { js_sys::Date::now() }

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn now_ms() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as f64).unwrap_or(0.0)
}

/// "15:04:07" for a timestamp in milliseconds — seconds matter in the feed.
#[cfg(target_arch = "wasm32")]
pub(crate) fn clock_hms(ts_ms: f64) -> String {
    let d = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(ts_ms));
    format!("{:02}:{:02}:{:02}", d.get_hours(), d.get_minutes(), d.get_seconds())
}

/// UTC rather than local time: there is no timezone database off-wasm, and the
/// only thing reading this without a browser is a test asserting a row drew.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn clock_hms(ts_ms: f64) -> String {
    let secs = (ts_ms / 1000.0) as i64;
    format!("{:02}:{:02}:{:02}", secs.div_euclid(3600) % 24, secs.div_euclid(60) % 60, secs.rem_euclid(60))
}

/// Run a future on the browser's event loop.
#[cfg(target_arch = "wasm32")]
pub(crate) fn spawn_local(future: impl std::future::Future<Output = ()> + 'static) { wasm_bindgen_futures::spawn_local(future) }

/// Off-wasm there is no event loop to run on, so the future is dropped un-run
/// and whatever it would have filled in stays at its loading state. Headless
/// tests drive the same work by awaiting it directly.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn spawn_local(future: impl std::future::Future<Output = ()> + 'static) { drop(future); }

/// A document-level `keydown` listener that unregisters when dropped.
///
/// Document-level and consuming (`preventDefault`) on purpose: the inspector
/// drawer is the innermost dismissable surface, and an app's own window-level
/// Escape handling should skip events the drawer already consumed. Document
/// listeners run before window ones, so one press closes only the drawer.
/// The JS closure, wrapped so it satisfies the `Send` bound Leptos puts on
/// cleanup closures. Sound: the panel runs on the browser's main thread only.
#[cfg(target_arch = "wasm32")]
type KeydownClosure = send_wrapper::SendWrapper<wasm_bindgen::closure::Closure<dyn FnMut(web_sys::KeyboardEvent)>>;

#[cfg(target_arch = "wasm32")]
pub(crate) struct DocumentKeydown(Option<KeydownClosure>);

#[cfg(target_arch = "wasm32")]
pub(crate) fn on_document_keydown(f: impl FnMut(web_sys::KeyboardEvent) + 'static) -> DocumentKeydown {
    use wasm_bindgen::JsCast;
    let closure = wasm_bindgen::closure::Closure::wrap(Box::new(f) as Box<dyn FnMut(web_sys::KeyboardEvent)>);
    if let Some(doc) = window().and_then(|w| w.document()) {
        let _ = doc.add_event_listener_with_callback("keydown", closure.as_ref().unchecked_ref());
    }
    DocumentKeydown(Some(send_wrapper::SendWrapper::new(closure)))
}

#[cfg(target_arch = "wasm32")]
impl Drop for DocumentKeydown {
    fn drop(&mut self) {
        use wasm_bindgen::JsCast;
        let Some(closure) = self.0.take() else { return };
        let closure = closure.take();
        if let Some(doc) = window().and_then(|w| w.document()) {
            let _ = doc.remove_event_listener_with_callback("keydown", closure.as_ref().unchecked_ref());
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) struct DocumentKeydown;

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn on_document_keydown(f: impl FnMut(web_sys::KeyboardEvent) + 'static) -> DocumentKeydown {
    drop(f);
    DocumentKeydown
}

/// The guard's whole contract is what happens when it drops, so it implements
/// `Drop` on both targets — off-wasm there was no document to register with,
/// so there is nothing to unregister.
#[cfg(not(target_arch = "wasm32"))]
impl Drop for DocumentKeydown {
    fn drop(&mut self) {}
}
