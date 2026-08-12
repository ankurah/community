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
//! side, lands in the same silence. The card used to answer that case with a
//! permanent way out — a button to the top-level flow, permanent because with
//! no failure to detect there is no moment to offer it. It was removed on a
//! ruling that the card is the frame and nothing else, so on a registered
//! origin a blocked frame now has no in-app fallback: the visitor closes the
//! ceremony, and sign-in waits until frames work. The top-level flow still
//! serves origins idp.to has not registered.
//!
//! Custody stays where it was. This component holds the attempt's `state` and
//! nothing else; the PKCE verifier and nonce stay in `sessionStorage` where
//! `auth` put them, the exchange is `auth::complete_sign_in`, and the frame
//! goes away the moment its code is in hand.
//!
//! Inside the mobile shell there is no ceremony at all. [`SignInFlow`] is
//! still the app's one way in, but what it opens there is the system's
//! sign-in sheet (`auth::start_native_sign_in`), because the shell's web view
//! shares no credential store with the browser and a frame in it would offer
//! no passkey the member already has.

use std::cell::Cell;
use std::rc::Rc;

use leptos::prelude::*;
use send_wrapper::SendWrapper;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::spawn_local;
use web_sys::{window, MessageEvent};

use crate::auth::{self, FramedAttempt, FramedMessage};

/// The app's one way into a sign-in, shared by the two places that offer one:
/// the card a visitor lands on when they cannot have a session at all, and an
/// anonymous reader reaching for something only a member can do.
///
/// FOR: those two places must behave identically. Both try the framed
/// ceremony, both hand the tab to idp.to where a frame would be refused, both
/// open the system's sheet instead inside the mobile shell, and all of those
/// finish the same way. Written once, they cannot drift — and the idempotence
/// the chat components require of the reader-facing one is a property of this
/// type rather than of each call site.
#[derive(Clone, Copy)]
pub struct SignInFlow {
    /// The live framed attempt, while one is on screen.
    attempt: RwSignal<Option<FramedAttempt>>,
    /// True while the mobile shell's sign-in sheet is up. The sheet is the
    /// system's window, not ours, so there is nothing to render and nothing to
    /// hold — but a raise arriving while it is open must still be swallowed,
    /// and this is what tells [`SignInFlow::begin`] to swallow it.
    sheet_open: RwSignal<bool>,
    /// Why the last attempt did not get anywhere. The card renders this under
    /// its own banner; see [`SignInFlow::error`].
    error: RwSignal<Option<String>>,
}

impl SignInFlow {
    /// A flow with nothing in progress, carrying whatever the OIDC callback
    /// left behind (set during `initialize`, before Leptos mounts).
    pub fn new() -> Self {
        Self {
            attempt: RwSignal::new(None),
            sheet_open: RwSignal::new(false),
            error: RwSignal::new(crate::AUTH_ERROR.read().ok().and_then(|guard| guard.clone())),
        }
    }

    /// Why the last attempt did not get anywhere.
    ///
    /// A HOST MUST PUT THIS SOMEWHERE. Two of the three writers have no modal
    /// on screen to speak for them: [`SignInFlow::hand_over_the_tab`], when
    /// `start_sign_in` cannot stash its one-time material, and
    /// [`SignInFlow::open_the_sheet`], every time the shell's sheet ends in
    /// anything but a session — the sheet is the system's window and it is
    /// already gone by then. That leaves a visitor pressing a control that
    /// does nothing — and for an anonymous reader, sign-in is the only way out
    /// of read-only, so a silent failure there is the whole conversion path
    /// failing quietly. The card renders this in its own banner; a host
    /// without one mounts [`SignInFlow::view_with_notice`] rather than
    /// [`SignInFlow::view`]. The third writer — an exchange that failed inside
    /// a live ceremony — speaks for itself first, in the modal, before landing
    /// here.
    pub fn error(&self) -> RwSignal<Option<String>> { self.error }

