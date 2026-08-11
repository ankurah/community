//! Push sender: an inbox row becomes one visible alert on the member's phones.
//!
//! FOR: the mention and DM fan-outs write a `Notification` row and stop there,
//! which reaches a member only while they have the app open. This worker is the
//! rest of the trip — it watches those rows and, for each newly minted one,
//! sends one alert to every device the recipient has registered
//! (`push::registry`), through Apple (`push::apns`).
//!
//! Consumes `NotificationView`s from the standing notification LiveQuery (see
//! `workers::watch_notifications`), on the same shape as the two message
//! workers: one query, one channel, one supervised consumer.
//!
//! WHAT IT SAYS is what the in-app inbox says. The alert's title is the place
//! and its body is the sentence the inbox row renders — same actor name, same
//! verb per kind, same fallbacks when a name cannot be read
//! (`leptos-app/src/notification_inbox.rs`). A member who reads the banner and
//! then opens the app must not find a differently-worded row waiting.
//!
//! WHAT IT DOES NOT ASK. Whether the member is at their laptop, whether they
//! have already seen the event, whether a similar alert went out a moment ago:
//! none of it. One notification-worthy event, one alert, per the ruling this
//! generation was scoped by (see `crate::push`). The only coalescing here is
//! Apple's own, via [`collapse_id`], and that is a burst of banners collapsing
//! on a lock screen — not a decision about whether to send.
//!
//! WHAT IT DOES NOT DECIDE EITHER. A recipient's notification preferences are
//! settled upstream: `mentions::pref_allows_delivery` runs before the row is
//! written, so a muted room or a `mentions_only` member has no row here to send
//! for. Re-checking would be a second copy of a policy that already has one.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use ankurah::{Context, EntityId};
use anyhow::{Context as _, Result};
use community_model::{NotificationView, RoomView, UserView};
use tokio::sync::mpsc::UnboundedReceiver;
use tracing::{debug, error, info, warn};

use crate::push::apns::{self, Alert, ApnsClient, FromEnv, Outcome, Target, Transport};
use crate::push::store::{token_prefix, DeviceTokens};

use super::dm_notify::DM_KIND;
use super::mentions::MENTION_KIND;

/// Everything the sender needs to reach a phone: who has which devices, and the
/// way to Apple.
///
/// One struct rather than two arguments because the two travel together
/// everywhere and because its absence is meaningful — `None` from
/// [`delivery_from_env`] is precisely "this deployment sends no push", and the
/// worker is never started.
#[derive(Clone)]
pub struct Delivery {
    tokens: Arc<dyn DeviceTokens>,
    apns: Arc<dyn Transport>,
}

impl Delivery {
    pub fn new(tokens: Arc<dyn DeviceTokens>, apns: Arc<dyn Transport>) -> Self { Self { tokens, apns } }
}

/// Read the APNs configuration and build the delivery seam, saying out loud
/// which of the three cases this boot is in.
///
/// FOR: push is optional infrastructure. A deployment with no Apple credentials
/// must start, serve chat, and keep accepting device registrations exactly as
/// before — so "not configured" is answered with one line and a `None`, not an
/// error, and an operator reading the log can tell that state apart from a
/// half-finished setup.
pub fn delivery_from_env(tokens: Arc<dyn DeviceTokens>) -> Option<Delivery> {
    match apns::from_env() {
        FromEnv::Ready(config) => {
            let (topic, endpoint) = (config.topic.clone(), config.endpoint.clone());
            match ApnsClient::new(config) {
                Ok(client) => {
                    info!(topic = %topic, endpoint = %endpoint, "push sender: APNs configured");
                    Some(Delivery::new(tokens, Arc::new(client)))
                }
                // A key that will not parse is a configured deployment that
                // cannot send. Loud, and still not fatal: chat runs, and the
                // registry keeps the tokens for whenever the key is fixed.
                Err(e) => {
                    error!("push sender: the APNs credentials are set but unusable, so no alerts are sent: {e:#}");
                    None
                }
            }
        }
        FromEnv::Absent => {
            info!(
                "push sender: {}, {}, {} and {} are unset, so the sender is dormant and no alerts are sent; POST /push/register keeps accepting device tokens",
                apns::KEY_P8_VAR, apns::KEY_ID_VAR, apns::TEAM_ID_VAR, apns::TOPIC_VAR
            );
            None
        }
        FromEnv::Incomplete { missing } => {
            warn!(
                "push sender: APNs is half-configured and dormant — {} {} unset; set all of {} or none of them",
                missing.join(", "),
                if missing.len() == 1 { "is" } else { "are" },
                apns::REQUIRED_VARS.join(", ")
            );
            None
        }
    }
}

