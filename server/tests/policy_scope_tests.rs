//! Pins the evaluator semantics the wave-1 policy relies on, against the
//! *actual* policy.json in the repo root — if someone edits a filter string
//! or a rule's shape, these tests fail before production does.
//!
//! Background (verified against ankurah-core 0.9.0 sources; see the model
//! substrate commit): `evaluate_predicate` errors with `PropertyNotFound`
//! when a comparison touches a property the entity does not have, and
//! `Predicate::Or` short-circuits left-to-right. The message write scope
//! `user = $jwt.sub OR collaborative = true` therefore:
//!
//! - always allows the author (left disjunct true — `collaborative` never
//!   evaluated, so legacy rows without the property keep working);
//! - denies non-authors on rows without `collaborative` via the error path
//!   (ankurah-jwt-auth's `enforce_write_scope` maps evaluator errors to
//!   `AccessDenied` — deny is the correct outcome, just via `Err` rather
//!   than a clean `false`);
//! - allows non-authors exactly when `collaborative = true`.

use ankurah::ankql::{ast::Expr, parser::parse_selection};
use ankurah::core::selection::filter::{evaluate_predicate, Error as FilterError, Filterable};
use ankurah::core::value::Value;
use ankurah::model::Model;
use ankurah::EntityId;
use ankurah_jwt_auth::PolicyConfig;

const POLICY_JSON: &str = include_str!("../../policy.json");

fn policy() -> PolicyConfig {
    serde_json::from_str(POLICY_JSON).expect("policy.json must parse as an ankurah-jwt-auth PolicyConfig")
}

/// Build the message write-scope predicate exactly the way the agent does:
/// `$jwt.sub` becomes a `?` placeholder, populated as a typed EntityId
/// literal (never spliced into the query text).
fn message_write_predicate(caller: EntityId) -> ankurah::ankql::ast::Predicate {
    let config = policy();
    let rule = &config.collections["message"].scope[0];
    let query = rule.filter.replace("$jwt.sub", "?");
    parse_selection(&query)
        .expect("message scope filter parses")
        .predicate
        .populate([Expr::from(&caller)])
        .expect("one placeholder, one value")
}

/// A message row as the scope evaluator sees it. `collaborative: None` models
/// both a legacy row (property never existed) and a row created with
/// `collaborative: None` — the LWW backend returns no value for either.
/// `deleted` is different: required at creation and in the model since
/// ankurah-chat's first model commit, so no real row lacks it — `None` here
/// exists only for the fail-closed pin in
/// [`absent_deleted_denies_via_error_path`].
struct FakeMessage {
    user: EntityId,
    collaborative: Option<bool>,
    deleted: Option<bool>,
}

impl Filterable for FakeMessage {
    fn collection(&self) -> &str {
        "message"
    }
    fn value(&self, name: &str) -> Option<Value> {
        match name {
            "user" => Some(Value::EntityId(self.user)),
            "collaborative" => self.collaborative.map(Value::Bool),
            "deleted" => self.deleted.map(Value::Bool),
            _ => None,
        }
    }
}

#[test]
fn author_edit_allowed_even_when_collaborative_absent() {
    let me = EntityId::new();
    let msg = FakeMessage { user: me, collaborative: None, deleted: Some(false) };
    // Left disjunct is true, OR short-circuits: the absent property is never
    // touched. This is what keeps every pre-existing message editable by its
    // author after the schema gained `collaborative`.
    assert_eq!(evaluate_predicate(&msg, &message_write_predicate(me)), Ok(true));
}

#[test]
fn non_author_denied_on_absent_collaborative_via_error_path() {
    let me = EntityId::new();
    let author = EntityId::new();
    let msg = FakeMessage { user: author, collaborative: None, deleted: Some(false) };
    // Left disjunct false → right disjunct touches the absent property and
    // errors. enforce_write_scope turns any evaluator error into
    // AccessDenied, so the outcome is a (correct) denial.
    assert_eq!(evaluate_predicate(&msg, &message_write_predicate(me)), Err(FilterError::PropertyNotFound("collaborative".to_string())));
}

#[test]
fn non_author_allowed_exactly_when_collaborative_true() {
    let me = EntityId::new();
    let author = EntityId::new();
    let opted_in = FakeMessage { user: author, collaborative: Some(true), deleted: Some(false) };
    assert_eq!(evaluate_predicate(&opted_in, &message_write_predicate(me)), Ok(true));

    let opted_out = FakeMessage { user: author, collaborative: Some(false), deleted: Some(false) };
    assert_eq!(evaluate_predicate(&opted_out, &message_write_predicate(me)), Ok(false));
}

