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
//!    [`MAX_INITIATIONS_PER_WINDOW`] in [`WINDOW_MS`], the message being
//!    observed is tombstoned. That is the message that opened the excess
//!    conversation the first time; a later message into a thread already over
//!    the limit trips as itself and is tombstoned as itself, which is the
//!    friction the narrower penalty relies on.
//!
//!    A conversation is counted per PAIR OF MEMBERS, not per thread row. Two
//!    tabs racing on a first DM leave two rows for the same two people, and
//!    the client already treats those as one conversation.
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
//! conversation belongs to is read from the thread row, so a row filed into
//! someone else's thread by a member it does not name is ignored instead of
//! rewriting that thread's facts — one such row would otherwise make a
//! monologue look answered and switch the unanswered limit off.
//!
//! That read happens for every message rather than once per thread, because
//! `DmThread.a`/`b` are themselves mutable and a participant's client can
//! rewrite the other seat. Remembering the first answer made that rewrite
//! free: the thread went on counting as a conversation between its original
//! two, opened long ago and answered, while the newly named member started
//! receiving the fan-out's notifications for it. A thread that now names a
//! different pair is treated as the new conversation it is — see
//! [`Limiter::reseat`].
//!
//! WHAT A RESEAT ACTUALLY HANDS OVER, since it is easy to overstate. The thread
//! row, and every notification the fan-out sends into it afterwards. NOT the
//! history: each `dm_message` carries its own copy of the pair and the read
//! scope reads that copy off the row, so messages written before the rewrite
//! still name the old pair and stay unreadable to the newcomer. The reseater can
//! hand over the messages THEY wrote, by rewriting `a`/`b` on those rows too —
//! the write scope permits it, asking only that the writer is the sender and one
//! of the pair — but not the other participant's, which are pinned to a sender
//! the reseater is not.
//!
//! TIMESTAMPS ARE CLIENT-SUPPLIED, AND THE WINDOW LIVES WITH THAT. A sender
//! could future-date messages to jump the timeline or back-date them to slip
//! out of the window. Future-dating is the move that pays (a message dated
//! next year sits at the top of every "newest first" list forever) and it is
//! neutralized in `dm_timestamp`, the stage this worker is fed FROM: it
//! rewrites such a timestamp to the server clock, COMMITS it, and only then
//! passes the row on, so every reader — this one, the fan-out, and the client
//! queries that sort inside the query — gets the same honest number.
//!
//! The local `min(now)` in [`Limiter::observe`] therefore covers nothing in the
//! ordinary case; it is kept as a property of the counting rule itself.
//! [`Limiter`] is a plain type with its own tests and no knowledge of who feeds
//! it, and "count a message at no later than the clock you are handed" should be
//! true of the type rather than of one caller's pipeline.
//!
//! It does have one live case, though, and this worker is the only DM consumer
//! that has one. When the settling write FAILS, `dm_timestamp` withholds that
//! row from the fan-out — which would otherwise stamp an inbox row against a
//! send time still claiming next century — but forwards it HERE regardless,
//! because an uncounted message is budget a bulk sender did not spend, and that
//! is the more expensive mistake. So this is the one path on which an unsettled
//! send time reaches a DM consumer, and the ceiling below is what makes it cost
//! nothing. What the ceiling must not become
//! again is the whole defence: a clamp recomputed against the current clock
//! moves every time it is evaluated, and the boot sweep evaluates it on every
//! restart, which collapsed six gradually-opened future-dated threads into one
//! window and tombstoned the sender's next message.
//!
//! Back-dating is self-defeating — a back-dated message buries itself in the
//! recipient's history — so it is accepted rather than defended against. That
//! acceptance now covers one more move: back-dating the first message of a
//! thread ages that thread out of the initiation window early. The sender pays
//! for it in the only currency that matters to them, by burying every one of
//! those openings at the bottom of the recipients' lists.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use ankurah::error::RetrievalError;
use ankurah::{Context, EntityId};
use anyhow::{Context as _, Result};
use community_model::{canonical_pair, DmMessageView, DmThreadView, ModAction};
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
    /// The sender has started too many conversations inside the window. The
    /// observed message is what gets tombstoned, which is the one that opened
    /// the excess conversation the first time round — and, after that, any
    /// further message of theirs into a conversation still over the limit.
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

