//! Client-side OIDC (Authorization Code + PKCE, public client) against idp.to,
//! plus the federate call to our own `/auth/session`.
//!
//! Two ways in, one exchange. [`start_sign_in`] hands the whole tab to idp.to
//! and gets it back at `/auth/callback`; [`begin_framed_sign_in`] puts the same
//! authorization request in a frame on the page the visitor is already looking
//! at, and the callback — same-origin, so it can talk to us — hands the code
//! back with `postMessage`. Both generate one PKCE verifier/challenge +
//! `state` + `nonce` into `sessionStorage`, and both finish in
//! [`complete_sign_in`], which exchanges the `code` for an `id_token` at
//! idp.to's token endpoint and POSTs it to our `/auth/session`, which validates
//! it and mints an ankurah session token.
//!
//! The framed request is the special case in two respects only: it must start
//! at the property host (see [`FRAMED_AUTHORIZE_ENDPOINT`]) and carry an
//! `embed_origin` from [`EMBED_ORIGINS`], and it is available on those origins
//! alone. Everything else — the parameters, the custody of the one-time
//! material, the exchange, the federate call, token storage — is the flow that
//! was already here.
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
/// Discriminates the framed callback's `postMessage` from every other message
/// the page might receive.
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
/// This is also the ceremony's escape hatch. A frame that the browser refuses
/// to display raises no event the parent can hear, so the visitor must always
/// have this within reach; taking it regenerates every one-time value, which is
/// why an abandoned framed attempt cannot spoil the next try.
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
/// parameters, but addressed to the property host and carrying `embed_origin`.
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
    let authorize_url = format!(
        "{FRAMED_AUTHORIZE_ENDPOINT}?{query}&embed_origin={embed}",
        query = authorize_query(&pending),
        embed = enc(embed_origin),
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
/// attempt can own the stash. Closing the ceremony is very nearly such a
/// caller: the sign-in button does nothing while a ceremony is up, so no
/// successor can be started from there. A caller that resumes after an await is
/// NOT — by then its own material is long consumed and anything present belongs
/// to a later attempt.
///
/// The stash that precondition misses sits right beside the caller. The
/// ceremony's escape hatch calls [`start_sign_in`], which stashes the top-level
/// attempt's material and then only *begins* a cross-origin navigation: the
/// document stays interactive until that navigation commits, with the × and
/// Escape still live. A close inside that window clears material the visitor is
/// seconds from spending at idp.to, and they come back to "no saved state
/// (stale callback?)". The window is narrow and predates the ceremony's cancel
/// handling — the same clear ran from the same call site before any of it — so
/// it is left standing rather than patched around here. The durable fix is for
/// the escape to mark the stash as handed off, so a later close skips the
/// clear. Written down rather than left for the next caller to rediscover from
/// a rule that does not quite hold.
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
/// live origin's callback (production, or a registered loopback port in dev) —
/// unlike `embed_origin`, it has to name where the browser actually is, and
/// idp.to matches it against the registered set.
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

/// Inside a frame, hand this callback's result to the page that framed it and
/// report that the app must not boot here. `false` at the top level, where the
/// caller carries on into [`handle_callback`].
///
/// Only a real authorization result is carried, and only when it can actually
/// be delivered. Returning `true` suppresses the app in this document for good,
/// so both of those refusals fall through to [`handle_callback`] instead — but
/// they land in different places, and the second is worth naming.
///
/// A `/auth/callback` framed with no `code` and no `error` gets the answer that
/// already existed for a callback carrying nothing. A message the parent
/// refuses is uglier: `handle_callback` then spends a real code right here,
/// mounting a second copy of the app inside the frame. What it mints is not
/// stranded — the frame is same-origin, so the session lands in exactly the
/// storage the parent reads on its next load — but until something reloads, the
/// visitor is looking at the app in a modal-sized box. That is still the better
/// of the two failures: the alternative is a blank frame and a parent waiting
/// on a message that never arrived. And it is close to unreachable, since a
/// same-origin post addressed to our own origin does not fail.
///
/// Only the short-lived authorization code and the returned `state` travel. The
/// page that framed this one holds the PKCE verifier and does the exchange, so
/// no token — idp.to's or ours — is ever put in a message, a URL, or this
/// frame's history.
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
/// the purpose at hand: no same-origin parent to hand a result to.
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
/// it with values read from its own URL; the ceremony reaches it with values a
/// framed callback sent its parent. Neither gets its own exchange.
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
    let token_body = http_post(TOKEN_ENDPOINT, &form, "application/x-www-form-urlencoded").await?;
    // Parse failures render on screen in the sign-in error text, and a 200
    // body can still carry tokens — the id_token here, the minted session
    // token below — so keep serde's error and leave the body out; the
    // network tab has the full response when debugging.
    let tokens: TokenResponse = serde_json::from_str(&token_body).map_err(|e| format!("could not parse token response: {e}"))?;

    // 2) Federate: hand the ID token to our server, which validates + mints.
    let session_url = format!("{origin}/auth/session");
    let session_req = serde_json::json!({ "id_token": tokens.id_token, "nonce": nonce });
    let session_body = http_post(&session_url, &session_req.to_string(), "application/json").await?;
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
    /// Not this attempt's result: from another origin, not the callback's
    /// message at all, carrying a `state` that does not match, or a second copy
    /// of one already taken.
    Ignored,
}

