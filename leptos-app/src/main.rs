use leptos::prelude::*;

use ankurah::{Context, EntityId, Node};
use ankurah_jwt_auth::{parse_claims_unverified, JwtAgent, JwtContext};
use ankurah_signals::{CurrentObserver, ReactiveGraphObserver};
use ankurah_storage_indexeddb_wasm::IndexedDBStorageEngine;
use ankurah_websocket_client_wasm::WebsocketClient;
use community_model::{RoomView, UserView};
use lazy_static::lazy_static;
use send_wrapper::SendWrapper;
use std::sync::{Arc, OnceLock, RwLock};
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::{spawn_local, JsFuture};
use web_sys::window;

mod auth;
mod ban_lock;
mod chat;
mod chat_debug_header;
mod editable_text_field;
mod fmt;
mod header;
mod link_preview;
mod markdown;
mod members_panel;
mod message_context_menu;
mod message_input;
mod message_list;
mod message_row;
mod mod_log_panel;
mod notification_inbox;
mod notification_manager;
mod panels;
mod profile_popover;
mod qr_code_modal;
mod queries;
mod reactions;
mod read_state;
mod room_list;
mod room_topic;
mod user_detail_panel;
mod xray;

use chat::Chat;
use header::Header;
use notification_manager::NotificationManager;
use read_state::ReadStateManager;
use room_list::RoomList;

lazy_static! {
    static ref NODE: OnceLock<Node<IndexedDBStorageEngine, JwtAgent>> = OnceLock::new();
    static ref CLIENT: OnceLock<SendWrapper<WebsocketClient>> = OnceLock::new();
    /// The ephemeral policy agent — used to poll `policy_ready()` after connect.
    static ref AGENT: OnceLock<JwtAgent> = OnceLock::new();
    /// The minted ankurah session token (present once signed in).
    static ref AUTH_TOKEN: RwLock<Option<String>> = RwLock::new(None);
    /// A sign-in failure from the OIDC callback, surfaced on the sign-in card.
    /// Set (at most once) during `initialize`, before Leptos mounts, so plain
    /// storage suffices — no signal needed.
    static ref AUTH_ERROR: RwLock<Option<String>> = RwLock::new(None);
}

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

/// The signed-in user's entity id (the JWT `sub`). Only call from within the
/// signed-in UI subtree (`ChatApp`), where the token is guaranteed present.
pub fn current_user_id() -> EntityId {
    let token = AUTH_TOKEN.read().expect("auth token lock poisoned").clone().expect("not authenticated");
    let claims = parse_claims_unverified(&token).expect("stored token is a valid JWT");
    EntityId::from_base64(&claims.sub).expect("JWT sub is a valid entity id")
}

/// The signed-in user's roles, as carried by the stored session token. Roles
/// are managed by the IdP and arrive as lowercase stable keys ("member",
/// "moderator", "admin"). UI gating only — the server enforces the real
/// policy at token mint and on every read/write. Unlike `current_user_id`,
/// this must never panic: it is called from rendering paths where a missing
/// or unreadable token should simply mean "no privileges", so any failure
/// yields an empty Vec.
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

    // Only build/connect the node when signed in. Sign-in is a full-page
    // redirect, so an anonymous connection would just be discarded on click.
    if AUTH_TOKEN.read().unwrap().is_some() {
        connect_node().await;
    }

    // Install the ReactiveGraphObserver at the base of the Ankurah observer stack
    // so that Leptos components can observe Ankurah signals via reactive_graph.
    CurrentObserver::set(ReactiveGraphObserver::new());

    leptos::mount::mount_to_body(App);
}

