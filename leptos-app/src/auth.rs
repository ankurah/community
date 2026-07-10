//! Client-side OIDC (Authorization Code + PKCE, public client) against idp.to,
//! plus the federate call to our own `/auth/session`.
//!
//! Flow: [`start_sign_in`] generates a PKCE verifier/challenge + `state` +
//! `nonce`, stashes them in `sessionStorage`, and redirects to idp.to's
//! authorize endpoint. On return, the SPA lands on `/auth/callback`;
//! [`handle_callback`] verifies `state`, exchanges the `code` for an `id_token`
//! at idp.to's token endpoint, then POSTs it to our `/auth/session`, which
//! validates it and mints an ankurah session token.
//!
//! No client secret and no server-side session: a static SPA does the whole
//! dance. All crypto here is pure-Rust (sha2) + the browser's CSPRNG (getrandom
//! "js"); the ankurah token is only ever *read* client-side.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::{spawn_local, JsFuture};
use web_sys::{window, Headers, Request, RequestInit, Response, Storage, UrlSearchParams};

// --- idp.to public-client config (verified against their live discovery doc) ---
const CLIENT_ID: &str = "app_HsW5XyYWbr0KQrHZb5iejw";
const AUTHORIZE_ENDPOINT: &str = "https://id.idp.to/oidc/authorize";
const TOKEN_ENDPOINT: &str = "https://id.idp.to/oidc/token";
const DISCOVERY_ENDPOINT: &str = "https://id.idp.to/.well-known/openid-configuration";
/// Always-requested base scopes.
const BASE_SCOPE: &str = "openid profile email";
/// Opt-in scope that makes idp.to include the `roles` claim in the id_token.
/// Only requested when the discovery doc advertises it (see `start_sign_in`).
const ROLES_SCOPE: &str = "roles";

// sessionStorage keys for one-time PKCE material (survives the redirect, not the tab).
const SS_VERIFIER: &str = "pkce_verifier";
const SS_STATE: &str = "oauth_state";
const SS_NONCE: &str = "oidc_nonce";
// localStorage key for the minted ankurah session token (survives reloads).
const LS_TOKEN: &str = "community_session_token";

/// The callback path our SPA fallback serves (also a registered redirect_uri).
const CALLBACK_PATH: &str = "/auth/callback";

#[derive(Deserialize)]
struct TokenResponse {
    id_token: String,
}

#[derive(Deserialize)]
struct SessionResponse {
    token: String,
}

/// True when the app is currently loading the OIDC redirect landing page.
pub fn is_callback() -> bool {
    window()
        .and_then(|w| w.location().pathname().ok())
        .map(|p| p == CALLBACK_PATH)
        .unwrap_or(false)
}

/// Begin sign-in: generate PKCE + state + nonce, stash them, and redirect to
/// idp.to. This navigates away, so it does not return on success.
///
/// The `roles` scope is requested conditionally (see
/// [`discovery_supports_roles`]). Because that decision requires a discovery
/// fetch, the redirect happens asynchronously — the synchronous setup still
/// validates up front, but a returned `Ok(())` means "sign-in initiated",
/// not "already navigated".
pub fn start_sign_in() -> Result<(), JsValue> {
    let window = window().ok_or_else(|| JsValue::from_str("no window"))?;
    let origin = window.location().origin().map_err(|_| JsValue::from_str("no origin"))?;
    let redirect_uri = format!("{origin}{CALLBACK_PATH}");

    let verifier = random_b64url(32);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let state = random_b64url(16);
    let nonce = random_b64url(16);

    let ss = session_storage().ok_or_else(|| JsValue::from_str("sessionStorage unavailable"))?;
    ss.set_item(SS_VERIFIER, &verifier)?;
    ss.set_item(SS_STATE, &state)?;
    ss.set_item(SS_NONCE, &nonce)?;

    // The `roles` scope is opt-in: request it whenever idp.to's discovery doc
    // advertises it. Discovery is consulted off the critical path: a slow,
    // failed, or unparseable fetch must never block or break sign-in — it
    // simply means no roles scope on this attempt. This spawns and returns;
    // the redirect fires once the fetch settles.
    spawn_local(async move {
        let include_roles = discovery_supports_roles().await;
        let scope =
            if include_roles { format!("{BASE_SCOPE} {ROLES_SCOPE}") } else { BASE_SCOPE.to_string() };
        // `web_sys::window()` (not the shadowed local `window` binding above).
        if let Some(w) = web_sys::window() {
            let _ = redirect_to_authorize(&w, &redirect_uri, &scope, &state, &nonce, &challenge);
        }
    });

    Ok(())
}

