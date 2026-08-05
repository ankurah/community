//! Server-side reactive workers: derived data the clients cannot (and must
//! not) compute for themselves.
//!
//! Ankurah has no aggregate queries; the sanctioned pattern for
//! server-maintained derived rows is a standing LiveQuery on the durable
//! node's privileged context feeding a background task — the same shape as
//! ankurah-jwt-auth's durable policy watcher (a spawned task that owns its
//! context and parks forever). Mechanism, verified against ankurah-core
//! 0.9.0 sources:
//!
//! - `Context::query::<MessageView>(..)` registers a reactor query on the
//!   durable node (`livequery.rs`: `create_inner` spawns the activation task
//!   directly when the node has no relay).
//! - `LiveQuery<R>: Subscribe<ChangeSet<R>>` delivers per-entity
//!   `ItemChange`s; the reactor is notified for BOTH local commits
//!   (`context.rs` `commit_local_trx`) and events arriving from remote
//!   (client) peers (`node.rs` `commit_events`, `node_applier.rs`) — so a
//!   message posted by any websocket client lands here.
//! - The `SubscriptionGuard` and the `LiveQuery` itself must stay alive for
//!   the subscription to keep firing; this task owns both across a
//!   `pending()` await.
//!
//! Startup handoff (why Initial items are ignored in the listener): the
//! subscription races the LiveQuery's own activation task, so Initial
//! delivery to our listener is not guaranteed. Instead we `wait_initialized`
//! and sweep the whole resultset once — the consumers are idempotent
//! (derived-row existence checks), so an item seen both ways costs one
//! redundant probe, and a message committed in the gap arrives as a normal
//! Add. The sweep also heals crash gaps: a message committed just before a
//! restart still gets its fan-out on the next boot.
//!
//! Both consumers are fed from ONE LiveQuery (one reactor registration, one
//! in-memory resultset) through separate channels, so a slow unfurl (network
//! I/O) can never delay mention delivery.
//!
//! Each consumer runs under a respawn supervisor ([`supervise`]): the
//! supervisor owns the channel receiver and lends it per attempt, so a panic
//! inside a consumer is caught and logged, the channel stays open (producers
//! keep buffering through the pause), and consumption resumes — only the
//! in-flight message is dropped, healed by the next boot sweep or the
//! message's next change.

pub mod dm_notify;
pub mod dm_rate_limit;
pub mod dm_timestamp;
pub mod mentions;
pub mod og;
pub mod ssrf;
pub mod unfurl;

use std::collections::HashMap;
use std::panic::AssertUnwindSafe;

use ankurah::changes::{ChangeSet, ItemChange};
use ankurah::signals::{Peek, Subscribe};
use ankurah::{Context, EntityId, LiveQuery};
use anyhow::Result;
use community_model::{DmMessageView, MessageView};
use futures_util::future::BoxFuture;
use futures_util::FutureExt;
use tokio::sync::mpsc::UnboundedReceiver;
use tracing::{error, info, warn};

/// Start the worker subsystem on the durable node's privileged (Root)
/// context. Fire-and-forget from `main`: failures to start are fatal-logged
/// (the server keeps serving chat; derived data just goes stale), and the
/// task never returns otherwise.
pub fn start(ctx: Context) {
    let dm_ctx = ctx.clone();
    tokio::spawn(async move {
        if let Err(e) = watch_messages(ctx).await {
            error!("message workers failed to start: {e:#}");
        }
    });
    tokio::spawn(async move {
        if let Err(e) = watch_dms(dm_ctx).await {
            error!("DM workers failed to start: {e:#}");
        }
    });
}