/// The two members a conversation is between, in [`canonical_pair`] order —
/// the identity of a conversation, and what initiations are counted by.
type Pair = (EntityId, EntityId);

/// The limiter's whole state: per-thread facts and, per sender, the timestamps
/// of their initiations and unanswered messages. Pure — no ankurah types, no
/// I/O — so the counting rules are testable on their own.
#[derive(Default)]
pub struct Limiter {
    threads: HashMap<EntityId, ThreadFacts>,
    /// sender -> a conversation they started -> the thread rows that
    /// conversation is spread across, each stamped with when its own oldest
    /// message landed.
    ///
    /// KEYED BY THE PAIR, NOT BY THE THREAD ROW. Two of one member's tabs
    /// racing on a first DM create two thread rows for the same two people and
    /// put an opener in each — the client treats that as ONE conversation
    /// (`leptos-app/src/dm.rs`, `Conversation`), and so must this: keyed by
    /// thread, three such races would spend all five of the sender's slots on
    /// three correspondents and tombstone their next message.
    ///
    /// The inner map is per thread rather than one stamp per pair because the
    /// window has to be measured against when the CONVERSATION started, which
    /// is the earliest of its rows. Keeping the rows apart is also what lets an
    /// entry be withdrawn from exactly one of them (see
    /// [`Limiter::withdraw_initiation`]).
    ///
    /// A pair is what counts, so a re-observed message (the boot sweep after a
    /// live delivery) cannot inflate the tally — and the stamp being the
    /// conversation's own start is what lets a long-standing one age out of the
    /// window while the sender keeps talking in it.
    initiations: HashMap<EntityId, HashMap<Pair, HashMap<EntityId, i64>>>,
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
    /// `now` is the server clock: the right edge of the window, and a ceiling
    /// over `client_ts`. That ceiling is inert on every settled row — which is
    /// every row the worker forwards except one whose settling write failed
    /// (`dm_timestamp::forward`) — and it is kept because it belongs to this
    /// type rather than to that pipeline: a
    /// message is counted at no later than the clock the caller supplies,
    /// whoever the caller is. The number itself must be the stored one, so that
    /// it is the same number after a restart.
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
        let pair = canonical_pair(participants.0, participants.1);
        // Inert on a settled row; live exactly when a settle failed upstream
        // (see the parameter doc). Kept so that the type's own rule holds for
        // any caller: without it a single future-dated opener stamps its initiation
        // past every cutoff and holds one of the sender's five slots for good.
        let ts = client_ts.min(now);

        // Who this thread's oldest message belonged to BEFORE this one joined
        // it. A message can arrive older than everything seen so far — the boot
        // sweep hands a thread's history back in entity-id order, and senders
        // stamp their own messages — so the opener is not settled until it is
        // recomputed below.
        let (initiated, monologue, started_at, displaced) = {
            let facts = self.threads.entry(thread).or_default();
            let opener_before = facts.first.map(|(_, who)| who);
            facts.observe(sender, ts);
            let displaced = opener_before.filter(|who| !facts.initiated_by(*who));
            // The conversation's own start, which is what the initiation window
            // is measured against. `ts` (this message) would restamp the entry
            // on every reply and it would never age out — see the module doc.
            (facts.initiated_by(sender), facts.unanswered_by_others(sender), facts.started_at().unwrap_or(ts), displaced)
        };

        // The credit follows the oldest message, so when the oldest message
        // changes hands the previous holder's entry has to go with it.
        // Otherwise a member whose clock lags — momentarily looking like the
        // thread's opener during the boot sweep — keeps an entry stamped at
        // their own message time for the rest of the window, and six of those
        // cost them a tombstoned message and a row every signed-in member can
        // read saying they opened six conversations.
        if let Some(displaced) = displaced {
            self.withdraw_initiation(displaced, pair, thread);
        }

        if initiated {
            self.initiations.entry(sender).or_default().entry(pair).or_default().insert(thread, started_at);
        }
        if monologue {
            self.unanswered.entry(sender).or_default().insert(message, ts);
        }

