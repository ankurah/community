//! What community answers when the chat components ask.
//!
//! The components render a message row without knowing what a `LinkPreview` or
//! a `ModAction` is — those collections are community's, not the shared chat
//! model's — so three places in a row are left for the embedder to fill, and
//! this module fills them:
//!
//! - under a bubble, the unfurl card for the first link that unfurled;
//! - inside a tombstone, who removed the message;
//! - in place of a blocked member's message, the veil that says so and offers
//!   the reader one look;
//! - in front of the composer's send and of creating a room, the guidelines
//!   gate;
//! - behind the actions menu's moderator Delete, the removal itself, with its
//!   optional public reason and the log row that records it;
//! - at the foot of that menu, three entries: Report message, Block author, and
//!   x-ray's Inspect — the last of which is how the inspector stays reachable
//!   from the keyboard, since a bubble is not a tab stop and the delegated
//!   click handler in `xray::inspect` only answers a mouse.
//!
//! Two more are plain routing: clicking an author opens the profile popover,
//! clicking an `@mention` opens the member detail panel.

use std::collections::HashMap;
use std::rc::Rc;

use leptos::prelude::*;
use web_sys::window;

use ankurah::EntityId;
use ankurah::View as _;
use ankurah_chat_leptos::ChatHooks;
use ankurah_signals::Get as AnkurahGet;
use community_model::{LinkPreviewView, MessageView, ModAction, ModActionView};

use crate::blocklist::BlockedVeil;
use crate::link_preview::LinkPreviewCard;
use crate::panels::{panels, Surface};

/// Where the profile popover should open, as the row that was clicked reports
/// it: the member, and the trigger's bottom-left corner in viewport space.
pub type ProfileAnchor = RwSignal<Option<(EntityId, i32, i32)>>;

/// Assemble the hooks. `previews` is the app-wide url → row map the link cards
/// look themselves up in; `profile` is the signal `ChatApp` renders the
/// popover from; `viewer` is who is reading, `None` for a guest.
///
/// The profile popover and the member-detail panel both open on member-only
/// data (the popover reads `userroles`, the panel the roster), so both are
/// withheld from a guest: passing `None` for those hooks makes the crate
/// render the avatar and `@mention` as inert text rather than a button that
/// opens an empty surface — a button only where it leads somewhere.
/// BUILT FROM `Default` AND ASSIGNED INTO, not written as a literal. `ChatHooks`
/// is `#[non_exhaustive]`, so the crate can add a door without breaking every
/// embedder that had named all of today's — and Rust admits no struct expression
/// for such a type from another crate, functional update syntax included. A hook
/// this app leaves unset is a door it does not open, and the crate renders the
/// affordance away rather than leaving a control that does nothing.
pub fn chat_hooks(previews: Memo<HashMap<String, LinkPreviewView>>, profile: ProfileAnchor, viewer: Option<EntityId>) -> ChatHooks {
    let signed_in = viewer.is_some();
    let mut hooks = ChatHooks::default();
    hooks.message_extras = Some(Box::new(move |message: MessageView| {
        view! { <LinkPreviewCard message=message previews=previews /> }.into_any()
    }));
    hooks.tombstone_body = Some(Box::new(|message: MessageView| view! { <TombstoneNotice message=message /> }.into_any()));
    hooks.message_veil = Some(Box::new(blocked_veil));
    hooks.moderator_delete = Some(Box::new(moderator_delete));
    // The guidelines, in front of every write the crate gates — the composer's
    // send and creating a room — exactly as they stand in front of every other
    // write leptos-app owns. `demand_terms_boxed` runs an accepted reader's
    // continuation inside the same click, which is what the components ask of a
    // gate.
    hooks.gate_write = Some(Box::new(crate::terms::demand_terms_boxed));
    hooks.member_preview = signed_in
        .then(|| Box::new(move |user: EntityId, x: i32, y: i32| profile.set(Some((user, x, y)))) as Box<dyn Fn(EntityId, i32, i32)>);
    hooks.member_detail = signed_in.then(|| Box::new(|user: EntityId| panels().open(Surface::UserDetail(user))) as Box<dyn Fn(EntityId)>);
    hooks.menu_actions = Some(Box::new(menu_entries));
    hooks
}

