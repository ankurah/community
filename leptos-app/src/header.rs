use leptos::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::window;

use ankurah_signals::Get as AnkurahGet;
use community_model::{RoomView, UserView};

use crate::{
    ctx, editable_text_field::EditableTextField, fmt, members_panel::MembersPanel, mod_log_panel::ModLogPanel,
    notification_inbox::{NotificationBadge, NotificationInbox},
    panels::{panels, Surface},
    qr_code_modal::QRCodeModal, room_topic::RoomTopic, user_detail_panel::UserDetailPanel, ws_client,
};

/// Header component displaying app title, the current room's topic, user
/// info, connection status, and the members / mod log / notifications / QR
/// code / sign-out buttons. Also hosts the exclusive surfaces those buttons
/// open — one at a time, via the panel manager (#58) — and the app-wide
/// Escape that closes the open one.
#[component]
pub fn Header(current_user: RwSignal<Option<UserView>>, selected_room: RwSignal<Option<RoomView>>) -> impl IntoView {
    // Escape closes the open surface — one window-level listener for every
    // surface, instead of a per-panel handler. Layering: nested dismissables
    // (context menus, popovers, the composer's edit/mention states) consume
    // their Escape with preventDefault, and element/document listeners run
    // before this window-level one — so an unconsumed Escape is ours. The
    // isComposing guard keeps an IME cancel from also closing a panel.
    let esc_handle = window_event_listener(leptos::ev::keydown, move |ev| {
        if ev.key() == "Escape" && !ev.default_prevented() && !ev.is_composing() && panels().current_untracked().is_some() {
            panels().close();
        }
    });
    on_cleanup(move || esc_handle.remove());

    // Outside-interaction dismiss for the inbox popover (#55). The anchor
    // wrapper below contains both the bell and the mounted popover, so the
    // contains() check exempts them together: a bell mousedown never reaches
    // the close path, which is what lets the bell keep its plain toggle —
    // message_row's ⋯ trigger needs a mousedown-snapshot precisely because
    // its dismiss listener DOES fire for its own trigger; this one
    // structurally can't. On narrow viewports the inbox presents as a
    // full-screen overlay that is a DOM descendant of the anchor, so every
    // mousedown lands "inside" and dismissal stays with the overlay's own
    // backdrop click, exactly like the modal surfaces.
    let bell_anchor = NodeRef::<leptos::html::Div>::new();
    let dismiss_handle = window_event_listener(leptos::ev::mousedown, move |ev| {
        if panels().current_untracked() != Some(Surface::Inbox) {
            return;
        }
        let Some(anchor) = bell_anchor.get_untracked() else { return };
        if let Some(node) = ev.target().and_then(|t| t.dyn_into::<web_sys::Node>().ok()) {
            if !anchor.contains(Some(&node)) {
                panels().close();
            }
        }
    });
    on_cleanup(move || dismiss_handle.remove());

    // Live connection state from the WebSocket client. Reading the reactive
    // `Read<ConnectionState>` under the ReactiveGraphObserver re-renders on change.
    let connection_status = move || ws_client().connection_state().get().to_string();

    let current_url = window().and_then(|w| w.location().href().ok()).unwrap_or_default();

    // Initials avatar for the signed-in user (deterministic hue per user id).
    let avatar_class = move || {
        let hue = current_user.get().map(|u| fmt::hue_class(&u.id().to_base64())).unwrap_or("hue-0");
        format!("userAvatar {}", hue)
    };
    let avatar_initials =
        move || current_user.get().map(|u| fmt::initials(&u.display_name().unwrap_or_default())).unwrap_or_else(|| "·".to_string());

    view! {
        <>
            <div class="header">
                <div class="headerBrand">
                    <div class="brandMark" aria-hidden="true">
                        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"
                            stroke-linecap="round" stroke-linejoin="round">
                            <path d="M7 20h10" />
                            <path d="M10 20c5.5-2.5.8-6.4 3-10" />
                            <path d="M9.5 9.4c1.1.8 1.8 2.2 2.3 3.7-2 .4-3.5.4-4.8-.3-1.2-.6-2.3-1.9-3-4.2 2.8-.5 4.4 0 5.5.8z" />
                            <path d="M14.1 6a7 7 0 0 0-1.1 4c1.9-.1 3.3-.6 4.3-1.4 1-1 1.6-2.3 1.7-4.6-2.7.1-4 1-4.9 2z" />
                        </svg>
                    </div>
                    <h1 class="title">"Ankurah Community"</h1>
                </div>
                <RoomTopic room=selected_room />
                <div class="headerRight">
                    <div class=move || {
                        let status = connection_status();
                        if status == "Connected" {
                            "connectionStatus connected"
                        } else {
                            "connectionStatus disconnected"
                        }
                    }>
                        {move || {
                            let status = connection_status();
                            if status.is_empty() { "Disconnected".to_string() } else { status }
                        }}
                    </div>
                    <div class="userInfo">
                        <div class=avatar_class aria-hidden="true">{avatar_initials}</div>
                        <Show
                            when=move || current_user.get().is_some()
                            fallback=|| view! { <span class="userName">"Loading..."</span> }
                        >
                            {move || {
                                current_user.get().map(|user| {
                                    let user_for_value = user.clone();
                                    let user_for_change = user.clone();
                                    view! {
                                        <EditableTextField
                                            value=Signal::derive(move || user_for_value.display_name().unwrap_or_default())
                                            on_change=move |new_name: String| {
                                                let user = user_for_change.clone();
                                                wasm_bindgen_futures::spawn_local(async move {
                                                    let result = async {
                                                        let trx = ctx().begin();
                                                        let _ = user.edit(&trx)?.display_name().replace(&new_name);
                                                        trx.commit().await?;
                                                        Ok::<_, Box<dyn std::error::Error>>(())
                                                    }
                                                    .await;
                                                    if let Err(e) = result {
                                                        tracing::error!("Failed to update display_name: {}", e);
                                                    }
                                                });
                                            }
                                            class="userName".to_string()
                                        />
                                    }
                                })
                            }}
                        </Show>
                    </div>
                    <button
                        class="membersButton"
                        on:click=move |_| panels().toggle(Surface::Members)
                        title="Members"
                        aria-pressed=move || panels().is_open(&Surface::Members).to_string()
                    >
                        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"
                            stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
                            <path d="M17 21v-2a4 4 0 0 0-4-4H5a4 4 0 0 0-4 4v2" />
                            <circle cx="9" cy="7" r="4" />
                            <path d="M23 21v-2a4 4 0 0 0-3-3.87" />
                            <path d="M16 3.13a4 4 0 0 1 0 7.75" />
                        </svg>
                    </button>
                    <button
                        class="modLogButton"
                        on:click=move |_| panels().toggle(Surface::ModLog)
                        title="Moderation log"
                        aria-pressed=move || panels().is_open(&Surface::ModLog).to_string()
                    >
                        // Gavel — the public record of moderator actions.
                        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"
                            stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
                            <path d="m14 13-8.5 8.5a2.12 2.12 0 1 1-3-3L11 10" />
                            <path d="m16 16 6-6" />
                            <path d="m8 8 6-6" />
                            <path d="m9 7 8 8" />
                            <path d="m21 11-8-8" />
                        </svg>
                    </button>
                    // The bell and its inbox popover share this anchor (#55):
                    // .popoverSurface hangs from the wrapper on wide viewports
                    // with pure CSS — no rect math — so the inbox mounts HERE,
                    // not in the surface match below, and the outside-mousedown
                    // dismiss above checks containment against the wrapper.
                    <div class="headerPopoverAnchor" node_ref=bell_anchor>
                        <button
                            class="notificationButton"
                            on:click=move |_| panels().toggle(Surface::Inbox)
                            title="Notifications"
                            aria-pressed=move || panels().is_open(&Surface::Inbox).to_string()
                            // Without this, a nonzero badge becomes the button's
                            // accessible name ("3, button") — the SVG is
                            // aria-hidden and name-from-content falls to the badge.
                            aria-label="Notifications"
                        >
                            // Bell — your inbox of mentions, with an unseen-count badge.
                            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"
                                stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
                                <path d="M18 8A6 6 0 0 0 6 8c0 7-3 9-3 9h18s-3-2-3-9" />
                                <path d="M13.73 21a2 2 0 0 1-3.46 0" />
                            </svg>
                            <NotificationBadge />
                        </button>
                        {move || {
                            (panels().current() == Some(Surface::Inbox)).then(|| {
                                view! { <NotificationInbox selected_room on_close=move || panels().close() /> }
                            })
                        }}
                    </div>
                    <button
                        class="xrayButton"
                        on:click=move |_| crate::xray::state().toggle()
                        title="X-ray mode"
                        aria-pressed=move || crate::xray::state().enabled.get().to_string()
                    >
                        // Magnifier-plus — inspect the live machinery.
                        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"
                            stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
                            <circle cx="11" cy="11" r="7" />
                            <path d="m21 21-4.3-4.3" />
                            <path d="M8 11h6" />
                            <path d="M11 8v6" />
                        </svg>
                    </button>
                    <button
                        class="qrButton"
                        on:click=move |_| panels().toggle(Surface::Qr)
                        title="Show QR Code"
                        aria-pressed=move || panels().is_open(&Surface::Qr).to_string()
                    >
                        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"
                            stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
                            <rect x="3" y="3" width="7" height="7" rx="1.5" />
                            <rect x="14" y="3" width="7" height="7" rx="1.5" />
                            <rect x="3" y="14" width="7" height="7" rx="1.5" />
                            <path d="M14 14h3v3h-3z" />
                            <path d="M21 14v.01" />
                            <path d="M14 21v.01" />
                            <path d="M21 21v.01" />
                            <path d="M18.5 18.5v.01" />
                        </svg>
                    </button>
                    <a
                        class="accountSettingsButton"
                        href=crate::auth::ACCOUNT_CENTER_URL
                        title="Account settings"
                        aria-label="Account settings"
                    >
                        // Gear — manage name, passkeys, and recovery at idp.to.
                        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"
                            stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
                            <circle cx="12" cy="12" r="3" />
                            <path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 1 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-4 0v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 1 1-2.83-2.83l.06-.06a1.65 1.65 0 0 0 .33-1.82 1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1 0-4h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 1 1 2.83-2.83l.06.06a1.65 1.65 0 0 0 1.82.33H9a1.65 1.65 0 0 0 1-1.51V3a2 2 0 0 1 4 0v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 1 1 2.83 2.83l-.06.06a1.65 1.65 0 0 0-.33 1.82V9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1z" />
                        </svg>
                    </a>
                    <button
                        class="signOutButton"
                        on:click=move |_| crate::auth::sign_out()
                        title="Sign out"
                    >
                        "Sign out"
                    </button>
                </div>
            </div>
            // The one open surface (#58). Components mount fresh per open —
            // exactly what the per-signal `<Show>` blocks did — and their ×
            // buttons / overlay clicks close through the same manager.
            {move || match panels().current() {
                Some(Surface::Qr) => {
                    view! { <QRCodeModal url=current_url.clone() on_close=move || panels().close() /> }.into_any()
                }
                Some(Surface::Members) => view! { <MembersPanel on_close=move || panels().close() /> }.into_any(),
                Some(Surface::ModLog) => view! { <ModLogPanel on_close=move || panels().close() /> }.into_any(),
                // The inbox opens and closes through this same manager, but
                // mounts at its bell anchor up in the header so its popover
                // presentation can anchor with pure CSS (#55).
                Some(Surface::Inbox) => ().into_any(),
                Some(Surface::UserDetail(user_id)) => {
                    view! { <UserDetailPanel user_id on_close=move || panels().close() /> }.into_any()
                }
                None => ().into_any(),
            }}
        </>
    }
}
