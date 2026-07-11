//! Public moderation log (issue #10, panel half): a world-readable list of
//! `ModAction` rows, newest first. Everyone sees the same log — that is the
//! point (the policy gives `modaction` read to every member, write only to
//! `moderate`). The messages branch writes the rows at moderation time; this
//! panel only renders them.
//!
//! The message reference is a short id with the full id as hover title —
//! deep-linking/scroll-to-message can come later.

use std::collections::HashMap;

use leptos::prelude::*;

use ankurah_signals::Get as AnkurahGet;
use community_model::{ModActionView, UserView};

use crate::{ctx, fmt};

#[component]
pub fn ModLogPanel(on_close: impl Fn() + Clone + 'static) -> impl IntoView {
    // The whole log, live — new actions appear while the panel is open. The
    // log is small by nature (one row per moderator action, ever), so a full
    // subscription is fine; revisit with LIMIT/pagination if it ever grows.
    let actions = ctx().query::<ModActionView>("true ORDER BY created_at DESC").expect("failed to create ModActionView LiveQuery");

    // Actor names, resolved through the same users-map idiom as MembersPanel.
    let users = ctx().query::<UserView>("true").expect("failed to create UserView LiveQuery");
    let names_by_user = Memo::new(move |_| {
        users
            .get()
            .iter()
            .map(|u| (u.id().to_base64(), u.display_name().unwrap_or_default()))
            .collect::<HashMap<String, String>>()
    });

    let actions_for_loading = actions.clone();
    let actions_for_empty = actions.clone();
    let actions_for_list = actions.clone();

    let on_close_overlay = on_close.clone();
    let on_close_button = on_close.clone();

    view! {
        <div class="membersOverlay" on:click=move |_| on_close_overlay()>
            <div class="membersContent modLogContent" on:click=|e| e.stop_propagation()>
                <div class="membersHeader">
                    <div class="membersTitles">
                        <h2>"Moderation log"</h2>
                        <p class="membersSubtitle">"Every moderator action, visible to everyone."</p>
                    </div>
                    <button class="membersCloseButton" aria-label="Close" on:click=move |_| on_close_button()>
                        "×"
                    </button>
                </div>

                <div class="membersList modLogList">
                    <Show when=move || !actions_for_loading.loaded()>
                        <div class="membersState">"Loading moderation log\u{2026}"</div>
                    </Show>
                    <Show when={
                        let actions = actions_for_empty.clone();
                        move || actions.loaded() && actions.get().is_empty()
                    }>
                        <div class="membersState">"No moderation actions — nothing hidden."</div>
                    </Show>
                    <For
                        each={
                            let actions = actions_for_list.clone();
                            move || {
                                let mut items = actions.get();
                                // Belt and braces: the query orders newest-first, but
                                // resultset iteration order is not contractual.
                                items.sort_by_cached_key(|a| std::cmp::Reverse(a.created_at().unwrap_or(0)));
                                items
                            }
                        }
                        key=|action: &ModActionView| action.id()
                        children=move |action: ModActionView| {
                            view! { <ModLogRow action names_by_user /> }
                        }
                    />
                </div>

                <p class="membersNote">
                    "Deleted messages are hidden, never erased — every action lands here."
                </p>
            </div>
        </div>
    }
}

/// One log row: actor avatar + name, what they did, the optional reason, and
/// when. Message-targeted rows carry the message's short id (full id on
/// hover); user-targeted rows ("ban"/"unban") name the member instead.
#[component]
fn ModLogRow(action: ModActionView, names_by_user: Memo<HashMap<String, String>>) -> impl IntoView {
    let actor_id = action.actor().map(|a| a.id().to_base64()).unwrap_or_default();
    let hue = fmt::hue_class(&actor_id);

    let actor_id_for_name = actor_id.clone();
    let actor_name = move || {
        names_by_user.with(|map| {
            map.get(&actor_id_for_name)
                .filter(|n| !n.trim().is_empty())
                .cloned()
                .unwrap_or_else(|| "Unknown".to_string())
        })
    };
    let actor_name_for_initials = actor_name.clone();

    // The target user's name, for user-targeted rows (live, like the actor's).
    let target_user_id = action.user().ok().flatten().map(|u| u.id().to_base64());
    let target_name = target_user_id.clone().map(|id| {
        move || {
            names_by_user
                .with(|map| map.get(&id).filter(|n| !n.trim().is_empty()).cloned().unwrap_or_else(|| "Unknown".to_string()))
        }
    });

    let verb = match action.action().unwrap_or_default().as_str() {
        "delete" => "removed a message".to_string(),
        "restore" => "restored a message".to_string(),
        "ban" => "banned".to_string(),
        "unban" => "unbanned".to_string(),
        other if target_user_id.is_some() => other.to_string(),
        other => format!("{} — message", other),
    };

    // Legacy rows always have `message`; user-targeted rows never do.
    let message_id = action.message().ok().flatten().map(|m| m.id().to_base64());
    let message_chip = message_id.map(|id| {
        let short = id.chars().take(8).collect::<String>();
        view! {
            <span class="modLogMsgRef" title=format!("Message {}", id)>{format!("⟨{}⟩", short)}</span>
        }
    });

    let ts = action.created_at().unwrap_or(0);
    let when = format!("{} · {}", fmt::day_label(ts), fmt::clock_time(ts));
    let when_title = fmt::full_stamp(ts);

    let reason = action.reason().ok().flatten().filter(|r| !r.trim().is_empty());

    view! {
        <div class="modLogRow">
            <div class=format!("memberAvatar {}", hue) aria-hidden="true">
                {move || fmt::initials(&actor_name_for_initials())}
            </div>
            <div class="modLogBody">
                <div class="modLogLine">
                    <span class="modLogActor">{actor_name}</span>
                    " "
                    <span class="modLogVerb">{verb}</span>
                    " "
                    {target_name.map(|name| view! { <span class="modLogActor">{name}</span> })}
                    {message_chip}
                </div>
                {reason.map(|r| view! { <div class="modLogReason">{format!("“{}”", r)}</div> })}
                <div class="modLogWhen" title=when_title>{when}</div>
            </div>
        </div>
    }
}
