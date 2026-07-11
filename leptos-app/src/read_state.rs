//! Persistent per-room read state and unread badges (issue #13).
//!
//! `ReadStateManager` owns two kinds of LiveQueries:
//!
//! - the signed-in user's own `ReadState` rows (`user = ?` — the readstate
//!   policy scope enforces this server-side; the client filter is
//!   belt-and-braces), collapsed into a `room id → last_read_ts` map;
//! - one `LIMIT 10` newest-messages window per room (the same window shape
//!   the NotificationManager uses for sounds), from which a room's unread
//!   count is "messages in the window newer than `last_read_ts`, authored by
//!   someone else". Counts therefore cap at 10, which the badge already
//!   renders as "10+".
//!
//! Cost model, stated plainly: ankurah 0.9.0 has no aggregate queries, so a
//! true unread *count* requires message rows on the client. This manager
//! subscribes to one LIMIT-10 window per room (in addition to the
//! NotificationManager's identical sound windows). For a community-sized
//! room list this is a handful of small subscriptions; if the room count
//! grows large, the windows could be shared between the two managers or
//! downgraded to a boolean dot.
//!
//! Write path: `mark_read(room, ts)` is called by chat.rs whenever the user
//! is viewing the bottom of a room (room switch, scroll-to-live, new message
//! while live). It no-ops unless `ts` advances the cursor, updates the local
//! map optimistically (badges clear instantly), then flushes an upsert of
//! the row. A per-room in-flight guard plus a "flushed" watermark coalesce
//! bursts into at most one trailing write, and a remembered created-row id
//! prevents duplicate rows when a create commits before the LiveQuery
//! catches up. Duplicate rows (e.g. two tabs racing their first write) stay
//! harmless: reads take the max across rows and edits converge on one row.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use ankurah::{changes::ChangeSet, EntityId, LiveQuery};
use ankurah_signals::{Get, Mut, Peek, Subscribe, SubscriptionGuard};
use community_model::{MessageView, ReadState, ReadStateView, RoomView};
use send_wrapper::SendWrapper;
use wasm_bindgen_futures::spawn_local;

use crate::{ctx, queries};

#[derive(Clone)]
pub struct ReadStateManager(SendWrapper<Arc<Inner>>);

struct Inner {
    user_id: EntityId,
    /// The user's own ReadState rows, live.
    read_states: LiveQuery<ReadStateView>,
    /// room id (base64) → effective read cursor. Server rows merged with
    /// optimistic local advances (always the max of the two).
    last_read: Mut<HashMap<String, i64>>,
    /// room id → newest cursor value confirmed written to a row. `mark_read`
    /// keeps flushing while `last_read` is ahead of this watermark.
    flushed: Mutex<HashMap<String, i64>>,
    /// Rooms with an upsert currently in flight (coalesces write bursts).
    in_flight: Mutex<HashSet<String>>,
    /// room id → id of the row this client created, so a second upsert racing
    /// the LiveQuery round-trip edits that row instead of creating a twin.
    row_ids: Mutex<HashMap<String, EntityId>>,
    /// room id → unread count within the LIMIT-10 window.
    unread: Mut<HashMap<String, usize>>,
    /// Per-room newest-message windows.
    windows: Mutex<HashMap<String, RoomWindow>>,
    /// False until the user's ReadState rows have arrived once; badges render
    /// as zero before that instead of flashing "everything unread".
    ready: Mut<bool>,
    _rooms_guard: Mutex<Option<SubscriptionGuard>>,
    _read_states_guard: Mutex<Option<SubscriptionGuard>>,
}

struct RoomWindow {
    room_id: EntityId,
    query: LiveQuery<MessageView>,
    _guard: SubscriptionGuard,
}

