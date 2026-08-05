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
//! moderator issuing a `Ban`) is the real answer to a persistent sender; this
//! worker is friction, not a wall.
//!
//! WHAT IS COUNTED. Two limits, both per sender, both over a trailing window:
//!
//! 1. **Initiations** — conversations the sender STARTED (their message is the
//!    oldest in its thread) *and started inside the window*. This is the
//!    stranger-DM shape: one person opening many threads at once. Over
//!    [`MAX_INITIATIONS_PER_WINDOW`] in [`WINDOW_MS`], the message that opened
//!    the excess conversation is tombstoned.
//! 2. **Unanswered messages** — messages into threads where the other
//!    participant has never said anything. Over
//!    [`MAX_UNANSWERED_PER_WINDOW`], the excess message is tombstoned. A thread
//!    the correspondent has replied in is a conversation, not a broadcast, and
//!    is not counted at all: two people talking are never rate limited.
//!
//! Neither limit reads message text. The worker never learns what a DM says.
//!
//! AN EVENT IS FILED UNDER ITS OWN TIME, NEVER UNDER THE MOMENT THIS WORKER
//! SAW IT. An initiation is stamped with the timestamp of the thread's OLDEST
//! message — when the conversation actually started — so it leaves the window
//! an hour after that. Stamping it with the message being observed would be a
//! data-destroying mistake, because "the oldest message in this thread is
//! mine" is a permanent property of every thread you ever opened: each reply
//! you sent would restamp it, the window would never age it out, and the map
//! would hold "old threads I have spoken in recently" instead of "threads I
//! started recently". Answering six long-standing correspondents within an
//! hour would then trip a limit that exists to slow six NEW conversations.
//! `answering_six_old_correspondents_within_an_hour_is_never_limited` is that
//! regression, and it is the reason this paragraph exists.
//!
//! WHAT A BREACH COSTS: THE MESSAGE, NEVER THE CONVERSATION. Both verdicts
//! tombstone the single offending message and nothing else — the `DmThread`
//! row and every earlier message in it survive. An earlier design tombstoned
//! the whole thread on an initiation breach, which meant one false positive
//! destroyed a two-way history (including the other participant's messages)
//! with no repair path anywhere in this codebase: nothing ever writes
//! `deleted` back to `false`. Message-only keeps the friction — the spam does
//! not land, and a thread whose only message was tombstoned has nothing left
//! to show, so it does not even appear in the recipient's sidebar (`dm_list`
//! hides threads with no messages) — while making the worst case a lost
//! message rather than a lost conversation.
//!
//! THE BOOT SWEEP ONLY BUILDS THE PICTURE; IT DOES NOT JUDGE. On startup
//! `workers::watch_dms` replays every live `dm_message` row through this
//! worker so the window and the per-thread facts survive a restart. Those
//! replayed rows are counted and never acted on, because the sweep's query has
//! no ORDER BY: a thread's messages arrive in entity-id order, which only
//! approximates send order across clients, so a verdict reached partway
//! through a thread's history would be a verdict on a half-read thread.
//! Enforcement begins when the sweep hands over [`Traffic::BacklogComplete`].
//! The cost is that a burst committed in the seconds before a restart is not
//! tombstoned retroactively — but the facts it left behind still count, so the
//! sender's next message pays for it.
//!
//! WHY THE SENDER IS INFERRED FROM MESSAGES RATHER THAN FROM THE THREAD ROW.
//! `DmThread` records no creator, deliberately: the write scope only checks
//! that the writer is one of `a`/`b`, so a `created_by` field would be
//! unreliable — a sender could name someone else as the creator and get
//! THEM rate limited.
//! `DmMessage.user` is pinned to the caller by the policy's sender-binding rule
//! (`user = $jwt.sub`), so "who started this conversation" derived from the
//! oldest message is the one attribution a client cannot lie about.
//!
//! AND WHY THE PAIR IS READ FROM THE THREAD ROW RATHER THAN FROM THE MESSAGE.
//! The `a`/`b` on a `dm_message` are denormalized copies that exist for one
//! reason: the read scope has to answer "may this user see this row" from the
//! row alone. They are client-written LWW fields, and the write scope checks
//! them only against the writer — so a sender can put any pair they like on
//! any row and file it under any thread id. Nothing here reads them. The
//! conversation a message belongs to is its `thread`, and who that
//! conversation belongs to is read from the thread row (and remembered), so a
//! row filed into someone else's thread by a member it does not name is
//! ignored instead of rewriting that thread's facts — one such row would
//! otherwise make a monologue look answered and switch the unanswered limit
//! off.
//!
//! TIMESTAMPS ARE CLIENT-SUPPLIED, AND THE WINDOW LIVES WITH THAT. A sender
//! could future-date messages to jump the timeline or back-date them to slip
//! out of the window. Future-dating is the move that pays (a message dated
//! next year sits at the top of every "newest first" list forever) and it is
//! neutralized in `dm_timestamp`, a sibling worker that rewrites such a
//! timestamp to the server clock and COMMITS it, so every reader — this one,
//! the fan-out, and the client queries that sort inside the query — gets the
//! same honest number. The local `min(now)` in [`Limiter::observe`] survives
//! only as cover for the commit-wide window before that write lands; on a
//! healed row it does nothing. What it must not become again is the whole
//! defence: a clamp recomputed against the current clock moves every time it
//! is evaluated, and the boot sweep evaluates it on every restart, which
//! collapsed six gradually-opened future-dated threads into one window and
//! tombstoned the sender's next message.
//!
//! Back-dating is self-defeating — a back-dated message buries itself in the
//! recipient's history — so it is accepted rather than defended against. That
//! acceptance now covers one more move: back-dating the first message of a
//! thread ages that thread out of the initiation window early. The sender pays
//! for it in the only currency that matters to them, by burying every one of
//! those openings at the bottom of the recipients' lists.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use ankurah::{Context, EntityId};
use anyhow::{Context as _, Result};
use community_model::{DmMessageView, DmThreadView, ModAction};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use super::now_ms;

