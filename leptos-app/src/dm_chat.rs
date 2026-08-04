//! The DM thread view (#30) — the room chat layout, pointed at one thread.
//!
//! It stands on the same [`crate::scroll_pane::ScrollPane`] as room chat, so
//! the pinned-to-bottom contract, pagination and the `data-msg-id` DOM contract
//! are literally the same code rather than a parallel implementation. The
//! composer is the same component too ([`crate::message_input::MessageInput`]),
//! with a DM target: mention autocomplete, `@Name` re-encoding at send and
//! `:emoji:` completion all behave exactly as they do in a room.
//!
//! What it does NOT ship in v1, deliberately: edit, delete, reply, reactions,
//! link previews and the message context menu. Those are room affordances with
//! their own model fields and workers; leaving them out keeps this lane to the
//! DM primitives rather than re-litigating each of them for a second
//! collection. `DmMessage.edited_at` is rendered when present so the field is
//! honest on the read side, but no client path writes it yet.
//!
//! THE FILING KEY IS `thread`, NEVER `a`/`b`. The participants are denormalized
//! onto every message so the policy read scope can answer "may this user see
//! me" row-locally — they are not an index. A message whose `a`/`b` disagree
//! with its thread (which a participant CAN hand-craft; see the `DmMessage`
//! model doc) is simply mis-filed into a view nobody looks at, and that
//! containment is exactly this predicate.

use leptos::prelude::*;

use ankurah::LiveQuery;
use ankurah_signals::Get as AnkurahGet;
use community_model::{DmMessageView, DmThreadView, MessageView, UserView};

use crate::{
    dm, dm_message_list::DmMessageList, dm_read_state::DmReadStateManager, message_input::{ComposerTarget, MessageInput},
    scroll_pane::ScrollPane,
};

#[component]
pub fn DmChat(
    thread: RwSignal<Option<DmThreadView>>,
    /// The viewer's whole thread set, so an open conversation can be read
    /// across every row its pair has (see [`crate::dm::pair_rows`]).
    threads: LiveQuery<DmThreadView>,
    current_user: RwSignal<Option<UserView>>,
    users: LiveQuery<UserView>,
    read_state: DmReadStateManager,
) -> impl IntoView {
    let pane = ScrollPane::<DmMessageView>::new();
    pane.install();

    // Which rows this conversation is spread across: normally just the
    // selected one, and more when a first-DM race left twins. Reactive,
    // because the losing twin can arrive after the view is already open.
    let rows = {
        let threads = threads.clone();
        Signal::derive(move || match thread.get() {
            Some(t) => dm::pair_rows(&threads.get(), &t),
            None => Vec::new(),
        })
    };

    // The composer's edit/reply state. DM v1 never arms either — they exist
    // because the composer is shared with room chat — and owning them here
    // (rather than sharing the room pane's) is what guarantees that a reply
    // armed in a room can never follow the reader into a private thread.
    let editing_message = RwSignal::new(None::<MessageView>);
    let replying_to = RwSignal::new(None::<MessageView>);
    let no_room_messages = Signal::derive(Vec::<MessageView>::new);

    Effect::new(move |_| {
        // The timeline is the union of the pair's rows, so a message written
        // into a race twin before the clients agreed on a winner is still part
        // of the conversation the reader sees. With no twins — the normal case
        // — this is the plain `thread = ?` it has always been.
        //
        // Tombstones stay in the timeline like room tombstones (#10), so the
        // scroll shape does not jump when one appears.
        let ids = rows.get();
        let predicate = (!ids.is_empty()).then(|| {
            let src = vec!["thread = ?"; ids.len()].join(" OR ");
            crate::queries::predicate(&src, ids.iter().map(|id| id.into())).expect("dm message predicate parses")
        });
        pane.set_source(predicate, "timestamp DESC");
    });

    let messages = pane.items;

    let partner_name = {
        let users = users.clone();
        Signal::derive(move || {
            let Some(t) = thread.get() else { return String::new() };
            // Track display-name edits: a rename retitles the open thread.
            let _ = users.get();
            match dm::partner_of(&t, crate::current_user_id()) {
                Some(partner) => dm::display_name(&users, partner),
                None => "Yourself".to_string(),
            }
        })
    };

    // Advance the read cursor whenever the viewer is at the live tail — the
    // room rule, per thread. Every row of the pair gets the cursor, because
    // the sidebar's badge counts across all of them: leaving a twin's cursor
    // behind would leave a badge nothing can clear.
    let mark_read_at_tail = {
        let read_state = read_state.clone();
        move || {
            let Some(ts) = newest_timestamp(&messages.get_untracked()) else { return };
            for id in rows.get_untracked() {
                read_state.mark_read(&id.to_base64(), ts);
            }
        }
    };
    Effect::new({
        let mark_read_at_tail = mark_read_at_tail.clone();
        move |_| {
            let _ = messages.get();
            if pane.is_live() {
                mark_read_at_tail();
            }
        }
    });

    view! {
        {
            let users = users.clone();
            let mark_read_at_tail = mark_read_at_tail.clone();
            move || {
                let current_thread = thread.get()?;
                let current_user_id = current_user.get().map(|u| u.id().to_base64());
                let users = users.clone();

                let handle_scroll = pane.scroll_handler(mark_read_at_tail.clone());
                let handle_jump = {
                    let mark_read_at_tail = mark_read_at_tail.clone();
                    move |_| {
                        pane.scroll_to_bottom();
                        mark_read_at_tail();
                    }
                };

                Some(view! {
                    <div class="chatContainer">
                        <div class="dmThreadHeader">
                            <span class="dmThreadWith">{move || partner_name.get()}</span>
                            <span class="dmThreadPrivacy" title="Only the two of you can read this conversation — not moderators.">
                                <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"
                                    stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
                                    <rect x="4" y="11" width="16" height="9" rx="2" />
                                    <path d="M8 11V7a4 4 0 0 1 8 0v4" />
                                </svg>
                                "Private"
                            </span>
                        </div>

                        <div class="messagesContainer" node_ref=pane.container_ref on:scroll=handle_scroll>
                            <div class="messagesContent" node_ref=pane.content_ref>
                                <DmMessageList
                                    messages=messages
                                    users=users.clone()
                                    current_user_id=current_user_id.clone()
                                    partner_name=partner_name
                                />
                            </div>
                        </div>

                        <Show when=move || pane.show_jump_to_current.get()>
                            <button class="jumpToCurrent" on:click=handle_jump.clone()>
                                "Jump to latest"
                                <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.4"
                                    stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
                                    <path d="M12 5v14" />
                                    <path d="m6 13 6 6 6-6" />
                                </svg>
                            </button>
                        </Show>

                        <MessageInput
                            target=ComposerTarget::Dm(current_thread.clone())
                            current_user=current_user.get()
                            editing_message=editing_message
                            replying_to=replying_to
                            messages=no_room_messages
                        />
                    </div>
                })
            }
        }
    }
}

fn newest_timestamp(messages: &[DmMessageView]) -> Option<i64> {
    messages.iter().filter_map(|m| m.timestamp().ok()).max()
}