/// Build the authorize URL and navigate to it. Navigates away on success.
fn redirect_to_authorize(
    window: &web_sys::Window,
    redirect_uri: &str,
    scope: &str,
    state: &str,
    nonce: &str,
    challenge: &str,
) -> Result<(), JsValue> {
    let auth_url = format!(
        "{AUTHORIZE_ENDPOINT}?response_type=code&client_id={client}&redirect_uri={redirect}\
         &scope={scope}&state={state}&nonce={nonce}&code_challenge={challenge}&code_challenge_method=S256",
        client = enc(CLIENT_ID),
        redirect = enc(redirect_uri),
        scope = enc(scope),
        state = enc(state),
        nonce = enc(nonce),
        challenge = enc(challenge),
    );

    window.location().assign(&auth_url)
}

/// Complete the callback: verify `state`, exchange the code for an `id_token`,
/// then federate it to our `/auth/session`. Returns the minted ankurah token.
pub async fn handle_callback() -> Result<String, String> {
    let window = window().ok_or("no window")?;
    let location = window.location();
    let origin = location.origin().map_err(|_| "no origin")?;
    let search = location.search().map_err(|_| "no query string")?;

    let params = UrlSearchParams::new_with_str(&search).map_err(|_| "malformed query string")?;

    if let Some(error) = params.get("error") {
        // `invalid_scope` means idp.to advertises the `roles` scope but hasn't
        // activated role configuration for this Application (or it regressed).
        // Degrading to a role-less request is pointless — the server requires
        // the roles claim — so this is a retry-later condition: the next
        // sign-in attempt re-reads discovery and asks again.
        if error == "invalid_scope" {
            return Err(
                "idp.to has not finished activating roles for this application — try signing in again shortly"
                    .into(),
            );
        }
        let desc = params.get("error_description").unwrap_or_default();
        return Err(format!("idp.to returned an error: {error} {desc}"));
    }

    let code = params.get("code").ok_or("callback missing `code`")?;
    let returned_state = params.get("state").ok_or("callback missing `state`")?;

    let ss = session_storage().ok_or("sessionStorage unavailable")?;
    let saved_state = ss.get_item(SS_STATE).ok().flatten().ok_or("no saved state (stale callback?)")?;
    if returned_state != saved_state {
        return Err("state mismatch — possible CSRF, aborting".into());
    }
    let verifier = ss.get_item(SS_VERIFIER).ok().flatten().ok_or("no PKCE verifier (stale callback?)")?;
    let nonce = ss.get_item(SS_NONCE).ok().flatten().unwrap_or_default();

    let redirect_uri = format!("{origin}{CALLBACK_PATH}");

    // 1) Exchange the authorization code for tokens (public client — no secret).
    let form = format!(
        "grant_type=authorization_code&code={code}&redirect_uri={redirect}&client_id={client}&code_verifier={verifier}",
        code = enc(&code),
        redirect = enc(&redirect_uri),
        client = enc(CLIENT_ID),
        verifier = enc(&verifier),
    );
    let token_body = http_post(TOKEN_ENDPOINT, &form, "application/x-www-form-urlencoded").await?;
    let tokens: TokenResponse =
        serde_json::from_str(&token_body).map_err(|e| format!("could not parse token response ({e}): {token_body}"))?;

    // 2) Federate: hand the ID token to our server, which validates + mints.
    let session_url = format!("{origin}/auth/session");
    let session_req = serde_json::json!({ "id_token": tokens.id_token, "nonce": nonce });
    let session_body = http_post(&session_url, &session_req.to_string(), "application/json").await?;
    let session: SessionResponse =
        serde_json::from_str(&session_body).map_err(|e| format!("could not parse session response ({e}): {session_body}"))?;

    // One-time material — don't leave it lying around.
    let _ = ss.remove_item(SS_VERIFIER);
    let _ = ss.remove_item(SS_STATE);
    let _ = ss.remove_item(SS_NONCE);

    Ok(session.token)
}

/// Persist the minted ankurah token across reloads.
pub fn store_token(token: &str) {
    if let Some(ls) = local_storage() {
        let _ = ls.set_item(LS_TOKEN, token);
    }
}

/// Restore a non-expired stored token, if any (discards an expired one).
pub fn stored_token() -> Option<String> {
    let ls = local_storage()?;
    let token = ls.get_item(LS_TOKEN).ok().flatten()?;
    if token_is_expired(&token) {
        let _ = ls.remove_item(LS_TOKEN);
        return None;
    }
    Some(token)
}