/// The trailing window both limits are counted over: one hour.
pub const WINDOW_MS: i64 = 60 * 60 * 1000;

/// How many conversations one sender may START per window. Sized for a human
/// meeting a community, not for a campaign: five introductions an hour is
/// generous for the "I just joined and want to say hi to a few people" case and
/// cheap to exhaust for anyone sending in bulk.
pub const MAX_INITIATIONS_PER_WINDOW: usize = 5;

/// How many messages one sender may send per window into threads the other
/// person has never answered. Comfortably above "hi / are you there / sorry,
/// meant to add —" and far below a broadcast. Replied-to threads are exempt
/// entirely, so an actual conversation never approaches this.
pub const MAX_UNANSWERED_PER_WINDOW: usize = 20;

/// What the limiter decided about one message. Returned rather than acted on
/// inline so the decision is testable without a node.
///
/// Both breaches cost the same thing — the observed message is tombstoned —
/// and differ only in which budget ran out, which is the sentence the audit
/// row carries. Neither ever touches the thread or its earlier messages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Within both limits.
    Allow,
    /// The sender has started too many conversations inside the window: this
    /// message is the one that opened the excess conversation.
    TooManyInitiations { initiations: usize },
    /// The sender is monologuing into too many threads nobody has answered.
    TooManyUnanswered { unanswered: usize },
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

    /// When this conversation started: the timestamp of its oldest message.
    /// The initiation window is measured against THIS, not against whichever
    /// message the worker happens to be looking at (see the module doc).
    fn started_at(&self) -> Option<i64> { self.first.map(|(ts, _)| ts) }

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
    /// sender -> thread -> when that thread's OLDEST message landed, for each
    /// conversation the sender started. Distinct threads are what count, so a
    /// re-observed message (boot sweep after a live delivery) cannot inflate
    /// the tally — and the stamp being the thread's own start is what lets a
    /// long-standing conversation age out of the window while the sender keeps
    /// talking in it.
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
    /// `participants` are the two people named by the THREAD row, and they are
    /// a parameter rather than something read off the message on purpose: a
    /// `DmMessage` carries its own `a`/`b` copies, they are client-written, and
    /// the write scope only checks them against the writer — so a sender can
    /// put any pair they like on a row they file into any thread. A message
    /// from someone the thread does not name is ignored outright rather than
    /// counted, because letting it in would let a stranger (or a second
    /// account) rewrite a conversation's facts: one such row makes a
    /// monologue look answered, which switches the unanswered limit off.
    ///
    /// `now` is the server clock: the right edge of the window, and a floor
    /// under `client_ts` for the one case that floor still has to cover — a
    /// future-dated row seen before `dm_timestamp`'s healing write has landed.
    /// On every healed row the floor is inert, which is the point: the number
    /// this limiter counts by has to be the same number after a restart.
    pub fn observe(
        &mut self,
        message: EntityId,
        thread: EntityId,
        participants: (EntityId, EntityId),
        sender: EntityId,
        client_ts: i64,
        now: i64,
    ) -> Verdict {
        if sender != participants.0 && sender != participants.1 {
            return Verdict::Allow;
        }
        // Inert on a healed row (see the parameter doc); load-bearing only in
        // the window before `dm_timestamp` commits, where without it a single
        // future-dated opener would stamp its initiation past every cutoff and
        // hold one of the sender's five slots for good.
        let ts = client_ts.min(now);

        let facts = self.threads.entry(thread).or_default();
        facts.observe(sender, ts);
        let initiated = facts.initiated_by(sender);
        let monologue = facts.unanswered_by_others(sender);
        // The conversation's own start, which is what the initiation window is
        // measured against. `ts` (this message) would restamp the entry on
        // every reply and it would never age out — see the module doc.
        let started_at = facts.started_at().unwrap_or(ts);

        if initiated {
            self.initiations.entry(sender).or_default().insert(thread, started_at);
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
            return Verdict::TooManyInitiations { initiations };
        }
        let unanswered = self.unanswered.get(&sender).map(HashMap::len).unwrap_or(0);
        if monologue && unanswered > MAX_UNANSWERED_PER_WINDOW {
            return Verdict::TooManyUnanswered { unanswered };
        }
        Verdict::Allow
    }

    /// Whether this sender still needs a `ModAction` row this window. A pure
    /// question: the caller marks them logged with [`Limiter::mark_logged`]
    /// only once the row has actually committed.
    fn needs_audit_row(&self, sender: EntityId) -> bool { !self.logged.contains_key(&sender) }

    /// Record that this sender's audit row for the window exists, so the rest
    /// of a burst produces no further rows.
    fn mark_logged(&mut self, sender: EntityId, now: i64) { self.logged.insert(sender, now); }

    /// A tombstoned message must stop counting toward the unanswered limit:
    /// the row it referred to is gone, so charging the sender's budget for it
    /// would spend a budget on nothing and tombstone messages they were owed.
    fn forget_message(&mut self, message: EntityId, sender: EntityId) {
        if let Some(by_message) = self.unanswered.get_mut(&sender) {
            by_message.remove(&message);
        }
    }

    /// The same, for a message that was also the one that opened its thread:
    /// the conversation it started has nothing left in it, so the thread stops
    /// counting toward the initiation limit too. Without this a sender who
    /// tripped once would stay tripped for the rest of the window and lose
    /// conversations they never got to start.
    ///
    /// The thread's FACTS deliberately stay. They record who spoke in it and
    /// when it started, which is exactly what makes the sender's next attempt
    /// at the same thread trip again — and dropping them would also forget
    /// that the correspondent had ever replied.
    fn forget_initiation(&mut self, message: EntityId, thread: EntityId, sender: EntityId) {
        self.forget_message(message, sender);
        if let Some(by_thread) = self.initiations.get_mut(&sender) {
            by_thread.remove(&thread);
        }
    }
}

