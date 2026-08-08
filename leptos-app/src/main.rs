use leptos::prelude::*;

use std::collections::HashMap;

use ankurah::{Context, EntityId, LiveQuery, Node};
use ankurah_chat_leptos::{ChatContext, DmConversation, RoomLog};
use ankurah_jwt_auth::{parse_claims_unverified, JwtAgent, JwtContext};
use ankurah_signals::{CurrentObserver, Get as AnkurahGet, ReactiveGraphObserver};
use ankurah_storage_indexeddb_wasm::IndexedDBStorageEngine;
use ankurah_websocket_client_wasm::WebsocketClient;
use community_model::{LinkPreviewView, UserView};
use lazy_static::lazy_static;
use send_wrapper::SendWrapper;
use std::cell::Cell;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, RwLock};
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::{spawn_local, JsFuture};
use web_sys::window;

mod auth;
mod ban_lock;
mod chat_hooks;
mod editable_text_field;
mod header;
mod link_preview;
mod members_panel;
mod mod_log_panel;
mod notification_inbox;
mod notification_manager;
mod panels;
mod profile_popover;
mod qr_code_modal;
mod room_topic;
mod sidebar;
mod sign_in_ceremony;
mod user_detail_panel;
mod xray;

// The chat surfaces bring these with them, and community's own chrome uses
// them too: a member row in the panels has to colour the same person the same
// way a message row does, and everything that builds an AnkQL predicate should
// build it the one safe way. Re-exported at the crate root so `crate::fmt`,
// `crate::queries`, `crate::query_registry` and `crate::dm` keep resolving.
pub use ankurah_chat_leptos::{dm, fmt, queries, query_registry};

use header::Header;
use notification_manager::NotificationManager;
use profile_popover::ProfilePopover;
use sidebar::Sidebar;

lazy_static! {
    static ref NODE: OnceLock<Node<IndexedDBStorageEngine, JwtAgent>> = OnceLock::new();
    static ref CLIENT: OnceLock<SendWrapper<WebsocketClient>> = OnceLock::new();
    /// The ephemeral policy agent. Set only once the boot has waited it out,
    /// so a reader of this global gets an agent whose policy has arrived;
    /// x-ray's node card reports `policy_ready` from here.
    static ref AGENT: OnceLock<JwtAgent> = OnceLock::new();
    /// The minted ankurah session token (present once signed in).
    static ref AUTH_TOKEN: RwLock<Option<String>> = RwLock::new(None);
    /// A sign-in failure from the OIDC callback, surfaced on the sign-in card.
    /// Set (at most once) during `initialize`, before Leptos mounts, so plain
    /// storage suffices — no signal needed.
    static ref AUTH_ERROR: RwLock<Option<String>> = RwLock::new(None);
}

/// Whether the boot produced a session that can actually read: a token, a node
/// built on storage the browser allowed, a websocket that joined the remote
/// system, and the server's policy synced into the local agent.
///
/// FOR: `ctx()` and everything under `ChatApp` assume all four, and until this
/// existed the only thing checked was the token — so a browser that refused
/// IndexedDB, a websocket that never reached the server, or a policy row that
/// never arrived each mounted a UI that could not read and then died in an
/// `.expect`. `App` gates on this instead, and a `false` sends the visitor to
/// the card with a sentence. Single-threaded wasm, so `Relaxed` is the whole
/// of the ordering question.
static SESSION_LIVE: AtomicBool = AtomicBool::new(false);

/// How long to wait for the node to join the remote system before giving up.
///
/// It has to cover a cold TCP + TLS + websocket handshake and one round trip
/// on a slow mobile connection, because the cost of being too eager is telling
/// a visitor whose connection was merely slow to reload. Eight seconds is the
/// shortest ceiling that comfortably does; the cost of being too patient is
/// only that a visitor whose websocket is going nowhere waits that long for
/// the card.
const SYSTEM_JOIN_TIMEOUT_MS: i32 = 8_000;

