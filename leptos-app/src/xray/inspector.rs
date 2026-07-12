//! The L1 per-entity inspector drawer: fetches an entity's full event
//! history (local IndexedDB dump first, then a backward walk over missing
//! ancestors via `CachedEventGetter`, which policy-checks server-side and
//! persists fetched events locally), lays it out as a DAG, and shows raw
//! event detail for a selected node.
//!
//! Fetches happen only when the drawer opens — never per visible message row.
//! A chat message is typically 1–10 events; the walk is capped at
//! [`FETCH_CAP`] as a guard against pathological histories.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use leptos::prelude::*;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::spawn_local;

use ankurah::core::property::backend::LWWBackend;
use ankurah::core::retrieval::{CachedEventGetter, GetEvents};
use ankurah::proto::{Clock, EntityId, Event, EventId};
use ankurah::View;
use ankurah_jwt_auth::{parse_claims_unverified, JwtContext};
use community_model::{MessageView, RoomView, UserView};

use super::dag::{layout, DagModel, DagNodeInput, DagView};
use super::decode::op_badges;
use super::{state, InspectTarget};
use crate::{can_moderate, ctx};

/// Hard cap on events fetched per inspection (dump + walk combined).
const FETCH_CAP: usize = 500;

#[derive(Clone, Debug, PartialEq)]
struct History {
    dag: DagModel,
    head: Clock,
    total: usize,
    local: usize,
    fetched: usize,
    /// Parent ids that could not be retrieved (offline / policy / cap).
    unresolved: usize,
    /// The entity's current materialized property values (name, value) — the
    /// present state, distinct from any single event's operations. Empty for
    /// collections the inspector doesn't have a typed view for.
    current_state: Vec<(String, String)>,
}

#[derive(Clone, Debug, PartialEq)]
enum Phase {
    Loading,
    /// Deliberate refusal (not an error) — see the deleted-message gate below.
    Refused(String),
    Failed(String),
    Ready(History),
}

/// Fetch + assemble one entity's history.
/// Event ids for one entity, straight off the `events` store's
/// `by_entity_id` index. Works around `dump_entity_events` in
/// ankurah-storage-indexeddb-wasm 0.9.0, which builds its IDBKeyRange from
/// the OWNED `EntityId → JsValue` conversion — a wasm-bindgen class object,
/// which IndexedDB rejects ("parameter is not a valid key") — while rows
/// index the base64 STRING form. Querying the index for primary KEYS (base64
/// event-id strings, exactly as `add_event` wrote them) lets the storage
/// layer's working `get_events` do all the decoding.
async fn local_event_ids(entity_id: EntityId) -> Result<Vec<EventId>, String> {
    use super::system_panel::{await_idb, js_err};
    let window = web_sys::window().ok_or("no window")?;
    let factory = window.indexed_db().map_err(js_err)?.ok_or("IndexedDB unavailable")?;
    let open = factory.open("community_app").map_err(js_err)?;
    let db: web_sys::IdbDatabase = await_idb(open.into())
        .await?
        .dyn_into()
        .map_err(|_| "open did not yield a database".to_string())?;
    let result: Result<Vec<EventId>, String> = async {
        let tx = db.transaction_with_str("events").map_err(js_err)?;
        let store = tx.object_store("events").map_err(js_err)?;
        let index = store.index("by_entity_id").map_err(js_err)?;
        let key = JsValue::from_str(&entity_id.to_base64());
        let req = index.get_all_keys_with_key(&key).map_err(js_err)?;
        let keys = await_idb(req).await?;
        let arr: js_sys::Array =
            keys.dyn_into().map_err(|_| "getAllKeys did not return an array".to_string())?;
        let mut ids = Vec::with_capacity(arr.length() as usize);
        for key in arr.iter() {
            let s = key.as_string().ok_or("event key was not a string")?;
            ids.push(EventId::from_base64(&s).map_err(|e| format!("bad event id: {e}"))?);
        }
        Ok(ids)
    }
    .await;
    db.close();
    result
}

