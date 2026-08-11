//! Mobile push notifications, the client half: this device tells the server
//! where to reach it, and a tapped alert opens what it announced.
//!
//! FOR: the in-app inbox reaches a member only while they are looking at the
//! app. The server already sends an alert per notification-worthy event
//! (`server/src/workers/push.rs`), but it can only send to a device it has
//! been told about — so the app hands over the device token APNs minted for
//! this install, and the alert that comes back has to land somewhere useful
//! when it is tapped. Both legs live here; the JavaScript they go through
//! (`shell.js`) and the typed native calls above it (`shell.rs`) hold no part
//! of the decision.
//!
//! WHEN THE MEMBER IS ASKED, and why it is not earlier. iOS shows its
//! notification prompt once per install, so where that prompt falls is the
//! whole of the member's experience of it. It falls on a boot that landed a
//! MEMBER session: somebody who has signed in, whose account is what a
//! notification would be addressed to. A guest boot asks nothing and calls
//! nothing — a guest names no `User` row, has nothing addressed to them, and
//! the registry refuses them by design (`server/src/push/registry.rs`) — and
//! an install that has never been signed into never sees the prompt at all.
//!
//! A REFUSAL IS FINAL AND SILENT. Denied stays denied: nothing here asks
//! again, and nothing tells the member what they are missing. iOS would not
//! show the prompt a second time anyway; changing the answer means Settings,
//! and that is the member's own business.
//!
//! EVERY MEMBER BOOT RE-REGISTERS. iOS reissues a device token without warning
//! — a restore, an app reinstall, an OS update — and the app is the only party
//! that ever learns the new one. The registry upserts per (member, device), so
//! a re-registration refreshes one row rather than adding one, which makes
//! "send it every launch" the cheap and correct answer instead of tracking
//! what we last sent.
//!
//! NO BANNER OVER AN OPEN APP. `mobile/capacitor.config.json` sets the push
//! plugin's `presentationOptions` to the empty list, so an alert arriving while
//! the member is looking at the app shows nothing. It is not dropped: the same
//! event wrote the inbox row that the notification panel is already announcing,
//! with a chime, in the place the member is looking.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use ankurah::EntityId;
use serde_json::Value;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{window, Headers, Request, RequestInit, Response};

use crate::shell::{self, PushPermission};

/// The member of the alert document carrying the facts only this app
/// understands, beside Apple's own `aps`. SHARED LITERAL: the server writes it
/// as `push::apns::TARGET_KEY`.
const TARGET_KEY: &str = "community";

/// `Notification.kind` for a direct message. SHARED LITERAL: the server's
/// `workers::dm_notify::DM_KIND`, and the same word the inbox row keys on
/// (`notification_inbox.rs`).
const DM_KIND: &str = "dm";

/// Which push service reaches this device — the one word the registry accepts
/// today (`server/src/push/store.rs`).
const PLATFORM: &str = "ios";

/// The registry's door (`server/src/push/registry.rs`).
const REGISTER_PATH: &str = "/push/register";

/// How much of a device token is safe to write down. The token is the
/// credential for waking this phone, so a log line names a device by its first
/// few characters and never by the whole of it — the same cut the server makes
/// (`push::store::token_prefix`).
const TOKEN_PREFIX_CHARS: usize = 8;

/// Where a tapped alert leads.
///
/// The two places a notification can be about, and the same two the in-app
/// inbox row deep-links to from the identical ids: a room, or the
/// correspondent whose direct message it announced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    Room(EntityId),
    /// The other member in the conversation — a direct message happens in no
    /// room, so the sender IS the place.
    Conversation(EntityId),
}

