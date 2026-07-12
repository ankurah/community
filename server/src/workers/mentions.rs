//! Mention fan-out: `<@id>` tokens in messages become `Notification` rows
//! (refs #18, #19, #25).
//!
//! Consumes `MessageView`s from the standing message LiveQuery (see
//! `workers::start`) and, for each mentioned user, creates ONE
//! `Notification { kind: "mention" }` under the privileged Root context —
//! the only path that can create rows for other users (the notification
//! write scope pins client writes to `recipient = $jwt.sub`).
//!
//! Invariants:
//! - Idempotent: at most one mention notification per (recipient, message),
//!   enforced by an existence query before each create — safe under the boot
//!   backlog sweep, edit-driven re-deliveries, and crash/restart replays.
//! - Resilient: a failure on one recipient/message is logged and never kills
//!   the loop; the message stays uncached so the next change to it retries.
//! - Pref-aware: a recipient's muted rooms suppress delivery. `mentions_only`
//!   never suppresses mentions — it exists to gate OTHER (future) kinds, and
//!   the check is structured per-kind so those kinds inherit it.

use std::collections::HashMap;

use ankurah::ankql::{ast::Expr, parser::parse_selection};
use ankurah::error::RetrievalError;
use ankurah::{Context, EntityId};
use anyhow::{Context as _, Result};
use community_model::{parse_mentions, MessageView, Notification, NotificationPrefView, NotificationView, UserView};
use tokio::sync::mpsc::UnboundedReceiver;
use tracing::{debug, info, warn};

use super::{now_ms, remember, signature};

/// The only notification kind this worker emits today. Stored verbatim in
/// `Notification.kind`; the client inbox matches on it.
const MENTION_KIND: &str = "mention";

/// Consumer loop: one message at a time, errors contained per message. The
/// receiver is borrowed from the supervisor (`workers::supervise`), which
/// respawns this loop if it ever panics.
pub async fn run(ctx: Context, rx: &mut UnboundedReceiver<MessageView>) {
    info!("notification fan-out worker started (mention tokens → notification rows)");
    // message id → signature of the mention list already fully delivered, so
    // per-keystroke edit Updates don't re-run storage queries. Purely an
    // optimization: a miss (or eviction) falls back to the existence checks.
    let mut delivered: HashMap<EntityId, u64> = HashMap::new();
    while let Some(msg) = rx.recv().await {
        let message_id = msg.id();
        if let Err(e) = process_message(&ctx, &msg, &mut delivered).await {
            warn!(message = %message_id, "mention fan-out failed (retries on the message's next change): {e:#}");
        }
    }
    // Only reachable if the producer side was dropped, i.e. the worker
    // subsystem is torn down with the process.
    warn!("notification fan-out worker: message stream closed; exiting");
}

async fn process_message(ctx: &Context, msg: &MessageView, delivered: &mut HashMap<EntityId, u64>) -> Result<()> {
    let text = msg.text().context("read message text")?;
    let mentions = parse_mentions(&text);
    let sig = signature(&mentions);
    if delivered.get(&msg.id()) == Some(&sig) {
        return Ok(());
    }
    if mentions.is_empty() {
        remember(delivered, msg.id(), sig);
        return Ok(());
    }

    // Carry plain EntityIds (Copy) through the loop — `Ref<T>`'s derived
    // Clone/Copy is bounded on `T`, which models don't implement — and mint
    // fresh `Ref`s via `.into()` at the create site.
    let author_id = msg.user().context("read message author")?.id();
    let room_id = msg.room().context("read message room")?.id();

    let mut all_delivered = true;
    for token in &mentions {
        match deliver(ctx, msg, token, author_id, room_id).await {
            Ok(Delivery::Created(recipient)) => {
                // The fan-out audit trail: who was notified about which
                // message. Ids only — never message text or token dumps.
                info!(recipient = %recipient, message = %msg.id(), room = %room_id, "mention notification created");
            }
            Ok(Delivery::Skipped(reason)) => {
                debug!(message = %msg.id(), "mention skipped: {reason}");
            }
            Err(e) => {
                all_delivered = false;
                warn!(message = %msg.id(), "mention delivery failed: {e:#}");
            }
        }
    }
    // Cache only fully-delivered messages so transient failures get retried
    // when the message next changes (best effort — there is no retry queue).
    if all_delivered {
        remember(delivered, msg.id(), sig);
    }
    Ok(())
}

