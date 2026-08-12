//! Terms acceptance: the community guidelines a reader agrees to before
//! anything they write reaches anybody else.
//!
//! FOR: a member who has never been told the rules cannot break them
//! knowingly, and cannot be moderated fairly for breaking them. Both app
//! stores require the agreement in the app for a product carrying member-
//! written content, and the requirement points at the same thing the community
//! wants anyway — the guidelines in front of the reader, once, before their
//! first contribution, with the moderation the guidelines promise spelled out.
//!
//! HOW A WRITE MEETS IT. Nothing here inspects a write. Every community-owned
//! write entry point calls [`demand_terms`] with what it was going to do: an
//! accepted reader's action runs immediately, in the same click, and an
//! unaccepted reader's action waits behind the modal and runs on Accept (or is
//! dropped on Not now). That shape is what lets a call site keep using
//! `window.prompt` afterwards — the continuation still runs inside the
//! member's own click.
//!
//! PER DEVICE, and honestly labelled as such. Acceptance is a `localStorage`
//! entry holding the version accepted, so a browser that has never accepted is
//! asked wherever the member signs in, and a guidelines revision re-asks
//! everyone by bumping [`TERMS_VERSION`]. There is no server record: a
//! collection recording who agreed to what would be a second source of truth
//! about a member, and v1 buys nothing with it that the modal does not already
//! buy. A browser that refuses storage is asked every time rather than let
//! through — the acceptance we could not write down is one we cannot claim.

use leptos::prelude::*;
use std::cell::RefCell;
use std::sync::OnceLock;
use web_sys::window;

/// Where to reach a human about this community — moderation, a member in
/// trouble, or anything the app itself cannot answer. Both stores require a
/// working address for a product carrying member-written content, and it is
/// rendered from this one constant everywhere it appears (the terms modal and
/// the welcome card's footer).
///
/// DRAFT for Daniel's approval before store submission — confirm this mailbox
/// exists and is read by somebody before the app is submitted.
pub const SUPPORT_CONTACT: &str = "community@ankurah.org";

/// The version of the guidelines this build ships. Bumping it re-asks every
/// reader on every device, which is the whole mechanism for a revision: a
/// stored acceptance names the version it was given, and an acceptance of
/// older text is not an acceptance of this one.
pub const TERMS_VERSION: &str = "2026-08-10";

/// localStorage key holding the accepted version (survives reloads; per
/// browser profile, like the session token).
const LS_TERMS_ACCEPTED: &str = "community_terms_accepted";

/// The community guidelines, one paragraph per entry.
///
/// DRAFT for Daniel's approval before store submission — this is the text a
/// member agrees to and the text an app reviewer reads. Two sentences commit
/// the operators to something and want a deliberate yes: the moderation log is
/// promised as readable by every signed-in member (which is what `policy.json`
/// already does), and the last paragraph promises review of reported content
/// within 24 hours, which is the response window store review asks for.
pub const COMMUNITY_TERMS: &[&str] = &[
    "Be kind. Everybody here is a person, and the room is shared.",
    "Do not post harassment, hate speech, sexual content involving minors, spam, or anything illegal.",
    "Moderators may remove any message and ban any account.",
    "Moderation is lights-on: every removal and every ban goes into the moderation log, which every signed-in member can read.",
    "Report anything that breaks these guidelines, and block any member whose messages you would rather not see.",
    "Reported content is reviewed within 24 hours, and accounts that post it are removed.",
    "There is no tolerance for objectionable content or abusive members.",
];

/// Whether this device has accepted the guidelines this build ships.
///
/// Answers `false` for every reason it cannot answer `true` — no storage, no
/// entry, an entry naming an older version — because each of those is a reader
/// who has not agreed to the text in front of them.
pub fn terms_accepted() -> bool {
    let Some(storage) = local_storage() else { return false };
    matches!(storage.get_item(LS_TERMS_ACCEPTED), Ok(Some(version)) if version == TERMS_VERSION)
}

