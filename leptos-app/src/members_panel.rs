use std::collections::HashMap;

use leptos::prelude::*;

use ankurah_signals::Get as AnkurahGet;
use community_model::{BanView, UserRolesView, UserView};

use crate::{
    ctx, fmt,
    panels::{panels, Surface},
};

/// Member directory. Every signed-in user sees the same roster: all community
/// members with avatar, display name, and IdP-assigned role badges (roles are
/// managed by the identity provider, not from inside the app). Moderators see
/// a "Banned" badge on banned rows — plain members don't see others' bans at
/// all (the ban read scope in policy.json shows a non-moderator only their
/// own rows), so for them the roster simply carries no ban state.
///
/// Every row is a navigation target: clicking opens the member's detail
/// sidebar (#57), which — for moderators — carries the ban/unban actions that
/// used to live on a per-row "⋯" menu here.
#[component]
pub fn MembersPanel(on_close: impl Fn() + Clone + 'static) -> impl IntoView {
    // All users, live — new sign-ins appear while the panel is open.
    let users = ctx().query::<UserView>("true").expect("failed to create UserView LiveQuery");

    // Server-maintained role cache (read-only for clients): one row per user
    // holding the lowercase role keys minted into their latest session token.
    let user_roles = ctx().query::<UserRolesView>("true").expect("failed to create UserRolesView LiveQuery");

    // Surface the panel's queries in the X-ray queries card for as long as
    // the panel is open (transient registrations, dropped on close).
    let xray_regs = (
        crate::xray::bus::bus().register("users (members panel)", &users),
        crate::xray::bus::bus().register("userroles (members panel)", &user_roles),
    );
    on_cleanup(move || {
        let bus = crate::xray::bus::bus();
        bus.unregister(xray_regs.0);
        bus.unregister(xray_regs.1);
    });

    // Active bans, live. What this returns depends on who asks — moderators
    // get every row (their `moderate` privilege bypasses the read scope),
    // everyone else at most their own. No client-side gating needed: the
    // policy already shapes the resultset.
    let bans = ctx().query::<BanView>("active = true").expect("failed to create BanView LiveQuery");

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

    // Same idiom for bans: user id → whether the viewer can see an active ban
    // (only users with at least one active row appear as keys).
    let banned_users = Memo::new(move |_| {
        let mut map: HashMap<String, Option<String>> = HashMap::new();
        for row in bans.get() {
            if let Ok(user) = row.user() {
                let entry = map.entry(user.id().to_base64()).or_default();
                // The badge tooltip carries the first non-empty ban reason.
                if entry.is_none() {
                    *entry = row.reason().ok().filter(|reason| !reason.trim().is_empty());
                }
            }
        }
        map
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
                            view! { <MemberRow user roles_by_user banned_users /> }
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
/// since the cache was introduced). Banned members (as visible to the viewer
/// under the ban read scope) get a danger-toned "Banned" badge.
///
/// The whole row is a button: it opens the member's detail sidebar (#57) —
/// the panel manager swaps this roster out for it.
#[component]
fn MemberRow(
    user: UserView,
    roles_by_user: Memo<HashMap<String, Vec<String>>>,
    banned_users: Memo<HashMap<String, Option<String>>>,
) -> impl IntoView {
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
    let user_id_for_roles = user_id.clone();
    let badge_roles = move || {
        roles_by_user.with(|map| {
            map.get(&user_id_for_roles)
                .map(|roles| roles.iter().filter(|role| role.as_str() != "member").cloned().collect::<Vec<String>>())
                .unwrap_or_default()
        })
    };

    // Whether the viewer can see an active ban on this user (for moderators
    // that means "this user is banned"; for a plain member the map only ever
    // contains themselves).
    let user_id_for_ban = user_id.clone();
    let banned = move || banned_users.with(|map| map.contains_key(&user_id_for_ban));
    let banned_for_class = banned.clone();

    let user_id_for_reason = user_id.clone();
    let ban_reason = move || banned_users.with(|map| map.get(&user_id_for_reason).cloned().flatten());

    let detail_target = user.id();
    let open_detail = move |_| panels().open(Surface::UserDetail(detail_target.clone()));

    view! {
        <button
            type="button"
            class=move || if banned_for_class() { "memberRow memberRowBanned" } else { "memberRow" }
            title="View member"
            on:click=open_detail
        >
            <div class=format!("memberAvatar {}", hue) aria-hidden="true">
                {move || fmt::initials(&name_for_initials())}
            </div>
            <span class="memberName">{name}</span>
            {move || {
                banned()
                    .then(|| {
                        view! {
                            <span class="roleBadge roleBadgeBanned" title=ban_reason()>"Banned"</span>
                        }
                    })
            }}
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
                                            )>{fmt::capitalize(&role)}</span>
                                        }
                                    })
                                    .collect_view()}
                            </span>
                        }
                    })
            }}
        </button>
    }
}