#[test]
fn message_scope_rule_shape_unchanged() {
    let config = policy();
    let rule = &config.collections["message"].scope[0];
    assert_eq!(rule.unless_privilege.as_deref(), Some("moderate"), "moderators bypass the message write scope");
    assert!(rule.applies_to.applies_to_writes() && !rule.applies_to.applies_to_reads(), "message scope gates writes only");
    // The self-check must stay the LEFT disjunct: OR short-circuits, and only
    // that ordering guarantees author writes never evaluate `collaborative`.
    assert!(
        rule.filter.trim().starts_with("user = $jwt.sub"),
        "author check must be the left disjunct of the message write scope, got: {}",
        rule.filter
    );
}

/// Build the message read-scope predicate the way the agent does. No `$jwt`
/// variable in this one — the filter is the constant `deleted = false` — but
/// the same build-from-policy.json discipline keeps the test pinned to the
/// shipped rule rather than a copy of it.
fn message_read_predicate() -> ankurah::ankql::ast::Predicate {
    let config = policy();
    let rule = &config.collections["message"].scope[1];
    parse_selection(&rule.filter).expect("message read-scope filter parses").predicate
}

/// A moderator-deleted message fails the read scope: non-moderator queries
/// have this predicate ANDed in (`filter_predicate`), and a by-id fetch
/// evaluates it against the entity state (`check_read`), so the row is
/// unreachable both ways — including by entity id.
#[test]
fn deleted_messages_fail_the_read_scope() {
    let deleted = FakeMessage { user: EntityId::new(), collaborative: None, deleted: Some(true) };
    assert_eq!(evaluate_predicate(&deleted, &message_read_predicate()), Ok(false));
}

#[test]
fn live_messages_pass_the_read_scope() {
    let live = FakeMessage { user: EntityId::new(), collaborative: None, deleted: Some(false) };
    assert_eq!(evaluate_predicate(&live, &message_read_predicate()), Ok(true));
}

/// `deleted` is required at creation and predates every prod row, so no real
/// message lacks it. If one ever did, the evaluator errors and jwt-auth maps
/// the error to a denial — pinned so the fail-closed direction never
/// silently flips.
#[test]
fn absent_deleted_denies_via_error_path() {
    let msg = FakeMessage { user: EntityId::new(), collaborative: None, deleted: None };
    assert_eq!(evaluate_predicate(&msg, &message_read_predicate()), Err(FilterError::PropertyNotFound("deleted".to_string())));
}

/// The read-scope rule's shape: reads only (writes have their own rule at
/// index 0), moderators bypass — their tooling (timeline, x-ray, restore)
/// still needs deleted rows visible.
#[test]
fn message_read_scope_rule_shape() {
    let config = policy();
    let rule = &config.collections["message"].scope[1];
    assert_eq!(rule.filter, "deleted = false");
    assert_eq!(rule.unless_privilege.as_deref(), Some("moderate"), "moderators keep deleted rows for restore/review");
    assert!(rule.applies_to.applies_to_reads() && !rule.applies_to.applies_to_writes(), "message read scope gates reads only");
}

#[test]
fn readstate_rows_are_private_to_their_owner_on_reads_and_writes() {
    let config = policy();
    let rules = &config.collections["readstate"];
    assert_eq!(rules.read.as_deref(), Some("view"));
    assert_eq!(rules.write.as_deref(), Some("post"));
    let rule = &rules.scope[0];
    assert_eq!(rule.filter, "user = $jwt.sub");
    assert!(rule.unless_privilege.is_none(), "not even moderators read others' read state");
    assert!(
        rule.applies_to.applies_to_reads() && rule.applies_to.applies_to_writes(),
        "readstate scope must constrain both reads and writes"
    );
}

#[test]
fn reaction_scope_gates_writes_only_with_no_moderator_bypass() {
    let config = policy();
    let rules = &config.collections["reaction"];
    assert_eq!(rules.read.as_deref(), Some("view"));
    assert_eq!(rules.write.as_deref(), Some("post"));
    let rule = &rules.scope[0];
    assert_eq!(rule.filter, "user = $jwt.sub");
    assert!(rule.unless_privilege.is_none(), "moderators do not edit others' reactions");
    assert!(rule.applies_to.applies_to_writes() && !rule.applies_to.applies_to_reads());
}

