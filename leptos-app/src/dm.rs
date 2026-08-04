//! Direct messages (#30): thread selection, race-safe find-or-create, and the
//! send path.
//!
//! The one interesting problem here is that a two-party thread has no owner who
//! could allocate it. Both participants open it from their own client, so two
//! people (or two tabs of one person) can both find no thread and both create
//! one, and ankurah 0.9.0 has no entity deletion to clean the twin up with.
//!
//! The whole answer is agreement rather than prevention:
//!
//! 1. participants are stored in [`community_model::canonical_pair`] order, so
//!    both sides build the identical `a = ? AND b = ?` query and neither can
//!    miss a thread the other created;
//! 2. when the query does return more than one, every reader picks the same one
//!    — [`community_model::canonical_thread`], the lowest entity id — and posts
//!    there;
//! 3. an open thread view re-resolves itself whenever the thread set changes
//!    ([`converge_selection`]), so a client that opened the twin during the race
//!    window slides onto the winner without the reader noticing.
//!
//! The twin keeps whatever landed in it during the race (a first message,
//! usually) and stops collecting traffic. `server/tests/dm_policy_live_tests.rs`
//! pins the convergence at the storage level against the same
//! `canonical_thread` this module calls.

use ankurah::{model::Mutable, EntityId, LiveQuery};
use ankurah_signals::{Get as AnkurahGet, Peek};
use community_model::{canonical_pair, canonical_thread, dm_partner, DmMessage, DmThread, DmThreadView, UserView};
use leptos::prelude::*;

use crate::{ctx, current_user_id, queries};

/// The viewer's threads, live. Scoped by policy to threads the viewer is in —
/// the resultset is self-shaping, so no client-side membership filter is needed
/// (and one would be a lie about where enforcement lives). Tombstoned threads
/// (the DM rate limiter's post-hoc action) are excluded.
pub fn threads_query() -> LiveQuery<DmThreadView> {
    ctx().query::<DmThreadView>("deleted = false").expect("failed to create DmThreadView LiveQuery")
}

/// One row per correspondent: duplicates from a first-DM race collapse to the
/// canonical thread, so the sidebar never shows the same person twice.
///
/// Threads are keyed by their participant pair rather than by their id, which
/// is exactly what makes the collapse possible.
pub fn canonical_threads(threads: &[DmThreadView]) -> Vec<DmThreadView> {
    let mut by_pair: std::collections::HashMap<(EntityId, EntityId), DmThreadView> = std::collections::HashMap::new();
    for thread in threads {
        let (Ok(a), Ok(b)) = (thread.a(), thread.b()) else { continue };
        let key = canonical_pair(a.id(), b.id());
        match by_pair.get(&key) {
            Some(existing) if existing.id() <= thread.id() => {}
            _ => {
                by_pair.insert(key, thread.clone());
            }
        }
    }
    let mut rows: Vec<DmThreadView> = by_pair.into_values().collect();
    // Stable order for the caller to re-sort; ids are ULIDs, so this is
    // oldest-thread-first until the sidebar sorts by recent activity.
    rows.sort_by_key(|t| t.id());
    rows
}

/// The other participant of a thread, from the viewer's seat.
pub fn partner_of(thread: &DmThreadView, viewer: EntityId) -> Option<EntityId> {
    let (a, b) = (thread.a().ok()?.id(), thread.b().ok()?.id());
    dm_partner(a, b, viewer)
}

