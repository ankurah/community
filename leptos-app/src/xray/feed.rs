//! The live event feed card: a bounded, newest-first stream of `ItemChange`s
//! from every registered query. Each row is a real reactor notification —
//! event ids, parent clocks, and per-backend op sizes, not a simulation.
//! Clicking a row opens the L1 inspector for that entity.

use leptos::prelude::*;
use wasm_bindgen::JsValue;

use super::bus::{bus, FeedEntry, FEED_CAP};
use super::state;

/// "15:04:07" local wall-clock for feed rows (seconds matter here).
fn clock_hms(ts_ms: f64) -> String {
    let d = js_sys::Date::new(&JsValue::from_f64(ts_ms));
    format!("{:02}:{:02}:{:02}", d.get_hours(), d.get_minutes(), d.get_seconds())
}

#[component]
pub fn FeedCard() -> impl IntoView {
    let feed = bus().feed();
    let feed_for_empty = feed.clone();

    view! {
        <section class="xrayCard">
            <h3 class="xrayCardTitle">"Live event feed"</h3>
            <p class="xrayCardSub">
                {format!("Changesets from registered queries · newest first · last {}", FEED_CAP)}
            </p>
            <Show when=move || feed_for_empty.with(|f| f.is_empty())>
                <p class="xrayStateNote">"Nothing yet — send a message or edit one to see its events land."</p>
            </Show>
            <ol class="xrayFeedList">
                <For
                    each=move || feed.get()
                    key=|entry: &FeedEntry| entry.seq
                    children=move |entry: FeedEntry| view! { <FeedRow entry /> }
                />
            </ol>
        </section>
    }
}

#[component]
fn FeedRow(entry: FeedEntry) -> impl IntoView {
    let FeedEntry { at_ms, query_label, collection, entity_id, kind, count, head_short, events, .. } = entry;

    let clickable = entity_id.is_some();
    let target_collection = collection.clone();
    let on_click = move |_| {
        if let Some(id) = entity_id {
            state().open_inspector(target_collection.clone(), id);
        }
    };

    let summary = match entity_id {
        Some(id) => format!("{}/{}", collection, id.to_base64_short()),
        None => format!("{} · {} item(s)", collection, count),
    };

    view! {
        <li>
            <button
                class="xrayFeedRow"
                class:xrayFeedRowStatic=!clickable
                disabled=!clickable
                title=if clickable { "Open in inspector" } else { "Initial query load" }
                on:click=on_click
            >
                <span class="xrayFeedTime xrayMono">{clock_hms(at_ms)}</span>
                <span class=format!("xrayKind xrayKind-{}", kind)>{kind}</span>
                <span class="xrayFeedSummary xrayMono">{summary}</span>
                <span class="xrayFeedQuery">{query_label}</span>
                {(!head_short.is_empty()).then(|| view! {
                    <span class="xrayChip xrayMono" title="entity head after this change">{head_short.clone()}</span>
                })}
                {(!events.is_empty()).then(|| view! {
                    <span class="xrayFeedEvents">
                        {events.iter().map(|ev| {
                            let ops = if ev.badges.is_empty() {
                                "no ops".to_string()
                            } else {
                                ev.badges
                                    .iter()
                                    .map(|b| format!("{} {} · {} B", b.backend, b.summary, b.bytes))
                                    .collect::<Vec<_>>()
                                    .join("  ")
                            };
                            let parent = if ev.parent.is_empty() {
                                "creation".to_string()
                            } else {
                                format!("← {}", ev.parent.to_base64_short())
                            };
                            view! {
                                <span class="xrayFeedEvent">
                                    <span class="xrayMono" title=ev.id.to_base64()>{ev.id.to_base64_short()}</span>
                                    <span class="xrayFeedParent xrayMono" title="parent clock">{parent}</span>
                                    <span class="xrayFeedOps">{ops}</span>
                                </span>
                            }
                        }).collect_view()}
                    </span>
                })}
            </button>
        </li>
    }
}
