//! Mobile push notifications, the client half: this device tells the server
//! where to reach it, and a tapped alert opens what it announced.
//!
//! FOR: the in-app inbox reaches a member only while they are looking at the
//! app. The server already sends an alert per notification-worthy event
//! (`server/src/workers/push.rs`), but it can only send to a device it has
//! been told about — so the app writes the device token APNs minted for this
//! install as its own self-scoped `PushDevice` entity, and the alert that comes
//! back has to land somewhere useful when it is tapped. Both legs live here;
//! the JavaScript they go through (`shell.js`) and the typed native calls above
//! it (`shell.rs`) hold no part of the decision.
//!
//! WHEN THE MEMBER IS ASKED, and why it is not earlier. iOS shows its
//! notification prompt once per install, so where that prompt falls is the
//! whole of the member's experience of it. It falls on a boot that landed a
//! MEMBER session: somebody who has signed in, whose account is what a
//! notification would be addressed to. A guest boot asks nothing and calls
//! nothing — a guest names no `User` row, has nothing addressed to them, and
//! cannot write the `pushdevice` collection under `policy.json` — and an
//! install that has never been signed into never sees the prompt at all.
//!
//! A REFUSAL IS FINAL AND SILENT. Denied stays denied: nothing here asks
//! again, and nothing tells the member what they are missing. iOS would not
//! show the prompt a second time anyway; changing the answer means Settings,
//! and that is the member's own business.
//!
//! EVERY MEMBER BOOT RE-REGISTERS. iOS reissues a device token without warning
//! — a restore, an app reinstall, an OS update — and the app is the only party
//! that ever learns the new one. The client upserts its own (member, device)
//! entity through Ankurah, so a re-registration refreshes one row rather than
//! adding one, which makes "send it every launch" the cheap and correct answer
//! instead of tracking what we last sent.
//!
//! NO BANNER OVER AN OPEN APP. `mobile/capacitor.config.json` sets the push
//! plugin's `presentationOptions` to the empty list, so an alert arriving while
//! the member is looking at the app shows nothing. It is not dropped: the same
//! event wrote the inbox row that the notification panel is already announcing,
//! with a chime, in the place the member is looking.
//!
//! AND SIGNING OUT TAKES THE DEVICE OFF. A phone somebody signed out of must
//! stop ringing for the account that left it — otherwise every mention goes on
//! arriving on a device nobody is signed into, and the member has no way to say
//! so. [`withdraw_this_device`] is what `auth::sign_out` calls, and the whole
//! reason [`LS_DEVICE_TOKEN`] exists: the token came from iOS, was sent once,
//! and by sign-out time nothing else remembers what it was.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use ankurah::ankql::{ast::Expr, parser::parse_selection};
use ankurah::EntityId;
use community_model::{PushDevice, PushDeviceView, MAX_PUSH_DEVICES_PER_USER};
use serde_json::Value;
use web_sys::window;

use crate::shell::{self, PushPermission};

/// The member of the alert document carrying the facts only this app
/// understands, beside Apple's own `aps`. SHARED LITERAL: the server writes it
/// as `push::apns::TARGET_KEY`.
const TARGET_KEY: &str = "community";

/// `Notification.kind` for a direct message. SHARED LITERAL: the server's
/// `workers::dm_notify::DM_KIND`, and the same word the inbox row keys on
/// (`notification_inbox.rs`).
const DM_KIND: &str = "dm";

/// Which push service reaches this device — shared with the server sender's
/// parser (`server/src/push/store.rs`).
const PLATFORM: &str = "ios";

/// localStorage key for the device token this install last filed with the
/// server.
///
/// FOR: sign-out has to name the row it is withdrawing, and by then the only
/// party that ever knew the token is this slot. Asking iOS again at sign-out
/// would mean re-registering for push to un-register from it, which is both
/// absurd and asynchronous inside a gesture that is about to reload the page.
///
/// Written only by a shell boot that registered, so a browser tab never has
/// one — which is also what makes the slot the shell test: there is no device
/// on the web to withdraw.
///
/// NOT A CREDENTIAL FOR THIS ORIGIN, unlike the two slots `auth` keeps. A
/// device token addresses a phone through Apple; holding one lets nobody send
/// to it without this deployment's APNs key. It is still never logged whole —
/// see [`token_prefix`].
const LS_DEVICE_TOKEN: &str = "community_push_device_token";

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

/// The part of a device token that may be written down.
fn token_prefix(token: &str) -> String { token.chars().take(TOKEN_PREFIX_CHARS).collect() }

/// What the mounted app does with a route. See [`install_navigator`].
type Navigate = Rc<dyn Fn(Route)>;

