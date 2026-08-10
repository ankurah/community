//! User detail sidebar (#57): the members-list hover implies navigation —
//! this is where it lands. Opened through the panel manager
//! (`Surface::UserDetail`) from members rows, the profile popover's name,
//! and mention chips in messages.
//!
//! v1 contents: identity (avatar, live display name, first-seen month),
//! IdP-assigned role badges from the `userroles` display cache, ban state,
//! and — for moderators — the ban/unban actions relocated here from the
//! members panel's per-row "⋯" menu (`ban_member`/`unban_member` moved with
//! them; the roster row's whole surface now just opens this sidebar).
//!
//! Presentation: a right sidebar, not a modal — no backdrop, the app stays
//! interactive, and it closes on Escape/×/opening any other surface.

use leptos::prelude::*;
use web_sys::window;

use ankurah::EntityId;
use ankurah_signals::Get as AnkurahGet;
use community_model::{Ban, BanView, ModAction, UserRolesView, UserView};

use crate::{ctx, fmt};

#[component]
pub fn UserDetailPanel(
    user_id: EntityId,
    /// The app's open-DM signal, so "Message" can open the conversation with
    /// this member (#30). Threaded from `ChatApp` through the panel host.
    selected_dm: RwSignal<Option<EntityId>>,
    on_close: impl Fn() + Clone + 'static,
) -> impl IntoView {
    // Opening a conversation is a write, and the chat handshake resolves only
    // where a reactive owner exists — so take it here, not in the click below.
    let chat = ankurah_chat_leptos::chat();

    // The user row, resolved once (local-first: IndexedDB, then the peer).
    // The view itself is live afterwards — display-name edits re-render.
    let user = RwSignal::new(None::<UserView>);
    let load_failed = RwSignal::new(false);
    wasm_bindgen_futures::spawn_local(async move {
        match ctx().get::<UserView>(user_id).await {
            // try_set: the panel may have been closed (disposed) before
            // the fetch resolved.
            Ok(u) => {
                let _ = user.try_set(Some(u));
            }
            Err(e) => {
                tracing::error!("Failed to load user {}: {}", user_id.to_base64(), e);
                let _ = load_failed.try_set(true);
            }
        }
    });

    // Role badges from the server-written `userroles` cache, scoped to this
    // user (the profile-popover idiom, #17-parameterized).
    let roles_query = crate::queries::selection("user = ?", [(&user_id).into()])
        .ok()
        .and_then(|sel| ctx().query::<UserRolesView>(sel).ok());
    let badge_roles = {
        let roles_query = roles_query.clone();
        move || -> Vec<String> {
            let Some(q) = roles_query.as_ref() else { return Vec::new() };
            q.get()
                .iter()
                .filter_map(|row| row.roles().ok())
                .filter_map(|json| {
                    json.as_array().map(|arr| arr.iter().filter_map(|v| v.as_str()).map(str::to_string).collect::<Vec<_>>())
                })
                .flatten()
                .filter(|role| role != "member")
                .collect()
        }
    };

    // This user's active bans. Policy-shaped like the members panel's query:
    // moderators see the rows, everyone else at most their own — so the ban
    // section simply renders nothing for members looking at others.
    let bans_query = crate::queries::selection("user = ? AND active = true", [(&user_id).into()])
        .ok()
        .and_then(|sel| ctx().query::<BanView>(sel).ok());
    let ban_rows = {
        let bans_query = bans_query.clone();
        move || -> Vec<BanView> { bans_query.as_ref().map(|q| q.get().to_vec()).unwrap_or_default() }
    };

    // Name the sidebar's queries for the app's query-registry observer while
    // it's open (transient registrations, dropped on close — members-panel
    // precedent).
    let query_regs = [
        roles_query.as_ref().map(|q| crate::query_registry::register("userroles (user detail)", q)),
        bans_query.as_ref().map(|q| crate::query_registry::register("bans (user detail)", q)),
    ];
    on_cleanup(move || drop(query_regs));

    // Move focus onto the sidebar so screen readers announce it (Escape is
    // window-level — see panels.rs — so it works wherever focus sits).
    let panel_ref = NodeRef::<leptos::html::Aside>::new();
    Effect::new(move |_| {
        if let Some(el) = panel_ref.get() {
            let _ = el.focus();
        }
    });

    // UI gating only — the server enforces the ban write policy regardless.
    // No self-ban affordance (locking yourself out is a mistake, not
    // moderation), and no actions on yourself at all.
    let show_mod_actions = crate::can_moderate() && crate::viewer() != Some(user_id);

    // Start-DM entry point (#30). Offered for every member except yourself: a
    // self-thread has no other participant to show or notify, and the
    // find-or-create refuses one anyway. This is the ONLY place a conversation
    // is started from — the sidebar section lists what already exists.
    //
    // It also disappears for a member this viewer can see an active ban on
    // (below): a banned member is refused at token mint, so they could never
    // answer, and offering to start a conversation with someone who cannot
    // reply promises something the app cannot deliver. Like the "Banned" badge
    // beside it, this is only as informed as the viewer's own sight of the ban
    // rows — moderators see them all, members at most their own — so it hides
    // the button exactly where the badge appears, and never pretends to be
    // enforcement.
    // A guest is offered no conversation at all: a thread has two members and
    // a guest is neither of them, so `viewer()` answering `None` closes this
    // the same way it closes the DM section in the rail.
    let can_message = matches!(crate::viewer(), Some(me) if me != user_id);
    let message_user_id = user_id;

    let user_id_b64 = user_id.to_base64();
    let hue = fmt::hue_class(&user_id_b64);
    // EntityIds are ULIDs: the User entity's id carries its creation time,
    // which is the user's first sign-in (users are keyed on their OIDC sub).
    let first_seen = format!("First seen {}", fmt::month_year(user_id.to_ulid().timestamp_ms() as i64));

    let name = move || {
        let n = user.get().map(|u| u.display_name().unwrap_or_default()).unwrap_or_default();
        if n.trim().is_empty() { "Unknown".to_string() } else { n }
    };
    let name_for_initials = name;
    let name_for_ban = name;

    let banned = {
        let ban_rows = ban_rows.clone();
        move || !ban_rows().is_empty()
    };
    let banned_for_badge = banned.clone();
    // The first non-empty reason among the active rows, if any.
    let ban_reason = {
        let ban_rows = ban_rows.clone();
        move || ban_rows().iter().filter_map(|r| r.reason().ok()).find(|reason| !reason.trim().is_empty())
    };

    let on_close_button = on_close.clone();

    view! {
        <aside class="userDetailPanel" aria-label="Member details" tabindex="-1" node_ref=panel_ref>
            <div class="userDetailHeader">
                <h2>"Member"</h2>
                <button class="membersCloseButton" aria-label="Close" on:click=move |_| on_close_button()>
                    "×"
                </button>
            </div>

            <Show
                when=move || user.get().is_some()
                fallback=move || {
                    view! {
                        <div class="userDetailState">
                            {move || {
                                if load_failed.get() {
                                    "Couldn't load this member."
                                } else {
                                    "Loading member\u{2026}"
                                }
                            }}
                        </div>
                    }
                }
            >
                <div class="userDetailIdentity">
                    <div class=format!("userDetailAvatar {}", hue) aria-hidden="true">
                        {move || fmt::initials(&name_for_initials())}
                    </div>
                    <div class="userDetailName">{name}</div>
                    <div class="userDetailFirstSeen">{first_seen.clone()}</div>
                </div>

                {
                    let badge_roles = badge_roles.clone();
                    let banned_for_badge = banned_for_badge.clone();
                    let ban_reason = ban_reason.clone();
                    move || {
                        let roles = badge_roles();
                        let is_banned = banned_for_badge();
                        (!roles.is_empty() || is_banned)
                            .then(|| {
                                view! {
                                    <div class="userDetailBadges">
                                        {is_banned
                                            .then(|| {
                                                view! {
                                                    <span class="roleBadge roleBadgeBanned" title=ban_reason()>
                                                        "Banned"
                                                    </span>
                                                }
                                            })}
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
                                    </div>
                                }
                            })
                    }
                }

                // A visible ban reason, for viewers the policy shows rows to.
                {
                    let banned = banned.clone();
                    let ban_reason = ban_reason.clone();
                    move || {
                        (banned() && ban_reason().is_some())
                            .then(|| {
                                view! {
                                    <div class="userDetailBanReason">
                                        {format!("Ban reason: “{}”", ban_reason().unwrap_or_default())}
                                    </div>
                                }
                            })
                    }
                }

                {
                    let banned = banned.clone();
                    let chat = chat.clone();
                    move || {
                        let chat = chat.clone();
                        (can_message && !banned())
                            .then(move || {
                                view! {
                                    <div class="userDetailActions">
                                        <button
                                            class="userDetailActionBtn userDetailMessageBtn"
                                            // Closes through the global panel
                                            // manager rather than this panel's
                                            // `on_close`: the button lives inside a
                                            // view leptos stores as a thread-safe
                                            // closure, and `on_close` is not
                                            // Send+Sync. Same effect — the header's
                                            // on_close IS panels().close().
                                            on:click=move |_| {
                                                crate::dm::open_thread_with(&chat, message_user_id, selected_dm);
                                                crate::panels::panels().close();
                                            }
                                        >
                                            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor"
                                                stroke-width="2" stroke-linecap="round" stroke-linejoin="round"
                                                aria-hidden="true">
                                                <path d="M4 6h16v11H8l-4 4z" />
                                            </svg>
                                            "Message"
                                        </button>
                                        <p class="userDetailMessageNote">
                                            "Direct messages are private to the two of you — moderators cannot read them."
                                        </p>
                                    </div>
                                }
                            })
                    }
                }

                {show_mod_actions
                    .then(|| {
                        let user_id_b64 = user_id_b64.clone();
                        let ban_rows = ban_rows.clone();
                        let banned = banned.clone();
                        view! {
                            <div class="userDetailActions">
                                <h3 class="userDetailActionsTitle">"Moderation"</h3>
                                {move || {
                                    let user_id_b64 = user_id_b64.clone();
                                    if banned() {
                                        let rows = ban_rows();
                                        view! {
                                            <button
                                                class="userDetailActionBtn"
                                                on:click=move |_| unban_member(
                                                    user_id_b64.clone(),
                                                    rows.clone(),
                                                )
                                            >
                                                <svg viewBox="0 0 24 24" fill="none" stroke="currentColor"
                                                    stroke-width="2" stroke-linecap="round"
                                                    stroke-linejoin="round" aria-hidden="true">
                                                    <circle cx="12" cy="12" r="9" />
                                                </svg>
                                                "Unban member"
                                            </button>
                                        }
                                            .into_any()
                                    } else {
                                        let name = name_for_ban;
                                        view! {
                                            <button
                                                class="userDetailActionBtn userDetailActionDanger"
                                                on:click=move |_| ban_member(user_id_b64.clone(), name())
                                            >
                                                <svg viewBox="0 0 24 24" fill="none" stroke="currentColor"
                                                    stroke-width="2" stroke-linecap="round"
                                                    stroke-linejoin="round" aria-hidden="true">
                                                    <circle cx="12" cy="12" r="9" />
                                                    <path d="m5.6 5.6 12.8 12.8" />
                                                </svg>
                                                "Ban member\u{2026}"
                                            </button>
                                        }
                                            .into_any()
                                    }
                                }}
                            </div>
                        }
                    })}
            </Show>

            <p class="userDetailNote">
                "Roles are managed by the identity provider and take effect at next sign-in."
            </p>
        </aside>
    }
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
        match async {
            // As at the message-removal call site: a log row with no actor
            // reads as "Automatic", so a caller with no viewer is refused
            // rather than recorded as the rate limiter.
            let actor = crate::viewer().ok_or("no signed-in moderator to attribute this ban to")?;
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
                actor: Some(actor.into()),
                message: None,
                user: Some(user_eid.into()),
                action: "ban".to_string(),
                reason,
                created_at: now,
            })
            .await?;
            trx.commit().await?;
            Ok::<_, Box<dyn std::error::Error>>(())
        }
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
        match async {
            let actor = crate::viewer().ok_or("no signed-in moderator to attribute this unban to")?;
            let user_eid = EntityId::from_base64(&user_id)?;
            let trx = ctx().begin();
            for row in &rows {
                row.edit(&trx)?.active().set(&false)?;
            }
            trx.create(&ModAction {
                actor: Some(actor.into()),
                message: None,
                user: Some(user_eid.into()),
                action: "unban".to_string(),
                reason: None,
                created_at: js_sys::Date::now() as i64,
            })
            .await?;
            trx.commit().await?;
            Ok::<_, Box<dyn std::error::Error>>(())
        }
        .await
        {
            Ok(_) => tracing::info!("Unbanned {}", user_id),
            Err(e) => tracing::error!("Failed to unban {}: {}", user_id, e),
        }
    });
}