/// What the in-app inbox calls an actor whose name it cannot read, and a room
/// whose name it cannot read. Taken from the client rather than invented, so
/// the banner and the row that follows it say the same thing.
const UNKNOWN_ACTOR: &str = "Someone";
const UNKNOWN_ROOM: &str = "a room";

/// An entity and what to call it in a sentence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Named {
    /// Entity id, base64 — what the deep link travels as.
    pub id: String,
    pub name: String,
}

/// The facts one inbox row states, read off it once.
///
/// FOR: reading a row is storage work and shaping a sentence is not, and
/// keeping them apart is what lets every question about what an alert SAYS be
/// answered without a node under the test. [`read_announcement`] does the
/// reading; everything below it is a pure function of this struct.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Announcement {
    /// The recipient's `User` entity id, base64 — the key the device-token
    /// registry files under.
    pub recipient: String,
    /// The `Notification` entity id, base64.
    pub notification: String,
    /// `Notification.kind`, verbatim.
    pub kind: String,
    /// Who caused it, if the row names anyone.
    pub actor: Option<Named>,
    /// Where it happened, if it happened in a room. A DM happens in none.
    pub room: Option<Named>,
    /// The room message that caused it, if the row names one.
    pub message: Option<String>,
}

/// The sentence fragment for one notification kind.
///
/// The words are the in-app inbox's own (`notification_inbox.rs::kind_verb`),
/// including its fallback for a kind the reader predates — which here means a
/// server that has learned a kind before the phone's app has.
fn kind_verb(kind: &str) -> String {
    match kind {
        MENTION_KIND => "mentioned you".to_string(),
        DM_KIND => "sent you a direct message".to_string(),
        other => format!("sent you a \u{201c}{other}\u{201d} notification"),
    }
}

/// Apple refuses a collapse id longer than this, with the whole alert.
const MAX_COLLAPSE_ID_BYTES: usize = 64;

/// The `apns-collapse-id` for this alert: one conversation, one banner.
///
/// FOR: a phone face-down through a busy hour should not come back to forty
/// banners from one room. APNs replaces an alert already on the lock screen
/// when a newer one carries the same collapse id, so keying on the conversation
/// leaves the newest banner per conversation and drops the rest.
///
/// THIS IS APNs'S OWN COALESCING OF A BURST AND NOT THE DEDUPLICATION PROTOCOL
/// THIS GENERATION DEFERRED (see `crate::push`). Nothing here asks whether the
/// member has already seen the event or is reading along somewhere else; every
/// notification-worthy event still produces a send, and Apple decides only what
/// a screen shows. A half-version of the deferred protocol must not grow out of
/// this line.
///
/// Keyed on (recipient, conversation). The recipient is in the key because one
/// phone can hold two members' registrations — the registry files per (member,
/// device) — and one member's alerts must not replace the other's.
fn collapse_id(announcement: &Announcement) -> String {
    let conversation = match (&announcement.room, &announcement.actor) {
        // A room is the conversation.
        (Some(room), _) => room.id.clone(),
        // A DM's conversation is the person; `actor` is the sender, which is
        // the same thing the client deep-links on for this kind.
        (None, Some(actor)) => format!("dm.{}", actor.id),
        // Neither: a kind that names nobody and nowhere. Collapsing per kind is
        // the most this can honestly say.
        (None, None) => announcement.kind.clone(),
    };
    let mut id = format!("{}.{}", announcement.recipient, conversation);
    // Two entity ids and a separator fit comfortably; `kind` is a server-written
    // discriminator and not bounded here, so the cut is real rather than
    // theoretical — and it lands on a character boundary, because truncating a
    // String anywhere else panics.
    if id.len() > MAX_COLLAPSE_ID_BYTES {
        let mut cut = MAX_COLLAPSE_ID_BYTES;
        while cut > 0 && !id.is_char_boundary(cut) {
            cut -= 1;
        }
        id.truncate(cut);
    }
    id
}