/// Build the ban read-scope predicate the way the agent does (same `?`
/// placeholder discipline as [`message_write_predicate`]).
fn ban_read_predicate(caller: EntityId) -> ankurah::ankql::ast::Predicate {
    let config = policy();
    let rule = &config.collections["ban"].scope[0];
    let query = rule.filter.replace("$jwt.sub", "?");
    parse_selection(&query).expect("ban scope filter parses").predicate.populate([Expr::from(&caller)]).expect("one placeholder, one value")
}

/// A ban row as the scope evaluator sees it. `user` is a required field set at
/// creation, so unlike `collaborative` there is no absent-property error path
/// to model here.
struct FakeBan {
    user: EntityId,
}

impl Filterable for FakeBan {
    fn collection(&self) -> &str {
        "ban"
    }
    fn value(&self, name: &str) -> Option<Value> {
        match name {
            "user" => Some(Value::EntityId(self.user)),
            _ => None,
        }
    }
}

/// The ban signal is self-readable: a banned user's own rows pass the read
/// scope, so the client's self-lock LiveQuery (`user = ? AND active = true`)
/// actually receives the ban that locks it out.
#[test]
fn banned_user_sees_their_own_ban_rows() {
    let me = EntityId::new();
    let ban = FakeBan { user: me };
    assert_eq!(evaluate_predicate(&ban, &ban_read_predicate(me)), Ok(true));
}

/// Non-moderators must not learn who else is banned: any row whose `user` is
/// someone else fails the read scope.
#[test]
fn non_moderator_cannot_see_others_ban_rows() {
    let me = EntityId::new();
    let them = EntityId::new();
    let ban = FakeBan { user: them };
    assert_eq!(evaluate_predicate(&ban, &ban_read_predicate(me)), Ok(false));
}

/// Moderators see every ban row: the read scope carries
/// `unless_privilege: "moderate"`, and both privileged roles hold that
/// privilege (the agent skips the filter entirely for them).
#[test]
fn moderators_bypass_the_ban_read_scope() {
    let config = policy();
    let rule = &config.collections["ban"].scope[0];
    assert_eq!(rule.unless_privilege.as_deref(), Some("moderate"), "moderators must see all ban rows");
    assert!(
        config.roles_have_privilege(&["moderator".to_string()], "moderate")
            && config.roles_have_privilege(&["admin".to_string()], "moderate"),
        "both privileged roles hold `moderate`, so both bypass the ban read scope"
    );
    // The scope gates reads only: writes are already collection-gated to
    // `moderate` below, and a read-write scope would be misleading about
    // where write enforcement actually lives.
    assert!(rule.applies_to.applies_to_reads() && !rule.applies_to.applies_to_writes(), "ban scope filters visibility only");
    assert_eq!(rule.filter, "user = $jwt.sub");
}

/// Members can read (their own) ban rows but never write any: `ban.read` is
/// the baseline `view` privilege, `ban.write` stays `moderate`, and the
/// member role does not hold `moderate`.
#[test]
fn members_read_bans_but_cannot_write_them() {
    let config = policy();
    let rules = &config.collections["ban"];
    assert_eq!(rules.read.as_deref(), Some("view"), "every member passes the collection read gate; the scope does the row filtering");
    assert_eq!(rules.write.as_deref(), Some("moderate"), "only moderators issue or lift bans");
    assert!(
        !config.roles_have_privilege(&["member".to_string()], "moderate"),
        "the member role must not hold `moderate`, or the ban write gate is meaningless"
    );
    assert_eq!(rules.scope.len(), 1, "exactly the self-visibility rule — a second scope rule would AND in and narrow it");
}

/// The moderation log is lights-on to the community and closed to the street.
/// It moved from `view` to `signed_in` when guests arrived (#79): every
/// signed-in member still reads every row — that is the whole point of
/// lights-on moderation — while a guest, who holds `view` and nothing else,
/// reads none of it. Who was banned and why is community business, not an
/// anonymous visitor's.
#[test]
fn modaction_is_signed_in_readable_and_moderator_writable() {
    let config = policy();
    let rules = &config.collections["modaction"];
    assert_eq!(rules.read.as_deref(), Some("signed_in"), "signed-in members read the whole log; guests read none of it");
    assert_eq!(rules.write.as_deref(), Some("moderate"));
    assert!(rules.scope.is_empty());
    assert!(config.roles_have_privilege(&["member".to_string()], "signed_in"), "no member loses sight of the log");
}

