//! The "Direct messages" sidebar section (#30), below the room list.
//!
//! The resultset is SELF-SHAPING: the `dm_thread` read scope is
//! `a = $jwt.sub OR b = $jwt.sub`, so a plain `deleted = false` LiveQuery
//! returns exactly the viewer's own threads and nothing else. There is no
//! client-side membership filter on purpose — one would read as though the
//! privacy came from this file, and it does not: it comes from policy.json,
//! and `server/tests/dm_policy_live_tests.rs` is where that is proven.
//!
//! Each row names the OTHER participant and carries an unread badge from the
//! viewer's own `DmReadState` cursor. Rows are ordered by their newest message,
//! most recent first, which is derived from the same per-thread windows the
//! unread counts come from — `DmThread` deliberately carries no `last_msg_ts`
//! field, because two participants racing to maintain one would be a write
//! conflict on every message for no reader benefit.
//!
//! Threads with no messages do not appear. That is the containment for empty
//! thread rows: anyone may open a thread with anyone (the write scope only
//! stops you opening threads between OTHER people), so a thread row on its own
//! must not be able to occupy space in a stranger's sidebar. The conversation
//! appears for the recipient when the first message does — which is also the
//! event the DM rate limiter counts.

use leptos::prelude::*;

use ankurah::LiveQuery;
use ankurah_signals::Get as AnkurahGet;
use community_model::{DmThreadView, UserView};

use crate::{current_user_id, dm, dm_read_state::DmReadStateManager, fmt};

#[component]
pub fn DmList(
    threads: LiveQuery<DmThreadView>,
    users: LiveQuery<UserView>,
    selected_dm: RwSignal<Option<DmThreadView>>,
    read_state: DmReadStateManager,
) -> impl IntoView {
    let me = current_user_id();

    // Duplicate threads from a concurrent first-DM race collapse to the
    // canonical one, so a correspondent never appears twice; rows with no
    // messages yet are hidden, and the rest sort by most recent activity.
    let rows = {
        let threads = threads.clone();
        let read_state = read_state.clone();
        Signal::derive(move || {
            let mut rows: Vec<(DmThreadView, i64)> = dm::canonical_threads(&threads.get())
                .into_iter()
                .map(|t| {
                    let newest = read_state.newest_ts(&t.id().to_base64());
                    (t, newest)
                })
                .filter(|(_, newest)| *newest > 0)
                .collect();
            rows.sort_by(|(a_thread, a_ts), (b_thread, b_ts)| b_ts.cmp(a_ts).then_with(|| a_thread.id().cmp(&b_thread.id())));
            rows.into_iter().map(|(t, _)| t).collect::<Vec<_>>()
        })
    };

    let rows_for_empty = rows;

    view! {
        <div class="sidebarHeader dmSectionHeader">
            <span class="sidebarTitle">"Direct messages"</span>
        </div>
        <div class="roomList dmList">
            <Show when=move || rows_for_empty.get().is_empty()>
                <div class="emptyRooms">
                    "No conversations yet — open a member and choose Message."
                </div>
            </Show>
            <For
                each=move || rows.get()
                key=|thread: &DmThreadView| thread.id()
                children={
                    let users = users.clone();
                    let read_state = read_state.clone();
                    move |thread: DmThreadView| {
                        view! {
                            <DmListItem
                                thread=thread
                                users=users.clone()
                                selected_dm=selected_dm
                                read_state=read_state.clone()
                                me=me
                            />
                        }
                    }
                }
            />
        </div>
    }
}

#[component]
fn DmListItem(
    thread: DmThreadView,
    users: LiveQuery<UserView>,
    selected_dm: RwSignal<Option<DmThreadView>>,
    read_state: DmReadStateManager,
    me: ankurah::EntityId,
) -> impl IntoView {
    let thread_id = thread.id().to_base64();
    let partner = dm::partner_of(&thread, me);

    // Reactive: a rename retitles the row without a reload.
    let partner_name = {
        let users = users.clone();
        move || match partner {
            Some(p) => {
                let _ = users.get();
                dm::display_name(&users, p)
            }
            // A self-thread has no other participant. The UI never creates
            // one; naming it honestly beats rendering "Unknown".
            None => "You".to_string(),
        }
    };
    let partner_name_for_initials = partner_name.clone();
    let hue = fmt::hue_class(&partner.map(|p| p.to_base64()).unwrap_or_default());

    let thread_id_selected = thread_id.clone();
    let is_selected = move || selected_dm.get().as_ref().map(|t| t.id().to_base64() == thread_id_selected).unwrap_or(false);

    let thread_for_click = thread.clone();
    let thread_id_badge = thread_id.clone();

    view! {
        <div
            class=move || if is_selected() { "roomItem dmItem selected" } else { "roomItem dmItem" }
            on:click=move |_| selected_dm.set(Some(thread_for_click.clone()))
        >
            <span class=format!("dmAvatar {}", hue) aria-hidden="true">
                {move || fmt::initials(&partner_name_for_initials())}
            </span>
            <span class="roomLabel">{partner_name}</span>
            {move || {
                let unread_count = read_state.unread_count(&thread_id_badge);
                (unread_count > 0).then(|| {
                    let badge_text = if unread_count >= 10 { "10+".to_string() } else { unread_count.to_string() };
                    view! { <span class="unreadBadge">{badge_text}</span> }
                })
            }}
        </div>
    }
}
