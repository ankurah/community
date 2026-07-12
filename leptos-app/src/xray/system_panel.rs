//! The L2 system panel: a non-blocking right slide-over with four cards —
//! this node, connection & peers, registered live queries, and the live
//! event feed. The app stays fully usable while it's open (that's the demo
//! point: kill the server and watch the connection card narrate the
//! reconnect while chat keeps scrolling).

use leptos::prelude::*;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::spawn_local;

use ankurah::proto::EntityId;
use ankurah_jwt_auth::JwtAgent;
use ankurah_signals::{Get as AnkurahGet, With as AnkurahWith};
use community_model::{MessageView, RoomView};

use super::bus::{bus, QuerySnapshot};
use super::feed::FeedCard;
use super::state;
use crate::{ctx, ws_client};

/// How many recent messages the panel's own demo query follows. Bounded so
/// opening the panel costs one small remote predicate, not a full sync.
const PANEL_MESSAGE_LIMIT: usize = 30;

#[component]
pub fn SystemPanel() -> impl IntoView {
    // The panel's own LiveQueries — the v0 feed sources. These exist only
    // while the panel is open (dropped + unregistered on close), so x-ray
    // adds zero standing query load. The integration pass registers the
    // app's real queries (rooms list, per-room messages) alongside these.
    let queries_note = match PanelQueries::create() {
        Ok(guards) => {
            on_cleanup(move || drop(guards));
            None
        }
        Err(e) => Some(format!("x-ray feed queries unavailable: {}", e)),
    };

    // The panel's × IS the off switch — x-ray is one mode, not a panel plus a
    // residue of chips (the dismiss-panel-only half-state read as "stuck on").
    let close = move |_| state().set_enabled(false);

    view! {
        <aside class="xrayPanel" role="complementary" aria-label="X-ray system panel">
            <div class="xrayPanelHeader">
                <div>
                    <h2 class="xrayTitle">"X-ray"</h2>
                    <p class="xrayPanelSub">"live node internals · Alt+X"</p>
                </div>
                <button class="xrayClose" aria-label="Close X-ray panel" on:click=close>"×"</button>
            </div>

            <div class="xrayPanelBody">
                <InspectEntityRow />
                {queries_note.map(|note| view! { <p class="xrayStateNote xrayError">{note}</p> })}
                <NodeCard />
                <ConnectionCard />
                <QueriesCard />
                <FeedCard />
            </div>
        </aside>
    }
}

/// The panel's own registered queries, bundled as one drop guard: on drop it
/// unregisters first (taps hold LiveQuery clones), then releases the queries
/// themselves (dropping the last LiveQuery clone unsubscribes remotely).
struct PanelQueries {
    regs: Vec<super::bus::RegistrationId>,
    _messages: ankurah::LiveQuery<MessageView>,
    _rooms: ankurah::LiveQuery<RoomView>,
}

impl PanelQueries {
    fn create() -> Result<Self, String> {
        let messages = ctx()
            .query::<MessageView>(format!("deleted = false ORDER BY timestamp DESC LIMIT {}", PANEL_MESSAGE_LIMIT).as_str())
            .map_err(|e| e.to_string())?;
        let rooms = ctx().query::<RoomView>("true ORDER BY name ASC").map_err(|e| e.to_string())?;

        let handle = bus();
        let reg_messages = handle.register("x-ray · recent messages", &messages);
        let reg_rooms = handle.register("x-ray · rooms", &rooms);
        Ok(PanelQueries { regs: vec![reg_messages, reg_rooms], _messages: messages, _rooms: rooms })
    }
}

impl Drop for PanelQueries {
    fn drop(&mut self) {
        let handle = bus();
        for reg in self.regs.drain(..) {
            handle.unregister(reg);
        }
    }
}