/// The derive lowercases the struct name for the collection id; the policy is
/// keyed by those strings. A silent mismatch would leave a collection with no
/// rules (deny-all) — catch it here.
///
/// All fourteen collections community serves are listed, and that completeness
/// now spans two repositories: eight of these structs are declared in
/// ankurah-chat-model, where nothing can see policy.json. A rename there would
/// arrive as a collection this policy has no entry for, which is why the list
/// has to be exhaustive rather than a sample.
#[test]
fn model_collection_names_match_policy_keys() {
    let config = policy();
    for collection in [
        community_model::User::collection(),
        community_model::Room::collection(),
        community_model::Message::collection(),
        community_model::UserRoles::collection(),
        community_model::Reaction::collection(),
        community_model::ReadState::collection(),
        community_model::ModAction::collection(),
        community_model::Ban::collection(),
        community_model::Notification::collection(),
        community_model::LinkPreview::collection(),
        community_model::NotificationPref::collection(),
        community_model::DmThread::collection(),
        community_model::DmMessage::collection(),
        community_model::DmReadState::collection(),
    ] {
        assert!(config.collections.contains_key(collection.as_str()), "policy.json has no entry for collection '{}'", collection.as_str());
    }
}

/// Build a collection's single self-scope predicate the way the agent does
/// (same `?` placeholder discipline as [`message_write_predicate`]). The
/// wave-2 private collections (notification, notificationpref) each carry
/// exactly one such rule.
fn self_scope_predicate(collection: &str, caller: EntityId) -> ankurah::ankql::ast::Predicate {
    let config = policy();
    let rule = &config.collections[collection].scope[0];
    let query = rule.filter.replace("$jwt.sub", "?");
    parse_selection(&query).expect("scope filter parses").predicate.populate([Expr::from(&caller)]).expect("one placeholder, one value")
}

/// A notification row as the scope evaluator sees it. `recipient` is set at
/// creation by the fan-out worker, so there is no absent-property path.
struct FakeNotification {
    recipient: EntityId,
}

impl Filterable for FakeNotification {
    fn collection(&self) -> &str {
        "notification"
    }
    fn value(&self, name: &str) -> Option<Value> {
        match name {
            "recipient" => Some(Value::EntityId(self.recipient)),
            _ => None,
        }
    }
}

/// The inbox is private: a recipient's own rows pass the scope (their inbox
/// LiveQuery receives them, and their `seen` flip satisfies the write path),
/// while anyone else's rows fail it.
#[test]
fn recipient_passes_notification_scope_others_fail_it() {
    let me = EntityId::new();
    let them = EntityId::new();
    let predicate = self_scope_predicate("notification", me);
    assert_eq!(evaluate_predicate(&FakeNotification { recipient: me }, &predicate), Ok(true));
    assert_eq!(evaluate_predicate(&FakeNotification { recipient: them }, &predicate), Ok(false));
}

/// Pin the shape the fan-out design depends on. Deliberate decision recorded
/// here: `write` is "post" + self-scope, NOT "system". can_write_collection is
/// checked BEFORE scope evaluation (ankurah-jwt-auth 0.9.0 check_event), so a
/// system-only write gate would also block the recipient flipping `seen` on
/// their own row. The server worker creates rows under Root (is_privileged
/// bypasses both the gate and the scope); the scoped write means the worst a
/// client can do is fabricate a notification addressed to ITSELF (the
/// after-state must satisfy `recipient = $jwt.sub`) — accepted self-spam
/// trade-off, never cross-user spoofing.
#[test]
fn notification_scope_rule_shape_unchanged() {
    let config = policy();
    let rules = &config.collections["notification"];
    assert_eq!(rules.read.as_deref(), Some("view"));
    assert_eq!(rules.write.as_deref(), Some("post"), "members must hold the write privilege or they cannot flip `seen`");
    assert!(
        config.can_write_collection(&["member".to_string()], &"notification".into()),
        "the member role must pass the notification collection write gate (the seen flip depends on it)"
    );
    assert_eq!(rules.scope.len(), 1, "exactly the self-visibility rule — a second rule would AND in and narrow it");
    let rule = &rules.scope[0];
    assert_eq!(rule.filter, "recipient = $jwt.sub");
    assert!(rule.unless_privilege.is_none(), "not even moderators read others' inboxes");
    assert!(
        rule.applies_to.applies_to_reads() && rule.applies_to.applies_to_writes(),
        "notification scope must constrain both reads and writes"
    );
}

