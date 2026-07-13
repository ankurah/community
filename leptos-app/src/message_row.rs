use leptos::ev::MouseEvent;
use leptos::prelude::*;
use wasm_bindgen::JsCast;

use ankurah::{EntityId, LiveQuery};
use ankurah_signals::Get as AnkurahGet;
use community_model::{LinkPreviewView, MessageView, UserView};

use std::collections::HashMap;

use crate::fmt;
use crate::link_preview::LinkPreviewCard;
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
    /// The composer's reply state (#23), armed by this row's context menu.
    replying_to: RwSignal<Option<MessageView>>,
    first_in_group: bool,
    last_in_group: bool,
    day_label: Option<String>,
    /// Render-ready reaction chips per message id (shared, built once in the
    /// list — see message_list.rs).
    reaction_chips: Memo<HashMap<String, Vec<ReactionChip>>>,
    /// Mention id → display name (#18; shared, built once in the list).
    mention_names: Memo<HashMap<String, String>>,
    /// Successful link previews by url (#20; shared, built once in the list
    /// from its one LiveQuery). Each row looks its own URLs up by key.
    link_previews: Memo<HashMap<String, LinkPreviewView>>,
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

    // Right-click opens the actions menu at the cursor (#59) — additive to
    // the ⋯ trigger and its keyboard path. The browser menu is suppressed on
    // the bubble only, and two spots fall through to it deliberately:
    // tombstones (no actions to offer) and links (the browser's open-in-new-
    // tab / copy-link menu is the useful one there).
    let handle_context_menu = move |e: MouseEvent| {
        if is_deleted_for_menu() {
            return;
        }
        if let Some(target) = e.target() {
            if let Ok(el) = target.dyn_into::<web_sys::Element>() {
                if el.closest("a").ok().flatten().is_some() {
                    return;
                }
            }
        }
        e.prevent_default();
        opened_from_trigger.set_value(false);
        context_menu.set(Some((e.client_x(), e.client_y())));
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
    let message_for_collab = message.clone();
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
    let message_for_preview = message.clone();
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

    // Reply context (#23): the referenced original's id, if any. `re` is set
    // at creation and never edited, so a non-reactive read is correct.
    let reply_target = message.re().ok().flatten().map(|r| r.id());

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
                            let message_for_collab = message_for_collab.clone();
                            let reply_target = reply_target.clone();
                            move || {
                                let message_for_text = message_for_text.clone();
                                let message_for_edited = message_for_edited.clone();
                                let message_for_collab = message_for_collab.clone();
                                view! {
                                    // Embedded reply preview (#23): the referenced
                                    // original, above the body, inside the bubble.
                                    {reply_target
                                        .clone()
                                        .map(|target| view! { <ReplyPreview target mention_names /> })}
                                    // Reactive read: CRDT text edits (local or remote)
                                    // re-render the bubble; markdown parses when the
                                    // text — or the mention-name map (#18) — changes.
                                    <div class="messageText">
                                        {move || {
                                            mention_names.with(|names| {
                                                crate::markdown::render_message(
                                                    &message_for_text.text().unwrap_or_default(),
                                                    names,
                                                )
                                            })
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
                                        // Co-edit indicator (#38): persistent, subtle, and
                                        // reactive — a remote toggle flips it live. It is
                                        // the discoverability surface for the "Edit" item
                                        // other members get on this message.
                                        {move || {
                                            message_for_collab
                                                .collaborative()
                                                .ok()
                                                .flatten()
                                                .unwrap_or(false)
                                                .then(|| {
                                                    view! {
                                                        <span
                                                            class="coEditBadge"
                                                            title="Collaborative message — anyone can edit it (right-click for Edit)"
                                                        >
                                                            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor"
                                                                stroke-width="2.4" stroke-linecap="round"
                                                                stroke-linejoin="round" aria-hidden="true">
                                                                <path d="M17 3a2.8 2.8 0 1 1 4 4L7.5 20.5 2 22l1.5-5.5z" />
                                                            </svg>
                                                            "co-edit"
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
                // Link preview card (#20): under the bubble (outside it — the
                // bubble root and its data-msg-id contract stay untouched),
                // never on tombstones. The component itself decides whether
                // any of the message's URLs has a preview worth rendering.
                <Show when={
                    let is_deleted = is_deleted.clone();
                    move || !is_deleted()
                }>
                    {
                        let message_for_preview = message_for_preview.clone();
                        move || {
                            view! {
                                <LinkPreviewCard
                                    message=message_for_preview.clone()
                                    previews=link_previews
                                />
                            }
                        }
                    }
                </Show>
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
                                        replying_to=replying_to
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

/// Embedded preview of the replied-to message (#23): author + one-line
/// snippet inside the reply's bubble. The ref resolves once via `ctx().get`
/// (local-first, then the peer — the TombstoneNotice per-row idiom: cost
/// confined to rows that actually carry `re`); the resolved view is live
/// afterwards, so an edit or delete of the original re-renders the preview
/// (tombstones render "Removed message"). Clicking jumps to the original
/// when it is rendered in the current timeline; a target outside the loaded
/// window does nothing — deliberately no pagination gymnastics here
/// (first-class jump APIs are the ankurah#357 item 4 ask).
#[component]
fn ReplyPreview(target: EntityId, mention_names: Memo<HashMap<String, String>>) -> impl IntoView {
    let original = RwSignal::new(None::<MessageView>);
    let unresolvable = RwSignal::new(false);
    {
        let target = target.clone();
        wasm_bindgen_futures::spawn_local(async move {
            match crate::ctx().get::<MessageView>(target.clone()).await {
                // try_set: the row may have been unmounted (virtual scroll)
                // before the fetch resolved.
                Ok(m) => {
                    let _ = original.try_set(Some(m));
                }
                Err(e) => {
                    tracing::warn!("Failed to resolve reply target {}: {}", target.to_base64(), e);
                    let _ = unresolvable.try_set(true);
                }
            }
        });
    }

    let target_b64 = target.to_base64();
    let jump = move |e: MouseEvent| {
        // Keep the click off the bubble's own handlers (x-ray exempts
        // buttons, but be explicit).
        e.stop_propagation();
        jump_to_message(&target_b64);
    };

    view! {
        <button type="button" class="replyPreview" title="Jump to the original message" on:click=jump>
            {move || match (original.get(), unresolvable.get()) {
                (Some(orig), _) => {
                    if orig.deleted().unwrap_or(false) {
                        view! { <span class="replyPreviewGone">"Removed message"</span> }.into_any()
                    } else {
                        let author_id = orig.user().map(|r| r.id().to_base64()).unwrap_or_default();
                        let (author, snippet) = mention_names
                            .with(|names| {
                                (
                                    names
                                        .get(&author_id)
                                        .cloned()
                                        .filter(|n| !n.is_empty())
                                        .unwrap_or_else(|| "Unknown".to_string()),
                                    crate::mentions::reply_snippet(
                                        &orig.text().unwrap_or_default(),
                                        names,
                                    ),
                                )
                            });
                        view! {
                            <span class="replyPreviewAuthor">{author}</span>
                            <span class="replyPreviewSnippet">{snippet}</span>
                        }
                            .into_any()
                    }
                }
                (None, true) => {
                    view! { <span class="replyPreviewGone">"Unavailable message"</span> }.into_any()
                }
                // Resolving (usually a single local read) — keep the frame so
                // the row's height settles in one step, not two.
                (None, false) => view! { <span class="replyPreviewGone">"\u{2026}"</span> }.into_any(),
            }}
        </button>
    }
}

/// Scroll a message's bubble into view with a brief highlight wash — only if
/// it is currently rendered (the virtual scroller keeps a bounded window; a
/// target outside it has no DOM node and the click quietly does nothing).
fn jump_to_message(msg_id_b64: &str) {
    let Some(doc) = web_sys::window().and_then(|w| w.document()) else { return };
    // Base64url ids ([A-Za-z0-9_-]) are inert inside a quoted attribute selector.
    let selector = format!(".messageBubble[data-msg-id=\"{msg_id_b64}\"]");
    let Ok(Some(el)) = doc.query_selector(&selector) else { return };
    let options = web_sys::ScrollIntoViewOptions::new();
    options.set_behavior(web_sys::ScrollBehavior::Smooth);
    options.set_block(web_sys::ScrollLogicalPosition::Center);
    el.scroll_into_view_with_scroll_into_view_options(&options);
    // The wash animation plays once; removing the class afterwards lets a
    // repeat click re-trigger it. Timeout outlives the animation (1.4s).
    let _ = el.class_list().add_1("replyJumpFlash");
    let closure = wasm_bindgen::closure::Closure::once_into_js({
        let el = el.clone();
        move || {
            let _ = el.class_list().remove_1("replyJumpFlash");
        }
    });
    if let Some(win) = web_sys::window() {
        let _ = win.set_timeout_with_callback_and_timeout_and_arguments_0(closure.unchecked_ref(), 1600);
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
