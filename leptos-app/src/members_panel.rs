use std::collections::HashMap;

use leptos::ev::MouseEvent as LeptosMouseEvent;
use leptos::prelude::*;
use send_wrapper::SendWrapper;
use wasm_bindgen::JsCast;
use web_sys::{window, KeyboardEvent, MouseEvent};

use ankurah::EntityId;
use ankurah_signals::Get as AnkurahGet;
use community_model::{Ban, BanView, ModAction, UserRolesView, UserView};

use crate::{ctx, fmt};

/// Member directory. Every signed-in user sees the same roster: all community
/// members with avatar, display name, and IdP-assigned role badges (roles are
/// managed by the identity provider, not from inside the app). Moderators
/// additionally get a per-row action menu to ban/unban members, and see a
/// "Banned" badge on banned rows — plain members don't see others' bans at
/// all (the ban read scope in policy.json shows a non-moderator only their
/// own rows), so for them the roster simply carries no ban state.
#[component]
pub fn MembersPanel(on_close: impl Fn() + Clone + 'static) -> impl IntoView {
    // All users, live — new sign-ins appear while the panel is open.
    let users = ctx().query::<UserView>("true").expect("failed to create UserView LiveQuery");

    // Server-maintained role cache (read-only for clients): one row per user
    // holding the lowercase role keys minted into their latest session token.
    let user_roles = ctx().query::<UserRolesView>("true").expect("failed to create UserRolesView LiveQuery");

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

    // Same idiom for bans: user id → that user's active ban rows (only users
    // with at least one active row appear as keys). The rows themselves are
    // kept so Unban can flip every one of them, not just the newest.
    let bans_by_user = Memo::new(move |_| {
        let mut map: HashMap<String, Vec<BanView>> = HashMap::new();
        for row in bans.get() {
            if let Ok(user) = row.user() {
                map.entry(user.id().to_base64()).or_default().push(row.clone());
            }
        }
        map
    });

    // UI gating only — the server enforces the ban write policy regardless.
    let is_mod = crate::can_moderate();

    // At most one member-action menu open at a time, anchored where the
    // trigger was clicked (fixed coordinates, so the scrolling list can't
    // clip it).
    let open_menu = RwSignal::new(None::<MemberMenuState>);

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
                            view! { <MemberRow user roles_by_user bans_by_user open_menu is_mod /> }
                        }
                    />
                </div>

                {move || {
                    open_menu
                        .get()
                        .map(|menu| {
                            let rows = bans_by_user
                                .with(|m| m.get(&menu.user_id).cloned().unwrap_or_default());
                            view! {
                                <MemberActionMenu
                                    menu
                                    rows
                                    on_close=move || open_menu.set(None)
                                />
                            }
                        })
                }}

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
/// under the ban read scope) get a danger-toned "Banned" badge; moderators
/// also get the "⋯" action trigger on every row but their own.
#[component]
fn MemberRow(
    user: UserView,
    roles_by_user: Memo<HashMap<String, Vec<String>>>,
    bans_by_user: Memo<HashMap<String, Vec<BanView>>>,
    open_menu: RwSignal<Option<MemberMenuState>>,
    is_mod: bool,
) -> impl IntoView {
    let user_id = user.id().to_base64();
    let hue = fmt::hue_class(&user_id);

    // No self-ban affordance: locking yourself out of your own community is
    // a mistake, not a moderation action.
    let show_actions = is_mod && user.id() != crate::current_user_id();

    // Reactive: display names are editable and update live.
    let user_for_name = user.clone();
    let name = move || {
        let n = user_for_name.display_name().unwrap_or_default();
        if n.trim().is_empty() { "Unknown".to_string() } else { n }
    };
    let name_for_initials = name.clone();
    let name_for_menu = name.clone();

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
    let banned = move || bans_by_user.with(|map| map.contains_key(&user_id_for_ban));
    let banned_for_class = banned.clone();

    // The badge tooltip carries the first non-empty ban reason, if any.
    let user_id_for_reason = user_id.clone();
    let ban_reason = move || {
        bans_by_user.with(|map| {
            map.get(&user_id_for_reason)
                .and_then(|rows| rows.iter().filter_map(|r| r.reason().ok()).find(|reason| !reason.trim().is_empty()))
        })
    };

    let user_id_for_expanded = user_id.clone();
    let menu_expanded =
        move || open_menu.with(|m| m.as_ref().map(|s| s.user_id == user_id_for_expanded).unwrap_or(false));

    let user_id_for_click = user_id.clone();
    let open_actions = move |e: LeptosMouseEvent| {
        // Don't let this click reach the document-level dismiss listener of
        // an already-open menu's replacement, or the overlay close handler.
        e.stop_propagation();
        let (x, y) = clamped_menu_position(e.client_x(), e.client_y());
        open_menu.set(Some(MemberMenuState { user_id: user_id_for_click.clone(), name: name_for_menu(), x, y }));
    };

    view! {
        <div class=move || if banned_for_class() { "memberRow memberRowBanned" } else { "memberRow" }>
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
                                            )>{capitalize(&role)}</span>
                                        }
                                    })
                                    .collect_view()}
                            </span>
                        }
                    })
            }}
            {show_actions
                .then(|| {
                    view! {
                        <button
                            class="memberActionsBtn"
                            aria-label="Member actions"
                            aria-haspopup="menu"
                            aria-expanded=move || menu_expanded().to_string()
                            on:click=open_actions
                        >
                            "⋯"
                        </button>
                    }
                })}
        </div>
    }
}

