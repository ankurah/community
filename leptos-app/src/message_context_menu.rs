use leptos::ev::MouseEvent as LeptosMouseEvent;
use leptos::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{KeyboardEvent, MouseEvent, window};

use community_model::MessageView;

use crate::ctx;

/// Context menu for message actions (edit, delete).
/// Appears on right-click of own messages.
#[component]
pub fn MessageContextMenu(
    x: i32,
    y: i32,
    message: MessageView,
    editing_message: RwSignal<Option<MessageView>>,
    on_close: impl Fn() + Clone + 'static,
) -> impl IntoView {
    let menu_ref = NodeRef::<leptos::html::Div>::new();
    let position = RwSignal::new((x, y));

    // Adjust position to prevent menu from going off-screen
    Effect::new({
        let menu_ref = menu_ref.clone();
        move |_| {
            if let Some(menu_el) = menu_ref.get() {
                let rect = menu_el.unchecked_ref::<web_sys::Element>().get_bounding_client_rect();
                let Some(win) = window() else { return };
                let win_width = win.inner_width().ok().and_then(|v| v.as_f64()).unwrap_or(1024.0) as i32;
                let win_height = win.inner_height().ok().and_then(|v| v.as_f64()).unwrap_or(768.0) as i32;

                let mut adjusted_x = x;
                let mut adjusted_y = y;

                // Check right edge
                if x + rect.width() as i32 > win_width {
                    adjusted_x = win_width - rect.width() as i32 - 10;
                }

                // Check bottom edge
                if y + rect.height() as i32 > win_height {
                    adjusted_y = win_height - rect.height() as i32 - 10;
                }

                // Check left edge
                if adjusted_x < 10 {
                    adjusted_x = 10;
                }

                // Check top edge
                if adjusted_y < 10 {
                    adjusted_y = 10;
                }

                position.set((adjusted_x, adjusted_y));
            }
        }
    });

    // Handle click outside and escape key
    Effect::new({
        let on_close = on_close.clone();
        let menu_ref = menu_ref.clone();
        move |_| {
            let on_close_click = on_close.clone();
            let on_close_key = on_close.clone();
            let menu_ref_click = menu_ref.clone();

            let click_handler = wasm_bindgen::closure::Closure::wrap(Box::new(move |e: MouseEvent| {
                if let Some(menu_el) = menu_ref_click.get() {
                    if let Some(target) = e.target() {
                        if let Ok(target_el) = target.dyn_into::<web_sys::Node>() {
                            if !menu_el.contains(Some(&target_el)) {
                                on_close_click();
                            }
                        }
                    }
                }
            }) as Box<dyn FnMut(_)>);

            let key_handler = wasm_bindgen::closure::Closure::wrap(Box::new(move |e: KeyboardEvent| {
                if e.key() == "Escape" {
                    on_close_key();
                }
            }) as Box<dyn FnMut(_)>);

            if let Some(doc) = window().and_then(|w| w.document()) {
                let _ = doc.add_event_listener_with_callback("mousedown", click_handler.as_ref().unchecked_ref());
                let _ = doc.add_event_listener_with_callback("keydown", key_handler.as_ref().unchecked_ref());
            }

            click_handler.forget();
            key_handler.forget();
        }
    });

    let handle_edit = {
        let on_close = on_close.clone();
        let message = message.clone();
        move |_: LeptosMouseEvent| {
            editing_message.set(Some(message.clone()));
            on_close();
        }
    };

    let handle_delete = move |_: LeptosMouseEvent| {
        let message = message.clone();
        let on_close = on_close.clone();
        wasm_bindgen_futures::spawn_local(async move {
            match (|| async {
                let trx = ctx().begin();
                let mutable = message.edit(&trx)?;
                let _ = mutable.deleted().set(&true);
                trx.commit().await?;
                Ok::<_, Box<dyn std::error::Error>>(())
            })()
            .await
            {
                Ok(_) => tracing::info!("Message deleted"),
                Err(e) => tracing::error!("Failed to delete message: {}", e),
            }
            on_close();
        });
    };

    view! {
        <div
            node_ref=menu_ref
            class="contextMenu"
            style:position="fixed"
            style:left=move || format!("{}px", position.get().0)
            style:top=move || format!("{}px", position.get().1)
        >
            <button class="contextMenuItem" on:click=handle_edit>
                <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"
                    stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
                    <path d="M17 3a2.8 2.8 0 1 1 4 4L7.5 20.5 2 22l1.5-5.5z" />
                </svg>
                "Edit message"
            </button>
            <button class="contextMenuItem contextMenuItemDanger" on:click=handle_delete>
                <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"
                    stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
                    <path d="M3 6h18" />
                    <path d="M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6" />
                    <path d="M8 6V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2" />
                </svg>
                "Delete"
            </button>
        </div>
    }
}