/// A blocked member's row, veiled.
///
/// Reads [`crate::blocklist::is_blocked`] and nothing else, which is what makes
/// the row follow the list: the components ask this inside a reactive pass, so
/// blocking somebody veils their rows and unblocking brings them straight back.
/// A message whose author cannot be resolved is left alone — a row nobody can
/// name is a row no block list can match.
fn blocked_veil(message: MessageView) -> Option<AnyView> {
    let author = message.user().ok()?.id();
    crate::blocklist::is_blocked(author).then(|| view! { <BlockedVeil message=message /> }.into_any())
}

/// What community puts at the foot of a message's actions menu.
///
/// Three entries. Two are the member-safety pair — hand this to a moderator,
/// or take this person off your own screen — and the third is the inspector.
/// The order is what a reader might actually need first, developer tool last.
///
/// NEITHER OF THE PAIR APPEARS ON YOUR OWN MESSAGE. Reporting yourself gives a
/// moderator nothing to act on, and blocking yourself would veil your own half
/// of the room — the same rule `BlockControl` already keeps in the member
/// sidebar. A guest owns no message, so a guest is offered both, and meets the
/// sign-in demand on pressing either (each entry says why).
fn menu_entries(message: MessageView, close: Box<dyn Fn()>) -> AnyView {
    // One `close` arrives and three entries want it. Each of them closes the
    // menu on every path out of itself, including the paths that do nothing.
    let close: Rc<dyn Fn()> = Rc::from(close);
    let mine = crate::viewer().is_some() && crate::viewer() == message.user().ok().map(|r| r.id());
    view! {
        {(!mine).then(|| report_entry(message.clone(), close.clone()))}
        {(!mine).then(|| block_entry(message.clone(), close.clone()))}
        {inspect_entry(message, close)}
    }
    .into_any()
}

/// Hand this message to the moderators.
///
/// A GUEST IS OFFERED IT AND MEETS SIGN-IN, which is how the actions menu
/// already treats what writes: a reaction is offered to a reader with no viewer
/// and raises the components' auth demand when they press it, and this does the
/// same. Hiding the entry instead would teach a guest that reporting is not on
/// offer here.
fn report_entry(message: MessageView, close: Rc<dyn Fn()>) -> AnyView {
    // Taken in the body, where the reactive owner is, and cloned into the
    // handler — the crate's own rule for anything that may defer.
    let chat = ankurah_chat_leptos::chat();
    view! {
        <button
            class="contextMenuItem"
            role="menuitem"
            on:click=move |_| {
                if crate::viewer().is_none() {
                    // Closed first: the ceremony is a card over the app, and a
                    // menu left open would stand in front of it.
                    close();
                    chat.demand_auth();
                    return;
                }
                let close = close.clone();
                crate::reports::report_message(message.clone(), Box::new(move || close()));
            }
        >
            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"
                stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
                <path d="M4 21V4h11l1 2h4v10h-6l-1-2H4" />
            </svg>
            "Report message"
        </button>
    }
    .into_any()
}

/// Take this author off this device's screen.
///
/// No guidelines gate: nothing is written and nobody is told, so a modal in
/// front of it would be a gate over nothing — the reading that already leaves
/// the inbox's own seen flip ungated. A GUEST MEETS SIGN-IN even so, and that
/// is the one place this departs from `blocklist`'s "a reader with no account
/// at all is still a reader": the way back out of a block is the member sidebar
/// and the profile card, and community withholds both from a guest, so a guest
/// who blocked from here would be veiled into a room with no handle to undo it.
fn block_entry(message: MessageView, close: Rc<dyn Fn()>) -> AnyView {
    let chat = ankurah_chat_leptos::chat();
    view! {
        <button
            class="contextMenuItem"
            role="menuitem"
            on:click=move |_| {
                close();
                if crate::viewer().is_none() {
                    chat.demand_auth();
                    return;
                }
                if let Ok(author) = message.user() {
                    crate::blocklist::block(author.id());
                }
            }
        >
            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"
                stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
                <circle cx="12" cy="12" r="9" />
                <path d="m5.6 5.6 12.8 12.8" />
            </svg>
            "Block author"
        </button>
    }
    .into_any()
}

/// X-ray's "Inspect" entry, offered only while the mode is on.
///
/// It exists for the keyboard. `xray::inspect` installs a delegated click
/// listener over every bubble's `data-entity-id`, which serves a mouse
/// perfectly and a keyboard not at all — a bubble is not focusable and there is
/// nothing to tab to. The actions menu is, so the same target is reachable
/// here. The menu mounts fresh on every open, so a non-reactive read of the
/// mode is correct.
fn inspect_entry(message: MessageView, close: Rc<dyn Fn()>) -> AnyView {
    if !crate::xray::state().enabled.get_untracked() {
        return ().into_any();
    }
    let message_id = message.id();
    view! {
        <button
            class="contextMenuItem"
            role="menuitem"
            on:click=move |_| {
                crate::xray::state().open_inspector(MessageView::collection(), message_id);
                close();
            }
        >
            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"
                stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
                <circle cx="11" cy="11" r="7" />
                <path d="m21 21-4.3-4.3" />
            </svg>
            "Inspect (X-ray)"
        </button>
    }
    .into_any()
}