/// Sign out: drop the token and reload to the sign-in screen.
pub fn sign_out() {
    if let Some(ls) = local_storage() {
        let _ = ls.remove_item(LS_TOKEN);
    }
    if let Some(w) = window() {
        let _ = w.location().set_href("/");
    }
}

// --- helpers ---------------------------------------------------------------

/// URL-encode a query-string component.
fn enc(s: &str) -> String {
    js_sys::encode_uri_component(s).as_string().unwrap_or_default()
}

/// `n` CSPRNG bytes, base64url (no padding). 32 bytes → a 43-char PKCE verifier.
fn random_b64url(n: usize) -> String {
    let mut buf = vec![0u8; n];
    getrandom::getrandom(&mut buf).expect("browser CSPRNG unavailable");
    URL_SAFE_NO_PAD.encode(&buf)
}

/// Client-side expiry check on the ankurah token (server still enforces the
/// real expiry). Reads `exp` from the JWT payload; a 30s leeway avoids using a
/// token that expires mid-request. Unparseable → treat as expired.
fn token_is_expired(token: &str) -> bool {
    let Some(payload_b64) = token.split('.').nth(1) else {
        return true;
    };
    let Ok(bytes) = URL_SAFE_NO_PAD.decode(payload_b64) else {
        return true;
    };
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return true;
    };
    // No `exp` → be lenient (our tokens always have one; this is only an optimization).
    let Some(exp) = value.get("exp").and_then(|v| v.as_f64()) else {
        return false;
    };
    (js_sys::Date::now() / 1000.0) + 30.0 >= exp
}

async fn http_post(url: &str, body: &str, content_type: &str) -> Result<String, String> {
    let window = window().ok_or("no window")?;

    let opts = RequestInit::new();
    opts.set_method("POST");
    opts.set_body(&JsValue::from_str(body));
    let headers = Headers::new().map_err(js_err)?;
    headers.set("Content-Type", content_type).map_err(js_err)?;
    opts.set_headers(headers.as_ref());

    let request = Request::new_with_str_and_init(url, &opts).map_err(js_err)?;
    let response_value = JsFuture::from(window.fetch_with_request(&request)).await.map_err(js_err)?;
    let response: Response = response_value.dyn_into().map_err(|_| "fetch did not return a Response".to_string())?;

    let text_js = JsFuture::from(response.text().map_err(js_err)?).await.map_err(js_err)?;
    let text = text_js.as_string().unwrap_or_default();

    if !response.ok() {
        return Err(format!("HTTP {} from {url}: {text}", response.status()));
    }
    Ok(text)
}

fn js_err(v: JsValue) -> String {
    v.as_string().unwrap_or_else(|| format!("{v:?}"))
}

fn session_storage() -> Option<Storage> {
    window()?.session_storage().ok().flatten()
}

fn local_storage() -> Option<Storage> {
    window()?.local_storage().ok().flatten()
}

/// Best-effort probe of idp.to's discovery doc for the `roles` scope. ANY
/// failure — no window, network error, non-200, unparseable body, or a missing
/// `scopes_supported` — returns `false`, so sign-in proceeds without the scope.
/// This fetch must never break sign-in.
async fn discovery_supports_roles() -> bool {
    async fn probe() -> Result<bool, String> {
        let window = window().ok_or("no window")?;

        let opts = RequestInit::new();
        opts.set_method("GET");
        let request = Request::new_with_str_and_init(DISCOVERY_ENDPOINT, &opts).map_err(js_err)?;

        let response_value = JsFuture::from(window.fetch_with_request(&request)).await.map_err(js_err)?;
        let response: Response =
            response_value.dyn_into().map_err(|_| "fetch did not return a Response".to_string())?;
        if !response.ok() {
            return Err(format!("discovery HTTP {}", response.status()));
        }

        let text_js = JsFuture::from(response.text().map_err(js_err)?).await.map_err(js_err)?;
        let text = text_js.as_string().unwrap_or_default();
        let doc: DiscoveryDoc = serde_json::from_str(&text).map_err(|e| e.to_string())?;
        Ok(doc.scopes_supported.iter().any(|s| s == ROLES_SCOPE))
    }

    probe().await.unwrap_or(false)
}

/// Just the one field we need from the OIDC discovery document.
#[derive(Deserialize)]
struct DiscoveryDoc {
    #[serde(default)]
    scopes_supported: Vec<String>,
}