impl ReadStateManager {
    pub fn new(rooms: LiveQuery<RoomView>, user_id: EntityId) -> Self {
        let read_states = ctx()
            .query::<ReadStateView>(
                queries::selection("user = ?", [(&user_id).into()]).expect("static readstate selection parses"),
            )
            .expect("failed to create ReadStateView LiveQuery");

        let inner = Arc::new(Inner {
            user_id,
            read_states: read_states.clone(),
            last_read: Mut::new(HashMap::new()),
            flushed: Mutex::new(HashMap::new()),
            in_flight: Mutex::new(HashSet::new()),
            row_ids: Mutex::new(HashMap::new()),
            unread: Mut::new(HashMap::new()),
            windows: Mutex::new(HashMap::new()),
            ready: Mut::new(false),
            _rooms_guard: Mutex::new(None),
            _read_states_guard: Mutex::new(None),
        });

        // Own read-state rows → cursor map (and re-derive every badge).
        let inner_for_rs = inner.clone();
        let rs_guard = read_states.subscribe(move |_: ChangeSet<ReadStateView>| {
            Self::rebuild_cursors(&inner_for_rs);
            if !inner_for_rs.ready.peek() {
                inner_for_rs.ready.set(true);
            }
            Self::recompute_all(&inner_for_rs);
        });
        *inner._read_states_guard.lock().unwrap() = Some(rs_guard);

        // One newest-messages window per room, following the rooms query.
        let inner_for_rooms = inner.clone();
        let rooms_guard = rooms.subscribe(move |changeset: ChangeSet<RoomView>| {
            for room in changeset.appeared() {
                Self::add_window(&inner_for_rooms, room);
            }
            for room in changeset.removed() {
                let key = room.id().to_base64();
                inner_for_rooms.windows.lock().unwrap().remove(&key);
                let mut unread = inner_for_rooms.unread.peek().clone();
                if unread.remove(&key).is_some() {
                    inner_for_rooms.unread.set(unread);
                }
            }
        });
        *inner._rooms_guard.lock().unwrap() = Some(rooms_guard);

        Self(SendWrapper::new(inner))
    }

    /// Reactive unread count for one room's badge. Zero until the user's own
    /// read-state rows have loaded (reads track both signals).
    pub fn unread_count(&self, room_id: &str) -> usize {
        if !self.0.ready.get() {
            return 0;
        }
        self.0.unread.get().get(room_id).copied().unwrap_or(0)
    }

    /// Record that the user has seen this room up to `ts` (the newest visible
    /// message timestamp). No-ops unless the cursor advances; otherwise the
    /// local map updates immediately and a row upsert is flushed in the
    /// background.
    pub fn mark_read(&self, room_id: &str, ts: i64) {
        let inner: &Arc<Inner> = &self.0;
        {
            let cursors = inner.last_read.peek();
            if ts <= cursors.get(room_id).copied().unwrap_or(0) {
                return;
            }
        }
        let mut cursors = inner.last_read.peek().clone();
        cursors.insert(room_id.to_string(), ts);
        inner.last_read.set(cursors);
        Self::recompute_room(inner, room_id);

        if !inner.in_flight.lock().unwrap().insert(room_id.to_string()) {
            return; // a flush loop is already running; it will pick this up
        }
        let inner = Arc::clone(inner);
        let room_id = room_id.to_string();
        spawn_local(async move {
            Self::flush(&inner, &room_id).await;
            inner.in_flight.lock().unwrap().remove(&room_id);
        });
    }

    /// Rebuild the cursor map from the row resultset, keeping local optimistic
    /// advances (max wins) and moving the flushed watermark up to row values.
    fn rebuild_cursors(inner: &Arc<Inner>) {
        let mut cursors = inner.last_read.peek().clone();
        let mut flushed = inner.flushed.lock().unwrap();
        for row in inner.read_states.peek() {
            let (Ok(room), Ok(ts)) = (row.room(), row.last_read_ts()) else { continue };
            let key = room.id().to_base64();
            let entry = cursors.entry(key.clone()).or_insert(0);
            *entry = (*entry).max(ts);
            let watermark = flushed.entry(key).or_insert(0);
            *watermark = (*watermark).max(ts);
        }
        drop(flushed);
        inner.last_read.set(cursors);
    }

