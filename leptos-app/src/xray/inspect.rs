//! Inspecting a message from outside the component that rendered it.
//!
//! The chat surfaces come from `ankurah-chat-leptos` and know nothing about
//! x-ray. What they leave behind is a pair of attributes on every message
//! bubble — `data-entity-id` and `data-collection` — and this module is the
//! whole of what community does with them: while x-ray is on it marks the
//! document, listens for clicks, and keeps a dashed outline on the bubbles
//! whose entity has more than one head.
//!
//! This is the end state ankurah/community#53 names for the app-side half of
//! x-ray: a data attribute, and handlers the panel installs. Everything below
//! runs only while the mode is on and is torn down when it goes off, so a
//! reader who never opens x-ray pays for one atomic in the components and
//! nothing here.
//!
//! `data-collection` rides along with the id because ankurah 0.9.0 cannot
//! answer "which collection is this entity in" from the id alone. When
//! ankurah/ankurah#362 lands, the attribute and the branch below it both go.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use leptos::prelude::*;
use send_wrapper::SendWrapper;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;

use ankurah::proto::{CollectionId, EntityId};
use ankurah::View as _;
use community_model::{DmMessageView, MessageView};

use crate::ctx;

/// Marks the document while x-ray is on, so the hover wash and the zoom-in
/// cursor can be plain CSS (see XRay.css).
const MODE_CLASS: &str = "xrayOn";

/// The dashed outline on a bubble whose entity has concurrent heads.
const CONCURRENT_CLASS: &str = "xrayConcurrent";

/// Bumped on every install. A resolve that was in flight when the mode went
/// off — or off and on again — finds its generation stale and drops what it
/// was about to build, rather than hanging an effect on a cleaned-up owner or
/// starting a second watcher for an entity already being watched.
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// What the affordances need to exist, and to be taken apart again.
struct Installed {
    /// Which install this is. See [`GENERATION`].
    generation: u64,
    click: Closure<dyn FnMut(web_sys::MouseEvent)>,
    observer: web_sys::MutationObserver,
    _mutations: Closure<dyn FnMut(js_sys::Array)>,
    /// Owns the per-entity effects that watch head clocks. Dropping x-ray
    /// disposes it, and every one of them with it.
    owner: Owner,
    /// Entity ids already being watched, so a bubble that remounts under the
    /// virtual scroller does not start a second watcher.
    watched: HashSet<String>,
    /// Entity ids currently showing concurrent heads.
    concurrent: HashSet<String>,
}

static INSTALLED: OnceLock<Mutex<Option<SendWrapper<Installed>>>> = OnceLock::new();

fn slot() -> &'static Mutex<Option<SendWrapper<Installed>>> { INSTALLED.get_or_init(|| Mutex::new(None)) }

fn document() -> Option<web_sys::Document> { web_sys::window().and_then(|w| w.document()) }

