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

/// The newest instant a read cursor in one thread can honestly stand at: this
/// client's own clock, or the newest send time that thread itself shows,
/// whichever is later.
///
/// Both halves are load-bearing, and in opposite directions.
///
/// THE CLOCK ALONE IS NOT THE CEILING, which is what it used to be. A cursor
/// holds the timestamp of a message, and a message's timestamp is the SERVER's
/// clock — the server settles it and stores it (see [`stamp_of`]). So a reader
/// whose browser runs five minutes slow, reading the tail of a thread, had
/// their cursor pinned at their own now while every message they had just read
/// stamped later than it: the badge relit immediately, and no later read could
/// walk the cursor forward, because each one pinned it again.
///
/// THE THREAD'S NEWEST MESSAGE ALONE IS NOT THE CEILING EITHER. A conversation
/// spread over the twin rows of a first-DM race gets ONE cursor written across
/// every one of its rows (`crate::dm_chat`), so a twin legitimately holds a
/// cursor newer than anything in that row. Hold it down to its own newest
/// message and the sidebar's badge starts counting messages the reader read in
/// the other row.
///
/// What the pair of them still refuses is a cursor with nothing behind it: a
/// message its sender dated in 2100, read in the moment before the server
/// settles it (`server/src/workers/dm_timestamp.rs`), would otherwise leave a
/// cursor no real message can ever pass, silencing the thread for good. Such a
/// cursor is justified only while the message it came from still shows 2100 —
/// the instant the settling write arrives this drops back to the clock, and
/// [`DmReadStateManager::recompute_thread`] walks the cursor back with it.
///
/// Walking one back lands it HERE rather than on the thread's newest message,
/// which is what keeps the repair from fighting the next read in the ordinary
/// case: a cursor taken from a message this window can see, on a client whose
/// clock is not behind the server's, does not exceed this, so nothing re-raises
/// it and no row is rewritten.
///
/// TWO CASES DO CHURN AGAINST IT, accepted rather than overlooked. Both need a
/// reader whose clock trails the server, and neither produces a wrong badge —
/// a walk-back lands at or above the newest message the window shows, so it can
/// never light a badge for anything already there.
///
/// - The thread view's timeline reads the pair's rows with no `deleted = false`
///   on it (`crate::dm_chat`), while the window below filters tombstones out. So
///   when the newest message in a thread has been tombstoned, `mark_read` is
///   handed its stamp and this ceiling cannot see it.
/// - A conversation spread over race twins shares one cursor across its rows,
///   so the stamp `mark_read` is handed for row A can come from row B and be
///   newer than anything A holds.
///
/// In both, the cursor is walked back and the next read at the tail raises it
/// again, so the pair of them trade places until the reader's clock passes the
/// stamp — costing a row write each time the ceiling has moved since the last
/// one. Bounded by the clock skew, and silent.
///
/// The other cost, stated because it is a choice: on a client whose clock runs
/// AHEAD, a walked-back cursor sits that far ahead of real time, so a message
/// arriving in the minutes after the repair counts as read. That one applies
/// only to a thread someone has aimed a future-dated message at, and it expires
/// as the clock catches up.
fn ceiling(newest_in_thread: i64) -> i64 { now_ms().max(newest_in_thread) }

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
/// the server owning the number; see that worker's module doc. What must not
/// outlive it is a read cursor set from the claimed value, and
/// [`DmReadStateManager::recompute_thread`] walks one back when the settled
/// value arrives.
fn stamp_of(message: &DmMessageView) -> Option<i64> { message.timestamp().ok() }

#[derive(Clone)]
pub struct DmReadStateManager(SendWrapper<Arc<Inner>>);

struct Inner {
    user_id: EntityId,
    /// The viewer's own DmReadState rows, live.
    cursors: LiveQuery<DmReadStateView>,
    /// thread id (base64) → effective read cursor: persisted rows and this
    /// session's own advances, merged by taking the later of the two, then held
    /// down to what the thread's messages justify (see [`ceiling`]).
    last_read: Mut<HashMap<String, i64>>,
    /// thread id → newest cursor value confirmed written to a row. Comes back
    /// down with the cursor when one is walked back, so `flush` is never told a
    /// row holds something the cursor no longer claims.
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

