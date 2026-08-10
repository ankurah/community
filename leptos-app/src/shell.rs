//! Whether this client is running inside the mobile shell, and the native
//! calls the shell offers it.
//!
//! FOR: the same wasm bundle is the web app and the app inside the iOS shell,
//! and three things a browser tab does are wrong in an app — signing in
//! through a frame the app's web view can never give a credential to, sending
//! a member to idp.to by replacing the app with it, and offering a QR code
//! whose whole purpose is "open this on your phone". This module is what the
//! rest of the client asks before taking any of those paths.
//!
//! DETECTION IS THE GLOBAL, and nothing else. `shell.js` defines
//! `window.__ankurahShell` only where Capacitor's injected runtime says this
//! is the app; in a browser the global never appears, [`is_shell`] is false
//! for the life of the document, and every branch below stays dormant. The
//! answer is resolved once because it cannot change under a running document.

use std::sync::OnceLock;

use js_sys::{Function, Object, Promise, Reflect};
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::{spawn_local, JsFuture};
use web_sys::window;

/// The global `shell.js` defines inside the app.
const SHELL_GLOBAL: &str = "__ankurahShell";

/// The `code` the sign-in sheet's rejection carries when the member dismissed
/// it rather than finishing. SHARED LITERAL: `AuthSessionPlugin.swift` writes
/// this word, and a rename there is a rename here.
const CANCELLED_CODE: &str = "cancelled";

/// How a run of the shell's sign-in sheet ended.
pub enum SheetOutcome {
    /// The URL the sheet was redirected to — the authorization response, as
    /// query parameters on the app's own scheme.
    Returned(String),
    /// The member dismissed the sheet. Not a failure, and the caller has
    /// nothing to report beyond the fact that no sign-in happened.
    Cancelled,
    /// The sheet could not run, or ended without a callback. The sentence is
    /// for the sign-in card.
    Failed(String),
}

/// Whether this document is the mobile shell's.
pub fn is_shell() -> bool {
    static IS_SHELL: OnceLock<bool> = OnceLock::new();
    *IS_SHELL.get_or_init(|| shell().is_some())
}

/// Open the system sign-in sheet on an authorization URL and wait for what it
/// catches on `callback_scheme`.
///
/// The sheet runs on the browser's own credential surface, so the passkey
/// prompt and any standing idp.to session are the ones the member already has.
/// This client never sees inside it: what comes back is the redirect URL and
/// nothing else.
pub async fn start_auth_session(url: &str, callback_scheme: &str) -> SheetOutcome {
    let Some((shell, start)) = shell_method("startAuthSession") else {
        return SheetOutcome::Failed("this app cannot open a sign-in sheet".into());
    };
    let call = start.call2(&shell, &JsValue::from_str(url), &JsValue::from_str(callback_scheme));
    let promise = match call.and_then(|value| value.dyn_into::<Promise>()) {
        Ok(promise) => promise,
        Err(e) => return SheetOutcome::Failed(js_message(&e)),
    };

    match JsFuture::from(promise).await {
        Ok(value) => match value.as_string() {
            Some(callback) => SheetOutcome::Returned(callback),
            None => SheetOutcome::Failed("the sign-in sheet came back with no callback URL".into()),
        },
        Err(rejection) if field(&rejection, "code").as_deref() == Some(CANCELLED_CODE) => SheetOutcome::Cancelled,
        Err(rejection) => SheetOutcome::Failed(js_message(&rejection)),
    }
}

/// Put a page in the system browser, on top of the app.
///
/// For the pages that belong to idp.to rather than to us — account settings
/// and the end-session URL. Both need the browser session the sign-in sheet
/// established, and neither may replace the app: a web view navigated to
/// idp.to is an app with no way back.
///
/// Fire-and-forget by design. Nothing downstream waits on the sheet appearing,
/// and a member who never comes back from it has already had whatever local
/// state the caller cleared before calling — so a failure is logged and the
/// caller carries on.
pub fn open_external(url: &str) {
    let Some((shell, open)) = shell_method("openExternal") else {
        tracing::error!("this app cannot open a browser sheet; nothing was opened");
        return;
    };
    match open.call1(&shell, &JsValue::from_str(url)).and_then(|value| value.dyn_into::<Promise>()) {
        Ok(promise) => spawn_local(async move {
            if let Err(e) = JsFuture::from(promise).await {
                tracing::error!("the browser sheet did not open: {}", js_message(&e));
            }
        }),
        Err(e) => tracing::error!("the browser sheet did not open: {}", js_message(&e)),
    }
}

/// Ask the browser to stop treating this origin's storage as disposable.
///
/// FOR: the local ankurah node lives in IndexedDB, and inside the app that
/// store is the member's own history rather than a cache of a site they
/// visited — an eviction under storage pressure costs them a full resync with
/// nothing said about it. Best-effort by nature: the answer is the browser's,
/// there is no fallback if it says no, and nothing here waits for it.
pub fn request_persistent_storage() {
    let Some(window) = window() else { return };
    let Ok(promise) = window.navigator().storage().persist() else { return };
    spawn_local(async move {
        let _ = JsFuture::from(promise).await;
    });
}

/// The shell global, when the page has one.
fn shell() -> Option<Object> {
    let window = window()?;
    Reflect::get(window.as_ref(), &JsValue::from_str(SHELL_GLOBAL)).ok()?.dyn_into::<Object>().ok()
}

/// One of the shell's calls, with the object to invoke it on.
fn shell_method(name: &str) -> Option<(Object, Function)> {
    let shell = shell()?;
    let function = Reflect::get(shell.as_ref(), &JsValue::from_str(name)).ok()?.dyn_into::<Function>().ok()?;
    Some((shell, function))
}

/// One string member of a JS value, absent and empty alike.
fn field(value: &JsValue, name: &str) -> Option<String> {
    Reflect::get(value, &JsValue::from_str(name)).ok()?.as_string().filter(|value| !value.is_empty())
}

/// What a rejected native call says, for the sign-in card. The plugin writes
/// fixed sentences, so this carries no response body and no token.
fn js_message(value: &JsValue) -> String {
    field(value, "message").or_else(|| value.as_string()).unwrap_or_else(|| format!("{value:?}"))
}
