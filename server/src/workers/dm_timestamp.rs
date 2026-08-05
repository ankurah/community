//! Makes a DM's stored send time honest, once, on first sight (#30), and is the
//! stage every other DM consumer is fed from.
//!
//! WHAT THIS IS FOR. `DmMessage.timestamp` is written by whichever client sent
//! the message, so it is a claim about when the message was sent, not a fact.
//! Everything in the DM lane sorts, counts, windows or compares on that one
//! number: the sidebar's activity order, the unread badges, the rate limiter's
//! initiation window, the fan-out's restart probe, and two client queries that
//! order INSIDE the query (`... ORDER BY timestamp DESC` in
//! `leptos-app/src/dm_read_state.rs` and `leptos-app/src/dm_chat.rs`) where no
//! amount of client-side arithmetic can reach the value at all. This worker
//! rewrites a future-dated timestamp to the server's clock and commits it, so
//! all of those read the same honest stored number.
//!
//! WHY CLAMP AND PERSIST RATHER THAN COMPENSATE AT EACH USE. The obvious
//! alternative — every reader computing `min(stored, now)` for itself — was
//! what this lane shipped first, and it produces a value that MOVES between
//! evaluations. A message dated 2100 then evaluates to "the current instant"
//! every time anyone looks at it: its conversation sits at the top of the
//! recipient's sidebar permanently, its unread badge relights after every read
//! (the cursor was pinned at the moment of reading, and the message stamps
//! later than that on the next recompute), the rate limiter re-ages the
//! initiation into the current window on every restart, and the fan-out's
//! restart probe compares a value that moved against a stored `created_at`
//! that did not, minting a duplicate notification per restart. One number that
//! does not move removes all four at the source.
//!
//! THE STANDING TO WRITE A ROW THIS SERVER DID NOT AUTHOR. The workers run on
//! the durable node's Root context (`server/src/main.rs`: `JwtContext::system()`
//! into `workers::start`), and the policy agent returns early for a privileged
//! context before it consults any collection role or scope filter. That is the
//! same standing the rate limiter uses to flip `deleted` on a member's message,
//! and it is per-entity — the policy grammar has no per-property write rules —
//! so `timestamp` is reachable exactly as `deleted` is.
//!
//! ONLY DOWNWARD, AND THEREFORE ONLY ONCE. A timestamp at or before the server
//! clock is left alone; a later one is rewritten to the server clock. A settled
//! row therefore reads at most `now` forever after, so re-seeing it — the boot
//! sweep, an edit, this worker's own write coming back as an Update — writes
//! nothing and cannot loop. A sender who future-dates the same row again is
//! settled again, once per write of theirs. Back-dating is untouched: it is
//! self-defeating (a back-dated message buries itself in the recipient's
//! history) and accepted, per the rate limiter's module doc.
//!
//! WHAT "FIRST SIGHT" IS WORTH, STATED HONESTLY. For a row that arrives while
//! this worker is running, first sight is within a second of the send, so the
//! stored time is very nearly the real one. For rows that were already future-
//! dated before this worker existed, first sight is the boot that finds them,
//! so a batch of them all land at that one instant — there is no earlier
//! honest observation to use, and the rate limiter reads that batch as
//! conversations opened at once. It counts them during the phase where it does
//! not judge, so nothing is tombstoned for it on that boot, and from that boot
//! on the values never move again.
//!
//! EVERY OTHER DM CONSUMER IS DOWNSTREAM OF THIS ONE. `workers::watch_dms` does
//! not fan the DM stream out three ways. It feeds this worker alone, and [`run`]
//! hands each row on to the fan-out and to the rate limiter itself, after it has
//! settled it ([`forward`]). The boot sweep is that same order written out
//! inline: it calls [`settle`] on every backlog row and forwards the backlog
//! only afterwards. So no server-side reader of a DM ever sees a send time that
//! is about to change, on either path.
//!
//! That is a structural guarantee, and it replaces a convergence argument that
//! did not hold. While the three ran in parallel, the fan-out could sample its
//! own clock for a new inbox row's `created_at` before this worker sampled its
//! clock and wrote — storing a notification dated EARLIER than the message it
//! announced. The settling write came back as an Update, but the fan-out
//! recognizes a message it has already delivered and returns before it re-probes
//! (`dm_notify`'s delivered cache), so no stored row was ever rewritten: once
//! the recipient read that notification and the server restarted, the restart
//! probe found neither an unseen row nor a `created_at` at or after the message,
//! and minted one duplicate unread row.
//!
//! TOMBSTONED ROWS ARE SETTLED TOO, AND GO NO FURTHER. The standing query is
//! every `dm_message` row rather than the live ones, because a tombstone does
//! not make a future-dated send time harmless: `leptos-app/src/dm_chat.rs` keeps
//! tombstones in the timeline and orders by `timestamp`, so an unsettled one
//! would sit at the top of the conversation for good — and a sender can still
//! write the timestamp of a row that is already tombstoned, their write scope
//! being unchanged by the flag. [`forward`] then drops them, so what the fan-out
//! and the limiter are handed is exactly what they were handed before: live rows
//! only.