    /// Record that the viewer has seen this thread up to `ts` — the stamp of
    /// the newest message they actually read, stored as it stands. No-ops
    /// unless the cursor advances; otherwise the local map updates immediately
    /// (badges clear instantly) and a row upsert is flushed in the background.
    ///
    /// This client's clock is not consulted. `ts` is a message timestamp, and
    /// so is everything the cursor is ever compared against
    /// ([`Self::recompute_thread`]) — both of them the server's number, not this
    /// browser's. Pinning the cursor to the reader's own clock, which is what
    /// this used to do, meant a browser running minutes behind the server stored
    /// a cursor OLDER than the messages it was meant to cover and relit the
    /// badge for every one of them.
    ///
    /// The ceiling that stops a cursor running away has not gone; it lives in
    /// `recompute_thread`, called on the next line and again on every change to
    /// the thread's window, which is where the thread's own messages are in hand
    /// to judge against (see [`ceiling`]).
    pub fn mark_read(&self, thread_id: &str, ts: i64) {
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

    /// Fold the viewer's persisted cursor rows into the local map.
    ///
    /// Persisted rows and this session's own advances are merged by taking the
    /// later of the two, because either can be ahead of the other: a row written
    /// on the viewer's other device, or a `mark_read` here whose write has not
    /// landed yet.
    ///
    /// NO CEILING IS APPLIED HERE, deliberately, and this is the function it was
    /// taken out of. Judging a stored cursor needs the thread's own messages to
    /// judge against ([`ceiling`]), and this function has none: it runs from the
    /// cursor subscription, which at startup fires before any thread window has
    /// delivered a message, so the only ceiling available to it would be the
    /// reader's clock — which is exactly the comparison that relit every badge
    /// on a slow machine. [`Self::recompute_thread`] holds the cursors down
    /// instead; the subscription that calls this calls it for every thread
    /// immediately afterwards, so a stored value that cannot be justified is
    /// walked back before it is ever counted with.
    fn rebuild_cursors(inner: &Arc<Inner>) {
        let mut cursors = inner.last_read.peek().clone();
        let mut flushed = inner.flushed.lock().unwrap();
        for row in inner.cursors.peek() {
            let (Ok(thread), Ok(stored)) = (row.thread(), row.last_read_ts()) else { continue };
            let key = thread.id().to_base64();
            let entry = cursors.entry(key.clone()).or_insert(0);
            *entry = (*entry).max(stored);
            // The watermark is what stops `flush` rewriting a row it has
            // already written.
            let watermark = flushed.entry(key).or_insert(0);
            *watermark = (*watermark).max(stored);
        }
        drop(flushed);
        inner.last_read.set(cursors);
    }

    /// Write the ceiling over a cursor row that has run past it.
    ///
    /// One repair per row at a time: the repair commits, the cursors LiveQuery
    /// delivers the change, and this function runs again on a row that now
    /// reads at most the ceiling — so the guard is what keeps the burst between
    /// those two moments from becoming a write per changeset. Nobody else can
    /// write this row — `dmreadstate`'s scope is `user = $jwt.sub` — so if the
    /// owner's client does not correct it, nothing will.
    fn heal_cursor(inner: &Arc<Inner>, row: DmReadStateView, ts: i64) {
        let row_id = row.id();
        if !inner.healing.lock().unwrap().insert(row_id) {
            return;
        }
        let inner = Arc::clone(inner);
        spawn_local(async move {
            if let Err(e) = write_cursor(&row, ts).await {
                tracing::error!("Failed to repair a DM read cursor that had run past its thread ({}): {}", row_id.to_base64(), e);
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
        // that recompute found no window and skipped; run once now. Repeating a
        // recompute costs nothing: it recounts and, if the window has already
        // delivered, re-applies a ceiling the cursor is by then already under.
        // The usual case here is a window that has not delivered at all, which
        // recomputes an empty thread and touches no cursor.
        Self::recompute_thread(inner, &key);
    }

    /// Unread for one thread = messages in its window newer than the cursor and
    /// authored by the other participant. Also refreshes the ordering key — and,
    /// once the window has delivered, walks the cursor back first if it has run
    /// past what the thread can justify, which is the one place in this file
    /// that judges a cursor at all.
    fn recompute_thread(inner: &Arc<Inner>, thread_id: &str) {
        let Some((thread_eid, loaded, items)) =
            inner.windows.lock().unwrap().get(thread_id).map(|w| (w.thread_id, w.query.loaded(), w.query.peek()))
        else {
            return;
        };
        let newest_ts = items.iter().filter_map(stamp_of).max().unwrap_or(0);
        // ONLY JUDGE A CURSOR AGAINST A WINDOW THAT HAS ACTUALLY DELIVERED. An
        // empty `peek()` means one of two completely different things, and
        // `loaded()` is what tells them apart — the same distinction
        // `members_panel`, `notification_inbox` and `mod_log_panel` draw before
        // they render "nothing here". Ungated, the wrong reading is the ruinous
        // one: the cursor subscription resolves before any of these per-thread
        // windows has populated, so at every page load `newest_ts` would read 0,
        // the ceiling would collapse to the reader's clock alone, and a reader
        // whose clock trails the server would have every good cursor walked back
        // to their own now AND written to the row — this file's original defect,
        // re-created on each load and now with a destructive write behind it.
        //
        // Waiting costs nothing. The walk-back exists for a cursor taken from a
        // message dated far in the future, and that message is IN the window
        // once the window loads, so the ceiling is meaningful exactly when the
        // repair fires. The unread count below runs either way: it is display
        // only, and an unloaded window shows zero unread until its first
        // changeset says otherwise.
        if loaded {
            Self::hold_cursor_down(inner, thread_id, thread_eid, ceiling(newest_ts));
        }

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

        let mut newest = inner.newest.peek().clone();
        if newest.get(thread_id).copied().unwrap_or(0) != newest_ts {
            newest.insert(thread_id.to_string(), newest_ts);
            inner.newest.set(newest);
        }
    }

    /// Walk one thread's cursor back to `ceiling` if it has run past it — in
    /// memory, and on the row it came from.
    ///
    /// What this is for is a cursor that has run past every message there is,
    /// which silences the thread's badge completely: nothing arriving afterwards
    /// is newer than it, and `mark_read` only ever advances, so nothing else in
    /// this file could undo it. The way to get one is to read a message in the
    /// moment before the server settles its send time.
    ///
    /// It also fires on two harmless mismatches — a tombstoned newest message,
    /// and a cursor shared across race twins — where it walks a cursor back that
    /// the next read raises again. [`ceiling`] sets out both, and why the trade
    /// is worth taking: a walk-back never lights a badge for a message already
    /// in the window.
    ///
    /// The row is repaired too, not just this session's copy, because the row is
    /// what the next session starts from. The `healing` guard in
    /// [`Self::heal_cursor`] is what stops the changesets between the write and
    /// its delivery from starting a second one.
    ///
    /// The caller decides WHEN this may run — only against a window that has
    /// delivered, see [`Self::recompute_thread`] — because the ceiling is
    /// meaningless before then.
    fn hold_cursor_down(inner: &Arc<Inner>, thread_id: &str, thread_eid: EntityId, ceiling: i64) {
        {
            let cursors = inner.last_read.peek();
            if cursors.get(thread_id).copied().unwrap_or(0) <= ceiling {
                return;
            }
        }
        let mut cursors = inner.last_read.peek().clone();
        cursors.insert(thread_id.to_string(), ceiling);
        inner.last_read.set(cursors);
        {
            // The watermark comes down with the cursor. Left where it was, it
            // would tell `flush` the row already holds something newer than
            // anything the reader could read next — so neither this repair nor
            // the next genuine read below the old value would ever be written.
            let mut flushed = inner.flushed.lock().unwrap();
            let watermark = flushed.entry(thread_id.to_string()).or_insert(0);
            *watermark = (*watermark).min(ceiling);
        }

        // And the row, if one has been written yet. Nothing persisted means the
        // walk-back above is already the whole repair.
        let stored = inner.cursors.peek().into_iter().find(|r| r.thread().map(|t| t.id() == thread_eid).unwrap_or(false));
        if let Some(row) = stored
            && row.last_read_ts().map(|ts| ts > ceiling).unwrap_or(false)
        {
            Self::heal_cursor(inner, row, ceiling);
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