/// Tombstone body for a deleted message. Attribution follows the lights-on
/// ruling's simple heuristic: a matching public `ModAction` row means a
/// moderator removed it; no row means the author did. The LiveQuery mounts
/// only for tombstoned rows, so the per-row cost stays confined to the rare
/// case.
///
/// THAT HEURISTIC ONLY HOLDS IF WE COULD LOOK, so there are three states here
/// and not two. `modaction` is signed-in-only in policy.json, so a guest's
/// query is refused outright at the collection gate; a member's query is fine
/// but answers empty for the moment before it loads. Reading either absence
/// as "no row exists" prints "Removed by the author" over a moderator's
/// removal — a false statement about a person, shown to every anonymous
/// reader of every moderated message. A query we could not open, and one that
/// has not answered yet, therefore both say "Removed" and claim nothing.
///
/// The fix stays on this side deliberately. Letting a guest read `modaction`
/// would trade a wrong label for a moderation record the street can page
/// through, which is what the `signed_in` privilege exists to refuse.
#[component]
fn TombstoneNotice(message: MessageView) -> impl IntoView {
    let mod_actions = crate::queries::selection("message = ? AND action = 'delete'", [(&message.id()).into()])
        .ok()
        .and_then(|sel| crate::ctx().query::<ModActionView>(sel).ok());
    let label = move || match mod_actions.as_ref() {
        None => "Removed",
        Some(query) if !query.loaded() => "Removed",
        Some(query) if query.get().is_empty() => "Removed by the author",
        Some(_) => "Removed by a moderator",
    };
    view! { <div class="messageText tombstoneNotice">{label}</div> }
}

/// A moderator removing someone else's message.
///
/// The removal may carry an optional public reason. Cancelling the prompt
/// aborts it; an empty OK proceeds without one; a blocked dialog (Err) never
/// blocks moderation. The tombstone and the log row commit together — the
/// public `ModAction` is what makes the tombstone read "by a moderator", so a
/// removal that landed without one would attribute itself to the author.
fn moderator_delete(message: MessageView, close: Box<dyn Fn()>) {
    crate::terms::demand_terms(move || moderator_delete_confirmed(message, close));
}

/// The removal itself, once the guidelines gate has let it through. Split so
/// the prompt still opens inside the moderator's own click (see the same split
/// on `ban_member`).
fn moderator_delete_confirmed(message: MessageView, close: Box<dyn Fn()>) {
    let reason = match window().map(|w| w.prompt_with_message("Reason for removal (optional):")) {
        Some(Ok(None)) => {
            close();
            return; // prompt cancelled — abort the removal
        }
        Some(Ok(Some(text))) => {
            let text = text.trim().to_string();
            (!text.is_empty()).then_some(text)
        }
        _ => None,
    };

    wasm_bindgen_futures::spawn_local(async move {
        match async {
            // A removal has to name who made it. `actor: None` is not a
            // fallback — the log renders it as "Automatic", which is the DM
            // rate limiter's row, not a moderator's — so a caller with no
            // viewer is refused here rather than mislabelled. Unreachable in
            // practice (the menu entry is a moderator's), and cheap to state.
            let actor = crate::viewer().ok_or("no signed-in moderator to attribute this removal to")?;
            let trx = crate::ctx().begin();
            let mutable = message.edit(&trx)?;
            mutable.deleted().set(&true)?;
            // Lights-on moderation ruling: deleting also clears the CRDT text
            // — the tombstone row survives, the content does not.
            mutable.text().replace("")?;
            trx.create(&ModAction {
                actor: Some(actor.into()),
                message: Some(ankurah::Ref::from(&message)),
                user: None,
                action: "delete".to_string(),
                reason,
                created_at: js_sys::Date::now() as i64,
            })
            .await?;
            trx.commit().await?;
            Ok::<_, Box<dyn std::error::Error>>(())
        }
        .await
        {
            Ok(_) => tracing::info!("Message deleted"),
            Err(e) => tracing::error!("Failed to delete message: {}", e),
        }
        close();
    });
}
