use leptos::ev::MouseEvent;
use leptos::prelude::*;

use ankurah::LiveQuery;
use ankurah_signals::Get as AnkurahGet;
use community_model::{MessageView, UserView};

use crate::fmt;
use crate::message_context_menu::MessageContextMenu;

/// Individual message row: optional day divider, avatar gutter (others only),
/// author/time meta on the first message of a group, and the bubble itself.
///
/// Structural contract: `.messageBubble` carries `data-msg-id` (the virtual
/// scroller and e2e tests find rows by it) and hosts the context-menu handler.
#[component]
pub fn MessageRow(
    message: MessageView,
    users: LiveQuery<UserView>,
    current_user_id: Option<String>,
    editing_message: RwSignal<Option<MessageView>>,
    first_in_group: bool,
    last_in_group: bool,
    day_label: Option<String>,
) -> impl IntoView {
    let context_menu = RwSignal::new(None::<(i32, i32)>);

    // Clone values that will be used in multiple closures
    let message_for_author = message.clone();
    let message_for_editing = message.clone();
    let message_for_own = message.clone();
    let current_user_id_for_own = current_user_id.clone();

    // Stable author id (a Ref on the message; not reactive).
    let author_user_id = message.user().map(|r| r.id().to_base64()).unwrap_or_default();

    // Find the author from the users list (reactive: display names can change).
    let author = move || {
        let user_list = users.get();
        let message_user = message_for_author.user().map(|r| r.id().to_base64()).unwrap_or_default();
        user_list.iter().find(|u| u.id().to_base64() == message_user).cloned()
    };

    let is_own_message = current_user_id_for_own
        .as_ref()
        .map(|id| message_for_own.user().ok().map(|r| r.id().to_base64()).as_deref() == Some(id.as_str()))
        .unwrap_or(false);

    // Whether the viewer can act on this message at all (own message, or any
    // message for moderators). UI gating only — the server enforces the policy.
    let can_act = is_own_message || crate::can_moderate();

    // The visible "⋯" affordance (#16): keyboard/hover path to the same menu.
    let trigger_ref = NodeRef::<leptos::html::Button>::new();
    // Whether the menu was opened from the trigger (vs right-click); governs
    // whether closing returns focus to the trigger.
    let opened_from_trigger = StoredValue::new(false);
    // Snapshot at mousedown: the menu's own outside-mousedown listener closes
    // it before our click fires, so without this a click on the trigger of an
    // open menu would instantly reopen it instead of toggling it closed.
    let menu_was_open_at_mousedown = StoredValue::new(false);

    // Right-click opens the menu on your own messages; moderators can open it
    // on anyone's (UI gating only — the server enforces the write policy).
    let handle_context_menu = move |e: MouseEvent| {
        e.prevent_default();
        if can_act {
            opened_from_trigger.set_value(false);
            context_menu.set(Some((e.client_x(), e.client_y())));
        }
    };

    let open_from_trigger = move |e: MouseEvent| {
        e.stop_propagation();
        if menu_was_open_at_mousedown.get_value() {
            return; // toggle-off: the outside-mousedown listener already closed it
        }
        if let Some(btn) = trigger_ref.get_untracked() {
            let rect = btn.get_bounding_client_rect();
            opened_from_trigger.set_value(true);
            context_menu.set(Some((rect.left() as i32, rect.bottom() as i32 + 6)));
        }
    };

    let is_editing =
        move || editing_message.get().as_ref().map(|em| em.id().to_base64() == message_for_editing.id().to_base64()).unwrap_or(false);

    let message_id = message.id().to_base64();
    let message_for_text = message.clone();
    let ts = message.timestamp().unwrap_or(0);
    let time_str = fmt::clock_time(ts);
    let stamp = fmt::full_stamp(ts);

    // Static per-row layout classes (grouping context is baked into the For key).
    let row_class = {
        let mut c = String::from("messageRow");
        if is_own_message {
            c.push_str(" own");
        }
        if first_in_group {
            c.push_str(" groupFirst");
        }
        if last_in_group {
            c.push_str(" groupLast");
        }
        c
    };

    let avatar_hue = fmt::hue_class(&author_user_id);
    let author_for_avatar = author.clone();
    let author_for_name = author.clone();

    view! {
        {day_label.map(|label| {
            view! {
                <div class="dayDivider" aria-hidden="true">
                    <span class="dayDividerLabel">{label}</span>
                </div>
            }
        })}
        <div class=row_class>
            {(!is_own_message)
                .then(|| {
                    view! {
                        <div class="messageGutter">
                            {first_in_group
                                .then(|| {
                                    view! {
                                        <div class=format!("avatar {}", avatar_hue) aria-hidden="true">
                                            {move || {
                                                fmt::initials(
                                                    &author_for_avatar()
                                                        .map(|u| u.display_name().unwrap_or_default())
                                                        .unwrap_or_default(),
                                                )
                                            }}
                                        </div>
                                    }
                                })}
                        </div>
                    }
                })}
            <div class="messageMain">
                {first_in_group
                    .then(|| {
                        if is_own_message {
                            view! {
                                <div class="messageMeta ownMeta">
                                    <span class="messageTime">{time_str.clone()}</span>
                                </div>
                            }
                                .into_any()
                        } else {
                            view! {
                                <div class="messageMeta">
                                    <span class="messageAuthor">
                                        {move || {
                                            author_for_name()
                                                .map(|u| u.display_name().unwrap_or_default())
                                                .filter(|n| !n.is_empty())
                                                .unwrap_or_else(|| "Unknown".to_string())
                                        }}
                                    </span>
                                    <span class="messageTime">{time_str.clone()}</span>
                                </div>
                            }
                                .into_any()
                        }
                    })}
                <div
                    class=move || {
                        let mut classes = vec!["messageBubble"];
                        if is_editing() {
                            classes.push("editing");
                        }
                        if is_own_message {
                            classes.push("ownMessage");
                        }
                        classes.join(" ")
                    }
                    data-msg-id=message_id.clone()
                    title=stamp
                    on:contextmenu=handle_context_menu
                >
                    // Reactive read: CRDT text edits (local or remote) re-render
                    // the bubble; markdown parses only when the text changes.
                    <div class="messageText">
                        {move || crate::markdown::render_message(&message_for_text.text().unwrap_or_default())}
                    </div>
                    {can_act
                        .then(|| {
                            view! {
                                <button
                                    node_ref=trigger_ref
                                    type="button"
                                    class="messageActions"
                                    aria-haspopup="menu"
                                    aria-label="Message actions"
                                    aria-expanded=move || {
                                        if context_menu.get().is_some() { "true" } else { "false" }
                                    }
                                    title="Message actions"
                                    on:mousedown=move |_| {
                                        menu_was_open_at_mousedown
                                            .set_value(context_menu.get_untracked().is_some())
                                    }
                                    on:click=open_from_trigger
                                >
                                    <svg viewBox="0 0 24 24" fill="currentColor" aria-hidden="true">
                                        <circle cx="5" cy="12" r="1.9" />
                                        <circle cx="12" cy="12" r="1.9" />
                                        <circle cx="19" cy="12" r="1.9" />
                                    </svg>
                                </button>
                            }
                        })}
                </div>
                <Show when=move || context_menu.get().is_some()>
                    {
                        let message = message.clone();
                        move || {
                            context_menu.get().map(|(x, y)| {
                                view! {
                                    <MessageContextMenu
                                        x=x
                                        y=y
                                        message=message.clone()
                                        editing_message=editing_message
                                        is_own=is_own_message
                                        on_close=move || {
                                            context_menu.set(None);
                                            // Keyboard path: hand focus back to the trigger.
                                            if opened_from_trigger.get_value() {
                                                if let Some(btn) = trigger_ref.get_untracked() {
                                                    let _ = btn.focus();
                                                }
                                            }
                                        }
                                    />
                                }
                            })
                        }
                    }
                </Show>
            </div>
        </div>
    }
}
