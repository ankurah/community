//! DM fan-out: a `DmMessage` becomes ONE `Notification { kind: "dm" }` for the
//! other participant (#30).
//!
//! Consumes `DmMessageView`s from the standing DM LiveQuery (see
//! `workers::start`) and creates the recipient's inbox row under the privileged
//! Root context — the only path that can create rows for another user (the
//! notification write scope pins client writes to `recipient = $jwt.sub`).
//!
//! THE RULE THAT MAKES THIS A SEPARATE WORKER RATHER THAN A BRANCH IN
//! `mentions.rs`: **DM text is never scanned for mentions.** A third party
//! named inside a private thread cannot read that thread — the `dm_message`
//! read scope names exactly two people — so notifying them would tell them a
//! conversation they have no access to is talking about them, and the inbox row
//! would deep-link them to a thread that renders as empty. Room mentions and DM
//! delivery are different products of the same text and must not share a code
//! path where one could grow into the other by accident. No function states
//! the rule, because nothing here has to: `workers::watch_dms` gives the DM
//! stream its own query and its own channel, so DM text never reaches
//! `mentions::run`, and no line in this file reads `DmMessage.text` at all.
//! `a_dm_mentioning_a_third_party_notifies_only_the_recipient` pins the result.
//!
//! Invariants (the mention worker's, restated for this kind):
//! - Idempotent: at most one `kind="dm"` notification per (recipient, message),
//!   enforced by an existence probe before each create — safe under the boot
//!   backlog sweep, edit-driven re-deliveries, and crash/restart replays.
//! - Resilient: a failure on one message is logged and never kills the loop.
//! - Pref-aware: the recipient's `NotificationPref` is consulted through the
//!   same [`super::mentions::pref_allows`] policy, so `mentions_only` suppresses
//!   DM notifications exactly as it suppresses every non-mention kind. Room
//!   mutes cannot apply — a DM happens in no room.

use std::collections::HashSet;

use ankurah::ankql::{ast::Expr, parser::parse_selection};
use ankurah::{Context, EntityId};
use anyhow::{Context as _, Result};
use community_model::{dm_partner, DmMessageView, Notification, NotificationView};
use tokio::sync::mpsc::UnboundedReceiver;
use tracing::{debug, info, warn};

use super::{mentions::pref_allows_delivery, now_ms};

/// The notification kind this worker emits. Stored verbatim in
/// `Notification.kind`; the client inbox matches on it to render the DM
/// sentence and to deep-link into the thread.
pub const DM_KIND: &str = "dm";

/// Consumer loop: one DM at a time, errors contained per message. The receiver
/// is borrowed from the supervisor (`workers::supervise`), which respawns this
/// loop if it ever panics.
pub async fn run(ctx: Context, rx: &mut UnboundedReceiver<DmMessageView>) {
    info!("DM notification worker started (dm_message rows -> kind=\"dm\" notification rows)");
    // (recipient, message) pairs already delivered, so edit-driven Updates
    // don't re-run storage queries. Purely an optimization: a miss falls back
    // to the existence probe. Unlike the mention worker's cache this is keyed
    // on the pair rather than a token signature, because a DM's recipient
    // cannot change — the participants are immutable.
    let mut delivered: HashSet<(EntityId, EntityId)> = HashSet::new();
    while let Some(msg) = rx.recv().await {
        let message_id = msg.id();
        if let Err(e) = process_dm(&ctx, &msg, &mut delivered).await {
            warn!(message = %message_id, "DM fan-out failed (retries on the message's next change): {e:#}");
        }
    }
    warn!("DM notification worker: message stream closed; exiting");
}

/// Bound on the handled-cache, matching the mention worker's `remember`:
/// eviction is a wholesale clear, because the cache is an optimization over an
/// idempotent storage probe and correctness never depends on what it remembers.
const MAX_CACHE_ENTRIES: usize = 8192;