/// How long to wait for the durable node's policy row to reach the ephemeral
/// agent. Shorter than the join above because the socket is already up by
/// then: this is one entity arriving over a live connection.
const POLICY_TIMEOUT_MS: i32 = 5_000;

/// How often the two waits above look. Also the latency each adds to a boot
/// that succeeds, which is why it is small.
const POLL_STEP_MS: i32 = 50;

/// Get the global authenticated Ankurah context. Only called from within the
/// signed-in UI subtree (`ChatApp`), so the token/node are guaranteed present.
pub fn ctx() -> Context {
    let token = AUTH_TOKEN.read().expect("auth token lock poisoned").clone().expect("not authenticated");
    let claims = parse_claims_unverified(&token).expect("stored token is a valid JWT");
    NODE.get()
        .expect("Node not initialized")
        .context(JwtContext::from_claims(claims, token))
        .expect("failed to create authenticated context")
}

/// Get the global WebSocket client.
pub fn ws_client() -> WebsocketClient {
    (**CLIENT.get().expect("Client not initialized")).clone()
}

/// The subject every guest token carries, as the server writes it
/// (`server/src/guest.rs`, `GUEST_SUB`). A literal, never an entity id: a
/// guest names no `User` row, so nothing keyed on identity can fire for one.
const GUEST_SUB: &str = "guest";

/// Who is reading, as the session token says: the `User` entity id a member's
/// token names, or `None` for a guest.
///
/// FOR: everything downstream that has to know whether there is somebody to
/// attribute a read or a write to — the pair handed to the chat handshake, the
/// mount gates on the member-only surfaces, the author of a moderation record.
/// It answers `None` rather than panicking because every one of those callers
/// is a rendering path or a gate, where "nobody is signed in" is both a real
/// answer and the safe one.
///
/// Three ways to get `None`, and only the last is a fault: no session at all,
/// a guest session, or a token whose subject is neither the guest literal nor
/// a parseable id. The third is logged and read as anonymous — the alternative
/// was `.expect`, which took the whole app down with it.
pub fn viewer() -> Option<EntityId> {
    let guard = AUTH_TOKEN.read().ok()?;
    let token = guard.as_deref()?;
    let claims = parse_claims_unverified(token).ok()?;
    if claims.sub == GUEST_SUB {
        return None;
    }
    match EntityId::from_base64(&claims.sub) {
        Ok(id) => Some(id),
        Err(e) => {
            tracing::error!("the session token's subject is neither the guest literal nor an entity id: {e}");
            None
        }
    }
}

/// The reader's roles, as carried by the session token. Roles are managed by
/// the IdP and arrive as lowercase stable keys ("member", "moderator",
/// "admin"); a guest session carries the single key "guest", which the server
/// mints and `policy.json` grants the anonymous `view` tier. UI gating only —
/// the server enforces the real policy at token mint and on every read/write.
/// Any failure yields an empty Vec, which reads as "no privileges".
pub fn current_user_roles() -> Vec<String> {
    let Ok(guard) = AUTH_TOKEN.read() else { return Vec::new() };
    let Some(token) = guard.as_deref() else { return Vec::new() };
    parse_claims_unverified(token).map(|claims| claims.roles).unwrap_or_default()
}

/// Whether the signed-in user holds a moderation-capable role (UI gating only).
pub fn can_moderate() -> bool { current_user_roles().iter().any(|r| r == "moderator" || r == "admin") }

fn main() {
    console_error_panic_hook::set_once();
    tracing_wasm::set_as_global_default_with_config(
        tracing_wasm::WASMLayerConfigBuilder::new()
            .set_max_level(tracing::Level::INFO) // Only show INFO, WARN, ERROR
            .build(),
    );

    // Resolve auth, connect (if signed in), then mount Leptos.
    spawn_local(initialize());
}

