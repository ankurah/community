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

pub mod mentions;
pub mod og;
pub mod ssrf;
pub mod unfurl;

use std::collections::HashMap;

use ankurah::changes::{ChangeSet, ItemChange};
use ankurah::signals::{Peek, Subscribe};
use ankurah::{Context, EntityId, LiveQuery};
use anyhow::Result;
use community_model::MessageView;
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
                        // send() only fails if a consumer died, which is
                        // already fatal-logged by the consumer itself.
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

    tokio::spawn(mentions::run(ctx.clone(), mention_rx));
    tokio::spawn(unfurl::run(ctx.clone(), unfurl_rx));

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