/// Link previews are a world-readable cache that only the server may write:
/// `write` is the "system" privilege, which no role holds — a client that
/// could write linkpreview rows could attach a forged title/image to any URL
/// it posts (phishing). The unfurl worker writes under Root, which bypasses
/// the gate entirely.
#[test]
fn linkpreview_world_readable_but_no_role_can_write_it() {
    let config = policy();
    let rules = &config.collections["linkpreview"];
    assert_eq!(rules.read.as_deref(), Some("view"), "previews render for every member");
    assert_eq!(rules.write.as_deref(), Some("system"));
    assert!(rules.scope.is_empty(), "no row filtering — the cache is public");
    for role in config.roles.keys() {
        assert!(
            !config.can_write_collection(std::slice::from_ref(role), &"linkpreview".into()),
            "role '{role}' can write linkpreview — 'system' must remain a privilege no role holds"
        );
    }
}

/// Notification prefs are fully private, the readstate idiom: both reads and
/// writes scoped to the owner, no moderator bypass. The fan-out worker reads
/// them under Root (bypasses scopes) to honor mutes.
#[test]
fn notificationpref_rows_are_private_to_their_owner_on_reads_and_writes() {
    let config = policy();
    let rules = &config.collections["notificationpref"];
    assert_eq!(rules.read.as_deref(), Some("view"));
    assert_eq!(rules.write.as_deref(), Some("post"));
    assert_eq!(rules.scope.len(), 1);
    let rule = &rules.scope[0];
    assert_eq!(rule.filter, "user = $jwt.sub");
    assert!(rule.unless_privilege.is_none(), "not even moderators read others' notification prefs");
    assert!(
        rule.applies_to.applies_to_reads() && rule.applies_to.applies_to_writes(),
        "notificationpref scope must constrain both reads and writes"
    );
}

// ---------------------------------------------------------------------------
// Direct messages (#30): the participant-pair scopes
// ---------------------------------------------------------------------------

/// Build a participant-pair predicate the way the agent does. The filter names
/// `$jwt.sub` TWICE, so the placeholder substitution yields two `?`s and both
/// are populated with the same caller id.
fn pair_scope_predicate(collection: &str, rule_index: usize, caller: EntityId) -> ankurah::ankql::ast::Predicate {
    let config = policy();
    let rule = &config.collections[collection].scope[rule_index];
    let query = rule.filter.replace("$jwt.sub", "?");
    parse_selection(&query)
        .expect("dm scope filter parses")
        .predicate
        .populate([Expr::from(&caller), Expr::from(&caller)])
        .expect("two placeholders, two values")
}

/// A `dm_thread` row as the scope evaluator sees it. Both participants are
/// `Option` HERE — not in the model — so the absent-property hazard the model's
/// "born with both fields" rule exists to prevent can be exercised.
struct FakeDmThread {
    a: Option<EntityId>,
    b: Option<EntityId>,
}

impl Filterable for FakeDmThread {
    fn collection(&self) -> &str { "dmthread" }
    fn value(&self, name: &str) -> Option<Value> {
        match name {
            "a" => self.a.map(Value::EntityId),
            "b" => self.b.map(Value::EntityId),
            _ => None,
        }
    }
}

/// Both participants pass the thread scope — the left arm short-circuits for
/// the one named `a`, and the one named `b` is reached only after the left arm
/// is evaluated and returns false. A third party fails it.
#[test]
fn both_dm_participants_pass_the_thread_scope_and_strangers_fail_it() {
    let me = EntityId::new();
    let them = EntityId::new();
    let stranger = EntityId::new();

    let as_me = pair_scope_predicate("dmthread", 0, me);
    assert_eq!(evaluate_predicate(&FakeDmThread { a: Some(me), b: Some(them) }, &as_me), Ok(true), "left arm names me");
    assert_eq!(evaluate_predicate(&FakeDmThread { a: Some(them), b: Some(me) }, &as_me), Ok(true), "right arm names me");

    let as_stranger = pair_scope_predicate("dmthread", 0, stranger);
    assert_eq!(evaluate_predicate(&FakeDmThread { a: Some(me), b: Some(them) }, &as_stranger), Ok(false), "a stranger is in neither arm");
}