/// Read a tapped alert for where it leads, or `None` when it names nowhere
/// this client can open.
///
/// The event is the plugin's, one object deep: `notification.data` is the JSON
/// document APNs delivered, and [`TARGET_KEY`] within it is what the server
/// addressed to this app. A `dm` is routed by its `actor` and everything else
/// by its `room`, which is what the inbox row does with the same two ids.
///
/// `None` covers every way a tap can arrive with nothing to act on: an alert
/// this client predates, a member absent because the server had nothing to put
/// there, and anything that is not the document at all. All of them mean the
/// app opens where it last was, which is the honest end of a tap nobody here
/// can follow.
pub fn route_for_tap(event: &Value) -> Option<Route> {
    let target = event.get("notification")?.get("data")?.get(TARGET_KEY)?;
    if target.get("kind").and_then(Value::as_str) == Some(DM_KIND) {
        return named_entity(target, "actor").map(Route::Conversation);
    }
    named_entity(target, "room").map(Route::Room)
}

/// One entity id off the alert. A member the server left out is absent, which
/// is ordinary; a member that is present and unreadable is worth a line,
/// because it means the two ends disagree about what an id looks like.
fn named_entity(target: &Value, name: &str) -> Option<EntityId> {
    let raw = target.get(name)?.as_str()?;
    match EntityId::from_base64(raw) {
        Ok(id) => Some(id),
        Err(e) => {
            tracing::warn!("a tapped alert named a `{name}` that is not an entity id: {e}");
            None
        }
    }
}

/// What this device sends to `POST /push/register`.
pub fn registration_body(device_token: &str) -> String {
    serde_json::json!({ "token": device_token, "platform": PLATFORM }).to_string()
}

/// The part of a device token that may be written down.
fn token_prefix(token: &str) -> String { token.chars().take(TOKEN_PREFIX_CHARS).collect() }

thread_local! {
    /// What the mounted app does with a route. See [`install_navigator`].
    static NAVIGATOR: RefCell<Option<Rc<dyn Fn(Route)>>> = const { RefCell::new(None) };
    /// A route that arrived before there was anything to follow it with. Only
    /// the newest is kept: a member can be in one place, and it is wherever
    /// they tapped last.
    static PENDING: Cell<Option<Route>> = const { Cell::new(None) };
}

/// Hand the mounted app the job of following a route, and give it whatever a
/// tap has already asked for.
///
/// FOR: a tap is what launches the app as often as it is what interrupts it,
/// and the two arrive at opposite ends of the boot. The launch tap is held by
/// the native side until this client listens (`shell.js`), which can be before
/// the UI exists; a tap on a running app arrives long after. One slot answers
/// both — the app claims it at mount and drains anything waiting, and every
/// later tap goes straight through.
///
/// Installed at mount and BEFORE the first effect runs, which is what settles
/// the one ordering that matters: the sidebar's default room choice
/// (`sidebar::auto_select_room`) only picks when nothing is selected, so a
/// route drained here is the selection it defers to rather than overrides.
pub fn install_navigator(navigate: impl Fn(Route) + 'static) {
    NAVIGATOR.with(|slot| *slot.borrow_mut() = Some(Rc::new(navigate)));
    if let Some(route) = PENDING.with(Cell::take) {
        follow(route);
    }
}

/// Take a route to the app, or hold it until there is an app to take it to.
fn follow(route: Route) {
    // Cloned out of the slot before the call, so nothing the app does while
    // following a route can find this borrow still held.
    let navigator = NAVIGATOR.with(|slot| slot.borrow().clone());
    match navigator {
        Some(navigate) => navigate(route),
        None => PENDING.with(|slot| slot.set(Some(route))),
    }
}

/// Listen for tapped alerts, for the life of this document.
///
/// Called for every shell boot, a member's and a guest's alike. Nothing is
/// asked of the member and nothing is sent anywhere — this only decides where
/// an alert that already exists lands, and an alert only exists because
/// somebody signed in on this device and registered it.
pub fn watch_taps() {
    shell::on_push_opened(|event| match route_for_tap(&event) {
        Some(route) => follow(route),
        None => tracing::info!("push: a tapped alert named nowhere this app can open; leaving it where it landed"),
    });
}

