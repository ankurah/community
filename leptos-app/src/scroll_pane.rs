//! The scrolling message timeline, shared by room chat and DM threads.
//!
//! This is `chat.rs`'s machinery lifted out verbatim, not a second
//! implementation of it: the pinned-to-bottom contract is subtle enough
//! (three cooperating effects, a ResizeObserver, and a pixel-truth flag that
//! exists to heal a stranded mode) that a copy would drift within a wave. Room
//! chat and DM threads differ only in which collection they page over, so the
//! pane is generic over the view type and knows nothing about either.
//!
//! What the pane owns:
//!
//! - the `ScrollManager<V>` for the current selection, rebuilt whenever the
//!   caller hands it a new predicate (viewport height is a constructor
//!   argument in ankurah-virtual-scroll 0.9.0, so the container is measured
//!   and fed in);
//! - the auto-scroll effect, which follows the live tail on two OR'd
//!   conditions — the manager saying it is in Live mode, and the PIXEL truth
//!   that the viewport sits at the bottom. The second is the self-heal for the
//!   stranded state where the manager left Live mode but the reader never
//!   touches the wheel, so no scroll EVENT is ever produced to let it back in;
//! - a ResizeObserver on the row stack, which re-pins the tail through
//!   ASYNCHRONOUS row growth (preview cards, reaction chips, images land on
//!   their own signals moments after the rows render and grow them without
//!   firing any scroll event — that is how a first load settles "close but not
//!   at" the bottom);
//! - the scroll handler, which reports the first/last visible row ids to the
//!   manager for pagination and maintains the pixel truth.
//!
//! What the caller owns: the container markup and its two `node_ref`s, the row
//! rendering, and whatever it wants to do when the reader is looking at the
//! live tail (advancing a read cursor, typically).
//!
//! DOM contract, unchanged and load-bearing: each row's bubble carries
//! `data-msg-id` (the base64 entity id). The pane finds visible rows by that
//! attribute, and the e2e suite finds rows the same way.

use std::sync::Arc;

use leptos::html::Div;
use leptos::prelude::*;
use send_wrapper::SendWrapper;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;

use ankurah::ankql::ast::Predicate;
use ankurah::{EntityId, View};
use ankurah_signals::Get as AnkurahGet;
use ankurah_virtual_scroll::{ScrollManager, ScrollMode};

use crate::ctx;

/// ankurah-virtual-scroll tuning, shared by every timeline.
const MIN_ROW_HEIGHT: u32 = 40;
const BUFFER_FACTOR: f64 = 2.0;
const DEFAULT_VIEWPORT_HEIGHT: u32 = 600;

/// `SendWrapper` lets the manager live in a Leptos signal on the
/// single-threaded wasm runtime; `Arc` makes it cheap to clone into handlers.
type Manager<V> = SendWrapper<Arc<ScrollManager<V>>>;

/// A scrolling timeline over one collection. Copy, so handlers and effects can
/// capture it freely — every field is a Leptos handle.
pub struct ScrollPane<V: View + Clone + Send + Sync + 'static> {
    manager: RwSignal<Option<Manager<V>>>,
    /// The scroll container — bind with `node_ref=pane.container_ref`.
    pub container_ref: NodeRef<Div>,
    /// The row stack inside the container: the ResizeObserver's target, and the
    /// reason one wrapping box exists (the rows themselves are flex items).
    pub content_ref: NodeRef<Div>,
    /// The manager's current visible window, oldest-first.
    pub items: Signal<Vec<V>>,
    /// Whether to offer "Jump to latest" (the manager is not in Live mode).
    pub show_jump_to_current: Signal<bool>,
    pub mode_str: Signal<String>,
    pub has_more_preceding: Signal<bool>,
    pub has_more_following: Signal<bool>,
    pub should_auto_scroll: Signal<bool>,
    pub item_count: Signal<usize>,
    /// Pixel-level "the reader is at the bottom", maintained by the scroll
    /// handler. Starts true: a freshly opened timeline renders at the live tail.
    pinned_to_bottom: StoredValue<bool>,
    last_scroll_top: StoredValue<i32>,
}