/// The constraint the model's participant fields exist to satisfy, restated
/// against the REAL policy filter (the spike proved it end-to-end through the
/// reactor with a test-local policy; this pins the shipped rule).
///
/// A row missing `a` is denied to the participant named by `b`: the left
/// comparison errors on the absent property and `Predicate::Or` propagates that
/// error instead of falling through to the right arm. `enforce_read_scope` maps
/// evaluator errors to `AccessDenied`, so the row is invisible to BOTH
/// participants — which is why no `dm_*` participant field may ever be
/// `Option` or added after the collection ships.
#[test]
fn a_dm_row_missing_the_left_participant_is_denied_even_to_the_right_one() {
    let me = EntityId::new();
    let predicate = pair_scope_predicate("dmthread", 0, me);
    assert_eq!(
        evaluate_predicate(&FakeDmThread { a: None, b: Some(me) }, &predicate),
        Err(FilterError::PropertyNotFound("a".to_string())),
        "an absent left arm errors the whole OR — a denial, not a fall-through"
    );
}

/// A `dm_message` row as the evaluator sees it: the pair that gates
/// read/write, plus the sender the second write rule pins.
struct FakeDmMessage {
    a: EntityId,
    b: EntityId,
    user: EntityId,
}

impl Filterable for FakeDmMessage {
    fn collection(&self) -> &str { "dmmessage" }
    fn value(&self, name: &str) -> Option<Value> {
        match name {
            "a" => Some(Value::EntityId(self.a)),
            "b" => Some(Value::EntityId(self.b)),
            "user" => Some(Value::EntityId(self.user)),
            _ => None,
        }
    }
}

/// Writing a DM message must satisfy BOTH rules (scope rules AND together):
/// the writer is one of the two participants, AND the message is attributed to
/// the writer. So a participant can post into their own thread as themselves,
/// and cannot post as the other person; a stranger fails the first rule
/// whatever they claim.
#[test]
fn dm_message_write_requires_participation_and_self_attribution() {
    let me = EntityId::new();
    let them = EntityId::new();
    let stranger = EntityId::new();

    // The sender rule is the SECOND rule on dmmessage, so it needs the
    // rule-index form rather than `self_scope_predicate` (which reads rule 0).
    let pair_rule = |caller| pair_scope_predicate("dmmessage", 0, caller);
    let sender_rule = |caller: EntityId| {
        let config = policy();
        let query = config.collections["dmmessage"].scope[1].filter.replace("$jwt.sub", "?");
        parse_selection(&query).expect("sender filter parses").predicate.populate([Expr::from(&caller)]).expect("one placeholder")
    };

    // Me, in my own thread, as myself: both rules pass.
    let mine = FakeDmMessage { a: me, b: them, user: me };
    assert_eq!(evaluate_predicate(&mine, &pair_rule(me)), Ok(true));
    assert_eq!(evaluate_predicate(&mine, &sender_rule(me)), Ok(true));

    // Me, in my own thread, attributed to THEM: the pair rule passes (I am a
    // participant), the sender rule denies it. This is the rule that stops a
    // participant putting words in the other person's mouth.
    let spoofed = FakeDmMessage { a: me, b: them, user: them };
    assert_eq!(evaluate_predicate(&spoofed, &pair_rule(me)), Ok(true));
    assert_eq!(evaluate_predicate(&spoofed, &sender_rule(me)), Ok(false), "sender binding must reject a mis-attributed message");

    // A stranger fails the pair rule outright, even attributing honestly.
    assert_eq!(evaluate_predicate(&mine, &pair_rule(stranger)), Ok(false));
    let stranger_msg = FakeDmMessage { a: me, b: them, user: stranger };
    assert_eq!(evaluate_predicate(&stranger_msg, &pair_rule(stranger)), Ok(false));
}