/// Which member the action menu is open for, and where to anchor it.
#[derive(Clone)]
struct MemberMenuState {
    user_id: String,
    name: String,
    x: i32,
    y: i32,
}

/// The per-member overflow menu (moderators only — the trigger never renders
/// otherwise). One context-appropriate action: Ban for members in good
/// standing, Unban for banned ones. Dismissal follows the profile-popover
/// pattern: outside mousedown or Escape, with listeners removed on unmount.
#[component]
fn MemberActionMenu(menu: MemberMenuState, rows: Vec<BanView>, on_close: impl Fn() + Clone + 'static) -> impl IntoView {
    let is_banned = !rows.is_empty();
    let menu_ref = NodeRef::<leptos::html::Div>::new();

    // Focus the action when the menu opens so Escape/Enter work immediately
    // (mouse users see no ring — the global outline is :focus-visible only).
    Effect::new({
        let menu_ref = menu_ref.clone();
        move |_| {
            if let Some(el) = menu_ref.get() {
                if let Ok(Some(node)) = el.query_selector("[role='menuitem']") {
                    if let Ok(button) = node.dyn_into::<web_sys::HtmlElement>() {
                        let _ = button.focus();
                    }
                }
            }
        }
    });

    // Outside-click + Escape dismiss, removed on unmount (popover pattern).
    let click_closure = wasm_bindgen::closure::Closure::wrap(Box::new({
        let on_close = on_close.clone();
        let menu_ref = menu_ref.clone();
        move |e: MouseEvent| {
            if let Some(el) = menu_ref.get_untracked() {
                if let Some(target) = e.target() {
                    if let Ok(node) = target.dyn_into::<web_sys::Node>() {
                        if !el.contains(Some(&node)) {
                            on_close();
                        }
                    }
                }
            }
        }
    }) as Box<dyn FnMut(_)>);
    let key_closure = wasm_bindgen::closure::Closure::wrap(Box::new({
        let on_close = on_close.clone();
        move |e: KeyboardEvent| {
            if e.key() == "Escape" {
                on_close();
            }
        }
    }) as Box<dyn FnMut(_)>);
    if let Some(doc) = window().and_then(|w| w.document()) {
        let _ = doc.add_event_listener_with_callback("mousedown", click_closure.as_ref().unchecked_ref());
        let _ = doc.add_event_listener_with_callback("keydown", key_closure.as_ref().unchecked_ref());
    }
    let closures = SendWrapper::new((click_closure, key_closure));
    on_cleanup(move || {
        let (click_closure, key_closure) = closures.take();
        if let Some(doc) = window().and_then(|w| w.document()) {
            let _ = doc.remove_event_listener_with_callback("mousedown", click_closure.as_ref().unchecked_ref());
            let _ = doc.remove_event_listener_with_callback("keydown", key_closure.as_ref().unchecked_ref());
        }
    });

    let handle_action = {
        let menu = menu.clone();
        let on_close = on_close.clone();
        move |e: LeptosMouseEvent| {
            e.stop_propagation();
            if is_banned {
                unban_member(menu.user_id.clone(), rows.clone());
            } else {
                ban_member(menu.user_id.clone(), menu.name.clone());
            }
            on_close();
        }
    };

    view! {
        <div
            node_ref=menu_ref
            class="memberMenu"
            role="menu"
            aria-label=format!("Actions for {}", menu.name)
            style:left=format!("{}px", menu.x)
            style:top=format!("{}px", menu.y)
        >
            <button
                class=if is_banned { "memberMenuItem" } else { "memberMenuItem memberMenuItemDanger" }
                role="menuitem"
                on:click=handle_action
            >
                <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"
                    stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
                    <circle cx="12" cy="12" r="9" />
                    {(!is_banned).then(|| view! { <path d="m5.6 5.6 12.8 12.8" /> })}
                </svg>
                {if is_banned { "Unban member" } else { "Ban member\u{2026}" }}
            </button>
        </div>
    }
}

