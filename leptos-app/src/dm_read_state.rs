//! Per-thread read cursors and unread badges for DMs (#30) — the `ReadState`
//! pattern of `read_state.rs`, keyed on a thread instead of a room.
//!
//! Same contract, restated because the types differ all the way down (a
//! `DmReadState` row names a `thread`, its windows page `DmMessageView`, and
//! its upsert builds a different struct — the generated View types share no
//! trait that would let one manager serve both). The shape is a known
//! duplication and a named candidate for the ankurah-chat extraction (#46),
//! which is where room chat and DM chat are supposed to converge; it is
//! deliberately NOT unified here, because doing it properly means a
//! read-cursor abstraction in the shared crate rather than a generic bolted
//! onto this app.
//!
//! Unread semantics, stated once: a thread's badge counts messages in its
//! newest-10 window that are newer than the viewer's cursor AND authored by
//! the other participant. Your own messages are read by definition, so sending
//! never lights up your own badge, and the count caps at 10 (rendered "10+"),
//! exactly like room badges.
//!
//! Inherited with that shape rather than introduced here, and deliberately not
//! fixed in the DM lane: the window is the thread's newest ten messages
//! whoever wrote them, so a badge reads low when the viewer's own replies fill
//! it. Room badges undercount the same way for the same reason (#62). The
//! number is an at-a-glance signal that someone is waiting, not an accounting
//! of how many times they said so.
//!
//! Privacy note that does not apply to rooms: these rows are scoped to their
//! OWNER (`user = $jwt.sub`), not to the thread's participant pair, so a read
//! cursor is never a read receipt the correspondent can see. See the
//! `DmReadState` model doc.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use ankurah::{changes::ChangeSet, EntityId, LiveQuery};
use ankurah_signals::{Get, Mut, Peek, Subscribe, SubscriptionGuard};
use community_model::{DmMessageView, DmReadState, DmReadStateView, DmThreadView};
use send_wrapper::SendWrapper;
use wasm_bindgen_futures::spawn_local;

use crate::{ctx, queries};

/// This client's clock, in the project's ms-since-epoch unit.
fn now_ms() -> i64 { js_sys::Date::now() as i64 }

/// A message's timestamp as the badges and the sidebar order use it: exactly
/// as stored, with no adjustment here.
///
/// Timestamps are written by whichever client sent the message, so "newest" is
/// a claim — but it is a claim the server has already settled. A row dated
/// later than the server's clock is rewritten to that clock and committed
/// (`server/src/workers/dm_timestamp.rs`), so what arrives here is honest.
///
/// This must NOT go back to `min(now_ms())`. That version was recomputed on
/// every render, so a future-dated message evaluated to the current instant
/// every time: its conversation held the top of the sidebar permanently, and
/// its unread badge relit after every read, because `mark_read` pins the
/// cursor at the moment of reading and the message stamped later than that on
/// the next recompute. It also could not reach the ordering at all — the
/// window query below sorts by `timestamp` inside the query.
///
/// Between a future-dated message arriving and the server settling it, this
/// client renders the claimed value. That transient is the accepted cost of
/// the server owning the number; see that worker's module doc.
fn stamp_of(message: &DmMessageView) -> Option<i64> { message.timestamp().ok() }

#[derive(Clone)]
pub struct DmReadStateManager(SendWrapper<Arc<Inner>>);

struct Inner {
    user_id: EntityId,
    /// The viewer's own DmReadState rows, live.
    cursors: LiveQuery<DmReadStateView>,
    /// thread id (base64) → effective read cursor (server rows merged with
    /// optimistic local advances; always the max of the two).
    last_read: Mut<HashMap<String, i64>>,
    /// thread id → newest cursor value confirmed written to a row.
    flushed: Mutex<HashMap<String, i64>>,
    /// Threads with an upsert in flight (coalesces write bursts).
    in_flight: Mutex<HashSet<String>>,
    /// thread id → id of the row this client created, so a second upsert
    /// racing the LiveQuery round-trip edits that row instead of twinning it.
    row_ids: Mutex<HashMap<String, EntityId>>,
    /// Cursor rows with a repair write in flight (see [`Inner::heal_cursor`]),
    /// so the changeset that repair produces does not start another one.
    healing: Mutex<HashSet<EntityId>>,
    /// thread id → unread count within its window.
    unread: Mut<HashMap<String, usize>>,
    /// thread id → newest message timestamp in its window, for sidebar
    /// ordering (most recently active conversation first).
    newest: Mut<HashMap<String, i64>>,
    windows: Mutex<HashMap<String, ThreadWindow>>,
    /// False until the viewer's cursor rows have arrived once; badges render
    /// as zero before that instead of flashing "everything unread".
    ready: Mut<bool>,
    _threads_guard: Mutex<Option<SubscriptionGuard>>,
    _cursors_guard: Mutex<Option<SubscriptionGuard>>,
}

