//! Blocking a member: the reader's own screen, kept on the reader's own
//! device.
//!
//! FOR: getting somebody out of your face without asking anyone's permission
//! and without waiting for a moderator. Reporting hands a judgement to a
//! moderator and takes as long as that takes; a ban is the community's verdict
//! on a member and belongs to moderators. Blocking is neither — it is one
//! reader deciding they are done reading one other person, which needs no
//! verdict and no delay.
//!
//! PER DEVICE, and the copy says so. The list is a `localStorage` entry of
//! `User` entity ids, so a member who blocks somebody on their laptop still
//! sees them on their phone. That is the v1 ruling and not an oversight: a
//! synced list would be a collection naming who avoids whom, readable by the
//! server and — however the policy were written — one rule away from being
//! readable by the person blocked. The honest label is on the control:
//! "Blocked on this device".
//!
//! WHAT IT HIDES, AND WHERE. Nothing here touches a query or a policy: blocked
//! members' rows still sync, and the server neither knows nor cares. Hiding is
//! the renderer's job, through [`is_blocked`] — which the community surfaces
//! read directly and which the chat components read through `chat_hooks`'
//! message veil. Reading it inside a reactive context re-renders when the list
//! changes, so blocking somebody clears them from the screen without a reload,
//! and unblocking brings them back the same way.

use std::collections::BTreeSet;
use std::sync::OnceLock;

use ankurah::EntityId;
use ankurah_signals::Get as AnkurahGet;
use community_model::mention_display::MemberDirectory;
use community_model::MessageView;
use leptos::prelude::*;
use web_sys::window;

/// localStorage key holding the blocked ids, as a JSON array of base64 `User`
/// entity ids (the `NotificationPref.muted_rooms` encoding, on this side of the
/// wire).
const LS_BLOCKED_USERS: &str = "community_blocked_users";

/// Whether this device has blocked `user`, reactively: a caller inside a
/// reactive context re-runs when the list changes.
///
/// This is the seam the chat components consume — a message row asks this
/// through the veil hook `chat_hooks` installs, and the row re-renders on
/// unblock because it asked reactively.
pub fn is_blocked(user: EntityId) -> bool { blocked().list.with(|ids| ids.contains(&user.to_base64())) }

/// Block `user` on this device (no-op if already blocked).
pub fn block(user: EntityId) {
    update(|ids| {
        ids.insert(user.to_base64());
    })
}

/// Unblock `user` on this device (no-op if not blocked).
pub fn unblock(user: EntityId) {
    update(|ids| {
        ids.remove(&user.to_base64());
    })
}

/// Apply a change to the list and write it back to storage.
///
/// Storage is written from the signal rather than the other way round, so the
/// screen updates whether or not the browser kept the change — a reader who
/// blocks somebody in a private window gets what they asked for, for as long
/// as that window lives, and is not told a lie about it lasting.
fn update(change: impl FnOnce(&mut BTreeSet<String>)) {
    let list = blocked().list;
    list.update(change);
    let payload = list.with_untracked(|ids| serde_json::to_string(&ids.iter().collect::<Vec<_>>()).unwrap_or_else(|_| "[]".to_string()));
    if let Some(storage) = local_storage() {
        if storage.set_item(LS_BLOCKED_USERS, &payload).is_err() {
            tracing::warn!("could not persist the block list — it will apply for this session only");
        }
    }
}

/// The one list, loaded from storage on first use.
///
/// `BTreeSet` rather than `HashSet` so the stored JSON is stable between
/// writes: a set that reordered itself on every change would rewrite the whole
/// entry for no reason and make the stored value impossible to eyeball.
#[derive(Clone)]
struct BlockList {
    list: ArcRwSignal<BTreeSet<String>>,
}

static STATE: OnceLock<BlockList> = OnceLock::new();

fn blocked() -> BlockList { STATE.get_or_init(|| BlockList { list: ArcRwSignal::new(load()) }).clone() }

/// Read the stored list. Anything unreadable — no storage, no entry, JSON that
/// is not an array of strings — is an empty list: a block list that cannot be
/// read is a block list nobody is on, and inventing entries from a half-parsed
/// value would hide messages the reader never asked to hide.
fn load() -> BTreeSet<String> {
    let Some(storage) = local_storage() else { return BTreeSet::new() };
    let Ok(Some(raw)) = storage.get_item(LS_BLOCKED_USERS) else { return BTreeSet::new() };
    match serde_json::from_str::<Vec<String>>(&raw) {
        Ok(ids) => ids.into_iter().collect(),
        Err(e) => {
            tracing::warn!("stored block list could not be read ({e}) — starting empty");
            BTreeSet::new()
        }
    }
}