/// Compose the alert for one inbox row.
///
/// The title is the place and the body is the sentence, which is how the inbox
/// row reads with its parts rearranged for a banner: the row says "Alice
/// mentioned you in #general", and the alert says "#general" / "Alice mentioned
/// you". When there is no room — a DM — the place IS the person, so the actor's
/// name rises to the title and the body is the verb alone, rather than naming
/// them twice.
pub fn alert_for(announcement: &Announcement) -> Alert {
    let actor_name = announcement.actor.as_ref().map(|actor| actor.name.as_str()).unwrap_or(UNKNOWN_ACTOR);
    let verb = kind_verb(&announcement.kind);
    let (title, body) = match &announcement.room {
        Some(room) => (format!("#{}", room.name), format!("{actor_name} {verb}")),
        None => (actor_name.to_string(), verb),
    };
    Alert {
        title,
        body,
        collapse_id: collapse_id(announcement),
        target: Target {
            kind: announcement.kind.clone(),
            notification: announcement.notification.clone(),
            room: announcement.room.as_ref().map(|room| room.id.clone()),
            message: announcement.message.clone(),
            actor: announcement.actor.as_ref().map(|actor| actor.id.clone()),
        },
    }
}

/// Send one alert to every device the recipient has registered, and drop the
/// registrations Apple says are dead.
///
/// FOR: a member with a phone and a tablet expects both to ring, and a member
/// who deleted the app should stop costing a request per notification forever.
/// Both are answered here, per device, so one dead token neither silences the
/// others nor survives the send that proved it dead.
///
/// Takes an [`Announcement`] rather than a row, which is what keeps this
/// function free of the node: everything below needs only the token store and
/// the way to Apple.
///
/// `Err` is reserved for not being able to read the registry at all. A refusal
/// from Apple concerns one device and is logged there.
pub async fn deliver(delivery: &Delivery, announcement: &Announcement) -> Result<()> {
    let devices = delivery.tokens.for_user(&announcement.recipient).await.context("read the recipient's device tokens")?;
    if devices.is_empty() {
        debug!(recipient = %announcement.recipient, "push: this member has registered no device");
        return Ok(());
    }

    let alert = alert_for(announcement);
    for device in devices {
        // Every line below names the device by its leading characters and never
        // by the token: the token is the credential for waking that phone (see
        // `push::store::token_prefix`).
        let device_name = token_prefix(&device.token);
        match delivery.apns.send(&device.token, &alert).await {
            Ok(Outcome::Delivered) => {
                info!(recipient = %announcement.recipient, device = %device_name, kind = %announcement.kind, "push alert sent");
            }
            Ok(Outcome::TokenGone { reason, invalidated_at }) => {
                // Apple's report is the one word about a device that arrives
                // without the device's cooperation — the backstop behind the
                // withdrawal a sign-out sends (see `push::registry`).
                //
                // BUT IT DESCRIBES A MOMENT, NOT THE PRESENT. Apple says when
                // the token stopped being valid, and a phone that deleted the
                // app and reinstalled it has re-registered SINCE: dropping that
                // row would undo the registration the reinstall just made, and
                // the member would stop being reachable with the app installed
                // and nothing to tell them. So a row claimed after the moment
                // Apple names is kept. A missing or unreadable timestamp leaves
                // the older behaviour — drop it — because then there is nothing
                // to compare and Apple's report is all there is.
                let re_registered_since = invalidated_at.is_some_and(|at| device.last_registered_at >= at);
                if re_registered_since {
                    info!(
                        recipient = %announcement.recipient,
                        device = %device_name,
                        reason,
                        "push: APNs reported this device gone before it registered again; keeping the newer registration"
                    );
                } else {
                    info!(recipient = %announcement.recipient, device = %device_name, reason, "push: APNs says this device is gone; dropping its registration");
                    if let Err(e) = delivery.tokens.forget(&announcement.recipient, &device.token).await {
                        warn!(recipient = %announcement.recipient, device = %device_name, "push: could not drop a device APNs rejected: {e:#}");
                    }
                }
            }
            // A refusal that names the CONFIGURATION rather than the device.
            // Both of these are true of every device at once when they are true
            // at all — the topic is one variable and so is the endpoint — so
            // neither prunes, and an operator gets a line that names what to go
            // and look at rather than a warning about one phone.
            Ok(Outcome::Refused { status, reason }) if reason == apns::BAD_DEVICE_TOKEN || reason == apns::DEVICE_TOKEN_NOT_FOR_TOPIC => {
                error!(
                    recipient = %announcement.recipient,
                    device = %device_name,
                    status,
                    reason = %reason,
                    "push: APNs will not accept this device token for this deployment's configuration. \
                     Nothing is pruned: this answer is what a wrong {topic} or {endpoint} produces for EVERY device, \
                     and pruning on it would empty the registry and cost every member a reinstall.",
                    topic = apns::TOPIC_VAR,
                    endpoint = apns::ENDPOINT_VAR
                );
            }
            Ok(Outcome::Refused { status, reason }) => {
                warn!(recipient = %announcement.recipient, device = %device_name, status, reason = %reason, "push: APNs refused this alert");
            }
            Err(e) => {
                warn!(recipient = %announcement.recipient, device = %device_name, "push: could not reach APNs for this alert: {e:#}");
            }
        }
    }
    Ok(())
}