async fn initialize() {
    // Resolve the session token: either finish an OIDC callback, or restore one.
    if auth::is_callback() {
        // Inside a sign-in frame this document is a courier, not the app: hand
        // the code and state to the page that framed it and stop here. Nothing
        // mounts and nothing connects — that page holds the PKCE verifier, does
        // the exchange, and takes the frame down afterwards.
        if auth::relay_callback_to_parent() {
            return;
        }
        match auth::handle_callback().await {
            Ok(token) => {
                auth::store_token(&token);
                *AUTH_TOKEN.write().unwrap() = Some(token);
            }
            Err(e) => {
                tracing::error!("OIDC sign-in failed: {}", e);
                *AUTH_ERROR.write().unwrap() = Some(e);
            }
        }
        // Drop the `?code&state` from the URL and land on `/`, success or not.
        if let Some(history) = window().and_then(|w| w.history().ok()) {
            let _ = history.replace_state_with_url(&JsValue::NULL, "", Some("/"));
        }
    } else if let Some(token) = auth::stored_token() {
        *AUTH_TOKEN.write().unwrap() = Some(token);
    }

    // Nobody signed in: the visitor reads as a guest. Our own server mints
    // that session — `POST /auth/guest`, no IdP round-trip and no account —
    // and `policy.json`'s `view` tier is what it opens: rooms, messages,
    // reactions and link previews, read-only, with no roster and no author
    // names (see `docs/auth.md`). This is what replaced the sign-in card as
    // the default landing: the card is now what a visitor sees only when they
    // cannot have a session at all.
    //
    // A FAILED sign-in is the one thing that keeps the card. The visitor asked
    // to become a member and something went wrong; dropping them into a
    // read-only session instead would answer a question they did not ask, and
    // would carry the reason off the screen with it.
    let sign_in_failed = AUTH_ERROR.read().map(|error| error.is_some()).unwrap_or(false);
    if AUTH_TOKEN.read().unwrap().is_none() && !sign_in_failed {
        match auth::mint_guest_token().await {
            Ok(token) => *AUTH_TOKEN.write().unwrap() = Some(token),
            Err(failure) => {
                // Both halves are deliberately thin. The card gets a fixed
                // sentence, and the log gets one of two words — never the
                // status line, the URL, or the response body, which is what
                // `GuestMintFailure` exists to keep out of here.
                tracing::warn!("could not mint a guest session: {}", failure.reason());
                *AUTH_ERROR.write().unwrap() =
                    Some("Could not start a read-only session — sign in, or reload in a moment.".to_string());
            }
        }
    }

    // Connect only with a session in hand. Without one there is nothing to
    // present on a request and nothing to read, so the card stands alone —
    // and the same is true of a connect that could not finish, which is why
    // only a whole one flips the gate the UI reads.
    if AUTH_TOKEN.read().unwrap().is_some() {
        match connect_node().await {
            Ok(()) => SESSION_LIVE.store(true, Ordering::Relaxed),
            Err(reason) => *AUTH_ERROR.write().unwrap() = Some(reason.to_string()),
        }
    }

    // Install the ReactiveGraphObserver at the base of the Ankurah observer stack
    // so that Leptos components can observe Ankurah signals via reactive_graph.
    CurrentObserver::set(ReactiveGraphObserver::new());

    // Community's choice of query-registry observer is x-ray. It has to be
    // attached before any component registers a query — see `xray::attach`.
    xray::attach();

    // The chat components carry their own stylesheet; put it in the document
    // before anything mounts. ChatTheme.css is where community hands it this
    // palette.
    ankurah_chat_leptos::install_styles();

    leptos::mount::mount_to_body(App);
}