struct ThreadWindow {
    thread_id: EntityId,
    query: LiveQuery<DmMessageView>,
    _guard: SubscriptionGuard,
}

impl DmReadStateManager {
    pub fn new(threads: LiveQuery<DmThreadView>, user_id: EntityId) -> Self {
        let cursors = ctx()
            .query::<DmReadStateView>(
                queries::selection("user = ?", [(&user_id).into()]).expect("static dmreadstate selection parses"),
            )
            .expect("failed to create DmReadStateView LiveQuery");

        let inner = Arc::new(Inner {
            user_id,
            cursors: cursors.clone(),
            last_read: Mut::new(HashMap::new()),
            flushed: Mutex::new(HashMap::new()),
            in_flight: Mutex::new(HashSet::new()),
            row_ids: Mutex::new(HashMap::new()),
            healing: Mutex::new(HashSet::new()),
            unread: Mut::new(HashMap::new()),
            newest: Mut::new(HashMap::new()),
            windows: Mutex::new(HashMap::new()),
            ready: Mut::new(false),
            _threads_guard: Mutex::new(None),
            _cursors_guard: Mutex::new(None),
        });

        let inner_for_cursors = inner.clone();
        let cursors_guard = cursors.subscribe(move |_: ChangeSet<DmReadStateView>| {
            Self::rebuild_cursors(&inner_for_cursors);
            if !inner_for_cursors.ready.peek() {
                inner_for_cursors.ready.set(true);
            }
            Self::recompute_all(&inner_for_cursors);
        });
        *inner._cursors_guard.lock().unwrap() = Some(cursors_guard);

        // One newest-messages window per thread, following the threads query.
        let inner_for_threads = inner.clone();
        let threads_guard = threads.subscribe(move |changeset: ChangeSet<DmThreadView>| {
            for thread in changeset.appeared() {
                Self::add_window(&inner_for_threads, thread.id());
            }
            for thread in changeset.removed() {
                let key = thread.id().to_base64();
                inner_for_threads.windows.lock().unwrap().remove(&key);
                let mut unread = inner_for_threads.unread.peek().clone();
                if unread.remove(&key).is_some() {
                    inner_for_threads.unread.set(unread);
                }
                let mut newest = inner_for_threads.newest.peek().clone();
                if newest.remove(&key).is_some() {
                    inner_for_threads.newest.set(newest);
                }
            }
        });
        *inner._threads_guard.lock().unwrap() = Some(threads_guard);

        // The subscription only reports CHANGES, so a resultset that arrived
        // before this manager existed needs one sweep.
        for thread in threads.peek() {
            Self::add_window(&inner, thread.id());
        }

        Self(SendWrapper::new(inner))
    }

    /// Reactive unread count for one thread's badge. Zero until the viewer's
    /// own cursor rows have loaded (reads track both signals).
    pub fn unread_count(&self, thread_id: &str) -> usize {
        if !self.0.ready.get() {
            return 0;
        }
        self.0.unread.get().get(thread_id).copied().unwrap_or(0)
    }

    /// Reactive "newest message in this thread" timestamp, for sidebar
    /// ordering. Zero for a thread with no messages in its window.
    pub fn newest_ts(&self, thread_id: &str) -> i64 { self.0.newest.get().get(thread_id).copied().unwrap_or(0) }

