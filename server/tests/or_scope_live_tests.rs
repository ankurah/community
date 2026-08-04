//! Spike for ankurah/community#28 (two-party DMs), DM-track gate 2: does a
//! policy read scope shaped `a = $jwt.sub OR b = $jwt.sub` — an OR over two
//! `Ref` fields — deliver LIVE adds and removes through the reactor, or does it
//! only produce correct one-shot fetches?
//!
//! Why the question exists. A two-party DM thread names both participants on
//! the row, so the only read scope that admits both of them is an OR over two
//! `Ref` fields. Two known behaviors meet in exactly that shape and neither is
//! covered anywhere upstream:
//!
//! - The reactor's watcher index collates `Ref` property values as raw EntityId
//!   bytes while string literals collate as text (ankurah#259). ankurah-jwt-auth
//!   works around it by substituting `$jwt.*` claim values that parse as
//!   EntityIds into typed `Literal::EntityId`s, and its own
//!   `scoped_live_update_tests.rs` proves that works — but only for a scope with
//!   a single `=` clause. An OR asks the index to register a watcher per arm.
//! - `Predicate::Or` short-circuits left to right and a comparison against an
//!   absent property errors rather than returning false (pinned in this crate's
//!   policy_scope_tests.rs, against the message write scope). A row that matches
//!   only on the RIGHT arm is therefore the interesting case: the left arm has
//!   to be evaluated and rejected first.
//!
//! What it found, on ankurah 0.9.0 + ankurah-jwt-auth 0.9.0 with the sled
//! backend: the shape works. Both arms deliver live adds, exactly once each; a
//! row leaving the scope produces a Remove; a row that keeps one matching arm
//! stays; and the live and fetch paths agree. One constraint falls out of it,
//! pinned by the last test below: both participant fields must always be
//! present, because an absent LEFT arm errors the whole OR into a silent denial
//! and hides the row from the participant the RIGHT arm names.
//!
//! What this file deliberately does NOT do. There is no `dm_*` production
//! collection here and no edit to policy.json — the models and the policy config
//! below are test-local, because production DM work is gated behind an upstream
//! event-read fix. The models make `a` and `b` mutable (`LWW`) purely to reach
//! the reactor's remove path; production DM threads would never rewrite their
//! participants, and the remove coverage here documents that immutability as a
//! design choice rather than something the machinery depends on.
//!
//! Token fidelity. Contexts are built with `JwtContext::from_claims` and a
//! placeholder token string. On a single local node the token is never verified
//! — it is only serialized as `AuthData` when a request crosses to a peer — and
//! every check under test (`filter_predicate`, `check_read`) reads the claims,
//! not the token. Signing real tokens would mean committing an RSA keypair to
//! this repo for no added coverage of the machinery in question.
//!
//! Sled-gated like model_pin_tests.rs: run with
//! `cargo test -p community-server --no-default-features --features sled`.
//! The default (postgres) test run compiles this file to nothing.
#![cfg(feature = "sled")]

use ankurah::changes::{ChangeKind, ChangeSet};
use ankurah::signals::Subscribe;
use ankurah::{Context, EntityId, LiveQuery, Model, Node, Ref};
use ankurah_jwt_auth::{JwtAgent, JwtClaims, JwtContext, PolicyConfig};
use ankurah_storage_sled::SledStorageEngine;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Stand-in for a `User`. Test-local so the spike never touches the production
/// model crate; only its id matters (it is what `$jwt.sub` carries).
#[derive(Model, Debug, Serialize, Deserialize)]
pub struct SpikeParty {
    pub label: String,
}

/// The two-participant row the spike is about. `a` and `b` are `LWW` so a test
/// can rewrite a participant and exercise the reactor's remove path; see the
/// module docs for why production DM rows would not be mutable.
#[derive(Model, Debug, Serialize, Deserialize)]
pub struct SpikePair {
    #[active_type(LWW)]
    pub a: Ref<SpikeParty>,
    #[active_type(LWW)]
    pub b: Ref<SpikeParty>,
    pub note: String,
}

/// A row on the SAME `spikepair` collection that carries no `a` property at all.
/// The `Model` derive keys the collection on the struct's identifier alone, so a
/// same-named struct in a nested module writes to `spikepair` while omitting the
/// left arm's field — the only way to produce the absent-property shape the OR
/// evaluator's error path needs. Stands in for a row written before a
/// participant field existed.
mod half_written {
    use super::SpikeParty;
    use ankurah::{Model, Ref};
    use serde::{Deserialize, Serialize};