    /// Start a sign-in — and do nothing at all while one is already on screen.
    ///
    /// IDEMPOTENT BY CONTRACT. `ankurah-chat-leptos` raises the host's auth
    /// demand once per gesture at the composer *and* on every anonymous write,
    /// so a reader who presses on the message box and then clicks a reaction
    /// raises it twice; a reader who dismisses the ceremony and presses again
    /// raises it again on purpose, which is how they get it back. Answering
    /// the second raise by starting a second attempt would restash fresh PKCE
    /// material and reload the frame under a credential prompt already in
    /// progress — so an open ceremony swallows it, and the card's own button
    /// gets the same answer for the same reason.
    pub fn begin(&self) {
        if self.attempt.get_untracked().is_some() || self.sheet_open.get_untracked() {
            return;
        }
        // A new attempt starts without the last one's failure over it.
        self.error.set(None);
        // In the mobile shell the frame is the wrong instrument: this web view
        // shares no credential store with the browser, so idp.to's page inside
        // it could offer no passkey the member already has. The system's
        // sign-in sheet is the way in there, and it replaces both branches
        // below — neither the frame nor handing over this document.
        if crate::shell::is_shell() {
            self.open_the_sheet();
            return;
        }
        match auth::begin_framed_sign_in() {
            // idp.to frames for this origin: run the ceremony without leaving the page.
            Ok(Some(attempt)) => self.attempt.set(Some(attempt)),
            // It does not, so a frame would be refused and would say nothing
            // about it. Hand over the whole tab, which always works.
            Ok(None) => self.hand_over_the_tab(),
            // Setting up the attempt failed (no sessionStorage, say). The
            // top-level flow needs the same things, so let it fail where the
            // visitor can see it.
            Err(e) => {
                tracing::error!("failed to set up framed sign-in: {:?}", e);
                self.hand_over_the_tab();
            }
        }
    }

    /// The flow that has always worked: idp.to gets the whole tab. Only its
    /// own setup failure returns.
    fn hand_over_the_tab(&self) {
        if let Err(e) = auth::start_sign_in() {
            tracing::error!("failed to start sign-in: {:?}", e);
            self.error.set(Some(format!("could not start sign-in: {e:?}")));
        }
    }

    /// The mobile shell's flow: the system's sign-in sheet, on idp.to's
    /// authorization page, over the app.
    ///
    /// Every ending is spoken for. A sheet that came back with a session boots
    /// the app as that member. A sheet that could not run at all lands on the
    /// card's error text, and so does a dismissal, because the alternative is a
    /// control the member pressed and nothing at all happening afterwards.
    ///
    /// A REFUSAL FROM IDP.TO NEVER REACHES THIS MATCH, and the unregistered
    /// native redirect URI is the case that matters today. OAuth forbids
    /// redirecting to a `redirect_uri` the client did not register, so idp.to
    /// draws its refusal on its own page INSIDE the sheet: nothing is sent to
    /// the app's scheme, the sheet stays open on that page, and the member
    /// closes it — which arrives here as `Cancelled`. So until the URI is
    /// registered, "Sign-in was cancelled." is what a refused attempt says, and
    /// the true reason is on the page the member just read (docs/auth.md, the
    /// native sign-in section).
    fn open_the_sheet(&self) {
        // Raised before the sheet opens and lowered on the two endings that
        // come back to this card. NOT on `Signed`: that path reloads the
        // document, so this signal — and everything else this flow holds — goes
        // away with it, and lowering it first would show the card unblocked for
        // the moment before the reload lands.
        self.sheet_open.set(true);
        let flow = *self;
        spawn_local(async move {
            match auth::start_native_sign_in().await {
                auth::NativeSignIn::Signed(token) => boot_as_the_member(token),
                auth::NativeSignIn::Cancelled => {
                    flow.sheet_open.set(false);
                    flow.error.set(Some("Sign-in was cancelled.".to_string()));
                }
                auth::NativeSignIn::Failed(reason) => {
                    // On screen rather than in the console, for the reason the
                    // ceremony's own failures are: these strings can carry a
                    // response body, and a body can carry a token.
                    tracing::error!("the app's sign-in did not complete; the reason is shown to the member");
                    flow.sheet_open.set(false);
                    flow.error.set(Some(reason));
                }
            }
        });
    }

    /// The ceremony modal, while an attempt is live, AND a notice carrying
    /// whatever [`SignInFlow::error`] holds — for a host with nowhere else to
    /// put one.
    ///
    /// The notice is dismissible because it outlives the gesture that caused
    /// it: nothing else on the page changes when a sign-in fails to start, so
    /// there is no next action that would clear it, and a reader who has read
    /// it needs a way to put it down. Starting another attempt clears it too
    /// (see [`SignInFlow::begin`]).
    pub fn view_with_notice(self) -> impl IntoView {
        view! {
            {self.view()}
            {move || self.error.get().map(|message| view! {
                <div class="signInNotice" role="alert">
                    <span class="signInNoticeText">{message}</span>
                    <button
                        class="signInNoticeClose"
                        aria-label="Dismiss"
                        on:click=move |_| self.error.set(None)
                    >"×"</button>
                </div>
            })}
        }
    }

