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
use tracing::{error, info};

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
/// LiveQuery feeding two consumers, exactly the shape [`watch_messages`] uses
/// for room messages.
///
/// It is a SEPARATE query and separate consumers on purpose, not a branch
/// inside the room-message pipeline. The mention fan-out must never run on DM
/// text — a third party named inside a private thread cannot read it and must
/// not be told it exists — and the surest way to guarantee that is for the DM
/// stream never to reach the mention worker at all. `dm_notify` explains the
/// rule; this is the structure that enforces it.
///
/// Cost: the query holds every non-tombstoned `dm_message` in memory on the
/// durable node, the same posture as the room-message query above (`deleted =
/// false` over the whole collection). The rate limiter needs a full history to
/// rebuild its window after a restart, so this is load-bearing rather than
/// incidental; if DM volume ever outgrows it, the replacement is a bounded
/// recent-window query plus persisted counters, not a smaller sweep.
async fn watch_dms(ctx: Context) -> Result<()> {
    let (notify_tx, notify_rx) = tokio::sync::mpsc::unbounded_channel::<DmMessageView>();
    // The limiter's channel carries the boot sweep's end marker as well as the
    // rows: it counts the backlog but does not judge it (see dm_rate_limit).
    let (limit_tx, limit_rx) = tokio::sync::mpsc::unbounded_channel::<dm_rate_limit::Traffic>();

    // Tombstoned DMs produce no notifications, and the limiter treats them as
    // history — the predicate keeps them out of the stream entirely. Note the
    // limiter's own tombstones therefore arrive as Removes, which it ignores:
    // enforcing on a row it just tombstoned would loop.
    let live: LiveQuery<DmMessageView> = ctx.query("deleted = false")?;

    let subscription_guard = {
        let notify_tx = notify_tx.clone();
        let limit_tx = limit_tx.clone();
        live.subscribe(move |changeset: ChangeSet<DmMessageView>| {
            for change in &changeset.changes {
                match change {
                    ItemChange::Add { item, .. } | ItemChange::Update { item, .. } => {
                        let _ = notify_tx.send(item.clone());
                        let _ = limit_tx.send(dm_rate_limit::Traffic::Message(item.clone()));
                    }
                    ItemChange::Initial { .. } | ItemChange::Remove { .. } => {}
                }
            }
        })
    };

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
    for msg in backlog {
        let _ = notify_tx.send(msg.clone());
        let _ = limit_tx.send(dm_rate_limit::Traffic::Message(msg));
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
}
