//! What the report queue actually admits, against the REAL `policy.json` and
//! the REAL `Report` model on a real (sled) node.
//!
//! `policy_scope_tests.rs` pins what the evaluator does with a hand-built row;
//! this pins what a signed-in context can and cannot reach, which is the claim
//! the feature rests on. Concretely:
//!
//! - a member files a report naming themselves, and that write lands;
//! - a member filing one that names ANOTHER member as the reporter is refused;
//! - a member filing one dated before the epoch is refused, which is what keeps
//!   the read shutout's comparison unsatisfiable;
//! - a member reads zero report rows — empty on fetch, empty on a live query,
//!   refused by entity id — including the report they filed themselves;
//! - a member cannot close a report, their own included, and cannot reopen one
//!   a moderator closed — though they MAY still retarget one that is still
//!   open, which is pinned here as the residual it is;
//! - a moderator reads the whole queue and resolves rows they did not file;
//! - a guest is refused the collection outright.
//!
//! WHAT THIS FILE DOES NOT WIDEN. A report row names a message, and the row is
//! moderator-readable only — so nothing here changes what a member may read
//! about any message. The message read scope (`deleted = false` unless
//! `moderate`) is untouched by this feature and keeps deciding that on its own.
//!
//! Sled-gated like the other live-node suites: run with
//! `cargo test -p community-server --no-default-features --features sled`.
#![cfg(feature = "sled")]

use ankurah::signals::Subscribe;
use ankurah::{Context, EntityId, Node};
use ankurah_jwt_auth::{JwtAgent, JwtClaims, JwtContext, PolicyConfig};
use ankurah_storage_sled::SledStorageEngine;
use community_model::{Message, Report, ReportView, Room, User};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The shipped policy, verbatim — the point of this harness is that nothing
/// here restates a rule that production could then drift away from.
const POLICY_JSON: &str = include_str!("../../policy.json");

/// A durable node carrying the real policy, plus a Root context for setup
/// writes (Root bypasses both the collection gate and the row scopes).
async fn report_node() -> (Node<SledStorageEngine, JwtAgent>, Context) {
    let agent = JwtAgent::new_ephemeral();
    agent.update_config(serde_json::from_str::<PolicyConfig>(POLICY_JSON).expect("policy.json parses"));
    let node = Node::new_durable(Arc::new(SledStorageEngine::new_test().unwrap()), agent);
    node.system.wait_loaded().await;
    if node.system.root().is_none() {
        node.system.create().await.unwrap();
    }
    node.system.wait_system_ready().await;
    let root = node.context_async(JwtContext::system()).await;
    (node, root)
}

/// A signed-in context for `user` with the given roles. Token fidelity matches
/// the DM suite's rationale: on a single local node the token string is never
/// verified, and every check under test reads the claims.
fn context_for(node: &Node<SledStorageEngine, JwtAgent>, user: EntityId, roles: &[&str]) -> Context {
    let claims = JwtClaims {
        sub: user.to_base64(),
        roles: roles.iter().map(|r| r.to_string()).collect(),
        email: format!("{}@report-tests.invalid", user.to_base64()),
        name: None,
        custom: serde_json::Map::new(),
    };
    node.context(JwtContext::from_claims(claims, "report-placeholder-token".to_string())).expect("context")
}

fn member(node: &Node<SledStorageEngine, JwtAgent>, user: EntityId) -> Context { context_for(node, user, &["member"]) }

/// A moderator context: `member` plus `moderate`, exactly what
/// `server::resolve_roles` mints for an idp.to moderator.
fn moderator(node: &Node<SledStorageEngine, JwtAgent>, user: EntityId) -> Context {
    context_for(node, user, &["member", "moderator"])
}

/// The context a guest token produces, as `server/src/guest.rs` mints it: the
/// literal subject, the one role, no email.
fn guest(node: &Node<SledStorageEngine, JwtAgent>) -> Context {
    let claims = JwtClaims {
        sub: "guest".to_string(),
        roles: vec!["guest".to_string()],
        email: String::new(),
        name: None,
        custom: serde_json::Map::new(),
    };
    node.context(JwtContext::from_claims(claims, "guest-placeholder-token".to_string())).expect("context")
}