/// Run `then` once the reader has accepted the guidelines: immediately if they
/// already have on this device, otherwise after they press Accept in the modal.
/// A reader who presses Not now never runs it.
///
/// This is the seam every community-owned write entry point goes through; the
/// chat crate's composer reaches the same gate through [`demand_terms_boxed`].
pub fn demand_terms(then: impl FnOnce() + 'static) { demand_terms_boxed(Box::new(then)) }

/// [`demand_terms`] in the shape a `Box<dyn Fn(...)>` hook can hold, so the
/// chat components can be handed it without naming a generic. `chat_hooks`
/// installs this as the components' `gate_write`, which is what puts the
/// guidelines in front of a member's send and their edit-saves alike.
///
/// Only one demand is pending at a time: the modal is one surface, and a second
/// action raised while it is up replaces the first rather than queueing behind
/// it. Reachable only by a reader who reaches two write affordances without
/// answering the modal in between, and dropping the older one is the honest
/// end for an action they walked away from.
pub fn demand_terms_boxed(then: Box<dyn FnOnce()>) {
    if terms_accepted() {
        then();
        return;
    }
    PENDING.with(|pending| *pending.borrow_mut() = Some(then));
    gate().open.set(true);
}

/// Record acceptance and run whatever was waiting on it. Storage failing is
/// not a reason to refuse the reader their action — they did accept — but it
/// does mean the next visit asks again, which [`terms_accepted`] already says.
fn accept() {
    if let Some(storage) = local_storage()
        && storage.set_item(LS_TERMS_ACCEPTED, TERMS_VERSION).is_err()
    {
        tracing::warn!("could not record terms acceptance — this device will be asked again");
    }
    gate().open.set(false);
    if let Some(action) = PENDING.with(|pending| pending.borrow_mut().take()) {
        action();
    }
}

/// Dismiss without accepting: the pending action is dropped, not deferred.
fn decline() {
    gate().open.set(false);
    PENDING.with(|pending| *pending.borrow_mut() = None);
}

// The action waiting on an answer, if any. A thread-local rather than a signal
// because a `FnOnce` is neither `Clone` nor `Send`, and wasm is
// single-threaded — the same reasoning `SESSION_LIVE` records for its
// `Relaxed` ordering in `main.rs`.
thread_local! {
    static PENDING: RefCell<Option<Box<dyn FnOnce()>>> = const { RefCell::new(None) };
}

/// Owner of the modal's open flag. The `panels()` idiom — an `ArcRwSignal`
/// behind a `static`, so a write entry point deep in the tree can raise the
/// gate without prop-drilling.
#[derive(Clone)]
struct TermsGateState {
    open: ArcRwSignal<bool>,
}

static STATE: OnceLock<TermsGateState> = OnceLock::new();

fn gate() -> TermsGateState { STATE.get_or_init(|| TermsGateState { open: ArcRwSignal::new(false) }).clone() }

fn local_storage() -> Option<web_sys::Storage> { window()?.local_storage().ok().flatten() }

/// The guidelines modal. Mounted once for the whole app; renders nothing until
/// a write entry point raises it.
///
/// Deliberately NOT a member of the panel manager's exclusive surfaces: those
/// are things a reader opens, and this is a question the app asks. It has to be
/// able to appear over an open panel — the report and ban affordances live
/// inside one — and it must not be closed by the app-wide Escape that dismisses
/// them, because dismissing it silently drops the action the reader asked for.
#[component]
pub fn TermsGate() -> impl IntoView {
    let open = gate().open;
    view! {
        {move || {
            open.get()
                .then(|| {
                    view! {
                        <div class="termsOverlay" role="dialog" aria-modal="true" aria-label="Community guidelines">
                            <div class="termsCard">
                                <h2 class="termsTitle">"Community guidelines"</h2>
                                <p class="termsLede">
                                    "Before you post, react, or change anything here, please read these and agree."
                                </p>
                                <ul class="termsList">
                                    {COMMUNITY_TERMS
                                        .iter()
                                        .map(|line| view! { <li>{*line}</li> })
                                        .collect_view()}
                                </ul>
                                <p class="termsContact">
                                    "Questions or problems: "
                                    <a href=format!("mailto:{SUPPORT_CONTACT}")>{SUPPORT_CONTACT}</a>
                                </p>
                                <div class="termsActions">
                                    <button class="termsDecline" on:click=move |_| decline()>
                                        "Not now"
                                    </button>
                                    <button class="termsAccept" on:click=move |_| accept()>
                                        "I agree"
                                    </button>
                                </div>
                                <p class="termsNote">
                                    "Recorded on this device. You will be asked again if the guidelines change."
                                </p>
                            </div>
                        </div>
                    }
                })
        }}
    }
}
