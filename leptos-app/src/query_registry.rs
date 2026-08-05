//! How a component tells the application which live queries it is holding, so
//! the application can watch them.
//!
//! Components register; the application observes. A component names each
//! long-lived query it creates and keeps the returned guard for as long as it
//! holds the query. An application that wants to see that traffic — a
//! debugging panel, a metrics collector, a test harness — attaches an observer
//! and receives every registration, its lifecycle, and, on request, its
//! changesets. Which observer (if any) is entirely the application's choice:
//! nothing here knows or cares what the queries are for.
//!
//! The registry is a notifier, not a store. It carries the handles ankurah
//! already exposes on a `LiveQuery` — its id, collection, selection, untyped
//! resultset, error — plus a type-erased way to tap its changesets, and hands
//! them to whoever is attached. Observers keep whatever they want to keep.
//!
//! Unattached, a registration costs one relaxed atomic load: the label is not
//! copied, no handles are cloned, and nothing is retained, so an embedder that
//! never attaches an observer pays for the call and nothing else. That comes
//! with an ordering requirement — attach observers during startup, before
//! mounting the components whose queries you want to see, because a
//! registration made while nothing is attached leaves no trace.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, RwLock};

use ankurah::ankql::ast::Selection;
use ankurah::changes::{ChangeKind, ChangeSet, ItemChange};
use ankurah::core::livequery::EntityLiveQuery;
use ankurah::core::resultset::EntityResultSet;
use ankurah::error::RetrievalError;
use ankurah::proto::{Attested, Clock, CollectionId, EntityId, Event, QueryId};
use ankurah::{LiveQuery, View};
use ankurah_signals::{Read, Subscribe, SubscriptionGuard};

/// Identifies one registration from creation to drop.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct RegistrationId(u64);

/// One membership change the reactor delivered for a registered query.
#[derive(Clone, Debug)]
pub struct QueryChange {
    pub kind: ChangeKind,
    pub entity_id: EntityId,
    /// `None` for [`ChangeKind::Initial`]: an item present at first load is
    /// not a change, and the reactor delivers it without events. Every other
    /// kind carries one.
    pub cause: Option<ChangeCause>,
}

/// The events behind one change, and where they left the entity.
#[derive(Clone, Debug)]
pub struct ChangeCause {
    /// The entity's head clock after the change.
    pub head: Clock,
    pub events: Vec<Attested<Event>>,
}

/// What a tap delivers: one call per changeset, carrying that changeset's changes.
pub type ChangeListener = Box<dyn Fn(&[QueryChange]) + Send + Sync>;

/// A live query a component holds, as an observer sees it. Every field is a
/// handle it may keep and read as often as it likes: `selection`, `resultset`
/// and `error` track reactively.
///
/// Keeping the whole struct past `query_unregistered` keeps the query itself
/// running: the tap closes over a strong `LiveQuery` clone, and that clone
/// holds the remote subscription open. The `selection` / `resultset` / `error`
/// handles are independent and pin nothing, so an observer that wants a
/// post-mortem record should keep those rather than the struct.
#[derive(Clone)]
pub struct RegisteredQuery {
    pub id: RegistrationId,
    /// The name the component registered under.
    pub label: String,
    pub query_id: QueryId,
    pub collection: CollectionId,
    /// Reactive (selection, version) — the version bumps on predicate updates.
    pub selection: Read<(Selection, u32)>,
    /// Untyped resultset; `len()` / `is_loaded()` track reactively.
    pub resultset: EntityResultSet,
    pub error: Read<Option<RetrievalError>>,
    /// Subscribes to the underlying query. Built at registration because
    /// subscribing needs the view type, which nothing here carries.
    tap: Arc<dyn Fn(ChangeListener) -> SubscriptionGuard + Send + Sync>,
}

impl RegisteredQuery {
    /// Watch this query's changesets for as long as the returned guard lives.
    /// Kept separate from registration so an observer that wants traffic only
    /// some of the time — while its panel is open, say — can start and stop
    /// without disturbing the registration.
    pub fn watch_changes(&self, on_changeset: impl Fn(&[QueryChange]) + Send + Sync + 'static) -> SubscriptionGuard {
        (self.tap)(Box::new(on_changeset))
    }
}