/// One room, one author, one message worth complaining about.
async fn seed_conversation(root: &Context) -> (EntityId, EntityId, EntityId) {
    let trx = root.begin();
    let author = trx.create(&User { display_name: "Author".to_string(), oidc_sub: None }).await.unwrap().id();
    let room = trx.create(&Room { name: "general".to_string(), created_by: None, topic: None }).await.unwrap().id();
    trx.commit().await.unwrap();

    let trx = root.begin();
    let message = trx
        .create(&Message {
            user: author.into(),
            room: room.into(),
            text: "the message under complaint".to_string(),
            timestamp: 1,
            deleted: false,
            edited_at: None,
            collaborative: None,
            re: None,
        })
        .await
        .unwrap()
        .id();
    trx.commit().await.unwrap();

    (author, room, message)
}

/// File one report under `ctx` and report whether the write survived. The scope
/// denial can surface at either `create` or `commit` (jwt-auth evaluates the
/// write scope against the post-write entity, and which of the two calls reaches
/// that check is an internal detail), so the whole attempt is one Result — the
/// DM suite's `try_send` rationale, for the same reason.
async fn try_file(
    ctx: &Context,
    reporter: EntityId,
    message: EntityId,
    room: EntityId,
    reason: Option<&str>,
) -> Result<EntityId, Box<dyn std::error::Error>> {
    try_file_at(ctx, reporter, message, room, reason, 2).await
}

/// The same filing with the timestamp chosen by the caller, for the one rule
/// that reads it.
async fn try_file_at(
    ctx: &Context,
    reporter: EntityId,
    message: EntityId,
    room: EntityId,
    reason: Option<&str>,
    created_at: i64,
) -> Result<EntityId, Box<dyn std::error::Error>> {
    let trx = ctx.begin();
    let id = trx
        .create(&Report {
            reporter: reporter.into(),
            message: message.into(),
            room: room.into(),
            reason: reason.map(str::to_string),
            created_at,
            resolved: false,
            resolved_by: None,
            resolved_at: None,
        })
        .await?
        .id();
    trx.commit().await?;
    Ok(id)
}

/// Let the reactor deliver anything it was going to, so a following "nothing
/// arrived" assertion is not just winning a race.
async fn quiesce() { tokio::time::sleep(Duration::from_millis(400)).await; }

/// The filing half of the feature, end to end: a member's own report lands, and
/// a report naming someone else as the filer does not. The refusal is the whole
/// point of the reporter binding — a queue where anyone can file under anyone's
/// name is a queue that cannot be read as evidence of anything.
#[tokio::test(flavor = "multi_thread")]
async fn a_member_files_reports_as_themselves_and_never_as_anyone_else() {
    let (node, root) = report_node().await;
    let (_author, room, message) = seed_conversation(&root).await;

    let trx = root.begin();
    let reporter = trx.create(&User { display_name: "Reporter".to_string(), oidc_sub: None }).await.unwrap().id();
    let bystander = trx.create(&User { display_name: "Bystander".to_string(), oidc_sub: None }).await.unwrap().id();
    trx.commit().await.unwrap();

    let reporter_ctx = member(&node, reporter);
    try_file(&reporter_ctx, reporter, message, room, Some("off topic")).await.expect("a member may file their own report");

    assert!(
        try_file(&reporter_ctx, bystander, message, room, Some("not mine to file")).await.is_err(),
        "a report naming another member as the reporter must be refused"
    );

    // Exactly the one permitted filing exists, read through Root (which
    // bypasses the scopes) so the count is about what LANDED and not about who
    // may look.
    let rows = root.fetch::<ReportView>("true").await.unwrap();
    assert_eq!(rows.len(), 1, "one report landed, not two");
    assert_eq!(rows[0].reporter().unwrap().id(), reporter);
    assert_eq!(rows[0].message().unwrap().id(), message);
    assert_eq!(rows[0].room().unwrap().id(), room, "the room is copied onto the row at filing time");
    assert_eq!(rows[0].resolved().unwrap(), false, "a report is born open");
}

