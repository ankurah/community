//! Community's data model, in two halves.
//!
//! The chat half — `Message`, `Room`, `User`, `Reaction`, `ReadState`, the DM
//! trio (`DmThread`, `DmMessage`, `DmReadState`) with their pair helpers, the
//! `text` scanner module behind [`parse_mentions`] / [`extract_urls`], and the
//! `mention_display` composer codec that mirrors that scanner's token rules —
//! is defined in `ankurah-chat-model` and re-exported from here. Community
//! shares it with every other Ankurah chat surface, so a chat panel embedded
//! in someone else's page reads these rows through the same definitions the
//! server writes them with, and neither side can drift from the other.
//!
//! The community half is defined below: `UserRoles`, `Ban`, `ModAction`,
//! `Report`, `Notification`, `LinkPreview`, `NotificationPref`, `PushDevice` —
//! moderation, the report queue, the notification inbox, the link-preview
//! cache, and the self-scoped mobile delivery registry. These reference the
//! shared types freely.
//!
//! The re-export is a glob on purpose. ankurah's derive emits a family of
//! types per collection (`MessageView`, `MessageMut`, `MessageResultSet`,
//! `MessageLiveQuery`, …); an enumerated list would go stale the first time a
//! consumer reached for one of them, and the whole shared model is meant to
//! come through, not a curated slice of it.

use ankurah::{property::Json, Model, Ref};
use serde::{Deserialize, Serialize};

pub use ankurah_chat_model::*;

/// Server-maintained display cache of a user's roles — one row per user.
///
/// Roles are NOT managed here. The source of truth is the idp.to `roles` claim
/// carried in the verified id_token and baked into the ankurah session token at
/// mint time (see `server::resolve_roles`); this row only mirrors that result so
/// the UI can render role badges without decoding the caller's JWT.
///
/// It is written exclusively by the server's privileged (Root) context. The
/// `userroles` policy entry requires a `system` write privilege that no role
/// holds, so remote JWT-bearing clients can never write it — otherwise a client
/// could spoof its own role badges.
#[derive(Model, Debug, Serialize, Deserialize)]
pub struct UserRoles {
    #[active_type(LWW)]
    pub user: Ref<User>,
    /// JSON array of stable lowercase role keys (e.g. `["member","moderator"]`),
    /// mirroring the roles minted into the user's most recent session token.
    pub roles: Json,
}

/// A moderation ban. Enforced at token mint (banned users are refused a new
/// session) and — once the guarded policy agent lands — live at the durable
/// node, so existing connections lose access as soon as the ban syncs.
#[derive(Model, Debug, Serialize, Deserialize)]
pub struct Ban {
    #[active_type(LWW)]
    pub user: Ref<User>,
    pub reason: String,
    pub created_at: i64,
    /// Bans are lifted by flipping this off (audit trail stays).
    #[active_type(LWW)]
    pub active: bool,
}

/// Public moderation-log row, created whenever a moderator acts on a message
/// (e.g. deleting it) or on a member (banning/unbanning). Readable by every
/// signed-in member by design — the community can see what moderation happened
/// — and by nobody else: the `modaction` read privilege is `signed_in`, which a
/// guest session (community#79) does not hold. Writable only with the
/// `moderate` privilege.
///
/// Exactly one of `message` / `user` is set per row: whichever names the
/// target, with `action` saying what was done to it. Both are `Option`
/// because a row only carries the property for its own target kind (and
/// absent properties only read cleanly through `Option<T>`).
#[derive(Model, Debug, Serialize, Deserialize)]
pub struct ModAction {
    /// The moderator who acted, or `None` when nothing human did — today the
    /// DM rate limiter (server/src/workers/dm_rate_limit.rs), which tombstones
    /// abusive traffic and logs it here so moderators can escalate to a Ban.
    ///
    /// This became `Option` when automated actions arrived, which is the
    /// textbook retrofit: every pre-existing row carries the property with a
    /// value and therefore reads as `Some`, while an absent property (none
    /// exist) would read as `None` rather than `PropertyError::Missing`. As
    /// with `ModAction.message`, keep comparisons on this field equality-only.
    #[active_type(LWW)]
    pub actor: Option<Ref<User>>,
    /// The message acted upon, for message-targeted actions ("delete",
    /// "restore"). `None` on user-targeted rows, which have no message —
    /// there is no null `Ref`, so `Option` is the only honest encoding.
    /// Every pre-ban row has this property, so legacy rows read as `Some`;
    /// rows created with `None` simply never write it. Queries filtering on
    /// `message = ?` skip such rows on every engine, but the mechanisms
    /// differ: sled and the reactor deny per-row on absent-property errors,
    /// while IndexedDB excludes them via the composite equality index (null
    /// is not a valid key) and would PROPAGATE a per-row error otherwise —
    /// so keep `message` comparisons equality-only (no `!=`/`IN`) unless
    /// the client fetch path grows real per-row fail-closed semantics.
    #[active_type(LWW)]
    pub message: Option<Ref<Message>>,
    /// The member acted upon, for user-targeted actions ("ban", "unban").
    /// `None` on message-targeted rows and absent on all legacy rows.
    #[active_type(LWW)]
    pub user: Option<Ref<User>>,
    /// What was done, as a stable lowercase verb (e.g. "delete", "restore",
    /// "ban", "unban"). LWW, not collaborative text: it is an enum-like atom.
    #[active_type(LWW)]
    pub action: String,
    /// Optional human-readable justification, shown in the public log.
    pub reason: Option<String>,
    pub created_at: i64,
}

