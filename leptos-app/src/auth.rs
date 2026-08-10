//! Client-side OIDC (Authorization Code + PKCE, public client) against idp.to,
//! plus the federate call to our own `/auth/session`.
//!
//! Two ways in, one exchange. [`start_sign_in`] hands the whole tab to idp.to
//! and gets it back at `/auth/callback`; [`begin_framed_sign_in`] puts the same
//! authorization request in a frame on the page the visitor is already looking
//! at, and idp.to's framed page posts the authorization response straight up to
//! this window (`web_message` delivery, their #93) — no callback navigation.
//! Both generate one PKCE verifier/challenge + `state` + `nonce` into
//! `sessionStorage`, and both finish in [`complete_sign_in`], which exchanges
//! the `code` for an `id_token` at idp.to's token endpoint and POSTs it to our
//! `/auth/session`, which validates it and mints an ankurah session token.
//!
//! The framed request is the special case in three respects only: it must start
//! at the property host (see [`FRAMED_AUTHORIZE_ENDPOINT`]) and carry an
//! `embed_origin` from [`EMBED_ORIGINS`], it is available on those origins
//! alone, and its result arrives as the frame's message (see
//! [`read_framed_message`]) rather than on a redirect. Everything else — the
//! parameters, the custody of the one-time material, the exchange, the
//! federate call, token storage — is the flow that was already here.
//!
//! No client secret and no server-side session: a static SPA does the whole
//! dance. All crypto here is pure-Rust (sha2) + the browser's CSPRNG (getrandom
//! "js"); the ankurah token is only ever *read* client-side.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::{spawn_local, JsFuture};
use web_sys::{window, Headers, MessageEvent, Request, RequestInit, Response, Storage, UrlSearchParams};

// --- idp.to public-client config (verified against their live discovery doc) ---
const CLIENT_ID: &str = "app_HsW5XyYWbr0KQrHZb5iejw";
const AUTHORIZE_ENDPOINT: &str = "https://id.idp.to/oidc/authorize";
const TOKEN_ENDPOINT: &str = "https://id.idp.to/oidc/token";
const DISCOVERY_ENDPOINT: &str = "https://id.idp.to/.well-known/openid-configuration";
/// Where a *framed* authorization request starts: idp.to's property host,
/// bypassing the discovery-advertised issuer above. The issuer deliberately
/// stays non-frameable, and Chromium checks framing policy on every hop of a
/// redirect chain — so a frame that began at the issuer would be refused
/// before it ever reached the property. Top-level sign-in keeps the issuer.
const FRAMED_AUTHORIZE_ENDPOINT: &str = "https://ankurah.login.idp.to/oidc/authorize";
/// The origins idp.to has registered as embedders of that property. Sending one
/// as `embed_origin` is what gets `frame-ancestors <that origin>` back instead
/// of `frame-ancestors 'none'`; the registry is exact, port included, which is
/// why the development entry names a port and why no other local port can be
/// substituted for it.
///
/// DEPLOYMENT CONSTANTS. What `embed_origin` says is "the page doing the
/// embedding is allowed to embed", so a value taken from a URL parameter, a
/// message, or a referrer would let the caller answer that question about
/// itself — [`registered_embed_origin`] therefore matches our own origin
/// against these literals and sends the literal, never the runtime string.
const EMBED_ORIGINS: [&str; 2] = ["https://community.ankurah.org", "http://127.0.0.1:5173"];
/// Discriminates idp.to's framed authorization result from every other message
/// the page might receive — the `web_message` envelope's published `type`
/// (their #93). The OAuth response itself rides in the message's `response`
/// member, verbatim: `code`/`state` on success, `error`/`error_description`/
/// `state` on a refusal. Only the single-use code ever travels, never a token.
const AUTHORIZATION_RESPONSE_TYPE: &str = "authorization_response";
/// The `type` of idp.to's frame-size report — `{type, height}`, posted by the
/// framed page whenever its layout changes so the embedder can size the frame
/// to the form instead of guessing. The literal spelling is their published
/// contract; listeners key on it exactly.
const EMBED_SIZE_TYPE: &str = "idp-embed-size";
/// The origin idp.to's framed messages arrive from: the property host that
/// serves [`FRAMED_AUTHORIZE_ENDPOINT`]. Two literals, one host — a
/// property-host rename must swap both together.
const FRAMED_MESSAGE_ORIGIN: &str = "https://ankurah.login.idp.to";
/// TRANSITION SHIM (see [`relay_callback_to_parent`]): the pre-#105 envelope
/// `type` that bundle's ceremonies key on. Deletes with the shim.
const CALLBACK_MESSAGE_TYPE: &str = "idp-auth-callback";
/// idp.to's account center for our directory (#36): where users manage their
/// name, passkeys, and recovery email. `return_to` brings them back to
/// Community — idp.to validates it against the domain's allowed return URLs, so
/// an un-allowlisted value simply drops the back-link (never an open redirect).
pub const ACCOUNT_CENTER_URL: &str =
    "https://ankurah.preferences.idp.to/account?return_to=https%3A%2F%2Fcommunity.ankurah.org";