/// Entity-id input → open the L1 inspector directly (the standalone v0 path
/// to the drawer; message bubbles gain a one-click chip in the integration
/// pass).
#[component]
fn InspectEntityRow() -> impl IntoView {
    let id_input = RwSignal::new(String::new());
    let collection = RwSignal::new("message".to_string());
    let error = RwSignal::new(None::<String>);

    let submit = move || {
        let raw = id_input.get_untracked();
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return;
        }
        match EntityId::from_base64(trimmed) {
            Ok(id) => {
                error.set(None);
                state().open_inspector(collection.get_untracked().as_str().into(), id);
            }
            Err(e) => error.set(Some(format!("not a valid entity id: {}", e))),
        }
    };
    let submit_click = submit; // Copy closure (captures only Copy signals)

    view! {
        <div class="xrayInspectRow">
            <select
                class="xrayInspectSelect"
                aria-label="Collection"
                on:change=move |ev| collection.set(event_target_value(&ev))
            >
                <option value="message" selected>"message"</option>
                <option value="room">"room"</option>
                <option value="user">"user"</option>
            </select>
            <input
                class="xrayInspectInput xrayMono"
                type="text"
                placeholder="entity id (base64) — paste to inspect"
                prop:value=move || id_input.get()
                on:input=move |ev| id_input.set(event_target_value(&ev))
                on:keydown=move |ev| { if ev.key() == "Enter" { submit(); } }
            />
            <button class="xrayInspectGo" on:click=move |_| submit_click()>"Inspect"</button>
            {move || error.get().map(|e| view! { <p class="xrayStateNote xrayError">{e}</p> })}
        </div>
    }
}

/// Card 1: the local node — identity, durability, policy, system, storage.
#[component]
fn NodeCard() -> impl IntoView {
    let node = crate::NODE.get().expect("Node not initialized");

    let node_id = node.id;
    let human = ankurah::proto::human_id::humanize(node_id.to_bytes(), 2);
    let durable = node.durable;
    let policy_ready = crate::AGENT.get().map(JwtAgent::policy_ready).unwrap_or(false);
    let system_ready = node.system.is_system_ready();
    let root_head = node.system.root().map(|r| r.payload.state.head.to_base64_short());

    // Storage counts via raw IndexedDB `count()` — StorageCollection has no
    // stats API in 0.9.0 (design doc ask A5), so this reads the two object
    // stores the wasm engine maintains. Labeled "local cache": it counts
    // what this browser has synced, not the community's total.
    let counts = RwSignal::new(None::<Result<(f64, f64), String>>);
    let refresh = move || {
        spawn_local(async move {
            counts.set(Some(idb_counts().await));
        });
    };
    refresh();

    view! {
        <section class="xrayCard">
            <h3 class="xrayCardTitle">"This node"</h3>
            <div class="xrayDetailRow">
                <span class="xrayMetaLabel">"node"</span>
                <span class="xrayMono xraySelectAll" title=node_id.to_base64()>{node_id.to_base64_short()}</span>
                <span class="xrayHumanName">{format!("“{}”", human)}</span>
            </div>
            <div class="xrayDetailRow">
                <span class="xrayMetaLabel">"role"</span>
                <span class="xrayChip">{if durable { "durable" } else { "ephemeral" }}</span>
                <span class="xrayChip" class:xrayChipOk=policy_ready>
                    {if policy_ready { "policy ready" } else { "policy syncing…" }}
                </span>
                <span class="xrayChip" class:xrayChipOk=system_ready>
                    {if system_ready { "system ready" } else { "joining system…" }}
                </span>
            </div>
            <div class="xrayDetailRow">
                <span class="xrayMetaLabel">"system root"</span>
                {match root_head {
                    Some(head) => view! { <span class="xrayMono">{head}</span> }.into_any(),
                    None => view! { <span class="xrayFaint">"none"</span> }.into_any(),
                }}
            </div>
            <div class="xrayDetailRow">
                <span class="xrayMetaLabel">"local cache"</span>
                {move || match counts.get() {
                    None => view! { <span class="xrayFaint">"counting…"</span> }.into_any(),
                    Some(Ok((entities, events))) => view! {
                        <span>{format!("{} entities · {} events", entities, events)}</span>
                    }.into_any(),
                    Some(Err(e)) => view! { <span class="xrayFaint" title=e>"unavailable"</span> }.into_any(),
                }}
                <button class="xrayMiniButton" title="Recount" on:click=move |_| refresh()>"↻"</button>
            </div>
        </section>
    }
}