    /// Record that the viewer has seen this thread up to `ts`. No-ops unless
    /// the cursor advances; otherwise the local map updates immediately
    /// (badges clear instantly) and a row upsert is flushed in the background.
    pub fn mark_read(&self, thread_id: &str, ts: i64) {
        // Never let a cursor run past now. `ts` comes from a message, and the
        // server settles a future-dated message's timestamp on the row — but
        // this client can be looking at one in the window before that write
        // lands, and a cursor parked in 2100 outlives the window: it silences
        // the thread's badge for every real message after it, and `mark_read`
        // only ever advances, so nothing here could walk it back.
        let ts = ts.min(now_ms());
        let inner: &Arc<Inner> = &self.0;
        {
            let cursors = inner.last_read.peek();
            if ts <= cursors.get(thread_id).copied().unwrap_or(0) {
                return;
            }
        }
        let mut cursors = inner.last_read.peek().clone();
        cursors.insert(thread_id.to_string(), ts);
        inner.last_read.set(cursors);
        Self::recompute_thread(inner, thread_id);

        if !inner.in_flight.lock().unwrap().insert(thread_id.to_string()) {
            return; // a flush loop is already running; it will pick this up
        }
        let inner = Arc::clone(inner);
        let thread_id = thread_id.to_string();
        spawn_local(async move {
            Self::flush(&inner, &thread_id).await;
            inner.in_flight.lock().unwrap().remove(&thread_id);
        });
    }

    /// Fold the viewer's persisted cursor rows into the local map, repairing
    /// any that are dated in the future on the way through.
    ///
    /// A cursor ahead of now silences a thread's badge completely — every
    /// message stamps at most today, so nothing is ever newer than the cursor —
    /// and `mark_read` cannot undo it, because `mark_read` only ever advances.
    /// Clamping what is read here fixes this session and nothing else: the row
    /// still says 2100, and the next session starts from the same bad number.
    /// So the clamped value is written back as well. Nobody else can write this
    /// row — `dmreadstate`'s scope is `user = $jwt.sub` — so if the owner's
    /// client does not correct it, nothing will.
    fn rebuild_cursors(inner: &Arc<Inner>) {
        let now = now_ms();
        let mut cursors = inner.last_read.peek().clone();
        let mut flushed = inner.flushed.lock().unwrap();
        let mut dated_ahead: Vec<DmReadStateView> = Vec::new();
        for row in inner.cursors.peek() {
            let (Ok(thread), Ok(stored)) = (row.thread(), row.last_read_ts()) else { continue };
            if stored > now {
                dated_ahead.push(row.clone());
            }
            let ts = stored.min(now);
            let key = thread.id().to_base64();
            let entry = cursors.entry(key.clone()).or_insert(0);
            *entry = (*entry).max(ts);
            // The watermark is what stops `flush` rewriting a row it has
            // already written, so it takes the clamped value too: parked in the
            // future it would tell `flush` there is nothing left to write.
            let watermark = flushed.entry(key).or_insert(0);
            *watermark = (*watermark).max(ts);
        }
        drop(flushed);
        inner.last_read.set(cursors);
        for row in dated_ahead {
            Self::heal_cursor(inner, row, now);
        }
    }

    /// Write today's date over a cursor row dated in the future.
    ///
    /// One repair per row at a time: the repair commits, the cursors LiveQuery
    /// delivers the change, and this function runs again on a row that now
    /// reads at most now — so the guard is what keeps the burst between those
    /// two moments from becoming a write per changeset.
    fn heal_cursor(inner: &Arc<Inner>, row: DmReadStateView, ts: i64) {
        let row_id = row.id();
        if !inner.healing.lock().unwrap().insert(row_id) {
            return;
        }
        let inner = Arc::clone(inner);
        spawn_local(async move {
            if let Err(e) = write_cursor(&row, ts).await {
                tracing::error!("Failed to repair a DM read cursor dated in the future ({}): {}", row_id.to_base64(), e);
            }
            inner.healing.lock().unwrap().remove(&row_id);
        });
    }

    fn add_window(inner: &Arc<Inner>, thread_id: EntityId) {
        let key = thread_id.to_base64();
        if inner.windows.lock().unwrap().contains_key(&key) {
            return;
        }
        let selection =
            queries::selection("thread = ? AND deleted = false ORDER BY timestamp DESC LIMIT 10", [(&thread_id).into()])
                .expect("static dm unread window selection parses");
        let query = match ctx().query::<DmMessageView>(selection) {
            Ok(q) => q,
            Err(e) => {
                tracing::error!("Failed to create DM unread window for thread {}: {:?}", key, e);
                return;
            }
        };

        let inner_for_sub = inner.clone();
        let key_for_sub = key.clone();
        let guard = query.subscribe(move |_: ChangeSet<DmMessageView>| {
            Self::recompute_thread(&inner_for_sub, &key_for_sub);
        });

        inner.windows.lock().unwrap().insert(key.clone(), ThreadWindow { thread_id, query, _guard: guard });
        // If the window's initial changeset fired before the map insert above,
        // that recompute found no window and skipped; run once now (idempotent).
        Self::recompute_thread(inner, &key);
    }