/// The read shutout, live: a member reads zero report rows — their own filing
/// included — on the one-shot path, through a subscription, and by entity id.
/// A moderator reading the same queue is the control, so "zero" is a denial
/// rather than an empty database.
#[tokio::test(flavor = "multi_thread")]
async fn a_member_reads_zero_reports_including_the_one_they_filed() {
    let (node, root) = report_node().await;
    let (_author, room, message) = seed_conversation(&root).await;

    let trx = root.begin();
    let reporter = trx.create(&User { display_name: "Reporter".to_string(), oidc_sub: None }).await.unwrap().id();
    let mod_user = trx.create(&User { display_name: "Moderator".to_string(), oidc_sub: None }).await.unwrap().id();
    trx.commit().await.unwrap();

    let reporter_ctx = member(&node, reporter);

    // Subscribe BEFORE the report exists, so a row arriving later would arrive
    // as a live delivery rather than an initial resultset — and record every
    // delivery, so "nothing arrived" is a claim about DELIVERY and not merely
    // about the resultset left behind.
    let member_reports = reporter_ctx.query::<ReportView>("true").unwrap();
    member_reports.wait_initialized().await;
    let delivered = Arc::new(Mutex::new(Vec::<EntityId>::new()));
    let sink = delivered.clone();
    let _guard = member_reports.subscribe(move |cs: ankurah::changes::ChangeSet<ReportView>| {
        let mut ids = sink.lock().unwrap();
        for change in &cs.changes {
            ids.push(change.entity().id());
        }
    });

    let report = try_file(&reporter_ctx, reporter, message, room, Some("please look")).await.expect("the filing lands");
    quiesce().await;

    assert!(member_reports.ids().is_empty(), "a member's report query must answer empty, even for their own filing");
    assert!(delivered.lock().unwrap().is_empty(), "no live delivery of any kind, got {:?}", delivered.lock().unwrap());
    assert!(reporter_ctx.fetch::<ReportView>("true").await.unwrap().is_empty(), "fetch must withhold it too");
    assert!(reporter_ctx.get::<ReportView>(report).await.is_err(), "not by id either — and they know the id, they just wrote it");

    // The control: a moderator sees the same row through the same node.
    let mod_ctx = moderator(&node, mod_user);
    let seen = mod_ctx.fetch::<ReportView>("true").await.unwrap();
    assert_eq!(seen.len(), 1, "a moderator reads the queue");
    assert_eq!(seen[0].id(), report);
    assert_eq!(seen[0].reason().unwrap().as_deref(), Some("please look"));
    assert!(mod_ctx.get::<ReportView>(report).await.is_ok(), "and by id");
}

/// Resolution is a moderator act and only a moderator act. The filer cannot
/// close their own report — the open-row write rule refuses the after-state —
/// and a moderator closes a report they did not file, which is the bypass that
/// rule exists to leave open.
#[tokio::test(flavor = "multi_thread")]
async fn only_a_moderator_can_close_a_report() {
    let (node, root) = report_node().await;
    let (_author, room, message) = seed_conversation(&root).await;

    let trx = root.begin();
    let reporter = trx.create(&User { display_name: "Reporter".to_string(), oidc_sub: None }).await.unwrap().id();
    let mod_user = trx.create(&User { display_name: "Moderator".to_string(), oidc_sub: None }).await.unwrap().id();
    trx.commit().await.unwrap();

    let reporter_ctx = member(&node, reporter);
    let report = try_file(&reporter_ctx, reporter, message, room, None).await.expect("the filing lands");

    // The filer, closing their own report. They cannot read the row, so the
    // only handle they have is Root's view of it — which is exactly the shape
    // of a blind write against an id they already know.
    let row = root.get::<ReportView>(report).await.unwrap();
    let filer_closes = async {
        let trx = reporter_ctx.begin();
        row.edit(&trx)?.resolved().set(&true)?;
        trx.commit().await?;
        Ok::<_, Box<dyn std::error::Error>>(())
    }
    .await;
    assert!(filer_closes.is_err(), "the member who filed a report must not be able to close it");
    assert_eq!(root.get::<ReportView>(report).await.unwrap().resolved().unwrap(), false, "and the row is still open");

    // The moderator, closing a report somebody else filed.
    let mod_ctx = moderator(&node, mod_user);
    let row = mod_ctx.get::<ReportView>(report).await.unwrap();
    let trx = mod_ctx.begin();
    let mutable = row.edit(&trx).unwrap();
    mutable.resolved().set(&true).unwrap();
    mutable.resolved_by().set(&Some(mod_user.into())).unwrap();
    mutable.resolved_at().set(&Some(3)).unwrap();
    trx.commit().await.expect("a moderator may resolve a report");

    let closed = root.get::<ReportView>(report).await.unwrap();
    assert_eq!(closed.resolved().unwrap(), true);
    assert_eq!(closed.resolved_by().unwrap().map(|r| r.id()), Some(mod_user), "the closing moderator is stamped on the row");
    assert_eq!(closed.resolved_at().unwrap(), Some(3));
}