/// Card 2: connection & peers — the reactive connection-state signal
/// rendered as a status line plus a transition log, and the durable peer set.
///
/// API-reality note (deviation from the design doc): the `ConnectionState`
/// enum lives in a private module of ankurah-websocket-client-wasm 0.9.0, so
/// app code cannot match its variants or reach the `Presence` payload — only
/// the strum `Display` name is reachable (same as the app header). Server
/// identity below therefore comes from `node.get_durable_peers()` (the
/// durable peer *is* the server), and the endpoint from the app's own
/// `ws_url()`. Structured presence display returns when upstream exports the
/// enum.
#[component]
fn ConnectionCard() -> impl IntoView {
    let node = crate::NODE.get().expect("Node not initialized");
    let conn_log = bus().conn_log();

    // Reading the Read<ConnectionState> under the ReactiveGraphObserver makes
    // all of this re-render on every transition (same pattern as the header).
    let current = move || ws_client().connection_state().get().to_string();
    let status_class = move || match current().as_str() {
        "Connected" => "xrayConnState xrayConnOk",
        "Closed" | "Error" => "xrayConnState xrayConnBad",
        _ => "xrayConnState",
    };

    let peers = move || {
        // Re-list peers whenever the connection transitions.
        let _ = current();
        node.get_durable_peers()
    };

    view! {
        <section class="xrayCard">
            <h3 class="xrayCardTitle">"Connection & peers"</h3>
            <div class="xrayDetailRow">
                <span class="xrayMetaLabel">"state"</span>
                <span class=status_class>{current}</span>
                <span class="xrayFaint xrayMono">{crate::ws_url()}</span>
            </div>
            <div class="xrayDetailRow">
                <span class="xrayMetaLabel">"durable peers"</span>
                {move || {
                    let list = peers();
                    if list.is_empty() {
                        view! { <span class="xrayFaint">"none connected"</span> }.into_any()
                    } else {
                        list.into_iter()
                            .map(|peer| view! {
                                <span class="xrayChip xrayMono" title=peer.to_base64()>{peer.to_base64_short()}</span>
                                <span class="xrayHumanName">
                                    {format!("“{}”", ankurah::proto::human_id::humanize(peer.to_bytes(), 2))}
                                </span>
                            })
                            .collect_view()
                            .into_any()
                    }
                }}
            </div>
            <div class="xrayConnLog">
                <span class="xrayMetaLabel">"transitions"</span>
                <ol class="xrayConnLogList">
                    <For
                        each=move || conn_log.get()
                        key=|(ts, line)| (ts.to_bits(), line.clone())
                        children=|(ts, line)| {
                            let d = js_sys::Date::new(&JsValue::from_f64(ts));
                            let stamp = format!("{:02}:{:02}:{:02}", d.get_hours(), d.get_minutes(), d.get_seconds());
                            view! {
                                <li class="xrayConnLogRow">
                                    <span class="xrayFeedTime xrayMono">{stamp}</span>
                                    <span>{line}</span>
                                </li>
                            }
                        }
                    />
                </ol>
            </div>
        </section>
    }
}