/// The DM half of the worker subsystem (#30): one standing `dm_message`
/// LiveQuery feeding a short PIPELINE. [`dm_timestamp`] settles every row's
/// send time first and passes the settled row on to [`dm_notify`] and
/// [`dm_rate_limit`] itself. Compare [`watch_messages`], where the two
/// room-message consumers really do run in parallel — neither of them writes
/// anything the other reads.
///
/// WHY THE TIMESTAMP WORKER IS A STAGE AND NOT A THIRD PARALLEL CONSUMER. It
/// rewrites the very number the other two count and compare by. Run alongside
/// them, it lost races: the fan-out could sample its own clock for a new inbox
/// row's `created_at` before the settling write sampled its clock, storing a
/// notification dated earlier than the message it announced, and nothing
/// afterwards repaired that row (see `dm_timestamp`'s module doc for the
/// duplicate unread notification it produced on the next restart). Settling
/// first removes the window instead of converging out of it — and when a settle
/// fails, the stage withholds that row from the fan-out rather than announcing
/// something it could not make honest. The limiter is told about it either way;
/// that is the one place a DM consumer can still see an unsettled send time, and
/// its own ceiling under the server clock is what covers it.
///
/// It is a SEPARATE query and a separate pipeline on purpose, not a branch
/// inside the room-message one. The mention fan-out must never run on DM
/// text — a third party named inside a private thread cannot read it and must
/// not be told it exists — and the surest way to guarantee that is for the DM
/// stream never to reach the mention worker at all. `dm_notify` explains the
/// rule; this is the structure that enforces it.
///
/// Cost: the query holds every `dm_message` in memory on the durable node,
/// tombstones included — one step wider than the room-message query above,
/// which keeps to `deleted = false`. Tombstones are in it because a tombstoned
/// row still has a send time that has to be settled (`dm_chat` renders
/// tombstones in the timeline, ordered by it); they are dropped again where the
/// pipeline forwards, so neither the fan-out nor the limiter sees them. The rate
/// limiter needs a full history to rebuild its window after a restart, so the
/// whole-collection query is load-bearing rather than incidental; if DM volume
/// ever outgrows it, the replacement is a bounded recent-window query plus
/// persisted counters, not a smaller sweep.
async fn watch_dms(ctx: Context) -> Result<()> {
    let (notify_tx, notify_rx) = tokio::sync::mpsc::unbounded_channel::<DmMessageView>();
    // The limiter's channel carries the boot sweep's end marker as well as the
    // rows: it counts the backlog but does not judge it (see dm_rate_limit).
    let (limit_tx, limit_rx) = tokio::sync::mpsc::unbounded_channel::<dm_rate_limit::Traffic>();
    // The head of the pipeline, and for live rows the ONLY way in: the two
    // channels above are fed by the timestamp worker once it has settled a row,
    // and by the boot sweep below once it has done the same inline.
    let (stamp_tx, stamp_rx) = tokio::sync::mpsc::unbounded_channel::<DmMessageView>();

    // Every dm_message row, tombstoned or not. A tombstone does not settle a
    // future-dated send time, and both remaining readers of one — the thread
    // timeline, which orders tombstones by `timestamp`, and a sender who can
    // still rewrite that field — need it settled (see dm_timestamp). Tombstoned
    // rows go no further than the forward step, so the limiter's own tombstone
    // write, which now arrives as an Update rather than a Remove, still reaches
    // neither consumer: enforcing on a row it just tombstoned would loop.
    let live: LiveQuery<DmMessageView> = ctx.query("true")?;

    let subscription_guard = live.subscribe(move |changeset: ChangeSet<DmMessageView>| {
        for change in &changeset.changes {
            match change {
                ItemChange::Add { item, .. } | ItemChange::Update { item, .. } => {
                    // send() fails only at process teardown: the supervisor
                    // owns each receiver for the process lifetime (a consumer
                    // panic pauses consumption without closing the channel).
                    let _ = stamp_tx.send(item.clone());
                }
                ItemChange::Initial { .. } | ItemChange::Remove { .. } => {}
            }
        }
    });

    {
        let ctx = ctx.clone();
        // The forwarding senders belong to the supervisor's closure, not to one
        // attempt: it clones them per attempt, so a respawned loop keeps
        // feeding the two consumers instead of settling rows into silence.
        let notify_tx = notify_tx.clone();
        let limit_tx = limit_tx.clone();
        supervise("DM timestamp", stamp_rx, move |rx| {
            dm_timestamp::run(ctx.clone(), rx, notify_tx.clone(), limit_tx.clone()).boxed()
        });
    }
    {
        let ctx = ctx.clone();
        supervise("DM notification", notify_rx, move |rx| dm_notify::run(ctx.clone(), rx).boxed());
    }
    {
        let ctx = ctx.clone();
        // The limiter's state belongs to the supervisor, not to the consumer
        // loop: a panic must not restart the window from empty (every old
        // thread would then look like a fresh initiation) nor drop the
        // enforcing latch, which only the boot sweep can raise.
        let state = std::sync::Arc::new(tokio::sync::Mutex::new(dm_rate_limit::State::default()));
        supervise("DM rate limit", limit_rx, move |rx| dm_rate_limit::run(ctx.clone(), state.clone(), rx).boxed());
    }

    live.wait_initialized().await;
    let backlog: Vec<DmMessageView> = live.resultset().peek();
    info!(messages = backlog.len(), "DM workers: standing dm_message LiveQuery initialized; sweeping backlog");
    // The pipeline's boot half, written out here rather than pushed through the
    // timestamp worker's channel, because the backlog has to be settled as a
    // WHOLE before any of it is counted. The rate limiter rebuilds its entire
    // window from this sweep on every restart, and that window is only worth
    // anything if it is built from values that will not move afterwards: a row
    // counted at its claimed year-2100 date and settled a moment later would
    // enter the window twice, once at each value. Doing it inline is nearly
    // free — a row already at or before the server clock costs one field read
    // and no write, which is every row after the first boot that sees it.
    let mut settled: Vec<bool> = Vec::with_capacity(backlog.len());
    for msg in &backlog {
        match dm_timestamp::settle(&ctx, msg).await {
            Ok(()) => settled.push(true),
            Err(e) => {
                warn!(message = %msg.id(), "DM boot sweep: could not settle this message's timestamp; counting it as it stands, and not announcing it: {e:#}");
                settled.push(false);
            }
        }
    }

    // Same forward step the live stage uses, so both paths agree on what the two
    // consumers are owed: the limiter is told about every live row, the fan-out
    // only about rows that are settled, and tombstones reach neither.
    for (msg, settled) in backlog.iter().zip(settled) {
        dm_timestamp::forward(msg, settled, &notify_tx, &limit_tx);
    }
    // The limiter has now seen every message that existed at startup, so its
    // picture of each thread is as complete as storage can make it. Verdicts
    // are acted on from here; everything above only counted.
    let _ = limit_tx.send(dm_rate_limit::Traffic::BacklogComplete);

    std::future::pending::<()>().await;
    drop((live, subscription_guard)); // unreachable; documents what parking keeps alive
    Ok(())
}

async fn watch_messages(ctx: Context) -> Result<()> {
    let (mention_tx, mention_rx) = tokio::sync::mpsc::unbounded_channel::<MessageView>();
    let (unfurl_tx, unfurl_rx) = tokio::sync::mpsc::unbounded_channel::<MessageView>();

    // Deleted messages produce neither notifications nor previews; the
    // predicate keeps them (and delete-flips, which arrive as Removes) out of
    // the stream entirely. Un-deleting re-delivers as an Add — harmless,
    // because the consumers are idempotent.
    let live: LiveQuery<MessageView> = ctx.query("deleted = false")?;

    let subscription_guard = {
        let mention_tx = mention_tx.clone();
        let unfurl_tx = unfurl_tx.clone();
        live.subscribe(move |changeset: ChangeSet<MessageView>| {
            for change in &changeset.changes {
                match change {
                    // Add covers new messages AND un-deletes; Update covers
                    // text edits (which may introduce mentions/URLs).
                    ItemChange::Add { item, .. } | ItemChange::Update { item, .. } => {
                        // send() fails only at process teardown: the
                        // supervisor owns each receiver for the process
                        // lifetime (a consumer panic pauses consumption
                        // without closing the channel).
                        let _ = mention_tx.send(item.clone());
                        let _ = unfurl_tx.send(item.clone());
                    }
                    // Initial: covered by the post-initialization sweep below.
                    // Remove: a deletion — nothing to derive.
                    ItemChange::Initial { .. } | ItemChange::Remove { .. } => {}
                }
            }
        })
    };

    {
        let ctx = ctx.clone();
        supervise("notification fan-out", mention_rx, move |rx| mentions::run(ctx.clone(), rx).boxed());
    }
    {
        let ctx = ctx.clone();
        supervise("link-unfurl", unfurl_rx, move |rx| unfurl::run(ctx.clone(), rx).boxed());
    }

    live.wait_initialized().await;
    let backlog: Vec<MessageView> = live.resultset().peek();
    info!(messages = backlog.len(), "message workers: standing message LiveQuery initialized; sweeping backlog");
    for msg in backlog {
        let _ = mention_tx.send(msg.clone());
        let _ = unfurl_tx.send(msg);
    }

    // Park forever. `live` and `subscription_guard` are owned across this
    // await — dropping either would silently tear the standing query down.
    std::future::pending::<()>().await;
    drop((live, subscription_guard)); // unreachable; documents what parking keeps alive
    Ok(())
}