/// A member's complaint about one message, filed for a moderator to judge.
///
/// FOR: the only route by which something a moderator never saw becomes
/// something they can act on. Moderators do not browse direct messages at all
/// (the dm read scopes have no `moderate` bypass — docs/moderation.md), and
/// nothing in the product opens another member's history, so without this row
/// a moderator learns about abuse only by happening to read it.
///
/// Who sees it: moderators and admins, and nobody else — not even the person
/// who filed it. The `report` read scope in policy.json is a comparison no row
/// can satisfy, applied to every caller without `moderate`, so a member's query
/// answers empty and a by-id read is refused. That is deliberate for v1: a
/// queue only moderators read is a queue nothing else has to reason about, and
/// a reporter who could watch their own filing would learn when a moderator
/// looked at it.
///
/// Who writes it: any signed-in member, for themselves. The write scope pins
/// `reporter` to the caller, pins `resolved` to false, and pins `created_at` at
/// or after the epoch — so a report can be filed in nobody else's name, closed
/// by nobody who filed it, and dated into no corner of the read scope the
/// shutout leaves unguarded. Moderators bypass all three, which is what lets
/// them resolve.
///
/// WHAT A FILER MAY STILL CHANGE, and why that makes this row a CLAIM. jwt-auth
/// evaluates the write scope against the prior state as well as the new one on
/// an update, so a resolved report is frozen: its filer can neither reopen it
/// nor rewrite what it says. An OPEN one is not. Every write rule holds both
/// before and after, so until a moderator closes it, the member who filed a
/// report may change which message and which room it names.
///
/// Nothing in the policy language can bind that. A scope filter names row
/// properties and `$jwt.*` claims and nothing else — there is no prior-state
/// variable and no way to tell an update from an insert — and any predicate a
/// filing satisfies is satisfied again by that same row as the before-state of
/// its first edit, so no predicate admits the one and refuses the other. The
/// consequence for whoever reads this row: `message` is the filer's claim about
/// what is at issue, and `room` is a claim about where. The moderator queue
/// renders the room from the message it resolves rather than from this row for
/// exactly that reason (`leptos-app/src/reports.rs`), and
/// `report_policy_live_tests` pins the residual so the day ankurah can express
/// the rule, closing it is a deliberate act.
#[derive(Model, Debug, Serialize, Deserialize)]
pub struct Report {
    /// Who filed it. Pinned to the caller by the write scope, so this is the
    /// one attribution a client cannot claim falsely — the same guarantee
    /// `DmMessage.user` gets from the sender-binding rule.
    #[active_type(LWW)]
    pub reporter: Ref<User>,
    /// The message complained about. Required at creation and never `Option`:
    /// a report about nothing is not a report, and a required field has no
    /// absent-property path for a scope or a query to trip over.
    #[active_type(LWW)]
    pub message: Ref<Message>,
    /// Where it was said, as the FILER named it — a claim, not a fact. It is
    /// copied off the message at filing time so a queue can name a place
    /// without resolving every reported message first, and it duplicates
    /// `Message.room` on purpose: a message never moves between rooms, so a
    /// truthful copy cannot go stale. What can change is this copy, for as
    /// long as the report is open (see the struct doc). So the moderator queue
    /// prints the room off the message it resolves and falls back to this only
    /// when that message will not read.
    #[active_type(LWW)]
    pub room: Ref<Room>,
    /// The reporter's own words, optional — "no reason given" is a real
    /// answer and must not be a blocked filing. `Option<String>` reads an
    /// absent property as `None` rather than `PropertyError::Missing`, the
    /// `ModAction.reason` idiom.
    pub reason: Option<String>,
    /// ms since epoch (same unit as `Message.timestamp`), and never before it:
    /// the write scope refuses a negative stamp, because the read scope hides
    /// every row by comparing this property against zero and a row dated below
    /// that would be one its filer could read back.
    pub created_at: i64,
    /// The lifecycle, and the thing the queue sorts on. Born `false` on every
    /// row so no report lacks the property — the write scope compares against
    /// it, and an absent property there would be an evaluator error rather
    /// than a clean answer. Rows are never deleted (entity deletion does not
    /// exist in ankurah 0.9.0), so a handled report stays as the record.
    #[active_type(LWW)]
    pub resolved: bool,
    /// The moderator who closed it, `None` while open — `ModAction.actor`'s
    /// stamping idiom, including its rule: keep comparisons on this field
    /// equality-only, since a `None` and an absent property are one value to
    /// a reader and two to an index.
    #[active_type(LWW)]
    pub resolved_by: Option<Ref<User>>,
    /// When it was closed (ms since epoch), `None` while open. `Option<i64>`
    /// for the same absent-property reason as `Message.edited_at`.
    pub resolved_at: Option<i64>,
}

