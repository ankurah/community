use ankurah::{property::Json, EntityId, Model, Ref};
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

// ---------------------------------------------------------------------------
// Direct messages (two-party) — #30
// ---------------------------------------------------------------------------
//
// The three `dm_*` collections below all carry their membership ON THE ROW, so
// the row itself answers "may this user see me": the policy read scope is
// `a = $jwt.sub OR b = $jwt.sub` over two `Ref<User>` fields, which is exactly
// what stock ankurah 0.9.0 can express for a two-party thread.
//
// WHY THE PARTICIPANT FIELDS ARE PLAIN `Ref<User>` AND NEVER `Option<Ref<User>>`
// ============================================================================
// Wave-1 taught this codebase a rule: a new field added to an EXISTING
// collection must be `Option<T>`, because rows written before the field existed
// carry no such property and only `Option<T>` reads an absent property as
// `None` instead of `PropertyError::Missing` (see `Room.topic`,
// `Message.collaborative`, `ModAction.message`). That rule is about RETROFITS.
//
// It must not be applied here, and applying it would be a security bug rather
// than a style choice. `Predicate::Or` evaluates its left operand first and
// propagates an evaluator error before it ever reaches the right operand, and
// comparing against a property an entity does not have IS an error, not
// `false`. So a `dm_*` row missing its `a` property is invisible to BOTH
// participants — including the one named by `b` — on the live path AND the
// fetch path, silently, with nothing delivered and no error surfaced. That is
// pinned by `server/tests/or_scope_live_tests.rs`
// (`a_row_missing_the_left_arm_is_invisible_even_to_the_right_arms_participant`).
//
// Therefore: these collections are BORN with both participant fields, every
// create path sets both, and no participant field may ever become optional or
// be added to a `dm_*` collection after the fact. A future reader "fixing" the
// missing `Option` here would make existing threads vanish for everyone in
// them. If a fourth participant-shaped field is ever needed, it needs a new
// collection, not a retrofit.
//
// The fields are `LWW` because that is the only backend available for a `Ref`
// — not because anything rewrites them. Nothing in this repo edits `a` or `b`
// after creation, and `server/tests/model_pin_tests.rs` pins that the create
// paths always set both.

/// Order a participant pair canonically: one unordered pair of users maps to
/// exactly one `(a, b)` tuple, so one pair ⇒ one `DmThread` and a find-or-create
/// on either side lands on the same query.
///
/// Ordering is `EntityId`'s own `Ord` — the ULID's 16 bytes, big-endian — NOT
/// the base64 text form. Those two orders differ (the base64url alphabet is not
/// ASCII-ordered), and the byte order is the one the spec names and the one the
/// storage engines collate `Ref` values by. The duplicate-thread tie-break in
/// the client uses this same `Ord` for the same reason; the older
/// `min_by_key(|r| r.id().to_base64())` idiom in `read_state`/notification prefs
/// is a different (purely local, order-irrelevant) decision and is not a
/// precedent for this one.
///
/// A self-pair (`x == y`) is returned unchanged. The UI never offers a
/// self-thread — the "Message" button is hidden on your own member card — but
/// the helper is total rather than partial so no caller has to invent an error
/// path for a case it cannot reach.
pub fn canonical_pair(x: EntityId, y: EntityId) -> (EntityId, EntityId) {
    if x <= y { (x, y) } else { (y, x) }
}

/// The lookup that finds a pair's threads: AnkQL source with four `?`
/// placeholders, to be populated with `a`, `b`, `b`, `a`.
///
/// It asks about BOTH orderings because nothing can insist on one. Clients
/// write the pair in [`canonical_pair`] order, but the `dm_thread` write scope
/// can only ask whether the writer is one of `a`/`b` — comparing the two
/// fields to each other is not something the scope grammar expresses — so a
/// row with the pair reversed is a row the server accepts. A lookup matching
/// only the canonical order would not see such a row, and find-or-create would
/// mint a second thread beside a perfectly good one, forking the conversation
/// in two. Matching both leaves a reversed row merely untidy:
/// [`canonical_thread`] picks the winner out of whatever comes back.
///
/// The source lives here, beside the ordering rule it compensates for, so that
/// it can be pinned by a test — the client that runs it is a wasm binary no
/// test in CI compiles, and a typo would surface only as a "Message" button
/// that quietly does nothing.
pub const THREADS_FOR_PAIR: &str = "((a = ? AND b = ?) OR (a = ? AND b = ?))";

