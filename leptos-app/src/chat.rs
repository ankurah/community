use leptos::html::Div;
use leptos::prelude::*;
use send_wrapper::SendWrapper;
use std::sync::Arc;
use wasm_bindgen::JsCast;

use ankurah::EntityId;
use ankurah_signals::Get as AnkurahGet;
use community_model::{MessageView, RoomView, UserView};
use ankurah_virtual_scroll::{ScrollManager, ScrollMode};

use crate::{
    chat_debug_header::ChatDebugHeader, ctx, message_input::MessageInput, message_list::MessageList,
    read_state::ReadStateManager,
};

// ankurah-virtual-scroll tuning. viewport_height is a constructor argument in the
// current API (there is no runtime setter), so we measure the container and feed it in.
const MIN_ROW_HEIGHT: u32 = 40;
const BUFFER_FACTOR: f64 = 2.0;
const DEFAULT_VIEWPORT_HEIGHT: u32 = 600;

/// The scroll manager holds its LiveQuery subscription internally, so remote
/// message adds flow into `visible_set()` and (via the ReactiveGraphObserver
/// bridge) re-render Leptos. `SendWrapper` lets us keep it in a Leptos signal on
/// the single-threaded wasm runtime; `Arc` makes it cheap to clone into handlers.
type Manager = SendWrapper<Arc<ScrollManager<MessageView>>>;