async fn fetch_history(target: InspectTarget, cancelled: Arc<AtomicBool>) -> Phase {
    let InspectTarget { collection, entity_id } = target;

    // Resolve a typed view when we know the collection. This provides (a) the
    // authoritative in-memory head, (b) LWW current-value provenance, and
    // (c) the deleted-message gate.
    let mut head: Option<Clock> = None;
    let mut lww_current: HashMap<EventId, Vec<String>> = HashMap::new();
    let mut current_state: Vec<(String, String)> = Vec::new();
    // Present a missing/errored optional field as an em dash.
    let dash = || "—".to_string();

    // POLICY POSTURE (pending product ruling, raised in community#39 /
    // alongside ankurah#337): `deleted` is an LWW flag, and members can read
    // deleted messages' events — the UI filters them, policy does not. X-ray
    // of a deleted message would resurrect its text from yrs history in one
    // click, so v0 refuses the inspector for non-moderators. This is UI
    // posture, not security: the events remain fetchable by any client with
    // a console. If the ruling says "history is public", delete this block.
    if collection == MessageView::collection() {
        match ctx().get::<MessageView>(entity_id).await {
            Ok(message) => {
                if message.deleted().unwrap_or(false) && !can_moderate() {
                    return Phase::Refused(
                        "This message was deleted. Its edit history is only inspectable by moderators.".to_string(),
                    );
                }
                head = Some(message.entity().head());
                collect_lww_provenance(message.entity(), &["user", "room", "timestamp", "deleted"], &mut lww_current);
                current_state = vec![
                    ("text".into(), message.text().unwrap_or_default()),
                    ("user".into(), message.user().map(|r| r.id().to_base64_short()).unwrap_or_else(|_| dash())),
                    ("room".into(), message.room().map(|r| r.id().to_base64_short()).unwrap_or_else(|_| dash())),
                    ("timestamp".into(), message.timestamp().map(|t| t.to_string()).unwrap_or_else(|_| dash())),
                    ("deleted".into(), message.deleted().map(|b| b.to_string()).unwrap_or_else(|_| dash())),
                    ("edited_at".into(), message.edited_at().ok().flatten().map(|t| t.to_string()).unwrap_or_else(dash)),
                    ("collaborative".into(), message.collaborative().ok().flatten().map(|b| b.to_string()).unwrap_or_else(dash)),
                ];
            }
            Err(e) => return Phase::Failed(format!("Could not load entity: {}", e)),
        }
    } else if collection == RoomView::collection()
        && let Ok(room) = ctx().get::<RoomView>(entity_id).await
    {
        head = Some(room.entity().head());
        collect_lww_provenance(room.entity(), &["created_by"], &mut lww_current);
        current_state = vec![
            ("name".into(), room.name().unwrap_or_default()),
            ("topic".into(), room.topic().ok().flatten().unwrap_or_else(dash)),
            ("created_by".into(), room.created_by().ok().flatten().map(|r| r.id().to_base64_short()).unwrap_or_else(dash)),
        ];
    } else if collection == UserView::collection()
        && let Ok(user) = ctx().get::<UserView>(entity_id).await
    {
        head = Some(user.entity().head());
        collect_lww_provenance(user.entity(), &["oidc_sub"], &mut lww_current);
        current_state = vec![
            ("display_name".into(), user.display_name().unwrap_or_default()),
            ("oidc_sub".into(), user.oidc_sub().ok().flatten().unwrap_or_else(dash)),
        ];
    }

    let col = match ctx().collection(&collection).await {
        Ok(col) => col,
        Err(e) => return Phase::Failed(format!("Could not open collection: {}", e)),
    };

    // 1) Everything already local — one indexed IndexedDB read (see
    //    `local_event_ids` for why this bypasses `dump_entity_events`),
    //    hydrated through the storage layer's working `get_events` path.
    let local_ids = match local_event_ids(entity_id).await {
        Ok(ids) => ids,
        Err(e) => return Phase::Failed(format!("Local event dump failed: {}", e)),
    };
    let dumped = match col.get_events(local_ids).await {
        Ok(events) => events,
        Err(e) => return Phase::Failed(format!("Local event dump failed: {}", e)),
    };

    // Fall back to the locally-stored state head if no view resolved above.
    if head.is_none() {
        head = col.get_state(entity_id).await.ok().map(|s| s.payload.state.head);
    }

    struct Rec {
        event: Event,
        attestations: Option<usize>,
        fetched: bool,
    }
    let mut known: HashMap<EventId, Rec> = HashMap::new();
    for attested in dumped {
        known.insert(
            attested.payload.id(),
            Rec { attestations: Some(attested.attestations.0.len()), fetched: false, event: attested.payload },
        );
    }
    let local_count = known.len();

    // 2) Walk backwards from the head over parents we don't have locally.
    // `CachedEventGetter` checks local storage, then asks a durable peer
    // (server-side `check_read_event` applies) and persists what it fetches —
    // one request per event, fine at message scale (A1 in the design doc is
    // the batched replacement).
    let mut frontier: VecDeque<EventId> = VecDeque::new();
    let mut seen: HashSet<EventId> = known.keys().cloned().collect();
    let enqueue_parents = |event: &Event, seen: &mut HashSet<EventId>, frontier: &mut VecDeque<EventId>| {
        for parent in event.parent.iter() {
            if seen.insert(parent.clone()) {
                frontier.push_back(parent.clone());
            }
        }
    };
    for rec in known.values() {
        for parent in rec.event.parent.iter() {
            if !seen.contains(parent) {
                seen.insert(parent.clone());
                frontier.push_back(parent.clone());
            }
        }
    }
    if let Some(head) = &head {
        for tip in head.iter() {
            if seen.insert(tip.clone()) {
                frontier.push_back(tip.clone());
            }
        }
    }

    let mut unresolved = 0usize;
    if !frontier.is_empty() {
        let node = crate::NODE.get().expect("Node not initialized");
        let cdata = match jwt_cdata() {
            Some(cdata) => cdata,
            None => return Phase::Failed("Not authenticated".to_string()),
        };
        let getter = CachedEventGetter::new(collection.clone(), col.clone(), node, &cdata);
        let mut fetched_ids: Vec<EventId> = Vec::new();
        while let Some(id) = frontier.pop_front() {
            // Drawer closed → stop walking. The phase signal is disposed by
            // then, so the return value is discarded anyway; the point is not
            // to keep issuing per-event fetches in the background.
            if cancelled.load(Ordering::Relaxed) {
                return Phase::Failed("Cancelled".to_string());
            }
            if known.len() >= FETCH_CAP {
                unresolved += 1 + frontier.len();
                break;
            }
            match getter.get_event(&id).await {
                Ok(event) => {
                    enqueue_parents(&event, &mut seen, &mut frontier);
                    fetched_ids.push(id.clone());
                    known.insert(id, Rec { event, attestations: None, fetched: true });
                }
                Err(_) => unresolved += 1,
            }
        }
        // The getter persisted fetched events locally; one batched local read
        // recovers their attestation counts.
        if !fetched_ids.is_empty()
            && let Ok(attested) = col.get_events(fetched_ids).await
        {
            for a in attested {
                let count = a.attestations.0.len();
                if let Some(rec) = known.get_mut(&a.payload.id()) {
                    rec.attestations = Some(count);
                }
            }
        }
    }

    if known.is_empty() {
        return Phase::Failed("No events found for this entity (nothing stored locally and no peer had it).".to_string());
    }

    // Head fallback of last resort: tips = events that are no known event's parent.
    let head = head.unwrap_or_else(|| {
        let mut parented: HashSet<EventId> = HashSet::new();
        for rec in known.values() {
            parented.extend(rec.event.parent.iter().cloned());
        }
        Clock::new(known.keys().filter(|id| !parented.contains(*id)).cloned().collect::<Vec<_>>())
    });

    let total = known.len();
    let fetched = known.values().filter(|r| r.fetched).count();
    let inputs: Vec<DagNodeInput> = known
        .into_values()
        .map(|rec| {
            let id = rec.event.id();
            DagNodeInput {
                badges: op_badges(&rec.event),
                wrote_current: lww_current.get(&id).cloned().unwrap_or_default(),
                parent: rec.event.parent.clone(),
                attestations: rec.attestations,
                fetched: rec.fetched,
                id,
            }
        })
        .collect();

    Phase::Ready(History {
        dag: layout(inputs, &head),
        head,
        total,
        local: local_count,
        fetched,
        unresolved,
        current_state,
    })
}