/// Turn the affordances on. Idempotent.
pub fn install() {
    let mut guard = slot().lock().unwrap_or_else(|e| e.into_inner());
    if guard.is_some() {
        return;
    }
    let Some(document) = document() else { return };

    // One delegated listener for every bubble there is or will be. Inner
    // controls — the actions trigger, reaction chips, links, mention chips —
    // keep their own behaviour, which is the same exemption the component used
    // to make for itself.
    let click = Closure::<dyn FnMut(web_sys::MouseEvent)>::new(move |ev: web_sys::MouseEvent| {
        let Some(target) = ev.target() else { return };
        let Ok(element) = target.dyn_into::<web_sys::Element>() else { return };
        if element.closest("button, a").ok().flatten().is_some() {
            return;
        }
        let Ok(Some(bubble)) = element.closest("[data-entity-id]") else { return };
        let Some(target) = inspect_target(&bubble) else { return };
        super::state().open_inspector(target.0, target.1);
    });
    let _ = document.add_event_listener_with_callback("click", click.as_ref().unchecked_ref());

    // Bubbles arrive and leave constantly under the virtual scroller, so the
    // outline cannot be applied once: watch the tree and pick up new ones.
    let mutations = Closure::<dyn FnMut(js_sys::Array)>::new(move |_: js_sys::Array| scan());
    let observer = match web_sys::MutationObserver::new(mutations.as_ref().unchecked_ref()) {
        Ok(observer) => observer,
        Err(e) => {
            // Nothing has been recorded yet, so nothing else will clean this
            // up: undo the listener here and leave the document as we found it.
            tracing::warn!("x-ray could not watch the document for message bubbles: {:?}", e);
            let _ = document.remove_event_listener_with_callback("click", click.as_ref().unchecked_ref());
            return;
        }
    };
    let options = web_sys::MutationObserverInit::new();
    options.set_child_list(true);
    options.set_subtree(true);
    if let Some(body) = document.body() {
        let _ = observer.observe_with_options(&body, &options);
    }

    // The mode class goes on LAST, once everything that has to be taken apart
    // again is about to be recorded — an earlier failure leaves the document
    // untouched rather than marked for handlers that do not exist.
    if let Some(root) = document.document_element() {
        let _ = root.class_list().add_1(MODE_CLASS);
    }

    *guard = Some(SendWrapper::new(Installed {
        generation: GENERATION.fetch_add(1, Ordering::Relaxed) + 1,
        click,
        observer,
        _mutations: mutations,
        owner: Owner::new(),
        watched: HashSet::new(),
        concurrent: HashSet::new(),
    }));
    drop(guard);
    scan();
}

/// Take them apart again.
pub fn uninstall() {
    let Some(installed) = slot().lock().unwrap_or_else(|e| e.into_inner()).take() else { return };
    let installed = installed.take();
    if let Some(document) = document() {
        let _ = document.remove_event_listener_with_callback("click", installed.click.as_ref().unchecked_ref());
        if let Some(root) = document.document_element() {
            let _ = root.class_list().remove_1(MODE_CLASS);
        }
    }
    installed.observer.disconnect();
    installed.owner.cleanup();
    for element in bubbles() {
        let _ = element.class_list().remove_1(CONCURRENT_CLASS);
    }
}

/// Every message bubble in the document right now.
fn bubbles() -> Vec<web_sys::Element> {
    let Some(document) = document() else { return Vec::new() };
    let Ok(nodes) = document.query_selector_all("[data-entity-id]") else { return Vec::new() };
    (0..nodes.length()).filter_map(|i| nodes.item(i)).filter_map(|n| n.dyn_into::<web_sys::Element>().ok()).collect()
}

/// The entity a bubble stands for, and the collection it lives in.
fn inspect_target(bubble: &web_sys::Element) -> Option<(CollectionId, EntityId)> {
    let id = bubble.get_attribute("data-entity-id")?;
    let entity_id = EntityId::from_base64(&id).ok()?;
    let collection = collection_named(&bubble.get_attribute("data-collection")?)?;
    Some((collection, entity_id))
}

/// Resolve a `data-collection` value to the collection it names. Only the two
/// message collections are inspectable this way; anything else is a bubble
/// this build does not know how to open.
fn collection_named(name: &str) -> Option<CollectionId> {
    if name == MessageView::collection().as_str() {
        Some(MessageView::collection())
    } else if name == DmMessageView::collection().as_str() {
        Some(DmMessageView::collection())
    } else {
        None
    }
}

/// Start watching any bubble we have not seen before, then repaint the
/// outlines. Runs on every DOM change while x-ray is on, which is what keeps
/// a row that the scroller just mounted from appearing without its outline.
fn scan() {
    let mut fresh: Vec<(CollectionId, EntityId, String, u64)> = Vec::new();
    let present: HashSet<String> = bubbles().iter().filter_map(|b| b.get_attribute("data-entity-id")).collect();
    {
        let mut guard = slot().lock().unwrap_or_else(|e| e.into_inner());
        let Some(installed) = guard.as_mut() else { return };
        let generation = installed.generation;
        for bubble in bubbles() {
            let Some(id) = bubble.get_attribute("data-entity-id") else { continue };
            if installed.watched.contains(&id) {
                continue;
            }
            let Some((collection, entity_id)) = inspect_target(&bubble) else { continue };
            installed.watched.insert(id.clone());
            fresh.push((collection, entity_id, id, generation));
        }
        // A reader scrolling through a long history mounts and unmounts
        // hundreds of bubbles; a watcher kept for every one of them would hold
        // a view and an effect alive for the whole session. The observer sees
        // the removals, so forget what has left — it is picked up again, from
        // the same live data, if it comes back.
        installed.watched.retain(|id| present.contains(id));
        installed.concurrent.retain(|id| present.contains(id));
    }
    for (collection, entity_id, id, generation) in fresh {
        watch_heads(collection, entity_id, id, generation);
    }
    paint();
}