/// The scopes we always request — `roles` included unconditionally: our server
/// requires the roles claim (strict mode), so a role-less token is useless and
/// degrading to one is never a fallback. If idp.to's role config ever
/// regresses, the authorize endpoint answers `invalid_scope`, which
/// `handle_callback` treats as retry-later.
const SCOPE: &str = "openid profile email roles";

// sessionStorage keys for one-time PKCE material (survives the redirect, not the tab).
const SS_VERIFIER: &str = "pkce_verifier";
const SS_STATE: &str = "oauth_state";
const SS_NONCE: &str = "oidc_nonce";
// localStorage key for the minted ankurah session token (survives reloads).
const LS_TOKEN: &str = "community_session_token";
// localStorage key for the idp.to id_token, retained ONLY to present as
// `id_token_hint` at RP-initiated logout — stored as a `RetainedIdToken` pair
// naming the session it belongs to, because this slot is shared across the
// origin's tabs and a concurrent sign-in overwrites it. Same custody tier as
// the session token above (it carries the same identity claims the session
// token does).
const LS_ID_TOKEN: &str = "community_id_token";

/// The callback path our SPA fallback serves (also a registered redirect_uri).
const CALLBACK_PATH: &str = "/auth/callback";

/// Our own server's guest mint (`server/src/guest.rs`).
const GUEST_MINT_PATH: &str = "/auth/guest";

/// The server's HTTP origin, for our own endpoints (`/auth/guest`,
/// `/auth/session`).
///
/// FOR the mobile shell: there the app is served out of the app bundle, so
/// the page origin (`capacitor://localhost`) names no server at all. The
/// same compile-time override that points the websocket at the real server
/// (`BACKEND_WS_URL` — `ws_url()` in main.rs reads it) is mapped to its HTTP
/// form here, so one knob moves both transports and they cannot skew. On the
/// web the override is absent and the page origin is the server, as before.
fn server_http_base() -> Option<String> {
    match option_env!("BACKEND_WS_URL") {
        Some(url) if !url.is_empty() => {
            Some(url.replacen("wss://", "https://", 1).replacen("ws://", "http://", 1))
        }
        _ => window()?.location().origin().ok(),
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    id_token: String,
}

#[derive(Deserialize)]
struct SessionResponse {
    token: String,
}

/// What one spent authorization code produced.
pub struct MintedSession {
    /// The ankurah session token — what the app runs on.
    pub token: String,
    /// The idp.to `id_token` the exchange retained for RP-initiated logout,
    /// handed back so a caller that decides this session is unwanted can undo
    /// exactly that write. See [`remove_id_token_if_matches`].
    pub id_token: String,
}

/// What sits in `LS_ID_TOKEN`: the retained id_token plus the `sub` (entity
/// id) of the ankurah session minted alongside it. The pairing is what lets
/// `sign_out` tell whether the hint it found belongs to the session it is
/// ending — without it, whichever tab signed in last owns the slot, and every
/// other session's sign-out presents (and deletes) a foreign hint.
#[derive(Serialize, Deserialize)]
struct RetainedIdToken {
    id_token: String,
    session_sub: String,
}

/// True when the app is currently loading the OIDC redirect landing page.
pub fn is_callback() -> bool {
    window()
        .and_then(|w| w.location().pathname().ok())
        .map(|p| p == CALLBACK_PATH)
        .unwrap_or(false)
}

/// Begin sign-in at the top level: generate PKCE + state + nonce, stash them,
/// and hand the tab to idp.to. Navigates away on success, so it only returns on
/// setup failure.
///
/// Every call regenerates the one-time values and overwrites the stash, so an
/// earlier abandoned attempt cannot spoil this one.
pub fn start_sign_in() -> Result<(), JsValue> {
    let window = window().ok_or_else(|| JsValue::from_str("no window"))?;
    let pending = stash_new_pending(&window)?;
    let auth_url = format!("{AUTHORIZE_ENDPOINT}?{}", authorize_query(&pending));
    window.location().assign(&auth_url)
}

/// A framed sign-in ready to be put on screen: the URL for the frame, and the
/// `state` the parent will hold until a result comes back claiming to be this
/// attempt's.
#[derive(Clone)]
pub struct FramedAttempt {
    pub authorize_url: String,
    pub state: String,
}

/// Begin sign-in in a frame: same one-time material as [`start_sign_in`], same
/// parameters, but addressed to the property host and carrying `embed_origin`
/// plus the sign-up launch mode (redirect, with our origin as the return leg).
/// Returns rather than navigating — the caller mounts the frame.
///
/// `Ok(None)` when idp.to does not frame for the origin we are served from. The
/// caller must take [`start_sign_in`] instead: a frame the browser refuses to
/// display reports nothing back, so attempting one here would show an empty box
/// and no reason for it. Nothing is stashed in that case — the top-level flow
/// generates its own material.
pub fn begin_framed_sign_in() -> Result<Option<FramedAttempt>, JsValue> {
    let Some(embed_origin) = registered_embed_origin() else { return Ok(None) };
    let window = window().ok_or_else(|| JsValue::from_str("no window"))?;
    let pending = stash_new_pending(&window)?;
    // Exactly one `embed_origin`: a duplicate or unlisted value is refused the
    // same way a missing one is.
    //
    // `signup_launch=redirect` + `return_url`: community's assigned launch
    // mode for the Sign Up affordance inside idp.to's framed page — clicking
    // it navigates THIS tab, top-level, to the sign-up page, and idp.to sends
    // the visitor back to `return_url` when sign-up completes, so the whole
    // round trip stays in what reads as one site. The return leg is what
    // makes redirect mode usable: if the value fails the property's return
    // allowlist, idp.to deliberately downgrades the launch to a popup rather
    // than strand this tab — an un-allowlisted origin costs us the assigned
    // mode, never the sign-up. We return to the origin we embed from; a
    // deeper return path would need its own allowlist entry.
    //
    // No `response_mode`: embedded requests default to `web_message` — the
    // framed page posts the authorization response to this window, where
    // [`read_framed_message`] claims it. (`response_mode=query` was the
    // pre-adoption pin, #103; dropping it was the adoption switch. Parents
    // still running that bundle are served by [`relay_callback_to_parent`]
    // until the shim retires.)
    let authorize_url = format!(
        "{FRAMED_AUTHORIZE_ENDPOINT}?{query}&embed_origin={embed}&signup_launch=redirect&return_url={ret}",
        query = authorize_query(&pending),
        embed = enc(embed_origin),
        ret = enc(embed_origin),
    );
    Ok(Some(FramedAttempt { authorize_url, state: pending.state }))
}

/// The `embed_origin` to send from the page we are on, or `None` when idp.to
/// has not registered this origin as an embedder.
///
/// The only read of the runtime origin in the framed path, and it selects
/// rather than derives: our own origin is compared for equality against
/// [`EMBED_ORIGINS`], and what a match returns is the matching literal, so the
/// bytes on the wire are always one of the two written above. An origin that
/// matches neither gets no frame at all.
fn registered_embed_origin() -> Option<&'static str> {
    let origin = window()?.location().origin().ok()?;
    EMBED_ORIGINS.into_iter().find(|registered| *registered == origin)
}