impl<V: View + Clone + Send + Sync + 'static> Clone for ScrollPane<V> {
    fn clone(&self) -> Self { *self }
}
impl<V: View + Clone + Send + Sync + 'static> Copy for ScrollPane<V> {}

impl<V: View + Clone + Send + Sync + 'static> Default for ScrollPane<V> {
    fn default() -> Self { Self::new() }
}

impl<V: View + Clone + Send + Sync + 'static> ScrollPane<V> {
    pub fn new() -> Self {
        let manager = RwSignal::new(None::<Manager<V>>);
        // Reading `visible_set().get()` under the ReactiveGraphObserver tracks
        // the ankurah signal, so live updates (local and remote) re-render.
        let items = Signal::derive(move || manager.get().map(|m| m.visible_set().get().items).unwrap_or_default());
        Self {
            manager,
            container_ref: NodeRef::new(),
            content_ref: NodeRef::new(),
            items,
            show_jump_to_current: Signal::derive(move || manager.get().map(|m| m.mode() != ScrollMode::Live).unwrap_or(false)),
            mode_str: Signal::derive(move || manager.get().map(|m| format!("{:?}", m.mode())).unwrap_or_else(|| "-".to_string())),
            has_more_preceding: Signal::derive(move || manager.get().map(|m| m.visible_set().get().has_more_preceding).unwrap_or(false)),
            has_more_following: Signal::derive(move || manager.get().map(|m| m.visible_set().get().has_more_following).unwrap_or(false)),
            should_auto_scroll: Signal::derive(move || manager.get().map(|m| m.visible_set().get().should_auto_scroll).unwrap_or(false)),
            item_count: Signal::derive(move || manager.get().map(|m| m.visible_set().get().items.len()).unwrap_or(0)),
            pinned_to_bottom: StoredValue::new(true),
            last_scroll_top: StoredValue::new(0),
        }
    }

    /// Whether the timeline is at its live tail right now (untracked — for
    /// event handlers and read-cursor decisions).
    pub fn is_live(&self) -> bool { self.manager.get_untracked().map(|m| m.mode() == ScrollMode::Live).unwrap_or(false) }

    /// Point the pane at a new selection, or at nothing. Call from an `Effect`
    /// that reads whatever selection signal the caller owns; a `None` predicate
    /// tears the manager down (empty state).
    pub fn set_source(&self, predicate: Option<Predicate>, display_order: &'static str) {
        let Some(predicate) = predicate else {
            self.manager.set(None);
            return;
        };
        // viewport height is a constructor argument, so measure the container
        // if it is already mounted and fall back on first render.
        let viewport_height = self
            .container_ref
            .get_untracked()
            .map(|el| el.client_height() as u32)
            .filter(|h| *h > 0)
            .unwrap_or(DEFAULT_VIEWPORT_HEIGHT);

        match ScrollManager::<V>::new(&ctx(), predicate, display_order, MIN_ROW_HEIGHT, BUFFER_FACTOR, viewport_height) {
            Ok(m) => {
                let m = Arc::new(m);
                let m_start = m.clone();
                leptos::task::spawn_local(async move { m_start.start().await });
                self.manager.set(Some(SendWrapper::new(m)));
            }
            Err(e) => {
                tracing::error!("Failed to create ScrollManager: {:?}", e);
                self.manager.set(None);
            }
        }
    }

    /// Install the tail-following effects. Call once, from the component body.
    pub fn install(&self) {
        let pane = *self;

        // Follow the live tail as items arrive. See the module docs for why the
        // pixel truth is OR'd in rather than trusting the manager's mode alone.
        Effect::new(move |_| {
            let _ = pane.items.get();
            let pinned = pane.pinned_to_bottom.get_value() && !pane.has_more_following.get_untracked();
            if pane.should_auto_scroll.get() || pinned {
                if let Some(el) = pane.container_ref.get_untracked() {
                    el.set_scroll_top(el.scroll_height());
                    // Once more next frame: same-tick layout shifts (composer
                    // autosize, code-block fonts) can move the bottom after
                    // this effect measured it.
                    let el = el.clone();
                    request_animation_frame(move || {
                        el.set_scroll_top(el.scroll_height());
                    });
                }
            }
        });

        // Re-pin through asynchronous row growth.
        Effect::new(move |prev: Option<Option<SendWrapper<ContentResizeGuard>>>| {
            drop(prev); // a selection switch rebinds the refs: disconnect the old observer
            let _ = pane.container_ref.get(); // track both bindings
            let content = pane.content_ref.get()?;
            let callback = Closure::<dyn FnMut()>::new(move || {
                if pane.pinned_to_bottom.get_value()
                    && let Some(el) = pane.container_ref.get_untracked()
                {
                    el.set_scroll_top(el.scroll_height());
                }
            });
            let observer = web_sys::ResizeObserver::new(callback.as_ref().unchecked_ref()).ok()?;
            observer.observe(&content);
            Some(SendWrapper::new(ContentResizeGuard { observer, _callback: callback }))
        });
    }

    /// The container's `on:scroll` handler. `at_live_tail` runs after every
    /// scroll that leaves the reader looking at the newest row — the read-cursor
    /// hook, and the only thing callers customize here.
    pub fn scroll_handler(self, at_live_tail: impl Fn() + Clone + 'static) -> impl Fn(leptos::ev::Event) + Clone {
        let pane = self;
        move |_ev: leptos::ev::Event| {
            let Some(m) = pane.manager.get_untracked() else { return };
            let Some(container) = pane.container_ref.get_untracked() else { return };
            let scroll_top = container.scroll_top();
            let scrolling_backward = scroll_top < pane.last_scroll_top.get_value();
            pane.last_scroll_top.set_value(scroll_top);
            // <=4px slack: fractional zoom/DPI can leave sub-pixel gaps at the
            // true bottom.
            let at_bottom_px = container.scroll_height() - container.client_height() - scroll_top <= 4;
            pane.pinned_to_bottom.set_value(at_bottom_px);
            if let Some((first, last)) = find_visible_ids(&container) {
                m.on_scroll(first, last, scrolling_backward);
            }
            if m.mode() == ScrollMode::Live {
                at_live_tail();
            }
        }
    }

    /// "Jump to latest": scroll to the bottom. The next scroll event drops the
    /// manager back into live/auto-scroll mode (there is no `jump_to_live()`
    /// API in ankurah-virtual-scroll 0.9.0).
    pub fn scroll_to_bottom(&self) {
        if let Some(el) = self.container_ref.get_untracked() {
            el.set_scroll_top(el.scroll_height());
        }
    }
}

