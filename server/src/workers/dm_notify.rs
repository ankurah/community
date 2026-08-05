//! DM fan-out: a `DmMessage` becomes ONE `Notification { kind: "dm" }` for the
//! other participant (#30).
//!
//! Consumes `DmMessageView`s handed over by `dm_timestamp`, the stage upstream
//! of this one (see `workers::watch_dms`), and creates the recipient's inbox row
//! under the privileged Root context — the only path that can create rows for
//! another user (the notification write scope pins client writes to
//! `recipient = $jwt.sub`).
//!
//! Being downstream is load-bearing, not incidental. Every row that arrives here
//! has had its send time settled already, so the `created_at` this worker stamps
//! on an inbox row is never earlier than the `timestamp` the message ends up
//! with — which is precisely what [`dm_notification_exists`] compares on the
//! next restart. Nothing in this file would repair that ordering if it were
//! wrong: the delivered cache below answers for a message already handled and
//! returns before the probe runs at all.
//!
//! "Every row" holds with no exception for failure. When a settling write fails,
//! the stage upstream keeps that row from this worker entirely and lets it
//! arrive on its next change or on the next boot sweep instead. So what a
//! failure costs here is a notification that comes late — never one stamped
//! against a send time that then moved under it.
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
//! - Idempotent: at most one unread `kind="dm"` row per (recipient, sender) at
//!   a time, and never a second row for a message already delivered — enforced
//!   by an existence probe before each create, which is what makes the boot
//!   backlog sweep, edit-driven re-deliveries and restart replays safe. See
//!   [`dm_notification_exists`] for why both halves of that are needed.
//! - Resilient: a failure on one message is logged and never kills the loop.
//! - Pref-aware: the recipient's `NotificationPref` is consulted through the
//!   same [`super::mentions::pref_allows`] policy, so `mentions_only` suppresses
//!   DM notifications exactly as it suppresses every non-mention kind. Room
//!   mutes cannot apply — a DM happens in no room.

use std::collections::HashSet;