/// Tell the server where to reach this phone: ask about notifications, ask the
/// member if nobody has yet, and file the device token that follows.
///
/// Every ending short of a filed token is one line in the log and no further
/// action. None of them is worth a word to the member: a phone that will not
/// ring is a phone that does not ring, and the app is otherwise whole.
pub async fn register_this_device() {
    let permission = match shell::push_permission().await {
        // Nobody has been asked. This is the one place the prompt appears, and
        // by now the member has signed in — see the module header.
        Some(PushPermission::Undetermined) => shell::request_push_permission().await,
        standing => standing,
    };
    if permission != Some(PushPermission::Granted) {
        tracing::info!("push: notifications are not permitted here, so this device is not registered");
        return;
    }

    let device_token = match shell::register_for_push().await {
        Ok(token) => token,
        Err(reason) => {
            tracing::warn!("push: APNs issued no device token for this install: {reason}");
            return;
        }
    };

    let Some(base) = crate::auth::server_http_base() else {
        tracing::warn!("push: no server address to register this device with");
        return;
    };
    // The same session token every other request to our own server presents.
    // Its subject is what the registry files the device under, so a device is
    // only ever filed under the member who is signed in right now.
    let Some(session) = crate::AUTH_TOKEN.read().ok().and_then(|guard| guard.clone()) else {
        tracing::warn!("push: no session to register this device with");
        return;
    };

    let device = token_prefix(&device_token);
    match post_registration(&format!("{base}{REGISTER_PATH}"), &session, &registration_body(&device_token)).await {
        Ok(204) => tracing::info!("push: registered this device ({device}…) for notifications"),
        // The registry answers every refusal with a status and a sentence, and
        // the sentence is for a developer rather than for the log — so the
        // status is what is written down. 401 is a session it would not verify,
        // 403 a guest (which this path does not reach), 400 a body it did not
        // recognize as a registration.
        Ok(status) => tracing::warn!("push: the server refused this device ({device}…) with HTTP {status}"),
        Err(reason) => tracing::warn!("push: could not reach the server to register this device: {reason}"),
    }
}

