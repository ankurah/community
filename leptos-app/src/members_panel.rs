use leptos::prelude::*;

use ankurah_signals::Get as AnkurahGet;
use community_model::UserView;

use crate::{ctx, fmt};

/// Read-only member directory. Every signed-in user sees the identical view:
/// all community members with avatar, display name, and IdP-assigned role
/// badges. Deliberately no controls of any kind — roles are managed by the
/// identity provider, not from inside the app.
#[component]
pub fn MembersPanel(on_close: impl Fn() + Clone + 'static) -> impl IntoView {
    // All users, live — new sign-ins appear while the panel is open.
    let users = ctx().query::<UserView>("true").expect("failed to create UserView LiveQuery");

    let users_for_count = users.clone();
    let users_for_loading = users.clone();
    let users_for_empty = users.clone();
    let users_for_list = users.clone();

    // "Loading…" until the query settles, then a live member count.
    let subtitle = move || {
        if !users_for_count.loaded() {
            "Loading\u{2026}".to_string()
        } else {
            match users_for_count.get().len() {
                1 => "1 member".to_string(),
                n => format!("{} members", n),
            }
        }
    };

    let on_close_overlay = on_close.clone();
    let on_close_button = on_close.clone();

    view! {
        <div class="membersOverlay" on:click=move |_| on_close_overlay()>
            <div class="membersContent" on:click=|e| e.stop_propagation()>
                <div class="membersHeader">
                    <div class="membersTitles">
                        <h2>"Members"</h2>
                        <p class="membersSubtitle">{subtitle}</p>
                    </div>
                    <button class="membersCloseButton" aria-label="Close" on:click=move |_| on_close_button()>
                        "×"
                    </button>
                </div>

                <div class="membersList">
                    <Show when=move || !users_for_loading.loaded()>
                        <div class="membersState">"Loading members\u{2026}"</div>
                    </Show>
                    <Show when={
                        let users = users_for_empty.clone();
                        move || users.loaded() && users.get().is_empty()
                    }>
                        <div class="membersState">"No members yet."</div>
                    </Show>
                    <For
                        each={
                            let users = users_for_list.clone();
                            move || {
                                let mut items = users.get();
                                // Stable, human order: name (case-insensitive), then id.
                                items.sort_by_cached_key(|u| {
                                    (u.display_name().unwrap_or_default().to_lowercase(), u.id().to_base64())
                                });
                                items
                            }
                        }
                        key=|user: &UserView| user.id()
                        children=move |user: UserView| view! { <MemberRow user /> }
                    />
                </div>

                <p class="membersNote">
                    "Roles are managed by the identity provider and take effect at next sign-in."
                </p>
            </div>
        </div>
    }
}

/// One directory row: initials avatar (deterministic hue), display name.
#[component]
fn MemberRow(user: UserView) -> impl IntoView {
    let hue = fmt::hue_class(&user.id().to_base64());

    // Reactive: display names are editable and update live.
    let user_for_name = user.clone();
    let name = move || {
        let n = user_for_name.display_name().unwrap_or_default();
        if n.trim().is_empty() { "Unknown".to_string() } else { n }
    };
    let name_for_initials = name.clone();

    view! {
        <div class="memberRow">
            <div class=format!("memberAvatar {}", hue) aria-hidden="true">
                {move || fmt::initials(&name_for_initials())}
            </div>
            <span class="memberName">{name}</span>
        </div>
    }
}