async fn process_dm(ctx: &Context, msg: &DmMessageView, delivered: &mut HashSet<(EntityId, EntityId)>) -> Result<()> {
    let sender = msg.user().context("read DM sender")?.id();
    let a = msg.a().context("read DM participant a")?.id();
    let b = msg.b().context("read DM participant b")?.id();

    // The recipient is the OTHER participant — resolved from the row's own
    // pair, never from the text. `None` means the sender is not one of the two
    // (impossible through the write scope) or the thread is degenerate
    // (self-DM): either way there is nobody else to tell.
    let Some(recipient) = dm_partner(a, b, sender) else {
        debug!(message = %msg.id(), "DM has no other participant to notify");
        return Ok(());
    };

    if delivered.contains(&(recipient, msg.id())) {
        return Ok(());
    }
    // PRODUCT DECISION, MARKED BECAUSE IT IS ONE. A recipient with
    // `mentions_only` set gets NO DM notifications at all: the pref's stated
    // contract is "suppress every kind EXCEPT mentions" (see the
    // `NotificationPref` model doc and `mentions::pref_allows`), and `dm` is a
    // kind. That is the literal reading, and it is the quiet-by-default one —
    // but a user who checked "only notify me when I'm mentioned" plausibly
    // meant "when someone is actually talking to me", which is precisely what
    // a DM is. Nothing here bypasses the pref; if the ruling goes the other
    // way the change is to exempt DM_KIND in `pref_allows`, alongside the
    // preferences copy, which today still reads "Skip everything except direct
    // @mentions" and says nothing about DMs (deliberate: DMs ship dark behind
    // community#68, and the prefs panel is not the place to announce them).
    //
    // The room-mute leg cannot apply — a DM happens in no room — so the empty
    // room id below is inert rather than a placeholder standing in for
    // something.
    if !pref_allows_delivery(ctx, recipient, DM_KIND, "").await? {
        debug!(message = %msg.id(), "DM notification suppressed by recipient's notification prefs");
        return Ok(());
    }
    if dm_notification_exists(ctx, recipient, sender).await? {
        remember(delivered, recipient, msg.id());
        return Ok(());
    }

    let trx = ctx.begin();
    trx.create(&Notification {
        recipient: recipient.into(),
        kind: DM_KIND.to_string(),
        // `Notification.message` is a `Ref<Message>` — a ROOM message — so a
        // DM message cannot ride in it. The thread is what the client needs to
        // deep-link anyway, and it travels in `room`… no: `room` is a
        // `Ref<Room>`. Neither typed slot fits a DM, so both stay `None` and
        // the deep-link target is carried by `actor`: clicking a kind="dm"
        // notification opens the thread with that person, which the client
        // resolves through the same find-or-create the "Message" button uses.
        message: None,
        actor: Some(sender.into()),
        room: None,
        created_at: now_ms(),
        seen: false,
    })
    .await
    .context("create DM notification")?;
    trx.commit().await.context("commit DM notification")?;

    // Ids only — never DM text, and never the message body in any form.
    info!(recipient = %recipient, message = %msg.id(), "DM notification created");
    remember(delivered, recipient, msg.id());
    Ok(())
}

fn remember(cache: &mut HashSet<(EntityId, EntityId)>, recipient: EntityId, message: EntityId) {
    if cache.len() >= MAX_CACHE_ENTRIES {
        cache.clear();
    }
    cache.insert((recipient, message));
}

/// Idempotency probe: does this recipient already have an UNSEEN DM
/// notification from this sender? `Notification` has no typed slot for a DM
/// message id (see the create above), so the probe cannot key on the message —
/// it keys on the (recipient, actor) pair, filtered to unseen rows.
///
/// The consequence, stated plainly: a recipient gets ONE unseen "X sent you a
/// message" row per correspondent, not one per message. A second DM from the
/// same person while the first is still unread adds no second row; once the
/// recipient marks it seen, the next DM mints a fresh one. That is what a DM
/// inbox wants — per-message rows would turn one conversation into inbox spam
/// — but it IS a semantic difference from mentions, where every message that
/// names you earns its own row. It is also what keeps the boot sweep cheap: a
/// thread with 500 backlogged messages produces at most one row per restart.
///
/// Equality-only on `actor`, per the `ModAction.message` note: it is an
/// `Option` field and rows lacking the property are excluded per-row.
async fn dm_notification_exists(ctx: &Context, recipient: EntityId, sender: EntityId) -> Result<bool> {
    let predicate = parse_selection("recipient = ? AND actor = ? AND seen = false")?
        .predicate
        .populate([Expr::from(&recipient), Expr::from(&sender)])?;
    let existing = ctx.fetch::<NotificationView>(predicate).await?;
    for n in existing {
        if n.kind()? == DM_KIND {
            return Ok(true);
        }
    }
    Ok(false)
}