/// Ids this process has already announced, bounded the way the other consumers'
/// caches are.
pub type Announced = Mutex<HashSet<EntityId>>;

/// Bound on [`Announced`], matching `workers::remember`.
const MAX_ANNOUNCED: usize = 8192;

/// Whether this row is this process's to announce — the restart probe.
///
/// FOR: an alert cannot be un-sent. The other consumers answer a restart by
/// asking storage whether the row they would write already exists, which is
/// what makes their boot sweep free. There is no such row to ask about here —
/// the phone is the only record that an alert went out — so the sweep this
/// worker also receives (`workers::watch_notifications`) is filtered by two
/// questions instead.
///
/// IS IT OLDER THAN THIS PROCESS? `floor_ms` is the instant `workers::start`
/// was called, and `Notification.created_at` is stamped from the same server
/// clock by the fan-out that wrote the row (`mentions::deliver`,
/// `dm_notify::process_dm`), so a row dated before the floor existed before
/// this boot and has had whatever alert it was ever going to get. This is the
/// whole of what keeps a restart quiet: the sweep may hand over the entire
/// history of the inbox, and all of it stops here.
///
/// HAVE WE ALREADY ANNOUNCED IT? The id set answers a row observed twice within
/// one process, which the boot sweep makes ordinary rather than exotic: a row
/// minted in the gap between this process starting and the standing query
/// activating arrives once as an Add and once in the sweep, and rings once. It
/// is held by the supervisor rather than by the consumer loop, on
/// `dm_rate_limit`'s precedent: a panic must not hand the respawned loop an
/// empty set and a second banner for everything in flight.
///
/// WHAT THIS DELIBERATELY GIVES UP: a notification minted while the server was
/// down never rings. It is in the member's inbox and the unread dot counts it;
/// it simply arrives silently. The alternative is a persisted sent-marker per
/// notification — a second server-side table on both storage engines — and the
/// ruling this generation was scoped by is one alert per event, not a delivery
/// guarantee across downtime.
fn claim(announced: &Announced, floor_ms: i64, id: EntityId, created_at: i64) -> bool {
    if created_at < floor_ms {
        return false;
    }
    let mut announced = announced.lock().unwrap_or_else(|e| e.into_inner());
    // Eviction is a wholesale clear, as everywhere else here. Unlike the other
    // caches this one is not purely an optimization — after a clear, a row
    // re-delivered as a second Add would ring twice — so the bound is set where
    // reaching it takes more notifications in one process lifetime than a
    // deployment this size mints, and the floor still covers every row from
    // before the boot.
    if announced.len() >= MAX_ANNOUNCED {
        announced.clear();
    }
    announced.insert(id)
}

/// Read one inbox row into the plain facts an alert is built from, resolving
/// the actor's and the room's names the way the inbox resolves them.
async fn read_announcement(ctx: &Context, view: &NotificationView) -> Result<Announcement> {
    let recipient = view.recipient().context("read notification recipient")?.id();
    let kind = view.kind().context("read notification kind")?;
    let actor = match view.actor().context("read notification actor")? {
        Some(actor) => Some(Named { id: actor.id().to_base64(), name: display_name(ctx, actor.id()).await }),
        None => None,
    };
    let room = match view.room().context("read notification room")? {
        Some(room) => Some(Named { id: room.id().to_base64(), name: room_name(ctx, room.id()).await }),
        None => None,
    };
    Ok(Announcement {
        recipient: recipient.to_base64(),
        notification: view.id().to_base64(),
        kind,
        actor,
        room,
        message: view.message().context("read notification message")?.map(|message| message.id().to_base64()),
    })
}