/// Discard whatever one-time material is currently stashed, so a result that
/// arrives afterwards finds no verifier and is refused rather than quietly
/// minting a session nobody is waiting for.
///
/// Blunt on purpose, and therefore only safe from a caller that knows no other
/// attempt can own the stash. Closing the ceremony is such a caller: the
/// sign-in button does nothing while a ceremony is up, and the card carries no
/// other control that starts an attempt, so whatever is stashed is the closed
/// attempt's own. (The card's retired top-level fallback button was the
/// exception — its [`start_sign_in`] stash sat exposed to a close for the
/// beat between stashing and its navigation committing. Re-adding any control
/// to the card that stashes fresh material re-opens that window.) A caller
/// that resumes after an await is NOT safe — by then its own material is long
/// consumed and anything present belongs to a later attempt.
///
/// The next attempt generates its own material, so this never blocks a retry.
pub fn cancel_pending_sign_in() {
    if let Some(ss) = session_storage() {
        let _ = ss.remove_item(SS_VERIFIER);
        let _ = ss.remove_item(SS_STATE);
        let _ = ss.remove_item(SS_NONCE);
    }
}

/// Withdraw the `id_token` one exchange retained, and only while the slot still
/// holds that exchange's value.
///
/// What this is for is the failed cancelled exchange. The unconditional removal
/// it replaced emptied the slot whatever had happened — including when the
/// exchange had failed and written nothing at all, so a cancel could take a
/// value the cancelled attempt never put there. Scoped to a compare, the `Err`
/// path touches the slot not at all.
///
/// Be exact about the cross-tab case, because a compare reads stronger than
/// this one is. [`complete_sign_in`] writes this slot on its way out of a
/// successful exchange (skipped only when the minted session token's `sub` is
/// unreadable, and then this exchange left nothing here to take back), and no
/// await separates that write from this call — so if another tab signed in
/// while the cancelled exchange was in flight, its pair is already overwritten
/// by the time the compare runs, and the compare then matches our own. The
/// compare bites only on a write landing inside that gap. What makes the
/// surviving overwrite harmless is the retention itself: the slot holds a
/// [`RetainedIdToken`], and `sign_out` spends a hint only for the session it
/// is ending, so no session's sign-out ever presents the value another
/// exchange left here.
pub fn remove_id_token_if_matches(expected: &str) {
    let Some(ls) = local_storage() else { return };
    let Some(raw) = ls.get_item(LS_ID_TOKEN).ok().flatten() else { return };
    // A non-pair value cannot be this exchange's write (an exchange writes
    // pairs), so a legacy bare token is left for sign_out's bridge to spend.
    let holds_ours = serde_json::from_str::<RetainedIdToken>(&raw)
        .map(|retained| retained.id_token == expected)
        .unwrap_or(false);
    if holds_ours {
        let _ = ls.remove_item(LS_ID_TOKEN);
    }
}