/// The shipped rule shapes for all three dm collections, pinned. The ruling
/// this encodes (community#30, 2026-08-04): **DMs are private from moderators**
/// — no `unless_privilege` anywhere in these scopes. Abuse response flows
/// through reports that carry a message ref, never through browsing threads.
/// Adding moderator visibility later is a one-line change HERE, which is
/// exactly why the absence must be asserted rather than assumed.
#[test]
fn dm_scope_rule_shapes_unchanged_and_no_moderator_bypass() {
    let config = policy();

    let thread = &config.collections["dmthread"];
    assert_eq!(thread.read.as_deref(), Some("view"));
    assert_eq!(thread.write.as_deref(), Some("post"));
    assert_eq!(thread.scope.len(), 1, "one rule: the participant pair. A second rule would AND in and narrow it");
    assert_eq!(thread.scope[0].filter, "a = $jwt.sub OR b = $jwt.sub");
    assert!(
        thread.scope[0].applies_to.applies_to_reads() && thread.scope[0].applies_to.applies_to_writes(),
        "the pair rule must gate reads AND writes: it is what stops anyone opening a thread between two other people"
    );
    assert!(thread.scope[0].unless_privilege.is_none(), "DMs are private from moderators (community#30 ruling)");

    let message = &config.collections["dmmessage"];
    assert_eq!(message.read.as_deref(), Some("view"));
    assert_eq!(message.write.as_deref(), Some("post"));
    assert_eq!(message.scope.len(), 2, "the pair rule plus the sender-binding rule");
    assert_eq!(message.scope[0].filter, "a = $jwt.sub OR b = $jwt.sub");
    assert!(message.scope[0].applies_to.applies_to_reads() && message.scope[0].applies_to.applies_to_writes());
    assert!(message.scope[0].unless_privilege.is_none(), "DMs are private from moderators (community#30 ruling)");
    assert_eq!(message.scope[1].filter, "user = $jwt.sub");
    assert!(
        message.scope[1].applies_to.applies_to_writes() && !message.scope[1].applies_to.applies_to_reads(),
        "sender binding is a WRITE rule; as a read rule it would hide the other person's messages from you"
    );
    assert!(message.scope[1].unless_privilege.is_none(), "moderators do not author or edit DM messages either");

    // The read state is the deliberate asymmetry: a read cursor is a read
    // receipt, so it stays private to its owner (the `readstate` idiom) rather
    // than being shared with the correspondent by a pair scope.
    let read_state = &config.collections["dmreadstate"];
    assert_eq!(read_state.read.as_deref(), Some("view"));
    assert_eq!(read_state.write.as_deref(), Some("post"));
    assert_eq!(read_state.scope.len(), 1);
    assert_eq!(read_state.scope[0].filter, "user = $jwt.sub", "NOT the participant pair — see the DmReadState model doc");
    assert!(read_state.scope[0].applies_to.applies_to_reads() && read_state.scope[0].applies_to.applies_to_writes());
    assert!(read_state.scope[0].unless_privilege.is_none(), "not even moderators read others' DM read cursors");
}

/// Members must hold the collection-level write privilege for all three dm
/// collections, or the row scopes never get a chance to run: jwt-auth checks
/// `can_write_collection` BEFORE evaluating scopes (the `notification`
/// precedent). Clients create their own threads and messages, so "system"
/// would break the feature outright.
#[test]
fn members_can_write_dm_collections_at_the_collection_gate() {
    let config = policy();
    for collection in ["dmthread", "dmmessage", "dmreadstate"] {
        assert!(
            config.can_write_collection(&["member".to_string()], &collection.into()),
            "the member role must pass the {collection} collection write gate; the row scope does the real filtering"
        );
    }
}

// ---------------------------------------------------------------------------
// Guests (#79): what a session with nobody signed in may read
// ---------------------------------------------------------------------------

/// The whole guest posture in one privilege split. `view` is the read tier an
/// anonymous visitor gets — rooms, messages, reactions, link previews, the
/// things a page has to render to be worth arriving at — plus, since `user`
/// gained `retrieve: view` (ankurah-jwt-auth 0.9.2), a user row the guest can
/// NAME: message refs carry author ids, and following one is how a guest
/// renders "Ada" instead of "Unknown". `signed_in` is the tier the bearer must
/// have signed in for, and it still guards what it always guarded — the
/// roster boundary moved from `user`'s entry gate to its scan check (listing
/// and querying), while `userroles` (membership list by another door) and
/// `modaction` (moderation records are community business) stay entirely
/// behind the gate.
///
/// Every half is asserted, because any one alone would pass while the feature
/// was broken: a guest with too much lists the roster, and a guest with too
/// little sees an empty room and nameless authors.
#[test]
fn the_guest_read_tier_covers_the_conversation_and_stops_at_the_roster() {
    let config = policy();
    let guest = ["guest".to_string()];
    assert_eq!(config.roles["guest"], vec!["view".to_string()], "a guest holds the read tier and nothing else");

    for collection in ["room", "message", "reaction", "linkpreview"] {
        assert!(
            config.can_access_collection(&guest, &collection.into()),
            "a guest must be able to read {collection} — it is part of what an anonymous reader sees"
        );
    }
    assert!(
        config.can_access_collection(&guest, &"user".into()),
        "a guest reaches user's entry gate: the retrieve tier is what lets a ref follow resolve an author"
    );
    assert!(
        !config.can_scan_collection(&guest, &"user".into()),
        "a guest must NOT scan user: retrieve admits naming a row, never listing the roster"
    );
    for collection in ["userroles", "modaction"] {
        assert!(
            !config.can_access_collection(&guest, &collection.into()),
            "a guest must NOT reach {collection}: no retrieve field, so the collection gate refuses as it always did"
        );
    }
}

