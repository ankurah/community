use leptos::prelude::*;
use web_sys::window;

use ankurah_signals::Get as AnkurahGet;
use community_model::UserView;

use crate::{ctx, editable_text_field::EditableTextField, qr_code_modal::QRCodeModal, ws_client};

/// Header component displaying app title, user info, connection status, and QR code button.
#[component]
pub fn Header(current_user: RwSignal<Option<UserView>>) -> impl IntoView {
    let show_qr_code = RwSignal::new(false);

    // Live connection state from the WebSocket client. Reading the reactive
    // `Read<ConnectionState>` under the ReactiveGraphObserver re-renders on change.
    let connection_status = move || ws_client().connection_state().get().to_string();

    let current_url = window().and_then(|w| w.location().href().ok()).unwrap_or_default();

    view! {
        <>
            <div class="header">
                <h1 class="title">"Ankurah Community"</h1>
                <div class="headerRight">
                    <button
                        class="qrButton"
                        on:click=move |_| show_qr_code.set(true)
                        title="Show QR Code"
                    >
                        "📱"
                    </button>
                    <div class="userInfo">
                        <span>"👤"</span>
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
                </div>
            </div>
            <Show when=move || show_qr_code.get()>
                <QRCodeModal url=current_url.clone() on_close=move || show_qr_code.set(false) />
            </Show>
        </>
    }
}