/// The one-time material for one authorization request, stashed and ready to
/// be spent by whichever callback context comes back.
struct PendingAuth {
    redirect_uri: String,
    state: String,
    nonce: String,
    challenge: String,
}

/// Generate PKCE verifier/challenge + `state` + `nonce` and stash the secrets
/// in `sessionStorage`, where they survive the redirect but not the tab.
fn stash_new_pending(window: &web_sys::Window) -> Result<PendingAuth, JsValue> {
    let origin = window.location().origin().map_err(|_| JsValue::from_str("no origin"))?;

    let verifier = random_b64url(32);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let state = random_b64url(16);
    let nonce = random_b64url(16);

    let ss = session_storage().ok_or_else(|| JsValue::from_str("sessionStorage unavailable"))?;
    ss.set_item(SS_VERIFIER, &verifier)?;
    ss.set_item(SS_STATE, &state)?;
    ss.set_item(SS_NONCE, &nonce)?;

    Ok(PendingAuth { redirect_uri: format!("{origin}{CALLBACK_PATH}"), state, nonce, challenge })
}

/// The authorization parameters, identical in both flows. `redirect_uri` is the
/// live origin's callback (production, or a registered loopback port in dev),
/// and idp.to matches it against the registered set in both legs — but only the
/// top-level flow ever lands on it. The framed flow's browser never goes there:
/// the value rides along as a matching token, required again at the token
/// endpoint, naming a place nothing navigates to.
fn authorize_query(pending: &PendingAuth) -> String {
    format!(
        "response_type=code&client_id={client}&redirect_uri={redirect}\
         &scope={scope}&state={state}&nonce={nonce}&code_challenge={challenge}&code_challenge_method=S256",
        client = enc(CLIENT_ID),
        redirect = enc(&pending.redirect_uri),
        scope = enc(SCOPE),
        state = enc(&pending.state),
        nonce = enc(&pending.nonce),
        challenge = enc(&pending.challenge),
    )
}

/// Complete a top-level callback: read `code`/`state` off the landing URL and
/// spend them. Returns the minted ankurah token.
pub async fn handle_callback() -> Result<String, String> {
    let window = window().ok_or("no window")?;
    let search = window.location().search().map_err(|_| "no query string")?;

    let params = UrlSearchParams::new_with_str(&search).map_err(|_| "malformed query string")?;

    if let Some(error) = params.get("error") {
        return Err(authorize_error_message(&error, &params.get("error_description").unwrap_or_default()));
    }

    let code = params.get("code").ok_or("callback missing `code`")?;
    let returned_state = params.get("state").ok_or("callback missing `state`")?;

    complete_sign_in(&code, &returned_state).await.map(|minted| minted.token)
}

/// TRANSITION SHIM — DELETE once #105 has been live for a release cycle (a
/// harness task tracks it; the steady-state framed flow never navigates
/// here). Serves one population: a parent page still running the pre-#105
/// bundle. That parent's framed attempt pinned `response_mode=query`, which
/// idp.to honors indefinitely, so its frame redirects to `/auth/callback` and
/// loads THIS bundle — a fresh document from the current deployment, not the
/// parent's. Without this branch, `handle_callback` would spend the code
/// right here and mount a second copy of the app inside the modal while the
/// parent waits on a message that never comes. Chat tabs stay open for days,
/// so that population drains slowly, not at deploy time.
///
/// Inside a frame, hand this callback's result to the page that framed it and
/// report that the app must not boot here. `false` at the top level, where
/// the caller carries on into [`handle_callback`]. Only the short-lived
/// authorization code and the returned `state` travel, in the pre-#105
/// envelope those parents key on; the parent holds the PKCE verifier and does
/// the exchange, so no token — idp.to's or ours — is ever put in a message, a
/// URL, or this frame's history.
pub fn relay_callback_to_parent() -> bool {
    let Some(window) = window() else { return false };
    let Some(parent) = embedding_parent(&window) else { return false };
    let Some(params) = window.location().search().ok().and_then(|search| UrlSearchParams::new_with_str(&search).ok())
    else {
        return false;
    };
    let field = |name: &str| params.get(name).filter(|value| !value.is_empty());

    if field("code").is_none() && field("error").is_none() {
        return false;
    }

    let message = js_sys::Object::new();
    let _ = js_sys::Reflect::set(&message, &JsValue::from_str("type"), &JsValue::from_str(CALLBACK_MESSAGE_TYPE));
    for name in ["code", "state", "error", "error_description"] {
        if let Some(value) = field(name) {
            let _ = js_sys::Reflect::set(&message, &JsValue::from_str(name), &JsValue::from_str(&value));
        }
    }

    // Addressed to our own origin, which is also the parent's: the callback and
    // the page that framed it are both served from here. A parent anywhere else
    // never receives this, whatever it claims to be.
    let origin = window.location().origin().unwrap_or_default();
    parent.post_message(&message, &origin).is_ok()
}