/// Follow one entity's head clock for as long as x-ray is on, recording
/// whether it has more than one tip.
///
/// The entity is resolved once — local first, then from the peer — and the
/// effect that reads it re-runs whenever it changes, which is how a
/// concurrent write that lands while the reader is looking at the row lights
/// the outline without a reload.
fn watch_heads(collection: CollectionId, entity_id: EntityId, id: String, generation: u64) {
    let owner = {
        let guard = slot().lock().unwrap_or_else(|e| e.into_inner());
        match guard.as_ref() {
            Some(installed) => installed.owner.clone(),
            None => return,
        }
    };
    // The context is taken HERE, before the future defers: `crate::ctx()` is a
    // global in this app, but the entity resolve below is the boundary the
    // component crate draws too, and reading it once keeps the pair honest.
    let ctx = ctx();
    leptos::task::spawn_local(async move {
        let resolved: Option<Box<dyn Fn() -> usize>> = if collection == DmMessageView::collection() {
            ctx.get::<DmMessageView>(entity_id).await.ok().map(|view| {
                Box::new(move || {
                    view.track();
                    view.entity().head().len()
                }) as Box<dyn Fn() -> usize>
            })
        } else {
            ctx.get::<MessageView>(entity_id).await.ok().map(|view| {
                Box::new(move || {
                    view.track();
                    view.entity().head().len()
                }) as Box<dyn Fn() -> usize>
            })
        };
        let Some(watch) = resolved else {
            // Forget it, so the next scan can try again. Left in `watched` a
            // failed resolve would mark the bubble as watched forever and its
            // outline would never appear, however many times it re-mounted.
            forget(&id, generation);
            return;
        };
        // The mode may have gone off — or off and on again — while that
        // resolve was in flight. Hanging an effect on the old owner would
        // build something already cleaned up; on a new one it would duplicate
        // a watcher the fresh scan has already started.
        if !still_current(generation) {
            return;
        }
        owner.with(|| {
            Effect::new(move |_| {
                let concurrent = watch() > 1;
                let mut guard = slot().lock().unwrap_or_else(|e| e.into_inner());
                let Some(installed) = guard.as_mut() else { return };
                if installed.generation != generation {
                    return;
                }
                let changed =
                    if concurrent { installed.concurrent.insert(id.clone()) } else { installed.concurrent.remove(&id) };
                drop(guard);
                if changed {
                    paint();
                }
            });
        });
    });
}

/// Whether `generation` is still the installed one.
fn still_current(generation: u64) -> bool {
    let guard = slot().lock().unwrap_or_else(|e| e.into_inner());
    guard.as_ref().map(|installed| installed.generation == generation).unwrap_or(false)
}

/// Stop treating an entity as watched, so a later scan can try it again.
fn forget(id: &str, generation: u64) {
    let mut guard = slot().lock().unwrap_or_else(|e| e.into_inner());
    let Some(installed) = guard.as_mut() else { return };
    if installed.generation != generation {
        return;
    }
    installed.watched.remove(id);
}

/// Put the outline where the concurrent entities are, and nowhere else.
fn paint() {
    let concurrent = {
        let guard = slot().lock().unwrap_or_else(|e| e.into_inner());
        match guard.as_ref() {
            Some(installed) => installed.concurrent.clone(),
            None => return,
        }
    };
    for bubble in bubbles() {
        let Some(id) = bubble.get_attribute("data-entity-id") else { continue };
        let list = bubble.class_list();
        if concurrent.contains(&id) {
            let _ = list.add_1(CONCURRENT_CLASS);
        } else {
            let _ = list.remove_1(CONCURRENT_CLASS);
        }
    }
}