/// Pick THE thread for a pair out of whatever the pair query returned.
///
/// Two clients opening their first DM at the same moment both find no thread
/// and both create one, and ankurah 0.9.0 has no entity deletion, so the twin
/// is permanent. Rather than a repair pass (which would race in its own right),
/// every reader collapses duplicates the same way: the LOWEST entity id wins,
/// and new messages are posted into it. The twin keeps whatever landed in it
/// during the race window — visible only if something did — and stops
/// collecting traffic the moment both clients have seen both rows.
///
/// This is deliberately the same `EntityId` ordering as [`canonical_pair`], and
/// it lives here rather than in the client so that the race test, the client,
/// and any future consumer converge by construction rather than by coincidence.
///
/// Winning decides where the NEXT message is written, and nothing more. What
/// landed in the twin during the race is still part of the conversation, so
/// every reading path takes the union of the pair's rows (see
/// `leptos-app/src/dm.rs`, `Conversation`); agreeing on where to write must not
/// make what was already written unreachable.
pub fn canonical_thread(candidates: impl IntoIterator<Item = EntityId>) -> Option<EntityId> {
    candidates.into_iter().min()
}

/// Given a thread's participants and the viewer, the OTHER participant — whose
/// name a DM row shows and who a DM notification goes to. `None` for a
/// degenerate self-thread, and for a viewer who is not a participant at all
/// (which the read scope already makes unreachable through a scoped context).
pub fn dm_partner(a: EntityId, b: EntityId, viewer: EntityId) -> Option<EntityId> {
    if a == viewer && b != viewer {
        Some(b)
    } else if b == viewer && a != viewer {
        Some(a)
    } else {
        None
    }
}

/// A two-party direct-message thread: exactly one row per unordered pair of
/// users, with the pair denormalized onto the row as `a`/`b` in
/// [`canonical_pair`] order.
///
/// Both participants are set at creation and never rewritten — see the module
/// section above for why they are plain `Ref<User>` and why that is
/// load-bearing.
///
/// Duplicate threads for one pair are possible (two clients opening their first
/// DM concurrently; entity deletion does not exist in ankurah 0.9.0). Readers
/// resolve the duplicate by pinning the LOWEST entity id as THE thread for the
/// pair, and post into that one; the twin is inert.
#[derive(Model, Debug, Serialize, Deserialize)]
pub struct DmThread {
    /// The lower-ordered participant. This pair IS the conversation's identity
    /// — every consumer that needs to know who a DM is between reads it from
    /// here rather than from the message's copies (see the `DmMessage` type
    /// doc) — so what it says has to stay true.
    ///
    /// No code path edits it after creation. Nothing enforces that: the write
    /// scope only asks whether the writer is one of `a`/`b`, so a participant's
    /// client could in principle rewrite the OTHER seat and hand the
    /// conversation, and its history, to a third person. Closing that needs a
    /// write gate the policy grammar cannot express today (a field pinned
    /// immutable after creation); it is recorded here rather than papered over,
    /// and it is a reason to be suspicious of any future client code that
    /// edits a thread row at all.
    #[active_type(LWW)]
    pub a: Ref<User>,
    /// The higher-ordered participant, on the same terms.
    #[active_type(LWW)]
    pub b: Ref<User>,
    /// ms since epoch (same unit as `Message.timestamp`).
    pub created_at: i64,
    /// Tombstone flag. **Nothing writes it today**, which is a ruling rather
    /// than an omission: the DM rate limiter tombstones the offending MESSAGE
    /// and leaves the conversation standing, because nothing in this codebase
    /// writes `deleted` back to `false` and an automatic penalty must not be
    /// able to destroy a two-way history it may have misjudged (see
    /// server/src/workers/dm_rate_limit.rs and docs/moderation.md). There is
    /// no user-facing "delete conversation" affordance either.
    ///
    /// The field stays because every client lists threads with
    /// `deleted = false`, and a row must carry the property for that filter to
    /// match it. Whoever gives this flag a writer owes the pair a way back:
    /// find-or-create (leptos-app/src/dm.rs) looks for live threads only, so a
    /// tombstoned thread would never be found, a second one would be minted
    /// beside it, and the original history would be unreachable for both
    /// participants.
    #[active_type(LWW)]
    pub deleted: bool,
}

