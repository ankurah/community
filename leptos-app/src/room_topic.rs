//! Current room's topic in the header (issue #12): muted, truncated, full
//! text on hover. Editing is gated client-side to moderators or the room's
//! creator — the server enforces the real rule via the room write scope in
//! policy.json (`created_by = $jwt.sub` unless `moderate`), so the gate here
//! is purely to avoid offering an edit that would be rejected.
//!
//! Deliberately its own minimal inline editor rather than reusing
//! `EditableTextField` (that file belongs to the messages branch).

use leptos::prelude::*;
use web_sys::KeyboardEvent;

use community_model::RoomView;

use crate::{can_moderate, ctx, current_user_id};

#[component]
pub fn RoomTopic(room: RwSignal<Option<RoomView>>) -> impl IntoView {
    let editing = RwSignal::new(false);
    let draft = RwSignal::new(String::new());

    // Reactive topic text: tracks both room switches (Leptos signal) and
    // remote topic edits (the LWW property signal, via ReactiveGraphObserver).
    let topic = move || room.get().and_then(|r| r.topic().ok().flatten()).filter(|t| !t.trim().is_empty());

    let can_edit = move || {
        room.get()
            .map(|r| {
                can_moderate() || r.created_by().ok().flatten().map(|c| c.id()) == Some(current_user_id())
            })
            .unwrap_or(false)
    };

    let start_edit = move |_| {
        if !can_edit() {
            return;
        }
        draft.set(topic().unwrap_or_default());
        editing.set(true);
    };

    let save = move || {
        editing.set(false);
        let Some(r) = room.get_untracked() else { return };
        let new_topic = {
            let text = draft.get_untracked().trim().to_string();
            if text.is_empty() { None } else { Some(text) }
        };
        if new_topic == r.topic().ok().flatten().filter(|t| !t.trim().is_empty()) {
            return; // unchanged — skip the write
        }
        wasm_bindgen_futures::spawn_local(async move {
            let result = async {
                let trx = ctx().begin();
                r.edit(&trx)?.topic().set(&new_topic)?;
                trx.commit().await?;
                Ok::<_, Box<dyn std::error::Error>>(())
            }
            .await;
            if let Err(e) = result {
                tracing::error!("Failed to update room topic: {}", e);
            }
        });
    };

    let handle_key = move |ev: KeyboardEvent| match ev.key().as_str() {
        "Enter" => {
            ev.prevent_default();
            save();
        }
        "Escape" => editing.set(false),
        _ => {}
    };

    view! {
        <Show when=move || room.get().is_some()>
            <div class="headerRoom">
                <span class="headerRoomName">
                    "#" {move || room.get().map(|r| r.name().unwrap_or_default()).unwrap_or_default()}
                </span>
                <Show
                    when=move || editing.get()
                    fallback=move || {
                        view! {
                            <Show
                                when=move || topic().is_some() || can_edit()
                                fallback=|| ()
                            >
                                <button
                                    class=move || if can_edit() { "headerTopic editable" } else { "headerTopic" }
                                    title=move || match (topic(), can_edit()) {
                                        (Some(t), true) => format!("{t} — click to edit"),
                                        (Some(t), false) => t,
                                        (None, _) => "Set the room topic".to_string(),
                                    }
                                    on:click=start_edit
                                >
                                    <span class=move || {
                                        if topic().is_some() { "headerTopicText" } else { "headerTopicText placeholder" }
                                    }>
                                        {move || topic().unwrap_or_else(|| "Add a topic…".to_string())}
                                    </span>
                                    <Show when=can_edit>
                                        <svg class="headerTopicPencil" viewBox="0 0 24 24" fill="none"
                                            stroke="currentColor" stroke-width="2" stroke-linecap="round"
                                            stroke-linejoin="round" aria-hidden="true">
                                            <path d="M12 20h9" />
                                            <path d="M16.5 3.5a2.1 2.1 0 0 1 3 3L7 19l-4 1 1-4Z" />
                                        </svg>
                                    </Show>
                                </button>
                            </Show>
                        }
                    }
                >
                    <input
                        class="headerTopicInput"
                        type="text"
                        placeholder="Room topic…"
                        maxlength="200"
                        prop:value=move || draft.get()
                        on:input=move |ev| draft.set(event_target_value(&ev))
                        on:keydown=handle_key
                        on:blur=move |_| editing.set(false)
                        autofocus
                    />
                </Show>
            </div>
        </Show>
    }
}
