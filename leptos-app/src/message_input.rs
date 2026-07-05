use leptos::prelude::*;
use web_sys::KeyboardEvent;

use ankurah_signals::Get as AnkurahGet;
use community_model::{Message, MessageView, RoomView, UserView};

use crate::{ctx, ws_client};

/// Message input component for sending and editing messages.
/// Handles Enter to send, Escape to cancel edit, Cmd/Ctrl+Up/Down to navigate own messages.
#[component]
pub fn MessageInput(
    room: RoomView,
    current_user: Option<UserView>,
    editing_message: RwSignal<Option<MessageView>>,
    /// Current visible messages (oldest-first), used for Cmd/Ctrl+Up/Down navigation.
    #[prop(into)] messages: Signal<Vec<MessageView>>,
) -> impl IntoView {
    let message_input = RwSignal::new(String::new());

    // Live connection state from the WebSocket client (reactive via the observer bridge).
    let connection_status = move || ws_client().connection_state().get().to_string();
    let is_connected = move || connection_status() == "Connected";
    let can_send = move || !message_input.get().trim().is_empty() && is_connected();

    // Mirror the editing message text into the input.
    Effect::new(move |_| {
        if let Some(edit_msg) = editing_message.get() {
            message_input.set(edit_msg.text().unwrap_or_default());
        } else {
            message_input.set(String::new());
        }
    });

    let send = {
        let current_user = current_user.clone();
        let room = room.clone();
        move || {
            let input_text = message_input.get();
            if input_text.trim().is_empty() {
                return;
            }
            let Some(user) = current_user.clone() else {
                tracing::info!("Cannot send: no user");
                return;
            };

            if let Some(edit_msg) = editing_message.get() {
                // Edit existing message via a CRDT text replace.
                let input_text = input_text.trim().to_string();
                wasm_bindgen_futures::spawn_local(async move {
                    let result = async {
                        let trx = ctx().begin();
                        let _ = edit_msg.edit(&trx)?.text().replace(&input_text);
                        trx.commit().await?;
                        Ok::<_, Box<dyn std::error::Error>>(())
                    }
                    .await;
                    match result {
                        Ok(_) => {
                            editing_message.set(None);
                            message_input.set(String::new());
                        }
                        Err(e) => tracing::error!("Failed to update message: {}", e),
                    }
                });
            } else {
                // Create a new message. ankurah stores user/room as typed Refs.
                let user_ref = ankurah::Ref::from(&user);
                let room_ref = ankurah::Ref::from(&room);
                let input_text = input_text.trim().to_string();
                wasm_bindgen_futures::spawn_local(async move {
                    let result = async {
                        let trx = ctx().begin();
                        let timestamp = js_sys::Date::now() as i64;
                        trx.create(&Message {
                            user: user_ref,
                            room: room_ref,
                            text: input_text,
                            timestamp,
                            deleted: false,
                        })
                        .await?;
                        trx.commit().await?;
                        Ok::<_, Box<dyn std::error::Error>>(())
                    }
                    .await;
                    match result {
                        Ok(_) => message_input.set(String::new()),
                        Err(e) => tracing::error!("Failed to send message: {}", e),
                    }
                });
            }
        }
    };

    // Select the previous/next message authored by the current user (for editing).
    let navigate_own = {
        let current_user = current_user.clone();
        move |backward: bool| {
            let Some(user) = current_user.clone() else { return };
            let user_id = user.id().to_base64();
            let msgs = messages.get_untracked();
            if msgs.is_empty() {
                return;
            }
            let is_own = |m: &MessageView| m.user().ok().map(|r| r.id().to_base64()).as_deref() == Some(user_id.as_str());

            let current_idx = editing_message
                .get()
                .and_then(|em| {
                    let id = em.id().to_base64();
                    msgs.iter().position(|m| m.id().to_base64() == id)
                });

            if backward {
                // Cmd/Ctrl+Up: search toward older messages (lower indices).
                let start = current_idx.unwrap_or(msgs.len());
                for i in (0..start).rev() {
                    if is_own(&msgs[i]) {
                        editing_message.set(Some(msgs[i].clone()));
                        return;
                    }
                }
            } else if let Some(start) = current_idx {
                // Cmd/Ctrl+Down: only meaningful while editing; search toward newer messages.
                for i in (start + 1)..msgs.len() {
                    if is_own(&msgs[i]) {
                        editing_message.set(Some(msgs[i].clone()));
                        return;
                    }
                }
                // Past the newest own message: exit edit mode.
                editing_message.set(None);
                message_input.set(String::new());
            }
        }
    };

    let handle_key_down = {
        let send = send.clone();
        move |e: KeyboardEvent| {
            if e.key() == "Enter" && !e.shift_key() {
                e.prevent_default();
                send();
            } else if e.key() == "Escape" && editing_message.get().is_some() {
                e.prevent_default();
                editing_message.set(None);
                message_input.set(String::new());
            } else if e.key() == "ArrowUp" && (e.meta_key() || e.ctrl_key()) {
                e.prevent_default();
                navigate_own(true);
            } else if e.key() == "ArrowDown" && (e.meta_key() || e.ctrl_key()) && editing_message.get().is_some() {
                e.prevent_default();
                navigate_own(false);
            }
        }
    };

    let send_click = send.clone();
    view! {
        <div class="inputContainer">
            <input
                type="text"
                class="input"
                placeholder="Type a message..."
                prop:value=move || message_input.get()
                on:input=move |ev| message_input.set(event_target_value(&ev))
                on:keydown=handle_key_down
                prop:disabled=move || !is_connected()
            />
            <button class="button" on:click=move |_| send_click() prop:disabled=move || !can_send()>
                {move || if editing_message.get().is_some() { "Update" } else { "Send" }}
            </button>
            <Show when=move || editing_message.get().is_some()>
                <button
                    class="button"
                    on:click=move |_| {
                        editing_message.set(None);
                        message_input.set(String::new());
                    }
                    style="margin-left: 8px"
                >
                    "Cancel"
                </button>
            </Show>
        </div>
    }
}
