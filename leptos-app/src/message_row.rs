use leptos::ev::MouseEvent;
use leptos::prelude::*;

use ankurah::LiveQuery;
use ankurah_signals::Get as AnkurahGet;
use community_model::{MessageView, UserView};

use crate::message_context_menu::MessageContextMenu;

/// Individual message row component.
/// Displays message text, author name, and context menu on right-click for own messages.
#[component]
pub fn MessageRow(
    message: MessageView,
    users: LiveQuery<UserView>,
    current_user_id: Option<String>,
    editing_message: RwSignal<Option<MessageView>>,
) -> impl IntoView {
    let context_menu = RwSignal::new(None::<(i32, i32)>);

    // Clone values that will be used in multiple closures
    let message_for_author = message.clone();
    let message_for_context = message.clone();
    let message_for_editing = message.clone();
    let message_for_own = message.clone();
    let current_user_id_for_context = current_user_id.clone();
    let current_user_id_for_own = current_user_id.clone();

    // Find the author from the users list
    let author = move || {
        let user_list = users.get();
        let message_user = message_for_author.user().map(|r| r.id().to_base64()).unwrap_or_default();
        user_list.iter().find(|u| u.id().to_base64() == message_user).cloned()
    };

    let handle_context_menu = move |e: MouseEvent| {
        e.prevent_default();
        if let Some(ref current_id) = current_user_id_for_context {
            if message_for_context.user().ok().map(|r| r.id().to_base64()).as_deref() == Some(current_id.as_str()) {
                context_menu.set(Some((e.client_x(), e.client_y())));
            }
        }
    };

    let is_editing =
        move || editing_message.get().as_ref().map(|em| em.id().to_base64() == message_for_editing.id().to_base64()).unwrap_or(false);

    let is_own_message = current_user_id_for_own
        .as_ref()
        .map(|id| message_for_own.user().ok().map(|r| r.id().to_base64()).as_deref() == Some(id.as_str()))
        .unwrap_or(false);

    let message_id = message.id().to_base64();
    let message_text = message.text().unwrap_or_default();

    view! {
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
            on:contextmenu=handle_context_menu
        >
            <Show when=move || !is_own_message fallback=|| ()>
                {
                    let author = author.clone();
                    move || view! {
                        <div class="messageHeader">
                            <span class="messageAuthor">
                                {author().map(|u| u.display_name().unwrap_or_default()).unwrap_or_else(|| "Unknown".to_string())}
                            </span>
                        </div>
                    }
                }
            </Show>
            <div class="messageText">{message_text.clone()}</div>
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
                                    on_close=move || context_menu.set(None)
                                />
                            }
                        })
                    }
                }
            </Show>
        </div>
    }
}