/// Build the ephemeral node, connect to `/ws`, and wait until the server's
/// policy (roles + verifying key) has synced into the local agent.
///
/// Every step here is something the browser or the network can refuse, and an
/// `Err` carries the sentence the sign-in card shows for it — the detail goes
/// to the log, where an operator wants it. None of these are programming
/// faults, so none of them may be a panic: the globals are set only once the
/// node can genuinely read, so a caller that sees `Err` leaves them unset and
/// nothing downstream can reach a half-built session.
async fn connect_node() -> Result<(), &'static str> {
    let storage = match IndexedDBStorageEngine::open("community_app").await {
        Ok(storage) => storage,
        Err(e) => {
            tracing::error!("could not open IndexedDB storage: {e}");
            return Err("This browser would not let the app store data locally — allow site data here, then reload.");
        }
    };
    let agent = JwtAgent::new_ephemeral();
    let node = Node::new(Arc::new(storage), agent.clone());

    let client = match WebsocketClient::new(node.clone(), &ws_url()) {
        Ok(client) => client,
        Err(e) => {
            tracing::error!("could not create the websocket client: {e}");
            return Err("Could not connect to the community — reload in a moment.");
        }
    };

    // Wait for the client to join the remote system (metadata, collections,
    // etc.) — bounded, because the wait itself is not.
    if !wait_system_joined(&node).await {
        tracing::error!("the node did not join the remote system within {SYSTEM_JOIN_TIMEOUT_MS}ms — is /ws reaching the server?");
        return Err("Could not connect to the community — reload in a moment.");
    }

    // Until the ephemeral agent has synced the durable node's `jwtpolicy`
    // entity, its local policy is deny-all — so every read would be rejected.
    if !wait_policy_ready(&agent).await {
        tracing::error!("the policy row did not sync within {POLICY_TIMEOUT_MS}ms — every read would be denied");
        return Err("Could not finish connecting to the community — reload in a moment.");
    }

    NODE.set(node).ok().expect("NODE already initialized");
    CLIENT.set(SendWrapper::new(client)).ok().expect("CLIENT already initialized");
    AGENT.set(agent).ok().expect("AGENT already initialized");
    Ok(())
}

/// Same-origin `ws(s)://{host}` by default (trunk proxies `/ws` in dev). A
/// cross-origin build can override the endpoint at build time with BACKEND_WS_URL.
fn ws_url() -> String {
    match option_env!("BACKEND_WS_URL") {
        Some(url) if !url.is_empty() => url.to_string(),
        _ => {
            let window = window().expect("no window available");
            let location = window.location();
            let host = location.host().unwrap_or_else(|_| "127.0.0.1".into());
            let protocol = location.protocol().unwrap_or_else(|_| "http:".into());
            let ws_scheme = if protocol == "https:" { "wss" } else { "ws" };
            format!("{}://{}", ws_scheme, host)
        }
    }
}

/// Wait for the node to join the remote system, giving up after
/// [`SYSTEM_JOIN_TIMEOUT_MS`]. `false` when it did not.
///
/// FOR: the bound. `wait_system_ready` parks on a notification with no timeout
/// of its own (ankurah-core 0.9.0 `system.rs`), and that notification only
/// ever arrives over the websocket — so a socket that cannot be opened, or an
/// upgrade a proxy refuses, parks this future for good. The boot is what
/// mounts the app, so parking it means no app at all: a white page, no
/// sentence, until the visitor gives up and leaves. Bounding it is what turns
/// that into the sign-in card. Reachable with nothing exotic — HTTP working
/// while the websocket does not is an ordinary misconfiguration, and the guest
/// mint succeeding over HTTP is what gets a first-time visitor this far.
///
/// The wait runs as its own task while this polls a flag it sets, because the
/// wasm build carries no executor offering a select — the same shape the
/// policy wait below uses, for the same reason. Nothing here can cancel the
/// abandoned task: it holds a node handle and goes on waiting for a join that
/// is not coming, until the document does. The caller is on its way to the
/// card, so that handle outlives nothing that matters.
async fn wait_system_joined(node: &Node<IndexedDBStorageEngine, JwtAgent>) -> bool {
    let joined = Rc::new(Cell::new(false));
    spawn_local({
        let joined = joined.clone();
        let node = node.clone();
        async move {
            node.system.wait_system_ready().await;
            joined.set(true);
        }
    });

    for _ in 0..(SYSTEM_JOIN_TIMEOUT_MS / POLL_STEP_MS) {
        if joined.get() {
            return true;
        }
        sleep_ms(POLL_STEP_MS).await;
    }
    joined.get()
}