/// One inbox row per (recipient, cause). Created exclusively by the server's
/// notification fan-out worker under the privileged (Root) context — clients
/// never create rows for OTHER users (the notification write scope in
/// policy.json is `recipient = $jwt.sub`, so the only client write that can
/// succeed is a user updating their own row, i.e. flipping `seen`).
///
/// The recipient's inbox is a LiveQuery on `recipient = ?`; the read scope
/// (`recipient = $jwt.sub`) makes every other user's rows invisible, so a
/// notification is private to the person it addresses.
#[derive(Model, Debug, Serialize, Deserialize)]
pub struct Notification {
    /// Who this notification is FOR. Immutable in practice; the policy scope
    /// pins both reads and writes to this field.
    #[active_type(LWW)]
    pub recipient: Ref<User>,
    /// Stable lowercase discriminator (today only "mention"). LWW, not
    /// collaborative text: it is an enum-like atom, like `ModAction.action`.
    #[active_type(LWW)]
    pub kind: String,
    /// The message that caused the notification. `Some` for kind="mention";
    /// `Option` because future kinds (e.g. a room-level announcement) may have
    /// no message, and absent properties only read cleanly through `Option<T>`
    /// (a bare `Ref` surfaces `PropertyError::Missing`). Queries touching this
    /// field must stay equality-only — see the `ModAction.message` note.
    #[active_type(LWW)]
    pub message: Option<Ref<Message>>,
    /// Who triggered it (for mentions: the message author). `Option` for the
    /// same future-kinds / absent-property reason as `message`.
    #[active_type(LWW)]
    pub actor: Option<Ref<User>>,
    /// Where it happened, so the inbox can deep-link into the room. `Option`
    /// for the same reason as `message`.
    #[active_type(LWW)]
    pub room: Option<Ref<Room>>,
    /// ms since epoch (same unit as `Message.timestamp`).
    pub created_at: i64,
    /// The one field the recipient writes: flipped true when the inbox row is
    /// acknowledged. Rows are never deleted (entity deletion does not exist in
    /// ankurah 0.9.0) — `seen` is the lifecycle.
    #[active_type(LWW)]
    pub seen: bool,
}

