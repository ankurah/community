use leptos::prelude::*;
use wasm_bindgen::JsValue;
use web_sys::{KeyboardEvent, window};

use ankurah::{LiveQuery, model::Mutable};
use ankurah_signals::Get as AnkurahGet;
use community_model::{Room, RoomView};

use crate::{ctx, read_state::ReadStateManager};

/// Auto-select a room from the list if none is currently selected.
/// Chooses based on URL parameter or defaults to "general".
/// Returns a closure that can be used in an Effect.
fn auto_select_room(rooms: &LiveQuery<RoomView>, selected_room: RwSignal<Option<RoomView>>) -> impl Fn() + 'static {
    let rooms = rooms.clone();
    move || {
        if selected_room.get().is_some() {
            return;
        }

        let items = rooms.get();
        if items.is_empty() {
            return;
        }

        let room_id_from_url = window()
            .and_then(|win| win.location().search().ok())
            .and_then(|search| web_sys::UrlSearchParams::new_with_str(&search).ok())
            .and_then(|params| params.get("room"));

        let room = room_id_from_url
            .and_then(|id| items.iter().find(|r| r.id().to_base64() == id).cloned())
            .or_else(|| items.iter().find(|r| r.name().unwrap_or_default() == "general").cloned());

        if let Some(room) = room {
            selected_room.set(Some(room));
        }
    }
}

/// Sync the browser URL with the selected room.
/// Returns a closure that can be used in an Effect.
fn sync_url_with_room(selected_room: &RwSignal<Option<RoomView>>) -> impl Fn() + 'static {
    let selected_room = selected_room.clone();
    move || {
        let Some(room) = selected_room.get() else { return };
        let Some(win) = window() else { return };
        let Ok(href) = win.location().href() else { return };
        let Ok(url) = web_sys::Url::new(&href) else { return };

        url.search_params().set("room", &room.id().to_base64());
        let _ = win.history().and_then(|h| h.replace_state_with_url(&JsValue::NULL, "", Some(&url.href())));
    }
}

/// Full Leptos port of the React `RoomList` component.
///
/// Validates the `ReactiveGraphObserver` + reactive_graph bridge:
/// - `rooms` is an Ankurah `LiveQuery<RoomView>` that updates when rooms change
/// - `selected_room` is a Leptos `RwSignal<Option<RoomView>>` for UI-local state
/// - Calling `Get::get(&rooms)` triggers reactive_graph tracking via the bridge
#[component]
pub fn RoomList(
    rooms: LiveQuery<RoomView>,
    selected_room: RwSignal<Option<RoomView>>,
    read_state: ReadStateManager,
) -> impl IntoView {
    let is_creating = RwSignal::new(false);
    Effect::new(auto_select_room(&rooms, selected_room));
    Effect::new(sync_url_with_room(&selected_room));

    let rooms_for_empty = rooms.clone();

    view! {
        <div class="sidebar">
            <div class="sidebarHeader">
                <span class="sidebarTitle">"Rooms"</span>
                <button class="createRoomButton" on:click=move |_| is_creating.set(true) title="Create new room">
                    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.4"
                        stroke-linecap="round" aria-hidden="true">
                        <path d="M12 5v14" />
                        <path d="M5 12h14" />
                    </svg>
                </button>
            </div>

            <div class="roomList">
                <Show when=move || is_creating.get()>
                    <NewRoomInput selected_room=selected_room on_cancel=move || is_creating.set(false) />
                </Show>

                <Show when=move || rooms_for_empty.get().is_empty()>
                    <div class="emptyRooms">"No rooms yet — press + to plant one."</div>
                </Show>

                <RoomListUl rooms selected_room read_state />
            </div>
        </div>
    }
}

#[component]
fn RoomListUl(
    #[prop(into)] rooms: LiveQuery<RoomView>,
    selected_room: RwSignal<Option<RoomView>>,
    read_state: ReadStateManager,
) -> impl IntoView {
    view! {
        <For
            each=move || rooms.get()
            key=|room: &RoomView| room.id()
            children={
                let read_state = read_state.clone();
                move |room: RoomView| {
                    view! {
                        <RoomItem
                            room=room
                            selected_room=selected_room
                            read_state=read_state.clone()
                        />
                    }
                }
            }
        />
    }
}

#[component]
fn RoomItem(room: RoomView, selected_room: RwSignal<Option<RoomView>>, read_state: ReadStateManager) -> impl IntoView {
    let room_id = room.id().to_base64();
    let name = room.name().unwrap_or_default();

    let room_id_selected = room_id.clone();
    let is_selected = move || selected_room.get().as_ref().map(|r| r.id().to_base64() == room_id_selected).unwrap_or(false);

    let room_for_click = room.clone();
    let room_id_badge = room_id.clone();

    view! {
        <div
            class=move || if is_selected() { "roomItem selected" } else { "roomItem" }
            on:click=move |_| selected_room.set(Some(room_for_click.clone()))
        >
            <span class="roomHash" aria-hidden="true">"#"</span>
            <span class="roomLabel">{name}</span>
            {move || {
                // Reactive read: re-renders as messages arrive or the user's
                // persistent read cursor (#13) advances on any device.
                let unread_count = read_state.unread_count(&room_id_badge);
                (unread_count > 0).then(|| {
                    let badge_text = if unread_count >= 10 { "10+".to_string() } else { unread_count.to_string() };
                    view! { <span class="unreadBadge">{badge_text}</span> }
                })
            }}
        </div>
    }
}

#[component]
fn NewRoomInput(selected_room: RwSignal<Option<RoomView>>, on_cancel: impl Fn() + Clone + 'static) -> impl IntoView {
    let room_name = RwSignal::new(String::new());

    let handle_key = {
        let on_cancel = on_cancel.clone();
        move |ev: KeyboardEvent| match ev.key().as_str() {
            "Enter" => {
                ev.prevent_default();
                let name = room_name.get().trim().to_string();
                if !name.is_empty() {
                    let on_cancel = on_cancel.clone();
                    wasm_bindgen_futures::spawn_local(async move {
                        match (|| async {
                            let transaction = ctx().begin();
                            // `created_by` must be the caller (the room write
                            // scope in policy.json rejects anything else).
                            let room = transaction
                                .create(&Room { name, created_by: Some(crate::current_user_id().into()), topic: None })
                                .await?
                                .read();
                            transaction.commit().await?;
                            Ok::<_, Box<dyn std::error::Error>>(room)
                        })()
                        .await
                        {
                            Ok(room) => {
                                selected_room.set(Some(room));
                                on_cancel();
                            }
                            Err(e) => {
                                tracing::error!("Failed to create room: {}", e);
                            }
                        }
                    });
                }
            }
            "Escape" => on_cancel(),
            _ => {}
        }
    };

    view! {
        <div class="createRoomInput">
            <input
                type="text"
                placeholder="Room name..."
                prop:value=move || room_name.get()
                on:input=move |ev| room_name.set(event_target_value(&ev))
                on:keydown=handle_key
                on:blur={
                    let on_cancel = on_cancel.clone();
                    move |_| {
                        // Use try_get() to avoid panic if signal is already disposed
                        if let Some(name) = room_name.try_get() {
                            if name.trim().is_empty() {
                                on_cancel();
                            }
                        }
                    }
                }

                autofocus
            />
        </div>
    }
}