/// The window that framed this document, when there is one this document can
/// reach. `None` at the top level, and `None` inside a frame whose embedder is
/// another origin — `frameElement` is null there — which is the same answer for
/// the purpose at hand: no same-origin parent to hand a result to. (Part of
/// the transition shim above; deletes with it.)
fn embedding_parent(window: &web_sys::Window) -> Option<web_sys::Window> {
    window.frame_element().ok().flatten()?;
    window.parent().ok().flatten()
}

/// idp.to's `error` response, worded for the sign-in card. Shared by both
/// callback contexts so the ceremony explains a refusal exactly as the
/// top-level redirect does.
fn authorize_error_message(error: &str, description: &str) -> String {
    // `invalid_scope` means idp.to advertises the `roles` scope but hasn't
    // activated role configuration for this Application (or it regressed).
    // Degrading to a role-less request is pointless — the server requires
    // the roles claim — so this is a retry-later condition: the next
    // sign-in attempt re-reads discovery and asks again.
    if error == "invalid_scope" {
        return "idp.to has not finished activating roles for this application — try signing in again shortly".into();
    }
    format!("idp.to returned an error: {error} {description}")
}

/// Spend an authorization code: verify `state` against the stashed attempt,
/// exchange the code for an `id_token`, then federate it to our
/// `/auth/session`. Returns the minted ankurah token, and the `id_token` this
/// exchange retained on its way out.
///
/// The one code-to-session path in the client. The top-level callback reaches
/// it with values read from its own URL; the ceremony reaches it with values
/// idp.to's framed page posted up. Neither gets its own exchange.
pub async fn complete_sign_in(code: &str, returned_state: &str) -> Result<MintedSession, String> {
    let window = window().ok_or("no window")?;
    let origin = window.location().origin().map_err(|_| "no origin")?;

    let ss = session_storage().ok_or("sessionStorage unavailable")?;
    let saved_state = ss.get_item(SS_STATE).ok().flatten().ok_or("no saved state (stale callback?)")?;
    if returned_state != saved_state {
        return Err("state mismatch — possible CSRF, aborting".into());
    }
    let verifier = ss.get_item(SS_VERIFIER).ok().flatten().ok_or("no PKCE verifier (stale callback?)")?;
    // Required: the server refuses a session mint without the nonce (it is
    // what binds the id_token to THIS browser's sign-in attempt).
    let nonce = ss.get_item(SS_NONCE).ok().flatten().ok_or("no OIDC nonce (stale callback?)")?;

    // The one-time material is consumed by THIS callback — clear it now, not
    // only on success, so a failed exchange can't leave it behind for a stale
    // retry (every attempt regenerates it in `stash_new_pending`).
    let _ = ss.remove_item(SS_VERIFIER);
    let _ = ss.remove_item(SS_STATE);
    let _ = ss.remove_item(SS_NONCE);

    let redirect_uri = format!("{origin}{CALLBACK_PATH}");

    // 1) Exchange the authorization code for tokens (public client — no secret).
    let form = format!(
        "grant_type=authorization_code&code={code}&redirect_uri={redirect}&client_id={client}&code_verifier={verifier}",
        code = enc(code),
        redirect = enc(&redirect_uri),
        client = enc(CLIENT_ID),
        verifier = enc(&verifier),
    );
    let token_body =
        http_post(TOKEN_ENDPOINT, &form, "application/x-www-form-urlencoded").await.map_err(|e| e.to_string())?;
    // Parse failures render on screen in the sign-in error text, and a 200
    // body can still carry tokens — the id_token here, the minted session
    // token below — so keep serde's error and leave the body out; the
    // network tab has the full response when debugging.
    let tokens: TokenResponse = serde_json::from_str(&token_body).map_err(|e| format!("could not parse token response: {e}"))?;

    // 2) Federate: hand the ID token to our server, which validates + mints.
    let session_url = format!("{}/auth/session", server_http_base().ok_or("no origin")?);
    let session_req = serde_json::json!({ "id_token": tokens.id_token, "nonce": nonce });
    let session_body =
        http_post(&session_url, &session_req.to_string(), "application/json").await.map_err(|e| e.to_string())?;
    let session: SessionResponse = serde_json::from_str(&session_body).map_err(|e| format!("could not parse session response: {e}"))?;

    // Retain the id_token for RP-initiated logout (`id_token_hint`): it
    // proves to idp.to at sign-out time which client and user are asking.
    // Paired with the minted session's `sub` so sign-out can tell the hint is
    // its own — this slot is shared across tabs, and an unowned value would
    // hand another session's sign-out a foreign hint. If the fresh session
    // token's `sub` is unreadable (not expected), retain nothing: a hint
    // nobody can claim is the bug, not a fallback.
    // Custody note: it expires within the hour and sits beside the 12h
    // session token, which is the bigger prize for the same attacker.
    if let (Some(ls), Some(session_sub)) = (local_storage(), token_sub(&session.token)) {
        let retained = RetainedIdToken { id_token: tokens.id_token.clone(), session_sub };
        if let Ok(json) = serde_json::to_string(&retained) {
            let _ = ls.set_item(LS_ID_TOKEN, &json);
        }
    }

    Ok(MintedSession { token: session.token, id_token: tokens.id_token })
}