/// Keep the (small, fixed-position) menu inside the viewport whatever corner
/// of the roster the trigger sits in.
fn clamped_menu_position(x: i32, y: i32) -> (i32, i32) {
    let (mut win_w, mut win_h) = (1024, 768);
    if let Some(win) = window() {
        win_w = win.inner_width().ok().and_then(|v| v.as_f64()).unwrap_or(1024.0) as i32;
        win_h = win.inner_height().ok().and_then(|v| v.as_f64()).unwrap_or(768.0) as i32;
    }
    (x.min(win_w - 200).max(10), y.min(win_h - 72).max(10))
}

/// Ban a member: prompt for a public reason (same affordance as the wave-1
/// moderator delete — Cancel aborts, an empty OK proceeds without a reason, a
/// blocked dialog never blocks moderation), then create the `Ban` row and its
/// public `ModAction` log row in one transaction.
///
/// What a ban does today: the banned client's self-lock takes over as soon as
/// this row syncs (see `ban_lock`), and the server's mint gate refuses their
/// next session. SEAM — account inactivation: banning should eventually also
/// disable the member's idp.to account so they can't authenticate anywhere;
/// that leg is out of scope pending the IdP team's design packet, and its
/// server-side hook belongs next to the ban gate in server/src/main.rs
/// (`auth_session`), not in this client.
fn ban_member(user_id: String, user_name: String) {
    // Prompt doubles as confirm: Cancel aborts the ban entirely.
    let reason = match window().map(|w| w.prompt_with_message(&format!("Ban {} — reason (optional):", user_name))) {
        Some(Ok(None)) => return, // prompt cancelled — no ban
        Some(Ok(Some(text))) => {
            let text = text.trim().to_string();
            (!text.is_empty()).then_some(text)
        }
        _ => None,
    };

    wasm_bindgen_futures::spawn_local(async move {
        match (|| async {
            let user_eid = EntityId::from_base64(&user_id)?;
            let now = js_sys::Date::now() as i64;
            let trx = ctx().begin();
            trx.create(&Ban {
                user: user_eid.into(),
                reason: reason.clone().unwrap_or_default(),
                created_at: now,
                active: true,
            })
            .await?;
            // The lights-on log row (#10): user-targeted, so no message ref.
            trx.create(&ModAction {
                actor: crate::current_user_id().into(),
                message: None,
                user: Some(user_eid.into()),
                action: "ban".to_string(),
                reason,
                created_at: now,
            })
            .await?;
            trx.commit().await?;
            Ok::<_, Box<dyn std::error::Error>>(())
        })()
        .await
        {
            Ok(_) => tracing::info!("Banned {}", user_id),
            Err(e) => tracing::error!("Failed to ban {}: {}", user_id, e),
        }
    });
}

/// Lift a ban: flip `active` off on every active row for the user (there is
/// no entity deletion in ankurah 0.9.0, and the inactive rows remain as the
/// audit trail), plus the public "unban" `ModAction` row. This is also what
/// "kick" means today — ban then unban — since there are no room-scoped
/// memberships to eject anyone from.
fn unban_member(user_id: String, rows: Vec<BanView>) {
    wasm_bindgen_futures::spawn_local(async move {
        match (|| async {
            let user_eid = EntityId::from_base64(&user_id)?;
            let trx = ctx().begin();
            for row in &rows {
                row.edit(&trx)?.active().set(&false)?;
            }
            trx.create(&ModAction {
                actor: crate::current_user_id().into(),
                message: None,
                user: Some(user_eid.into()),
                action: "unban".to_string(),
                reason: None,
                created_at: js_sys::Date::now() as i64,
            })
            .await?;
            trx.commit().await?;
            Ok::<_, Box<dyn std::error::Error>>(())
        })()
        .await
        {
            Ok(_) => tracing::info!("Unbanned {}", user_id),
            Err(e) => tracing::error!("Failed to unban {}: {}", user_id, e),
        }
    });
}

/// "moderator" → "Moderator" for badge labels (role keys are lowercase ASCII).
fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}
