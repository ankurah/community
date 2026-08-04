//! Adversarial coverage for the DM read/write scopes (#30), against the REAL
//! `policy.json` and the REAL `dm_*` models on a real (sled) node.
//!
//! `policy_scope_tests.rs` pins what the evaluator does with a hand-built row;
//! this file pins what an actual signed-in context can and cannot reach, which
//! is the claim the feature rests on. Concretely, for a signed-in
//! non-participant:
//!
//! - a one-shot `fetch` of `dm_thread` / `dm_message` returns nothing;
//! - a `LiveQuery` receives nothing, ever, including for rows committed while
//!   it was already subscribed;
//! - writes into someone else's thread are refused, and so is opening a thread
//!   between two other people.
//!
//! And, because DMs are private from moderators (the community#30 ruling): a
//! moderator who is not a participant is exactly as blind as anyone else. That
//! test is the one that fails the day somebody adds `unless_privilege:
//! "moderate"` to a dm read rule.
//!
//! WHAT THIS FILE CANNOT COVER, AND WHERE THAT IS TRACKED. The DM feature's
//! third read path is EVENTS, and `JwtAgent::check_read_event` (jwt-auth 0.9.0)
//! authorizes event reads at COLLECTION level only — it never evaluates the
//! per-entity read scope that `check_read` applies to state. A non-participant
//! who learns a `dm_message` event id can therefore still fetch that event and
//! reconstruct the text. The fix is upstream (ankurah#438) and its acceptance
//! is community#68, the release gate DMs ship dark behind; it is not reachable
//! from this harness anyway, because `check_read_event` only runs when a
//! request crosses between peers and these tests run on a single node. Do not
//! read a green run here as "DM content is protected on every path" — read it
//! as "state and live delivery are protected; events are #68's job".
//!
//! Sled-gated like `model_pin_tests.rs` and the spike: run with
//! `cargo test -p community-server --no-default-features --features sled`.
#![cfg(feature = "sled")]

use ankurah::changes::{ChangeKind, ChangeSet};
use ankurah::signals::Subscribe;
use ankurah::{Context, EntityId, LiveQuery, Node};
use ankurah_jwt_auth::{JwtAgent, JwtClaims, JwtContext, PolicyConfig};
use ankurah_storage_sled::SledStorageEngine;
use community_model::{
    canonical_pair, DmMessage, DmMessageView, DmReadState, DmReadStateView, DmThread, DmThreadView, User,
};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The shipped policy, verbatim — the point of this harness is that nothing
/// here restates a rule that production could then drift away from.
const POLICY_JSON: &str = include_str!("../../policy.json");