/// POST the registration, answering with the status the server gave.
///
/// `Err` is reserved for never having been answered at all — offline, name
/// resolution, TLS, a blocked request. The response body is deliberately never
/// read: the caller logs, and a body in a log is what `auth::GuestMintFailure`
/// exists to keep out of one.
async fn post_registration(url: &str, session_token: &str, body: &str) -> Result<u16, String> {
    let describe = |e: JsValue| e.as_string().unwrap_or_else(|| format!("{e:?}"));
    let window = window().ok_or("no window")?;

    let opts = RequestInit::new();
    opts.set_method("POST");
    opts.set_body(&JsValue::from_str(body));
    let headers = Headers::new().map_err(describe)?;
    headers.set("Content-Type", "application/json").map_err(describe)?;
    headers.set("Authorization", &format!("Bearer {session_token}")).map_err(describe)?;
    opts.set_headers(headers.as_ref());

    let request = Request::new_with_str_and_init(url, &opts).map_err(describe)?;
    let answer = JsFuture::from(window.fetch_with_request(&request)).await.map_err(describe)?;
    let response: Response = answer.dyn_into().map_err(|_| "fetch did not return a Response".to_string())?;
    Ok(response.status())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wasm_bindgen_test::wasm_bindgen_test;

    const GENERAL: &str = "RZk3jW0RvkW8pTGnQxYzRQ";
    const BOB: &str = "BZk3jW0RvkW8pTGnQxYzRQ";
    const NOTIFICATION: &str = "NZk3jW0RvkW8pTGnQxYzRQ";
    const MESSAGE: &str = "MZk3jW0RvkW8pTGnQxYzRQ";

    /// One tapped alert, as the plugin hands it over: the whole APNs document
    /// under `notification.data`, with the app's own facts beside Apple's
    /// `aps`.
    fn tap(target: Value) -> Value {
        json!({
            "actionId": "tap",
            "notification": {
                "id": "1",
                "title": "#general",
                "body": "Bob mentioned you",
                "data": { "aps": { "alert": { "title": "#general", "body": "Bob mentioned you" } }, TARGET_KEY: target },
            },
        })
    }

    #[wasm_bindgen_test]
    fn a_mention_leads_to_the_room_it_names() {
        let event = tap(json!({ "kind": "mention", "notification": NOTIFICATION, "room": GENERAL, "message": MESSAGE, "actor": BOB }));
        assert_eq!(route_for_tap(&event), Some(Route::Room(EntityId::from_base64(GENERAL).unwrap())));

        // A kind this client predates still leads somewhere: what makes a room
        // route is the room, not the word for what happened in it.
        let later = tap(json!({ "kind": "reaction", "notification": NOTIFICATION, "room": GENERAL }));
        assert_eq!(route_for_tap(&later), Some(Route::Room(EntityId::from_base64(GENERAL).unwrap())));
    }

    #[wasm_bindgen_test]
    fn a_direct_message_leads_to_whoever_sent_it() {
        // A DM happens in no room, so the server sends neither `room` nor
        // `message` — the sender in `actor` is the whole of the target.
        let event = tap(json!({ "kind": DM_KIND, "notification": NOTIFICATION, "actor": BOB }));
        assert_eq!(route_for_tap(&event), Some(Route::Conversation(EntityId::from_base64(BOB).unwrap())));

        // And the kind decides, not the shape: a `dm` that somehow carried a
        // room still opens the conversation, which is what the inbox row does
        // with the same row.
        let odd = tap(json!({ "kind": DM_KIND, "notification": NOTIFICATION, "room": GENERAL, "actor": BOB }));
        assert_eq!(route_for_tap(&odd), Some(Route::Conversation(EntityId::from_base64(BOB).unwrap())));
    }

    #[wasm_bindgen_test]
    fn an_alert_naming_nowhere_leads_nowhere() {
        for event in [
            // A kind that named neither a room nor anyone.
            tap(json!({ "kind": "reaction", "notification": NOTIFICATION })),
            // A DM with no sender on it.
            tap(json!({ "kind": DM_KIND, "notification": NOTIFICATION })),
            // Ids that are not ids, of either sort.
            tap(json!({ "kind": "mention", "room": "not an entity id" })),
            tap(json!({ "kind": DM_KIND, "actor": "" })),
            // Right names, wrong types.
            tap(json!({ "kind": "mention", "room": 7 })),
            // The app's own member missing, empty, or not an object.
            tap(json!({})),
            json!({ "actionId": "tap", "notification": { "data": {} } }),
            json!({ "actionId": "tap", "notification": {} }),
            json!({ "actionId": "tap" }),
            json!({}),
            json!("not an event at all"),
        ] {
            assert_eq!(route_for_tap(&event), None, "led somewhere: {event}");
        }

        // `kind` is read but never required — it is consulted for one word and
        // one word only. An alert with no kind at all, or one whose kind is not
        // even a word, is still a room when it names a room.
        let general = Some(Route::Room(EntityId::from_base64(GENERAL).unwrap()));
        assert_eq!(route_for_tap(&tap(json!({ "room": GENERAL }))), general);
        assert_eq!(route_for_tap(&tap(json!({ "kind": 7, "room": GENERAL, "actor": BOB }))), general);
    }

    #[wasm_bindgen_test]
    fn a_registration_says_the_token_and_the_service_and_nothing_else() {
        // The token goes over verbatim: iOS writes it in upper-case hex, and
        // the registry checks the alphabet case-insensitively — so re-casing it
        // here would only risk filing one phone under two rows.
        let token = "0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF";
        let body: Value = serde_json::from_str(&registration_body(token)).unwrap();
        assert_eq!(body, json!({ "token": token, "platform": "ios" }));
    }

    #[wasm_bindgen_test]
    fn a_device_is_named_in_a_log_by_its_first_characters_only() {
        let token = "0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF";
        let named = token_prefix(token);
        assert_eq!(named, "01234567");
        assert!(token.starts_with(&named) && named.len() < token.len(), "a log line must never carry the whole token");
    }
}
