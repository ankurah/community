//! What community answers when the chat components ask.
//!
//! The components render a message row without knowing what a `LinkPreview` or
//! a `ModAction` is — those collections are community's, not the shared chat
//! model's — so three places in a row are left for the embedder to fill, and
//! this module fills them:
//!
//! - under a bubble, the unfurl card for the first link that unfurled;
//! - inside a tombstone, who removed the message;
//! - behind the actions menu's moderator Delete, the removal itself, with its
//!   optional public reason and the log row that records it.
//!
//! Two more are plain routing: clicking an author opens the profile popover,
//! clicking an `@mention` opens the member detail panel.

use std::collections::HashMap;

use leptos::prelude::*;
use web_sys::window;

use ankurah::EntityId;
use ankurah_chat_leptos::ChatHooks;
use ankurah_signals::Get as AnkurahGet;
use community_model::{LinkPreviewView, MessageView, ModAction, ModActionView};

use crate::link_preview::LinkPreviewCard;
use crate::panels::{panels, Surface};

/// Where the profile popover should open, as the row that was clicked reports
/// it: the member, and the trigger's bottom-left corner in viewport space.
pub type ProfileAnchor = RwSignal<Option<(EntityId, i32, i32)>>;

/// Assemble the hooks. `previews` is the app-wide url → row map the link cards
/// look themselves up in; `profile` is the signal `ChatApp` renders the
/// popover from.
pub fn chat_hooks(previews: Memo<HashMap<String, LinkPreviewView>>, profile: ProfileAnchor) -> ChatHooks {
    ChatHooks {
        message_extras: Some(Box::new(move |message: MessageView| {
            view! { <LinkPreviewCard message=message previews=previews /> }.into_any()
        })),
        tombstone_body: Some(Box::new(|message: MessageView| view! { <TombstoneNotice message=message /> }.into_any())),
        moderator_delete: Some(Box::new(moderator_delete)),
        member_preview: Some(Box::new(move |user: EntityId, x: i32, y: i32| profile.set(Some((user, x, y))))),
        member_detail: Some(Box::new(|user: EntityId| panels().open(Surface::UserDetail(user)))),
    }
}

/// Tombstone body for a deleted message. Attribution follows the lights-on
/// ruling's simple heuristic: a matching public `ModAction` row means a
/// moderator removed it; no row means the author did. The LiveQuery mounts
/// only for tombstoned rows, so the per-row cost stays confined to the rare
/// case.
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

/// A moderator removing someone else's message.
///
/// The removal may carry an optional public reason. Cancelling the prompt
/// aborts it; an empty OK proceeds without one; a blocked dialog (Err) never
/// blocks moderation. The tombstone and the log row commit together — the
/// public `ModAction` is what makes the tombstone read "by a moderator", so a
/// removal that landed without one would attribute itself to the author.
fn moderator_delete(message: MessageView, close: Box<dyn Fn()>) {
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
        match (|| async {
            let trx = crate::ctx().begin();
            let mutable = message.edit(&trx)?;
            mutable.deleted().set(&true)?;
            // Lights-on moderation ruling: deleting also clears the CRDT text
            // — the tombstone row survives, the content does not.
            mutable.text().replace("")?;
            trx.create(&ModAction {
                actor: Some(crate::current_user_id().into()),
                message: Some(ankurah::Ref::from(&message)),
                user: None,
                action: "delete".to_string(),
                reason,
                created_at: js_sys::Date::now() as i64,
            })
            .await?;
            trx.commit().await?;
            Ok::<_, Box<dyn std::error::Error>>(())
        })()
        .await
        {
            Ok(_) => tracing::info!("Message deleted"),
            Err(e) => tracing::error!("Failed to delete message: {}", e),
        }
        close();
    });
}
