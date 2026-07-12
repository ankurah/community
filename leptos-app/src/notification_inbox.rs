//! Notification inbox (issue #19) and notification preferences (issue #25).
//!
//! The bell in the header carries a live unseen count; clicking it opens this
//! panel — the members-panel modal shell — listing the signed-in user's own
//! `Notification` rows newest-first (the notification policy scopes reads to
//! `recipient = $jwt.sub`, so the resultset is self-scoped server-side; the
//! client filters on recipient too, belt-and-braces). Clicking a row marks it
//! seen (the recipient may write `seen` on their own rows) and deep-links to
//! the room it points at. A gear toggles the preferences view: `mentions_only`
//! and per-room mutes, stored on the user's single `NotificationPref` row —
//! the server worker reads these when deciding what to create.
//!
//! NOTE: live wiring (queries against `NotificationView` /
//! `NotificationPrefView`) lands with the wave2-data model merge; until then
//! this is the compiled shell: structure, styling, empty/loading states.

use leptos::prelude::*;

use ankurah_signals::Get as AnkurahGet;
use community_model::RoomView;

use crate::ctx;

/// Unseen-notification count for the header bell. Renders nothing at zero —
/// the badge only exists when there is something to see.
#[component]
pub fn NotificationBadge() -> impl IntoView {
    // Wiring lands with the wave2-data merge: a LiveQuery on
    // `recipient = ? AND seen = false`, count rendered as a corner badge.
    ()
}

/// The inbox modal. `selected_room` is the app's room selection signal
/// (threaded from ChatApp through Header) so clicking a room-bearing
/// notification can navigate the chat behind the overlay.
#[component]
pub fn NotificationInbox(
    selected_room: RwSignal<Option<RoomView>>,
    on_close: impl Fn() + Clone + 'static,
) -> impl IntoView {
    // Deep-link wiring (set this signal from a clicked notification's room)
    // lands with the wave2-data merge.
    let _ = &selected_room;

    // Preferences need room names for the mute list; rooms are world-readable
    // and the list is small, so a panel-lifetime subscription is fine.
    let rooms = ctx().query::<RoomView>("true ORDER BY name ASC").expect("failed to create RoomView LiveQuery");

    // Inbox list vs preferences view, swapped by the gear in the header.
    let show_prefs = RwSignal::new(false);

    let on_close_overlay = on_close.clone();
    let on_close_button = on_close.clone();

    let subtitle = move || {
        if show_prefs.get() {
            "Preferences".to_string()
        } else {
            "You're all caught up.".to_string()
        }
    };

    view! {
        <div class="membersOverlay" on:click=move |_| on_close_overlay()>
            <div class="membersContent notificationContent" on:click=|e| e.stop_propagation()>
                <div class="membersHeader">
                    <div class="membersTitles">
                        <h2>"Notifications"</h2>
                        <p class="membersSubtitle">{subtitle}</p>
                    </div>
                    <button
                        class="notifPrefsButton"
                        title="Notification preferences"
                        aria-label="Notification preferences"
                        aria-pressed=move || show_prefs.get().to_string()
                        on:click=move |_| show_prefs.update(|v| *v = !*v)
                    >
                        // Sliders — tune what reaches you.
                        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"
                            stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
                            <path d="M4 21v-7" />
                            <path d="M4 10V3" />
                            <path d="M12 21v-9" />
                            <path d="M12 8V3" />
                            <path d="M20 21v-5" />
                            <path d="M20 12V3" />
                            <path d="M1 14h6" />
                            <path d="M9 8h6" />
                            <path d="M17 16h6" />
                        </svg>
                    </button>
                    <button class="membersCloseButton" aria-label="Close" on:click=move |_| on_close_button()>
                        "×"
                    </button>
                </div>

                <Show
                    when=move || show_prefs.get()
                    fallback=|| {
                        view! {
                            <div class="membersList notifList">
                                <div class="membersState">"You're all caught up."</div>
                            </div>
                        }
                    }
                >
                    <NotificationPrefs rooms=rooms.clone() />
                </Show>

                <p class="membersNote">
                    {move || {
                        if show_prefs.get() {
                            "Preferences apply to new notifications, on every device you're signed in on."
                        } else {
                            "You're notified when someone mentions you."
                        }
                    }}
                </p>
            </div>
        </div>
    }
}

/// The preferences view: a `mentions_only` toggle and per-room mutes. These
/// live on the user's single `NotificationPref` row, created on first change.
///
/// NOTE: until the wave2-data merge lands `NotificationPrefView`, the
/// controls render disabled — there is no row to read or write yet.
#[component]
fn NotificationPrefs(rooms: ankurah::LiveQuery<RoomView>) -> impl IntoView {
    view! {
        <div class="membersList notifPrefs">
            <label class="notifPrefRow">
                <span class="notifPrefLabel">
                    "Only notify me when I'm mentioned"
                    <span class="notifPrefHint">"Skip everything except direct @mentions."</span>
                </span>
                <input type="checkbox" disabled=true />
            </label>

            <div class="notifPrefsSection">"Muted rooms"</div>
            <For
                each=move || rooms.get()
                key=|room: &RoomView| room.id()
                children=move |room: RoomView| {
                    let name = room.name().unwrap_or_default();
                    view! {
                        <label class="notifPrefRow">
                            <span class="notifPrefLabel">
                                <span class="notifPrefRoomHash" aria-hidden="true">"# "</span>
                                {name}
                            </span>
                            <input type="checkbox" disabled=true />
                        </label>
                    }
                }
            />
        </div>
    }
}