/// Everything the worker carries between messages: the counting state, and
/// whether the boot sweep has finished handing over the backlog.
///
/// Owned by the supervisor rather than by [`run`], so a consumer panic loses
/// only the in-flight message. If this lived in `run`'s stack frame, a respawn
/// would restart with an empty window (every old thread would look like a
/// fresh initiation) and with `enforcing` back to false and no second sweep
/// coming to flip it — the limiter would go quiet for the life of the process.
#[derive(Default)]
pub struct State {
    limiter: Limiter,
    /// False until [`Traffic::BacklogComplete`] arrives. Messages seen before
    /// then are counted and never acted on: the sweep delivers a thread's
    /// history in entity-id order, so a verdict mid-history is a verdict on a
    /// thread the worker has only half read.
    enforcing: bool,
    /// thread -> the pair the THREAD row names, read once from storage. The
    /// limiter is told who a conversation belongs to rather than believing the
    /// message's own copy of it (see [`Limiter::observe`]), and this is what
    /// keeps that from costing a query per message during the boot sweep.
    thread_pairs: HashMap<EntityId, (EntityId, EntityId)>,
}

/// What the limiter's channel carries.
pub enum Traffic {
    /// One `dm_message` row — from the live query, or replayed by the boot
    /// sweep. Which one it is does not have to be marked: everything the sweep
    /// sends precedes the marker below.
    Message(DmMessageView),
    /// The boot sweep has handed over its whole backlog. Verdicts are acted on
    /// from here.
    BacklogComplete,
}

