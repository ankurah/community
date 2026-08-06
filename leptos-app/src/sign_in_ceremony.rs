//! The in-page sign-in ceremony: idp.to's authorization page in a frame on the
//! card the visitor is already looking at, instead of a trip out of the app and
//! back.
//!
//! What the frame is for is the passkey. A `publickey-credentials-get` frame
//! keeps the credential prompt on this page, so signing in never costs the
//! visitor their place — no navigation away, no lost scroll position, no
//! wondering whether they are coming back. Everything that is not "sign in with
//! a credential you already have" — enrolling one, setting up a device,
//! recovery — belongs at the top level, on idp.to's own page, and the ceremony
//! is deliberately not a door to those.
//!
//! The frame can also simply not appear, and say nothing about it. When a
//! document answers `frame-ancestors 'none'` the browser refuses to display it
//! and the parent gets no load error, no event, no readable state. The
//! ceremony only opens on an origin idp.to has registered as an embedder — the
//! rest go straight to the top-level flow — but registration is not a promise:
//! a browser or extension that blocks framed documents, or trouble on idp.to's
//! side, lands in the same silence. That is why the way out is a button that is
//! always on screen rather than something offered once a failure is detected:
//! there is no failure to detect.
//!
//! Custody stays where it was. This component holds the attempt's `state` and
//! nothing else; the PKCE verifier and nonce stay in `sessionStorage` where
//! `auth` put them, the exchange is `auth::complete_sign_in`, and the frame
//! goes away the moment its code is in hand.

use leptos::prelude::*;
use send_wrapper::SendWrapper;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::spawn_local;
use web_sys::{window, MessageEvent};

use crate::auth::{self, FramedAttempt, FramedMessage};

/// How far the ceremony has got. The frame is on screen for `Waiting` only:
/// once a code is in hand the frame has done its work, and once the attempt has
/// failed there is nothing left for it to do.
#[derive(Clone)]
enum Phase {
    Waiting,
    Exchanging,
    Failed(String),
}

