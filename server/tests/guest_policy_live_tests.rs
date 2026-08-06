//! What a guest session actually reaches, against the REAL `policy.json` and
//! the REAL models on a real (sled) node (#79).
//!
//! `policy_scope_tests.rs` pins the privilege split as written in the file;
//! this pins what the agent does with it, which is the claim the feature rests
//! on. A guest arrives with no account, `sub = "guest"` and one role, and gets:
//!
//! - the conversation — rooms, messages, reactions, link previews;
//! - nothing from the roster (`user`, `userroles`) or the moderation log
//!   (`modaction`), which the collection gate refuses outright;
//! - nothing from anybody's direct messages, refused by the row scopes that
//!   were already there — no rule in this policy mentions guests.
//!
//! And, because a read-only visitor that can write is not read-only: a guest's
//! attempt to post is refused.
//!
//! THE GUEST CLAIMS ARE SPELLED OUT HERE, and they must stay the same two
//! strings `server/src/guest.rs` mints (`GUEST_SUB`, `GUEST_ROLE`). Nothing can
//! import them: community-server is a binary crate, so an integration test
//! cannot reach its constants. If the endpoint's literals ever change and these
//! do not, this file goes on passing while production hands out a token that
//! matches no role in the policy — which fails closed, but silently.
//!
//! Sled-gated like the other live-node suites: run with
//! `cargo test -p community-server --no-default-features --features sled`.
#![cfg(feature = "sled")]

use ankurah::property::Json;
use ankurah::{Context, EntityId, Node};
use ankurah_jwt_auth::{JwtAgent, JwtClaims, JwtContext, PolicyConfig};
use ankurah_storage_sled::SledStorageEngine;
use community_model::{
    canonical_pair, DmMessage, DmMessageView, DmThread, DmThreadView, LinkPreview, LinkPreviewView, Message, MessageView, ModAction,
    ModActionView, Reaction, ReactionView, Room, RoomView, User, UserRoles, UserRolesView, UserView,
};
use std::sync::Arc;

/// The shipped policy, verbatim — the point of this harness is that nothing
/// here restates a rule that production could then drift away from.
const POLICY_JSON: &str = include_str!("../../policy.json");

