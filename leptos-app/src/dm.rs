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
//! 1. participants are stored in [`community_model::canonical_pair`] order and
//!    looked up in BOTH orders, so neither side can miss a thread the other
//!    created — not even one written with the pair reversed, which policy has
//!    no way to refuse (see [`find_or_create_thread`]);
//! 2. when the query does return more than one, every reader picks the same one
//!    — [`community_model::canonical_thread`], the lowest entity id — and posts
//!    there;
//! 3. an open thread view re-resolves itself whenever the thread set changes
//!    ([`converge_selection`]), so a client that opened the twin during the race
//!    window slides onto the winner without the reader noticing;
//! 4. and every view that READS a conversation reads all of the pair's rows,
//!    not just the winner ([`Conversation`], [`pair_rows`]) — the twin keeps
//!    whatever landed in it during the race, and agreeing on where to write
//!    next must not make what was already written unreachable.
//!
//! `server/tests/dm_policy_live_tests.rs` pins the convergence at the storage
//! level against the same `canonical_thread` this module calls.

use ankurah::{model::Mutable, EntityId, LiveQuery};
use ankurah_signals::{Get as AnkurahGet, Peek};
use community_model::{
    canonical_pair, canonical_thread, dm_partner, DmMessage, DmThread, DmThreadView, UserView, THREADS_FOR_PAIR,
};
use leptos::prelude::*;

use crate::{ctx, current_user_id, queries};

/// The viewer's threads, live. Scoped by policy to threads the viewer is in —
/// the resultset is self-shaping, so no client-side membership filter is needed
/// (and one would be a lie about where enforcement lives). Tombstoned threads
/// are excluded, though nothing writes that flag today: the DM rate limiter
/// tombstones the offending message and leaves the conversation standing (see
/// `DmThread::deleted` and docs/moderation.md).
pub fn threads_query() -> LiveQuery<DmThreadView> {
    ctx().query::<DmThreadView>("deleted = false").expect("failed to create DmThreadView LiveQuery")
}

/// One conversation per correspondent, as the UI has to treat it: the row
/// every reader agrees to call THE thread for that pair, plus every row the
/// pair has.
///
/// The extra rows are the losers of a first-DM race, and they are not inert.
/// Whoever wrote into one before the race resolved left their message THERE,
/// and no later message joins it. A view that reads only the agreed row can
/// therefore show an empty conversation — or hide it from the sidebar
/// entirely, since a thread with no messages is not listed — while the words
/// sit one row over. So activity, unread counts and the message timeline are
/// all read across `rows`; only what a click selects, and where a new message
/// is written, is [`Conversation::canonical`].
#[derive(Clone)]
pub struct Conversation {
    /// The lowest entity id for the pair — the row every client converges on.
    pub canonical: DmThreadView,
    /// Every row for the pair, canonical first, in id order.
    pub rows: Vec<EntityId>,
}

/// Group the viewer's threads by correspondent. Threads are keyed by their
/// participant pair rather than by their id, which is what makes duplicates
/// from a race collapse into one sidebar row — and the pair is canonicalized
/// on the way in, so a row stored in the reversed order (which policy permits;
/// see [`find_or_create_thread`]) groups with its twin rather than beside it.
pub fn conversations(threads: &[DmThreadView]) -> Vec<Conversation> {
    let mut by_pair: std::collections::HashMap<(EntityId, EntityId), Vec<DmThreadView>> = std::collections::HashMap::new();
    for thread in threads {
        let (Ok(a), Ok(b)) = (thread.a(), thread.b()) else { continue };
        by_pair.entry(canonical_pair(a.id(), b.id())).or_default().push(thread.clone());
    }
    let mut conversations: Vec<Conversation> = by_pair
        .into_values()
        .filter_map(|mut rows| {
            rows.sort_by_key(|t| t.id());
            let canonical = rows.first()?.clone();
            Some(Conversation { canonical, rows: rows.iter().map(|t| t.id()).collect() })
        })
        .collect();
    // Stable order for the caller to re-sort; ids are ULIDs, so this is
    // oldest-thread-first until the sidebar sorts by recent activity.
    conversations.sort_by_key(|c| c.canonical.id());
    conversations
}

/// Every thread row belonging to the same pair as `thread`, including it.
///
/// What it is for: any view opened on one row of a raced pair has to read the
/// whole pair, or the messages that landed in the other row are unreachable
/// (see [`Conversation`]).
pub fn pair_rows(threads: &[DmThreadView], thread: &DmThreadView) -> Vec<EntityId> {
    let (Ok(a), Ok(b)) = (thread.a(), thread.b()) else { return vec![thread.id()] };
    let pair = canonical_pair(a.id(), b.id());
    let mut rows: Vec<EntityId> = threads
        .iter()
        .filter(|t| {
            let (Ok(ta), Ok(tb)) = (t.a(), t.b()) else { return false };
            canonical_pair(ta.id(), tb.id()) == pair
        })
        .map(|t| t.id())
        .collect();
    if !rows.contains(&thread.id()) {
        // The open selection is not in the live thread set. Usually that means
        // the set has not caught up yet — `open_thread_with` selects a row from
        // its own fetch — but it equally covers a selected row that has since
        // left the set, which `deleted = false` would do the day anything
        // tombstones a thread. Either way, the row the reader has open belongs
        // in what the reader is shown.
        rows.push(thread.id());
    }
    rows.sort();
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
    //
    // The lookup asks about both orderings, because the model cannot insist on
    // one — see `community_model::THREADS_FOR_PAIR`, where the source lives so
    // that a test can prove it parses (no test CI runs compiles this file).
    //
    // `deleted = false` is safe only while nothing tombstones threads, which is
    // today's ruling (see `DmThread::deleted`). The day something does, this
    // line stops finding the pair's thread and mints a second one beside it,
    // stranding the history in a row neither participant can reach again — so
    // whoever adds a thread tombstone owes this call an adoption path.
    let selection = queries::selection(
        &format!("{THREADS_FOR_PAIR} AND deleted = false"),
        [(&a).into(), (&b).into(), (&b).into(), (&a).into()],
    )?;
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