/// Which LWW properties' *current* values each event wrote, via
/// `LWWBackend::get_event_id`. (The backend has no property enumeration in
/// 0.9.0, so callers pass the model's known LWW property names.)
fn collect_lww_provenance(entity: &ankurah::entity::Entity, props: &[&str], out: &mut HashMap<EventId, Vec<String>>) {
    if let Ok(backend) = entity.get_backend::<LWWBackend>() {
        for prop in props {
            if let Some(event_id) = backend.get_event_id(&(*prop).to_string()) {
                out.entry(event_id).or_default().push((*prop).to_string());
            }
        }
    }
}

/// Build the policy context data for peer requests, same way `ctx()` does.
fn jwt_cdata() -> Option<JwtContext> {
    let token = crate::AUTH_TOKEN.read().ok()?.clone()?;
    let claims = parse_claims_unverified(&token).ok()?;
    Some(JwtContext::from_claims(claims, token))
}

/// The drawer itself. Fetches on open; refetches (cheap — local) when the
/// live feed shows new events for this entity while the drawer is open.
#[component]
pub fn XRayInspector(target: InspectTarget) -> impl IntoView {
    let phase = RwSignal::new(Phase::Loading);
    let selected = RwSignal::new(None::<EventId>);

    // Cancellation: set on unmount so an in-flight walk stops issuing
    // fetches after the drawer closes (rapid open/close would otherwise
    // stack detached walkers).
    let cancelled = Arc::new(AtomicBool::new(false));
    on_cleanup({
        let cancelled = cancelled.clone();
        move || cancelled.store(true, Ordering::Relaxed)
    });
    let fetch_target = target.clone();
    let fetch_cancelled = cancelled.clone();
    let run_fetch = move || {
        let target = fetch_target.clone();
        let cancelled = fetch_cancelled.clone();
        spawn_local(async move {
            let result = fetch_history(target, cancelled).await;
            // On first load, select the newest head tip so the detail pane
            // opens on the latest change rather than empty. A later refetch
            // (new events) leaves an existing selection alone.
            if let Phase::Ready(h) = &result {
                if selected.get_untracked().is_none() {
                    if let Some(tip) = h.head.iter().next() {
                        selected.set(Some(tip.clone()));
                    }
                }
            }
            let _ = phase.try_set(result);
        });
    };
    run_fetch();

    // Live append: when the feed reports events for this entity, refresh.
    // The events were just persisted locally by the applier, so this re-runs
    // the local dump — no extra network.
    let feed = super::bus::bus().feed();
    let watched_id = target.entity_id;
    let last_seen = StoredValue::new(None::<u64>);
    let refetch = run_fetch.clone();
    let retry_fetch = run_fetch.clone();
    Effect::new(move |_| {
        let newest = feed.with(|entries| {
            entries.iter().find(|e| e.entity_id == Some(watched_id)).map(|e| e.seq)
        });
        if let Some(seq) = newest {
            let is_new = last_seen.get_value().map(|prev| seq > prev).unwrap_or(true);
            last_seen.set_value(Some(seq));
            if is_new && matches!(phase.get_untracked(), Phase::Ready(_)) {
                refetch();
            }
        }
    });

    // `close` captures nothing, so it's Copy — reuse it freely.
    let close = move || state().inspect.set(None);
    let close_scrim = close;
    let close_button = close;

    // Escape closes the drawer (scrim click and × also work).
    let esc = window_event_listener(leptos::ev::keydown, move |ev| {
        if ev.key() == "Escape" {
            state().inspect.set(None);
        }
    });
    on_cleanup(move || esc.remove());

    let collection_label = target.collection.to_string();
    let id_full = target.entity_id.to_base64();
    let id_short = target.entity_id.to_base64_short();

    view! {
        <div class="xrayDrawerScrim" on:click=move |_| close_scrim()>
            <aside
                class="xrayDrawer"
                role="dialog"
                aria-label="Entity X-ray"
                on:click=|e| e.stop_propagation()
            >
                <div class="xrayDrawerHeader">
                    <div>
                        <h2 class="xrayTitle">"Entity X-ray"</h2>
                        <p class="xrayDrawerSub">
                            <span class="xrayChip xrayChipCollection">{collection_label}</span>
                            <span class="xrayMono xraySelectAll" title=id_full.clone()>{id_short}</span>
                        </p>
                    </div>
                    <button class="xrayClose" aria-label="Close inspector" on:click=move |_| close_button()>"×"</button>
                </div>

                {move || match phase.get() {
                    Phase::Loading => view! {
                        <div class="xrayStateNote">"Loading event history…"</div>
                    }.into_any(),
                    Phase::Refused(reason) => view! {
                        <div class="xrayStateNote xrayRefused">
                            <strong>"Not shown. "</strong>
                            {reason}
                        </div>
                    }.into_any(),
                    Phase::Failed(error) => {
                        let retry = retry_fetch.clone();
                        view! {
                            <div class="xrayStateNote xrayError">{error}</div>
                            <button class="xrayInspectGo" on:click=move |_| { phase.set(Phase::Loading); retry(); }>
                                "Retry"
                            </button>
                        }.into_any()
                    }
                    Phase::Ready(history) => {
                        let tips: Vec<String> = history.head.iter().map(|id| id.to_base64_short()).collect();
                        let concurrent = tips.len() > 1;
                        let provenance = if history.fetched > 0 {
                            format!("{} events · {} local · {} fetched from peer", history.total, history.local, history.fetched)
                        } else {
                            format!("{} events · all local", history.total)
                        };
                        let current_state = history.current_state.clone();
                        view! {
                            <div class="xrayDrawerBody">
                                {(!current_state.is_empty()).then(|| view! {
                                    <section class="xrayCard">
                                        <h3 class="xrayCardTitle">"Current values"</h3>
                                        {current_state.into_iter().map(|(k, v)| view! {
                                            <div class="xrayDetailRow xrayValueRow">
                                                <span class="xrayMetaLabel">{k}</span>
                                                <span class="xrayValue">{v}</span>
                                            </div>
                                        }).collect_view()}
                                    </section>
                                })}
                                <div class="xrayMetaRow">
                                    <span class="xrayMetaLabel">"head"</span>
                                    <span class="xrayHeadChips" class:xrayHeadConcurrent=concurrent>
                                        {tips.into_iter().map(|tip| view! {
                                            <span class="xrayChip xrayMono">{tip}</span>
                                        }).collect_view()}
                                    </span>
                                    {concurrent.then(|| view! {
                                        <span class="xrayConcurrencyNote">"2+ tips — concurrent edits, not yet merged"</span>
                                    })}
                                </div>
                                <p class="xrayProvenance">
                                    {provenance}
                                    {(history.unresolved > 0)
                                        .then(|| format!(" · {} ancestor(s) unavailable", history.unresolved))}
                                </p>
                                <DagView model=history.dag.clone() selected />
                                <NodeDetail dag=history.dag selected />
                                <p class="xrayFootnote">
                                    "yrs deltas decoded in-app; LWW payloads are opaque client-side until ankurah#337. Events carry no author or wall-clock — that metadata is #337 piece 3."
                                </p>
                            </div>
                        }.into_any()
                    }
                }}
            </aside>
        </div>
    }
}