/// The modal that holds the frame. Mounted while an attempt is live; closing it
/// unmounts the frame and the message listener together.
///
/// `on_close` abandons the attempt — the × and Escape take it, and a failure
/// leaves it as one of the two ways on, beside the escape hatch.
/// `on_signed_in` receives the minted ankurah session token.
#[component]
pub fn SignInCeremony(
    attempt: FramedAttempt,
    on_close: impl Fn() + Clone + 'static,
    on_signed_in: impl Fn(String) + Clone + 'static,
) -> impl IntoView {
    let phase = RwSignal::new(Phase::Waiting);

    // The attempt's `state`, held for the one message allowed to claim it. The
    // exchange needs it again afterwards, so the copy the listener consumes is
    // its own.
    let attempt_state = attempt.state.clone();
    let expected_state = StoredValue::new(Some(attempt.state.clone()));

    let message_closure = wasm_bindgen::closure::Closure::wrap(Box::new({
        let on_signed_in = on_signed_in.clone();
        move |event: MessageEvent| {
            let verdict = expected_state
                .try_update_value(|expected| auth::read_framed_message(&event, expected))
                .unwrap_or(FramedMessage::Ignored);
            let code = match verdict {
                FramedMessage::Ignored => return,
                FramedMessage::Failed(message) => {
                    phase.set(Phase::Failed(message));
                    return;
                }
                FramedMessage::Accepted { code } => code,
            };

            // The frame is done — unmounting it here is what removes it from
            // the page — and the code becomes a session on the existing path.
            phase.set(Phase::Exchanging);
            let attempt_state = attempt_state.clone();
            let on_signed_in = on_signed_in.clone();
            spawn_local(async move {
                match auth::complete_sign_in(&code, &attempt_state).await {
                    Ok(token) => on_signed_in(token),
                    Err(e) => {
                        tracing::error!("framed sign-in failed: {}", e);
                        phase.set(Phase::Failed(e));
                    }
                }
            });
        }
    }) as Box<dyn FnMut(_)>);

    // Escape closes, as it does for every other overlay in the app. It only
    // reaches us while the parent page holds focus — once the frame has it, the
    // key belongs to idp.to's page — so it is a convenience, not the way out.
    let key_closure = wasm_bindgen::closure::Closure::wrap(Box::new({
        let on_close = on_close.clone();
        move |e: web_sys::KeyboardEvent| {
            if e.key() == "Escape" {
                on_close();
            }
        }
    }) as Box<dyn FnMut(_)>);

    if let Some(w) = window() {
        let _ = w.add_event_listener_with_callback("message", message_closure.as_ref().unchecked_ref());
        let _ = w.add_event_listener_with_callback("keydown", key_closure.as_ref().unchecked_ref());
    }
    let listeners = SendWrapper::new((message_closure, key_closure));
    on_cleanup(move || {
        let (message_closure, key_closure) = listeners.take();
        if let Some(w) = window() {
            let _ = w.remove_event_listener_with_callback("message", message_closure.as_ref().unchecked_ref());
            let _ = w.remove_event_listener_with_callback("keydown", key_closure.as_ref().unchecked_ref());
        }
    });

    let escape = move |_| {
        // The flow that has always worked: hand the whole tab to idp.to. It
        // regenerates every one-time value on the way out, so an attempt
        // abandoned here cannot spoil this one.
        if let Err(e) = auth::start_sign_in() {
            tracing::error!("failed to start top-level sign-in: {:?}", e);
        }
    };

    let close_button = on_close.clone();

    view! {
        // No dismiss-on-scrim-click, unlike the app's other modals: a stray
        // click while the credential prompt is up would take the ceremony down
        // mid-touch. The × and the escape below are the ways out.
        <div class="ceremonyOverlay" role="dialog" aria-modal="true" aria-label="Sign in with idp.to">
            <div class="ceremonyCard">
                <div class="ceremonyHeader">
                    <div>
                        <h2>"Sign in with idp.to"</h2>
                        <p class="ceremonySubtitle">"Use the passkey you already have — this page stays where it is."</p>
                    </div>
                    <button class="ceremonyClose" aria-label="Close" on:click=move |_| close_button()>"×"</button>
                </div>

                <div class="ceremonyStage">
                    {move || match phase.get() {
                        Phase::Waiting => view! {
                            <iframe
                                class="ceremonyFrame"
                                // No `sandbox`: an opaque origin, or a missing
                                // script/form/storage permission, breaks both
                                // idp.to's page and WebAuthn inside it.
                                src=attempt.authorize_url.clone()
                                allow="publickey-credentials-get"
                                title="Sign in with idp.to"
                            ></iframe>
                        }.into_any(),
                        Phase::Exchanging => view! {
                            <div class="ceremonyStatus">
                                <span class="ceremonySpinner" aria-hidden="true"></span>
                                "Finishing sign-in…"
                            </div>
                        }.into_any(),
                        Phase::Failed(message) => view! {
                            <div class="ceremonyStatus ceremonyStatusFailed" role="alert">{message}</div>
                        }.into_any(),
                    }}
                </div>

                // A refused frame paints its own blank rectangle and the parent
                // cannot restyle it or hear about it, so the panel above can sit
                // there empty with nothing to explain it. This line is that
                // explanation, and it is permanent for the same reason the
                // button below is: there is no refusal to detect.
                {move || matches!(phase.get(), Phase::Waiting).then(|| view! {
                    <p class="ceremonyHint">
                        "Panel still blank? Some browsers refuse to show idp.to inside another page."
                    </p>
                })}

                <button class="ceremonyEscape" on:click=escape>
                    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2"
                        stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
                        <path d="M14 4h6v6" />
                        <path d="M20 4 11 13" />
                        <path d="M18 14v5a1 1 0 0 1-1 1H5a1 1 0 0 1-1-1V7a1 1 0 0 1 1-1h5" />
                    </svg>
                    "Open the idp.to sign-in page instead"
                </button>
            </div>
        </div>
    }
}
