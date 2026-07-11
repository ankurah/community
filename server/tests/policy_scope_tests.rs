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
struct FakeMessage {
    user: EntityId,
    collaborative: Option<bool>,
}

impl Filterable for FakeMessage {
    fn collection(&self) -> &str {
        "message"
    }
    fn value(&self, name: &str) -> Option<Value> {
        match name {
            "user" => Some(Value::EntityId(self.user)),
            "collaborative" => self.collaborative.map(Value::Bool),
            _ => None,
        }
    }
}

#[test]
fn author_edit_allowed_even_when_collaborative_absent() {
    let me = EntityId::new();
    let msg = FakeMessage { user: me, collaborative: None };
    // Left disjunct is true, OR short-circuits: the absent property is never
    // touched. This is what keeps every pre-existing message editable by its
    // author after the schema gained `collaborative`.
    assert_eq!(evaluate_predicate(&msg, &message_write_predicate(me)), Ok(true));
}

#[test]
fn non_author_denied_on_absent_collaborative_via_error_path() {
    let me = EntityId::new();
    let author = EntityId::new();
    let msg = FakeMessage { user: author, collaborative: None };
    // Left disjunct false → right disjunct touches the absent property and
    // errors. enforce_write_scope turns any evaluator error into
    // AccessDenied, so the outcome is a (correct) denial.
    assert_eq!(evaluate_predicate(&msg, &message_write_predicate(me)), Err(FilterError::PropertyNotFound("collaborative".to_string())));
}

#[test]
fn non_author_allowed_exactly_when_collaborative_true() {
    let me = EntityId::new();
    let author = EntityId::new();
    let opted_in = FakeMessage { user: author, collaborative: Some(true) };
    assert_eq!(evaluate_predicate(&opted_in, &message_write_predicate(me)), Ok(true));

    let opted_out = FakeMessage { user: author, collaborative: Some(false) };
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

#[test]
fn modaction_is_world_readable_and_moderator_writable() {
    let config = policy();
    let rules = &config.collections["modaction"];
    assert_eq!(rules.read.as_deref(), Some("view"), "the moderation log is public by design");
    assert_eq!(rules.write.as_deref(), Some("moderate"));
    assert!(rules.scope.is_empty());
}

/// The derive lowercases the struct name for the collection id; the policy is
/// keyed by those strings. A silent mismatch would leave a collection with no
/// rules (deny-all) — catch it here.
#[test]
fn model_collection_names_match_policy_keys() {
    let config = policy();
    for collection in [
        community_model::Reaction::collection(),
        community_model::ReadState::collection(),
        community_model::ModAction::collection(),
        community_model::Ban::collection(),
    ] {
        assert!(config.collections.contains_key(collection.as_str()), "policy.json has no entry for collection '{}'", collection.as_str());
    }
}