/// Poll the ephemeral agent until it has synced policy + verifying key, giving
/// up after [`POLICY_TIMEOUT_MS`]. `false` when it did not.
///
/// Until that row arrives the agent's local policy is deny-all, so a boot that
/// gave up and mounted anyway would leave a reader in front of a shell with no
/// rooms, no messages and nothing saying why — and it would stay there, since
/// the chat handshake rebuilds its queries when the session changes and
/// nothing here changes one. The card, with a sentence, is the honest end of
/// that path.
async fn wait_policy_ready(agent: &JwtAgent) -> bool {
    for _ in 0..(POLICY_TIMEOUT_MS / POLL_STEP_MS) {
        if agent.policy_ready() {
            return true;
        }
        sleep_ms(POLL_STEP_MS).await;
    }
    agent.policy_ready()
}

/// Await a browser `setTimeout`, so `wait_policy_ready` can yield without busy-looping.
async fn sleep_ms(ms: i32) {
    let promise = js_sys::Promise::new(&mut |resolve, _reject| {
        if let Some(w) = window() {
            let _ = w.set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, ms);
        }
    });
    let _ = JsFuture::from(promise).await;
}

/// Top-level gate: chat with a live session, the card without one. A session
/// is now the ordinary case either way — a visitor who has not signed in gets
/// a guest one at boot — so the card is what a visitor sees when the boot
/// could not give them something to read with: a refused mint, a browser that
/// refused storage, a websocket that never reached the server, a policy row
/// that never arrived, or a sign-in they asked for that failed and whose
/// reason is still worth reading. Every one of those wrote its sentence into
/// `AUTH_ERROR`, and the card renders it. Sign-in and sign-out are both
/// full-page transitions, so this is resolved once at mount.
#[component]
pub fn App() -> impl IntoView {
    if SESSION_LIVE.load(Ordering::Relaxed) {
        view! { <ChatApp /> }.into_any()
    } else {
        view! { <SignIn /> }.into_any()
    }
}

/// The landing view for a visitor with nothing to read through: no session, or
/// one that could not be connected.
#[component]
pub fn SignIn() -> impl IntoView {
    let flow = sign_in_ceremony::SignInFlow::new();
    // Failures used to reach only the console; render them where the visitor
    // actually is. Seeded from the callback's failure or a refused guest mint
    // (both set before mount), and written again if the click itself cannot
    // get off the ground.
    let auth_error = flow.error();
    // The overlay covers this button for the mouse but not for the keyboard,
    // and `begin` is what swallows the second Enter.
    let start = move |_| flow.begin();
    view! {
        <div class="signIn">
            <div class="signInGlow signInGlowA" aria-hidden="true"></div>
            <div class="signInGlow signInGlowB" aria-hidden="true"></div>
            <div class="signInCard">
                // Sprout mark — "ankura" is Sanskrit for sprout.
                <div class="signInMark" aria-hidden="true">
                    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"
                        stroke-linecap="round" stroke-linejoin="round">
                        <path d="M7 20h10" />
                        <path d="M10 20c5.5-2.5.8-6.4 3-10" />
                        <path d="M9.5 9.4c1.1.8 1.8 2.2 2.3 3.7-2 .4-3.5.4-4.8-.3-1.2-.6-2.3-1.9-3-4.2 2.8-.5 4.4 0 5.5.8z" />
                        <path d="M14.1 6a7 7 0 0 0-1.1 4c1.9-.1 3.3-.6 4.3-1.4 1-1 1.6-2.3 1.7-4.6-2.7.1-4 1-4.9 2z" />
                    </svg>
                </div>
                <h1 class="signInTitle">"Ankurah Community"</h1>
                <p class="signInSubtitle">
                    "Chat, ask questions, and share with the community — synced live, everywhere."
                </p>
                <div class="signInFeatures">
                    <span class="signInFeature">
                        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2"
                            stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
                            <path d="M13 2 3 14h7l-1 8 11-13h-7l1-7z" />
                        </svg>
                        "Real-time sync"
                    </span>
                    <span class="signInFeature">
                        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2"
                            stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
                            <path d="M12 22v-9" />
                            <path d="M9.5 9.4c1.1.8 1.8 2.2 2.3 3.7-2 .4-3.5.4-4.8-.3-1.2-.6-2.3-1.9-3-4.2 2.8-.5 4.4 0 5.5.8z" />
                            <path d="M14.1 6a7 7 0 0 0-1.1 4c1.9-.1 3.3-.6 4.3-1.4 1-1 1.6-2.3 1.7-4.6-2.7.1-4 1-4.9 2z" />
                        </svg>
                        "Built on Ankurah"
                    </span>
                    <span class="signInFeature">
                        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2"
                            stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
                            <path d="M20 13c0 5-3.5 7.5-7.7 9a.6.6 0 0 1-.6 0C7.5 20.5 4 18 4 13V6a1 1 0 0 1 1-1c2 0 4.5-1.2 6.2-2.7a1.2 1.2 0 0 1 1.6 0C14.5 3.8 17 5 19 5a1 1 0 0 1 1 1z" />
                        </svg>
                        "Open community"
                    </span>
                </div>
                {move || auth_error.get().map(|message| view! {
                    <div class="signInError" role="alert">{message}</div>
                })}
                <button class="signInButton" on:click=start>
                    "Sign in with idp.to"
                    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.4"
                        stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
                        <path d="M5 12h14" />
                        <path d="m13 6 6 6-6 6" />
                    </svg>
                </button>
                <p class="signInFootnote">"Authentication by idp.to — local-first chat, built in Rust + wasm."</p>
            </div>
            {flow.view()}
        </div>
    }
}