use ankurah::Context;
use anyhow::{Context as _, Result};
use community_model::DmMessageView;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tracing::{info, warn};

use super::dm_rate_limit::Traffic;
use super::now_ms;

/// The pipeline's live stage: settle each row's send time, then hand it to the
/// fan-out and the rate limiter.
///
/// The receiver is borrowed from the supervisor (`workers::supervise`), which
/// respawns this loop if it ever panics; a row missed during that pause is
/// settled and forwarded by the message's next change or the next boot sweep.
/// The two senders are the supervisor's own clones, remade per attempt, so a
/// respawn resumes forwarding rather than talking into a dropped channel.
pub async fn run(
    ctx: Context,
    rx: &mut UnboundedReceiver<DmMessageView>,
    notify_tx: UnboundedSender<DmMessageView>,
    limit_tx: UnboundedSender<Traffic>,
) {
    info!("DM timestamp worker started (a dm_message dated after the server clock is rewritten to it, then passed on)");
    while let Some(msg) = rx.recv().await {
        let message_id = msg.id();
        if let Err(e) = settle(&ctx, &msg).await {
            // Forwarded anyway, on the boot sweep's terms: a row nobody could
            // settle is still traffic, and holding it back would cost the
            // recipient their notification over a failed write.
            warn!(message = %message_id, "DM timestamp clamp failed (retries on the message's next change); passing the row on as it stands: {e:#}");
        }
        forward(&msg, &notify_tx, &limit_tx);
    }
    warn!("DM timestamp worker: message stream closed; exiting");
}

/// Hand one settled row to the fan-out and the rate limiter — unless it is
/// tombstoned, which is where the widened query is narrowed back down.
///
/// A tombstoned DM notifies nobody and is history to the limiter, which is the
/// job the old `deleted = false` predicate did before this worker needed to see
/// tombstones at all (see the module doc). A row whose `deleted` cannot be read
/// is dropped for the same reason it used to be: a predicate excludes rows
/// missing the property it names, so this is the behaviour those two consumers
/// have always had for such a row, not a new judgement about it.
pub(super) fn forward(msg: &DmMessageView, notify_tx: &UnboundedSender<DmMessageView>, limit_tx: &UnboundedSender<Traffic>) {
    match msg.deleted() {
        Ok(false) => {
            // send() fails only at process teardown: the supervisor owns each
            // receiver for the process lifetime.
            let _ = notify_tx.send(msg.clone());
            let _ = limit_tx.send(Traffic::Message(msg.clone()));
        }
        Ok(true) => {}
        Err(e) => warn!(message = %msg.id(), "DM row does not say whether it is tombstoned; not passing it on: {e:#}"),
    }
}

/// Rewrite this message's `timestamp` to the server clock if it claims a time
/// the server has not reached yet. A no-op otherwise, which is the common case
/// and every case once a row has been settled.
pub(super) async fn settle(ctx: &Context, msg: &DmMessageView) -> Result<()> {
    let claimed = msg.timestamp().context("read DM timestamp")?;
    let now = now_ms();
    if claimed <= now {
        return Ok(());
    }
    let trx = ctx.begin();
    msg.edit(&trx)?.timestamp().set(&now)?;
    trx.commit().await.context("commit DM timestamp clamp")?;
    // Ids and times only — never text, never the pair.
    info!(message = %msg.id(), claimed, stored = now, "DM timestamp was later than the server clock; rewritten");
    Ok(())
}