/// A durable node carrying the real policy, plus a Root context for setup
/// writes (Root bypasses both the collection gate and the row scopes, which is
/// how the server's own workers reach these rows).
async fn dm_node() -> (Node<SledStorageEngine, JwtAgent>, Context) {
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
/// the spike's rationale: on a single local node the token string is never
/// verified, and every check under test reads the claims.
fn context_for(node: &Node<SledStorageEngine, JwtAgent>, user: EntityId, roles: &[&str]) -> Context {
    let claims = JwtClaims {
        sub: user.to_base64(),
        roles: roles.iter().map(|r| r.to_string()).collect(),
        email: format!("{}@dm.invalid", user.to_base64()),
        name: None,
        custom: serde_json::Map::new(),
    };
    node.context(JwtContext::from_claims(claims, "dm-placeholder-token".to_string())).expect("context")
}

fn member(node: &Node<SledStorageEngine, JwtAgent>, user: EntityId) -> Context { context_for(node, user, &["member"]) }

/// A moderator context: `member` plus `moderate`, exactly what
/// `server::resolve_roles` mints for an idp.to moderator.
fn moderator(node: &Node<SledStorageEngine, JwtAgent>, user: EntityId) -> Context {
    context_for(node, user, &["member", "moderator"])
}

async fn create_user(root: &Context, name: &str) -> EntityId {
    let trx = root.begin();
    let id = trx.create(&User { display_name: name.to_string(), oidc_sub: None }).await.unwrap().id();
    trx.commit().await.unwrap();
    id
}

/// A thread between two users, created under Root the way the tests' setup
/// needs it (the client path creates it under the participant's own context —
/// that path is exercised by `a_participant_can_open_their_own_thread` below).
async fn create_thread(root: &Context, x: EntityId, y: EntityId) -> (EntityId, EntityId, EntityId) {
    let (a, b) = canonical_pair(x, y);
    let trx = root.begin();
    let id = trx.create(&DmThread { a: a.into(), b: b.into(), created_at: 1, deleted: false }).await.unwrap().id();
    trx.commit().await.unwrap();
    (id, a, b)
}

async fn create_message(root: &Context, thread: EntityId, a: EntityId, b: EntityId, sender: EntityId, text: &str) -> EntityId {
    let trx = root.begin();
    let id = trx
        .create(&DmMessage {
            thread: thread.into(),
            a: a.into(),
            b: b.into(),
            user: sender.into(),
            text: text.to_string(),
            timestamp: 2,
            deleted: false,
            edited_at: None,
        })
        .await
        .unwrap()
        .id();
    trx.commit().await.unwrap();
    id
}

/// Everything the reactor pushed to one subscriber, so "nothing arrived" is a
/// claim about DELIVERY and not merely about the resultset left behind.
#[derive(Clone, Default)]
struct Deliveries(Arc<Mutex<Vec<(ChangeKind, EntityId)>>>);

impl Deliveries {
    fn all(&self) -> Vec<(ChangeKind, EntityId)> { self.0.lock().unwrap().clone() }
}

fn record<V: ankurah::View + Clone + Send + Sync + 'static>(lq: &LiveQuery<V>) -> (Deliveries, ankurah::signals::SubscriptionGuard) {
    let log = Deliveries::default();
    let sink = log.clone();
    let guard = lq.subscribe(move |cs: ChangeSet<V>| {
        let mut entries = sink.0.lock().unwrap();
        for change in &cs.changes {
            entries.push((change.kind(), change.entity().id()));
        }
    });
    (log, guard)
}