/// What to call the member who caused this. A name we cannot read, or an empty
/// one, becomes the inbox's own placeholder rather than a failed send: the
/// event is worth announcing whether or not the roster row reads.
async fn display_name(ctx: &Context, user: EntityId) -> String {
    let Ok(view) = ctx.get::<UserView>(user).await else { return UNKNOWN_ACTOR.to_string() };
    match view.display_name() {
        Ok(name) if !name.trim().is_empty() => name,
        _ => UNKNOWN_ACTOR.to_string(),
    }
}

/// And what to call the room, on the same rule.
async fn room_name(ctx: &Context, room: EntityId) -> String {
    let Ok(view) = ctx.get::<RoomView>(room).await else { return UNKNOWN_ROOM.to_string() };
    match view.name() {
        Ok(name) if !name.trim().is_empty() => name,
        _ => UNKNOWN_ROOM.to_string(),
    }
}

/// Consumer loop: one notification at a time, errors contained per row. The
/// receiver is borrowed from the supervisor (`workers::supervise`), which
/// respawns this loop if it ever panics.
pub async fn run(
    ctx: Context,
    delivery: Delivery,
    floor_ms: i64,
    announced: Arc<Announced>,
    rx: &mut UnboundedReceiver<NotificationView>,
) {
    info!("push sender worker started (notification rows → APNs alerts)");
    while let Some(view) = rx.recv().await {
        let notification = view.id();
        if let Err(e) = announce(&ctx, &delivery, floor_ms, &announced, &view).await {
            warn!(notification = %notification, "push send failed: {e:#}");
        }
    }
    warn!("push sender worker: notification stream closed; exiting");
}

/// One row, from claim to send.
///
/// The claim is taken BEFORE the send, and stands even when the send goes
/// badly. A member with three devices whose second one failed must not have the
/// first rung again, and nothing retries anyway: the row reaches this worker
/// once, on the Add that created it.
async fn announce(ctx: &Context, delivery: &Delivery, floor_ms: i64, announced: &Announced, view: &NotificationView) -> Result<()> {
    let created_at = view.created_at().context("read notification created_at")?;
    if !claim(announced, floor_ms, view.id(), created_at) {
        debug!(notification = %view.id(), "push: this inbox row is not this process's to announce");
        return Ok(());
    }
    let announcement = read_announcement(ctx, view).await?;
    deliver(delivery, &announcement).await
}

/// An APNs that records what it was asked to send and answers however a test
/// wants, so the worker can be driven with no network under it.
#[cfg(test)]
pub mod stub {
    use super::*;
    use futures_util::future::BoxFuture;
    use futures_util::FutureExt;
    use std::collections::HashMap;

    #[derive(Default)]
    pub struct RecordingApns {
        sent: Mutex<Vec<(String, Alert)>>,
        /// Per device token, what APNs answers. Anything not named here is
        /// taken.
        answers: Mutex<HashMap<String, Outcome>>,
    }

    impl RecordingApns {
        pub fn new() -> Arc<Self> { Arc::new(Self::default()) }

        /// Script one device's answer.
        pub fn answers_for(&self, device_token: &str, outcome: Outcome) {
            self.answers.lock().unwrap().insert(device_token.to_string(), outcome);
        }

        /// Everything sent so far, in order.
        pub fn sent(&self) -> Vec<(String, Alert)> { self.sent.lock().unwrap().clone() }
    }

    impl Transport for RecordingApns {
        fn send<'a>(&'a self, device_token: &'a str, alert: &'a Alert) -> BoxFuture<'a, Result<Outcome>> {
            async move {
                self.sent.lock().unwrap().push((device_token.to_string(), alert.clone()));
                Ok(self.answers.lock().unwrap().get(device_token).cloned().unwrap_or(Outcome::Delivered))
            }
            .boxed()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::stub::RecordingApns;
    use super::*;
    use crate::push::store::{memory::MemoryDeviceTokens, Platform};