/// Check one message against the attempt the ceremony is waiting on.
///
/// `expected_state` is the attempt's `state`, and it is taken by the first
/// message that matches it — so a replay, or anything arriving once the
/// ceremony has settled, finds nothing to match and is ignored. The stashed
/// `state` in `sessionStorage` is checked again inside [`complete_sign_in`];
/// this check is what keeps an unexpected message from starting an exchange at
/// all.
pub fn read_framed_message(event: &MessageEvent, expected_state: &mut Option<String>) -> FramedMessage {
    let Some(origin) = window().and_then(|w| w.location().origin().ok()) else { return FramedMessage::Ignored };
    if event.origin() != origin {
        return FramedMessage::Ignored;
    }

    let data = event.data();
    if message_field(&data, "type").as_deref() != Some(CALLBACK_MESSAGE_TYPE) {
        return FramedMessage::Ignored;
    }
    let Some(expected) = expected_state.as_deref() else { return FramedMessage::Ignored };
    if message_field(&data, "state").as_deref() != Some(expected) {
        return FramedMessage::Ignored;
    }
    *expected_state = None;

    if let Some(error) = message_field(&data, "error") {
        let description = message_field(&data, "error_description").unwrap_or_default();
        return FramedMessage::Failed(authorize_error_message(&error, &description));
    }
    match message_field(&data, "code") {
        Some(code) => FramedMessage::Accepted { code },
        None => FramedMessage::Failed("the sign-in frame came back with neither a code nor an error".into()),
    }
}

/// Read one string member of a received message, treating absent and empty
/// alike — the relay omits a parameter it did not find, and a member that
/// arrived empty says nothing either.
fn message_field(data: &JsValue, name: &str) -> Option<String> {
    js_sys::Reflect::get(data, &JsValue::from_str(name)).ok()?.as_string().filter(|value| !value.is_empty())
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
pub async fn mint_guest_token() -> Result<String, String> {
    let window = window().ok_or("no window")?;
    let origin = window.location().origin().map_err(|_| "no origin")?;
    let body = http_post(&format!("{origin}{GUEST_MINT_PATH}"), "", "application/json").await?;
    // Keep serde's error and leave the body out: a 200 from either mint route
    // carries a session token, and neither belongs in a message on screen.
    let session: SessionResponse =
        serde_json::from_str(&body).map_err(|e| format!("could not parse the guest session response: {e}"))?;
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
            _ => "/".to_string(),
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

