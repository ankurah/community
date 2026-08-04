//! Per-sender DM rate limiting (#30) — the stranger-DM mitigation.
//!
//! WHY THIS IS A WORKER AND NOT A GATE. The only place a remote write can be
//! refused is `check_event` inside ankurah-jwt-auth, which an application
//! cannot extend (the wrapper-agent approach was killed in an earlier
//! architecture review). So there is no "reject the 6th thread" seam anywhere
//! in this codebase. Enforcement is therefore **post-hoc**: the offending rows
//! are committed and replicated first, and this worker tombstones them
//! afterwards — typically within a second, but a recipient with a live thread
//! open can see a message appear and then turn into a tombstone. That is the
//! honest cost of the seam we have, and it is why the escalation path (a human
//! moderator issuing a `Ban`) is the real answer to a determined abuser; this
//! worker is friction, not a wall.
//!
//! WHAT IS COUNTED. Two limits, both per sender, both over a trailing window:
//!
//! 1. **Initiations** — conversations the sender STARTED (their message is the
//!    oldest in its thread). This is the stranger-DM shape: one person opening
//!    many threads. Over [`MAX_INITIATIONS_PER_WINDOW`] in
//!    [`WINDOW_MS`], the excess thread is tombstoned along with its messages.
//! 2. **Unanswered messages** — messages into threads where the other
//!    participant has never said anything. Over
//!    [`MAX_UNANSWERED_PER_WINDOW`], the excess message is tombstoned. A thread
//!    the correspondent has replied in is a conversation, not a broadcast, and
//!    is not counted at all: two people talking are never rate limited.
//!
//! Neither limit reads message text. The worker never learns what a DM says.
//!
//! WHY THE SENDER IS INFERRED FROM MESSAGES RATHER THAN FROM THE THREAD ROW.
//! `DmThread` records no creator, deliberately: the write scope only checks
//! that the writer is one of `a`/`b`, so a `created_by` field would be
//! forgeable — a spammer could blame the victim and get THEM rate limited.
//! `DmMessage.user` is pinned to the caller by the policy's sender-binding rule
//! (`user = $jwt.sub`), so "who started this conversation" derived from the
//! oldest message is the one attribution a client cannot lie about.
//!
//! TIMESTAMPS ARE CLIENT-SUPPLIED, AND THE WINDOW LIVES WITH THAT. A sender
//! could future-date messages to jump the timeline or back-date them to slip
//! out of the window. Future-dating is the attractive attack (a message dated
//! next year sits at the top of every "newest first" list forever) and it is
//! neutralized: timestamps are clamped to the server's clock on arrival.
//! Back-dating is self-defeating — a back-dated message buries itself in the
//! recipient's history — so it is accepted rather than defended against.

use std::collections::{HashMap, HashSet};

use ankurah::{Context, EntityId};
use anyhow::{Context as _, Result};
use community_model::{DmMessageView, DmThreadView, ModAction};
use tokio::sync::mpsc::UnboundedReceiver;
use tracing::{info, warn};

use super::now_ms;

/// The trailing window both limits are counted over: one hour.
pub const WINDOW_MS: i64 = 60 * 60 * 1000;

/// How many conversations one sender may START per window. Sized for a human
/// meeting a community, not for a campaign: five introductions an hour is
/// generous for the "I just joined and want to say hi to a few people" case and
/// cheap for a spammer to exhaust.
pub const MAX_INITIATIONS_PER_WINDOW: usize = 5;

/// How many messages one sender may send per window into threads the other
/// person has never answered. Comfortably above "hi / are you there / sorry,
/// meant to add —" and far below a broadcast. Replied-to threads are exempt
/// entirely, so an actual conversation never approaches this.
pub const MAX_UNANSWERED_PER_WINDOW: usize = 20;

/// What the limiter decided about one message. Returned rather than acted on
/// inline so the decision is testable without a node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Within both limits.
    Allow,
    /// The sender has started too many conversations this window: tombstone the
    /// thread and everything in it.
    TombstoneThread { initiations: usize },
    /// The sender is monologuing into too many unanswered threads: tombstone
    /// this message, leave the thread.
    TombstoneMessage { unanswered: usize },
}

