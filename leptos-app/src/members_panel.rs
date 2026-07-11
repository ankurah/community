use std::collections::HashMap;

use leptos::prelude::*;

use ankurah_signals::Get as AnkurahGet;
use community_model::{UserRolesView, UserView};

use crate::{ctx, fmt};

/// Read-only member directory. Every signed-in user sees the identical view:
/// all community members with avatar, display name, and IdP-assigned role
/// badges. Deliberately no controls of any kind — roles are managed by the
/// identity provider, not from inside the app.
#[component]
pub fn MembersPanel(on_close: impl Fn() + Clone + 'static) -> impl IntoView {
    // All users, live — new sign-ins appear while the panel is open.
    let users = ctx().query::<UserView>("true").expect("failed to create UserView LiveQuery");

    // Server-maintained role cache (read-only for clients): one row per user
    // holding the lowercase role keys minted into their latest session token.
    let user_roles = ctx().query::<UserRolesView>("true").expect("failed to create UserRolesView LiveQuery");

    // Collapse the cache into a keyed map (user id → role keys), rebuilt once
    // whenever it changes. Each row then does an O(1) lookup instead of scanning
    // every userroles row on every render (previously O(users × userroles)).
    let roles_by_user = Memo::new(move |_| {
        user_roles
            .get()
            .iter()
            .filter_map(|row| {
                let id = row.user().ok()?.id().to_base64();
                let roles = row
                    .roles()
                    .ok()
                    .and_then(|json| {
                        json.as_array()
                            .map(|arr| arr.iter().filter_map(|v| v.as_str()).map(|s| s.to_string()).collect::<Vec<String>>())
                    })
                    .unwrap_or_default();
                Some((id, roles))
            })
            .collect::<HashMap<String, Vec<String>>>()
    });

    let users_for_count = users.clone();
    let users_for_loading = users.clone();
    let users_for_empty = users.clone();
    let users_for_list = users.clone();

    // "Loading…" until the query settles, then a live member count.
    let subtitle = move || {
        if !users_for_count.loaded() {
            "Loading\u{2026}".to_string()
        } else {
            match users_for_count.get().len() {
                1 => "1 member".to_string(),
                n => format!("{} members", n),
            }
        }
    };

    let on_close_overlay = on_close.clone();
    let on_close_button = on_close.clone();

    view! {
        <div class="membersOverlay" on:click=move |_| on_close_overlay()>
            <div class="membersContent" on:click=|e| e.stop_propagation()>
                <div class="membersHeader">
                    <div class="membersTitles">
                        <h2>"Members"</h2>
                        <p class="membersSubtitle">{subtitle}</p>
                    </div>
                    <button class="membersCloseButton" aria-label="Close" on:click=move |_| on_close_button()>
                        "×"
                    </button>
                </div>

                <div class="membersList">
                    <Show when=move || !users_for_loading.loaded()>
                        <div class="membersState">"Loading members\u{2026}"</div>
                    </Show>
                    <Show when={
                        let users = users_for_empty.clone();
                        move || users.loaded() && users.get().is_empty()
                    }>
                        <div class="membersState">"No members yet."</div>
                    </Show>
                    <For
                        each={
                            let users = users_for_list.clone();
                            move || {
                                let mut items = users.get();
                                // Stable, human order: name (case-insensitive), then id.
                                items.sort_by_cached_key(|u| {
                                    (u.display_name().unwrap_or_default().to_lowercase(), u.id().to_base64())
                                });
                                items
                            }
                        }
                        key=|user: &UserView| user.id()
                        children=move |user: UserView| {
                            view! { <MemberRow user roles_by_user /> }
                        }
                    />
                </div>

                <p class="membersNote">
                    "Roles are managed by the identity provider and take effect at next sign-in."
                </p>
            </div>
        </div>
    }
}

/// One directory row: initials avatar (deterministic hue), display name, and
/// role badges from the `userroles` cache. Plain members carry no badge, and
/// neither does a user with no `userroles` row yet (they have never signed in
/// since the cache was introduced).
#[component]
fn MemberRow(user: UserView, roles_by_user: Memo<HashMap<String, Vec<String>>>) -> impl IntoView {
    let user_id = user.id().to_base64();
    let hue = fmt::hue_class(&user_id);

    // Reactive: display names are editable and update live.
    let user_for_name = user.clone();
    let name = move || {
        let n = user_for_name.display_name().unwrap_or_default();
        if n.trim().is_empty() { "Unknown".to_string() } else { n }
    };
    let name_for_initials = name.clone();

    // Badge-worthy roles for this user, via an O(1) lookup into the shared map
    // (reactive: rows appear/change as the server refreshes the cache on
    // sign-in). "member" is the baseline and gets no badge; anything else
    // renders capitalized.
    let badge_roles = move || {
        roles_by_user.with(|map| {
            map.get(&user_id)
                .map(|roles| roles.iter().filter(|role| role.as_str() != "member").cloned().collect::<Vec<String>>())
                .unwrap_or_default()
        })
    };

    view! {
        <div class="memberRow">
            <div class=format!("memberAvatar {}", hue) aria-hidden="true">
                {move || fmt::initials(&name_for_initials())}
            </div>
            <span class="memberName">{name}</span>
            {move || {
                let roles = badge_roles();
                (!roles.is_empty())
                    .then(|| {
                        view! {
                            <span class="memberBadges">
                                {roles
                                    .into_iter()
                                    .map(|role| {
                                        view! {
                                            <span class=format!(
                                                "roleBadge role-{}",
                                                role,
                                            )>{capitalize(&role)}</span>
                                        }
                                    })
                                    .collect_view()}
                            </span>
                        }
                    })
            }}
        </div>
    }
}

/// "moderator" → "Moderator" for badge labels (role keys are lowercase ASCII).
fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}
