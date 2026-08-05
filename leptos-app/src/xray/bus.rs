//! The x-ray bus: x-ray's view of the app's LiveQueries plus a bounded live
//! event feed built from their changesets.
//!
//! The bus does not know the app's queries first-hand — it is one observer of
//! the generic query registry (`crate::query_registry`), which components
//! register with and which x-ray attaches to at startup (`xray::attach`).
//! Every registration lands here as a [`QueryEntry`]; when the component drops
//! its registration, the entry goes with it.
//!
//! Honesty note: ankurah 0.9.0 keeps the reactor's subscription table
//! `pub(crate)` (`ankurah-core/src/node.rs`), so a client cannot enumerate
//! *all* node subscriptions — only the queries the app itself holds and
//! chooses to register. The panel labels itself accordingly.
//!
//! An entry stores cheap introspection handles (`query_id`, the reactive
//! selection, the untyped resultset, the error signal). The changeset *tap*
//! (which is what feeds the event stream) is installed only while x-ray is
//! enabled, and dropped on disable — a registered query costs nothing while
//! x-ray is off.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock, RwLock};

use leptos::prelude::{ArcRwSignal, Update};

use ankurah::changes::ChangeKind;
use ankurah::proto::{Attested, Clock, CollectionId, EntityId, Event, EventId};
use ankurah_signals::{Read, Subscribe, SubscriptionGuard};

use crate::query_registry::{QueryChange, QueryObserver, RegisteredQuery, RegistrationId};
use crate::ws_client;

/// One registered LiveQuery: the registry's handles plus x-ray's own
/// per-query bookkeeping.
struct QueryEntry {
    query: RegisteredQuery,
    /// Changesets seen by the tap since registration (activity indicator).
    changes_seen: ArcRwSignal<u64>,
    tap: Mutex<Option<SubscriptionGuard>>,
}

/// Renderable clone of one registry entry (see [`BusHandle::snapshot`]).
#[derive(Clone)]
pub struct QuerySnapshot {
    pub query: RegisteredQuery,
    pub changes_seen: ArcRwSignal<u64>,
}

/// One event as it appeared in a changeset, summarized for the feed.
#[derive(Clone, Debug)]
pub struct FeedEvent {
    pub id: EventId,
    pub parent: Clock,
    /// Per-backend op summaries (yrs deltas decoded, LWW byte sizes).
    pub badges: Vec<super::decode::OpBadge>,
}

/// One row of the live feed: a membership change from a registered query.
#[derive(Clone, Debug)]
pub struct FeedEntry {
    pub seq: u64,
    pub at_ms: f64,
    pub query_label: String,
    pub collection: CollectionId,
    /// `None` for coalesced initial-load batches.
    pub entity_id: Option<EntityId>,
    pub kind: &'static str,
    /// Number of items this row covers (>1 only for initial batches).
    pub count: usize,
    /// The entity's head clock after the change (short form).
    pub head_short: String,
    pub events: Vec<FeedEvent>,
}

pub const FEED_CAP: usize = 100;
const CONN_LOG_CAP: usize = 24;

struct BusInner {
    entries: RwLock<Vec<QueryEntry>>,
    /// Bumped on register/unregister so the panel re-reads the entry list.
    entries_rev: ArcRwSignal<u64>,
    feed: ArcRwSignal<VecDeque<FeedEntry>>,
    tapping: AtomicBool,
    /// Connection-state transition log (timestamp ms, description).
    conn_log: ArcRwSignal<VecDeque<(f64, String)>>,
    conn_guard: Mutex<Option<SubscriptionGuard>>,
}