    #[derive(Model, Debug, Serialize, Deserialize)]
    pub struct SpikePair {
        #[active_type(LWW)]
        pub b: Ref<SpikeParty>,
        pub note: String,
    }
}

/// The scope under test. `spikepair`/`spikeparty` are the collection ids the
/// `Model` derive produces (struct name, lowercased).
///
/// The rule gates reads only: writes here all happen under the Root context,
/// which bypasses scopes entirely, and a write scope would add a second failure
/// mode to every assertion about read delivery.
const SPIKE_POLICY_JSON: &str = r#"{
    "roles": {
        "member": ["view", "post"]
    },
    "collections": {
        "spikepair": {
            "read": "view",
            "write": "post",
            "scope": [
                { "filter": "a = $jwt.sub OR b = $jwt.sub", "applies_to": "read" }
            ]
        },
        "spikeparty": {
            "read": "view",
            "write": "post"
        }
    }
}"#;

/// A durable node carrying the spike policy, plus a Root context for setup
/// writes. Root bypasses both the collection gate and the scope, so every row
/// in these tests is created and edited regardless of who may read it.
async fn spike_node() -> (Node<SledStorageEngine, JwtAgent>, Context) {
    let agent = JwtAgent::new_ephemeral();
    agent.update_config(serde_json::from_str::<PolicyConfig>(SPIKE_POLICY_JSON).expect("spike policy parses"));
    let node = Node::new_durable(Arc::new(SledStorageEngine::new_test().unwrap()), agent);
    node.system.wait_loaded().await;
    if node.system.root().is_none() {
        node.system.create().await.unwrap();
    }
    node.system.wait_system_ready().await;
    let root = node.context_async(JwtContext::system()).await;
    (node, root)
}

/// A member context whose `sub` is `party` — the claim the scope substitutes
/// into both arms of the OR.
fn member_context(node: &Node<SledStorageEngine, JwtAgent>, party: EntityId) -> Context {
    let claims = JwtClaims {
        sub: party.to_base64(),
        roles: vec!["member".to_string()],
        email: format!("{}@spike.invalid", party.to_base64()),
        name: None,
        custom: serde_json::Map::new(),
    };
    node.context(JwtContext::from_claims(claims, "spike-placeholder-token".to_string())).expect("member context")
}

async fn create_party(root: &Context, label: &str) -> EntityId {
    let trx = root.begin();
    let id = trx.create(&SpikeParty { label: label.to_string() }).await.unwrap().id();
    trx.commit().await.unwrap();
    id
}

async fn create_pair(root: &Context, a: EntityId, b: EntityId, note: &str) -> EntityId {
    let trx = root.begin();
    let id = trx.create(&SpikePair { a: a.into(), b: b.into(), note: note.to_string() }).await.unwrap().id();
    trx.commit().await.unwrap();
    id
}

/// Commit a `spikepair` row that has a `b` property and no `a` property at all.
async fn create_pair_without_a(root: &Context, b: EntityId, note: &str) -> EntityId {
    let trx = root.begin();
    let id = trx.create(&half_written::SpikePair { b: b.into(), note: note.to_string() }).await.unwrap().id();
    trx.commit().await.unwrap();
    id
}

/// Rewrite one participant of an existing row, under Root.
async fn set_a(root: &Context, pair: EntityId, new_a: EntityId) {
    let trx = root.begin();
    let row = root.get::<SpikePairView>(pair).await.unwrap();
    row.edit(&trx).unwrap().a().set(&new_a.into()).unwrap();
    trx.commit().await.unwrap();
}

async fn set_b(root: &Context, pair: EntityId, new_b: EntityId) {
    let trx = root.begin();
    let row = root.get::<SpikePairView>(pair).await.unwrap();
    row.edit(&trx).unwrap().b().set(&new_b.into()).unwrap();
    trx.commit().await.unwrap();
}

/// Everything the reactor pushed to one subscriber, in arrival order, so a test
/// can assert on the delivery itself and not merely on the resultset it leaves
/// behind. A row that silently appears without an `Add`, or is dropped without a
/// `Remove`, is exactly the defect this spike is looking for.
#[derive(Clone, Default)]
struct Deliveries(Arc<Mutex<Vec<(ChangeKind, EntityId)>>>);