/// One message in a `DmThread`.
///
/// `a`/`b` are copied verbatim from the thread at send: they exist so the
/// policy read scope can answer "may this user see me" from the row alone,
/// without a join the scope grammar cannot express. They are NOT the filing
/// key — every render path queries `thread = ?`, never `a`/`b`.
///
/// That split is what contains the one integrity nuance this design carries.
/// `a`/`b` are client-written LWW fields and the write scope checks them only
/// against the writer, so a member can hand-craft a row naming themselves and
/// ANY stranger, filed under any thread id they like — including a
/// conversation between two other people. The scope still stops them reading
/// anyone else's data (both scopes read the same two fields, so such a row
/// is visible to exactly the people it names) and still pins `user` to them.
///
/// What stops the claim being worth anything is an invariant every consumer
/// must keep: **`a`/`b` are a read-scope device, never a statement of who is
/// talking to whom.** Who a message is between is read from the row it is
/// filed under — `thread` — by the render paths, by the DM fan-out
/// (server/src/workers/dm_notify.rs) and by the rate limiter
/// (server/src/workers/dm_rate_limit.rs) alike. A consumer that skips the
/// thread lookup and believes these two fields hands every member an unlimited
/// notification channel to strangers, each notification deep-linking to a
/// conversation the stranger cannot open. That is not hypothetical: it is what
/// `claimed_participants_on_a_dm_notify_the_thread_not_the_claim` exists to
/// keep from coming back.
#[derive(Model, Debug, Serialize, Deserialize)]
pub struct DmMessage {
    /// The thread this message belongs to — the only field render paths filter
    /// on. Set at creation, never edited.
    #[active_type(LWW)]
    pub thread: Ref<DmThread>,
    /// Participant copied from the thread at send, for the read scope's use
    /// only — never read as the truth about who this message is between (see
    /// the type doc).
    #[active_type(LWW)]
    pub a: Ref<User>,
    /// The other one, on the same terms.
    #[active_type(LWW)]
    pub b: Ref<User>,
    /// The sender. The `dm_message` write scope pins this to the caller
    /// (`user = $jwt.sub`), so a participant cannot attribute a message in
    /// their own thread to the other person.
    #[active_type(LWW)]
    pub user: Ref<User>,
    /// Collaborative text, exactly like `Message.text`: no `#[active_type]`
    /// attribute means the derive picks `YrsString` for a `String`. DM text has
    /// to be the same active type as room text or the composer, the markdown
    /// renderer and the x-ray decoder would all need a second code path.
    pub text: String,
    /// ms since epoch (same unit as `Message.timestamp`), written by the
    /// sending client — and, unlike `Message.timestamp`, REWRITTEN BY THE
    /// SERVER when it claims a time the server clock has not reached yet.
    /// `server/src/workers/dm_timestamp.rs` settles it once, on first sight,
    /// and the whole DM lane sorts, counts, windows and compares on the settled
    /// value.
    ///
    /// So read it here as honest, and do not add a second correction
    /// downstream. A correction recomputed against the current clock moves
    /// every time it is evaluated, which is what put a future-dated message
    /// permanently at the top of a sidebar, relit a badge its reader could not
    /// clear, and re-dated a rate-limit window on every restart. It also could
    /// not reach the queries that sort by this field inside the query.
    pub timestamp: i64,
    /// Soft delete, like `Message.deleted`: rows are never removed (entity
    /// deletion does not exist in ankurah 0.9.0), they become tombstones.
    #[active_type(LWW)]
    pub deleted: bool,
    /// When the sender last edited the text (ms since epoch), `None` if never.
    /// `Option<i64>` matches `Message.edited_at` — but note the reason differs:
    /// there are no legacy `dm_message` rows, so this `Option` encodes "never
    /// edited", not "property might be absent". It is not a precedent for
    /// making a participant field optional.
    pub edited_at: Option<i64>,
}

/// Per-user, per-thread read cursor — the `ReadState` pattern, keyed on a
/// thread instead of a room. Unread badges in the DM sidebar are messages newer
/// than `last_read_ts` authored by the other participant.
///
/// DELIBERATE POLICY ASYMMETRY: unlike `DmThread`/`DmMessage`, this collection
/// carries NO `a`/`b` and its policy scope is `user = $jwt.sub` (the `readstate`
/// idiom), not `a = $jwt.sub OR b = $jwt.sub`. A read cursor is a read receipt:
/// scoping it to the participant pair would publish "when did you last look at
/// our thread" to your correspondent, which the room read cursors deliberately
/// do not do and which nothing in the DM design asks for. The row is private to
/// its owner on reads and writes, with no moderator bypass — same posture as
/// `readstate` and `notificationpref`.
#[derive(Model, Debug, Serialize, Deserialize)]
pub struct DmReadState {
    #[active_type(LWW)]
    pub user: Ref<User>,
    #[active_type(LWW)]
    pub thread: Ref<DmThread>,
    pub last_read_ts: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// [`THREADS_FOR_PAIR`] really is a query: it parses, it takes exactly the
    /// four parameters its callers pass, and it survives the `AND deleted =
    /// false` the client appends.
    ///
    /// Pinned here because the only code that runs it is a wasm binary CI never
    /// compiles into a test. A typo in that string would not fail a build; it
    /// would fail as a "Message" button that quietly does nothing, on a path
    /// whose whole job is to not open a second thread for a pair.
    #[test]
    fn the_pair_lookup_parses_and_takes_four_parameters() {
        use ankurah::ankql::{ast::Expr, parser::parse_selection};

        let (a, b) = canonical_pair(EntityId::new(), EntityId::new());
        let as_the_client_writes_it = format!("{THREADS_FOR_PAIR} AND deleted = false");

        let selection = parse_selection(&as_the_client_writes_it).expect("the pair lookup parses, parentheses and all");
        selection
            .predicate
            .clone()
            .populate([Expr::from(&a), Expr::from(&b), Expr::from(&b), Expr::from(&a)])
            .expect("four placeholders take the pair in both orders");

        // And exactly four: a caller passing one ordering only must fail
        // closed rather than silently querying half the question.
        assert!(
            selection.predicate.populate([Expr::from(&a), Expr::from(&b)]).is_err(),
            "a placeholder/parameter mismatch must fail rather than pass a half-populated query"
        );
    }
}
