use leptos::html::Textarea;
use leptos::prelude::*;
use web_sys::KeyboardEvent;

use ankurah_signals::Get as AnkurahGet;
use community_model::{Message, MessageView, RoomView, UserView};

use crate::{ctx, ws_client};

/// Cap on the auto-grown composer height (#50) — roughly eight lines of text;
/// beyond it the textarea scrolls internally instead of eating the timeline.
const MAX_COMPOSER_HEIGHT: i32 = 192;

/// Reply quotes are clipped to this many characters (#23).
const REPLY_SNIPPET_MAX: usize = 140;

/// Pending composer prefill (#23 reply). Plain shared state, deliberately NOT
/// a lazily-created leptos signal: a signal first created inside a context-menu
/// event handler would be registered under that menu's reactive Owner and
/// disposed when the menu closes. The requester instead pokes the composer by
/// re-setting `editing_message` (an unconditional notify), whose effect
/// consumes this. A module global (not a prop) because the composer is
/// instantiated in chat.rs, which wave-2 message work does not touch.
static COMPOSE_PREFILL: std::sync::RwLock<Option<String>> = std::sync::RwLock::new(None);

fn take_compose_prefill() -> Option<String> {
    COMPOSE_PREFILL.write().ok().and_then(|mut slot| slot.take())
}

/// Start a reply to `author`'s message (#23, v1 text convention): prefill the
/// composer with an editable quoted snippet — Slack-style — rather than a
/// durable reference. A `re: Ref<Message>` model field is a fast-follow; no
/// Message field stores the referenced id today.
///
/// Cancels any in-progress edit: a reply composes a NEW message, and the
/// `editing_message` write doubles as the composer's wake-up call.
pub fn request_reply_prefill(author: &str, text: &str, editing_message: RwSignal<Option<MessageView>>) {
    // Single-line snippet: newlines and runs of whitespace collapse, so the
    // quote stays one `>` line even when quoting multiline/code messages.
    let mut snippet: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if snippet.chars().count() > REPLY_SNIPPET_MAX {
        snippet = snippet.chars().take(REPLY_SNIPPET_MAX).collect::<String>().trim_end().to_string() + "\u{2026}";
    }
    let prefill = format!("> **{author}**: {snippet}\n\n");
    if let Ok(mut slot) = COMPOSE_PREFILL.write() {
        *slot = Some(prefill);
    }
    editing_message.set(None);
}

/// Fit the composer textarea to its content, up to [`MAX_COMPOSER_HEIGHT`].
/// Collapse-to-auto first so shrinking works (scrollHeight never shrinks
/// below the styled height on its own).
fn autosize(el: &web_sys::HtmlTextAreaElement) {
    // Fully qualified: leptos' `ElementExt::style` extension shadows the
    // web_sys inherent getter in this scope.
    let style = web_sys::HtmlElement::style(el);
    let _ = style.set_property("height", "auto");
    // scrollHeight covers content + padding; the element is border-box, so
    // add the border (offset − client) to avoid a 2px internal scroll.
    let border = el.offset_height() - el.client_height();
    let content = el.scroll_height() + border;
    let clamped = content.min(MAX_COMPOSER_HEIGHT);
    let _ = style.set_property("height", &format!("{clamped}px"));
    let _ = style.set_property("overflow-y", if content > MAX_COMPOSER_HEIGHT { "auto" } else { "hidden" });
}