/// The chat application. Mounted with a session of either kind, so `ctx()` is
/// always valid here — but `viewer` may be `None`, and everything that needs
/// somebody to attribute a read or a write to is gated on it.
#[component]
pub fn ChatApp() -> impl IntoView {
    // Who is reading, resolved once: the session is fixed for as long as this
    // document lives (signing in reloads). `None` is a guest.
    let viewer = viewer();

    // The way into a sign-in from inside the app. An anonymous reader who
    // reaches for the message box or any write raises the chat components'
    // auth demand, and this is what answers it; the header offers the same
    // flow as a plain button for a reader who would rather ask first.
    let sign_in = sign_in_ceremony::SignInFlow::new();
    // Where the profile popover opens, when a message row's avatar or author
    // name is clicked. Owned here rather than by the row: the popover is
    // community's chrome (it reads the `userroles` cache, which is community's
    // collection), and a row that unmounts under the virtual scroller must not
    // take an open popover with it.
    let profile = RwSignal::new(None::<(EntityId, i32, i32)>);

    // UI-local state for selected room (Leptos signal, not Ankurah).
    let selected_room = RwSignal::new(None::<EntityId>);

    // UI-local state for current user (Leptos signal).
    let current_user = RwSignal::new(None::<UserView>);

    // Direct messages: which conversation is open, if any. Declared here
    // because the link-preview query below is scoped by it; the thread set and
    // the cursors follow further down.
    let selected_dm = RwSignal::new(None::<EntityId>);

    // Link previews: ONE standing LiveQuery for the room timeline, grouped by
    // url. `LinkPreview` rows are keyed by url with no room ref, and a query
    // per row would churn with the virtual scroller. `ok = false` rows are
    // excluded — a failed unfurl renders as the plain link that is already in
    // the bubble. The chat components render this into the slot they leave
    // under each bubble; see chat_hooks.
    //
    // Created on first sight of a room timeline rather than at mount, and kept
    // afterwards: only room rows have a preview slot, so a reader who spends
    // their visit in conversations never opens this subscription at all. A
    // failure logs and leaves the map empty — a message without its unfurl card
    // still reads, and the plain link is right there in the bubble.
    let link_previews = RwSignal::new(None::<SendWrapper<LiveQuery<LinkPreviewView>>>);
    Effect::new(move |_| {
        let looking_at_a_room = selected_dm.get().is_none() && selected_room.get().is_some();
        if !looking_at_a_room || link_previews.get_untracked().is_some() {
            return;
        }
        match ctx().query::<LinkPreviewView>("ok = true") {
            Ok(query) => link_previews.set(Some(SendWrapper::new(query))),
            Err(e) => tracing::error!("Failed to create the link-preview LiveQuery: {:?}", e),
        }
    });
    let previews_by_url = Memo::new(move |_| match link_previews.get() {
        Some(query) => query.get().into_iter().filter_map(|p| p.url().ok().map(|u| (u, p))).collect(),
        None => HashMap::<String, LinkPreviewView>::new(),
    });

    // The handshake the chat components read everything host-shaped through.
    //
    // The session is the host's to own and the host's alone to write, so it is
    // a signal here. Both halves are known by the time this mounts — the
    // context from the token `initialize` resolved, the viewer from that
    // token's subject — and nothing sets this signal today: signing in
    // mid-visit is a real path in the components, but not one community takes.
    // It reloads, and boots as the member on the way back.
    //
    // A WRITEABLE SIGNAL ANYWAY, deliberately, and now with a second claimant.
    // `ChatContext::new` takes the bare pair too and wraps it in a signal that
    // never moves, which is the honest shape for a session that is fixed for
    // good. This one is an `RwSignal` because of what it holds open: a guest
    // token expires under a tab left open for two hours, and recovering means
    // minting a fresh one and setting this pair — `session.set((ctx(),
    // viewer))` here and nothing else (the expiry work is #86). Set the pair
    // as one value when that lands: the components guarantee what a single
    // read sees, so halves moved separately arrive as two sessions.
    let session = RwSignal::new((ctx(), viewer));
    let chat = ChatContext::new(session)
        .online(|| ws_client().connection_state().get().to_string() == "Connected")
        .moderator(can_moderate)
        // What an anonymous reader meets. The components refuse the caret and
        // raise this when somebody with no viewer presses on the composer, and
        // raise it again on every write they attempt; `begin` is idempotent, so
        // a raise while the ceremony is already up is a no-op.
        .on_auth_demand(move || sign_in.begin())
        .hooks(chat_hooks::chat_hooks(previews_by_url, profile, viewer))
        .provide();

    // Notification sounds, which want a per-room message window. The rooms
    // come from the chat handshake, which owns that query for the session — a
    // second subscription here would be a second copy of the same rows.
    //
    // The NotificationManager then HOLDS that handle for the application's
    // lifetime, which is the anti-pattern the accessor's doc warns about: the
    // handle is a borrow of the session's, and a host that swapped sessions
    // would leave this manager reading through a context the reader had left.
    // It is correct here and only here, because community signs in with a
    // full-page redirect and never swaps a session under a mounted tree. A
    // host that does swap must re-ask.
    //
    // THREE WAYS TO GET NOTHING, and all cost the reader chimes rather than
    // the page. A guest gets no manager at all: the chime is a member
    // affordance, because the preference that silences it lives in
    // member-only `notificationpref` rows — a sound the listener cannot turn
    // off must not play, and with no member id every arriving message would
    // count as "from others". `rooms()` answers `None` when the handshake
    // could not open its query, which the boot gate rules out — a session
    // that cannot read never reaches this component — and is kept as a
    // degradation on the same terms as the header panels: the cost of being
    // wrong about that must not be an app-wide panic. `NotificationManager::new`
    // answers `None` when the browser refuses an `AudioContext`, which no
    // gate rules out at all.
    let notification_manager = viewer
        .and(chat.rooms())
        .and_then(|rooms| NotificationManager::new(rooms, current_user.get_untracked().map(|u| u.id().to_base64())));

    // Load the signed-in user (the server upserted it before minting our token;
    // the JWT `sub` is that User entity's id). A guest has no row to load and
    // could not read it if they had — `user` is signed-in-only in policy.json
    // — so this only runs for a member, and `current_user` simply stays `None`
    // for everybody else.
    if let Some(me) = viewer {
        Effect::new({
            let current_user = current_user.clone();
            move |_| {
                spawn_local(async move {
                    match load_current_user(me).await {
                        Ok(user) => current_user.set(Some(user)),
                        Err(e) => tracing::error!("Failed to load current user: {}", e),
                    }
                });
            }
        });
    }

    // `current_user` is resolved asynchronously, so push the id into the
    // NotificationManager once it's available (otherwise it stays None and
    // treats your own messages as coming from others → chimes on send). The
    // manager is self-contained — it holds its own room/message subscriptions
    // — and this Effect is also what keeps it alive for ChatApp's lifetime.
    if let Some(notification_manager) = notification_manager {
        Effect::new(move |_| notification_manager.set_current_user_id(current_user.get().map(|u| u.id().to_base64())));
    }

    // Read cursors, the DM thread set, the members list and the reactions
    // query are all the chat handshake's now, built once per session and
    // registered there under their own labels — so x-ray's inventory is the
    // same as it was without community holding any of them.

    view! {
        // X-ray is a member's tool. Not because an anonymous reader could see
        // anything their own claims do not already permit — the inspector
        // reads through the same session as everything else — but because what
        // it reads is a message's EVENT HISTORY, which is the text an author
        // edited away, and its inspect-by-id row invites poking at ids. Widening
        // the audience for "I edited that to take something out" from members
        // to anyone at all is a choice, and this is it being declined. Nothing
        // else changes: `xray::attach` still installs the query-registry
        // observer at boot, and with no launcher there is nothing to turn on.
        {viewer.map(|_| view! { <xray::XRayLauncher /> })}
        <div class="container">
            // Banned-client self-lock: watches the viewer's own active bans and
            // replaces the UI with a lockout + delayed sign-out (see ban_lock.rs).
            // A guest has no rows to watch and nothing to be banned from —
            // anonymous identity is free, so the ban table is a member tool.
            {viewer.map(|me| view! { <ban_lock::BanLock viewer=me /> })}
            <Header current_user selected_room selected_dm viewer sign_in />

            <div class="mainContent">
                <Sidebar selected_room selected_dm viewer />
                // One timeline at a time. Switching rebuilds the pane's
                // ScrollManager, which is what a room switch already does.
                // Nothing can open a conversation without a viewer — the one
                // affordance that sets this signal is a member's — so the DM
                // branch is a member's too, by way of the signal rather than a
                // second gate here.
                {
                    move || {
                        if selected_dm.get().is_some() {
                            view! { <DmConversation partner=selected_dm /> }.into_any()
                        } else {
                            view! { <RoomLog room=selected_room debug_header=true /> }.into_any()
                        }
                    }
                }
            </div>
            // The sign-in ceremony, raised by the components' auth demand or by
            // the header's button. One mount for the whole app, and the variant
            // that carries the flow's own failures: unlike the card, this host
            // has no banner of its own, and a sign-in that cannot start is the
            // one failure an anonymous reader must not meet in silence.
            {sign_in.view_with_notice()}
            // The profile popover, opened from any message row's avatar or
            // author name. It positions itself from the coordinates the row
            // reported, so rendering it here rather than inside the row costs
            // nothing and outlives a row the scroller unmounts.
            {
                let chat = chat.clone();
                move || {
                profile.get().map(|(user_id, x, y)| {
                    chat.members()
                        .map(|q| q.get())
                        .unwrap_or_default()
                        .into_iter()
                        .find(|u| u.id() == user_id)
                        .map(|user| {
                            view! {
                                <ProfilePopover
                                    user=user
                                    x=x
                                    y=y
                                    on_close=move || profile.set(None)
                                />
                            }
                        })
                })
                }
            }
        </div>
    }
}

/// Resolve the signed-in member's own `User` row. Called only with a viewer in
/// hand — a guest reaching the `user` collection is refused at the gate.
async fn load_current_user(user_id: EntityId) -> Result<UserView, Box<dyn std::error::Error>> {
    let user = ctx().get::<UserView>(user_id).await?;
    Ok(user)
}