/// The application's window onto the queries its components hold. Implemented
/// by the application, never by a component.
///
/// The pairing is exact: an observer hears about the registrations made while
/// it was attached and no others, and hears the matching unregistration for
/// each of them exactly once, when the component drops its guard. An id it was
/// never told about never reaches `query_unregistered`, so an observer may
/// treat an unknown id as a defect.
pub trait QueryObserver: Send + Sync + 'static {
    /// A component has begun holding `query`.
    fn query_registered(&self, query: &RegisteredQuery);
    /// The component holding the query with this id has released it.
    fn query_unregistered(&self, id: RegistrationId);
}

/// Held by the component for as long as it holds the query; dropping it tells
/// the observers the query is gone.
///
/// It carries the observers it was announced to rather than looking them up
/// again on drop, which is what keeps the pairing exact: an observer that
/// attached after this registration was made never hears about its end,
/// because it never heard about its beginning.
#[must_use = "the registration lasts only as long as this guard"]
pub struct QueryRegistration(Option<Announced>);

/// One registration and the observers that were told about it.
struct Announced {
    id: RegistrationId,
    observers: Vec<Arc<dyn QueryObserver>>,
}

impl Drop for QueryRegistration {
    fn drop(&mut self) {
        if let Some(announced) = self.0.take() {
            for observer in &announced.observers {
                observer.query_unregistered(announced.id);
            }
        }
    }
}

struct Registry {
    observers: RwLock<Vec<Arc<dyn QueryObserver>>>,
    next_id: AtomicU64,
}

/// Consulted before anything else on the registration path, so a component
/// tree with nothing attached never even reaches the registry.
static ATTACHED: AtomicBool = AtomicBool::new(false);
static REGISTRY: OnceLock<Registry> = OnceLock::new();

fn registry() -> &'static Registry {
    REGISTRY.get_or_init(|| Registry { observers: RwLock::new(Vec::new()), next_id: AtomicU64::new(1) })
}

/// The attached observers, cloned out of the lock so that a callback is free
/// to register queries or attach observers of its own.
fn observers() -> Vec<Arc<dyn QueryObserver>> {
    if !ATTACHED.load(Ordering::Relaxed) {
        return Vec::new();
    }
    registry().observers.read().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Attach an observer, which then sees every subsequent registration.
/// Do this during application startup: registrations made while nothing is
/// attached are not retained, so an observer attached later starts blind to
/// the queries the tree already holds.
///
/// Attachment lasts for the life of the process. There is no detach, and the
/// `Arc` handed over here is held until teardown — an observer that wants to
/// stop doing work should stop in its own callbacks.
pub fn attach_observer(observer: Arc<dyn QueryObserver>) {
    registry().observers.write().unwrap_or_else(|e| e.into_inner()).push(observer);
    ATTACHED.store(true, Ordering::Relaxed);
}

/// Register a live query under a label a human will read in whatever the
/// application attached. Hold the returned guard as long as the component
/// holds the query, then drop it.
pub fn register<R>(label: &str, query: &LiveQuery<R>) -> QueryRegistration
where R: View + Clone + Send + Sync + 'static {
    let observers = observers();
    if observers.is_empty() {
        return QueryRegistration(None);
    }

    let tap: Arc<dyn Fn(ChangeListener) -> SubscriptionGuard + Send + Sync> = {
        let query = query.clone();
        Arc::new(move |listener: ChangeListener| {
            query.subscribe(move |changeset: ChangeSet<R>| {
                let changes: Vec<QueryChange> = changeset.changes.iter().map(describe_change).collect();
                listener(&changes);
            })
        })
    };

    // Untyped resultset via the `EntityLiveQuery` deref: the typed
    // `LiveQuery::resultset` would pin observers to R for no benefit.
    let entity_query: &EntityLiveQuery = query;
    let registered = RegisteredQuery {
        id: RegistrationId(registry().next_id.fetch_add(1, Ordering::Relaxed)),
        label: label.to_string(),
        query_id: query.query_id(),
        collection: R::collection(),
        selection: query.selection(),
        resultset: entity_query.resultset(),
        error: query.error(),
        tap,
    };

    for observer in &observers {
        observer.query_registered(&registered);
    }
    QueryRegistration(Some(Announced { id: registered.id, observers }))
}

/// Flatten one typed change into the untyped record observers see.
fn describe_change<R: View>(change: &ItemChange<R>) -> QueryChange {
    let item = change.entity();
    let kind = change.kind();
    let cause = match kind {
        ChangeKind::Initial => None,
        _ => Some(ChangeCause { head: item.entity().head(), events: change.events().to_vec() }),
    };
    QueryChange { kind, entity_id: item.id(), cause }
}