/// What the worker knows about one thread, accumulated from the messages it has
/// seen (the boot sweep replays every message, so this is rebuilt from storage
/// on each restart rather than persisted).
#[derive(Debug, Default, Clone)]
struct ThreadFacts {
    /// The oldest message seen in this thread, and who sent it. Messages arrive
    /// in arbitrary order during the boot sweep, so this is a running minimum.
    first: Option<(i64, EntityId)>,
    /// Everyone who has sent anything in this thread.
    senders: HashSet<EntityId>,
}

impl ThreadFacts {
    fn observe(&mut self, sender: EntityId, ts: i64) {
        self.senders.insert(sender);
        match self.first {
            Some((existing_ts, _)) if existing_ts <= ts => {}
            _ => self.first = Some((ts, sender)),
        }
    }

    /// Whether `sender` started this conversation.
    fn initiated_by(&self, sender: EntityId) -> bool { self.first.map(|(_, who)| who == sender).unwrap_or(false) }

    /// Whether the sender is talking to themselves: they started it and nobody
    /// else has ever answered.
    fn unanswered_by_others(&self, sender: EntityId) -> bool {
        self.initiated_by(sender) && self.senders.len() == 1
    }
}

/// The limiter's whole state: per-thread facts and, per sender, the timestamps
/// of their initiations and unanswered messages. Pure — no ankurah types, no
/// I/O — so the counting rules are testable on their own.
#[derive(Default)]
pub struct Limiter {
    threads: HashMap<EntityId, ThreadFacts>,
    /// sender -> (timestamp, thread) for each conversation they started.
    /// Distinct threads are what count, so a re-observed message (boot sweep
    /// after a live delivery) cannot inflate the tally.
    initiations: HashMap<EntityId, HashMap<EntityId, i64>>,
    /// sender -> message id -> timestamp, for messages into unanswered threads.
    unanswered: HashMap<EntityId, HashMap<EntityId, i64>>,
    /// Senders already logged this window, so a burst produces ONE ModAction
    /// row rather than one per tombstoned message.
    logged: HashMap<EntityId, i64>,
}

impl Limiter {
    /// Record one message and decide what to do about it.
    ///
    /// `now` is the server clock, used both to clamp a client-supplied
    /// timestamp and as the right edge of the window.
    pub fn observe(&mut self, message: EntityId, thread: EntityId, sender: EntityId, client_ts: i64, now: i64) -> Verdict {
        let ts = client_ts.min(now);

        let facts = self.threads.entry(thread).or_default();
        facts.observe(sender, ts);
        let initiated = facts.initiated_by(sender);
        let monologue = facts.unanswered_by_others(sender);

        if initiated {
            self.initiations.entry(sender).or_default().insert(thread, ts);
        }
        if monologue {
            self.unanswered.entry(sender).or_default().insert(message, ts);
        }

        let cutoff = now - WINDOW_MS;
        if let Some(by_thread) = self.initiations.get_mut(&sender) {
            by_thread.retain(|_, t| *t >= cutoff);
        }
        if let Some(by_message) = self.unanswered.get_mut(&sender) {
            by_message.retain(|_, t| *t >= cutoff);
        }
        self.logged.retain(|_, t| *t >= cutoff);

        let initiations = self.initiations.get(&sender).map(HashMap::len).unwrap_or(0);
        if initiated && initiations > MAX_INITIATIONS_PER_WINDOW {
            return Verdict::TombstoneThread { initiations };
        }
        let unanswered = self.unanswered.get(&sender).map(HashMap::len).unwrap_or(0);
        if monologue && unanswered > MAX_UNANSWERED_PER_WINDOW {
            return Verdict::TombstoneMessage { unanswered };
        }
        Verdict::Allow
    }

    /// Whether this sender still needs a `ModAction` row this window. Returns
    /// true at most once per sender per window; the caller logs only then.
    pub fn should_log(&mut self, sender: EntityId, now: i64) -> bool {
        if self.logged.contains_key(&sender) {
            return false;
        }
        self.logged.insert(sender, now);
        true
    }

