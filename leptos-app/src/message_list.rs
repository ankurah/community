use leptos::prelude::*;

use ankurah::LiveQuery;
use community_model::{MessageView, UserView};

use crate::message_row::MessageRow;

/// Message list component that displays messages.
#[component]
pub fn MessageList(
    #[prop(into)] messages: Signal<Vec<MessageView>>,
    users: LiveQuery<UserView>,
    current_user_id: Option<String>,
    editing_message: RwSignal<Option<MessageView>>,
) -> impl IntoView {
    view! {
        <Show
            when=move || !messages.get().is_empty()
            fallback=|| {
                view! {
                    <div class="emptyState">
                        "No messages yet. Be the first to say hello!"
                    </div>
                }
            }
        >
            <For
                each=move || messages.get()
                key=|message: &MessageView| message.id()
                children={
                    let users = users.clone();
                    let current_user_id = current_user_id.clone();
                    move |message: MessageView| {
                        view! {
                            <MessageRow
                                message=message
                                users=users.clone()
                                current_user_id=current_user_id.clone()
                                editing_message=editing_message
                            />
                        }
                    }
                }
            />
        </Show>
    }
}