/// Build the ephemeral node, connect to `/ws`, and wait until the server's
/// policy (roles + verifying key) has synced into the local agent.
async fn connect_node() {
    let storage = IndexedDBStorageEngine::open("community_app").await.expect("failed to open IndexedDB storage");
    let agent = JwtAgent::new_ephemeral();
    let node = Node::new(Arc::new(storage), agent.clone());

    let client = WebsocketClient::new(node.clone(), &ws_url()).expect("failed to create WebsocketClient");

    // Wait for the client to join the remote system (metadata, collections, etc.).
    node.system.wait_system_ready().await;

    NODE.set(node).ok().expect("NODE already initialized");
    CLIENT.set(SendWrapper::new(client)).ok().expect("CLIENT already initialized");
    AGENT.set(agent).ok().expect("AGENT already initialized");

    // Until the ephemeral agent has synced the durable node's `jwtpolicy` entity,
    // its local policy is deny-all — so reads and writes would be rejected.
    wait_policy_ready().await;
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

/// Poll the ephemeral agent until it has synced policy + verifying key (or time
/// out after ~5s and proceed — the UI degrades to "no rooms" rather than hanging).
async fn wait_policy_ready() {
    for _ in 0..100 {
        if AGENT.get().map(JwtAgent::policy_ready).unwrap_or(false) {
            return;
        }
        sleep_ms(50).await;
    }
    tracing::warn!("policy not ready after ~5s; proceeding (reads/writes may fail until it syncs)");
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

/// Top-level gate: the chat UI requires a signed-in session. Both sign-in and
/// sign-out are full-page transitions, so this is resolved once at mount.
#[component]
pub fn App() -> impl IntoView {
    let signed_in = AUTH_TOKEN.read().map(|t| t.is_some()).unwrap_or(false);
    if signed_in {
        view! { <ChatApp /> }.into_any()
    } else {
        view! { <SignIn /> }.into_any()
    }
}

/// The signed-out landing view.
#[component]
pub fn SignIn() -> impl IntoView {
    let start = move |_| {
        if let Err(e) = auth::start_sign_in() {
            tracing::error!("failed to start sign-in: {:?}", e);
        }
    };
    // Sign-in failures used to reach only the console; render them where the
    // user actually is. Read once — the value is set before mount.
    let auth_error = AUTH_ERROR.read().ok().and_then(|guard| guard.clone());
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
                {auth_error.map(|message| view! {
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
        </div>
    }
}

/// The authenticated chat application (was `App`). Only mounted when signed in,
/// so `ctx()` is always valid here.
#[component]
pub fn ChatApp() -> impl IntoView {
    // Build the rooms LiveQuery from the global authenticated context.
    let rooms = ctx().query::<RoomView>("true ORDER BY name ASC").expect("failed to create RoomView LiveQuery");

    // UI-local state for selected room (Leptos signal, not Ankurah).
    let selected_room = RwSignal::new(None::<RoomView>);

    // UI-local state for current user (Leptos signal).
    let current_user = RwSignal::new(None::<UserView>);

    // Load the signed-in user (the server upserted it before minting our token;
    // the JWT `sub` is that User entity's id).
    Effect::new({
        let current_user = current_user.clone();
        move |_| {
            spawn_local(async move {
                match load_current_user().await {
                    Ok(user) => current_user.set(Some(user)),
                    Err(e) => tracing::error!("Failed to load current user: {}", e),
                }
            });
        }
    });

    // Notification sounds (self-contained: it holds its own room/message
    // subscriptions; the Effect below keeps it alive for ChatApp's lifetime).
    let notification_manager = NotificationManager::new(rooms.clone(), current_user.get_untracked().map(|u| u.id().to_base64()));

    // `current_user` is resolved asynchronously, so push the id into the
    // NotificationManager once it's available (otherwise it stays None and
    // treats your own messages as coming from others → chimes on send).
    Effect::new({
        let notification_manager = notification_manager.clone();
        move |_| notification_manager.set_current_user_id(current_user.get().map(|u| u.id().to_base64()))
    });

    // Persistent per-room read cursors + unread badges (#13).
    let read_state = ReadStateManager::new(rooms.clone(), current_user_id());

    // App-lifetime queries surfaced in the X-ray queries card (id discarded).
    xray::bus::bus().register("rooms (app)", &rooms);

    view! {
        <xray::XRayLauncher />
        <div class="container">
            // Banned-client self-lock: watches the viewer's own active bans and
            // replaces the UI with a lockout + delayed sign-out (see ban_lock.rs).
            <ban_lock::BanLock />
            <Header current_user selected_room />

            <div class="mainContent">
                <RoomList rooms selected_room read_state=read_state.clone() />
                <Chat room=selected_room current_user=current_user read_state=read_state />
            </div>
        </div>
    }
}

/// Resolve the signed-in `User` from the session token's `sub` (its entity id).
async fn load_current_user() -> Result<UserView, Box<dyn std::error::Error>> {
    let token = AUTH_TOKEN.read().unwrap().clone().ok_or("not authenticated")?;
    let claims = parse_claims_unverified(&token)?;
    let user_id = EntityId::from_base64(&claims.sub)?;
    let user = ctx().get::<UserView>(user_id).await?;
    Ok(user)
}