/// Card 3: the LiveQuery registry. Honestly labeled: these are the queries
/// this client registered with the x-ray bus, not the node's full reactor
/// table (which is `pub(crate)` in ankurah 0.9.0 — design doc ask A4).
#[component]
fn QueriesCard() -> impl IntoView {
    let handle = bus();
    let rev = handle.entries_rev();
    let snapshot = move || {
        let _ = rev.get(); // re-snapshot on register/unregister
        bus().snapshot()
    };
    let snapshot_for_empty = snapshot.clone();

    view! {
        <section class="xrayCard">
            <h3 class="xrayCardTitle">"Live queries"</h3>
            <p class="xrayCardSub">"queries this client holds and registered with x-ray"</p>
            <Show when=move || snapshot_for_empty().is_empty()>
                <p class="xrayStateNote">"No queries registered."</p>
            </Show>
            <For
                each=snapshot
                key=|q: &QuerySnapshot| q.id
                children=move |q: QuerySnapshot| {
                    let QuerySnapshot { label, query_id, collection, selection, resultset, error, changes_seen, .. } = q;
                    let resultset_len = resultset.clone();
                    let loaded = resultset;
                    let selection_text = {
                        let selection = selection.clone();
                        move || {
                            let (sel, version) = selection.get();
                            format!("{} · v{}", sel, version)
                        }
                    };
                    view! {
                        <div class="xrayQueryRow">
                            <div class="xrayQueryHead">
                                <span class="xrayQueryLabel">{label}</span>
                                <span class="xrayChip xrayMono" title="query id">{query_id.to_string()}</span>
                                <span class="xrayChip">{collection.to_string()}</span>
                            </div>
                            <code class="xraySelection">{selection_text}</code>
                            <div class="xrayQueryStats">
                                <span>{move || format!("{} results", resultset_len.len())}</span>
                                <span>{move || if loaded.is_loaded() { "loaded" } else { "loading…" }}</span>
                                <span>{move || format!("{} changesets", changes_seen.get())}</span>
                                {move || {
                                    // RetrievalError isn't Clone, so read it in place.
                                    error
                                        .with(|e| e.as_ref().map(|e| e.to_string()))
                                        .map(|msg| view! { <span class="xrayError">{format!("error: {}", msg)}</span> })
                                }}
                            </div>
                        </div>
                    }
                }
            />
        </section>
    }
}

// ---------------------------------------------------------------------------
// Raw-IndexedDB counts (the design doc's sanctioned "hack tier" — replace
// with StorageCollection::stats when ask A5 lands upstream).
// ---------------------------------------------------------------------------

pub(super) fn js_err(e: JsValue) -> String { format!("{:?}", e) }

/// Wrap an IDBRequest's success/error events in a JS Promise. One-shot
/// closures pass ownership to the JS GC (`once_into_js`), so nothing leaks.
fn idb_request_promise(req: web_sys::IdbRequest) -> js_sys::Promise {
    js_sys::Promise::new(&mut |resolve, reject| {
        let req_ok = req.clone();
        let ok = wasm_bindgen::closure::Closure::once_into_js(move |_: web_sys::Event| {
            let _ = resolve.call1(&JsValue::NULL, &req_ok.result().unwrap_or(JsValue::NULL));
        });
        req.set_onsuccess(Some(ok.unchecked_ref()));
        let err = wasm_bindgen::closure::Closure::once_into_js(move |_: web_sys::Event| {
            let _ = reject.call1(&JsValue::NULL, &JsValue::from_str("IndexedDB request failed"));
        });
        req.set_onerror(Some(err.unchecked_ref()));
    })
}

pub(super) async fn await_idb(req: web_sys::IdbRequest) -> Result<JsValue, String> {
    wasm_bindgen_futures::JsFuture::from(idb_request_promise(req)).await.map_err(js_err)
}

/// Count the `entities` / `events` object stores of the app's IndexedDB
/// database (`community_app`, opened by `connect_node`). Store names per
/// ankurah-storage-indexeddb-wasm 0.9.0 `database.rs`.
async fn idb_counts() -> Result<(f64, f64), String> {
    let window = web_sys::window().ok_or("no window")?;
    let factory = window.indexed_db().map_err(js_err)?.ok_or("IndexedDB unavailable")?;
    let open = factory.open("community_app").map_err(js_err)?;
    let db: web_sys::IdbDatabase =
        await_idb(open.unchecked_into()).await?.dyn_into().map_err(|_| "unexpected open result".to_string())?;

    let stores = js_sys::Array::of2(&"entities".into(), &"events".into());
    let tx = db
        .transaction_with_str_sequence_and_mode(&stores, web_sys::IdbTransactionMode::Readonly)
        .map_err(|e| {
            db.close();
            js_err(e)
        })?;
    // Issue both counts before awaiting: an IDB transaction auto-commits once
    // control returns to the event loop with no requests pending.
    let entities_req = tx.object_store("entities").map_err(js_err)?.count().map_err(js_err)?;
    let events_req = tx.object_store("events").map_err(js_err)?.count().map_err(js_err)?;
    let entities = await_idb(entities_req).await?.as_f64().ok_or("count was not a number")?;
    let events = await_idb(events_req).await?.as_f64().ok_or("count was not a number")?;
    db.close();
    Ok((entities, events))
}
