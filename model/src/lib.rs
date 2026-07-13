use ankurah::{property::Json, Model, Ref};
use serde::{Deserialize, Serialize};

pub mod mention_display;
pub mod text;
pub use text::{extract_urls, parse_mentions};

#[derive(Model, Debug, Serialize, Deserialize)]
pub struct User {
    pub display_name: String,
    /// Stable subject identifier from the OIDC provider (idp.to `sub`). `None`
    /// for legacy anonymous users; `Some` once a user signs in. Users are keyed
    /// on this so repeat sign-ins resolve to the same `User` entity.
    pub oidc_sub: Option<String>,
}

// Room model - chat rooms
#[derive(Model, Debug, Serialize, Deserialize)]
pub struct Room {
    pub name: String,
    /// Creator of the room. `None` for the seeded default rooms, which makes
    /// them moderator-managed only (see the room write scope in policy.json:
    /// `created_by = $jwt.sub` unless the caller holds `moderate`).
    pub created_by: Option<Ref<User>>,
    /// Room topic, shown in the chat header. `Option` is required, not
    /// stylistic: rooms created before this field existed have no `topic`
    /// property at all, and only `Option<T>` maps an absent property to
    /// `None` (`Property for Option<T>` catches `PropertyError::Missing`;
    /// bare types surface it as an error).
    pub topic: Option<String>,
}

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

#[derive(Model, Debug, Serialize, Deserialize)]
pub struct Message {
    #[active_type(LWW)]
    pub user: Ref<User>,
    #[active_type(LWW)]
    pub room: Ref<Room>,
    pub text: String,
    pub timestamp: i64,
    #[active_type(LWW)]
    pub deleted: bool,
    /// When the author last edited the message (ms since epoch), `None` if
    /// never edited. `Option<i64>` because messages predating this field have
    /// no such property and only `Option<T>` reads an absent property as
    /// `None` instead of `PropertyError::Missing`.
    pub edited_at: Option<i64>,
    /// Author opt-in allowing other members to edit this message's text (the
    /// message write scope in policy.json is `user = $jwt.sub OR
    /// collaborative = true`). `Option<bool>` rather than `bool`: legacy
    /// messages have no such property, and a bare `bool` read would error
    /// with `PropertyError::Missing` instead of defaulting. Absent/`None`
    /// means not collaborative. Only the author can flip this (a non-author
    /// write must satisfy the scope on the post-write state too, and with
    /// `collaborative` no longer `true` it would not).
    pub collaborative: Option<bool>,
    /// The message this one replies to (#23, nested replies). `None` for
    /// ordinary messages, and absent on every pre-reply row — only
    /// `Option<T>` reads an absent property as `None` (bare types surface
    /// `PropertyError::Missing`). Two storage shapes collapse to that `None`:
    /// a fresh row created with `None` carries the property with a null
    /// value (the derive initializes every field), a legacy row lacks the
    /// key entirely. Same read, different bytes — so queries touching this
    /// field must stay equality-only, per the `ModAction.message` note. Set
    /// at creation, never edited.
    #[active_type(LWW)]
    pub re: Option<Ref<Message>>,
}

/// A user's emoji reaction to a message. One row per (message, user, emoji);
/// un-reacting flips `active` off rather than deleting the row (entity
/// deletion does not exist in ankurah 0.9.0), and re-reacting flips it back.
/// The reaction write scope in policy.json (`user = $jwt.sub`) has no
/// `unless_privilege`: moderators do not edit other people's reactions.
#[derive(Model, Debug, Serialize, Deserialize)]
pub struct Reaction {
    #[active_type(LWW)]
    pub message: Ref<Message>,
    #[active_type(LWW)]
    pub user: Ref<User>,
    /// The emoji itself (e.g. "👍"). LWW, not collaborative text: it is an
    /// atom chosen from a picker, never edited character-wise.
    #[active_type(LWW)]
    pub emoji: String,
    #[active_type(LWW)]
    pub active: bool,
}

/// Per-user, per-room read cursor: the timestamp of the newest message the
/// user has seen in that room. One row per (user, room), upserted as the user
/// views rooms; unread badges are messages newer than `last_read_ts`. The
/// readstate policy scopes both reads and writes to `user = $jwt.sub`, so
/// these rows are private to their owner.
#[derive(Model, Debug, Serialize, Deserialize)]
pub struct ReadState {
    #[active_type(LWW)]
    pub user: Ref<User>,
    #[active_type(LWW)]
    pub room: Ref<Room>,
    pub last_read_ts: i64,
}

/// Public moderation-log row, created whenever a moderator acts on a message
/// (e.g. deleting it) or on a member (banning/unbanning). World-readable by
/// design — the community can see what moderation happened — but only
/// writable with the `moderate` privilege.
///
/// Exactly one of `message` / `user` is set per row: whichever names the
/// target, with `action` saying what was done to it. Both are `Option`
/// because a row only carries the property for its own target kind (and
/// absent properties only read cleanly through `Option<T>`).
#[derive(Model, Debug, Serialize, Deserialize)]
pub struct ModAction {
    /// The moderator who acted.
    #[active_type(LWW)]
    pub actor: Ref<User>,
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
/// otherwise a message could carry a forged title/image for a phishing link.
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