thread_local! {
    static NAVIGATOR: RefCell<Option<Navigate>> = const { RefCell::new(None) };
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

    let Some(member) = crate::viewer() else {
        tracing::warn!("push: no member session to own this device registration");
        return;
    };

    let device = token_prefix(&device_token);
    match upsert_device(member, &device_token).await {
        Ok(()) => {
            // Remembered only once the entity commit succeeds, so the slot
            // never names a row this node did not file.
            if let Some(ls) = local_storage() {
                let _ = ls.set_item(LS_DEVICE_TOKEN, &device_token);
            }
            tracing::info!("push: registered this device ({device}…) for notifications");
        }
        Err(reason) => tracing::warn!("push: could not commit this device ({device}…) through Ankurah: {reason}"),
    }
}

/// Deactivate this phone's self-owned registration when signing out.
///
/// The slot is cleared FIRST, and not on success: this device is signing out
/// whatever the server says, and a stale token left behind would be sent again
/// by the next sign-out under whichever account signs in next — a request
/// naming a row that member never filed.
pub async fn withdraw_this_device() {
    // The presence of the slot is the shell test: only a shell boot registers,
    // so only a shell boot ever wrote one.
    let Some(ls) = local_storage() else { return };
    let Some(device_token) = ls.get_item(LS_DEVICE_TOKEN).ok().flatten().filter(|token| !token.is_empty()) else { return };
    let _ = ls.remove_item(LS_DEVICE_TOKEN);

    let Some(member) = crate::viewer() else {
        tracing::warn!("push: no member session to deactivate this device with; the registration is left for APNs feedback");
        return;
    };

    let device = token_prefix(&device_token);
    match deactivate_device(member, &device_token).await {
        Ok(()) => tracing::info!("push: withdrew this device ({device}…) from notifications"),
        Err(reason) => tracing::warn!("push: could not deactivate this device ({device}…) through Ankurah: {reason}"),
    }
}

/// This document's localStorage, when there is one.
fn local_storage() -> Option<web_sys::Storage> { window()?.local_storage().ok().flatten() }

type PushResult<T> = Result<T, Box<dyn std::error::Error>>;

fn device_predicate(member: EntityId) -> PushResult<ankurah::ankql::ast::Predicate> {
    Ok(parse_selection("user = ?")?.predicate.populate([Expr::from(&member)])?)
}

async fn device_rows(member: EntityId) -> PushResult<Vec<PushDeviceView>> {
    Ok(crate::ctx().fetch::<PushDeviceView>(device_predicate(member)?).await?)
}

/// Upsert this installation and deactivate honest-client overflow. The server
/// independently sends only to the newest ten rows, so a custom client cannot
/// turn one notification into unbounded APNs traffic by omitting this cleanup.
async fn upsert_device(member: EntityId, device_token: &str) -> PushResult<()> {
    let rows = device_rows(member).await?;
    let mut matching: Vec<PushDeviceView> = rows
        .iter()
        .filter(|row| row.token().map(|stored| stored == device_token).unwrap_or(false))
        .cloned()
        .collect();
    matching.sort_by_key(|row| row.id().to_base64());

    let now = js_sys::Date::now() as i64;
    let trx = crate::ctx().begin();
    if let Some(keeper) = matching.first() {
        let editable = keeper.edit(&trx)?;
        editable.platform().set(&PLATFORM.to_string())?;
        editable.last_registered_at().set(&now)?;
        editable.active().set(&true)?;
        for duplicate in matching.iter().skip(1) {
            duplicate.edit(&trx)?.active().set(&false)?;
        }
    } else {
        trx.create(&PushDevice {
            user: member.into(),
            token: device_token.to_string(),
            platform: PLATFORM.to_string(),
            last_registered_at: now,
            active: true,
        })
        .await?;
    }
    trx.commit().await?;

    let mut active = Vec::new();
    for row in device_rows(member).await? {
        if row.active()? {
            active.push((row.last_registered_at()?, row.token()?, row.id().to_base64(), row));
        }
    }
    active.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)).then_with(|| b.2.cmp(&a.2)));
    if active.len() > MAX_PUSH_DEVICES_PER_USER {
        let trx = crate::ctx().begin();
        for (_, _, _, row) in active.into_iter().skip(MAX_PUSH_DEVICES_PER_USER) {
            row.edit(&trx)?.active().set(&false)?;
        }
        trx.commit().await?;
    }
    Ok(())
}

async fn deactivate_device(member: EntityId, device_token: &str) -> PushResult<()> {
    let matching: Vec<PushDeviceView> = device_rows(member)
        .await?
        .into_iter()
        .filter(|row| row.token().map(|stored| stored == device_token).unwrap_or(false))
        .collect();
    if matching.is_empty() {
        return Ok(());
    }
    let trx = crate::ctx().begin();
    for row in matching {
        row.edit(&trx)?.active().set(&false)?;
    }
    trx.commit().await?;
    Ok(())
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
    fn a_device_is_named_in_a_log_by_its_first_characters_only() {
        let token = "0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF";
        let named = token_prefix(token);
        assert_eq!(named, "01234567");
        assert!(token.starts_with(&named) && named.len() < token.len(), "a log line must never carry the whole token");
    }
}