// --- the parent side of the ceremony ----------------------------------------

/// What a message the page received turned out to be.
pub enum FramedMessage {
    /// This attempt's authorization code, ready for [`complete_sign_in`].
    Accepted { code: String },
    /// This attempt came back as an idp.to error rather than a code.
    Failed(String),
    /// Not this attempt's result: from another origin, not the authorization
    /// response at all, carrying a `state` that does not match, or a second
    /// copy of one already taken.
    Ignored,
}

/// Check one message against the attempt the ceremony is waiting on.
///
/// Only the property host is listened to: a `web_message` result is the framed
/// document speaking, and that document is idp.to's — a message from any other
/// origin (our own included) is not the frame and is ignored. `expected_state`
/// is the attempt's `state`, and it is taken by the first message that matches
/// it — so a replay, or anything arriving once the ceremony has settled, finds
/// nothing to match and is ignored. The stashed `state` in `sessionStorage` is
/// checked again inside [`complete_sign_in`]; this check is what keeps an
/// unexpected message from starting an exchange at all.
pub fn read_framed_message(event: &MessageEvent, expected_state: &mut Option<String>) -> FramedMessage {
    if event.origin() != FRAMED_MESSAGE_ORIGIN {
        return FramedMessage::Ignored;
    }

    let data = event.data();
    if message_field(&data, "type").as_deref() != Some(AUTHORIZATION_RESPONSE_TYPE) {
        return FramedMessage::Ignored;
    }
    let Some(response) = object_field(&data, "response") else { return FramedMessage::Ignored };
    let Some(expected) = expected_state.as_deref() else { return FramedMessage::Ignored };
    if message_field(&response, "state").as_deref() != Some(expected) {
        return FramedMessage::Ignored;
    }
    *expected_state = None;

    if let Some(error) = message_field(&response, "error") {
        let description = message_field(&response, "error_description").unwrap_or_default();
        return FramedMessage::Failed(authorize_error_message(&error, &description));
    }
    match message_field(&response, "code") {
        Some(code) => FramedMessage::Accepted { code },
        None => FramedMessage::Failed("the sign-in frame came back with neither a code nor an error".into()),
    }
}

/// Read idp.to's frame-size report off one message: the reported height in CSS
/// pixels, when this is that report from the property host. The caller applies
/// its own bounds — the report is a measurement, not an instruction.
pub fn read_embed_size(event: &MessageEvent) -> Option<f64> {
    if event.origin() != FRAMED_MESSAGE_ORIGIN {
        return None;
    }
    let data = event.data();
    if message_field(&data, "type").as_deref() != Some(EMBED_SIZE_TYPE) {
        return None;
    }
    js_sys::Reflect::get(&data, &JsValue::from_str("height")).ok()?.as_f64().filter(|h| h.is_finite() && *h > 0.0)
}

/// Read one string member of a received message, treating absent and empty
/// alike — idp.to omits a member it has nothing for, and a member that arrived
/// empty says nothing either.
fn message_field(data: &JsValue, name: &str) -> Option<String> {
    js_sys::Reflect::get(data, &JsValue::from_str(name)).ok()?.as_string().filter(|value| !value.is_empty())
}

/// Read one object member of a received message — the envelope nests the OAuth
/// response one level down, and a missing or non-object member means this is
/// not that envelope.
fn object_field(data: &JsValue, name: &str) -> Option<JsValue> {
    js_sys::Reflect::get(data, &JsValue::from_str(name)).ok().filter(|value| value.is_object())
}

/// Why a guest mint produced no session — and the whole of what leaves
/// [`mint_guest_token`], on purpose.
///
/// FOR: this value is LOGGED rather than shown, so it must carry nothing that
/// does not belong in a log. A refused mint answers with a status, a URL and
/// a body; serde's parse error quotes the body it choked on. None of that is
/// ours to write down, and #84 already settled that for the sign-in path.
/// What triage actually needs is one bit — did the server answer — so that is
/// the whole type.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum GuestMintFailure {
    /// The server answered, and the answer was not a session: the mint budget
    /// refusing, the mint failing on its own side, or a body that did not
    /// parse. Whichever it was, the server's log already has the detail and
    /// this one does not need a copy.
    Refused,
    /// No answer at all — offline, name resolution, TLS, a blocked request.
    Unreachable,
}

impl GuestMintFailure {
    /// A stable word for the log. Non-identifying by construction: there are
    /// two of them and neither comes from the wire.
    pub fn reason(self) -> &'static str {
        match self {
            Self::Refused => "refused",
            Self::Unreachable => "unreachable",
        }
    }
}

