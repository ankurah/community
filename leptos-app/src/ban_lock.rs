//! Banned-client self-lock: the moment the signed-in user's own active `Ban`
//! row syncs (the ban read scope makes exactly those rows self-readable), the
//! chat UI is replaced by a full-screen lockout and the session is signed out
//! shortly after.
//!
//! Scope, stated honestly: this lock is UX, not security. The durable node
//! keeps honoring this session token for reads and writes until it expires —
//! live mid-session revocation is a framework-level gap (FA-1 territory, the
//! guarded-agent follow-up). The *hard* stop is the server's mint gate:
//! `/auth/session` refuses an actively banned user a new session, so once
//! this client signs itself out there is no way back in. The third leg —
//! disabling the user's idp.to account so they can't authenticate anywhere —
//! is out of scope pending the IdP team's design packet (see the SEAM note at
//! the ban call site in members_panel.rs).

use leptos::prelude::*;
use wasm_bindgen_futures::spawn_local;

use ankurah_signals::Get as AnkurahGet;
use community_model::BanView;

use crate::{ctx, queries};

/// How long the lockout screen holds before signing out: long enough to read
/// why you were removed, short enough that the session visibly ends. "Sign
/// out now" is always available for the impatient.
const SIGN_OUT_DELAY_MS: i32 = 10_000;

/// Mounts the watcher LiveQuery and renders nothing until the viewer is
/// banned; then overlays the entire app (opaque, above every modal) and arms
/// the delayed sign-out. Unbanned-in-time is handled: the overlay vanishes
/// and the pending sign-out disarms itself.
#[component]
pub fn BanLock() -> impl IntoView {
    // The viewer's own active bans. The `user = ?` clause is belt-and-braces
    // (the read scope pins non-moderators to their own rows anyway) — but a
    // *moderator* viewer sees every ban row, so without it a mod would lock
    // themselves out by banning someone else.
    let bans = ctx()
        .query::<BanView>(
            queries::selection("user = ? AND active = true", [(&crate::current_user_id()).into()])
                .expect("static ban self-watch selection parses"),
        )
        .expect("failed to create BanView LiveQuery");

    // `None` while in good standing; `Some(reason)` once banned (first
    // non-empty reason across rows, or an empty string for a reasonless ban).
    let ban_reason = Memo::new(move |_| {
        let rows = bans.get();
        rows.iter()
            .filter_map(|ban| ban.reason().ok())
            .find(|reason| !reason.trim().is_empty())
            .or_else(|| (!rows.is_empty()).then(String::new))
    });

    // Arm the delayed sign-out on the first ban sighting. If the ban is
    // lifted before the timer fires, keep the session (the overlay is
    // already gone; the mint gate would re-admit them anyway) and re-arm for
    // any future ban.
    let armed = StoredValue::new(false);
    Effect::new(move |_| {
        if ban_reason.get().is_some() && !armed.get_value() {
            armed.set_value(true);
            spawn_local(async move {
                crate::sleep_ms(SIGN_OUT_DELAY_MS).await;
                // try_*: the component may have been disposed (e.g. a manual
                // sign-out already navigated) by the time the timer fires.
                if ban_reason.try_get_untracked().flatten().is_some() {
                    crate::auth::sign_out();
                } else {
                    let _ = armed.try_set_value(false);
                }
            });
        }
    });

    move || {
        ban_reason.get().map(|reason| {
            let reason = reason.trim().to_string();
            view! {
                <div class="banLock" role="alertdialog" aria-modal="true" aria-labelledby="banLockTitle">
                    <div class="banLockCard">
                        <div class="banLockIcon" aria-hidden="true">
                            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"
                                stroke-linecap="round" stroke-linejoin="round">
                                <circle cx="12" cy="12" r="9" />
                                <path d="m5.6 5.6 12.8 12.8" />
                            </svg>
                        </div>
                        <h1 id="banLockTitle" class="banLockTitle">
                            "You have been removed from this community"
                        </h1>
                        {(!reason.is_empty())
                            .then(|| view! { <p class="banLockReason">{format!("“{}”", reason)}</p> })}
                        <p class="banLockNote">
                            "You will be signed out in a few seconds. If you believe this is a mistake, contact a moderator."
                        </p>
                        <button class="banLockButton" on:click=|_| crate::auth::sign_out()>
                            "Sign out now"
                        </button>
                    </div>
                </div>
            }
        })
    }
}
