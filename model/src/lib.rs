use ankurah::{property::Json, Model, Ref};
use serde::{Deserialize, Serialize};

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