/// Ask our own server for a read-only session: `POST /auth/guest`, no body, no
/// credential, no IdP round-trip. What comes back is an ankurah session token
/// signed by the same key `/auth/session` uses, carrying `roles=["guest"]` and
/// the literal `guest` as its subject.
///
/// NOTHING IS STORED. A guest token is minted per visit and left in memory
/// where the rest of the boot can reach it; the reason is that a member token
/// costs a ceremony to replace and this costs one unattended POST, so keeping
/// one would buy nothing and leave a session behind in a browser that had
/// closed the tab. A reload mints again, which is what the mint's budgets are
/// sized for (ten per address per minute — see `server/src/guest.rs`).
///
/// ONE CALL, NO RETRY, and the reason is worth stating because a retry looks
/// obviously right. Nothing presents this token at connect: ankurah's
/// websocket handshake carries no credential at all, and every request signs
/// itself with the claims of the context it runs through — so there is no
/// connect-time refusal to recover from, and a token minted here has two hours
/// before anything can reject it. What CAN reject it is a request made later,
/// under a tab left open past that; recovering there means calling this again
/// and setting the session pair the handshake reads (`ChatApp`), which is #86.
/// This function is what #86 calls.
pub async fn mint_guest_token() -> Result<String, GuestMintFailure> {
    let base = server_http_base().ok_or(GuestMintFailure::Unreachable)?;
    let body = http_post(&format!("{base}{GUEST_MINT_PATH}"), "", "application/json")
        .await
        .map_err(|failure| if failure.answered() { GuestMintFailure::Refused } else { GuestMintFailure::Unreachable })?;
    // Discarded rather than wrapped, and that is the point: serde's error
    // quotes the input it choked on, and the input here is a mint response.
    let session: SessionResponse = serde_json::from_str(&body).map_err(|_| GuestMintFailure::Refused)?;
    Ok(session.token)
}

/// Persist the minted ankurah token across reloads. Members only — a guest
/// token is deliberately never written anywhere (see [`mint_guest_token`]).
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

/// Sign out — of Community AND of idp.to (RP-initiated logout).
///
/// Local state goes first: whatever the IdP side does, this browser is signed
/// out of Community the moment the user clicks. Then, when idp.to advertises
/// an `end_session_endpoint` and the retained id_token belongs to the session
/// this tab is ending, navigate through it so the idp.to session actually
/// ends — otherwise the next "Sign in" click would silently re-admit without
/// a passkey touch. A hint some other session's sign-in retained stays in
/// storage for that session's own sign-out, and this one degrades to the
/// local-only path (the idp.to session standing at that point is the other
/// session's, not ours to end). Any discovery trouble degrades the same way
/// (reload to the sign-in screen, IdP session left standing).
pub fn sign_out() {
    // The session being ended is this tab's in-memory one — deliberately NOT
    // whatever `LS_TOKEN` holds: that slot is shared across tabs and
    // last-writer-wins, the same clobber the ownership check guards against.
    let live_sub = crate::AUTH_TOKEN.read().ok().and_then(|guard| guard.as_deref().and_then(token_sub));
    let id_token = match local_storage() {
        Some(ls) => {
            let _ = ls.remove_item(LS_TOKEN);
            take_id_token_if_owned(&ls, live_sub.as_deref())
        }
        None => None,
    };

    spawn_local(async move {
        let end_session = discovery_end_session_endpoint().await;
        let Some(w) = web_sys::window() else { return };
        let target = match (end_session, id_token) {
            (Some(endpoint), Some(id_token)) => {
                let origin = w.location().origin().unwrap_or_default();
                format!(
                    "{endpoint}?id_token_hint={hint}&post_logout_redirect_uri={redirect}",
                    hint = enc(&id_token),
                    redirect = enc(&format!("{origin}/")),
                )
            }
            // The degrade path: local state is already cleared, but the idp.to
            // session is left standing, so the next "Sign in" may re-admit
            // without a passkey. This is silent by nature — a reload to "/" —
            // and a silent no-op is exactly what hides a broken sign-out, so
            // name the reason. Two ways to land here: the discovery document
            // advertised no `end_session_endpoint` (nothing we can call), or
            // this tab held no id_token hint it owned (the pairing guards
            // against presenting another session's hint — see
            // `take_id_token_if_owned`; a reader holding several accounts can
            // reach this even with a live session).
            (None, _) => {
                tracing::warn!(
                    "sign-out: idp.to discovery advertises no end_session_endpoint; \
                     signed out locally only, the idp.to session is left standing"
                );
                "/".to_string()
            }
            (Some(_), None) => {
                tracing::warn!(
                    "sign-out: no owned id_token hint for this session; \
                     signed out locally only, the idp.to session is left standing"
                );
                "/".to_string()
            }
        };
        let _ = w.location().set_href(&target);
    });
}

