//! Makes a DM's stored send time honest, once, on first sight (#30).
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
//! clock is left alone; a later one is rewritten to the server clock. A healed
//! row therefore reads at most `now` forever after, so re-seeing it — the boot
//! sweep, an edit, this worker's own write coming back as an Update — writes
//! nothing and cannot loop. A sender who future-dates the same row again is
//! healed again, once per write of theirs. Back-dating is untouched: it is
//! self-defeating (a back-dated message buries itself in the recipient's
//! history) and accepted, per the rate limiter's module doc.
//!
//! WHAT "FIRST SIGHT" IS WORTH, STATED HONESTLY. For a row that arrives while
//! this worker is running, first sight is within a second of the send, so the
//! stored time is very nearly the real one. For rows that were already future-
//! dated before this worker existed, first sight is the boot that finds them,
//! so a batch of them all land at that one instant — there is no earlier
//! honest observation to use. From that boot on, the value never moves again.
//!
//! THE TRANSIENT, AND WHY IT IS ACCEPTABLE. This worker is one consumer of the
//! shared DM stream, so the rate limiter, the fan-out and a recipient's open
//! client can all see a row before the healing write lands. The window is one
//! commit wide and every consumer converges when the write comes back as an
//! Update: the limiter takes a running minimum of the timestamps it sees, the
//! fan-out re-probes, and a client re-renders. The limiter additionally keeps
//! its own local `min(now)` for that window, which is inert on a healed row —
//! see [`super::dm_rate_limit::Limiter::observe`].

use ankurah::Context;
use anyhow::{Context as _, Result};
use community_model::DmMessageView;
use tokio::sync::mpsc::UnboundedReceiver;
use tracing::{info, warn};

use super::now_ms;

/// Consumer loop. The receiver is borrowed from the supervisor
/// (`workers::supervise`), which respawns this loop if it ever panics; a row
/// missed during that pause is healed by the message's next change or the next
/// boot sweep.
pub async fn run(ctx: Context, rx: &mut UnboundedReceiver<DmMessageView>) {
    info!("DM timestamp worker started (a dm_message dated after the server clock is rewritten to it)");
    while let Some(msg) = rx.recv().await {
        let message_id = msg.id();
        if let Err(e) = clamp_to_server_clock(&ctx, &msg).await {
            warn!(message = %message_id, "DM timestamp clamp failed (retries on the message's next change): {e:#}");
        }
    }
    warn!("DM timestamp worker: message stream closed; exiting");
}

/// Rewrite this message's `timestamp` to the server clock if it claims a time
/// the server has not reached yet. A no-op otherwise, which is the common case
/// and every case once a row has been healed.
async fn clamp_to_server_clock(ctx: &Context, msg: &DmMessageView) -> Result<()> {
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