        let cutoff = now - WINDOW_MS;
        if let Some(by_pair) = self.initiations.get_mut(&sender) {
            // A conversation leaves the window when it STARTED before the
            // cutoff, and a conversation spread over two rows started at the
            // earlier of them. Pruning the rows individually would instead drop
            // the older row and leave the pair dated by its twin, which is how
            // a month-old conversation would read as opened today.
            by_pair.retain(|_, rows| rows.values().min().is_some_and(|t| *t >= cutoff));
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
    fn forget_initiation(&mut self, message: EntityId, thread: EntityId, participants: Pair, sender: EntityId) {
        self.forget_message(message, sender);
        self.withdraw_initiation(sender, canonical_pair(participants.0, participants.1), thread);
    }

    /// Drop one member's claim to having opened one thread row, and with it the
    /// whole conversation if that was its last row.
    fn withdraw_initiation(&mut self, sender: EntityId, pair: Pair, thread: EntityId) {
        let Some(by_pair) = self.initiations.get_mut(&sender) else { return };
        let Some(rows) = by_pair.get_mut(&pair) else { return };
        rows.remove(&thread);
        if rows.is_empty() {
            by_pair.remove(&pair);
        }
    }

    /// A thread row that now names a different pair is a different
    /// conversation, and this limiter has to account for it as one.
    ///
    /// `DmThread.a`/`b` are ordinary mutable properties and the write scope
    /// asks only whether the writer is one of them, so a participant's client
    /// can rewrite the other seat (recorded on the model field; closing it
    /// needs an immutable-field rule the policy grammar does not have). What
    /// this stops is that move being free: everything accumulated about the
    /// thread described a conversation between the OLD pair, and none of it is
    /// true of the new one. So the facts go — the newly named member has never
    /// answered, whatever the old correspondent did — and the message being
    /// observed re-establishes the thread from scratch, which charges its sender
    /// an initiation with the new correspondent, dated now, instead of
    /// inheriting a conversation opened long ago and answered.
    ///
    /// THE OLD PAIR KEEPS ITS INITIATION. The sender really did start a
    /// conversation with the previous correspondent, and the row changing hands
    /// does not unmake that — the limit counts conversations the sender STARTED
    /// and started inside the window, which is what this worker's module doc
    /// says it counts. Handing that entry back made the entire move free: open
    /// five conversations with five strangers this hour, rewrite one of those
    /// rows onto a sixth, and the withdrawal plus the re-observation netted to
    /// five, so the message went through and the sixth stranger was notified.
    /// Repeat for a seventh and an eighth: one thread row walked through
    /// correspondent after correspondent at no initiation cost, bounded only by
    /// the unanswered-monologue limit four times further up.
    fn reseat(&mut self, thread: EntityId) { self.threads.remove(&thread); }
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
    /// thread -> the pair the THREAD row named the last time this worker
    /// looked. The limiter is told who a conversation belongs to rather than
    /// believing the message's own copy of it (see [`Limiter::observe`]), and
    /// this is what that answer is compared against so a thread quietly
    /// reseated onto a third member is noticed (see [`thread_participants`]).
    ///
    /// Memory posture, on the same terms as [`Limiter::threads`] beside it: one
    /// entry per thread this process has ever seen a message in, never evicted,
    /// two entity ids each. It is bounded by the number of conversations rather
    /// than by traffic, and at community scale that is small enough to keep the
    /// simple thing. Whatever bounds `Limiter::threads` one day — the standing
    /// DM query holding every live message in memory is the bigger number, and
    /// `workers::watch_dms` says what replaces it — bounds this too.
    thread_pairs: HashMap<EntityId, Pair>,
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
            state.limiter.forget_initiation(msg.id(), thread, participants, sender);
            // Ids and counts only — never text, and never the recipient: the
            // mod log is readable by every signed-in member, and naming who
            // someone DMs would disclose exactly what the DM read scope exists
            // to protect.
            warn!(sender = %sender, thread = %thread, initiations, "DM rate limit: tombstoned a message into a conversation past the initiation limit");
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

/// The pair named by a thread row, re-read for every message and compared
/// against the pair this worker last saw on it.
///
/// WHY IT IS RE-READ RATHER THAN REMEMBERED. `DmThread.a`/`b` are the load-
/// bearing answer to who a conversation belongs to — every consumer resolves
/// participants from here rather than from the sender-written copies on a
/// message — and they are ordinary mutable properties. The write scope asks
/// only whether the writer is one of `a`/`b`, which a participant satisfies
/// both before and after changing the other seat. Remembering the first answer
/// and short-circuiting on it meant a reseated thread stayed, in this worker's
/// picture, a conversation between its original two members that was opened
/// long ago and answered — which is to say it cost nothing, while the newly
/// named member started receiving the fan-out's notifications for it (not its
/// history; the module doc says what a reseat does and does not hand over).
/// The cost of asking every time is one local read per message, including
/// during the boot sweep; the thing it buys is that the pair the limiter counts
/// by is the pair the row actually names.
///
/// `None` means the row is not there or does not carry both participants —
/// which is not an error worth retrying: a message can name any thread id its
/// sender likes, including one no row was ever created for, and such a message
/// is invisible to everyone (no thread row, no sidebar entry, and `dm_notify`
/// tells nobody about it).
async fn thread_participants(ctx: &Context, state: &mut State, thread: EntityId) -> Option<Pair> {
    let view = match ctx.get::<DmThreadView>(thread).await {
        Ok(view) => view,
        // No such row is ordinary traffic and stays at debug — messages naming a
        // thread nobody created are an accepted residual, and logging each one
        // would be noise. A storage failure is a different thing wearing the
        // same shape: this lookup runs for EVERY message, and a failed one drops
        // that message out of the window and the unanswered count with nothing
        // to show for it.
        //
        // The split is `mentions::deliver`'s, the disposition is not: that
        // function hands a storage error back to its caller, while this one
        // answers `None` to both legs, so `process` cannot tell them apart and
        // returns Ok either way. Neither shape is retried in-process by anything
        // in this file, so what the return type costs is the precision of this
        // log line and nothing beyond it. See the same note in `dm_notify`.
        Err(RetrievalError::EntityNotFound(_)) | Err(RetrievalError::CollectionNotFound(_)) => {
            debug!(thread = %thread, "DM rate limit: no thread row for this message, so nothing to count");
            return None;
        }
        Err(e) => {
            warn!(thread = %thread, "DM rate limit: could not read this message's thread row, so the message goes uncounted: {e:#}");
            return None;
        }
    };
    let (Ok(a), Ok(b)) = (view.a(), view.b()) else {
        debug!(thread = %thread, "DM rate limit: thread row names no participants, so nothing to count");
        return None;
    };
    let pair = (a.id(), b.id());
    let reseated =
        state.thread_pairs.insert(thread, pair).is_some_and(|was| canonical_pair(was.0, was.1) != canonical_pair(pair.0, pair.1));
    if reseated {
        // Ids only, and only the thread's: the mod log's rule applies to the
        // server log too.
        warn!(thread = %thread, "DM rate limit: this thread now names a different pair; counting it as a new conversation");
        state.limiter.reseat(thread);
    }
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
/// DISCLOSURE, STATED: `modaction` is readable by every signed-in member by
/// design (and by no guest), so this row tells the community that this member
/// tripped the DM rate limit. It does not say who they messaged or what they
/// wrote. That trade is deliberate: without
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
    /// limit and tombstoned past it — and the count is of DISTINCT
    /// conversations, so re-observing the same message (the boot sweep after a
    /// live delivery) never inflates it.
    #[test]
    fn initiations_are_counted_per_distinct_conversation_and_capped() {
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
    /// false) and wrote a row the whole signed-in community can read, accusing
    /// the member of sending in bulk.
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

    /// A conversation is the two members, never the row they happened to agree
    /// on. Two of one member's tabs racing on a first DM leave two `dm_thread`
    /// rows for the same pair and put an opener in each, and the client already
    /// reads those as one conversation (`leptos-app/src/dm.rs`,
    /// `dm::Conversation`). Counted per row instead, three such races would
    /// spend all five of the sender's slots on three correspondents, tombstone
    /// their next message and write a row every signed-in member can read,
    /// saying they had started six conversations.
    #[test]
    fn race_twins_between_the_same_two_members_cost_one_initiation() {
        let mut limiter = Limiter::default();
        let sender = EntityId::new();
        let now = 1_700_000_000_000;
        let correspondents = ids(MAX_INITIATIONS_PER_WINDOW + 1);

        // Every one of the five conversations is opened twice, in two rows.
        for (i, correspondent) in correspondents.iter().take(MAX_INITIATIONS_PER_WINDOW).enumerate() {
            let with = pair(sender, *correspondent);
            for twin in 0..2 {
                let ts = now + twin;
                assert_eq!(
                    limiter.observe(EntityId::new(), EntityId::new(), with, sender, ts, ts),
                    Verdict::Allow,
                    "row {twin} of conversation {i}"
                );
            }
        }

        // A sixth correspondent is a sixth conversation, and the count says so:
        // six, not eleven.
        let last = MAX_INITIATIONS_PER_WINDOW;
        assert_eq!(
            limiter.observe(EntityId::new(), EntityId::new(), pair(sender, correspondents[last]), sender, now, now),
            Verdict::TooManyInitiations { initiations: MAX_INITIATIONS_PER_WINDOW + 1 },
            "ten rows between five people are five conversations; the sixth person is the sixth"
        );
    }

    /// An initiation is credited to whoever sent the thread's OLDEST message,
    /// so when an older message arrives and moves that, the entry moves too.
    ///
    /// The boot sweep hands a thread's history back in entity-id order, which
    /// only approximates send order, and every client stamps its own messages —
    /// so a member whose clock lags can momentarily look like a thread's
    /// opener. Left in place, that entry counts against them for the rest of
    /// the window, and six of them cost a tombstoned message and a public row
    /// saying they started six conversations they did not.
    #[test]
    fn an_initiation_moves_off_a_member_when_an_older_message_takes_the_thread() {
        let mut limiter = Limiter::default();
        let (opener, replier) = (EntityId::new(), EntityId::new());
        let thread = EntityId::new();
        let with = pair(opener, replier);
        let now = 1_700_000_000_000;

        // The reply is seen first, so for a moment the thread looks like the
        // replier's.
        assert_eq!(limiter.observe(EntityId::new(), thread, with, replier, now - 30_000, now), Verdict::Allow);
        // Then the message that actually opened the thread arrives, older.
        assert_eq!(limiter.observe(EntityId::new(), thread, with, opener, now - 60_000, now), Verdict::Allow);

        // The replier is owed their whole budget: a conversation they did not
        // start must not be holding one of their five slots.
        let strangers = ids(MAX_INITIATIONS_PER_WINDOW);
        for (i, stranger) in strangers.iter().enumerate() {
            assert_eq!(
                limiter.observe(EntityId::new(), EntityId::new(), pair(replier, *stranger), replier, now, now),
                Verdict::Allow,
                "conversation {i}, which the replier really did start"
            );
        }
    }

    /// A thread row rewritten to name someone else is a different
    /// conversation, and it is charged as one.
    ///
    /// Nothing stops the rewrite — `DmThread.a`/`b` are ordinary mutable
    /// properties and the write scope only asks whether the writer is one of
    /// them — so what this limiter can do is make it cost the same as opening
    /// a conversation, which is what it is. Everything the worker had
    /// accumulated described the OLD pair: that the thread was opened a month
    /// ago, and that the correspondent had answered. None of it is true of the
    /// newly named member, so none of it survives.
    #[test]
    fn a_thread_reseated_onto_a_third_member_is_charged_as_a_new_conversation() {
        const DAY_MS: i64 = 24 * 60 * 60 * 1000;

        let mut limiter = Limiter::default();
        let sender = EntityId::new();
        let now = 1_700_000_000_000;
        let threads = ids(MAX_INITIATIONS_PER_WINDOW + 1);
        let originals = ids(MAX_INITIATIONS_PER_WINDOW + 1);
        let newcomers = ids(MAX_INITIATIONS_PER_WINDOW + 1);

        // Six conversations opened weeks ago, one per day and every one of them
        // answered, so every one is outside today's window and exempt from
        // both limits.
        for (i, thread) in threads.iter().enumerate() {
            let with = pair(sender, originals[i]);
            let opened = now - (30 - i as i64) * DAY_MS;
            assert_eq!(
                limiter.observe(EntityId::new(), *thread, with, sender, opened, opened),
                Verdict::Allow,
                "opening conversation {i}, weeks apart"
            );
            assert_eq!(
                limiter.observe(EntityId::new(), *thread, with, originals[i], opened + 1, opened + 1),
                Verdict::Allow,
                "correspondent {i} answering"
            );
        }

        // Today the sender rewrites five of those threads onto five new people
        // and writes into each. Each one is a conversation started today.
        for (i, thread) in threads.iter().enumerate().take(MAX_INITIATIONS_PER_WINDOW) {
            limiter.reseat(*thread);
            assert_eq!(
                limiter.observe(EntityId::new(), *thread, pair(sender, newcomers[i]), sender, now, now),
                Verdict::Allow,
                "reseating conversation {i} onto someone new"
            );
        }

        let last = MAX_INITIATIONS_PER_WINDOW;
        limiter.reseat(threads[last]);
        assert_eq!(
            limiter.observe(EntityId::new(), threads[last], pair(sender, newcomers[last]), sender, now, now),
            Verdict::TooManyInitiations { initiations: MAX_INITIATIONS_PER_WINDOW + 1 },
            "five reseated threads are five conversations started today, so the sixth is over the limit"
        );
    }

    /// The boundary the previous test cannot reach, because every conversation
    /// in it was opened weeks ago and had already aged out of the window: a
    /// thread changing hands does not give the sender back what they spent
    /// starting the conversation it used to be.
    ///
    /// The sender opens five conversations with five strangers this hour, which
    /// is the whole budget, then rewrites one of those rows onto a sixth
    /// stranger and writes into it. That is a sixth conversation started inside
    /// the same hour and the message into it is tombstoned. While `reseat`
    /// withdrew the old pair's entry, the re-observation put the new pair's
    /// entry in its place and the count never moved: the message went through,
    /// the sixth stranger was notified, and the same row could be walked through
    /// correspondent after correspondent at no cost — the initiation limit
    /// replaced, in practice, by the unanswered-monologue one four times further
    /// up.
    #[test]
    fn reseating_a_thread_opened_this_hour_does_not_refund_its_initiation() {
        let mut limiter = Limiter::default();
        let sender = EntityId::new();
        let newcomer = EntityId::new();
        let now = 1_700_000_000_000;
        let threads = ids(MAX_INITIATIONS_PER_WINDOW);
        let strangers = ids(MAX_INITIATIONS_PER_WINDOW);

        // The whole budget, spent within the hour.
        for (i, thread) in threads.iter().enumerate() {
            assert_eq!(
                limiter.observe(EntityId::new(), *thread, pair(sender, strangers[i]), sender, now, now),
                Verdict::Allow,
                "opening conversation {i}"
            );
        }

        // One of those rows is rewritten onto someone who was never in it.
        limiter.reseat(threads[0]);
        assert_eq!(
            limiter.observe(EntityId::new(), threads[0], pair(sender, newcomer), sender, now + 1, now + 1),
            Verdict::TooManyInitiations { initiations: MAX_INITIATIONS_PER_WINDOW + 1 },
            "a sixth correspondent inside the window is a sixth conversation, whatever thread row carried the sender there"
        );
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
            let with = pair(sender, EntityId::new());
            let ts = now + i;
            assert!(
                matches!(limiter.observe(message, thread, with, sender, ts, ts), Verdict::TooManyInitiations { .. }),
                "attempt {i} past the initiation limit"
            );
            limiter.forget_initiation(message, thread, with, sender);
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

    /// A future-dated message cannot buy itself a fresh window: this limiter
    /// counts it at no later than the server clock it is handed.
    ///
    /// This is the type's own rule, not the lane's answer to future-dating —
    /// `dm_timestamp` settles a row before it forwards it, so the worker's
    /// `client_ts` arrives at or below `now` and the ceiling does nothing there.
    /// It is pinned here because [`Limiter`] does not know who feeds it, and
    /// what it costs to drop is exactly the case below: a single future-dated
    /// opener stamping its initiation past every cutoff and holding one of the
    /// sender's five slots for good.
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