/// Withdraw the retained id_token for use as `id_token_hint`, only when it
/// belongs to the session being ended. A pair some other session retained is
/// left where it is — its owner's sign-out still needs it, and removing it
/// here is exactly the cross-tab clobber the pairing exists to prevent. A
/// pre-pairing value (a bare JWT, from before `RetainedIdToken`) names no
/// owner; it is spent as-is, once — discarding it instead would cost every
/// already-signed-in browser the idp.to half of its next sign-out.
fn take_id_token_if_owned(ls: &Storage, live_sub: Option<&str>) -> Option<String> {
    let raw = ls.get_item(LS_ID_TOKEN).ok().flatten()?;
    match serde_json::from_str::<RetainedIdToken>(&raw) {
        Ok(retained) if live_sub == Some(retained.session_sub.as_str()) => {
            let _ = ls.remove_item(LS_ID_TOKEN);
            Some(retained.id_token)
        }
        Ok(_) => None,
        // Not a pair: a legacy bare id_token, or unrecognizable debris. Spend
        // the former; clear the latter rather than presenting junk to idp.to.
        Err(_) => {
            let _ = ls.remove_item(LS_ID_TOKEN);
            if token_sub(&raw).is_some() { Some(raw) } else { None }
        }
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
    let Some(value) = payload_json(token) else {
        return true;
    };
    // No `exp` → be lenient (our tokens always have one; this is only an optimization).
    let Some(exp) = value.get("exp").and_then(|v| v.as_f64()) else {
        return false;
    };
    (js_sys::Date::now() / 1000.0) + 30.0 >= exp
}

/// A JWT's payload as JSON — no signature check: these are client-side reads
/// of tokens we already hold (expiry optimization, ownership matching), never
/// trust decisions. The server is the enforcer.
fn payload_json(token: &str) -> Option<serde_json::Value> {
    let payload_b64 = token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload_b64).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// A JWT's `sub` claim. For the ankurah session token that is the user's
/// entity id — the name retention and sign-out agree on when deciding whom a
/// stored id_token belongs to.
fn token_sub(token: &str) -> Option<String> {
    Some(payload_json(token)?.get("sub")?.as_str()?.to_string())
}

/// What a POST did not do.
///
/// FOR: the two callers want different things out of a failure, and one of
/// them must not have what the other needs. The OIDC exchange puts its
/// failure in front of the visitor, where the status and the server's own
/// words are the whole value of it — [`Display`](std::fmt::Display) is that
/// sentence, unchanged from when this type was a `String`. The guest mint
/// shows nothing and LOGS instead, and a response body in the log is the same
/// mistake #84 fixed for parse failures. So it reads [`answered`] and never
/// the sentence.
struct PostFailure {
    /// The response status, when a response arrived at all. `None` means the
    /// request never got one: offline, name resolution, TLS, a blocked
    /// request, or a reply that was not a `Response`.
    status: Option<u16>,
    /// The sentence for a caller that shows one. Carries the URL and the
    /// response body, so a caller that only logs must read `status` instead.
    message: String,
}

impl PostFailure {
    /// A failure with no response behind it.
    fn unanswered(message: impl Into<String>) -> Self { Self { status: None, message: message.into() } }

    /// Whether the server answered at all. The one thing a caller may read
    /// without carrying a body along with it.
    fn answered(&self) -> bool { self.status.is_some() }
}

impl std::fmt::Display for PostFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str(&self.message) }
}

async fn http_post(url: &str, body: &str, content_type: &str) -> Result<String, PostFailure> {
    let unanswered = |e: JsValue| PostFailure::unanswered(js_err(e));
    let window = window().ok_or_else(|| PostFailure::unanswered("no window"))?;

    let opts = RequestInit::new();
    opts.set_method("POST");
    opts.set_body(&JsValue::from_str(body));
    let headers = Headers::new().map_err(unanswered)?;
    headers.set("Content-Type", content_type).map_err(unanswered)?;
    opts.set_headers(headers.as_ref());

    let request = Request::new_with_str_and_init(url, &opts).map_err(unanswered)?;
    let response_value = JsFuture::from(window.fetch_with_request(&request)).await.map_err(unanswered)?;
    let response: Response =
        response_value.dyn_into().map_err(|_| PostFailure::unanswered("fetch did not return a Response"))?;

    // Past here the server HAS answered, so every remaining failure carries
    // its status: a caller deciding "refused" against "never reached us" must
    // not read a body that failed to arrive as an absent server.
    let status = response.status();
    let answered = |e: JsValue| PostFailure { status: Some(status), message: js_err(e) };
    let text_js = JsFuture::from(response.text().map_err(answered)?).await.map_err(answered)?;
    let text = text_js.as_string().unwrap_or_default();

    if !response.ok() {
        return Err(PostFailure { status: Some(status), message: format!("HTTP {status} from {url}: {text}") });
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

/// Best-effort probe of idp.to's discovery doc for the RP-initiated-logout
/// endpoint. ANY failure — no window, network error, non-200, unparseable
/// body, or a missing member — returns `None`, and sign-out degrades to the
/// local-only path. This fetch must never break sign-out.
async fn discovery_end_session_endpoint() -> Option<String> {
    async fn probe() -> Result<Option<String>, String> {
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
        Ok(doc.end_session_endpoint.filter(|endpoint| !endpoint.is_empty()))
    }

    probe().await.unwrap_or_default()
}

/// Just the one field we need from the OIDC discovery document.
#[derive(Deserialize)]
struct DiscoveryDoc {
    #[serde(default)]
    end_session_endpoint: Option<String>,
}