    const ALICE: &str = "AZk3jW0RvkW8pTGnQxYzRR";
    const BOB: &str = "BZk3jW0RvkW8pTGnQxYzRR";
    const GENERAL: &str = "RZk3jW0RvkW8pTGnQxYzRR";
    const RANDOM: &str = "SZk3jW0RvkW8pTGnQxYzRR";
    const NOTIFICATION: &str = "NZk3jW0RvkW8pTGnQxYzRR";
    const MESSAGE: &str = "MZk3jW0RvkW8pTGnQxYzRR";
    const PHONE: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const TABLET: &str = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";

    fn a_mention() -> Announcement {
        Announcement {
            recipient: ALICE.to_string(),
            notification: NOTIFICATION.to_string(),
            kind: MENTION_KIND.to_string(),
            actor: Some(Named { id: BOB.to_string(), name: "Bob".to_string() }),
            room: Some(Named { id: GENERAL.to_string(), name: "general".to_string() }),
            message: Some(MESSAGE.to_string()),
        }
    }

    fn a_dm() -> Announcement {
        Announcement {
            recipient: ALICE.to_string(),
            notification: NOTIFICATION.to_string(),
            kind: DM_KIND.to_string(),
            actor: Some(Named { id: BOB.to_string(), name: "Bob".to_string() }),
            room: None,
            message: None,
        }
    }

    #[test]
    fn a_mention_alert_reads_like_the_inbox_row_it_announces() {
        // The inbox renders "Bob mentioned you in #general"; the banner says
        // the same thing with the place as its title.
        let alert = alert_for(&a_mention());
        assert_eq!(alert.title, "#general");
        assert_eq!(alert.body, "Bob mentioned you");

        let payload = alert.payload();
        assert_eq!(payload["aps"]["alert"]["title"], "#general");
        assert_eq!(payload["aps"]["alert"]["body"], "Bob mentioned you");
        assert_eq!(payload["aps"]["sound"], "default", "a visible alert makes a sound; iOS renders the rest");
        assert!(payload["aps"].get("badge").is_none(), "no badge: an unread count is awareness this generation set aside");

        // The deep link, for the client leg: enough to open the room, mark the
        // row seen, and know which message it was about.
        let target = &payload[apns::TARGET_KEY];
        assert_eq!(target["kind"], MENTION_KIND);
        assert_eq!(target["notification"], NOTIFICATION);
        assert_eq!(target["room"], GENERAL);
        assert_eq!(target["message"], MESSAGE);
        assert_eq!(target["actor"], BOB);
    }

    #[test]
    fn a_dm_alert_puts_the_person_where_the_room_would_be() {
        // A DM happens in no room, so the conversation IS the sender — naming
        // Bob in the title and again in the body would say it twice.
        let alert = alert_for(&a_dm());
        assert_eq!(alert.title, "Bob");
        assert_eq!(alert.body, "sent you a direct message");

        let target = &alert.payload()[apns::TARGET_KEY];
        assert_eq!(target["kind"], DM_KIND);
        assert_eq!(target["actor"], BOB, "the sender is the deep-link target: Notification has no slot a DM thread fits in");
        // Absent ids are absent members, not nulls the client has to tell apart
        // from missing ones.
        assert!(target.get("room").is_none());
        assert!(target.get("message").is_none());
    }

    #[test]
    fn an_alert_falls_back_to_the_same_words_the_inbox_falls_back_to() {
        // A kind the phone's app predates still says something true, and a row
        // whose actor cannot be resolved is still worth announcing.
        let mut announcement = a_mention();
        announcement.kind = "reaction".to_string();
        assert_eq!(alert_for(&announcement).body, "Bob sent you a \u{201c}reaction\u{201d} notification");

        announcement.actor = None;
        assert_eq!(alert_for(&announcement).body, "Someone sent you a \u{201c}reaction\u{201d} notification");

        let mut nameless = a_mention();
        nameless.room = Some(Named { id: GENERAL.to_string(), name: "a room".to_string() });
        assert_eq!(alert_for(&nameless).title, "#a room");
    }