    fn add_window(inner: &Arc<Inner>, room: RoomView) {
        let key = room.id().to_base64();
        if inner.windows.lock().unwrap().contains_key(&key) {
            return;
        }
        let selection = queries::selection(
            "room = ? AND deleted = false ORDER BY timestamp DESC LIMIT 10",
            [(&room.id()).into()],
        )
        .expect("static unread window selection parses");
        let query = match ctx().query::<MessageView>(selection) {
            Ok(q) => q,
            Err(e) => {
                tracing::error!("Failed to create unread window for room {}: {:?}", key, e);
                return;
            }
        };

        let inner_for_sub = inner.clone();
        let key_for_sub = key.clone();
        let guard = query.subscribe(move |_: ChangeSet<MessageView>| {
            Self::recompute_room(&inner_for_sub, &key_for_sub);
        });

        inner.windows.lock().unwrap().insert(key.clone(), RoomWindow { room_id: room.id(), query, _guard: guard });
        // If the window's initial changeset fired before the map insert above,
        // that recompute found no window and skipped; run once now (idempotent).
        Self::recompute_room(inner, &key);
    }

    /// Unread for one room = messages in its window newer than the cursor and
    /// authored by someone else (your own messages are read by definition).
    fn recompute_room(inner: &Arc<Inner>, room_id: &str) {
        let Some(items) = inner.windows.lock().unwrap().get(room_id).map(|w| w.query.peek()) else { return };
        let cursor = inner.last_read.peek().get(room_id).copied().unwrap_or(0);
        let count = items
            .iter()
            .filter(|m| m.timestamp().map(|ts| ts > cursor).unwrap_or(false))
            .filter(|m| m.user().map(|u| u.id() != inner.user_id).unwrap_or(true))
            .count();

        let mut unread = inner.unread.peek().clone();
        let changed = unread.get(room_id).copied().unwrap_or(0) != count;
        if changed {
            if count == 0 {
                unread.remove(room_id);
            } else {
                unread.insert(room_id.to_string(), count);
            }
            inner.unread.set(unread);
        }
    }

    fn recompute_all(inner: &Arc<Inner>) {
        let keys: Vec<String> = inner.windows.lock().unwrap().keys().cloned().collect();
        for key in keys {
            Self::recompute_room(inner, &key);
        }
    }

    /// Keep upserting until the row watermark catches the local cursor, so a
    /// burst of `mark_read`s collapses into one trailing write.
    async fn flush(inner: &Arc<Inner>, room_id: &str) {
        loop {
            let desired = inner.last_read.peek().get(room_id).copied().unwrap_or(0);
            let watermark = inner.flushed.lock().unwrap().get(room_id).copied().unwrap_or(0);
            if desired <= watermark {
                return;
            }
            match Self::upsert(inner, room_id, desired).await {
                Ok(()) => {
                    let mut flushed = inner.flushed.lock().unwrap();
                    let entry = flushed.entry(room_id.to_string()).or_insert(0);
                    *entry = (*entry).max(desired);
                }
                Err(e) => {
                    tracing::error!("Failed to persist read state for room {}: {}", room_id, e);
                    return;
                }
            }
        }
    }

    async fn upsert(inner: &Arc<Inner>, room_id: &str, ts: i64) -> Result<(), Box<dyn std::error::Error>> {
        let room_eid = match inner.windows.lock().unwrap().get(room_id) {
            Some(w) => w.room_id,
            None => EntityId::from_base64(room_id)?,
        };

        // Prefer a row from the LiveQuery, then a row this client created that
        // the LiveQuery hasn't delivered yet.
        let existing = inner
            .read_states
            .peek()
            .into_iter()
            .find(|r| r.room().map(|rf| rf.id() == room_eid).unwrap_or(false));
        let existing = match existing {
            Some(row) => Some(row),
            None => {
                let recorded = inner.row_ids.lock().unwrap().get(room_id).copied();
                match recorded {
                    Some(id) => ctx().get::<ReadStateView>(id).await.ok(),
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
                    .create(&ReadState { user: inner.user_id.into(), room: room_eid.into(), last_read_ts: ts })
                    .await?;
                inner.row_ids.lock().unwrap().insert(room_id.to_string(), created.id());
            }
        }
        trx.commit().await?;
        Ok(())
    }
}
