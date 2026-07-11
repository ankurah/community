use leptos::prelude::*;
use web_sys::window;

use ankurah_signals::Get as AnkurahGet;
use community_model::{RoomView, UserView};

use crate::{
    ctx, editable_text_field::EditableTextField, fmt, members_panel::MembersPanel, qr_code_modal::QRCodeModal,
    room_topic::RoomTopic, ws_client,
};

/// Header component displaying app title, the current room's topic, user
/// info, connection status, and the members / mod log / QR code / sign-out
/// buttons.
#[component]
pub fn Header(current_user: RwSignal<Option<UserView>>, selected_room: RwSignal<Option<RoomView>>) -> impl IntoView {
    let show_qr_code = RwSignal::new(false);
    let show_members = RwSignal::new(false);

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
                        on:click=move |_| show_members.set(true)
                        title="Members"
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
                        class="qrButton"
                        on:click=move |_| show_qr_code.set(true)
                        title="Show QR Code"
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
                    <button
                        class="signOutButton"
                        on:click=move |_| crate::auth::sign_out()
                        title="Sign out"
                    >
                        "Sign out"
                    </button>
                </div>
            </div>
            <Show when=move || show_qr_code.get()>
                <QRCodeModal url=current_url.clone() on_close=move || show_qr_code.set(false) />
            </Show>
            <Show when=move || show_members.get()>
                <MembersPanel on_close=move || show_members.set(false) />
            </Show>
        </>
    }
}