    /// The ceremony modal, while an attempt is live. Mount it once, wherever
    /// the host wants the overlay to sit. The host owns showing
    /// [`SignInFlow::error`] — see that method before choosing this over
    /// [`SignInFlow::view_with_notice`].
    pub fn view(self) -> impl IntoView {
        // Closing takes the frame down with the modal and abandons the stashed
        // attempt, so the next raise starts clean. A reason handed back is kept
        // for a host that shows one — the modal that was displaying it is about
        // to go, and the visitor may still need to read it.
        let close = move |reason: Option<String>| {
            auth::cancel_pending_sign_in();
            if reason.is_some() {
                self.error.set(reason);
            }
            self.attempt.set(None);
        };
        move || {
            self.attempt.get().map(|attempt| {
                view! { <SignInCeremony attempt=attempt on_close=close on_signed_in=boot_as_the_member /> }
            })
        }
    }
}

/// Take a freshly minted session: store the token and let the app boot the way
/// it boots on every other load with a member session in hand — `initialize`
/// picks it up from `stored_token`, connects the node, waits for policy, and
/// mounts as that member. Deliberately NOT a swap under the mounted tree: there
/// is one path into a signed-in session, and this is it. A reload rather than a
/// navigation, so a reader who was looking at `?room=…` comes back to the same
/// room.
///
/// Shared by the ceremony and the shell's sheet, so signing in mid-visit means
/// the same thing whichever of them the member went through.
fn boot_as_the_member(token: String) {
    auth::store_token(&token);
    if let Some(w) = window() {
        let _ = w.location().reload();
    }
}

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
/// `on_close` abandons the attempt — the × and Escape take it, and after a
/// failure they are what is left to press. Its argument is a message the
/// visitor should still be able to read once the modal is gone: why the
/// attempt failed. `on_signed_in` receives the minted ankurah session token.
#[component]
pub fn SignInCeremony(
    attempt: FramedAttempt,
    on_close: impl Fn(Option<String>) + Clone + 'static,
    on_signed_in: impl Fn(String) + Clone + 'static,
) -> impl IntoView {
    let phase = RwSignal::new(Phase::Waiting);

    // idp.to's reported form height, once a size report has arrived; the
    // stage's stylesheet fallback holds until then — and holds for good in a
    // frame that never loads, since a refused document reports nothing.
    let reported_height = RwSignal::new(None::<f64>);

    // Set once the visitor has asked to stop. The exchange is a spawned future
    // that owns everything it needs before its first await, so unmounting the
    // modal cannot stop it — without this it would run to completion and sign
    // the visitor in after they closed the ceremony.
    let abandoned = Rc::new(Cell::new(false));

    // The attempt's `state`, held for the one message allowed to claim it. The
    // exchange needs it again afterwards, so the copy the listener consumes is
    // its own.
    let attempt_state = attempt.state.clone();
    let expected_state = StoredValue::new(Some(attempt.state.clone()));
    // What this attempt told idp.to its callback would be. The token exchange
    // has to repeat it, and a value recomputed there could differ from the one
    // the authorization request actually carried.
    let attempt_redirect_uri = attempt.redirect_uri.clone();

    let message_closure = wasm_bindgen::closure::Closure::wrap(Box::new({
        let on_signed_in = on_signed_in.clone();
        let abandoned = abandoned.clone();
        move |event: MessageEvent| {
            // The frame's size reports arrive on the same channel as its
            // result. The stage follows them so the card hugs the form
            // instead of holding the guessed fallback — clamped, because the
            // report is a measurement from the framed document, not an
            // instruction, and the ceremony bounds what it is willing to be.
            if let Some(height) = auth::read_embed_size(&event) {
                reported_height.set(Some(height.clamp(240.0, 640.0)));
                return;
            }

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
            let attempt_redirect_uri = attempt_redirect_uri.clone();
            let on_signed_in = on_signed_in.clone();
            let abandoned = abandoned.clone();
            spawn_local(async move {
                let outcome = auth::complete_sign_in(&code, &attempt_state, &attempt_redirect_uri).await;
                if abandoned.get() {
                    // Closed while this was in flight. The mint already happened
                    // server-side and cannot be taken back, but nothing of it is
                    // kept here: no session token is stored and no navigation
                    // follows.
                    //
                    // What this must not do is reach for whatever is in storage
                    // now. An await can be arbitrarily long — a stalled request,
                    // the × the visitor was invited to use, a retry that gets
                    // further — so by this line the pending-attempt keys hold a
                    // SUCCESSOR's material (this attempt's was consumed before
                    // the first request went out) and are not this task's to
                    // clear. The id_token is: this exchange retained it
                    // moments ago, paired with the session it minted, and the
                    // compare reads that pair's `id_token` — so only this
                    // exchange's own write is taken back. See
                    // `remove_id_token_if_matches` for what that compare does
                    // and does not cover.
                    if let Ok(minted) = &outcome {
                        auth::remove_id_token_if_matches(&minted.id_token);
                    }
                    return;
                }
                match outcome {
                    Ok(minted) => on_signed_in(minted.token),
                    Err(e) => {
                        // The reason goes on screen, never through the console:
                        // these strings can carry a raw response body, and a
                        // body can carry a token.
                        tracing::error!("framed sign-in did not complete; the reason is shown to the visitor");
                        phase.set(Phase::Failed(e));
                    }
                }
            });
        }
    }) as Box<dyn FnMut(_)>);

    // Stop, whatever is in flight: mark the attempt abandoned, then hand up
    // whatever the visitor should still be able to read once the modal is gone.
    let close = {
        let abandoned = abandoned.clone();
        move || {
            abandoned.set(true);
            let reason = match phase.get_untracked() {
                Phase::Failed(message) => Some(message),
                _ => None,
            };
            on_close(reason);
        }
    };

    // Escape closes, as it does for every other overlay in the app. It only
    // reaches us while the parent page holds focus — once the frame has it, the
    // key belongs to idp.to's page — so it is a convenience, not the way out.
    let key_closure = wasm_bindgen::closure::Closure::wrap(Box::new({
        let close = close.clone();
        move |e: web_sys::KeyboardEvent| {
            if e.key() == "Escape" {
                close();
            }
        }
    }) as Box<dyn FnMut(_)>);

    if let Some(w) = window() {
        let _ = w.add_event_listener_with_callback("message", message_closure.as_ref().unchecked_ref());
        let _ = w.add_event_listener_with_callback("keydown", key_closure.as_ref().unchecked_ref());
    }
    let on_unmount = SendWrapper::new((message_closure, key_closure, abandoned.clone()));
    on_cleanup(move || {
        let (message_closure, key_closure, abandoned) = on_unmount.take();
        // Whatever took the ceremony off the page, nothing it started may
        // finish. `close` has usually said so already; this covers every other
        // way the modal can be unmounted.
        abandoned.set(true);
        if let Some(w) = window() {
            let _ = w.remove_event_listener_with_callback("message", message_closure.as_ref().unchecked_ref());
            let _ = w.remove_event_listener_with_callback("keydown", key_closure.as_ref().unchecked_ref());
        }
    });

    // Focus starts inside the modal. Otherwise it stays on the button that
    // opened it, one Enter away from restashing fresh material and reloading
    // the frame under a credential prompt already in progress.
    let close_ref = NodeRef::<leptos::html::Button>::new();
    Effect::new(move |_| {
        if let Some(el) = close_ref.get() {
            let _ = el.focus();
        }
    });

    let close_button = close.clone();

    // The reported height sizes the frame, and the frame is on screen for
    // Waiting only; the status phases keep the stylesheet minimums, so a
    // failure sentence wraps and grows instead of clipping inside a short
    // reported box — a compact form's height is no measure of an error's.
    let frame_height = move || match (phase.get(), reported_height.get()) {
        (Phase::Waiting, Some(height)) => Some(height),
        _ => None,
    };

    view! {
        // No dismiss-on-scrim-click, unlike the app's other modals: a stray
        // click while the credential prompt is up would take the ceremony down
        // mid-touch. The × (and Escape while the parent page holds focus) is
        // the way out.
        //
        // The dialog keeps an accessible name although the card shows no
        // title: the visible words live on idp.to's page inside the frame,
        // and a name has to come from this document.
        <div class="ceremonyOverlay" role="dialog" aria-modal="true" aria-label="Sign in with idp.to">
            <div class="ceremonyCard">
                <button
                    class="ceremonyClose"
                    aria-label="Close"
                    node_ref=close_ref
                    on:click=move |_| close_button()
                >"×"</button>

                <div
                    class="ceremonyStage"
                    style:height=move || frame_height().map(|h| format!("{h}px")).unwrap_or_default()
                    style:min-height=move || frame_height().map(|_| "0".to_string()).unwrap_or_default()
                >
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
            </div>
        </div>
    }
}