impl Deliveries {
    fn all(&self) -> Vec<(ChangeKind, EntityId)> { self.0.lock().unwrap().clone() }

    /// The kinds delivered for one row, in order.
    fn for_row(&self, id: EntityId) -> Vec<ChangeKind> {
        self.0.lock().unwrap().iter().filter(|(_, row)| *row == id).map(|(kind, _)| kind.clone()).collect()
    }

    fn count(&self, kind: ChangeKind, id: EntityId) -> usize {
        self.0.lock().unwrap().iter().filter(|(k, row)| *k == kind && *row == id).count()
    }
}

/// Attach a recorder to a LiveQuery. The returned guard must stay alive for the
/// duration of the test — dropping it unsubscribes.
fn record(lq: &LiveQuery<SpikePairView>) -> (Deliveries, ankurah::signals::SubscriptionGuard) {
    let log = Deliveries::default();
    let sink = log.clone();
    let guard = lq.subscribe(move |cs: ChangeSet<SpikePairView>| {
        let mut entries = sink.0.lock().unwrap();
        for change in &cs.changes {
            entries.push((change.kind(), change.entity().id()));
        }
    });
    (log, guard)
}

fn sorted(mut ids: Vec<EntityId>) -> Vec<EntityId> {
    ids.sort();
    ids
}