/// Main chat component: message list, input, and scroll controls, backed by
/// `ankurah_virtual_scroll::ScrollManager<MessageView>`.
#[component]
pub fn Chat(
    room: RwSignal<Option<RoomView>>,
    current_user: RwSignal<Option<UserView>>,
    read_state: ReadStateManager,
) -> impl IntoView {
    let show_debug = RwSignal::new(false);
    let editing_message = RwSignal::new(None::<MessageView>);
    let manager = RwSignal::new(None::<Manager>);
    let messages_container_ref = NodeRef::<Div>::new();
    let last_scroll_top = StoredValue::new(0);

    // Query all users once (for author name lookup in message rows).
    let users = ctx().query::<UserView>("true").expect("failed to create UserView LiveQuery");

    // (Re)create the scroll manager whenever the selected room changes. viewport
    // height is a constructor argument, so we measure the container (if already
    // mounted) and fall back to a sensible default on first render.
    Effect::new(move |_| {
        let room_opt = room.get();
        let viewport_height = messages_container_ref
            .get_untracked()
            .map(|el| el.client_height() as u32)
            .filter(|h| *h > 0)
            .unwrap_or(DEFAULT_VIEWPORT_HEIGHT);

        match room_opt {
            Some(current_room) => {
                // Deleted messages are deliberately NOT filtered out: they render
                // as tombstone rows (#10), so the scroll timeline keeps its shape.
                let predicate = crate::queries::predicate("room = ?", [(&current_room.id()).into()])
                    .expect("static message predicate parses");
                match ScrollManager::<MessageView>::new(
                    &ctx(),
                    predicate,
                    "timestamp DESC",
                    MIN_ROW_HEIGHT,
                    BUFFER_FACTOR,
                    viewport_height,
                ) {
                    Ok(m) => {
                        let m = Arc::new(m);
                        let m_start = m.clone();
                        leptos::task::spawn_local(async move { m_start.start().await });
                        manager.set(Some(SendWrapper::new(m)));
                    }
                    Err(e) => {
                        tracing::error!("Failed to create ScrollManager: {:?}", e);
                        manager.set(None);
                    }
                }
            }
            None => manager.set(None),
        }
    });

    // Reactive views of the manager state. Reading `visible_set().get()` under the
    // ReactiveGraphObserver tracks the ankurah signal, so live updates (local and
    // remote) re-render. These are `Copy` Signals, reusable across closures.
    let messages =
        Signal::derive(move || manager.get().map(|m| m.visible_set().get().items).unwrap_or_default());
    let should_auto_scroll =
        Signal::derive(move || manager.get().map(|m| m.visible_set().get().should_auto_scroll).unwrap_or(false));
    let has_more_preceding =
        Signal::derive(move || manager.get().map(|m| m.visible_set().get().has_more_preceding).unwrap_or(false));
    let has_more_following =
        Signal::derive(move || manager.get().map(|m| m.visible_set().get().has_more_following).unwrap_or(false));
    let mode_str = Signal::derive(move || manager.get().map(|m| format!("{:?}", m.mode())).unwrap_or_else(|| "-".to_string()));
    let show_jump_to_current =
        Signal::derive(move || manager.get().map(|m| m.mode() != ScrollMode::Live).unwrap_or(false));
    let item_count = Signal::derive(move || messages.get().len());

    // Auto-scroll to the bottom while in live mode and new messages arrive.
    Effect::new(move |_| {
        let _ = messages.get();
        if should_auto_scroll.get() {
            if let Some(el) = messages_container_ref.get_untracked() {
                el.set_scroll_top(el.scroll_height());
            }
        }
    });

    // Advance the persistent read cursor (#13) whenever the user is looking at
    // the live tail of a room: on room switch, and again as new messages
    // arrive while live (this effect tracks `messages`). Scrolled-up readers
    // keep their cursor — history browsing doesn't mark anything read.
    Effect::new({
        let read_state = read_state.clone();
        move |_| {
            let newest = newest_timestamp(&messages.get());
            if let (Some(m), Some(r), Some(ts)) = (manager.get(), room.get(), newest)
                && m.mode() == ScrollMode::Live
            {
                read_state.mark_read(&r.id().to_base64(), ts);
            }
        }
    });

    view! {
        <Show
            when=move || room.get().is_some()
            fallback=|| {
                view! {
                    <div class="chatContainer">
                        <div class="emptyState">
                            <div class="emptyStateArt" aria-hidden="true">
                                <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8"
                                    stroke-linecap="round" stroke-linejoin="round">
                                    <path d="M21 11.5a8.4 8.4 0 0 1-9 8.4 8.9 8.9 0 0 1-3.9-.9L3 20l1-4.9a8.3 8.3 0 0 1-1-4A8.4 8.4 0 0 1 12 3a8.4 8.4 0 0 1 9 8.5z" />
                                    <path d="M8 10.5h8" />
                                    <path d="M8 14h5" />
                                </svg>
                            </div>
                            <div class="emptyStateTitle">"Select a room to start chatting"</div>
                            <div class="emptyStateHint">
                                "Pick a room from the sidebar — every message syncs live."
                            </div>
                        </div>
                    </div>
                }
            }
        >
            {
                let users = users.clone();
                let read_state = read_state.clone();
                move || {
                    let current_room = room.get()?;
                    let current_user_id = current_user.get().map(|u| u.id().to_base64());
                    let users = users.clone();

                    // Report the first/last *visible* message EntityIds to the manager so
                    // it can paginate (the current API is intersection/EntityId-based).
                    let handle_scroll = {
                        let read_state = read_state.clone();
                        move |_ev: leptos::ev::Event| {
                            let Some(m) = manager.get_untracked() else { return };
                            let Some(container) = messages_container_ref.get_untracked() else { return };
                            let scroll_top = container.scroll_top();
                            let scrolling_backward = scroll_top < last_scroll_top.get_value();
                            last_scroll_top.set_value(scroll_top);
                            if let Some((first, last)) = find_visible_ids(&container) {
                                m.on_scroll(first, last, scrolling_backward);
                            }
                            // Scrolled back to the live tail → the newest message is on
                            // screen, so advance the read cursor.
                            if m.mode() == ScrollMode::Live
                                && let (Some(r), Some(ts)) =
                                    (room.get_untracked(), newest_timestamp(&messages.get_untracked()))
                            {
                                read_state.mark_read(&r.id().to_base64(), ts);
                            }
                        }
                    };

                    // "Jump to current": scroll to bottom; the next scroll event drops the
                    // manager back into live/auto-scroll mode (there is no jumpToLive() API).
                    let handle_jump = {
                        let read_state = read_state.clone();
                        move |_| {
                            if let Some(el) = messages_container_ref.get_untracked() {
                                el.set_scroll_top(el.scroll_height());
                            }
                            if let (Some(r), Some(ts)) =
                                (room.get_untracked(), newest_timestamp(&messages.get_untracked()))
                            {
                                read_state.mark_read(&r.id().to_base64(), ts);
                            }
                        }
                    };

                    Some(view! {
                        <div class="chatContainer">
                            <Show when=move || show_debug.get()>
                                <ChatDebugHeader
                                    mode=mode_str
                                    has_more_preceding=has_more_preceding
                                    has_more_following=has_more_following
                                    should_auto_scroll=should_auto_scroll
                                    item_count=item_count
                                />
                            </Show>

                            <button
                                class="debugToggle"
                                on:click=move |_| show_debug.update(|v| *v = !*v)
                                title=move || if show_debug.get() { "Hide debug info" } else { "Show debug info" }
                            >
                                {move || if show_debug.get() { "▼" } else { "▲" }}
                            </button>

                            <div class="messagesContainer" node_ref=messages_container_ref on:scroll=handle_scroll>
                                <MessageList
                                    messages=messages
                                    users=users.clone()
                                    current_user_id=current_user_id.clone()
                                    editing_message=editing_message
                                />
                            </div>

                            <Show when=move || show_jump_to_current.get()>
                                <button class="jumpToCurrent" on:click=handle_jump.clone()>
                                    "Jump to latest"
                                    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.4"
                                        stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
                                        <path d="M12 5v14" />
                                        <path d="m6 13 6 6 6-6" />
                                    </svg>
                                </button>
                            </Show>

                            <MessageInput
                                room=current_room.clone()
                                current_user=current_user.get()
                                editing_message=editing_message
                                messages=messages
                            />
                        </div>
                    })
                }
            }
        </Show>
    }
}

/// Newest message timestamp in a visible set (they arrive ordered, but max()
/// is cheap and immune to ordering changes).
fn newest_timestamp(messages: &[MessageView]) -> Option<i64> {
    messages.iter().filter_map(|m| m.timestamp().ok()).max()
}

/// Find the first and last message elements currently intersecting the scroll
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