/// THE TIMESTAMP THE READ SHUTOUT RESTS ON, enforced rather than assumed.
///
/// The `report` read scope is `created_at < 0` — one comparison no row can
/// satisfy, because every row is stamped from a clock. A member choosing their
/// own `created_at` is the one thing that could make that false, and a report
/// filed at `-1` would then be readable by whoever filed it. The write rule
/// `created_at >= 0` closes that: a filing dated before the epoch is refused,
/// and an ordinary one lands.
#[tokio::test(flavor = "multi_thread")]
async fn a_report_dated_before_the_epoch_is_refused_and_an_ordinary_one_lands() {
    let (node, root) = report_node().await;
    let (_author, room, message) = seed_conversation(&root).await;

    let trx = root.begin();
    let reporter = trx.create(&User { display_name: "Reporter".to_string(), oidc_sub: None }).await.unwrap().id();
    trx.commit().await.unwrap();
    let reporter_ctx = member(&node, reporter);

    assert!(
        try_file_at(&reporter_ctx, reporter, message, room, Some("dated out of the queue"), -1).await.is_err(),
        "a report stamped before the epoch would satisfy the read shutout's comparison, so the write rule refuses it"
    );
    // The exact boundary, on both sides of it, and then the stamp the rest of
    // this file uses as the control that the rule refuses a DATE and not a
    // filing.
    //
    // SMALL STAMPS THROUGHOUT, and not because a real one would be refused —
    // the write rule takes any non-negative number. It is the READ rule that
    // cannot carry one: sled compares `created_at < 0` by parsing that `0` as
    // a 32-bit integer, so a row stamped with a real clock reading answers
    // `TypeMismatch(I32, I64)` instead of comparing. That fails closed on
    // every path (a member's fetch errors rather than returning rows), so the
    // shutout holds either way, and no client opens a member-side `report`
    // query — but it is why the stamps here stay inside 32 bits, and it is
    // pre-existing rather than anything this rule introduced.
    assert!(try_file_at(&reporter_ctx, reporter, message, room, None, 0).await.is_ok(), "the epoch itself is a legal stamp");
    assert!(try_file_at(&reporter_ctx, reporter, message, room, Some("please look"), 2).await.is_ok(), "and so is an ordinary filing");

    let rows = root.fetch::<ReportView>("true").await.unwrap();
    assert_eq!(rows.len(), 2, "the two legal filings landed and the illegal one did not");
    assert!(
        rows.iter().all(|r| r.reason().unwrap().as_deref() != Some("dated out of the queue")),
        "and the refused one is not among them"
    );

    // And the shutout still holds for what did land: the member reads none of
    // it, which is the property the timestamp rule exists to keep true.
    assert!(reporter_ctx.fetch::<ReportView>("true").await.unwrap().is_empty());
}