/// Poll until `check` holds, or fail naming what never happened. Delivery is
/// asynchronous, so every positive assertion has to be a wait; the negative
/// assertions (a row must NEVER arrive) are checked after a positive one for the
/// same commit has already landed, which is what makes them meaningful rather
/// than merely early.
async fn eventually(what: &str, check: impl Fn() -> bool) {
    for _ in 0..100 {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for: {what}");
}

/// Give the reactor a beat to deliver anything it was going to deliver, so a
/// following "nothing arrived" assertion is not just winning a race.
async fn quiesce() { tokio::time::sleep(Duration::from_millis(400)).await; }

/// Legs 1 and 4, in one window so the silence claim is literally simultaneous
/// with the delivery claim: A and C both subscribe before any row exists, then
/// three rows are committed. A must receive the row naming it on the LEFT arm
/// and the row naming it on the RIGHT arm; C must receive only its own row, and
/// neither subscriber may ever see the other's.
///
/// Fetch parity is asserted at the end: the same scope evaluated on the one-shot
/// path must produce the same two rows, so a live/fetch divergence names itself.
#[tokio::test(flavor = "multi_thread")]
async fn both_arms_deliver_live_and_non_participants_stay_silent() {
    let (node, root) = spike_node().await;
    let (a, b, c, d) = (
        create_party(&root, "A").await,
        create_party(&root, "B").await,
        create_party(&root, "C").await,
        create_party(&root, "D").await,
    );

    let a_ctx = member_context(&node, a);
    let c_ctx = member_context(&node, c);
    let a_lq = a_ctx.query::<SpikePairView>("true").unwrap();
    let c_lq = c_ctx.query::<SpikePairView>("true").unwrap();
    a_lq.wait_initialized().await;
    c_lq.wait_initialized().await;
    assert_eq!(a_lq.ids().len(), 0, "no rows exist yet");
    assert_eq!(c_lq.ids().len(), 0, "no rows exist yet");

    let (a_log, _a_guard) = record(&a_lq);
    let (c_log, _c_guard) = record(&c_lq);

    // All three committed after both subscriptions were established.
    let left = create_pair(&root, a, b, "A on the left arm").await;
    let right = create_pair(&root, b, a, "A on the right arm").await;
    let neither = create_pair(&root, c, d, "A in neither arm").await;

    eventually("A to receive both of its rows", || sorted(a_lq.ids()) == sorted(vec![left, right])).await;
    eventually("C to receive its own row", || c_lq.ids() == vec![neither]).await;
    quiesce().await;

    assert_eq!(sorted(a_lq.ids()), sorted(vec![left, right]), "A sees exactly the two rows naming it, and never (C,D)");
    assert_eq!(c_lq.ids(), vec![neither], "C sees exactly its own row");

    assert_eq!(a_log.for_row(left), vec![ChangeKind::Add], "the left-arm row must arrive as a single live Add");
    assert_eq!(a_log.for_row(right), vec![ChangeKind::Add], "the right-arm row must arrive as a single live Add");
    assert!(a_log.for_row(neither).is_empty(), "A must receive no delivery of any kind for (C,D), got {:?}", a_log.for_row(neither));
    assert!(
        c_log.for_row(left).is_empty() && c_log.for_row(right).is_empty(),
        "C must receive no delivery for A's rows, got {:?}",
        c_log.all()
    );

    // Fetch-path parity: the one-shot path applies the same scope.
    let fetched: Vec<EntityId> = a_ctx.fetch::<SpikePairView>("true").await.unwrap().iter().map(|v| v.id()).collect();
    assert_eq!(sorted(fetched), sorted(vec![left, right]), "one-shot fetch must return the same two rows the LiveQuery holds");
    let c_fetched: Vec<EntityId> = c_ctx.fetch::<SpikePairView>("true").await.unwrap().iter().map(|v| v.id()).collect();
    assert_eq!(c_fetched, vec![neither], "C's one-shot fetch must return only its own row");
}

/// Leg 2, isolated so a failure names itself rather than hiding inside the
/// broader window above: a row whose LEFT arm mismatches must still be evaluated
/// against the RIGHT arm on the LIVE path. This is where an OR that
/// short-circuits on the left, or a watcher index that only registers the first
/// arm, would swallow the delivery — and it is the shape every DM a user did not
/// initiate would take.
#[tokio::test(flavor = "multi_thread")]
async fn right_arm_only_row_arrives_live() {
    let (node, root) = spike_node().await;
    let a = create_party(&root, "A").await;
    let b = create_party(&root, "B").await;

    let a_ctx = member_context(&node, a);
    let lq = a_ctx.query::<SpikePairView>("true").unwrap();
    lq.wait_initialized().await;
    let (log, _guard) = record(&lq);

    // A appears ONLY as `b`. Nothing else is committed, so nothing can mask it.
    let right = create_pair(&root, b, a, "A only on the right arm").await;

    eventually("the right-arm row to be delivered live", || lq.ids() == vec![right]).await;
    assert_eq!(log.for_row(right), vec![ChangeKind::Add], "right-arm-only row must arrive as exactly one Add");
}

/// Leg 3: the reactor's remove path under an OR. The row starts matching on the
/// left arm only; rewriting `a` to a third party leaves neither arm matching, so
/// the subscriber must be told the row left its scope — silently dropping it
/// from the resultset (or leaving it there) both leak.
#[tokio::test(flavor = "multi_thread")]
async fn remove_delivered_when_the_last_matching_arm_flips_away() {
    let (node, root) = spike_node().await;
    let a = create_party(&root, "A").await;
    let b = create_party(&root, "B").await;
    let c = create_party(&root, "C").await;

    let a_ctx = member_context(&node, a);
    let lq = a_ctx.query::<SpikePairView>("true").unwrap();
    lq.wait_initialized().await;
    let (log, _guard) = record(&lq);

    let pair = create_pair(&root, a, b, "starts as A's").await;
    eventually("the row to arrive", || lq.ids() == vec![pair]).await;

    set_a(&root, pair, c).await;

    eventually("the row to be removed from the resultset", || lq.ids().is_empty()).await;
    assert_eq!(
        log.for_row(pair),
        vec![ChangeKind::Add, ChangeKind::Remove],
        "the row must arrive as an Add and leave as a Remove, with no spurious deliveries in between"
    );
}

/// Leg 3, the OR-specific hazard: a row matching on BOTH arms that loses one of
/// them must stay. If the reactor keyed membership on whichever arm's watcher
/// fired, the surviving arm would be ignored and a live DM would vanish from the
/// participant's list mid-conversation.
#[tokio::test(flavor = "multi_thread")]
async fn row_stays_when_only_one_of_two_matching_arms_flips_away() {
    let (node, root) = spike_node().await;
    let a = create_party(&root, "A").await;
    let c = create_party(&root, "C").await;

    let a_ctx = member_context(&node, a);
    let lq = a_ctx.query::<SpikePairView>("true").unwrap();
    lq.wait_initialized().await;
    let (log, _guard) = record(&lq);

    // Both arms name A — a self-thread, and the cheapest way to hold two
    // matching arms at once.
    let pair = create_pair(&root, a, a, "both arms are A").await;
    eventually("the row to arrive", || lq.ids() == vec![pair]).await;

    // Left arm flips away; the right arm still names A.
    set_a(&root, pair, c).await;
    quiesce().await;

    assert_eq!(lq.ids(), vec![pair], "the row must stay: the right arm still matches");
    assert_eq!(log.count(ChangeKind::Remove, pair), 0, "no Remove may be delivered while an arm still matches, got {:?}", log.for_row(pair));
}

/// Leg 3's mirror: an existing row that did NOT match becomes a match when its
/// right arm is rewritten to the subscriber. This is the live-add-by-mutation
/// path (as opposed to add-by-creation, covered above), and the only way a
/// participant could be added to an existing thread.
#[tokio::test(flavor = "multi_thread")]
async fn mutating_the_right_arm_toward_the_subscriber_adds_the_row_live() {
    let (node, root) = spike_node().await;
    let a = create_party(&root, "A").await;
    let c = create_party(&root, "C").await;
    let d = create_party(&root, "D").await;

    let a_ctx = member_context(&node, a);
    let lq = a_ctx.query::<SpikePairView>("true").unwrap();
    lq.wait_initialized().await;
    let (log, _guard) = record(&lq);

    let pair = create_pair(&root, c, d, "nothing to do with A").await;
    quiesce().await;
    assert!(lq.ids().is_empty(), "the row must not be visible before it names A");
    assert!(log.for_row(pair).is_empty(), "no delivery for a row outside the scope, got {:?}", log.for_row(pair));

    set_b(&root, pair, a).await;

    eventually("the row to become visible once its right arm names A", || lq.ids() == vec![pair]).await;
    assert_eq!(log.for_row(pair), vec![ChangeKind::Add], "the row must arrive as an Add when the right arm starts matching");
}

/// The constraint this shape imposes on any model that adopts it, pinned so it
/// cannot be discovered the hard way: a row missing the LEFT arm's property is
/// invisible to BOTH participants — including the one the RIGHT arm names.
///
/// `Predicate::Or` propagates an evaluator error from its left operand before it
/// ever reaches the right one (`evaluate_predicate(left)? || …`), and comparing
/// against a property the entity does not have is an error, not `false`. The
/// error becomes a denial, so the surviving right arm is never consulted. Both
/// the live path and the fetch path drop the row silently — nothing is
/// delivered, and no error surfaces to the subscriber.
///
/// A model using this scope must therefore declare both participant fields as
/// plain `Ref<_>` set at creation, never `Option<Ref<_>>` and never added to the
/// collection later: either would make existing rows vanish for everyone.
#[tokio::test(flavor = "multi_thread")]
async fn a_row_missing_the_left_arm_is_invisible_even_to_the_right_arms_participant() {
    let (node, root) = spike_node().await;
    let a = create_party(&root, "A").await;

    let a_ctx = member_context(&node, a);
    let lq = a_ctx.query::<SpikePairView>("true").unwrap();
    lq.wait_initialized().await;
    let (log, _guard) = record(&lq);

    // `b` names A, so on a `false`-returning evaluator this row would match.
    let pair = create_pair_without_a(&root, a, "no `a` property at all").await;
    quiesce().await;

    // Control: the row really is on `spikepair` and really does name A in `b`.
    // Without this, an empty resultset below would prove nothing.
    let seen_by_root = root.fetch::<half_written::SpikePairView>("true").await.unwrap();
    assert_eq!(seen_by_root.iter().map(|v| v.id()).collect::<Vec<_>>(), vec![pair], "the row exists on `spikepair` (Root bypasses the scope)");
    assert_eq!(seen_by_root[0].b().unwrap().id(), a, "and its `b` names A");

    assert!(lq.ids().is_empty(), "the row is withheld from A on the live path despite `b` naming A");
    assert!(log.all().is_empty(), "and nothing at all is delivered — the denial is silent, got {:?}", log.all());
    let fetched: Vec<EntityId> = a_ctx.fetch::<SpikePairView>("true").await.unwrap().iter().map(|v| v.id()).collect();
    assert!(fetched.is_empty(), "the fetch path withholds it too, got {fetched:?}");
}