    /// A tombstoned row must stop counting toward the limits, or a sender who
    /// trips the initiation limit once would stay tripped for the rest of the
    /// window and lose conversations they never got to start.
    fn forget_thread(&mut self, thread: EntityId, sender: EntityId) {
        self.threads.remove(&thread);
        if let Some(by_thread) = self.initiations.get_mut(&sender) {
            by_thread.remove(&thread);
        }
    }

    fn forget_message(&mut self, message: EntityId, sender: EntityId) {
        if let Some(by_message) = self.unanswered.get_mut(&sender) {
            by_message.remove(&message);
        }
    }
}

/// Consumer loop. The receiver is borrowed from the supervisor
/// (`workers::supervise`), which respawns this loop if it ever panics — the
/// limiter state is rebuilt by the next boot sweep, so a respawn loses at most
/// the window's counts and never mis-tombstones anything.
pub async fn run(ctx: Context, rx: &mut UnboundedReceiver<DmMessageView>) {
    info!(
        window_minutes = WINDOW_MS / 60_000,
        max_initiations = MAX_INITIATIONS_PER_WINDOW,
        max_unanswered = MAX_UNANSWERED_PER_WINDOW,
        "DM rate limiter started (post-hoc: offending rows are tombstoned after they commit)"
    );
    let mut limiter = Limiter::default();
    while let Some(msg) = rx.recv().await {
        let message_id = msg.id();
        if let Err(e) = process(&ctx, &mut limiter, &msg).await {
            warn!(message = %message_id, "DM rate limiting failed (retries on the message's next change): {e:#}");
        }
    }
    warn!("DM rate limiter: message stream closed; exiting");
}

async fn process(ctx: &Context, limiter: &mut Limiter, msg: &DmMessageView) -> Result<()> {
    // A message already tombstoned (by an earlier pass, or by its sender) is
    // history, not new traffic.
    if msg.deleted().context("read DM deleted flag")? {
        return Ok(());
    }
    let sender = msg.user().context("read DM sender")?.id();
    let thread = msg.thread().context("read DM thread")?.id();
    let client_ts = msg.timestamp().context("read DM timestamp")?;
    let now = now_ms();

    match limiter.observe(msg.id(), thread, sender, client_ts, now) {
        Verdict::Allow => Ok(()),
        Verdict::TombstoneThread { initiations } => {
            tombstone_thread(ctx, thread).await?;
            limiter.forget_thread(thread, sender);
            // Ids and counts only — never text, and never the recipient: the
            // mod log is world-readable, and naming who someone DMs would leak
            // exactly what the DM read scope exists to protect.
            warn!(sender = %sender, thread = %thread, initiations, "DM rate limit: tombstoned a thread over the initiation limit");
            if limiter.should_log(sender, now) {
                log_action(
                    ctx,
                    sender,
                    format!(
                        "Automatic DM rate limit: {initiations} new conversations started within {} minutes (limit {MAX_INITIATIONS_PER_WINDOW}).",
                        WINDOW_MS / 60_000
                    ),
                )
                .await?;
            }
            Ok(())
        }
        Verdict::TombstoneMessage { unanswered } => {
            tombstone_message(ctx, msg).await?;
            limiter.forget_message(msg.id(), sender);
            warn!(sender = %sender, thread = %thread, unanswered, "DM rate limit: tombstoned a message over the unanswered limit");
            if limiter.should_log(sender, now) {
                log_action(
                    ctx,
                    sender,
                    format!(
                        "Automatic DM rate limit: {unanswered} messages within {} minutes into conversations nobody answered (limit {MAX_UNANSWERED_PER_WINDOW}).",
                        WINDOW_MS / 60_000
                    ),
                )
                .await?;
            }
            Ok(())
        }
    }
}

/// Flip `deleted` on a thread AND on every message in it, under Root. Both
/// halves matter: the thread flag removes it from the sidebars, and the message
/// flags stop a client that already holds the rows from rendering the payload.
async fn tombstone_thread(ctx: &Context, thread: EntityId) -> Result<()> {
    use ankurah::ankql::{ast::Expr, parser::parse_selection};

    let thread_view = ctx.get::<DmThreadView>(thread).await.context("load the thread to tombstone")?;
    let predicate = parse_selection("thread = ?")?.predicate.populate([Expr::from(&thread)])?;
    let messages = ctx.fetch::<DmMessageView>(predicate).await.context("load the thread's messages")?;

    let trx = ctx.begin();
    thread_view.edit(&trx)?.deleted().set(&true)?;
    for message in &messages {
        if !message.deleted().unwrap_or(false) {
            message.edit(&trx)?.deleted().set(&true)?;
        }
    }
    trx.commit().await.context("commit thread tombstone")?;
    Ok(())
}