/// A guest writes nothing, anywhere. The `post` privilege is what every write
/// gate in this policy is keyed to (bar the moderator and system ones), and the
/// guest role does not hold it — so read-only is a property of the role, not of
/// each collection's rules being individually correct.
#[test]
fn a_guest_holds_no_write_privilege_over_any_collection() {
    let config = policy();
    let guest = ["guest".to_string()];
    for collection in config.collections.keys() {
        assert!(
            !config.can_write_collection(&guest, &collection.as_str().into()),
            "a guest must not be able to write {collection}"
        );
    }
}

/// The no-regression half of the split: moving three reads from `view` to
/// `signed_in` must leave every signed-in role seeing exactly what it saw
/// before. All three hold the new privilege, so all three still pass those
/// gates.
#[test]
fn signed_in_roles_keep_every_collection_they_could_read_before() {
    let config = policy();
    for role in ["member", "moderator", "admin"] {
        let roles = [role.to_string()];
        assert!(config.roles_have_privilege(&roles, "signed_in"), "role '{role}' must hold the signed-in read privilege");
        for collection in config.collections.keys() {
            assert!(
                config.can_access_collection(&roles, &collection.as_str().into()),
                "role '{role}' lost read access to {collection}"
            );
        }
    }
}

/// The invariant that has to keep holding as roles are added, and the reason it
/// needs stating at all: a SCAN — listing, querying, subscribing — passes on
/// the read privilege OR the write one. `signed_in` shuts the roster and the
/// mod log today only because no role holds `post`, `moderate` or `system`
/// without also holding `signed_in` — add `"contributor": ["view", "post"]`
/// and it would scan the roster through `user`'s WRITE gate, having been
/// granted no read privilege for it at all.
///
/// So this asserts the biconditional over every role in the file, the ones
/// there now and the ones somebody adds later: SCANNING one of the three is
/// exactly holding `signed_in`. The entry gate is asserted separately,
/// because `user`'s is deliberately wider — `retrieve: view` admits a
/// by-name ref follow (that is the guest author-names feature) — and this
/// pins that it is exactly that wide and no wider, while `userroles` and
/// `modaction` keep gate == signed_in with no widening at all.
#[test]
fn only_signed_in_roles_scan_the_signed_in_collections() {
    let config = policy();
    for role in config.roles.keys() {
        let roles = [role.clone()];
        let signed_in = config.roles_have_privilege(&roles, "signed_in");
        for collection in ["user", "userroles", "modaction"] {
            assert_eq!(
                config.can_scan_collection(&roles, &collection.into()),
                signed_in,
                "role '{role}' scans {collection} iff it holds signed_in — a role that may WRITE a collection also passes its scan check"
            );
        }
        assert_eq!(
            config.can_access_collection(&roles, &"user".into()),
            signed_in || config.roles_have_privilege(&roles, "view"),
            "role '{role}' reaches user's entry gate iff it holds signed_in or view — retrieve: view is the only widening"
        );
        for collection in ["userroles", "modaction"] {
            assert_eq!(
                config.can_access_collection(&roles, &collection.into()),
                signed_in,
                "role '{role}' reaches {collection} iff it holds signed_in — no retrieve field, the gate is still the boundary"
            );
        }
    }
}

/// A guest's `sub` is the literal `guest` (`server/src/guest.rs`, `GUEST_SUB`),
/// which is not an entity id and never equals one — so every row-local scope in
/// this policy fails closed for a guest without any rule being written about
/// guests. This pins the property those rules now silently depend on: the
/// literal cannot be parsed as an id, so it cannot collide with a member's.
#[test]
fn the_guest_subject_is_not_an_entity_id() {
    assert!(EntityId::from_base64("guest").is_err(), "the guest subject must never parse as an entity id");
}
