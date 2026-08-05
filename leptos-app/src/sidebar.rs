//! The left rail: rooms above, conversations below.
//!
//! Both lists are `ankurah-chat-leptos` components. What is community's, and
//! therefore here, is the rail they share and the two behaviours that only
//! make sense for a whole page rather than an embedded panel:
//!
//! - which room is selected when nobody has picked one yet, and
//! - keeping the browser's address bar in step with that choice.
//!
//! Neither belongs to a component. A chat panel dropped into someone else's
//! page must not rewrite their URL, and must not decide for them which room a
//! visitor lands in.
//!
//! The two lists know about each other only through this file: `active` tells
//! the room selector whether the rooms surface is the one on screen, so a
//! single rail row looks selected at a time, and `on_select` closes the open
//! conversation when a room is clicked.
//!
//! Both lists take ids and nothing else. The rooms they list, the members they
//! name and the cursors behind their badges all belong to the chat handshake.

use leptos::prelude::*;
use wasm_bindgen::JsValue;
use web_sys::window;

use ankurah::EntityId;
use ankurah_chat_leptos::{ChatContext, DmSidebar, RoomSelector};
use ankurah_signals::Get as AnkurahGet;

/// Pick a room when none is selected: the one named in `?room=`, else
/// `general`. Returns a closure for an `Effect`.
///
/// It reads the rooms out of the chat handshake, which owns that query for the
/// session — community no longer holds one of its own.
fn auto_select_room(chat: ChatContext, selected_room: RwSignal<Option<EntityId>>) -> impl Fn() + 'static {
    move || {
        if selected_room.get().is_some() {
            return;
        }

        let items = chat.rooms().map(|q| q.get()).unwrap_or_default();
        if items.is_empty() {
            return;
        }

        let room_id_from_url = window()
            .and_then(|win| win.location().search().ok())
            .and_then(|search| web_sys::UrlSearchParams::new_with_str(&search).ok())
            .and_then(|params| params.get("room"));

        let room = room_id_from_url
            .and_then(|id| items.iter().find(|r| r.id().to_base64() == id).map(|r| r.id()))
            .or_else(|| items.iter().find(|r| r.name().unwrap_or_default() == "general").map(|r| r.id()));

        if let Some(room) = room {
            selected_room.set(Some(room));
        }
    }
}

/// Keep `?room=` pointing at the selected room. Returns a closure for an
/// `Effect`.
fn sync_url_with_room(selected_room: RwSignal<Option<EntityId>>) -> impl Fn() + 'static {
    move || {
        let Some(room_id) = selected_room.get() else { return };
        let Some(win) = window() else { return };
        let Ok(href) = win.location().href() else { return };
        let Ok(url) = web_sys::Url::new(&href) else { return };

        url.search_params().set("room", &room_id.to_base64());
        let _ = win.history().and_then(|h| h.replace_state_with_url(&JsValue::NULL, "", Some(&url.href())));
    }
}

#[component]
pub fn Sidebar(
    selected_room: RwSignal<Option<EntityId>>,
    /// Which correspondent's conversation is open, by id.
    selected_dm: RwSignal<Option<EntityId>>,
) -> impl IntoView {
    // Resolved in the body and handed to the effect. An effect does carry an
    // owner, so this is not a fix for a panic; it is so that the closure below
    // — which is `'static` and could be installed anywhere — does not depend
    // on where it ends up being run.
    Effect::new(auto_select_room(ankurah_chat_leptos::chat(), selected_room));
    Effect::new(sync_url_with_room(selected_room));

    // A room stays selected in app state while a conversation is open, so
    // closing the conversation returns the reader to it — but only one rail
    // row may LOOK selected.
    let rooms_active = Signal::derive(move || selected_dm.get().is_none());
    let close_dm = Callback::new(move |_room: EntityId| selected_dm.set(None));

    view! {
        <div class="sidebar">
            <RoomSelector selected_room=selected_room active=rooms_active on_select=close_dm />
            <DmSidebar selected_dm=selected_dm />
        </div>
    }
}