/// Run one consumer under a respawn supervisor. The supervisor owns the
/// channel receiver and lends it to each attempt, so a panic inside the
/// consumer — caught here, logged loudly — never closes the channel:
/// producers keep buffering through the pause and only the in-flight message
/// is dropped (idempotent probes heal it on the message's next change or the
/// next boot sweep). A graceful channel close ends the supervisor: that only
/// happens at process teardown.
fn supervise<T: Send + 'static>(
    name: &'static str,
    mut rx: UnboundedReceiver<T>,
    run: impl for<'a> Fn(&'a mut UnboundedReceiver<T>) -> BoxFuture<'a, ()> + Send + 'static,
) {
    tokio::spawn(async move {
        loop {
            match AssertUnwindSafe(run(&mut rx)).catch_unwind().await {
                Ok(()) => break,
                Err(_) => {
                    error!("{name} worker panicked; respawning consumer in 5s (channel intact, in-flight message dropped)");
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                }
            }
        }
    });
}

/// ms since epoch — the project's timestamp unit (`Message.timestamp` is
/// `js_sys::Date::now() as i64` on the client).
pub(crate) fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// Order-sensitive fingerprint of a scanned-token list, for the consumers'
/// "already handled this exact set" caches.
pub(crate) fn signature(items: &[String]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    items.hash(&mut hasher);
    hasher.finish()
}

/// Insert into a consumer's handled-cache, keeping it bounded. Eviction is a
/// wholesale clear: crude, but the cache is an optimization over idempotent
/// storage checks, so correctness never depends on what it remembers.
pub(crate) fn remember(cache: &mut HashMap<EntityId, u64>, id: EntityId, sig: u64) {
    const MAX_ENTRIES: usize = 8192;
    if cache.len() >= MAX_ENTRIES {
        cache.clear();
    }
    cache.insert(id, sig);
}

/// Runtime proof of the keystone claim — that a standing LiveQuery on a
/// durable node's context really does drive the workers for freshly
/// committed messages — on a real (sled) node. Gated on the sled feature:
/// run with `cargo test -p community-server --no-default-features --features
/// sled`. The default (postgres) test run skips it because it would need a
/// live database.
#[cfg(all(test, feature = "sled"))]
mod tests {
    use super::*;
    use ankurah::policy::{PermissiveAgent, DEFAULT_CONTEXT};
    use ankurah::Node;
    use ankurah_storage_sled::SledStorageEngine;
    use community_model::{
        canonical_pair, DmMessage, DmMessageView, DmThread, DmThreadView, LinkPreview, LinkPreviewView, Message,
        ModActionView, NotificationView, Room, User,
    };
    use std::sync::Arc;
    use std::time::Duration;

    async fn test_context() -> Context {
        // The same durable-node dance as main(), with the permissive agent —
        // worker mechanics don't depend on which policy agent runs, only on
        // having a privileged-equivalent Context.
        let node = Node::new_durable(Arc::new(SledStorageEngine::new_test().unwrap()), PermissiveAgent::new());
        node.system.wait_loaded().await;
        if node.system.root().is_none() {
            node.system.create().await.unwrap();
        }
        node.system.wait_system_ready().await;
        node.context_async(DEFAULT_CONTEXT).await
    }