use ankurah::ankql::{ast::Expr, parser::parse_selection};
use ankurah::error::RetrievalError;
use ankurah::{Context, EntityId};
use anyhow::{Context as _, Result};
use community_model::{dm_partner, DmMessageView, DmThreadView, Notification, NotificationView};
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
    // don't re-run storage queries. A miss falls back to the existence probe; a
    // HIT returns without probing at all, which is why the row this worker
    // writes has to be right the first time — see the module doc, and the stage
    // upstream that settles the send time before handing the row over. Keyed on
    // the pair rather than on a token signature (the mention worker's shape)
    // because there are no tokens here to sign — a DM's recipient is whoever its
    // thread names, and this cache only has to recognize a message it already
    // handled for that person.
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
    let thread_id = msg.thread().context("read DM thread")?.id();

    // THE RECIPIENT COMES FROM THE THREAD ROW, NEVER FROM THE MESSAGE'S OWN
    // `a`/`b`. Those two fields are denormalized copies that exist so the read
    // scope can decide row-locally who may see a message; they are
    // client-written, and the write scope only checks them against the writer.
    // A sender can therefore put ANY pair on a row and file it under any
    // thread. Believing them here would hand every member an unlimited
    // notification channel to strangers: write one message into your own
    // long-answered thread, name a stranger in `a`/`b`, and this worker taps
    // them on the shoulder about a conversation they cannot open — then edit
    // the same row for the next stranger, forever, with the rate limiter
    // seeing one quiet old thread the whole time.
    //
    // Read from the thread and the claim buys nothing: the row is visible
    // only to whoever the sender named, and nobody is told about it.
    let Some((a, b)) = thread_participants(ctx, thread_id).await else {
        debug!(message = %msg.id(), thread = %thread_id, "DM names no thread we can resolve; nobody to notify");
        return Ok(());
    };

    // The recipient is the OTHER participant. `None` means the sender is not
    // in this thread at all (a row filed into someone else's conversation) or
    // the thread is degenerate (self-DM): either way there is nobody to tell.
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
    // Read as stored, deliberately not compensated here. `dm_timestamp` settled
    // this row before it forwarded it, so this number is honest, it is at or
    // below the `created_at` written a few lines down, and — the part the probe
    // rests on — it is the SAME number on the next restart. Taking `min(now)`
    // here instead is what made a year-2100 message re-clamp to a later instant
    // on every boot, miss the probe below, and mint a fresh unread row each
    // time.
    let sent_at = msg.timestamp().context("read DM timestamp")?;
    if dm_notification_exists(ctx, recipient, sender, sent_at).await? {
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

/// The pair a thread row names — the only trustworthy answer to "who is this
/// conversation between", per the rule at the top of `process_dm`. `None`
/// means no such row, or a row without both participants: a message can name
/// any thread id its sender likes, and one nobody created is one nobody can
/// open, so there is no one to notify.
async fn thread_participants(ctx: &Context, thread: EntityId) -> Option<(EntityId, EntityId)> {
    let view = match ctx.get::<DmThreadView>(thread).await {
        Ok(view) => view,
        // Split the same way the limiter's twin of this function splits it, and
        // for the same reason: no such row is expected traffic and stays quiet,
        // while a storage failure costs the recipient a notification and has to
        // be visible.
        //
        // The split is `mentions::deliver`'s; the DISPOSITION is not, and the
        // difference is worth naming. That function propagates a storage error to
        // its caller and this one returns `None` for both legs, so a caller here
        // cannot tell a message naming no thread from a message whose thread
        // could not be read. Neither shape retries in-process — `mentions::run`
        // logs what it is handed and moves to the next message — so what the
        // return type costs is the log line's precision, which is why the two
        // legs are worded differently, and nothing else.
        Err(RetrievalError::EntityNotFound(_)) | Err(RetrievalError::CollectionNotFound(_)) => {
            debug!(thread = %thread, "DM fan-out: no thread row to resolve participants from");
            return None;
        }
        Err(e) => {
            warn!(thread = %thread, "DM fan-out: could not read this message's thread row, so nobody is notified of it: {e:#}");
            return None;
        }
    };
    let (Ok(a), Ok(b)) = (view.a(), view.b()) else {
        debug!(thread = %thread, "DM fan-out: thread row names no participants");
        return None;
    };
    Some((a.id(), b.id()))
}

/// Idempotency probe: has this DM already been accounted for in the
/// recipient's inbox? `Notification` has no typed slot for a DM message id
/// (see the create above), so the probe cannot key on the message; it keys on
/// the (recipient, actor) pair and answers yes on either of two grounds.
///
/// **An unseen row exists** — the coalescing rule. A recipient gets ONE unseen
/// "X sent you a message" row per correspondent, not one per message: a second
/// DM while the first is unread adds nothing, and once they mark it seen the
/// next DM mints a fresh one. That is what a DM inbox wants — per-message rows
/// would turn one conversation into inbox spam — though it IS a semantic
/// difference from mentions, where every message naming you earns a row.
///
/// **A row exists that is at least as new as this message** — the restart
/// rule, and the reason `seen` alone is not enough. The boot sweep replays
/// every message ever sent, so after the recipient reads a DM and the server
/// restarts, the unseen leg no longer matches and the old message would mint a
/// brand-new unread row — announcing last month's DM again, on every restart.
/// A row created at or after the message's own timestamp is proof that message
/// was already delivered. This leg rests on two things the pipeline gives it.
/// The stored timestamp stands still — `dm_timestamp` settles a future-dated
/// row once and persists it, so the same message answers this probe the same
/// way on every boot. And the row this worker writes really is proof, because
/// the settling write happened before this worker was handed the message: its
/// `created_at` cannot be earlier than the timestamp the message ended up with.
/// While the two ran in parallel that second half was not true, and the
/// notification a recipient had already read stopped answering for its own
/// message after a restart.
///
/// The two legs cannot be collapsed into "any row at all": that would make the
/// FIRST notification from a correspondent the last one they could ever send.
///
/// Equality-only on `actor`, per the `ModAction.message` note: it is an
/// `Option` field and rows lacking the property are excluded per-row. `kind`,
/// `seen` and `created_at` are compared in code so that one fetch answers
/// both legs.
async fn dm_notification_exists(ctx: &Context, recipient: EntityId, sender: EntityId, sent_at: i64) -> Result<bool> {
    let predicate =
        parse_selection("recipient = ? AND actor = ?")?.predicate.populate([Expr::from(&recipient), Expr::from(&sender)])?;
    let existing = ctx.fetch::<NotificationView>(predicate).await?;
    for n in existing {
        if n.kind()? != DM_KIND {
            continue;
        }
        // ACCEPTED, NOT OVERLOOKED: the two sides of this comparison come from
        // different clocks. `created_at` is the server's at delivery; `sent_at`
        // is the sender's own, clamped down by `dm_timestamp` but never raised.
        // A sender whose clock runs a minute behind, replying within a minute
        // of the recipient marking the previous row seen, produces
        // `created_at(previous) >= sent_at(new)` and gets no inbox row for a
        // genuinely new message. It is silent and it costs the inbox row only:
        // the unread badge is derived from the same sender's timestamps and
        // still lights. Closing it honestly needs a typed DM slot on
        // `Notification` so the probe can key on the message the way
        // `mentions.rs` does — see the note at [`dm_notification_exists`] for
        // why no existing slot fits — which is a model change this lane is not
        // taking. A better timestamp heuristic here would not close it.
        if !n.seen()? || n.created_at()? >= sent_at {
            return Ok(true);
        }
    }
    Ok(false)
}
