//! Profile popover (#15): opened by clicking an author's avatar or display
//! name in a message row. Shows avatar, display name, first-seen month, and
//! IdP-assigned role badges.
//!
//! The role-badge lookup replicates the `userroles` cache pattern from the
//! members panel (server-written display cache; "member" is baseline and gets
//! no badge) — deliberately its own scoped LiveQuery + CSS rather than a
//! dependency on that component.

use leptos::prelude::*;
use send_wrapper::SendWrapper;
use wasm_bindgen::JsCast;
use web_sys::{window, KeyboardEvent, MouseEvent};

use ankurah_signals::Get as AnkurahGet;
use community_model::{UserRolesView, UserView};

use crate::{
    ctx, fmt,
    panels::{panels, Surface},
};

#[component]
pub fn ProfilePopover(
    user: UserView,
    /// Anchor position (viewport coordinates, typically the trigger's
    /// bottom-left corner). Clamped to stay on screen.
    x: i32,
    y: i32,
    on_close: impl Fn() + Clone + 'static,
) -> impl IntoView {
    let pop_ref = NodeRef::<leptos::html::Div>::new();
    let position = RwSignal::new((x, y));

    // Clamp to the viewport once rendered (same approach as the context menu).
    Effect::new({
        let pop_ref = pop_ref.clone();
        move |_| {
            if let Some(el) = pop_ref.get() {
                let rect = el.unchecked_ref::<web_sys::Element>().get_bounding_client_rect();
                let Some(win) = window() else { return };
                let win_w = win.inner_width().ok().and_then(|v| v.as_f64()).unwrap_or(1024.0) as i32;
                let win_h = win.inner_height().ok().and_then(|v| v.as_f64()).unwrap_or(768.0) as i32;
                let adj_x = (x.min(win_w - rect.width() as i32 - 10)).max(10);
                let adj_y = (y.min(win_h - rect.height() as i32 - 10)).max(10);
                position.set((adj_x, adj_y));
            }
        }
    });

    // Move focus into the dialog so Esc works without further tabbing and
    // screen readers announce it.
    Effect::new({
        let pop_ref = pop_ref.clone();
        move |_| {
            if let Some(el) = pop_ref.get() {
                let _ = el.focus();
            }
        }
    });

    // Outside-click + Escape dismiss. Unlike the context menu's forget()'d
    // closures, these are removed on unmount.
    let click_closure = wasm_bindgen::closure::Closure::wrap(Box::new({
        let on_close = on_close.clone();
        let pop_ref = pop_ref.clone();
        move |e: MouseEvent| {
            if let Some(el) = pop_ref.get_untracked() {
                if let Some(target) = e.target() {
                    if let Ok(node) = target.dyn_into::<web_sys::Node>() {
                        if !el.contains(Some(&node)) {
                            on_close();
                        }
                    }
                }
            }
        }
    }) as Box<dyn FnMut(_)>);
    let key_closure = wasm_bindgen::closure::Closure::wrap(Box::new({
        let on_close = on_close.clone();
        move |e: KeyboardEvent| {
            if e.key() == "Escape" {
                // Consumed: the header's window-level Escape (panel manager)
                // skips defaultPrevented events, so only this popover closes.
                e.prevent_default();
                on_close();
            }
        }
    }) as Box<dyn FnMut(_)>);
    if let Some(doc) = window().and_then(|w| w.document()) {
        let _ = doc.add_event_listener_with_callback("mousedown", click_closure.as_ref().unchecked_ref());
        let _ = doc.add_event_listener_with_callback("keydown", key_closure.as_ref().unchecked_ref());
    }
    let closures = SendWrapper::new((click_closure, key_closure));
    on_cleanup(move || {
        let (click_closure, key_closure) = closures.take();
        if let Some(doc) = window().and_then(|w| w.document()) {
            let _ = doc.remove_event_listener_with_callback("mousedown", click_closure.as_ref().unchecked_ref());
            let _ = doc.remove_event_listener_with_callback("keydown", key_closure.as_ref().unchecked_ref());
        }
    });

    let user_id = user.id().to_base64();
    let hue = fmt::hue_class(&user_id);
    // EntityIds are ULIDs, so the User entity's id carries its creation time —
    // which is the user's first sign-in (users are keyed on their OIDC sub).
    let first_seen = format!("First seen {}", fmt::month_year(user.id().to_ulid().timestamp_ms() as i64));

    // Reactive display name (names are editable and update live).
    let user_for_name = user.clone();
    let name = move || {
        let n = user_for_name.display_name().unwrap_or_default();
        if n.trim().is_empty() { "Unknown".to_string() } else { n }
    };
    let name_for_initials = name.clone();
    let aria_label = format!("Profile: {}", name());

    // Role badges from the server-written `userroles` cache, scoped to this
    // user (parameterized per the #17 idiom). A missing row (never signed in
    // since the cache landed) or a bare "member" role renders no badges.
    let roles_query = crate::queries::selection("user = ?", [(&user.id()).into()])
        .ok()
        .and_then(|sel| ctx().query::<UserRolesView>(sel).ok());
    let badge_roles = move || -> Vec<String> {
        let Some(q) = roles_query.as_ref() else { return Vec::new() };
        q.get()
            .iter()
            .filter_map(|row| row.roles().ok())
            .filter_map(|json| {
                json.as_array().map(|arr| arr.iter().filter_map(|v| v.as_str()).map(str::to_string).collect::<Vec<_>>())
            })
            .flatten()
            .filter(|role| role != "member")
            .collect()
    };

    view! {
        <div
            node_ref=pop_ref
            class="profilePopover"
            role="dialog"
            aria-label=aria_label
            tabindex="-1"
            style:position="fixed"
            style:left=move || format!("{}px", position.get().0)
            style:top=move || format!("{}px", position.get().1)
        >
            <div class="profileHeader">
                <div class=format!("profileAvatar {}", hue) aria-hidden="true">
                    {move || fmt::initials(&name_for_initials())}
                </div>
                <div class="profileIdentity">
                    // The name is the doorway to the full member detail (#57):
                    // clicking it swaps this popover for the sidebar.
                    <button
                        type="button"
                        class="profileName"
                        title="View member"
                        on:click={
                            let on_close = on_close.clone();
                            let detail_target = user.id();
                            move |_| {
                                panels().open(Surface::UserDetail(detail_target.clone()));
                                on_close();
                            }
                        }
                    >
                        {name}
                    </button>
                    <div class="profileFirstSeen">{first_seen}</div>
                </div>
            </div>
            {move || {
                let roles = badge_roles();
                (!roles.is_empty())
                    .then(|| {
                        view! {
                            <div class="profileBadges">
                                {roles
                                    .into_iter()
                                    .map(|role| {
                                        view! {
                                            <span class=format!(
                                                "profileBadge role-{}",
                                                role,
                                            )>{fmt::capitalize(&role)}</span>
                                        }
                                    })
                                    .collect_view()}
                            </div>
                        }
                    })
            }}
        </div>
    }
}