/// WHAT A REPORTER MAY STILL DO TO A REPORT THEY FILED, pinned deliberately.
///
/// The write rules are predicates over a row's properties, and jwt-auth
/// evaluates them against the PRIOR state as well as the after-state whenever
/// the write is an update (`JwtAgent::check_event`, which runs
/// `enforce_write_scope` on `entity_before` when its head is non-empty). That
/// is what refuses a reopen: a resolved row fails `resolved = false` as a
/// before-state, so the filer cannot flip it back.
///
/// It does NOT refuse a retarget. An open row satisfies every write rule both
/// before and after, so its filer may change which message and which room it
/// names for as long as it stays open. Nothing in ankurah-jwt-auth 0.9.2 can
/// express the rule that would close it: a scope filter names only row
/// properties and `$jwt.*` claims (`variables::resolve_variable`), there is no
/// variable for the prior state and no way to tell an update from an insert —
/// and any predicate a create satisfies is satisfied again by that same row as
/// the before-state of its first update, so no predicate can admit the one and
/// refuse the other.
///
/// SO THE QUEUE READS A REPORT AS THE FILER'S CLAIM about which message is at
/// issue, not as a fact — which is why the moderator queue renders the room
/// from the message it resolves rather than from the row (`reports.rs`). This
/// test exists so that the day the policy language can bind prior state, the
/// change is made deliberately and this assertion is the one that says so.
#[tokio::test(flavor = "multi_thread")]
async fn a_reporter_may_retarget_an_open_report_but_never_reopen_a_closed_one() {
    let (node, root) = report_node().await;
    let (_author, room, message) = seed_conversation(&root).await;
    let (_other_author, other_room, other_message) = seed_conversation(&root).await;

    let trx = root.begin();
    let reporter = trx.create(&User { display_name: "Reporter".to_string(), oidc_sub: None }).await.unwrap().id();
    let mod_user = trx.create(&User { display_name: "Moderator".to_string(), oidc_sub: None }).await.unwrap().id();
    trx.commit().await.unwrap();

    let reporter_ctx = member(&node, reporter);
    let report = try_file(&reporter_ctx, reporter, message, room, Some("please look")).await.expect("the filing lands");

    // The filer cannot read their own row, so the handle is Root's view of it —
    // the same blind-write shape the resolve test uses.
    let row = root.get::<ReportView>(report).await.unwrap();
    let retarget = async {
        let trx = reporter_ctx.begin();
        let mutable = row.edit(&trx)?;
        mutable.message().set(&other_message.into())?;
        mutable.room().set(&other_room.into())?;
        trx.commit().await?;
        Ok::<_, Box<dyn std::error::Error>>(())
    }
    .await;
    assert!(retarget.is_ok(), "PINNED, not endorsed: an open report's target is writable by its filer — see this test's doc");
    let retargeted = root.get::<ReportView>(report).await.unwrap();
    assert_eq!(retargeted.message().unwrap().id(), other_message);
    assert_eq!(retargeted.room().unwrap().id(), other_room);

    // A moderator closes it.
    let mod_ctx = moderator(&node, mod_user);
    let row = mod_ctx.get::<ReportView>(report).await.unwrap();
    let trx = mod_ctx.begin();
    row.edit(&trx).unwrap().resolved().set(&true).unwrap();
    trx.commit().await.expect("a moderator may resolve a report");

    // And the filer cannot open it again, nor touch anything else on it: the
    // before-state fails `resolved = false`, so every write to a closed row is
    // refused rather than only the one that flips the flag.
    let row = root.get::<ReportView>(report).await.unwrap();
    let reopen = async {
        let trx = reporter_ctx.begin();
        row.edit(&trx)?.resolved().set(&false)?;
        trx.commit().await?;
        Ok::<_, Box<dyn std::error::Error>>(())
    }
    .await;
    assert!(reopen.is_err(), "a resolved report must not be reopened by the member who filed it");
    let retarget_closed = async {
        let trx = reporter_ctx.begin();
        row.edit(&trx)?.reason().set(&Some("second thoughts".to_string()))?;
        trx.commit().await?;
        Ok::<_, Box<dyn std::error::Error>>(())
    }
    .await;
    assert!(retarget_closed.is_err(), "and neither may they rewrite what it says");
    let closed = root.get::<ReportView>(report).await.unwrap();
    assert_eq!(closed.resolved().unwrap(), true, "the row is still closed");
    assert_eq!(closed.reason().unwrap().as_deref(), Some("please look"), "and still says what it said");
}

/// A guest reaches the collection at all only through a read, write or retrieve
/// privilege, and the report entry has no retrieve tier — so the gate refuses
/// before any row is considered, on the query path and by id alike. Filing is
/// refused for the same reason a guest posts nothing anywhere: the role holds
/// no `post`.
#[tokio::test(flavor = "multi_thread")]
async fn a_guest_neither_reads_nor_files_reports() {
    let (node, root) = report_node().await;
    let (_author, room, message) = seed_conversation(&root).await;

    let trx = root.begin();
    let reporter = trx.create(&User { display_name: "Reporter".to_string(), oidc_sub: None }).await.unwrap().id();
    trx.commit().await.unwrap();
    let report = try_file(&member(&node, reporter), reporter, message, room, None).await.expect("the filing lands");

    let guest = guest(&node);
    assert!(guest.fetch::<ReportView>("true").await.is_err(), "the report queue is refused at the collection gate");
    assert!(guest.get::<ReportView>(report).await.is_err(), "and knowing the id buys nothing");
    assert!(
        try_file(&guest, reporter, message, room, Some("as nobody")).await.is_err(),
        "a guest holds no write privilege, so there is nothing to file with"
    );
    assert_eq!(root.fetch::<ReportView>("true").await.unwrap().len(), 1, "nothing the guest attempted landed");
}