    /// Unread for one thread = messages in its window newer than the cursor and
    /// authored by the other participant. Also refreshes the ordering key.
    fn recompute_thread(inner: &Arc<Inner>, thread_id: &str) {
        let Some(items) = inner.windows.lock().unwrap().get(thread_id).map(|w| w.query.peek()) else { return };
        let cursor = inner.last_read.peek().get(thread_id).copied().unwrap_or(0);
        let count = items
            .iter()
            .filter(|m| stamp_of(m).map(|ts| ts > cursor).unwrap_or(false))
            .filter(|m| m.user().map(|u| u.id() != inner.user_id).unwrap_or(true))
            .count();

        let mut unread = inner.unread.peek().clone();
        if unread.get(thread_id).copied().unwrap_or(0) != count {
            if count == 0 {
                unread.remove(thread_id);
            } else {
                unread.insert(thread_id.to_string(), count);
            }
            inner.unread.set(unread);
        }

        let newest_ts = items.iter().filter_map(stamp_of).max().unwrap_or(0);
        let mut newest = inner.newest.peek().clone();
        if newest.get(thread_id).copied().unwrap_or(0) != newest_ts {
            newest.insert(thread_id.to_string(), newest_ts);
            inner.newest.set(newest);
        }
    }

    fn recompute_all(inner: &Arc<Inner>) {
        let keys: Vec<String> = inner.windows.lock().unwrap().keys().cloned().collect();
        for key in keys {
            Self::recompute_thread(inner, &key);
        }
    }

    /// Keep upserting until the row watermark catches the local cursor, so a
    /// burst of `mark_read`s collapses into one trailing write.
    async fn flush(inner: &Arc<Inner>, thread_id: &str) {
        loop {
            let desired = inner.last_read.peek().get(thread_id).copied().unwrap_or(0);
            let watermark = inner.flushed.lock().unwrap().get(thread_id).copied().unwrap_or(0);
            if desired <= watermark {
                return;
            }
            match Self::upsert(inner, thread_id, desired).await {
                Ok(()) => {
                    let mut flushed = inner.flushed.lock().unwrap();
                    let entry = flushed.entry(thread_id.to_string()).or_insert(0);
                    *entry = (*entry).max(desired);
                }
                Err(e) => {
                    tracing::error!("Failed to persist DM read state for thread {}: {}", thread_id, e);
                    return;
                }
            }
        }
    }

    async fn upsert(inner: &Arc<Inner>, thread_id: &str, ts: i64) -> Result<(), Box<dyn std::error::Error>> {
        let thread_eid = match inner.windows.lock().unwrap().get(thread_id) {
            Some(w) => w.thread_id,
            None => EntityId::from_base64(thread_id)?,
        };

        // Prefer a row from the LiveQuery, then a row this client created that
        // the LiveQuery hasn't delivered yet.
        let existing = inner.cursors.peek().into_iter().find(|r| r.thread().map(|t| t.id() == thread_eid).unwrap_or(false));
        let existing = match existing {
            Some(row) => Some(row),
            None => {
                let recorded = inner.row_ids.lock().unwrap().get(thread_id).copied();
                match recorded {
                    Some(id) => ctx().get::<DmReadStateView>(id).await.ok(),
                    None => None,
                }
            }
        };

        let trx = ctx().begin();
        match existing {
            Some(row) => {
                row.edit(&trx)?.last_read_ts().set(&ts)?;
            }
            None => {
                let created = trx
                    .create(&DmReadState { user: inner.user_id.into(), thread: thread_eid.into(), last_read_ts: ts })
                    .await?;
                inner.row_ids.lock().unwrap().insert(thread_id.to_string(), created.id());
            }
        }
        trx.commit().await?;
        Ok(())
    }
}

/// Set one cursor row's `last_read_ts`, for the repair path above.
async fn write_cursor(row: &DmReadStateView, ts: i64) -> Result<(), Box<dyn std::error::Error>> {
    let trx = ctx().begin();
    row.edit(&trx)?.last_read_ts().set(&ts)?;
    trx.commit().await?;
    Ok(())
}
