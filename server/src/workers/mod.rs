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
use community_model::MessageView;
use futures_util::future::BoxFuture;
use futures_util::FutureExt;
use tokio::sync::mpsc::UnboundedReceiver;
use tracing::{error, info};

/// Start the worker subsystem on the durable node's privileged (Root)
/// context. Fire-and-forget from `main`: failures to start are fatal-logged
/// (the server keeps serving chat; derived data just goes stale), and the
/// task never returns otherwise.
pub fn start(ctx: Context) {
    tokio::spawn(async move {
        if let Err(e) = watch_messages(ctx).await {
            error!("message workers failed to start: {e:#}");
        }
    });
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
fn supervise(
    name: &'static str,
    mut rx: UnboundedReceiver<MessageView>,
    run: impl for<'a> Fn(&'a mut UnboundedReceiver<MessageView>) -> BoxFuture<'a, ()> + Send + 'static,
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
    use community_model::{LinkPreview, LinkPreviewView, Message, NotificationView, Room, User};
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
}