    #[test]
    fn a_collapse_id_names_one_conversation_for_one_recipient() {
        let one = collapse_id(&a_mention());
        // Every mention in the same room for the same member replaces the last
        // banner: the message and the notification are not in the key.
        let mut later = a_mention();
        later.notification = "later".to_string();
        later.message = Some("later".to_string());
        assert_eq!(collapse_id(&later), one, "a burst in one room leaves one banner");

        // A different room, a different member, and a DM are each their own.
        let mut elsewhere = a_mention();
        elsewhere.room = Some(Named { id: RANDOM.to_string(), name: "random".to_string() });
        assert_ne!(collapse_id(&elsewhere), one, "another room must not replace this room's banner");

        let mut someone_else = a_mention();
        someone_else.recipient = BOB.to_string();
        assert_ne!(collapse_id(&someone_else), one, "one phone can hold two members' registrations");

        assert_ne!(collapse_id(&a_dm()), one, "a DM is its own conversation");
        let mut from_carol = a_dm();
        from_carol.actor = Some(Named { id: RANDOM.to_string(), name: "Carol".to_string() });
        assert_ne!(collapse_id(&from_carol), collapse_id(&a_dm()), "each correspondent is their own conversation");

        // Apple refuses the whole alert over 64 bytes, so a kind of any length
        // is cut rather than sent.
        let mut wordy = a_mention();
        wordy.room = None;
        wordy.actor = None;
        wordy.kind = "k".repeat(500);
        assert!(collapse_id(&wordy).len() <= MAX_COLLAPSE_ID_BYTES);
        assert!(collapse_id(&a_mention()).len() <= MAX_COLLAPSE_ID_BYTES, "and the ordinary case is nowhere near it");
    }

    #[tokio::test]
    async fn every_device_a_member_registered_is_rung_once() {
        let tokens = MemoryDeviceTokens::new();
        tokens.register(ALICE, PHONE, Platform::Ios, 1).await.unwrap();
        tokens.register(ALICE, TABLET, Platform::Ios, 2).await.unwrap();
        // Bob's phone is Bob's: a notification for Alice must not reach it.
        tokens.register(BOB, "aa".repeat(32).as_str(), Platform::Ios, 3).await.unwrap();

        let apns = RecordingApns::new();
        deliver(&Delivery::new(tokens.clone(), apns.clone()), &a_mention()).await.unwrap();

        let sent = apns.sent();
        assert_eq!(sent.len(), 2, "a member with a phone and a tablet has both rung");
        let addressed: HashSet<&str> = sent.iter().map(|(device, _)| device.as_str()).collect();
        assert_eq!(addressed, HashSet::from([PHONE, TABLET]));
        assert_eq!(sent[0].1.body, "Bob mentioned you", "and both get the same alert");
        assert_eq!(sent[0].1, sent[1].1);
    }

    #[tokio::test]
    async fn a_member_with_no_registered_device_is_not_an_error() {
        let tokens = MemoryDeviceTokens::new();
        let apns = RecordingApns::new();
        deliver(&Delivery::new(tokens.clone(), apns.clone()), &a_mention()).await.unwrap();
        assert!(apns.sent().is_empty(), "nothing to send to, and nothing goes wrong");
    }