/// A durable node carrying the real policy, plus a Root context for setup
/// writes (Root bypasses both the collection gate and the row scopes).
async fn guest_node() -> (Node<SledStorageEngine, JwtAgent>, Context) {
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

/// The context a guest token produces. Same claim shape as the endpoint mints:
/// the literal subject, the one role, no email, no custom claims. On a single
/// local node the token string is never verified (every check under test reads
/// the claims), which is what lets this suite run without an RSA keypair.
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

/// A signed-in member, for the no-regression half of every assertion below.
fn member(node: &Node<SledStorageEngine, JwtAgent>, user: EntityId) -> Context {
    let claims = JwtClaims {
        sub: user.to_base64(),
        roles: vec!["member".to_string()],
        email: format!("{}@guest-tests.invalid", user.to_base64()),
        name: None,
        custom: serde_json::Map::new(),
    };
    node.context(JwtContext::from_claims(claims, "member-placeholder-token".to_string())).expect("context")
}

/// One room with one message in it, one reaction to that message, and one link
/// preview: the four collections that make a page worth arriving at.
async fn seed_conversation(root: &Context) -> (EntityId, EntityId, EntityId, EntityId, EntityId) {
    let trx = root.begin();
    let author = trx.create(&User { display_name: "Alice".to_string(), oidc_sub: None }).await.unwrap().id();
    let room = trx.create(&Room { name: "general".to_string(), created_by: None, topic: None }).await.unwrap().id();
    trx.commit().await.unwrap();

    let trx = root.begin();
    let message = trx
        .create(&Message {
            user: author.into(),
            room: room.into(),
            text: "https://example.com is worth a look".to_string(),
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

    let trx = root.begin();
    let reaction =
        trx.create(&Reaction { message: message.into(), user: author.into(), emoji: "👍".to_string(), active: true }).await.unwrap().id();
    let preview = trx
        .create(&LinkPreview {
            url: "https://example.com".to_string(),
            title: Some("Example".to_string()),
            description: None,
            image_url: None,
            fetched_at: 2,
            ok: true,
        })
        .await
        .unwrap()
        .id();
    trx.commit().await.unwrap();

    (author, room, message, reaction, preview)
}

/// What an anonymous visitor is here for: the rooms, what was said in them, the
/// reactions, and the link previews that render alongside. All four on the
/// one-shot path and by direct id, because a guest client does both.
#[tokio::test(flavor = "multi_thread")]
async fn a_guest_reads_the_conversation() {
    let (node, root) = guest_node().await;
    let (_author, room, message, reaction, preview) = seed_conversation(&root).await;
    let guest = guest(&node);

    assert_eq!(guest.fetch::<RoomView>("true").await.unwrap().len(), 1, "a guest sees the rooms");
    let messages = guest.fetch::<MessageView>("true").await.unwrap();
    assert_eq!(messages.len(), 1, "a guest reads what was said");
    assert_eq!(messages[0].text().unwrap(), "https://example.com is worth a look");
    assert_eq!(guest.fetch::<ReactionView>("true").await.unwrap().len(), 1, "a guest sees the reactions");
    assert_eq!(guest.fetch::<LinkPreviewView>("true").await.unwrap().len(), 1, "a guest sees the link previews");

    assert_eq!(guest.get::<RoomView>(room).await.unwrap().name().unwrap(), "general");
    assert!(guest.get::<MessageView>(message).await.is_ok());
    assert!(guest.get::<ReactionView>(reaction).await.is_ok());
    assert!(guest.get::<LinkPreviewView>(preview).await.is_ok());
}

/// The no-roster ruling, executable. `user` and `userroles` are the membership
/// list and its badges; `modaction` is the moderation record. All three sit
/// behind the signed-in read privilege, so the collection gate refuses a guest
/// before any row is considered — which is why these are errors rather than
/// empty resultsets.
#[tokio::test(flavor = "multi_thread")]
async fn a_guest_reads_no_roster_and_no_moderation_log() {
    let (node, root) = guest_node().await;
    let (author, _room, message, _reaction, _preview) = seed_conversation(&root).await;

    let trx = root.begin();
    let roles = trx
        .create(&UserRoles { user: author.into(), roles: Json::new(serde_json::json!(["member"])) })
        .await
        .unwrap()
        .id();
    let action = trx
        .create(&ModAction {
            actor: None,
            message: Some(message.into()),
            user: None,
            action: "delete".to_string(),
            reason: Some("off topic".to_string()),
            created_at: 3,
        })
        .await
        .unwrap()
        .id();
    trx.commit().await.unwrap();

    let guest = guest(&node);
    assert!(guest.fetch::<UserView>("true").await.is_err(), "no roster for a guest");
    assert!(guest.fetch::<UserRolesView>("true").await.is_err(), "no role badges for a guest");
    assert!(guest.fetch::<ModActionView>("true").await.is_err(), "the moderation log is community business");

    // Knowing an id buys nothing: the gate is on the collection, not the query.
    assert!(guest.get::<UserView>(author).await.is_err());
    assert!(guest.get::<UserRolesView>(roles).await.is_err());
    assert!(guest.get::<ModActionView>(action).await.is_err());

    // The control: a signed-in member reads all three, so the refusals above
    // are the privilege split talking and not an empty database.
    let member = member(&node, author);
    assert_eq!(member.fetch::<UserView>("true").await.unwrap().len(), 1);
    assert_eq!(member.fetch::<UserRolesView>("true").await.unwrap().len(), 1);
    assert_eq!(member.fetch::<ModActionView>("true").await.unwrap().len(), 1);
}

/// Nobody's DMs. This is the case no rule in the policy was written for: the
/// participant-pair scopes compare `a`/`b` against `$jwt.sub`, and a guest's
/// subject is the literal `guest`, which is not an entity id and equals none of
/// them. The comparison is false rather than an error, so the query simply
/// matches nothing and a direct get is refused outright.
#[tokio::test(flavor = "multi_thread")]
async fn a_guest_reads_nobodys_direct_messages() {
    let (node, root) = guest_node().await;

    let trx = root.begin();
    let alice = trx.create(&User { display_name: "Alice".to_string(), oidc_sub: None }).await.unwrap().id();
    let bob = trx.create(&User { display_name: "Bob".to_string(), oidc_sub: None }).await.unwrap().id();
    trx.commit().await.unwrap();
    let (a, b) = canonical_pair(alice, bob);

    let trx = root.begin();
    let thread = trx.create(&DmThread { a: a.into(), b: b.into(), created_at: 1, deleted: false }).await.unwrap().id();
    trx.commit().await.unwrap();
    let trx = root.begin();
    let dm = trx
        .create(&DmMessage {
            thread: thread.into(),
            a: a.into(),
            b: b.into(),
            user: alice.into(),
            text: "the quiet part".to_string(),
            timestamp: 2,
            deleted: false,
            edited_at: None,
        })
        .await
        .unwrap()
        .id();
    trx.commit().await.unwrap();

    let guest = guest(&node);
    assert!(guest.fetch::<DmThreadView>("true").await.unwrap().is_empty(), "a guest is in nobody's thread");
    assert!(guest.fetch::<DmMessageView>("true").await.unwrap().is_empty(), "and reads nobody's messages");
    assert!(guest.get::<DmThreadView>(thread).await.is_err(), "not by id either");
    assert!(guest.get::<DmMessageView>(dm).await.is_err());

    // A participant does receive it, so "empty" above is a denial rather than
    // an empty database — and a signed-in member who is NOT a participant is
    // just as blind as the guest, which is the pre-existing posture this
    // change must not have loosened.
    let alice_ctx = member(&node, alice);
    assert_eq!(alice_ctx.fetch::<DmMessageView>("true").await.unwrap().len(), 1);
    let carol_ctx = member(&node, EntityId::new());
    assert!(carol_ctx.fetch::<DmMessageView>("true").await.unwrap().is_empty(), "a non-participant member reads no DMs");
    assert!(carol_ctx.get::<DmMessageView>(dm).await.is_err());
}

/// Read-only means read-only. The guest role holds no write privilege at all,
/// so the collection write gate refuses a post before any row scope runs — and
/// the same goes for the collections a guest CAN read.
#[tokio::test(flavor = "multi_thread")]
async fn a_guest_writes_nothing() {
    let (node, root) = guest_node().await;
    let (author, room, message, _reaction, _preview) = seed_conversation(&root).await;
    let guest = guest(&node);

    let posted = async {
        let trx = guest.begin();
        trx.create(&Message {
            user: author.into(),
            room: room.into(),
            text: "posting as nobody".to_string(),
            timestamp: 4,
            deleted: false,
            edited_at: None,
            collaborative: None,
            re: None,
        })
        .await?;
        trx.commit().await?;
        Ok::<_, Box<dyn std::error::Error>>(())
    }
    .await;
    assert!(posted.is_err(), "a guest may not post");

    let reacted = async {
        let trx = guest.begin();
        trx.create(&Reaction { message: message.into(), user: author.into(), emoji: "🔥".to_string(), active: true }).await?;
        trx.commit().await?;
        Ok::<_, Box<dyn std::error::Error>>(())
    }
    .await;
    assert!(reacted.is_err(), "a guest may not react");

    // The control: the same call, from a signed-in member writing as
    // themselves, lands. Without it a write path broken for everybody — a
    // misconfigured node, a model that no longer commits — would read as a
    // guest-specific denial and this test would pass for the wrong reason.
    let member = member(&node, author);
    let trx = member.begin();
    trx.create(&Message {
        user: author.into(),
        room: room.into(),
        text: "posting as myself".to_string(),
        timestamp: 5,
        deleted: false,
        edited_at: None,
        collaborative: None,
        re: None,
    })
    .await
    .unwrap();
    trx.commit().await.expect("a member may post as themselves");

    // Nothing the guest attempted landed; the member's message did.
    let messages = root.fetch::<MessageView>("true").await.unwrap();
    assert_eq!(messages.len(), 2, "the seeded message and the member's, and nothing from the guest");
    assert!(messages.iter().all(|row| row.text().unwrap() != "posting as nobody"));
    assert_eq!(root.fetch::<ReactionView>("true").await.unwrap().len(), 1, "only the seeded reaction exists");
}