/// Message input component for sending and editing messages.
/// The composer is a multiline textarea (#50): Enter sends, Shift+Enter
/// inserts a newline, Escape cancels an edit, and Cmd/Ctrl+Up/Down navigates
/// the viewer's own messages for editing.
#[component]
pub fn MessageInput(
    room: RoomView,
    current_user: Option<UserView>,
    editing_message: RwSignal<Option<MessageView>>,
    /// Current visible messages (oldest-first), used for Cmd/Ctrl+Up/Down navigation.
    #[prop(into)] messages: Signal<Vec<MessageView>>,
) -> impl IntoView {
    let message_input = RwSignal::new(String::new());
    let textarea_ref = NodeRef::<Textarea>::new();

    // Live connection state from the WebSocket client (reactive via the observer bridge).
    let connection_status = move || ws_client().connection_state().get().to_string();
    let is_connected = move || connection_status() == "Connected";
    let can_send = move || !message_input.get().trim().is_empty() && is_connected();

    // One effect owns every externally-driven composer fill — the edit mirror
    // and the reply prefill (#23) — so the two sources cannot clobber each
    // other across effect runs. `prev` carries the editing-message id from
    // the previous run: the mirror only fires on an actual edit transition,
    // and the prefill branch returns the current id so the transition that
    // delivered it (reply cancels any edit) is not re-mirrored into a clear.
    Effect::new(move |prev: Option<Option<String>>| {
        let editing = editing_message.get();
        let editing_id = editing.as_ref().map(|m| m.id().to_base64());

        if let Some(text) = take_compose_prefill() {
            message_input.set(text);
            if let Some(el) = textarea_ref.get_untracked() {
                let _ = el.focus();
            }
            return editing_id;
        }

        if prev.map(|p| p != editing_id).unwrap_or(true) {
            match &editing {
                Some(edit_msg) => message_input.set(edit_msg.text().unwrap_or_default()),
                None => message_input.set(String::new()),
            }
        }
        editing_id
    });

    // Refit the textarea after every programmatic content change (edit-mirror
    // fill, clear-after-send). Effects run after the render effect has pushed
    // the new value into the DOM, so scrollHeight is current. Typing is
    // covered separately by the on:input handler for zero-lag growth.
    Effect::new(move |_| {
        let _ = message_input.get();
        if let Some(el) = textarea_ref.get_untracked() {
            autosize(&el);
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
                        // No-op edits commit nothing (and earn no "(edited)" marker).
                        if edit_msg.text().unwrap_or_default() == input_text {
                            return Ok(());
                        }
                        let trx = ctx().begin();
                        let mutable = edit_msg.edit(&trx)?;
                        mutable.text().replace(&input_text)?;
                        // Stamp the edit for the "(edited)" indicator (#11).
                        mutable.edited_at().set(&Some(js_sys::Date::now() as i64))?;
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
                            edited_at: None,
                            collaborative: None,
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
            // Tombstones are not editable — skip them while navigating (#10).
            let is_own = |m: &MessageView| {
                m.user().ok().map(|r| r.id().to_base64()).as_deref() == Some(user_id.as_str())
                    && !m.deleted().unwrap_or(false)
            };

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
            // Enter sends; Shift+Enter falls through to the textarea's native
            // newline (#50). An Enter that confirms an IME composition
            // (isComposing) must not send the message.
            if e.key() == "Enter" && !e.shift_key() && !e.is_composing() {
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
            <Show when=move || editing_message.get().is_some()>
                <div class="editingNotice">
                    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"
                        stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
                        <path d="M17 3a2.8 2.8 0 1 1 4 4L7.5 20.5 2 22l1.5-5.5z" />
                    </svg>
                    <span>"Editing message"</span>
                    <span class="editingNoticeHint">
                        <kbd>"Esc"</kbd>
                        " to cancel"
                    </span>
                </div>
            </Show>
            <div class="inputRow">
                // Multiline composer (#50). Keeps class="input" + the same
                // placeholder: e2e locates it by `.input[placeholder=...]`.
                <textarea
                    node_ref=textarea_ref
                    class="input"
                    placeholder="Type a message..."
                    rows="1"
                    aria-label="Message"
                    prop:value=move || message_input.get()
                    on:input=move |ev| {
                        message_input.set(event_target_value(&ev));
                        if let Some(el) = textarea_ref.get_untracked() {
                            autosize(&el);
                        }
                    }
                    on:keydown=handle_key_down
                    prop:disabled=move || !is_connected()
                ></textarea>
                <Show when=move || editing_message.get().is_some()>
                    <button
                        class="button buttonGhost"
                        on:click=move |_| {
                            editing_message.set(None);
                            message_input.set(String::new());
                        }
                    >
                        "Cancel"
                    </button>
                </Show>
                <button class="button sendButton" on:click=move |_| send_click() prop:disabled=move || !can_send()>
                    {move || if editing_message.get().is_some() { "Update" } else { "Send" }}
                </button>
            </div>
        </div>
    }
}