/// Disconnects the content ResizeObserver when dropped (selection switch or
/// unmount). The callback closure must outlive the observer, so they travel
/// together.
struct ContentResizeGuard {
    observer: web_sys::ResizeObserver,
    _callback: Closure<dyn FnMut()>,
}

impl Drop for ContentResizeGuard {
    fn drop(&mut self) { self.observer.disconnect(); }
}

/// Find the first and last row elements currently intersecting the scroll
/// container, by their `data-msg-id` (base64 EntityId).
fn find_visible_ids(container: &web_sys::HtmlElement) -> Option<(EntityId, EntityId)> {
    let container_rect = container.get_bounding_client_rect();
    let (top, bottom) = (container_rect.top(), container_rect.bottom());

    let nodes = container.query_selector_all("[data-msg-id]").ok()?;
    let mut first: Option<EntityId> = None;
    let mut last: Option<EntityId> = None;

    for i in 0..nodes.length() {
        let Some(node) = nodes.item(i) else { continue };
        let Ok(el) = node.dyn_into::<web_sys::HtmlElement>() else { continue };
        let rect = el.get_bounding_client_rect();
        if rect.bottom() > top && rect.top() < bottom {
            if let Some(id) = el.get_attribute("data-msg-id").and_then(|s| EntityId::from_base64(&s).ok()) {
                if first.is_none() {
                    first = Some(id);
                }
                last = Some(id);
            }
        }
    }

    match (first, last) {
        (Some(f), Some(l)) => Some((f, l)),
        _ => None,
    }
}