/// Cheap clonable handle to the process-wide bus.
#[derive(Clone)]
pub struct BusHandle(&'static BusInner);

static BUS: OnceLock<BusInner> = OnceLock::new();
static FEED_SEQ: AtomicU64 = AtomicU64::new(0);

/// The global x-ray bus (created on first use).
pub fn bus() -> BusHandle {
    BusHandle(BUS.get_or_init(|| BusInner {
        entries: RwLock::new(Vec::new()),
        entries_rev: ArcRwSignal::new(0),
        feed: ArcRwSignal::new(VecDeque::new()),
        tapping: AtomicBool::new(false),
        conn_log: ArcRwSignal::new(VecDeque::new()),
        conn_guard: Mutex::new(None),
    }))
}

/// X-ray's attachment to the query registry. Stateless — every registration it
/// hears about lands in the process-wide bus.
pub struct BusObserver;

impl QueryObserver for BusObserver {
    fn query_registered(&self, query: &RegisteredQuery) {
        let handle = bus();
        let changes_seen = ArcRwSignal::new(0u64);
        // A query that arrives while x-ray is on starts tapped; `set_tapping`
        // covers the ones already here when it is switched on.
        let tap = handle.0.tapping.load(Ordering::Relaxed).then(|| install_tap(query, &changes_seen));
        let entry = QueryEntry { query: query.clone(), changes_seen, tap: Mutex::new(tap) };

        handle.0.entries.write().unwrap_or_else(|e| e.into_inner()).push(entry);
        handle.0.entries_rev.update(|r| *r += 1);
    }

    fn query_unregistered(&self, id: RegistrationId) {
        let handle = bus();
        handle.0.entries.write().unwrap_or_else(|e| e.into_inner()).retain(|e| e.query.id != id);
        handle.0.entries_rev.update(|r| *r += 1);
    }
}

/// Start turning one query's changesets into feed rows.
fn install_tap(query: &RegisteredQuery, changes_seen: &ArcRwSignal<u64>) -> SubscriptionGuard {
    let feed = bus().0.feed.clone();
    let label = query.label.clone();
    let collection = query.collection.clone();
    let changes_seen = changes_seen.clone();
    query.watch_changes(move |changes: &[QueryChange]| {
        changes_seen.update(|n| *n += 1);
        push_changes(&feed, &label, &collection, changes);
    })
}

impl BusHandle {
    /// Install or drop the changeset taps on every registered query.
    /// Called from `XRayState::set_enabled` — not part of the public API.
    pub(crate) fn set_tapping(&self, on: bool) {
        self.0.tapping.store(on, Ordering::Relaxed);
        let entries = self.0.entries.read().unwrap_or_else(|e| e.into_inner());
        for entry in entries.iter() {
            let mut tap = entry.tap.lock().unwrap_or_else(|e| e.into_inner());
            *tap = on.then(|| install_tap(&entry.query, &entry.changes_seen));
        }
    }

    /// Reactive revision counter for the entry list (bumps on (un)register).
    pub fn entries_rev(&self) -> ArcRwSignal<u64> { self.0.entries_rev.clone() }

    /// Clone-out snapshot of the registry's reactive handles for rendering.
    /// (Entries themselves hold the tap and are not `Clone`.)
    pub fn snapshot(&self) -> Vec<QuerySnapshot> {
        let entries = self.0.entries.read().unwrap_or_else(|e| e.into_inner());
        entries.iter().map(|e| QuerySnapshot { query: e.query.clone(), changes_seen: e.changes_seen.clone() }).collect()
    }

    /// The live feed ring buffer (newest first).
    pub fn feed(&self) -> ArcRwSignal<VecDeque<FeedEntry>> { self.0.feed.clone() }

    /// The connection-state transition log (newest first).
    pub fn conn_log(&self) -> ArcRwSignal<VecDeque<(f64, String)>> { self.0.conn_log.clone() }

    /// Drop accumulated observations (live feed + connection log) so a
    /// re-enable starts fresh instead of replaying stale rows.
    pub fn clear_history(&self) {
        self.0.feed.update(|f| f.clear());
        self.0.conn_log.update(|l| l.clear());
    }
}

/// Convert one changeset into feed rows. Add/Update/Remove get a row each
/// (with their events); the initial load is coalesced into a single row so
/// opening a query doesn't flood the feed.
fn push_changes(feed: &ArcRwSignal<VecDeque<FeedEntry>>, label: &str, collection: &CollectionId, changes: &[QueryChange]) {
    let now = js_sys::Date::now();
    let mut rows: Vec<FeedEntry> = Vec::new();
    let mut initial_count = 0usize;

    for change in changes {
        let kind = match change.kind {
            ChangeKind::Initial => {
                initial_count += 1;
                continue;
            }
            ChangeKind::Add => "add",
            ChangeKind::Update => "update",
            ChangeKind::Remove => "remove",
        };
        // Every non-initial change carries its cause; a row without one still
        // reports the change, just without head or events.
        let (head_short, events) = match &change.cause {
            Some(cause) => (cause.head.to_base64_short(), cause.events.iter().map(summarize_event).collect()),
            None => (String::new(), Vec::new()),
        };
        rows.push(FeedEntry {
            seq: FEED_SEQ.fetch_add(1, Ordering::Relaxed),
            at_ms: now,
            query_label: label.to_string(),
            collection: collection.clone(),
            entity_id: Some(change.entity_id),
            kind,
            count: 1,
            head_short,
            events,
        });
    }

    if initial_count > 0 {
        rows.push(FeedEntry {
            seq: FEED_SEQ.fetch_add(1, Ordering::Relaxed),
            at_ms: now,
            query_label: label.to_string(),
            collection: collection.clone(),
            entity_id: None,
            kind: "initial",
            count: initial_count,
            head_short: String::new(),
            events: Vec::new(),
        });
    }

    if rows.is_empty() {
        return;
    }
    feed.update(|q| {
        for row in rows {
            q.push_front(row);
        }
        q.truncate(FEED_CAP);
    });
}

fn summarize_event(attested: &Attested<Event>) -> FeedEvent {
    let event = &attested.payload;
    FeedEvent { id: event.id(), parent: event.parent.clone(), badges: super::decode::op_badges(event) }
}

/// Start recording connection-state transitions (idempotent). The demo beat:
/// kill the server and the connection card narrates Closed → Connecting →
/// Connected in order, with timestamps.
///
/// API-reality note: the design doc assumed the `ConnectionState` enum was
/// matchable, but ankurah-websocket-client-wasm 0.9.0 keeps its module
/// private (`lib.rs` re-exports only `WebsocketClient`), so the type is
/// unnameable from app code. We can still hold `Read<ConnectionState>` and
/// use the value's strum `Display` ("Connected", "Closed", ...) — which is
/// exactly what the app header already does. Structured `Presence` access
/// (server url / node id / system root via the Connected variant) awaits an
/// upstream re-export; the connection card derives server identity from
/// `node.get_durable_peers()` instead.
pub(crate) fn start_connection_log() {
    let handle = bus();
    let mut guard = handle.0.conn_guard.lock().unwrap_or_else(|e| e.into_inner());
    if guard.is_some() {
        return;
    }
    let log = handle.0.conn_log.clone();
    let read = ws_client().connection_state();

    // Seed with the current state so the log never starts empty.
    use ankurah_signals::Peek;
    push_conn(&log, read.peek().to_string());

    *guard = Some(subscribe_display(&read, move |line| push_conn(&log, line)));
}

/// Subscribe to a `Read<T>` rendering values through `Display`. Exists
/// because `T` here is the ws client's unnameable `ConnectionState`: a bare
/// closure can't annotate its parameter, which `IntoSubscribeListener`'s
/// two-generic blanket impl needs — a generic param pinned by `&Read<T>`
/// resolves it.
fn subscribe_display<T>(read: &Read<T>, f: impl Fn(String) + Send + Sync + 'static) -> SubscriptionGuard
where T: std::fmt::Display + Clone + Send + Sync + 'static {
    read.subscribe(move |value: T| f(value.to_string()))
}

pub(crate) fn stop_connection_log() {
    let handle = bus();
    *handle.0.conn_guard.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

fn push_conn(log: &ArcRwSignal<VecDeque<(f64, String)>>, line: String) {
    log.update(|q| {
        // Collapse consecutive duplicates (the signal can re-emit the same state).
        if q.front().map(|(_, l)| l == &line).unwrap_or(false) {
            return;
        }
        q.push_front((js_sys::Date::now(), line));
        q.truncate(CONN_LOG_CAP);
    });
}