    /// Poll until the notification materializes, or fail after a generous
    /// deadline — the workers are asynchronous by design, so assertions on
    /// their output must wait for them.
    async fn wait_for_first_notification(ctx: &Context) -> NotificationView {
        for _ in 0..200 {
            if let Some(n) = ctx.fetch::<NotificationView>("true").await.unwrap().into_iter().next() {
                return n;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("timed out waiting for the mention notification");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn workers_react_to_committed_messages_end_to_end() {
        let ctx = test_context().await;
        start(ctx.clone());

        // Seed users, a room, and (crucially, BEFORE any message references
        // it) a LinkPreview row — its existence must stop the unfurl worker
        // from ever fetching this URL. Tests must not touch the network.
        let url = "https://example.invalid/cached-before-message";
        let trx = ctx.begin();
        let author = trx.create(&User { display_name: "Author".into(), oidc_sub: None }).await.unwrap().id();
        let recipient = trx.create(&User { display_name: "Recipient".into(), oidc_sub: None }).await.unwrap().id();
        let room = trx.create(&Room { name: "general".into(), created_by: None, topic: None }).await.unwrap().id();
        trx.create(&LinkPreview {
            url: url.to_string(),
            title: Some("seeded".into()),
            description: None,
            image_url: None,
            fetched_at: 1,
            ok: true,
        })
        .await
        .unwrap();
        trx.commit().await.unwrap();

        // A message mentioning the recipient (and the author — self-mentions
        // must NOT notify) and carrying the pre-cached URL.
        let text = format!("hey <@{}> (ignore <@{}>) see {url}", recipient.to_base64(), author.to_base64());
        let trx = ctx.begin();
        let message =
            trx.create(&Message { user: author.into(), room: room.into(), text, timestamp: 1, deleted: false, edited_at: None, collaborative: None, re: None })
                .await
                .unwrap()
                .id();
        trx.commit().await.unwrap();

        // The reactive path (or the boot sweep, if the commit won the race
        // against LiveQuery activation — both are correct) must produce the
        // notification without any polling logic in the worker itself.
        let notification = wait_for_first_notification(&ctx).await;
        assert_eq!(notification.recipient().unwrap().id(), recipient);
        assert_eq!(notification.kind().unwrap(), "mention");
        assert_eq!(notification.message().unwrap().map(|r| r.id()), Some(message));
        assert_eq!(notification.actor().unwrap().map(|r| r.id()), Some(author));
        assert_eq!(notification.room().unwrap().map(|r| r.id()), Some(room));
        assert!(!notification.seen().unwrap());

        // Idempotency under edits: change the text but keep the mention. The
        // Update flows through the same pipeline; the existence check must
        // swallow it.
        let trx = ctx.begin();
        let editable = ctx.fetch::<community_model::MessageView>("true").await.unwrap().into_iter().next().unwrap().edit(&trx).unwrap();
        editable.text().replace(&format!("edited <@{}> {url}", recipient.to_base64())).unwrap();
        trx.commit().await.unwrap();

        // Deliberately generous settle time; then: still exactly one
        // notification (self-mention excluded, edit not double-delivered)...
        tokio::time::sleep(Duration::from_millis(500)).await;
        let notifications = ctx.fetch::<NotificationView>("true").await.unwrap();
        assert_eq!(notifications.len(), 1, "exactly one notification: no self-mention row, no edit duplicate");
        // ...and still exactly one LinkPreview row, the seeded one — the
        // worker recognized it and never re-fetched (had it tried to fetch,
        // .invalid can't resolve and a second ok:false row would exist).
        let previews = ctx.fetch::<LinkPreviewView>("true").await.unwrap();
        assert_eq!(previews.len(), 1, "pre-cached URL must not be re-fetched or duplicated");
        assert_eq!(previews[0].title().unwrap().as_deref(), Some("seeded"));
    }

    /// The DM fan-out rule that has real privacy weight (#30): a DM notifies
    /// the OTHER PARTICIPANT and nobody else — in particular, a mention token
    /// inside DM text notifies nobody.
    ///
    /// Why this must be pinned rather than assumed: the mention scanner is a
    /// pure function over text and the notification worker is one channel away
    /// from the DM stream, so "run the mention fan-out on DM text too" is a
    /// two-line change that looks like a feature and is actually a leak. A
    /// third party named in a private thread cannot read that thread (the
    /// `dm_message` read scope names exactly two people), so a notification
    /// would tell them a conversation they have no access to is about them and
    /// deep-link them into a view that renders empty.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_dm_mentioning_a_third_party_notifies_only_the_recipient() {
        let ctx = test_context().await;
        start(ctx.clone());

        let trx = ctx.begin();
        let alice = trx.create(&User { display_name: "Alice".into(), oidc_sub: None }).await.unwrap().id();
        let bob = trx.create(&User { display_name: "Bob".into(), oidc_sub: None }).await.unwrap().id();
        let carol = trx.create(&User { display_name: "Carol".into(), oidc_sub: None }).await.unwrap().id();
        trx.commit().await.unwrap();

        let (a, b) = canonical_pair(alice, bob);
        let trx = ctx.begin();
        let thread = trx.create(&DmThread { a: a.into(), b: b.into(), created_at: 1, deleted: false }).await.unwrap().id();
        trx.commit().await.unwrap();

        // Alice DMs Bob, naming Carol in the text. Carol is not a participant.
        let text = format!("bob, what do you make of <@{}>?", carol.to_base64());
        let trx = ctx.begin();
        trx.create(&DmMessage {
            thread: thread.into(),
            a: a.into(),
            b: b.into(),
            user: alice.into(),
            text,
            timestamp: 2,
            deleted: false,
            edited_at: None,
        })
        .await
        .unwrap();
        trx.commit().await.unwrap();

        let notification = wait_for_first_notification(&ctx).await;
        assert_eq!(notification.recipient().unwrap().id(), bob, "the recipient is the other participant");
        assert_eq!(notification.kind().unwrap(), "dm");
        assert_eq!(notification.actor().unwrap().map(|r| r.id()), Some(alice), "the actor is the sender — the deep-link target");
        assert_eq!(notification.message().unwrap().map(|r| r.id()), None, "Notification.message is a room-message ref; a DM cannot ride in it");
        assert_eq!(notification.room().unwrap().map(|r| r.id()), None, "a DM happens in no room");
        assert!(!notification.seen().unwrap());

        // Generous settle, then: still exactly one row in the whole database.
        // Carol has none, and Bob has not been notified twice.
        tokio::time::sleep(Duration::from_millis(500)).await;
        let all = ctx.fetch::<NotificationView>("true").await.unwrap();
        assert_eq!(all.len(), 1, "exactly one notification: the mention token in DM text must mint nothing");
        assert!(
            !all.iter().any(|n| n.recipient().map(|r| r.id() == carol).unwrap_or(false)),
            "a third party named inside a DM must never be notified of it"
        );

        // A second DM from the same sender, while the first is unread, does not
        // add a row — one unseen row per correspondent (see dm_notify).
        let trx = ctx.begin();
        trx.create(&DmMessage {
            thread: thread.into(),
            a: a.into(),
            b: b.into(),
            user: alice.into(),
            text: "still there?".into(),
            timestamp: 3,
            deleted: false,
            edited_at: None,
        })
        .await
        .unwrap();
        trx.commit().await.unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(
            ctx.fetch::<NotificationView>("true").await.unwrap().len(),
            1,
            "a second unread DM from the same person coalesces into the existing inbox row"
        );
    }

    /// `DmMessage.a`/`b` are the SENDER'S CLAIM about who a message is between,
    /// and nothing that acts on a DM may read them as the truth.
    ///
    /// They exist so the read scope can decide row-locally who may see a
    /// message, they are client-written LWW fields, and the write scope checks
    /// them only against the writer — so any member can file a row into any
    /// thread with any pair on it. If the fan-out believed them, one member
    /// would have an unlimited notification channel to strangers: write into
    /// your own long-answered thread, name whoever you like in `a`/`b`, and
    /// every one of them gets tapped on the shoulder about a conversation they
    /// cannot open, while the rate limiter sees one quiet old thread.
    ///
    /// So both workers resolve the pair from the THREAD row. Alice writes two
    /// rows here — one in her own thread with Bob, one filed into Bob and
    /// Carol's thread — and both name Carol. Carol must hear about neither.
    #[tokio::test(flavor = "multi_thread")]
    async fn claimed_participants_on_a_dm_notify_the_thread_not_the_claim() {
        let ctx = test_context().await;
        start(ctx.clone());

        let trx = ctx.begin();
        let alice = trx.create(&User { display_name: "Alice".into(), oidc_sub: None }).await.unwrap().id();
        let bob = trx.create(&User { display_name: "Bob".into(), oidc_sub: None }).await.unwrap().id();
        let carol = trx.create(&User { display_name: "Carol".into(), oidc_sub: None }).await.unwrap().id();
        trx.commit().await.unwrap();

        // Two real conversations: Alice with Bob, and Bob with Carol.
        let (ab_a, ab_b) = canonical_pair(alice, bob);
        let (bc_a, bc_b) = canonical_pair(bob, carol);
        let trx = ctx.begin();
        let ab = trx.create(&DmThread { a: ab_a.into(), b: ab_b.into(), created_at: 1, deleted: false }).await.unwrap().id();
        let bc = trx.create(&DmThread { a: bc_a.into(), b: bc_b.into(), created_at: 1, deleted: false }).await.unwrap().id();
        trx.commit().await.unwrap();

        // Honest traffic: Bob writes to Carol, Alice writes to Bob.
        let trx = ctx.begin();
        trx.create(&DmMessage {
            thread: bc.into(),
            a: bc_a.into(),
            b: bc_b.into(),
            user: bob.into(),
            text: "hi carol".into(),
            timestamp: 2,
            deleted: false,
            edited_at: None,
        })
        .await
        .unwrap();
        trx.create(&DmMessage {
            thread: ab.into(),
            a: ab_a.into(),
            b: ab_b.into(),
            user: alice.into(),
            text: "hi bob".into(),
            timestamp: 3,
            deleted: false,
            edited_at: None,
        })
        .await
        .unwrap();
        trx.commit().await.unwrap();

        // The mismatched rows. Both carry Alice+Carol as the pair; neither
        // thread says so. The first is filed in Alice's own thread with Bob, the
        // second in a conversation Alice is not part of at all.
        let (ac_a, ac_b) = canonical_pair(alice, carol);
        let trx = ctx.begin();
        for (thread, text) in [(ab, "filed into my own thread"), (bc, "filed into someone else's")] {
            trx.create(&DmMessage {
                thread: thread.into(),
                a: ac_a.into(),
                b: ac_b.into(),
                user: alice.into(),
                text: text.into(),
                timestamp: 4,
                deleted: false,
                edited_at: None,
            })
            .await
            .unwrap();
        }
        trx.commit().await.unwrap();

        // Generous settle — the workers are asynchronous, so "nothing was
        // created" has to be given time to be wrong.
        tokio::time::sleep(Duration::from_millis(600)).await;
        let notifications = ctx.fetch::<NotificationView>("true").await.unwrap();

        let for_carol: Vec<_> = notifications.iter().filter(|n| n.recipient().unwrap().id() == carol).collect();
        assert_eq!(for_carol.len(), 1, "Carol hears only from the thread she is actually in");
        assert_eq!(
            for_carol[0].actor().unwrap().map(|r| r.id()),
            Some(bob),
            "and that one is from Bob: a claimed pair must never introduce a third member"
        );

        let for_bob: Vec<_> = notifications.iter().filter(|n| n.recipient().unwrap().id() == bob).collect();
        assert_eq!(for_bob.len(), 1, "Bob hears from Alice once — the mismatched row in their thread coalesces, it does not vanish");
        assert_eq!(for_bob[0].actor().unwrap().map(|r| r.id()), Some(alice));

        // Nothing was tombstoned either: the mismatched rows were counted
        // against the threads they were filed in (an old, answered one) or
        // ignored as an outsider's row, never as new conversations Alice was
        // starting.
        let live = ctx.fetch::<DmMessageView>("deleted = false").await.unwrap();
        assert_eq!(live.len(), 4, "a claimed pair does not turn an old conversation into a rate-limited new one");
    }

    /// The rate limiter, end to end on a real node: a sender who opens more
    /// conversations than the window allows has the excess MESSAGE tombstoned,
    /// the thread it opened left intact, and one public `dm-rate-limit`
    /// ModAction row written with no human actor.
    ///
    /// This is the post-hoc shape stated in dm_rate_limit's module docs: the
    /// rows really do commit first, and the test waits for the worker to catch
    /// up — which is exactly what a recipient with the thread open would see.
    /// It also exercises the boot-sweep handshake end to end: the limiter acts
    /// only after `Traffic::BacklogComplete` reaches it, so a marker that never
    /// arrived would leave every message below untouched.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_dm_rate_limiter_tombstones_the_excess_and_logs_one_action() {
        use super::dm_rate_limit::MAX_INITIATIONS_PER_WINDOW;

        let ctx = test_context().await;
        start(ctx.clone());

        let trx = ctx.begin();
        let opener = trx.create(&User { display_name: "Opener".into(), oidc_sub: None }).await.unwrap().id();
        let mut partners = Vec::new();
        for i in 0..(MAX_INITIATIONS_PER_WINDOW + 1) {
            partners.push(trx.create(&User { display_name: format!("Partner {i}"), oidc_sub: None }).await.unwrap().id());
        }
        trx.commit().await.unwrap();

        // One thread per partner, each opened by the same sender, committed one at a
        // time so the worker sees them in order (a single transaction would
        // deliver them as one changeset and the ordering claim would be vague).
        let now = now_ms();
        let mut threads = Vec::new();
        for (i, partner) in partners.iter().enumerate() {
            let (a, b) = canonical_pair(opener, *partner);
            let trx = ctx.begin();
            let thread = trx.create(&DmThread { a: a.into(), b: b.into(), created_at: now, deleted: false }).await.unwrap().id();
            trx.create(&DmMessage {
                thread: thread.into(),
                a: a.into(),
                b: b.into(),
                user: opener.into(),
                text: format!("unsolicited {i}"),
                timestamp: now,
                deleted: false,
                edited_at: None,
            })
            .await
            .unwrap();
            trx.commit().await.unwrap();
            threads.push(thread);
            // Let the worker consume this one before the next commits, so
            // "the excess" is the last thread and not an arbitrary one.
            tokio::time::sleep(Duration::from_millis(120)).await;
        }

        // The limiter's tombstone is what removes rows from `deleted = false`.
        let mut remaining = usize::MAX;
        for _ in 0..200 {
            remaining = ctx.fetch::<DmMessageView>("deleted = false").await.unwrap().len();
            if remaining == MAX_INITIATIONS_PER_WINDOW {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert_eq!(
            remaining, MAX_INITIATIONS_PER_WINDOW,
            "exactly the messages within the limit survive; the one that opened the excess conversation is tombstoned"
        );

        // The penalty is the message, never the conversation: every thread row
        // is still alive, including the one whose only message was tombstoned.
        // Nothing in this codebase writes `deleted` back to false, so a thread
        // tombstone would be a permanent, unrepairable loss of history on what
        // may well be a false positive — and it buys nothing, because a thread
        // with no visible messages does not appear in either sidebar
        // (leptos-app/src/dm_list.rs drops threads with no messages).
        let live_threads = ctx.fetch::<DmThreadView>("deleted = false").await.unwrap();
        assert_eq!(
            live_threads.len(),
            threads.len(),
            "a rate limit tombstones the offending message and leaves every conversation standing"
        );

        // One public audit row, no human actor, naming the sender and nothing
        // about the recipients or the text.
        tokio::time::sleep(Duration::from_millis(300)).await;
        let actions: Vec<ModActionView> = ctx
            .fetch::<ModActionView>("true")
            .await
            .unwrap()
            .into_iter()
            .filter(|m| m.action().map(|a| a == "dm-rate-limit").unwrap_or(false))
            .collect();
        assert_eq!(actions.len(), 1, "a burst produces one ModAction row per sender per window, not one per message");
        assert_eq!(actions[0].actor().unwrap().map(|r| r.id()), None, "nothing human acted");
        assert_eq!(actions[0].user().unwrap().map(|r| r.id()), Some(opener));
        let reason = actions[0].reason().unwrap().unwrap_or_default();
        assert!(reason.contains("rate limit"), "the reason explains itself to a moderator, got: {reason}");
        for partner in &partners {
            assert!(!reason.contains(&partner.to_base64()), "the public log must never name who was messaged");
        }
    }

    /// Two users and a thread between them, for the DM timestamp tests below.
    async fn a_thread_between_two_members(ctx: &Context) -> (EntityId, EntityId, EntityId) {
        let trx = ctx.begin();
        let alice = trx.create(&User { display_name: "Alice".into(), oidc_sub: None }).await.unwrap().id();
        let bob = trx.create(&User { display_name: "Bob".into(), oidc_sub: None }).await.unwrap().id();
        trx.commit().await.unwrap();

        let (a, b) = canonical_pair(alice, bob);
        let trx = ctx.begin();
        let thread = trx.create(&DmThread { a: a.into(), b: b.into(), created_at: now_ms(), deleted: false }).await.unwrap().id();
        trx.commit().await.unwrap();
        (alice, bob, thread)
    }

    async fn send_dm_at(ctx: &Context, thread: EntityId, sender: EntityId, partner: EntityId, timestamp: i64) -> EntityId {
        let (a, b) = canonical_pair(sender, partner);
        let trx = ctx.begin();
        let id = trx
            .create(&DmMessage {
                thread: thread.into(),
                a: a.into(),
                b: b.into(),
                user: sender.into(),
                text: "hello".into(),
                timestamp,
                deleted: false,
                edited_at: None,
            })
            .await
            .unwrap()
            .id();
        trx.commit().await.unwrap();
        id
    }

    /// Push one row through the pipeline's live stage and hand back what
    /// reached the fan-out and the limiter — the same channels
    /// `watch_dms` wires up, driven by hand so a test can stand in for a boot
    /// and for the boot after it.
    ///
    /// Going through `dm_timestamp::run` rather than around it is the point:
    /// nothing in the test decides that the row is settled before the fan-out
    /// sees it, because nothing can. The stage forwards what it has settled.
    async fn pipeline(
        ctx: &Context,
        message: EntityId,
    ) -> (tokio::sync::mpsc::UnboundedReceiver<DmMessageView>, tokio::sync::mpsc::UnboundedReceiver<dm_rate_limit::Traffic>) {
        let view = ctx.get::<DmMessageView>(message).await.unwrap();
        let (stamp_tx, mut stamp_rx) = tokio::sync::mpsc::unbounded_channel::<DmMessageView>();
        let (notify_tx, notify_rx) = tokio::sync::mpsc::unbounded_channel::<DmMessageView>();
        let (limit_tx, limit_rx) = tokio::sync::mpsc::unbounded_channel::<dm_rate_limit::Traffic>();
        stamp_tx.send(view).unwrap();
        drop(stamp_tx);
        dm_timestamp::run(ctx.clone(), &mut stamp_rx, notify_tx, limit_tx).await;
        (notify_rx, limit_rx)
    }

    /// A message dated after the server's clock has its timestamp rewritten ON
    /// THE ROW, and a message dated in the past is left exactly as sent.
    ///
    /// Persisting is the whole point, and it is what the earlier version of
    /// this lane did not do. Each reader compensating privately with
    /// `min(stored, now)` recomputes against the current clock, so the value
    /// moves every time anyone looks: the conversation holds the top of the
    /// recipient's sidebar forever, the unread badge relights after every read,
    /// and the rate limiter re-dates the initiation into the current window on
    /// every restart. A private adjustment also cannot reach the two client
    /// queries that sort by `timestamp` inside the query. One stored number
    /// that stands still answers all of it, so the second half of this test —
    /// that the settled value does not move again — carries as much weight as
    /// the first.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_future_dated_dm_is_rewritten_to_the_server_clock_on_the_row() {
        let ctx = test_context().await;
        start(ctx.clone());

        let (alice, bob, thread) = a_thread_between_two_members(&ctx).await;

        const YEAR_MS: i64 = 365 * 24 * 60 * 60 * 1000;
        let dated_in_2126 = now_ms() + 100 * YEAR_MS;
        let dated_a_minute_ago = now_ms() - 60_000;
        let future_row = send_dm_at(&ctx, thread, alice, bob, dated_in_2126).await;
        let honest_row = send_dm_at(&ctx, thread, alice, bob, dated_a_minute_ago).await;

        let mut stored = dated_in_2126;
        for _ in 0..200 {
            stored = ctx.get::<DmMessageView>(future_row).await.unwrap().timestamp().unwrap();
            if stored <= now_ms() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(stored <= now_ms(), "a timestamp the server clock has not reached is rewritten to the server clock");
        assert!(
            stored >= dated_a_minute_ago,
            "and it is rewritten to WHEN THE SERVER SAW IT, not to zero or to the sender's other messages"
        );

        assert_eq!(
            ctx.get::<DmMessageView>(honest_row).await.unwrap().timestamp().unwrap(),
            dated_a_minute_ago,
            "a timestamp the server clock has already passed is left exactly as sent — back-dating is accepted, not repaired"
        );

        // Every later sight of the row — the worker's own write coming back as
        // an Update, and the fan-out's and limiter's re-deliveries — must write
        // nothing. A value that moved on re-observation would be the same
        // defect one layer down.
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(
            ctx.get::<DmMessageView>(future_row).await.unwrap().timestamp().unwrap(),
            stored,
            "the settled value stands still: seeing the row again writes nothing"
        );
    }

    /// The rate limiter re-reads the pair a thread names for every message
    /// instead of remembering the first answer, so a thread quietly reseated
    /// onto a third member is counted as the new conversation it is.
    ///
    /// `DmThread.a`/`b` are ordinary mutable properties and the write scope
    /// only asks whether the writer is one of them, so a participant's client
    /// can rewrite the OTHER seat — that residual is disclosed on the model
    /// field, and closing it needs an immutable-field rule the policy grammar
    /// does not have. What must not ALSO be true is that the move is free.
    /// While the limiter cached the pair, a reseated thread went on looking
    /// like a conversation between the original two, opened a month ago and
    /// answered — so it cost nothing — while the newly named member began
    /// receiving the fan-out's notifications for it. The history does not travel
    /// with the row: every message already in the thread carries its own copy of
    /// the old pair and the read scope reads that copy off the row, so the
    /// newcomer opens an empty conversation. What the reseater can also hand
    /// over is the messages they wrote themselves, by rewriting `a`/`b` on those
    /// rows too — never the other participant's.
    ///
    /// The limiter is driven by hand here: everything before
    /// `Traffic::BacklogComplete` is counted and not judged, which is the boot
    /// sweep, and the message after it is live traffic.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_reseated_thread_costs_the_sender_an_initiation() {
        use super::dm_rate_limit::{State as LimiterState, Traffic, MAX_INITIATIONS_PER_WINDOW};

        const DAY_MS: i64 = 24 * 60 * 60 * 1000;
        let ctx = test_context().await;

        let trx = ctx.begin();
        let sender = trx.create(&User { display_name: "Sender".into(), oidc_sub: None }).await.unwrap().id();
        let old_partner = trx.create(&User { display_name: "Old partner".into(), oidc_sub: None }).await.unwrap().id();
        let newcomer = trx.create(&User { display_name: "Newcomer".into(), oidc_sub: None }).await.unwrap().id();
        let mut strangers = Vec::new();
        for i in 0..MAX_INITIATIONS_PER_WINDOW {
            strangers.push(trx.create(&User { display_name: format!("Stranger {i}"), oidc_sub: None }).await.unwrap().id());
        }
        trx.commit().await.unwrap();

        // The sender's whole budget, spent today on five strangers.
        let mut backlog = Vec::new();
        for stranger in &strangers {
            let (a, b) = canonical_pair(sender, *stranger);
            let trx = ctx.begin();
            let thread = trx.create(&DmThread { a: a.into(), b: b.into(), created_at: now_ms(), deleted: false }).await.unwrap().id();
            trx.commit().await.unwrap();
            backlog.push(send_dm_at(&ctx, thread, sender, *stranger, now_ms()).await);
        }

        // And one long-standing conversation, opened a month ago and answered:
        // outside the window, exempt from both limits.
        let (a, b) = canonical_pair(sender, old_partner);
        let a_month_ago = now_ms() - 30 * DAY_MS;
        let trx = ctx.begin();
        let settled = trx.create(&DmThread { a: a.into(), b: b.into(), created_at: a_month_ago, deleted: false }).await.unwrap().id();
        trx.commit().await.unwrap();
        backlog.push(send_dm_at(&ctx, settled, sender, old_partner, a_month_ago).await);
        backlog.push(send_dm_at(&ctx, settled, old_partner, sender, a_month_ago + 60_000).await);

        // Boot: the limiter builds its picture, judges none of it, and is
        // enforcing once the marker has gone through.
        let state = std::sync::Arc::new(tokio::sync::Mutex::new(LimiterState::default()));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Traffic>();
        for message in &backlog {
            tx.send(Traffic::Message(ctx.get::<DmMessageView>(*message).await.unwrap())).unwrap();
        }
        tx.send(Traffic::BacklogComplete).unwrap();
        drop(tx);
        dm_rate_limit::run(ctx.clone(), state.clone(), &mut rx).await;
        assert_eq!(
            ctx.fetch::<DmMessageView>("deleted = false").await.unwrap().len(),
            backlog.len(),
            "nothing is tombstoned yet: five conversations is the budget, and the sixth is a month old"
        );

        // The sender rewrites the settled thread's OTHER seat onto a member who
        // was never part of it, then writes into it.
        let settled_view = ctx.get::<DmThreadView>(settled).await.unwrap();
        let trx = ctx.begin();
        let editable = settled_view.edit(&trx).unwrap();
        if a == sender {
            editable.b().set(&newcomer.into()).unwrap();
        } else {
            editable.a().set(&newcomer.into()).unwrap();
        }
        trx.commit().await.unwrap();
        let after_reseat = send_dm_at(&ctx, settled, sender, newcomer, now_ms()).await;

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Traffic>();
        tx.send(Traffic::Message(ctx.get::<DmMessageView>(after_reseat).await.unwrap())).unwrap();
        drop(tx);
        dm_rate_limit::run(ctx.clone(), state, &mut rx).await;

        assert!(
            ctx.get::<DmMessageView>(after_reseat).await.unwrap().deleted().unwrap(),
            "the reseated thread is a sixth conversation started today, so the message into it is tombstoned"
        );
        let actions: Vec<ModActionView> = ctx
            .fetch::<ModActionView>("true")
            .await
            .unwrap()
            .into_iter()
            .filter(|m| m.action().map(|a| a == "dm-rate-limit").unwrap_or(false))
            .collect();
        assert_eq!(actions.len(), 1, "and the tombstone has its one public trace");
        assert_eq!(actions[0].user().unwrap().map(|r| r.id()), Some(sender));
    }

    /// A DM notification the recipient has already read is not reissued when
    /// the server restarts, even for a message its sender dated in the future.
    ///
    /// The fan-out has no message id to key on (`Notification` carries no DM
    /// slot), so its restart probe asks whether a row exists that is at least
    /// as new as the message. That test only holds if the message's timestamp
    /// is the same number on the second boot as on the first — which is what
    /// storing the settled value buys. When the fan-out clamped privately
    /// instead, the message re-dated itself to the new "now" on every boot, no
    /// stored `created_at` could satisfy the probe, and last month's
    /// conversation announced itself again on every restart.
    ///
    /// The workers are driven by hand here rather than through [`start`],
    /// because a second `run` with a fresh delivered-cache IS what a process
    /// restart looks like from the fan-out's seat. What this test no longer has
    /// to arrange is the ORDER: the fan-out is fed from the timestamp stage's
    /// own output, so a row reaching it unsettled is not a case a test can set
    /// up. `a_live_future_dated_dm_is_settled_before_it_is_announced` is where
    /// that ordering is checked on the live path.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_restart_does_not_reissue_a_seen_notification_for_a_future_dated_dm() {
        let ctx = test_context().await;
        let (alice, bob, thread) = a_thread_between_two_members(&ctx).await;

        const YEAR_MS: i64 = 365 * 24 * 60 * 60 * 1000;
        let message = send_dm_at(&ctx, thread, alice, bob, now_ms() + 10 * YEAR_MS).await;

        // First boot: the row goes through the timestamp stage, which settles
        // it and passes it to the fan-out, which tells Bob about it.
        let (mut notify_rx, _limit_rx) = pipeline(&ctx, message).await;
        let stored = ctx.get::<DmMessageView>(message).await.unwrap().timestamp().unwrap();
        assert!(stored <= now_ms(), "the row is honest before the fan-out reads it");

        dm_notify::run(ctx.clone(), &mut notify_rx).await;
        let inbox = ctx.fetch::<NotificationView>("true").await.unwrap();
        assert_eq!(inbox.len(), 1, "Bob is told once");
        assert_eq!(inbox[0].recipient().unwrap().id(), bob);

        // Bob reads it.
        let trx = ctx.begin();
        inbox[0].edit(&trx).unwrap().seen().set(&true).unwrap();
        trx.commit().await.unwrap();

        // Second boot: the whole backlog replays, this message included.
        let (mut notify_rx, _limit_rx) = pipeline(&ctx, message).await;
        dm_notify::run(ctx.clone(), &mut notify_rx).await;
        assert_eq!(
            ctx.fetch::<NotificationView>("true").await.unwrap().len(),
            1,
            "a restart must not announce a DM the recipient has already read"
        );
    }

    /// The same rule as the test above, on the LIVE path and through [`start`]:
    /// a DM sent while the workers are running is settled before it is
    /// announced, and reading the notification survives a restart.
    ///
    /// This is the ordering the previous shape could not promise. With the
    /// timestamp worker running as a third parallel consumer, the fan-out could
    /// sample `now` for the inbox row before the settling write sampled its own
    /// clock, storing `created_at` EARLIER than the timestamp the message ended
    /// up with. Nothing repaired it: the settling write comes back as an Update,
    /// and the fan-out's delivered cache answers before the probe runs. The
    /// damage showed up one restart later — the recipient marks the row seen, the
    /// server restarts with an empty cache, the probe finds neither an unseen
    /// row nor a `created_at` at or after the message, and the same DM announces
    /// itself a second time.
    ///
    /// So the assertion that matters is the comparison between the two stored
    /// numbers, and the second boot is what makes a violation of it visible.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_live_future_dated_dm_is_settled_before_it_is_announced() {
        let ctx = test_context().await;
        start(ctx.clone());

        let (alice, bob, thread) = a_thread_between_two_members(&ctx).await;

        // Sent with the workers already running, so this row travels the live
        // path rather than the boot sweep.
        const YEAR_MS: i64 = 365 * 24 * 60 * 60 * 1000;
        let message = send_dm_at(&ctx, thread, alice, bob, now_ms() + 10 * YEAR_MS).await;

        let notification = wait_for_first_notification(&ctx).await;
        assert_eq!(notification.recipient().unwrap().id(), bob);
        let stored = ctx.get::<DmMessageView>(message).await.unwrap().timestamp().unwrap();
        assert!(stored <= now_ms(), "the live row is settled, not left dated ten years out");
        assert!(
            notification.created_at().unwrap() >= stored,
            "the inbox row is stamped after the message's settled send time, which is what the restart probe compares"
        );

        // Bob reads it, and the server restarts: fresh workers, fresh caches,
        // the whole backlog replayed.
        let trx = ctx.begin();
        notification.edit(&trx).unwrap().seen().set(&true).unwrap();
        trx.commit().await.unwrap();
        start(ctx.clone());

        tokio::time::sleep(Duration::from_millis(700)).await;
        assert_eq!(
            ctx.fetch::<NotificationView>("true").await.unwrap().len(),
            1,
            "a restart must not announce a DM the recipient has already read"
        );
    }

