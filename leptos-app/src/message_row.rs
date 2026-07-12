use leptos::ev::MouseEvent;
use leptos::prelude::*;
use wasm_bindgen::JsCast;

use ankurah::LiveQuery;
use ankurah_signals::Get as AnkurahGet;
use community_model::{MessageView, UserView};

use std::collections::HashMap;

use crate::fmt;
use crate::message_context_menu::MessageContextMenu;
use crate::profile_popover::ProfilePopover;
use crate::reactions::{ReactionBar, ReactionChip};
use community_model::ModActionView;

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
    /// Render-ready reaction chips per message id (shared, built once in the
    /// list — see message_list.rs).
    reaction_chips: Memo<HashMap<String, Vec<ReactionChip>>>,
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

    // Since reactions (#14) every non-tombstone message has menu actions for
    // every viewer; Edit/Delete are gated inside the menu itself.

    // Tombstone state (#10): deleted messages stay in the timeline as muted
    // rows. Reactive — a remote delete flips the row live.
    let message_for_deleted = message.clone();
    let is_deleted = move || message_for_deleted.deleted().unwrap_or(false);
    let is_deleted_for_menu = is_deleted.clone();
    let is_deleted_for_class = is_deleted.clone();

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
        // Tombstones offer no actions — for the author or for moderators.
        if !is_deleted_for_menu() {
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

    // Profile popover (#15): opened from the avatar or author-name buttons.
    let profile = RwSignal::new(None::<(UserView, i32, i32)>);
    let open_profile = {
        let author = author.clone();
        move |e: MouseEvent| {
            e.stop_propagation();
            let Some(user) = author() else { return };
            let (px, py) = e
                .current_target()
                .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
                .map(|el| {
                    let r = el.get_bounding_client_rect();
                    (r.left() as i32, r.bottom() as i32 + 6)
                })
                .unwrap_or((e.client_x(), e.client_y()));
            profile.set(Some((user, px, py)));
        }
    };
    let open_profile_from_avatar = open_profile.clone();
    let open_profile_from_name = open_profile;

    let is_editing =
        move || editing_message.get().as_ref().map(|em| em.id().to_base64() == message_for_editing.id().to_base64()).unwrap_or(false);

    let message_id = message.id().to_base64();
    let message_for_text = message.clone();
    let message_for_edited = message.clone();
    let message_for_xray = message.clone();
    // X-ray: the message itself is the inspect target — no per-message id
    // chrome; a distinct hover treatment (CSS) marks the mode instead.
    let xray_click_id = message.id();
    let handle_xray_click = move |e: MouseEvent| {
        if !crate::xray::state().enabled.get_untracked() {
            return;
        }
        // Inner interactive elements (menu trigger, reactions, links) keep
        // their own behavior.
        if let Some(target) = e.target() {
            if let Ok(el) = target.dyn_into::<web_sys::Element>() {
                if el.closest("button, a").ok().flatten().is_some() {
                    return;
                }
            }
        }
        use ankurah::View as _;
        crate::xray::state().open_inspector(MessageView::collection(), xray_click_id.clone());
    };
    let message_for_bar = message.clone();
    // This row's reaction chips, from the shared per-message map (#14).
    let chips = Signal::derive({
        let msg_id = message.id().to_base64();
        move || reaction_chips.with(|m| m.get(&msg_id).cloned().unwrap_or_default())
    });
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
    let message_for_tomb = message.clone();

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
                                    let author_for_label = author_for_avatar.clone();
                                    view! {
                                        <button
                                            type="button"
                                            class=format!("avatar {}", avatar_hue)
                                            aria-label=move || {
                                                format!(
                                                    "View profile: {}",
                                                    author_for_label()
                                                        .map(|u| u.display_name().unwrap_or_default())
                                                        .filter(|n| !n.is_empty())
                                                        .unwrap_or_else(|| "Unknown".to_string()),
                                                )
                                            }
                                            on:click=open_profile_from_avatar
                                        >
                                            {move || {
                                                fmt::initials(
                                                    &author_for_avatar()
                                                        .map(|u| u.display_name().unwrap_or_default())
                                                        .unwrap_or_default(),
                                                )
                                            }}
                                        </button>
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
                                    <button
                                        type="button"
                                        class="messageAuthor"
                                        title="View profile"
                                        on:click=open_profile_from_name.clone()
                                    >
                                        {move || {
                                            author_for_name()
                                                .map(|u| u.display_name().unwrap_or_default())
                                                .filter(|n| !n.is_empty())
                                                .unwrap_or_else(|| "Unknown".to_string())
                                        }}
                                    </button>
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
                        if is_deleted_for_class() {
                            classes.push("tombstone");
                        }
                        if crate::xray::state().enabled.get() {
                            use ankurah::View as _;
                            classes.push("xrayInspectable");
                            // Track the entity so head changes re-evaluate the
                            // concurrency tint (only while x-ray is on).
                            message_for_xray.track();
                            if message_for_xray.entity().head().len() > 1 {
                                classes.push("xrayConcurrent");
                            }
                        }
                        classes.join(" ")
                    }
                    data-msg-id=message_id.clone()
                    title=stamp
                    on:contextmenu=handle_context_menu
                    on:click=handle_xray_click
                >
                    <Show
                        when={
                            let is_deleted = is_deleted.clone();
                            move || is_deleted()
                        }
                        fallback={
                            let message_for_text = message_for_text.clone();
                            let message_for_edited = message_for_edited.clone();
                            move || {
                                let message_for_text = message_for_text.clone();
                                let message_for_edited = message_for_edited.clone();
                                view! {
                                    // Reactive read: CRDT text edits (local or remote)
                                    // re-render the bubble; markdown parses only when
                                    // the text changes.
                                    <div class="messageText">
                                        {move || {
                                            crate::markdown::render_message(
                                                &message_for_text.text().unwrap_or_default(),
                                            )
                                        }}
                                        {move || {
                                            message_for_edited
                                                .edited_at()
                                                .ok()
                                                .flatten()
                                                .map(|ts| {
                                                    view! {
                                                        <span
                                                            class="messageEdited"
                                                            title=format!("Edited {}", fmt::full_stamp(ts))
                                                        >
                                                            "(edited)"
                                                        </span>
                                                    }
                                                })
                                        }}
                                    </div>
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
                            }
                        }
                    >
                        {
                            let message_for_tomb = message_for_tomb.clone();
                            move || view! { <TombstoneNotice message=message_for_tomb.clone() /> }
                        }
                    </Show>
                </div>
                // Reaction chips (#14): under the bubble, never on tombstones.
                <Show when={
                    let is_deleted = is_deleted.clone();
                    move || !chips.get().is_empty() && !is_deleted()
                }>
                    {
                        let message_for_bar = message_for_bar.clone();
                        move || view! { <ReactionBar message=message_for_bar.clone() chips=chips /> }
                    }
                </Show>
                <Show when=move || profile.get().is_some()>
                    {move || {
                        profile
                            .get()
                            .map(|(user, px, py)| {
                                view! {
                                    <ProfilePopover
                                        user=user
                                        x=px
                                        y=py
                                        on_close=move || profile.set(None)
                                    />
                                }
                            })
                    }}
                </Show>
                <Show when={
                    let is_deleted = is_deleted.clone();
                    move || context_menu.get().is_some() && !is_deleted()
                }>
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

/// Tombstone body for a deleted message (#10). Attribution follows the
/// lights-on ruling's simple heuristic: a matching public `ModAction` row
/// means a moderator removed it; no row means the author did. The LiveQuery
/// mounts only for tombstoned rows, so the per-row cost stays confined to
/// the rare case.
#[component]
fn TombstoneNotice(message: MessageView) -> impl IntoView {
    let mod_actions = crate::queries::selection("message = ? AND action = 'delete'", [(&message.id()).into()])
        .ok()
        .and_then(|sel| crate::ctx().query::<ModActionView>(sel).ok());
    let label = move || {
        let by_moderator = mod_actions.as_ref().map(|q| !q.get().is_empty()).unwrap_or(false);
        if by_moderator { "Removed by a moderator" } else { "Removed by the author" }
    };
    view! { <div class="messageText tombstoneNotice">{label}</div> }
}