fn local_storage() -> Option<web_sys::Storage> { window()?.local_storage().ok().flatten() }

/// What a blocked member's message row shows instead of what they said.
///
/// FOR: blocking is a decision about a person, and a reader who has made it
/// still runs into the occasional line they need — the one somebody else is
/// answering, the one that names them. Unblocking to read it would let the
/// whole room back in, so this offers the one row and leaves the block exactly
/// where it is. Nothing here undoes a block: that lives on the member panels,
/// where the copy can say what it means.
///
/// THE LOOK IS THIS ROW'S AND THIS VISIT'S — an ordinary signal, written
/// nowhere and gone when the row is rebuilt, which is what blocking or
/// unblocking anybody does to every row that asked [`is_blocked`]. And it is a
/// LOOK rather than the message handed back: the stored text with mentions read
/// as names, without markdown, without the reply preview, and without the
/// reaction chips and actions menu the chat components hold back from a veiled
/// row.
#[component]
pub fn BlockedVeil(message: MessageView) -> impl IntoView {
    let revealed = RwSignal::new(false);
    // Names rather than tokens, for the reason a bubble resolves them too: a
    // `<@id>` names nobody to a reader. The directory is the chat handshake's
    // members query — the roster every other surface names people from — and a
    // session that cannot list it leaves the tokens as they stand.
    let chat = ankurah_chat_leptos::chat();
    let text = move || {
        let stored = message.text().unwrap_or_default();
        match chat.members() {
            Some(members) => {
                MemberDirectory::new(members.get().iter().map(|u| (u.id().to_base64(), u.display_name().unwrap_or_default())))
                    .decode(&stored)
            }
            None => stored,
        }
    };
    view! {
        <div class="blockedVeil">
            <div class="blockedVeilLine">
                <span class="blockedVeilLabel">"Blocked member"</span>
                <button class="blockedVeilToggle" on:click=move |_| revealed.update(|open| *open = !*open)>
                    {move || if revealed.get() { "Hide" } else { "Show" }}
                </button>
            </div>
            {move || revealed.get().then(|| view! { <div class="blockedVeilText">{text()}</div> })}
        </div>
    }
}

/// Block / unblock for one member, with the copy that says what it does.
///
/// Offered on everybody but yourself. Unlike the ban controls beside it in the
/// member sidebar, this is not a moderator's affordance and carries no role
/// gate: blocking is what every reader may do about their own screen, and a
/// reader with no account at all is still a reader.
#[component]
pub fn BlockControl(
    user_id: EntityId,
    /// The member's display name, for the button's label.
    name: Signal<String>,
    /// Compact presentation for the profile popover, which has no room for the
    /// sidebar's heading and note.
    #[prop(optional)]
    compact: bool,
) -> impl IntoView {
    if crate::viewer() == Some(user_id) {
        return ().into_any();
    }
    let blocked_now = move || is_blocked(user_id);

    let button = view! {
        <button
            class=move || {
                if compact {
                    "profileBlockBtn".to_string()
                } else if blocked_now() {
                    "userDetailActionBtn".to_string()
                } else {
                    "userDetailActionBtn userDetailActionDanger".to_string()
                }
            }
            on:click=move |_| if blocked_now() { unblock(user_id) } else { block(user_id) }
        >
            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"
                stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
                <circle cx="12" cy="12" r="9" />
                <path d="m5.6 5.6 12.8 12.8" />
            </svg>
            {move || if blocked_now() { "Unblock".to_string() } else { format!("Block {}", name.get()) }}
        </button>
    };

    if compact {
        return view! {
            <div class="profileBlock">
                {button}
                <Show when=blocked_now>
                    <p class="profileBlockNote">"Blocked on this device — their messages are hidden here."</p>
                </Show>
            </div>
        }
        .into_any();
    }

    view! {
        <div class="userDetailActions">
            <h3 class="userDetailActionsTitle">"On this device"</h3>
            {button}
            <p class="userDetailMessageNote">
                {move || {
                    if blocked_now() {
                        "Blocked on this device — their messages are hidden here."
                    } else {
                        "Blocking hides their messages on this device only, and tells them nothing."
                    }
                }}
            </p>
        </div>
    }
    .into_any()
}