    /// A tombstoned DM is settled like any other row, and goes no further.
    ///
    /// Settled, because a tombstone does not make a future-dated send time
    /// harmless: the thread view keeps tombstones in the timeline and orders by
    /// `timestamp` (`leptos-app/src/dm_chat.rs`), so an unsettled one floats at
    /// the top of the conversation for good — and the sender can still rewrite
    /// that field, the write scope being unchanged by the flag. Before this, the
    /// standing query read `deleted = false` and such a row was never seen at
    /// all.
    ///
    /// No further, because widening the query must not widen what the other two
    /// consumers act on. The recipient of a tombstoned DM is told nothing, and
    /// the limiter treats it as history — exactly what the old predicate bought,
    /// now bought at the forward step instead.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_tombstoned_future_dated_dm_is_settled_and_passed_no_further() {
        let ctx = test_context().await;
        let (alice, bob, thread) = a_thread_between_two_members(&ctx).await;

        const YEAR_MS: i64 = 365 * 24 * 60 * 60 * 1000;
        let claimed = now_ms() + 10 * YEAR_MS;
        let message = send_dm_at(&ctx, thread, alice, bob, claimed).await;
        let trx = ctx.begin();
        ctx.get::<DmMessageView>(message).await.unwrap().edit(&trx).unwrap().deleted().set(&true).unwrap();
        trx.commit().await.unwrap();