/// Server-maintained cache of a fetched link preview — one row per URL, keyed
/// by exact-string equality on `url` (the dedup key: the unfurl worker checks
/// `url = ?` before fetching, and clients look previews up the same way, so
/// both sides must derive URLs with [`extract_urls`]).
///
/// Written exclusively by the server's unfurl worker under the Root context.
/// The `linkpreview` policy entry requires a `system` write privilege that no
/// role holds, so clients can never spoof a preview for a URL they posted —
/// otherwise a message could carry a made-up title/image for a deceptive link.
#[derive(Model, Debug, Serialize, Deserialize)]
pub struct LinkPreview {
    /// The URL exactly as extracted from message text (no normalization
    /// beyond `extract_urls`' trailing-punctuation trim). LWW atom.
    #[active_type(LWW)]
    pub url: String,
    /// og:title, falling back to `<title>`. `None` when the page had neither
    /// (or `ok` is false). `Option<String>` so absent properties read as
    /// `None` rather than `PropertyError::Missing`.
    pub title: Option<String>,
    /// og:description (or `<meta name="description">`).
    pub description: Option<String>,
    /// Absolute http(s) og:image URL, resolved against the final fetched URL.
    /// Non-http(s) or relative-only values are dropped server-side.
    pub image_url: Option<String>,
    /// ms since epoch when the fetch attempt finished (same unit as
    /// `Message.timestamp`).
    pub fetched_at: i64,
    /// False when the fetch failed, timed out, tripped an SSRF guard, or the
    /// response was not HTML. A false row is deliberately persisted so the
    /// client renders a plain link AND the worker never refetches a known-bad
    /// URL (the row's existence is the idempotency check).
    #[active_type(LWW)]
    pub ok: bool,
}

/// Per-user notification preferences — one row per user, created lazily by the
/// client the first time the user touches notification settings. Fully private
/// (the notificationpref policy scopes both reads and writes to
/// `user = $jwt.sub`, like `readstate`). The server's fan-out worker reads it
/// under Root (which bypasses scopes) to decide whether to deliver.
#[derive(Model, Debug, Serialize, Deserialize)]
pub struct NotificationPref {
    #[active_type(LWW)]
    pub user: Ref<User>,
    /// When true, suppress every notification kind EXCEPT mentions. A no-op
    /// while "mention" is the only kind, but the fan-out worker is structured
    /// so future kinds (e.g. room activity) respect it.
    #[active_type(LWW)]
    pub mentions_only: bool,
    /// JSON array of room entity-id strings (base64, same encoding as
    /// `EntityId::to_base64`) the user has muted, e.g. `["4QUv…","9zAb…"]` —
    /// mirrors the `UserRoles.roles` Json-array idiom. Mentions in a muted
    /// room produce no notification.
    pub muted_rooms: Json,
}

/// One APNs address registered by one member's app installation.
///
/// This is an Ankurah collection deliberately, not a side table or an HTTP
/// registration RPC. The app writes it through its ephemeral node, and the
/// policy agent is the confidentiality boundary: `policy.json` scopes reads
/// and writes to `user = $jwt.sub`. A member can see and update every device
/// registered to their own account and nobody else's; the durable node's Root
/// context reads active rows for delivery.
///
/// Rows are deactivated instead of deleted because Ankurah entities have no
/// delete lifecycle. Re-registering the same `(user, token)` reactivates and
/// refreshes its existing row; APNs invalidation and sign-out flip `active`
/// false. Historical events remain visible only to the owning member and Root.
pub const MAX_PUSH_DEVICES_PER_USER: usize = 10;

#[derive(Model, Debug, Serialize, Deserialize)]
pub struct PushDevice {
    #[active_type(LWW)]
    pub user: Ref<User>,
    /// APNs device token, hexadecimal as issued by Apple. This is a delivery
    /// credential and must never be readable by another member or appear in a
    /// whole-token log line.
    #[active_type(LWW)]
    pub token: String,
    /// Delivery service discriminator. `ios` today; stored as an atom so a
    /// later Android transport has to be recognized explicitly by the sender.
    #[active_type(LWW)]
    pub platform: String,
    /// Milliseconds since epoch when this installation most recently claimed
    /// the token. The per-member cap retains the ten newest active rows.
    #[active_type(LWW)]
    pub last_registered_at: i64,
    /// False after sign-out, APNs invalidation, or cap eviction.
    #[active_type(LWW)]
    pub active: bool,
}