/// Consumer loop. The receiver is borrowed from the supervisor
/// (`workers::supervise`), which respawns this loop if it ever panics; `state`
/// is owned by the supervisor for the same reason, so a respawn resumes with
/// the window it had built rather than with a blank one (see [`State`]).
pub async fn run(ctx: Context, state: Arc<Mutex<State>>, rx: &mut UnboundedReceiver<Traffic>) {
    info!(
        window_minutes = WINDOW_MS / 60_000,
        max_initiations = MAX_INITIATIONS_PER_WINDOW,
        max_unanswered = MAX_UNANSWERED_PER_WINDOW,
        "DM rate limiter started (post-hoc: offending messages are tombstoned after they commit)"
    );
    while let Some(traffic) = rx.recv().await {
        let mut state = state.lock().await;
        match traffic {
            Traffic::BacklogComplete => {
                state.enforcing = true;
                info!("DM rate limiter: boot backlog absorbed; live traffic is enforced from here");
            }
            Traffic::Message(msg) => {
                let message_id = msg.id();
                if let Err(e) = process(&ctx, &mut state, &msg).await {
                    warn!(message = %message_id, "DM rate limiting failed (retries on the message's next change): {e:#}");
                }
            }
        }
    }
    warn!("DM rate limiter: message stream closed; exiting");
}

async fn process(ctx: &Context, state: &mut State, msg: &DmMessageView) -> Result<()> {
    // A message already tombstoned (by an earlier pass, or by its sender) is
    // history, not new traffic.
    if msg.deleted().context("read DM deleted flag")? {
        return Ok(());
    }
    let sender = msg.user().context("read DM sender")?.id();
    let thread = msg.thread().context("read DM thread")?.id();
    let client_ts = msg.timestamp().context("read DM timestamp")?;
    let now = now_ms();

    // Who this conversation belongs to comes from the thread row it is filed
    // under. A message whose thread cannot be resolved is filed into a view
    // nobody can open — no thread row means no sidebar entry and no
    // notification — so there is no traffic here to limit.
    let Some(participants) = thread_participants(ctx, state, thread).await else {
        return Ok(());
    };

    let verdict = state.limiter.observe(msg.id(), thread, participants, sender, client_ts, now);
    if !state.enforcing {
        // Boot backlog: the row counted, and that is the whole job. Judging a
        // thread the sweep has only half delivered is what this defers.
        return Ok(());
    }

    match verdict {
        Verdict::Allow => Ok(()),
        Verdict::TooManyInitiations { initiations } => {
            audit(
                ctx,
                &mut state.limiter,
                sender,
                now,
                format!(
                    "Automatic DM rate limit: {initiations} new conversations started within {} minutes (limit {MAX_INITIATIONS_PER_WINDOW}).",
                    WINDOW_MS / 60_000
                ),
            )
            .await?;
            tombstone_message(ctx, msg).await?;
            state.limiter.forget_initiation(msg.id(), thread, sender);
            // Ids and counts only — never text, and never the recipient: the
            // mod log is world-readable, and naming who someone DMs would leak
            // exactly what the DM read scope exists to protect.
            warn!(sender = %sender, thread = %thread, initiations, "DM rate limit: tombstoned the message that opened a conversation over the initiation limit");
            Ok(())
        }
        Verdict::TooManyUnanswered { unanswered } => {
            audit(
                ctx,
                &mut state.limiter,
                sender,
                now,
                format!(
                    "Automatic DM rate limit: {unanswered} messages within {} minutes into conversations nobody answered (limit {MAX_UNANSWERED_PER_WINDOW}).",
                    WINDOW_MS / 60_000
                ),
            )
            .await?;
            tombstone_message(ctx, msg).await?;
            state.limiter.forget_message(msg.id(), sender);
            warn!(sender = %sender, thread = %thread, unanswered, "DM rate limit: tombstoned a message over the unanswered limit");
            Ok(())
        }
    }
}

/// The pair named by a thread row, read once per thread and remembered.
///
/// `None` means the row is not there or does not carry both participants —
/// which is not an error worth retrying: a message can name any thread id its
/// sender likes, including one no row was ever created for, and such a message
/// is invisible to everyone (no thread row, no sidebar entry, and `dm_notify`
/// tells nobody about it).
async fn thread_participants(ctx: &Context, state: &mut State, thread: EntityId) -> Option<(EntityId, EntityId)> {
    if let Some(pair) = state.thread_pairs.get(&thread) {
        return Some(*pair);
    }
    let view = match ctx.get::<DmThreadView>(thread).await {
        Ok(view) => view,
        Err(e) => {
            debug!(thread = %thread, "DM rate limit: no thread row for this message, so nothing to count: {e:#}");
            return None;
        }
    };
    let (Ok(a), Ok(b)) = (view.a(), view.b()) else {
        debug!(thread = %thread, "DM rate limit: thread row names no participants, so nothing to count");
        return None;
    };
    let pair = (a.id(), b.id());
    state.thread_pairs.insert(thread, pair);
    Some(pair)
}

