//! End-to-end policy coverage for the self-scoped PushDevice collection.
//!
//! Registration is a normal Ankurah write from the phone's ephemeral node,
//! not a privileged HTTP RPC. That makes the policy agent the privacy and
//! ownership boundary: a member may create, read, and edit rows naming their
//! own User id, while another member's row is invisible and immutable even to
//! a moderator. Guests cannot reach the collection at all.
//!
//! Sled-gated like the other live-node policy suites: run with
//! `cargo test -p community-server --no-default-features --features sled`.
#![cfg(feature = "sled")]

use ankurah::{Context, EntityId, Node};
use ankurah_jwt_auth::{JwtAgent, JwtClaims, JwtContext, PolicyConfig};
use ankurah_storage_sled::SledStorageEngine;
use community_model::{PushDevice, PushDeviceView, User};
use std::sync::Arc;

const POLICY_JSON: &str = include_str!("../../policy.json");
const ALICE_TOKEN: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const BOB_TOKEN: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

async fn push_node() -> (Node<SledStorageEngine, JwtAgent>, Context) {
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

fn context_for(node: &Node<SledStorageEngine, JwtAgent>, user: EntityId, roles: &[&str]) -> Context {
    let claims = JwtClaims {
        sub: user.to_base64(),
        roles: roles.iter().map(|role| role.to_string()).collect(),
        email: format!("{}@push-tests.invalid", user.to_base64()),
        name: None,
        custom: serde_json::Map::new(),
    };
    node.context(JwtContext::from_claims(claims, "push-placeholder-token".to_string())).expect("context")
}

fn member(node: &Node<SledStorageEngine, JwtAgent>, user: EntityId) -> Context { context_for(node, user, &["member"]) }

fn moderator(node: &Node<SledStorageEngine, JwtAgent>, user: EntityId) -> Context {
    context_for(node, user, &["member", "moderator"])
}

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

async fn create_user(root: &Context, name: &str) -> EntityId {
    let trx = root.begin();
    let id = trx.create(&User { display_name: name.to_string(), oidc_sub: None }).await.unwrap().id();
    trx.commit().await.unwrap();
    id
}

async fn try_register(ctx: &Context, user: EntityId, token: &str) -> Result<EntityId, Box<dyn std::error::Error>> {
    let trx = ctx.begin();
    let id = trx
        .create(&PushDevice {
            user: user.into(),
            token: token.to_string(),
            platform: "ios".to_string(),
            last_registered_at: 1,
            active: true,
        })
        .await?
        .id();
    trx.commit().await?;
    Ok(id)
}

#[tokio::test(flavor = "multi_thread")]
async fn members_create_edit_and_read_only_their_own_devices() {
    let (node, root) = push_node().await;
    let alice = create_user(&root, "Alice").await;
    let bob = create_user(&root, "Bob").await;
    let alice_ctx = member(&node, alice);
    let bob_ctx = member(&node, bob);

    let alice_device = try_register(&alice_ctx, alice, ALICE_TOKEN).await.expect("Alice registers her own phone");
    let bob_device = try_register(&bob_ctx, bob, BOB_TOKEN).await.expect("Bob registers his own phone");

    let alice_rows = alice_ctx.fetch::<PushDeviceView>("true").await.unwrap();
    assert_eq!(alice_rows.len(), 1);
    assert_eq!(alice_rows[0].id(), alice_device);
    assert_eq!(alice_rows[0].token().unwrap(), ALICE_TOKEN);
    assert!(alice_ctx.get::<PushDeviceView>(bob_device).await.is_err(), "knowing Bob's row id does not reveal his token");

    let row = alice_ctx.get::<PushDeviceView>(alice_device).await.unwrap();
    let trx = alice_ctx.begin();
    row.edit(&trx).unwrap().active().set(&false).unwrap();
    trx.commit().await.unwrap();
    assert!(!alice_ctx.get::<PushDeviceView>(alice_device).await.unwrap().active().unwrap(), "Alice may deactivate her own row");

    let bob_row = root.get::<PushDeviceView>(bob_device).await.unwrap();
    let trx = alice_ctx.begin();
    assert!(bob_row.edit(&trx).is_err(), "Alice cannot edit Bob's row even when handed its Root view");
}

#[tokio::test(flavor = "multi_thread")]
async fn foreign_ownership_moderator_privilege_and_guests_buy_nothing() {
    let (node, root) = push_node().await;
    let alice = create_user(&root, "Alice").await;
    let bob = create_user(&root, "Bob").await;
    let alice_ctx = member(&node, alice);

    assert!(try_register(&alice_ctx, bob, ALICE_TOKEN).await.is_err(), "Alice cannot create a row owned by Bob");

    let bob_device = try_register(&member(&node, bob), bob, BOB_TOKEN).await.unwrap();
    let alice_mod = moderator(&node, alice);
    assert!(alice_mod.fetch::<PushDeviceView>("true").await.unwrap().is_empty(), "moderator privilege does not bypass device privacy");
    assert!(alice_mod.get::<PushDeviceView>(bob_device).await.is_err());

    let guest = guest(&node);
    assert!(guest.fetch::<PushDeviceView>("true").await.is_err(), "guests cannot scan the registration collection");
    assert!(try_register(&guest, bob, ALICE_TOKEN).await.is_err(), "guests cannot register a device");
}