/// Keep an open thread pointed at the canonical thread for its pair.
///
/// Install once per app: whenever the thread set changes, a selection sitting
/// on a race twin slides onto the winner. Without this, two tabs that opened
/// the same correspondent concurrently would each keep talking into their own
/// row — the messages would all be readable, but the conversation would look
/// like it had forked.
pub fn converge_selection(threads: LiveQuery<DmThreadView>, selected: RwSignal<Option<DmThreadView>>) {
    Effect::new(move |_| {
        let all = threads.get();
        let Some(current) = selected.get() else { return };
        let (Ok(a), Ok(b)) = (current.a(), current.b()) else { return };
        let pair = canonical_pair(a.id(), b.id());
        let candidates: Vec<EntityId> = all
            .iter()
            .filter(|t| {
                let (Ok(ta), Ok(tb)) = (t.a(), t.b()) else { return false };
                canonical_pair(ta.id(), tb.id()) == pair
            })
            .map(|t| t.id())
            .collect();
        let Some(winner) = canonical_thread(candidates) else { return };
        if winner != current.id()
            && let Some(row) = all.iter().find(|t| t.id() == winner)
        {
            tracing::info!("DM thread race resolved: moving from {} to {}", current.id().to_base64(), winner.to_base64());
            selected.set(Some(row.clone()));
        }
    });
}

/// Open the thread with `partner`, creating it if this is the first DM.
///
/// Race-safe by construction rather than by locking: the query is on the
/// canonical pair, so it sees any thread the other side already created, and a
/// twin created in the same instant is resolved by [`converge_selection`] as
/// soon as it syncs. Fire-and-forget from a click handler; failures are logged
/// and leave the selection untouched.
pub fn open_thread_with(partner: EntityId, selected: RwSignal<Option<DmThreadView>>) {
    let me = current_user_id();
    if partner == me {
        // The UI does not offer this (no "Message" button on your own card),
        // and a self-thread has no other participant to notify or name.
        tracing::warn!("refusing to open a DM thread with yourself");
        return;
    }
    wasm_bindgen_futures::spawn_local(async move {
        match find_or_create_thread(me, partner).await {
            Ok(thread) => selected.set(Some(thread)),
            Err(e) => tracing::error!("Failed to open DM thread: {}", e),
        }
    });
}

async fn find_or_create_thread(me: EntityId, partner: EntityId) -> Result<DmThreadView, Box<dyn std::error::Error>> {
    let (a, b) = canonical_pair(me, partner);

    // Parameterized, never spliced (#17). Both participants build this exact
    // query, which is what makes find-or-create converge.
    let selection = queries::selection("a = ? AND b = ? AND deleted = false", [(&a).into(), (&b).into()])?;
    let existing = ctx().fetch::<DmThreadView>(selection).await?;
    if let Some(winner) = canonical_thread(existing.iter().map(|t| t.id()))
        && let Some(row) = existing.into_iter().find(|t| t.id() == winner)
    {
        return Ok(row);
    }

    let trx = ctx().begin();
    let created = trx
        .create(&DmThread { a: a.into(), b: b.into(), created_at: js_sys::Date::now() as i64, deleted: false })
        .await?
        .read();
    trx.commit().await?;
    Ok(created)
}

/// Send a DM into `thread`.
///
/// `a`/`b` are copied verbatim from the thread — they are what lets the policy
/// read scope answer "may this user see me" from the row alone — and `user` is
/// the sender, which the write scope pins to the caller anyway. `text` is
/// already wire-encoded by the composer (`@Name` runs re-encoded to `<@id>`
/// tokens, #56), the same bytes a room message would carry: DM text renders
/// mentions, and the server deliberately does NOT fan them out (see
/// server/src/workers/dm_notify.rs).
pub async fn send_dm(thread: &DmThreadView, sender: &UserView, wire_text: String) -> Result<(), Box<dyn std::error::Error>> {
    let a = thread.a()?;
    let b = thread.b()?;
    let trx = ctx().begin();
    trx.create(&DmMessage {
        thread: ankurah::Ref::from(thread),
        a,
        b,
        user: ankurah::Ref::from(sender),
        text: wire_text,
        timestamp: js_sys::Date::now() as i64,
        deleted: false,
        edited_at: None,
    })
    .await?;
    trx.commit().await?;
    Ok(())
}

/// Resolve a correspondent's display name from a users resultset, live.
pub fn display_name(users: &LiveQuery<UserView>, who: EntityId) -> String {
    users
        .peek()
        .iter()
        .find(|u| u.id() == who)
        .and_then(|u| u.display_name().ok())
        .filter(|n| !n.trim().is_empty())
        .unwrap_or_else(|| "Unknown".to_string())
}