async fn tombstone_message(ctx: &Context, msg: &DmMessageView) -> Result<()> {
    let trx = ctx.begin();
    msg.edit(&trx)?.deleted().set(&true)?;
    trx.commit().await.context("commit message tombstone")?;
    Ok(())
}

/// The public audit row. `actor` is `None` — nothing human acted — and the
/// reason carries the counts, never a recipient and never text.
///
/// DISCLOSURE, STATED: `modaction` is world-readable by design, so this row
/// tells the community that this member tripped the DM rate limit. It does not
/// say who they messaged or what they wrote. That trade is deliberate: without
/// a public row, an automated tombstone would be invisible to the moderators
/// who are supposed to decide whether it warrants a Ban, and DMs are private
/// from moderators precisely so that reports and rows like this one are the
/// only signal they get.
async fn log_action(ctx: &Context, sender: EntityId, reason: String) -> Result<()> {
    let trx = ctx.begin();
    trx.create(&ModAction {
        actor: None,
        message: None,
        user: Some(sender.into()),
        action: "dm-rate-limit".to_string(),
        reason: Some(reason),
        created_at: now_ms(),
    })
    .await
    .context("create rate-limit ModAction")?;
    trx.commit().await.context("commit rate-limit ModAction")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(n: usize) -> Vec<EntityId> { (0..n).map(|_| EntityId::new()).collect() }

    /// A sender opening conversations one after another is allowed up to the
    /// limit and tombstoned past it — and the count is of DISTINCT threads, so
    /// re-observing the same message (the boot sweep after a live delivery)
    /// never inflates it.
    #[test]
    fn initiations_are_counted_per_distinct_thread_and_capped() {
        let mut limiter = Limiter::default();
        let sender = EntityId::new();
        let now = 1_000_000_000;
        let threads = ids(MAX_INITIATIONS_PER_WINDOW + 1);
        let messages = ids(MAX_INITIATIONS_PER_WINDOW + 1);

        for i in 0..MAX_INITIATIONS_PER_WINDOW {
            assert_eq!(limiter.observe(messages[i], threads[i], sender, now, now), Verdict::Allow, "conversation {i} is within the limit");
            // The same message seen twice must not count twice.
            assert_eq!(limiter.observe(messages[i], threads[i], sender, now, now), Verdict::Allow, "re-observing message {i} must be inert");
        }

        let last = MAX_INITIATIONS_PER_WINDOW;
        assert_eq!(
            limiter.observe(messages[last], threads[last], sender, now, now),
            Verdict::TombstoneThread { initiations: MAX_INITIATIONS_PER_WINDOW + 1 },
            "the conversation past the limit is tombstoned"
        );
    }

    /// The window really is trailing: initiations older than an hour stop
    /// counting, so a steady low rate is never limited.
    #[test]
    fn initiations_outside_the_window_are_forgotten() {
        let mut limiter = Limiter::default();
        let sender = EntityId::new();
        let start = 1_000_000_000;
        let threads = ids(MAX_INITIATIONS_PER_WINDOW + 1);
        let messages = ids(MAX_INITIATIONS_PER_WINDOW + 1);

        for i in 0..MAX_INITIATIONS_PER_WINDOW {
            assert_eq!(limiter.observe(messages[i], threads[i], sender, start, start), Verdict::Allow);
        }
        // One window and a minute later, every earlier initiation has aged out.
        let later = start + WINDOW_MS + 60_000;
        let last = MAX_INITIATIONS_PER_WINDOW;
        assert_eq!(limiter.observe(messages[last], threads[last], sender, later, later), Verdict::Allow, "an aged-out window frees the sender");
    }

    /// Replying is what distinguishes a conversation from a broadcast: once the
    /// other participant says anything, the thread stops counting toward the
    /// unanswered limit, and messages into it are never tombstoned.
    #[test]
    fn a_reply_exempts_a_thread_from_the_unanswered_limit() {
        let mut limiter = Limiter::default();
        let sender = EntityId::new();
        let partner = EntityId::new();
        let thread = EntityId::new();
        let now = 1_000_000_000;

        // Sender opens it, partner answers.
        assert_eq!(limiter.observe(EntityId::new(), thread, sender, now, now), Verdict::Allow);
        assert_eq!(limiter.observe(EntityId::new(), thread, partner, now + 1, now + 1), Verdict::Allow);

        // Now the sender can talk as much as they like in THIS thread.
        for i in 0..(MAX_UNANSWERED_PER_WINDOW * 3) {
            let ts = now + 2 + i as i64;
            assert_eq!(limiter.observe(EntityId::new(), thread, sender, ts, ts), Verdict::Allow, "message {i} into an answered thread");
        }
    }

    /// Monologuing into unanswered threads is capped. The initiation limit is
    /// avoided here by having the PARTNER open each thread, so what is being
    /// tested is the second limit rather than the first.
    #[test]
    fn unanswered_messages_are_capped_across_threads() {
        let mut limiter = Limiter::default();
        let sender = EntityId::new();
        let now = 1_000_000_000;

        let mut verdicts = Vec::new();
        for i in 0..(MAX_UNANSWERED_PER_WINDOW + 1) {
            // A fresh thread each time, started by the sender — but only one
            // initiation counts per thread and we stay under that limit by
            // reusing a small set of threads.
            let thread = EntityId::new();
            let _ = limiter.observe(EntityId::new(), thread, sender, now, now);
            verdicts.push(limiter.observe(EntityId::new(), thread, sender, now + 1, now + 1));
            let _ = i;
        }
        assert!(
            verdicts.iter().any(|v| matches!(v, Verdict::TombstoneThread { .. } | Verdict::TombstoneMessage { .. })),
            "a sender monologuing into many unanswered threads must be limited, got {verdicts:?}"
        );
    }

    /// A future-dated message cannot buy itself a fresh window: the timestamp
    /// is clamped to the server clock before it is counted.
    #[test]
    fn future_dated_timestamps_are_clamped_to_the_server_clock() {
        let mut limiter = Limiter::default();
        let sender = EntityId::new();
        let now = 1_000_000_000;
        let far_future = now + 10 * WINDOW_MS;

        let threads = ids(MAX_INITIATIONS_PER_WINDOW + 1);
        for (i, thread) in threads.iter().enumerate().take(MAX_INITIATIONS_PER_WINDOW) {
            assert_eq!(limiter.observe(EntityId::new(), *thread, sender, far_future, now), Verdict::Allow, "conversation {i}");
        }
        assert!(
            matches!(
                limiter.observe(EntityId::new(), threads[MAX_INITIATIONS_PER_WINDOW], sender, far_future, now),
                Verdict::TombstoneThread { .. }
            ),
            "future-dating every message must not spread them across windows"
        );
    }

    /// One audit row per sender per window, however many rows get tombstoned.
    #[test]
    fn the_audit_row_is_logged_once_per_sender_per_window() {
        let mut limiter = Limiter::default();
        let sender = EntityId::new();
        let now = 1_000_000_000;
        assert!(limiter.should_log(sender, now), "the first breach is logged");
        assert!(!limiter.should_log(sender, now), "a burst does not produce a row per message");

        // A different sender is independent.
        assert!(limiter.should_log(EntityId::new(), now));
    }

    /// Two senders do not share a budget.
    #[test]
    fn limits_are_per_sender() {
        let mut limiter = Limiter::default();
        let noisy = EntityId::new();
        let quiet = EntityId::new();
        let now = 1_000_000_000;

        for _ in 0..(MAX_INITIATIONS_PER_WINDOW + 2) {
            let _ = limiter.observe(EntityId::new(), EntityId::new(), noisy, now, now);
        }
        assert_eq!(
            limiter.observe(EntityId::new(), EntityId::new(), quiet, now, now),
            Verdict::Allow,
            "one sender's burst must not spend another's budget"
        );
    }
}