async fn eventually(what: &str, check: impl Fn() -> bool) {
    for _ in 0..100 {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for: {what}");
}

/// Let the reactor deliver anything it was going to, so a following "nothing
/// arrived" assertion is not just winning a race.
async fn quiesce() { tokio::time::sleep(Duration::from_millis(400)).await; }

/// The whole feature in one window: both participants receive the thread and
/// its messages live and on fetch, and a signed-in third party receives
/// neither — not in the resultset, and not as a delivery of any kind.
#[tokio::test(flavor = "multi_thread")]
async fn participants_read_their_thread_live_and_strangers_read_nothing() {
    let (node, root) = dm_node().await;
    let alice = create_user(&root, "Alice").await;
    let bob = create_user(&root, "Bob").await;
    let carol = create_user(&root, "Carol").await;

    let alice_ctx = member(&node, alice);
    let bob_ctx = member(&node, bob);
    let carol_ctx = member(&node, carol);

    // Subscribe BEFORE anything exists, so every row below arrives as a live
    // delivery rather than an initial resultset.
    let alice_threads = alice_ctx.query::<DmThreadView>("true").unwrap();
    let bob_threads = bob_ctx.query::<DmThreadView>("true").unwrap();
    let carol_threads = carol_ctx.query::<DmThreadView>("true").unwrap();
    let carol_messages = carol_ctx.query::<DmMessageView>("true").unwrap();
    for lq in [&alice_threads, &bob_threads, &carol_threads] {
        lq.wait_initialized().await;
    }
    carol_messages.wait_initialized().await;
    let (carol_thread_log, _g1) = record(&carol_threads);
    let (carol_message_log, _g2) = record(&carol_messages);

    let (thread, a, b) = create_thread(&root, alice, bob).await;
    let message = create_message(&root, thread, a, b, alice, "the quiet part").await;

    eventually("Alice to receive the thread", || alice_threads.ids() == vec![thread]).await;
    eventually("Bob to receive the thread", || bob_threads.ids() == vec![thread]).await;
    quiesce().await;

    // Carol: nothing in either resultset, and nothing ever delivered.
    assert!(carol_threads.ids().is_empty(), "a non-participant must not see the thread");
    assert!(carol_messages.ids().is_empty(), "a non-participant must not see the messages");
    assert!(carol_thread_log.all().is_empty(), "no thread delivery of any kind, got {:?}", carol_thread_log.all());
    assert!(carol_message_log.all().is_empty(), "no message delivery of any kind, got {:?}", carol_message_log.all());

    // The one-shot path agrees with the live path on all three viewers.
    let ids = |rows: Vec<DmThreadView>| rows.iter().map(|r| r.id()).collect::<Vec<_>>();
    assert_eq!(ids(alice_ctx.fetch::<DmThreadView>("true").await.unwrap()), vec![thread]);
    assert_eq!(ids(bob_ctx.fetch::<DmThreadView>("true").await.unwrap()), vec![thread]);
    assert!(carol_ctx.fetch::<DmThreadView>("true").await.unwrap().is_empty(), "fetch must withhold it too");
    assert!(carol_ctx.fetch::<DmMessageView>("true").await.unwrap().is_empty(), "fetch must withhold the messages too");

    // Both participants can read the message body; the render key is `thread`.
    for (who, ctx) in [("Alice", &alice_ctx), ("Bob", &bob_ctx)] {
        let rows = ctx.fetch::<DmMessageView>("true").await.unwrap();
        assert_eq!(rows.len(), 1, "{who} sees exactly the one message");
        assert_eq!(rows[0].id(), message);
        assert_eq!(rows[0].thread().unwrap().id(), thread);
        assert_eq!(rows[0].text().unwrap(), "the quiet part");
    }

    // Carol cannot reach the row by id either — the scope is per-entity, not
    // per-query, so guessing the id buys nothing on the state path.
    assert!(carol_ctx.get::<DmMessageView>(message).await.is_err(), "a direct get by id must be refused too");
}

/// The community#30 ruling, made executable: a moderator who is not in the
/// thread reads nothing. This test fails the moment someone adds
/// `unless_privilege: "moderate"` to a dm read rule — which is exactly the
/// one-line change that would flip the product posture, so it should be a
/// deliberate act with a test edit attached, not a quiet policy tweak.
#[tokio::test(flavor = "multi_thread")]
async fn a_moderator_who_is_not_a_participant_is_denied_dm_reads() {
    let (node, root) = dm_node().await;
    let alice = create_user(&root, "Alice").await;
    let bob = create_user(&root, "Bob").await;
    let mallory = create_user(&root, "Mallory the moderator").await;

    let mod_ctx = moderator(&node, mallory);
    let mod_threads = mod_ctx.query::<DmThreadView>("true").unwrap();
    let mod_messages = mod_ctx.query::<DmMessageView>("true").unwrap();
    mod_threads.wait_initialized().await;
    mod_messages.wait_initialized().await;
    let (thread_log, _g1) = record(&mod_threads);
    let (message_log, _g2) = record(&mod_messages);

    let (thread, a, b) = create_thread(&root, alice, bob).await;
    let message = create_message(&root, thread, a, b, alice, "not for the mod log").await;

    // A participant really does receive it, so an empty moderator resultset is
    // a denial and not an empty database.
    let alice_threads = member(&node, alice).query::<DmThreadView>("true").unwrap();
    eventually("Alice to receive the thread", || alice_threads.ids() == vec![thread]).await;
    quiesce().await;

    assert!(mod_threads.ids().is_empty(), "moderators do not browse DM threads");
    assert!(mod_messages.ids().is_empty(), "moderators do not browse DM messages");
    assert!(thread_log.all().is_empty(), "no live delivery to a moderator, got {:?}", thread_log.all());
    assert!(message_log.all().is_empty(), "no live delivery to a moderator, got {:?}", message_log.all());
    assert!(mod_ctx.fetch::<DmThreadView>("true").await.unwrap().is_empty());
    assert!(mod_ctx.fetch::<DmMessageView>("true").await.unwrap().is_empty());
    assert!(mod_ctx.get::<DmMessageView>(message).await.is_err(), "not even by id");
    // Abuse response goes through reports that carry the message ref, and
    // through the existing Ban/ModAction machinery — see docs/moderation.md.
}

/// The client's own create path, under a participant's context rather than
/// Root: opening a thread you are in, and posting into it as yourself, both
/// pass the write scope.
#[tokio::test(flavor = "multi_thread")]
async fn a_participant_can_open_their_own_thread_and_post_in_it() {
    let (node, root) = dm_node().await;
    let alice = create_user(&root, "Alice").await;
    let bob = create_user(&root, "Bob").await;
    let alice_ctx = member(&node, alice);
    let (a, b) = canonical_pair(alice, bob);

    let trx = alice_ctx.begin();
    let thread = trx.create(&DmThread { a: a.into(), b: b.into(), created_at: 1, deleted: false }).await.unwrap().id();
    trx.commit().await.expect("a participant may open their own thread");

    let trx = alice_ctx.begin();
    trx.create(&DmMessage {
        thread: thread.into(),
        a: a.into(),
        b: b.into(),
        user: alice.into(),
        text: "hi".into(),
        timestamp: 2,
        deleted: false,
        edited_at: None,
    })
    .await
    .unwrap();
    trx.commit().await.expect("a participant may post as themselves");

    // And the other participant receives it.
    let bob_ctx = member(&node, bob);
    let bob_messages = bob_ctx.query::<DmMessageView>("true").unwrap();
    eventually("Bob to receive Alice's message", || bob_messages.ids().len() == 1).await;
}

/// Create one `DmMessage` under `ctx` and report whether the write survived.
/// The scope denial can surface at either `create` or `commit` (jwt-auth
/// evaluates the write scope against the post-write entity, and which of the
/// two calls reaches that check is an internal detail), so the whole attempt is
/// one Result — a test asserting on only one of them would pass for the wrong
/// reason if that detail ever moved.
async fn try_send(
    ctx: &Context,
    thread: EntityId,
    a: EntityId,
    b: EntityId,
    sender: EntityId,
    text: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let trx = ctx.begin();
    trx.create(&DmMessage {
        thread: thread.into(),
        a: a.into(),
        b: b.into(),
        user: sender.into(),
        text: text.to_string(),
        timestamp: 3,
        deleted: false,
        edited_at: None,
    })
    .await?;
    trx.commit().await?;
    Ok(())
}

/// The write scope's three refusals, in one place:
/// 1. a stranger cannot open a thread between two other people;
/// 2. a stranger cannot post into a thread they are not in;
/// 3. a participant cannot attribute a message in their OWN thread to the
///    other person (the sender-binding rule).
#[tokio::test(flavor = "multi_thread")]
async fn dm_writes_are_refused_for_strangers_and_for_mis_attribution() {
    let (node, root) = dm_node().await;
    let alice = create_user(&root, "Alice").await;
    let bob = create_user(&root, "Bob").await;
    let carol = create_user(&root, "Carol").await;
    let (thread, a, b) = create_thread(&root, alice, bob).await;

    let carol_ctx = member(&node, carol);
    let alice_ctx = member(&node, alice);

    // 1. Carol fabricating a thread between Alice and Bob.
    let (fab_a, fab_b) = canonical_pair(alice, bob);
    let opened = async {
        let trx = carol_ctx.begin();
        trx.create(&DmThread { a: fab_a.into(), b: fab_b.into(), created_at: 1, deleted: false }).await?;
        trx.commit().await?;
        Ok::<_, Box<dyn std::error::Error>>(())
    }
    .await;
    assert!(opened.is_err(), "nobody may open a thread between two OTHER people");

    // 2. Carol posting into Alice and Bob's thread. She has to name the pair on
    //    the row for it to be readable at all, and naming a pair she is not in
    //    is precisely what the scope refuses.
    assert!(
        try_send(&carol_ctx, thread, a, b, carol, "intruding").await.is_err(),
        "a non-participant may not post into someone else's thread"
    );

    // 3. Alice putting words in Bob's mouth, inside her own thread.
    assert!(
        try_send(&alice_ctx, thread, a, b, bob, "I, Bob, agree").await.is_err(),
        "the sender-binding rule must refuse a mis-attributed message"
    );

    // The control: the same call shape from the right person does land, so the
    // three refusals above are the scope talking and not a broken harness.
    try_send(&alice_ctx, thread, a, b, alice, "and this one is mine").await.expect("Alice may post as herself");

    quiesce().await;
    let landed = alice_ctx.fetch::<DmMessageView>("true").await.unwrap();
    assert_eq!(landed.len(), 1, "exactly the one permitted message landed");
    assert_eq!(landed[0].user().unwrap().id(), alice);
}

/// Read cursors are private to their owner, not shared with the correspondent
/// — the deliberate asymmetry documented on `DmReadState`. Bob is Alice's DM
/// partner and still cannot see (or write) when she last read the thread.
#[tokio::test(flavor = "multi_thread")]
async fn dm_read_cursors_are_private_even_from_the_other_participant() {
    let (node, root) = dm_node().await;
    let alice = create_user(&root, "Alice").await;
    let bob = create_user(&root, "Bob").await;
    let (thread, _a, _b) = create_thread(&root, alice, bob).await;

    let alice_ctx = member(&node, alice);
    let bob_ctx = member(&node, bob);

    let trx = alice_ctx.begin();
    let cursor = trx.create(&DmReadState { user: alice.into(), thread: thread.into(), last_read_ts: 7 }).await.unwrap().id();
    trx.commit().await.expect("a user may write their own read cursor");

    let bob_cursors = bob_ctx.query::<DmReadStateView>("true").unwrap();
    bob_cursors.wait_initialized().await;
    quiesce().await;
    assert!(bob_cursors.ids().is_empty(), "a DM read cursor is a read receipt — the correspondent must not see it");
    assert!(bob_ctx.fetch::<DmReadStateView>("true").await.unwrap().is_empty());
    assert!(bob_ctx.get::<DmReadStateView>(cursor).await.is_err());

    // And Alice sees her own.
    assert_eq!(alice_ctx.fetch::<DmReadStateView>("true").await.unwrap().len(), 1);
}

/// The concurrent first-DM race, at the storage level: Alice and Bob (or two
/// tabs of one user) both find no thread for the pair and both create one.
/// There is no entity deletion in ankurah 0.9.0, so both rows are permanent —
/// what has to hold is that every reader independently agrees on WHICH of them
/// is the thread, so the conversation does not silently fork.
///
/// The agreement rule is `community_model::canonical_thread` (lowest entity
/// id), called here exactly as the client calls it — a test with its own copy
/// of the rule would prove nothing about convergence.
#[tokio::test(flavor = "multi_thread")]
async fn a_concurrent_first_dm_race_converges_on_one_thread() {
    use community_model::canonical_thread;

    let (node, root) = dm_node().await;
    let alice = create_user(&root, "Alice").await;
    let bob = create_user(&root, "Bob").await;
    let (a, b) = canonical_pair(alice, bob);

    let alice_ctx = member(&node, alice);
    let bob_ctx = member(&node, bob);

    // Both sides commit their own thread for the same pair, neither having seen
    // the other's. Two separate contexts, so this is the real two-client shape
    // and not one client writing twice.
    let open = |ctx: Context| async move {
        let trx = ctx.begin();
        let id = trx.create(&DmThread { a: a.into(), b: b.into(), created_at: 1, deleted: false }).await.unwrap().id();
        trx.commit().await.unwrap();
        id
    };
    let (first, second) = tokio::join!(open(alice_ctx.clone()), open(bob_ctx.clone()));
    assert_ne!(first, second, "the race really did produce two rows — otherwise this test proves nothing");

    // Both participants converge on the same resultset...
    let alice_threads = alice_ctx.query::<DmThreadView>("true").unwrap();
    let bob_threads = bob_ctx.query::<DmThreadView>("true").unwrap();
    eventually("Alice to see both racing threads", || alice_threads.ids().len() == 2).await;
    eventually("Bob to see both racing threads", || bob_threads.ids().len() == 2).await;

    // ...and on the same choice within it.
    let alice_choice = canonical_thread(alice_threads.ids());
    let bob_choice = canonical_thread(bob_threads.ids());
    assert_eq!(alice_choice, bob_choice, "both clients must pick the same thread out of the duplicate pair");
    assert_eq!(alice_choice, Some(std::cmp::min(first, second)), "the choice is the lowest entity id");

    // And a message posted into the chosen thread is what both sides read.
    let chosen = alice_choice.unwrap();
    try_send(&alice_ctx, chosen, a, b, alice, "converged").await.expect("post into the chosen thread");
    let bob_messages = bob_ctx.query::<DmMessageView>("true").unwrap();
    eventually("Bob to receive the message in the chosen thread", || bob_messages.ids().len() == 1).await;
    let row = bob_ctx.fetch::<DmMessageView>("true").await.unwrap().into_iter().next().unwrap();
    assert_eq!(row.thread().unwrap().id(), chosen);
}