enum Delivery {
    Created(EntityId),
    Skipped(&'static str),
}

async fn deliver(ctx: &Context, msg: &MessageView, token: &str, author_id: EntityId, room_id: EntityId) -> Result<Delivery> {
    // The parser guarantees charset, not validity — foreign tokens are
    // expected traffic (someone typing `<@lol>`), not errors.
    let Ok(recipient) = EntityId::from_base64(token) else {
        return Ok(Delivery::Skipped("token is not an entity id"));
    };
    if recipient == author_id {
        return Ok(Delivery::Skipped("self-mention"));
    }
    // Must name a real User (also rejects ids of non-user entities: the get
    // is scoped to the user collection). NotFound is a skip; anything else
    // (storage trouble) is a real error and must surface for retry.
    match ctx.get::<UserView>(recipient).await {
        Ok(_) => {}
        Err(RetrievalError::EntityNotFound(_)) | Err(RetrievalError::CollectionNotFound(_)) => {
            return Ok(Delivery::Skipped("mentioned id is not a user"));
        }
        Err(e) => return Err(e).context("look up mentioned user"),
    }
    if !pref_allows_delivery(ctx, recipient, MENTION_KIND, &room_id.to_base64()).await? {
        return Ok(Delivery::Skipped("suppressed by recipient's notification prefs"));
    }
    if mention_notification_exists(ctx, recipient, msg.id()).await? {
        return Ok(Delivery::Skipped("already notified"));
    }

    let trx = ctx.begin();
    trx.create(&Notification {
        recipient: recipient.into(),
        kind: MENTION_KIND.to_string(),
        message: Some(msg.id().into()),
        actor: Some(author_id.into()),
        room: Some(room_id.into()),
        created_at: now_ms(),
        seen: false,
    })
    .await
    .context("create notification")?;
    trx.commit().await.context("commit notification")?;
    Ok(Delivery::Created(recipient))
}

/// Idempotency probe: is there already a mention notification for this
/// (recipient, message)? Parameterized equality only — `message` is an
/// Option field, and rows without the property are excluded per-row (see the
/// `ModAction.message` equality-only note in the model).
async fn mention_notification_exists(ctx: &Context, recipient: EntityId, message: EntityId) -> Result<bool> {
    let predicate = parse_selection("recipient = ? AND message = ?")?
        .predicate
        .populate([Expr::from(&recipient), Expr::from(&message)])?;
    let existing = ctx.fetch::<NotificationView>(predicate).await?;
    // Filter by kind in code: a future kind referencing the same message
    // (e.g. "reaction") must not block the mention notification.
    for n in existing {
        if n.kind()? == MENTION_KIND {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Whether the recipient's `NotificationPref` (if any) allows delivering
/// `kind` for an event in `room`. Runs under Root, which bypasses the
/// pref collection's owner-only scope. No pref row means default-allow.
///
/// Duplicate rows can exist (two devices racing their first-ever write; rows
/// are not deletable in ankurah 0.9.0). The client pins the LOWEST id — by
/// base64, its exact comparator — as THE row for display and edits, so this
/// evaluates only that row: honoring a twin the UI neither shows nor edits
/// would suppress a room's notifications forever with no user-reachable
/// repair.
async fn pref_allows_delivery(ctx: &Context, recipient: EntityId, kind: &str, room_b64: &str) -> Result<bool> {
    let predicate = parse_selection("user = ?")?.predicate.populate([Expr::from(&recipient)])?;
    let Some(pref) = ctx.fetch::<NotificationPrefView>(predicate).await?.into_iter().min_by_key(|p| p.id().to_base64()) else {
        return Ok(true);
    };
    let muted_rooms = pref.muted_rooms()?.into_inner();
    let mentions_only = pref.mentions_only()?;
    Ok(pref_allows(kind, mentions_only, &muted_rooms, room_b64))
}

/// Pure pref policy, factored out for testing:
/// - a muted room suppresses EVERY kind, mentions included;
/// - `mentions_only` suppresses every kind EXCEPT mentions (a no-op today,
///   load-bearing the moment a second kind ships).
fn pref_allows(kind: &str, mentions_only: bool, muted_rooms: &serde_json::Value, room_b64: &str) -> bool {
    if let Some(rooms) = muted_rooms.as_array() {
        if rooms.iter().any(|r| r.as_str() == Some(room_b64)) {
            return false;
        }
    }
    if mentions_only && kind != MENTION_KIND {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const ROOM: &str = "AZk3jW0RvkW8pTGnQxYzRR";

    #[test]
    fn defaults_allow_mentions() {
        assert!(pref_allows(MENTION_KIND, false, &json!([]), ROOM));
        // Non-array junk in muted_rooms must fail open, not panic.
        assert!(pref_allows(MENTION_KIND, false, &json!(null), ROOM));
        assert!(pref_allows(MENTION_KIND, false, &json!("garbage"), ROOM));
    }

    #[test]
    fn muted_room_suppresses_even_mentions() {
        assert!(!pref_allows(MENTION_KIND, false, &json!([ROOM]), ROOM));
        assert!(!pref_allows(MENTION_KIND, true, &json!(["other", ROOM]), ROOM));
        // Other rooms stay live.
        assert!(pref_allows(MENTION_KIND, false, &json!(["other"]), ROOM));
    }

    #[test]
    fn mentions_only_gates_other_kinds_but_never_mentions() {
        assert!(pref_allows(MENTION_KIND, true, &json!([]), ROOM));
        // The forward-looking branch: any future kind is suppressed.
        assert!(!pref_allows("room_activity", true, &json!([]), ROOM));
        assert!(pref_allows("room_activity", false, &json!([]), ROOM));
    }
}