    #[tokio::test]
    async fn a_device_apns_reports_gone_leaves_the_registry_and_the_others_stay() {
        let tokens = MemoryDeviceTokens::new();
        tokens.register(ALICE, PHONE, Platform::Ios, 1).await.unwrap();
        tokens.register(ALICE, TABLET, Platform::Ios, 2).await.unwrap();

        let apns = RecordingApns::new();
        // The app was deleted from the phone; the tablet still has it. No
        // timestamp in Apple's answer, so there is nothing to weigh the
        // registration against and the report stands on its own.
        apns.answers_for(PHONE, Outcome::TokenGone { reason: apns::UNREGISTERED, invalidated_at: None });
        deliver(&Delivery::new(tokens.clone(), apns.clone()), &a_mention()).await.unwrap();

        let remaining = tokens.for_user(ALICE).await.unwrap();
        assert_eq!(remaining.len(), 1, "the device APNs says is gone is dropped");
        assert_eq!(remaining[0].token, TABLET, "and the one that took the alert is untouched");

        // A refusal that is NOT about the device leaves the registration
        // standing: a wrong APNS_TOPIC answers this way about every device at
        // once, and would otherwise empty the registry.
        let apns = RecordingApns::new();
        apns.answers_for(TABLET, Outcome::Refused { status: 400, reason: apns::DEVICE_TOKEN_NOT_FOR_TOPIC.to_string() });
        deliver(&Delivery::new(tokens.clone(), apns.clone()), &a_mention()).await.unwrap();
        assert_eq!(tokens.for_user(ALICE).await.unwrap().len(), 1, "a refusal about the send is not a report about the device");

        // Nor does `BadDeviceToken`, for the same reason: it judges the token
        // against ONE environment variable, so a deployment pointed at the
        // wrong APNs host hears it about every device it has.
        let apns = RecordingApns::new();
        apns.answers_for(TABLET, Outcome::Refused { status: 400, reason: apns::BAD_DEVICE_TOKEN.to_string() });
        deliver(&Delivery::new(tokens.clone(), apns.clone()), &a_mention()).await.unwrap();
        assert_eq!(tokens.for_user(ALICE).await.unwrap().len(), 1, "an environment mismatch must not empty the registry");

        // And a second send to a token already dropped is not an error: the
        // registry's `forget` accepts a row that is already gone.
        let apns = RecordingApns::new();
        apns.answers_for(TABLET, Outcome::TokenGone { reason: apns::UNREGISTERED, invalidated_at: None });
        deliver(&Delivery::new(tokens.clone(), apns.clone()), &a_mention()).await.unwrap();
        deliver(&Delivery::new(tokens.clone(), apns.clone()), &a_mention()).await.unwrap();
        assert!(tokens.for_user(ALICE).await.unwrap().is_empty());
    }

    /// A device that deleted the app and reinstalled it: Apple goes on
    /// reporting the OLD token gone for a while, and the report names the
    /// moment that became true. A row claimed since is the reinstall's, and
    /// dropping it would take away the registration that just arrived.
    #[tokio::test]
    async fn a_device_that_registered_after_apns_invalidated_it_keeps_its_row() {
        let invalidated_at = 1_700_000_000_000;
        let tokens = MemoryDeviceTokens::new();
        // The phone re-registered a second after Apple's invalidation; the
        // tablet has not been claimed since a second before it.
        tokens.register(ALICE, PHONE, Platform::Ios, invalidated_at + 1_000).await.unwrap();
        tokens.register(ALICE, TABLET, Platform::Ios, invalidated_at - 1_000).await.unwrap();

        let apns = RecordingApns::new();
        for device in [PHONE, TABLET] {
            apns.answers_for(device, Outcome::TokenGone { reason: apns::UNREGISTERED, invalidated_at: Some(invalidated_at) });
        }
        deliver(&Delivery::new(tokens.clone(), apns.clone()), &a_mention()).await.unwrap();

        let remaining = tokens.for_user(ALICE).await.unwrap();
        assert_eq!(remaining.len(), 1, "only the row older than the invalidation is dropped");
        assert_eq!(remaining[0].token, PHONE, "the reinstall's registration survives the report it predates");

        // The exact boundary is kept, not dropped: a row claimed in the same
        // millisecond Apple names is not older than the report.
        let tokens = MemoryDeviceTokens::new();
        tokens.register(ALICE, PHONE, Platform::Ios, invalidated_at).await.unwrap();
        deliver(&Delivery::new(tokens.clone(), apns.clone()), &a_mention()).await.unwrap();
        assert_eq!(tokens.for_user(ALICE).await.unwrap().len(), 1, "same instant is not older");
    }

    #[test]
    fn a_row_from_before_this_process_is_not_announced_again() {
        let announced: Announced = Mutex::new(HashSet::new());
        let boot = 1_700_000_000_000;
        let (old, fresh, another) = (EntityId::new(), EntityId::new(), EntityId::new());

        // The restart case: rows the last process minted are still in the
        // collection, and a standing query can hand them over. None of them
        // rings.
        assert!(!claim(&announced, boot, old, boot - 1), "a row minted a millisecond before this boot is history");
        assert!(!claim(&announced, boot, old, boot - 30 * 24 * 60 * 60 * 1000), "and so is last month's");

        // A row minted after the floor rings once, and only once however many
        // times it is observed.
        assert!(claim(&announced, boot, fresh, boot));
        assert!(!claim(&announced, boot, fresh, boot), "an un-delete arriving as a second Add must not ring again");
        assert!(claim(&announced, boot, another, boot + 5_000), "and a different row still does");
    }
}