/// Raw-event detail for the selected DAG node.
#[component]
fn NodeDetail(dag: DagModel, selected: RwSignal<Option<EventId>>) -> impl IntoView {
    move || {
        let Some(id) = selected.get() else {
            return view! { <p class="xrayDetailHint">"Select a node to see the raw event."</p> }.into_any();
        };
        let Some(node) = dag.nodes.iter().find(|n| n.input.id == id) else {
            return view! { <p class="xrayDetailHint">"Select a node to see the raw event."</p> }.into_any();
        };
        let input = node.input.clone();
        let attestation_line = match input.attestations {
            // Community's JwtAgent attests nothing today (`check_event → Ok(None)`),
            // so 0 is the expected, honest value for local commits.
            Some(0) => "0 (unattested ephemeral-client commit)".to_string(),
            Some(n) => format!("{}", n),
            None => "unknown (payload fetched without attestations)".to_string(),
        };
        view! {
            <div class="xrayDetail">
                <div class="xrayDetailRow">
                    <span class="xrayMetaLabel">"event"</span>
                    <span class="xrayMono xraySelectAll">{input.id.to_base64()}</span>
                </div>
                <div class="xrayDetailRow">
                    <span class="xrayMetaLabel">"parents"</span>
                    {if input.parent.is_empty() {
                        view! { <span class="xrayChip">"none — creation event"</span> }.into_any()
                    } else {
                        input.parent.iter().map(|p| view! {
                            <span class="xrayChip xrayMono" title=p.to_base64()>{p.to_base64_short()}</span>
                        }).collect_view().into_any()
                    }}
                </div>
                <div class="xrayDetailRow">
                    <span class="xrayMetaLabel">"ops"</span>
                    <span class="xrayDetailOps">
                        {input.badges.iter().map(|b| view! {
                            <span class="xrayChip">
                                <span class="xrayBackendTag">{b.backend.clone()}</span>
                                {format!(" {} · {} op(s) · {} B", b.summary, b.op_count, b.bytes)}
                            </span>
                        }).collect_view()}
                    </span>
                </div>
                <div class="xrayDetailRow">
                    <span class="xrayMetaLabel">"attested"</span>
                    <span>{attestation_line}</span>
                </div>
                <div class="xrayDetailRow">
                    <span class="xrayMetaLabel">"source"</span>
                    <span>{if input.fetched { "fetched from durable peer (now cached locally)" } else { "local storage" }}</span>
                </div>
                {(!input.wrote_current.is_empty()).then(|| view! {
                    <div class="xrayDetailRow">
                        <span class="xrayMetaLabel">"wrote current"</span>
                        <span>{input.wrote_current.join(", ")} " (LWW values still standing)"</span>
                    </div>
                })}
            </div>
        }
        .into_any()
    }
}