/// Write this sender's one public row for the window, unless they already have
/// one, and mark them logged only once it has committed.
///
/// The order is load-bearing in both directions. The row is written BEFORE the
/// caller tombstones anything, because the whole justification for tombstoning
/// with no human in the loop (docs/moderation.md) is that the community can
/// see it happened — a tombstone that outran its public trace is the thing
/// that argument forbids. And the sender is marked only after the commit
/// succeeds, so a failed write leaves the window unlogged and the offending
/// message un-tombstoned (the caller propagates the error): the sender's next
/// message retries both halves, instead of the window's single row being spent
/// on a write that never landed.
async fn audit(ctx: &Context, limiter: &mut Limiter, sender: EntityId, now: i64, reason: String) -> Result<()> {
    if !limiter.needs_audit_row(sender) {
        return Ok(());
    }
    log_action(ctx, sender, reason).await?;
    limiter.mark_logged(sender, now);
    Ok(())
}

/// Flip `deleted` on the offending message, under Root. The thread row and
/// every other message in it are untouched — the penalty is this message (see
/// the module doc).
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

    /// The pair a thread names, for the many tests where the only participants
    /// that matter are the sender and whoever they are talking to. Written out
    /// rather than defaulted, because "who does this thread belong to" is an
    /// input the limiter is given (see [`Limiter::observe`]) and a test that
    /// hid it would hide the check that reads it.
    fn pair(sender: EntityId, correspondent: EntityId) -> (EntityId, EntityId) { (sender, correspondent) }

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
        let strangers = ids(MAX_INITIATIONS_PER_WINDOW + 1);

        for i in 0..MAX_INITIATIONS_PER_WINDOW {
            let with = pair(sender, strangers[i]);
            assert_eq!(limiter.observe(messages[i], threads[i], with, sender, now, now), Verdict::Allow, "conversation {i} is within the limit");
            // The same message seen twice must not count twice.
            assert_eq!(limiter.observe(messages[i], threads[i], with, sender, now, now), Verdict::Allow, "re-observing message {i} must be inert");
        }

        let last = MAX_INITIATIONS_PER_WINDOW;
        assert_eq!(
            limiter.observe(messages[last], threads[last], pair(sender, strangers[last]), sender, now, now),
            Verdict::TooManyInitiations { initiations: MAX_INITIATIONS_PER_WINDOW + 1 },
            "the message opening the conversation past the limit is tombstoned"
        );
    }

    /// THE regression the initiation window exists to not become: six
    /// long-standing correspondents, every one of them answered within an hour.
    ///
    /// The sender opened all six threads — a month ago — so "the oldest message
    /// in this thread is mine" is true of them forever. What must never be true
    /// is that answering them today reads as starting six conversations today.
    /// Every reply below has to be allowed: when the window was stamped with
    /// the message being observed instead of the thread's own start, the sixth
    /// reply tombstoned a month of two-way conversation (the correspondent's
    /// messages included, with nothing anywhere that writes `deleted` back to
    /// false) and wrote a world-readable row accusing the member of spamming.
    #[test]
    fn answering_six_old_correspondents_within_an_hour_is_never_limited() {
        const DAY_MS: i64 = 24 * 60 * 60 * 1000;

        let mut limiter = Limiter::default();
        let sender = EntityId::new();
        let now = 1_700_000_000_000;
        let threads = ids(MAX_INITIATIONS_PER_WINDOW + 1);
        let correspondents = ids(MAX_INITIATIONS_PER_WINDOW + 1);

        // A month of real two-way conversation: the sender opened each thread
        // on a different day, and each correspondent answered.
        for (i, thread) in threads.iter().enumerate() {
            let with = pair(sender, correspondents[i]);
            let opened = now - (30 - i as i64) * DAY_MS;
            assert_eq!(
                limiter.observe(EntityId::new(), *thread, with, sender, opened, opened),
                Verdict::Allow,
                "opening conversation {i}, weeks apart, is within the limit"
            );
            let answered = opened + 60_000;
            assert_eq!(
                limiter.observe(EntityId::new(), *thread, with, correspondents[i], answered, answered),
                Verdict::Allow,
                "correspondent {i} answering"
            );
        }

        // Today, inside one hour, the sender answers all six.
        for (i, thread) in threads.iter().enumerate() {
            let ts = now + i as i64 * 10 * 60_000;
            assert_eq!(
                limiter.observe(EntityId::new(), *thread, pair(sender, correspondents[i]), sender, ts, ts),
                Verdict::Allow,
                "reply {i} into a conversation started a month ago must not count as starting one"
            );
        }
    }

    /// The other half of that rule, so the fix cannot be an over-correction:
    /// the stamp is the thread's FIRST message, so a conversation opened INSIDE
    /// the window keeps counting until the window passes it. A bulk sender must
    /// not be able to age their own burst out by chatting into the threads they
    /// just opened.
    #[test]
    fn conversations_started_inside_the_window_keep_counting_until_it_passes() {
        let mut limiter = Limiter::default();
        let sender = EntityId::new();
        let start = 1_700_000_000_000;
        let threads = ids(MAX_INITIATIONS_PER_WINDOW + 2);
        let strangers = ids(MAX_INITIATIONS_PER_WINDOW + 2);

        for (i, thread) in threads.iter().take(MAX_INITIATIONS_PER_WINDOW).enumerate() {
            assert_eq!(
                limiter.observe(EntityId::new(), *thread, pair(sender, strangers[i]), sender, start, start),
                Verdict::Allow,
                "opening conversation {i}"
            );
        }

        // Half an hour of chatter into those same threads. None of it re-dates
        // when they were opened.
        let half = start + WINDOW_MS / 2;
        for (i, thread) in threads.iter().take(MAX_INITIATIONS_PER_WINDOW).enumerate() {
            let ts = half + i as i64;
            assert_eq!(
                limiter.observe(EntityId::new(), *thread, pair(sender, strangers[i]), sender, ts, ts),
                Verdict::Allow,
                "chatter in conversation {i}"
            );
        }

        // A sixth conversation, with the first five still inside the window.
        let sixth = MAX_INITIATIONS_PER_WINDOW;
        assert_eq!(
            limiter.observe(EntityId::new(), threads[sixth], pair(sender, strangers[sixth]), sender, half + 1000, half + 1000),
            Verdict::TooManyInitiations { initiations: MAX_INITIATIONS_PER_WINDOW + 1 },
            "the five openings are still inside the window, so the sixth is over the limit"
        );

        // Once the window has moved past those five, opening one is fine again.
        let later = start + WINDOW_MS + 60_000;
        let seventh = MAX_INITIATIONS_PER_WINDOW + 1;
        assert_eq!(
            limiter.observe(EntityId::new(), threads[seventh], pair(sender, strangers[seventh]), sender, later, later),
            Verdict::Allow,
            "an aged-out window frees the sender"
        );
    }

    /// A tombstoned message stops counting toward BOTH limits. The initiation
    /// half is what lets the sender open a conversation again at all; the
    /// unanswered half is what stops messages that no longer exist from
    /// quietly eating the budget of the ones that do.
    #[test]
    fn a_tombstoned_message_stops_spending_both_budgets() {
        let mut limiter = Limiter::default();
        let sender = EntityId::new();
        let now = 1_700_000_000_000;
        let allowed = ids(MAX_INITIATIONS_PER_WINDOW);
        let strangers = ids(MAX_INITIATIONS_PER_WINDOW);

        // Five conversations, one message each: the whole initiation budget,
        // and five of the unanswered budget.
        for (i, thread) in allowed.iter().enumerate() {
            assert_eq!(
                limiter.observe(EntityId::new(), *thread, pair(sender, strangers[i]), sender, now, now),
                Verdict::Allow,
                "opening conversation {i}"
            );
        }

        // Ten further attempts, each tombstoned on arrival and forgotten
        // exactly as `process` forgets it.
        for i in 0..10 {
            let (message, thread) = (EntityId::new(), EntityId::new());
            let ts = now + i;
            assert!(
                matches!(
                    limiter.observe(message, thread, pair(sender, EntityId::new()), sender, ts, ts),
                    Verdict::TooManyInitiations { .. }
                ),
                "attempt {i} past the initiation limit"
            );
            limiter.forget_initiation(message, thread, sender);
        }

        // The unanswered budget was charged for the five messages that survived
        // and for none of the ten that did not, so exactly the rest of it is
        // available in the conversations the sender kept.
        for i in MAX_INITIATIONS_PER_WINDOW..MAX_UNANSWERED_PER_WINDOW {
            let ts = now + 1000 + i as i64;
            let which = i % allowed.len();
            assert_eq!(
                limiter.observe(EntityId::new(), allowed[which], pair(sender, strangers[which]), sender, ts, ts),
                Verdict::Allow,
                "unanswered message {i} is inside the budget"
            );
        }
        let ts = now + 2000;
        assert_eq!(
            limiter.observe(EntityId::new(), allowed[0], pair(sender, strangers[0]), sender, ts, ts),
            Verdict::TooManyUnanswered { unanswered: MAX_UNANSWERED_PER_WINDOW + 1 },
            "the budget runs out at the limit, not ten tombstoned messages early"
        );
    }

    /// A row from someone the THREAD does not name never joins that thread's
    /// facts. The `a`/`b` on a message are the sender's own claim, so anyone
    /// can file a row into anyone's conversation; if the limiter believed such
    /// a row, one such message (from a second account or a cooperating member)
    /// would make a monologue look answered and switch the unanswered limit
    /// off for the thread it was aimed at.
    #[test]
    fn a_row_from_someone_the_thread_does_not_name_never_joins_its_facts() {
        let mut limiter = Limiter::default();
        let (bob, carol, alice) = (EntityId::new(), EntityId::new(), EntityId::new());
        let thread = EntityId::new();
        let theirs = pair(bob, carol);
        let now = 1_700_000_000_000;

        // Bob monologues at Carol, right up to the cap.
        for i in 0..MAX_UNANSWERED_PER_WINDOW {
            let ts = now + i as i64;
            assert_eq!(limiter.observe(EntityId::new(), thread, theirs, bob, ts, ts), Verdict::Allow, "monologue {i}");
        }

        // Alice, who is in neither seat, files a row into their thread.
        let outsider = now + MAX_UNANSWERED_PER_WINDOW as i64;
        assert_eq!(
            limiter.observe(EntityId::new(), thread, theirs, alice, outsider, outsider),
            Verdict::Allow,
            "an outsider's row is not traffic in this conversation; it is ignored"
        );

        // Carol still has not answered, so Bob's next message is still a
        // monologue and still over the cap.
        assert_eq!(
            limiter.observe(EntityId::new(), thread, theirs, bob, outsider + 1, outsider + 1),
            Verdict::TooManyUnanswered { unanswered: MAX_UNANSWERED_PER_WINDOW + 1 },
            "an outsider's row must not count as the correspondent answering"
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
        let strangers = ids(MAX_INITIATIONS_PER_WINDOW + 1);

        for i in 0..MAX_INITIATIONS_PER_WINDOW {
            assert_eq!(limiter.observe(messages[i], threads[i], pair(sender, strangers[i]), sender, start, start), Verdict::Allow);
        }
        // One window and a minute later, every earlier initiation has aged out.
        let later = start + WINDOW_MS + 60_000;
        let last = MAX_INITIATIONS_PER_WINDOW;
        assert_eq!(
            limiter.observe(messages[last], threads[last], pair(sender, strangers[last]), sender, later, later),
            Verdict::Allow,
            "an aged-out window frees the sender"
        );
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

        let with = pair(sender, partner);

        // Sender opens it, partner answers.
        assert_eq!(limiter.observe(EntityId::new(), thread, with, sender, now, now), Verdict::Allow);
        assert_eq!(limiter.observe(EntityId::new(), thread, with, partner, now + 1, now + 1), Verdict::Allow);

        // Now the sender can talk as much as they like in THIS thread.
        for i in 0..(MAX_UNANSWERED_PER_WINDOW * 3) {
            let ts = now + 2 + i as i64;
            assert_eq!(limiter.observe(EntityId::new(), thread, with, sender, ts, ts), Verdict::Allow, "message {i} into an answered thread");
        }
    }

    /// Monologuing is capped even inside the conversations the sender was
    /// allowed to start: three threads, nobody answering, and the sender keeps
    /// typing. The initiation limit is deliberately not in play here (three is
    /// under it), so what trips is the second limit and the verdict is a
    /// message tombstone rather than a thread one.
    #[test]
    fn unanswered_messages_are_capped_within_the_threads_a_sender_may_start() {
        let mut limiter = Limiter::default();
        let sender = EntityId::new();
        let now = 1_000_000_000;
        let threads = ids(3);
        let strangers = ids(3);

        let mut sent = 0usize;
        let mut tombstoned = None;
        // Round-robin across the three threads until the cap bites.
        for i in 0..(MAX_UNANSWERED_PER_WINDOW * 2) {
            let which = i % threads.len();
            let ts = now + i as i64;
            match limiter.observe(EntityId::new(), threads[which], pair(sender, strangers[which]), sender, ts, ts) {
                Verdict::Allow => sent += 1,
                Verdict::TooManyUnanswered { unanswered } => {
                    tombstoned = Some((i, unanswered));
                    break;
                }
                Verdict::TooManyInitiations { initiations } => {
                    panic!("three threads must not trip the initiation limit, got {initiations}")
                }
            }
        }

        let (index, unanswered) = tombstoned.expect("a sender monologuing past the cap must be tombstoned");
        assert_eq!(sent, MAX_UNANSWERED_PER_WINDOW, "everything up to the cap is allowed through");
        assert_eq!(index, MAX_UNANSWERED_PER_WINDOW, "the FIRST message past the cap is the one tombstoned");
        assert_eq!(unanswered, MAX_UNANSWERED_PER_WINDOW + 1);
    }

    /// The same monologue, but the correspondent replies partway through: the
    /// answered thread stops counting, so the sender's budget lasts longer.
    /// This is the exemption that keeps a real conversation out of the
    /// limiter's way.
    #[test]
    fn a_reply_partway_through_frees_that_threads_messages_from_the_cap() {
        let mut limiter = Limiter::default();
        let sender = EntityId::new();
        let partner = EntityId::new();
        let thread = EntityId::new();
        let now = 1_000_000_000;

        let with = pair(sender, partner);

        // The sender opens it and monologues right up to the cap.
        for i in 0..MAX_UNANSWERED_PER_WINDOW {
            let ts = now + i as i64;
            assert_eq!(limiter.observe(EntityId::new(), thread, with, sender, ts, ts), Verdict::Allow);
        }
        // The partner answers. From here the thread is a conversation.
        let reply_ts = now + MAX_UNANSWERED_PER_WINDOW as i64;
        assert_eq!(limiter.observe(EntityId::new(), thread, with, partner, reply_ts, reply_ts), Verdict::Allow);

        // The sender can now keep talking without limit in this thread.
        for i in 0..(MAX_UNANSWERED_PER_WINDOW * 2) {
            let ts = reply_ts + 1 + i as i64;
            assert_eq!(
                limiter.observe(EntityId::new(), thread, with, sender, ts, ts),
                Verdict::Allow,
                "message {i} after a reply must not be limited"
            );
        }
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
        let strangers = ids(MAX_INITIATIONS_PER_WINDOW + 1);
        for (i, thread) in threads.iter().enumerate().take(MAX_INITIATIONS_PER_WINDOW) {
            assert_eq!(
                limiter.observe(EntityId::new(), *thread, pair(sender, strangers[i]), sender, far_future, now),
                Verdict::Allow,
                "conversation {i}"
            );
        }
        let last = MAX_INITIATIONS_PER_WINDOW;
        assert!(
            matches!(
                limiter.observe(EntityId::new(), threads[last], pair(sender, strangers[last]), sender, far_future, now),
                Verdict::TooManyInitiations { .. }
            ),
            "future-dating every message must not spread them across windows"
        );
    }

    /// One audit row per sender per window, however many messages get
    /// tombstoned — and the sender is marked only when the row has actually
    /// been written, so a failed write leaves the window owed a row rather
    /// than silently spending it.
    #[test]
    fn the_audit_row_is_owed_until_it_is_written_then_once_per_sender_per_window() {
        let mut limiter = Limiter::default();
        let sender = EntityId::new();
        let now = 1_000_000_000;

        assert!(limiter.needs_audit_row(sender), "the first breach is owed a row");
        assert!(limiter.needs_audit_row(sender), "asking is not writing: an unwritten row is still owed, so a failed commit retries");

        limiter.mark_logged(sender, now);
        assert!(!limiter.needs_audit_row(sender), "a burst does not produce a row per message");

        // A different sender is independent.
        assert!(limiter.needs_audit_row(EntityId::new()));
    }

    /// Two senders do not share a budget.
    #[test]
    fn limits_are_per_sender() {
        let mut limiter = Limiter::default();
        let noisy = EntityId::new();
        let quiet = EntityId::new();
        let now = 1_000_000_000;

        for _ in 0..(MAX_INITIATIONS_PER_WINDOW + 2) {
            let _ = limiter.observe(EntityId::new(), EntityId::new(), pair(noisy, EntityId::new()), noisy, now, now);
        }
        assert_eq!(
            limiter.observe(EntityId::new(), EntityId::new(), pair(quiet, EntityId::new()), quiet, now, now),
            Verdict::Allow,
            "one sender's burst must not spend another's budget"
        );
    }
}