        // Boot with the row already tombstoned, so the boot sweep is what has
        // to find it.
        start(ctx.clone());

        let mut stored = claimed;
        for _ in 0..200 {
            stored = ctx.get::<DmMessageView>(message).await.unwrap().timestamp().unwrap();
            if stored <= now_ms() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(stored <= now_ms(), "a tombstoned row's future-dated send time is settled like any other");

        let row = ctx.get::<DmMessageView>(message).await.unwrap();
        assert!(row.deleted().unwrap(), "and settling it does not resurrect it");
        assert!(
            ctx.fetch::<NotificationView>("true").await.unwrap().is_empty(),
            "a tombstoned DM tells nobody about itself, whatever it is dated"
        );
        let actions: Vec<ModActionView> = ctx
            .fetch::<ModActionView>("true")
            .await
            .unwrap()
            .into_iter()
            .filter(|m| m.action().map(|a| a == "dm-rate-limit").unwrap_or(false))
            .collect();
        assert!(actions.is_empty(), "and it is not traffic the limiter judges");
    }

    /// A row whose send time could not be settled goes to the rate limiter and
    /// NOT to the fan-out.
    ///
    /// The fan-out half is the one that matters. `settle` writes nothing at all
    /// for a row already at or before the server clock, so a row that failed to
    /// settle is either dated in the future or too broken to read a timestamp
    /// off — and on the first of those the fan-out would stamp an inbox row from
    /// its own clock against a send time still claiming next century, then never
    /// revisit it, because its delivered cache answers for a message it has
    /// handled. That is the duplicate-after-restart defect this lane already
    /// fixed once, and a failed write must not be a way back into it. Waiting is
    /// cheap: the row returns on its next change or the next boot sweep.
    ///
    /// The limiter half is the deliberate asymmetry. Withholding traffic from it
    /// is the expensive mistake — an uncounted message is budget a bulk sender
    /// did not spend — and its own ceiling under the server clock is what makes
    /// an unsettled value cost it nothing.
    ///
    /// The forward step is called directly here because nothing in this test
    /// setup can make a commit fail: the workers run on a Root context over a
    /// local sled node, and there is no fault-injection seam to reach through.
    /// So the decision is pinned where it is made.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_row_that_could_not_be_settled_is_counted_but_not_announced() {
        let ctx = test_context().await;
        let (alice, bob, thread) = a_thread_between_two_members(&ctx).await;
        let view = ctx.get::<DmMessageView>(send_dm_at(&ctx, thread, alice, bob, now_ms()).await).await.unwrap();

        let (notify_tx, mut notify_rx) = tokio::sync::mpsc::unbounded_channel::<DmMessageView>();
        let (limit_tx, mut limit_rx) = tokio::sync::mpsc::unbounded_channel::<dm_rate_limit::Traffic>();

        dm_timestamp::forward(&view, false, &notify_tx, &limit_tx);
        assert!(notify_rx.try_recv().is_err(), "a row nobody could settle must not be announced to its recipient");
        assert!(
            matches!(limit_rx.try_recv(), Ok(dm_rate_limit::Traffic::Message(m)) if m.id() == view.id()),
            "but it is still traffic, and the limiter counts it"
        );

        // And the ordinary case, so the assertions above are read as a
        // difference rather than as an empty channel proving nothing.
        dm_timestamp::forward(&view, true, &notify_tx, &limit_tx);
        assert!(
            matches!(notify_rx.try_recv(), Ok(m) if m.id() == view.id()),
            "a settled row goes to both"
        );
        assert!(matches!(limit_rx.try_recv(), Ok(dm_rate_limit::Traffic::Message(m)) if m.id() == view.id()));
    }
}
