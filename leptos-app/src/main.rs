use leptos::prelude::*;

use ankurah::{Context, EntityId, Node, model::Mutable, policy::DEFAULT_CONTEXT as C, policy::PermissiveAgent};
use ankurah_signals::{CurrentObserver, ReactiveGraphObserver};
use ankurah_storage_indexeddb_wasm::IndexedDBStorageEngine;
use community_model::{RoomView, User, UserView};
use ankurah_websocket_client_wasm::WebsocketClient;
use lazy_static::lazy_static;
use send_wrapper::SendWrapper;
use std::sync::{Arc, OnceLock};
use wasm_bindgen_futures::spawn_local;
use web_sys::window;

mod chat;
mod chat_debug_header;
mod debug_overlay;
mod editable_text_field;
mod header;
mod message_context_menu;
mod message_input;
mod message_list;
mod message_row;
mod notification_manager;
mod qr_code_modal;
mod room_list;

use chat::Chat;
use debug_overlay::DebugOverlay;
use header::Header;
use notification_manager::NotificationManager;
use room_list::RoomList;

lazy_static! {
    static ref NODE: OnceLock<Node<IndexedDBStorageEngine, PermissiveAgent>> = OnceLock::new();
    static ref CLIENT: OnceLock<SendWrapper<WebsocketClient>> = OnceLock::new();
}

/// Get the global Ankurah context.
pub fn ctx() -> Context {
    NODE.get().expect("Node not initialized").context(C).expect("failed to create context")
}

/// Get the global WebSocket client.
pub fn ws_client() -> WebsocketClient {
    (**CLIENT.get().expect("Client not initialized")).clone()
}

fn main() {
    console_error_panic_hook::set_once();
    tracing_wasm::set_as_global_default_with_config(
        tracing_wasm::WASMLayerConfigBuilder::new()
            .set_max_level(tracing::Level::INFO) // Only show INFO, WARN, ERROR
            .build(),
    );

    // Initialize the Ankurah node and LiveQuery asynchronously, then mount Leptos.
    spawn_local(initialize());
}

async fn initialize() {
    // Open IndexedDB-backed storage and create a Node.
    let storage = IndexedDBStorageEngine::open("community_app").await.expect("failed to open IndexedDB storage");
    let node = Node::new(Arc::new(storage), PermissiveAgent::new());

    // Same-origin by default: connect to the host the app was served from (in
    // dev, trunk proxies /ws to the backend, so the randomized server port is
    // never hard-coded here; the ankurah WebsocketClient appends the /ws path).
    // A cross-origin build (SPA on a CDN, backend on Cloud Run) can override the
    // endpoint at build time with BACKEND_WS_URL, e.g. wss://community-api.ankurah.org.
    let ws_url = match option_env!("BACKEND_WS_URL") {
        Some(url) if !url.is_empty() => url.to_string(),
        _ => {
            let window = window().expect("no window available");
            let location = window.location();
            let host = location.host().unwrap_or_else(|_| "127.0.0.1".into());
            let protocol = location.protocol().unwrap_or_else(|_| "http:".into());
            let ws_scheme = if protocol == "https:" { "wss" } else { "ws" };
            format!("{}://{}", ws_scheme, host)
        }
    };

    let client = WebsocketClient::new(node.clone(), &ws_url).expect("failed to create WebsocketClient");

    // Wait for the client to join the remote system (metadata, collections, etc.).
    node.system.wait_system_ready().await;

    // Store node and client in global statics.
    NODE.set(node).ok().expect("NODE already initialized");
    CLIENT.set(SendWrapper::new(client)).ok().expect("CLIENT already initialized");

    // Install the ReactiveGraphObserver at the base of the Ankurah observer stack
    // so that Leptos components can observe Ankurah signals via reactive_graph.
    CurrentObserver::set(ReactiveGraphObserver::new());

    leptos::mount::mount_to_body(App);
}

#[component]
pub fn App() -> impl IntoView {
    // Build the rooms LiveQuery from the global context.
    let rooms = ctx().query::<RoomView>("true ORDER BY name ASC").expect("failed to create RoomView LiveQuery");

    // UI-local state for selected room (Leptos signal, not Ankurah).
    let selected_room = RwSignal::new(None::<RoomView>);

    // UI-local state for current user (Leptos signal).
    let current_user = RwSignal::new(None::<UserView>);

    // Initialize user asynchronously
    Effect::new({
        let current_user = current_user.clone();
        move |_| {
            spawn_local(async move {
                match ensure_user().await {
                    Ok(user) => current_user.set(Some(user)),
                    Err(e) => tracing::error!("Failed to initialize user: {}", e),
                }
            });
        }
    });

    // Create notification manager with rooms query and current user ID
    let notification_manager = NotificationManager::new(rooms.clone(), current_user.get_untracked().map(|u| u.id().to_base64()));

    // `current_user` is resolved asynchronously by `ensure_user`, so push the id
    // into the NotificationManager once it's available (otherwise it stays None
    // and treats your own messages as coming from others → chimes on send).
    Effect::new({
        let notification_manager = notification_manager.clone();
        move |_| notification_manager.set_current_user_id(current_user.get().map(|u| u.id().to_base64()))
    });

    view! {
        <DebugOverlay />

        <div class="container">
            <Header current_user />

            <div class="mainContent">
                <RoomList rooms selected_room notification_manager=notification_manager.clone() />
                <Chat room=selected_room current_user=current_user notification_manager=notification_manager />
            </div>
        </div>
    }
}

const STORAGE_KEY_USER_ID: &str = "community_user_id";

/// Ensures a user exists, creating one if necessary.
/// Stores the user ID in localStorage for persistence across sessions.
async fn ensure_user() -> Result<UserView, Box<dyn std::error::Error>> {
    let context = ctx();

    // Check localStorage for existing user
    if let Some(storage) = window().and_then(|w| w.local_storage().ok().flatten()) {
        if let Ok(Some(stored_id)) = storage.get_item(STORAGE_KEY_USER_ID) {
            if let Ok(entity_id) = EntityId::from_base64(&stored_id) {
                if let Ok(user) = context.get::<UserView>(entity_id).await {
                    return Ok(user);
                }
            }
        }
    }

    // Create new user
    let transaction = context.begin();
    let random_suffix = (js_sys::Math::random() * 10000.0).floor() as u32;
    let mutable = transaction.create(&User { display_name: format!("User-{}", random_suffix) }).await?;
    let user = mutable.read();
    transaction.commit().await?;

    // Store user ID in localStorage
    if let Some(storage) = window().and_then(|w| w.local_storage().ok().flatten()) {
        let _ = storage.set_item(STORAGE_KEY_USER_ID, &user.id().to_base64());
    }

    Ok(user)
}
