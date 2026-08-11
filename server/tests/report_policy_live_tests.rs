//! What the report queue actually admits, against the REAL `policy.json` and
//! the REAL `Report` model on a real (sled) node.
//!
//! `policy_scope_tests.rs` pins what the evaluator does with a hand-built row;
//! this pins what a signed-in context can and cannot reach, which is the claim
//! the feature rests on. Concretely:
//!
//! - a member files a report naming themselves, and that write lands;
//! - a member filing one that names ANOTHER member as the reporter is refused;
//! - a member reads zero report rows — empty on fetch, empty on a live query,
//!   refused by entity id — including the report they filed themselves;
//! - a member cannot close a report, their own included;
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
    let trx = ctx.begin();
    let id = trx
        .create(&Report {
            reporter: reporter.into(),
            message: message.into(),
            room: room.into(),
            reason: reason.map(str::to_string),
            created_at: 2,
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
