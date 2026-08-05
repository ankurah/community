//! Pins the property-read semantics new model fields rely on, against a real
//! (sled) node — the derive/backend behavior the model comments assert, not
//! the scope evaluator (that lives in policy_scope_tests.rs).
//!
//! Sled-gated like the worker end-to-end test (server/src/workers/mod.rs):
//! run with `cargo test -p community-server --no-default-features --features
//! sled`. The default (postgres) test run compiles this file to nothing.
#![cfg(feature = "sled")]

use ankurah::policy::{PermissiveAgent, DEFAULT_CONTEXT};
use ankurah::{Context, Node};
use ankurah_storage_sled::SledStorageEngine;
use community_model::{Message, MessageView, Room, User};
use std::sync::Arc;

/// The durable-node init dance from main() with the permissive agent — same
/// helper as the worker test; duplicated because integration tests cannot
/// reach the binary crate's modules.
async fn test_context() -> Context {
    let node = Node::new_durable(Arc::new(SledStorageEngine::new_test().unwrap()), PermissiveAgent::new());
    node.system.wait_loaded().await;
    if node.system.root().is_none() {
        node.system.create().await.unwrap();
    }
    node.system.wait_system_ready().await;
    node.context_async(DEFAULT_CONTEXT).await
}

fn message(user: ankurah::EntityId, room: ankurah::EntityId, text: &str, re: Option<ankurah::Ref<Message>>) -> Message {
    Message { user: user.into(), room: room.into(), text: text.into(), timestamp: 1, deleted: false, edited_at: None, collaborative: None, re }
}

/// `Message.re` (#23): a `Ref` written at creation reads back with the same
/// id, and a row created with `None` reads `Ok(None)` through
/// `Option<Ref<_>>` instead of erroring. Shape caveat: a fresh `None` row
/// stores the property WITH a null value (the derive initializes every
/// field); a true pre-reply legacy row lacks the key entirely. Both collapse
/// to `Option<Value>::None` at the LWW read — that read behavior is what
/// this pins. A regression specific to absent-key handling would need a
/// field-less stand-in model on this collection to catch.
#[tokio::test(flavor = "multi_thread")]
async fn reply_ref_round_trips_and_absent_property_reads_none() {
    let ctx = test_context().await;

    let trx = ctx.begin();
    let author = trx.create(&User { display_name: "Author".into(), oidc_sub: None }).await.unwrap().id();
    let room = trx.create(&Room { name: "general".into(), created_by: None, topic: None }).await.unwrap().id();
    let original = trx.create(&message(author, room, "original", None)).await.unwrap().id();
    let reply = trx.create(&message(author, room, "the reply", Some(original.into()))).await.unwrap().id();
    trx.commit().await.unwrap();

    // Null-valued property (fresh `re: None` row) reads None — the same
    // Option collapse an absent-key legacy row takes (see the doc comment).
    let original_view = ctx.get::<MessageView>(original).await.unwrap();
    assert_eq!(original_view.re().unwrap().map(|r| r.id()), None);

    // Ref round-trip: the reply points at the original.
    let reply_view = ctx.get::<MessageView>(reply).await.unwrap();
    assert_eq!(reply_view.re().unwrap().map(|r| r.id()), Some(original));
}

// ---------------------------------------------------------------------------
// Direct messages (#30)
// ---------------------------------------------------------------------------

/// One unordered pair of users maps to exactly one `(a, b)` tuple, whichever
/// side asks. This is what makes find-or-create converge: both participants
/// build the same `THREADS_FOR_PAIR` lookup, so neither can miss a thread the
/// other created and open a second one. That lookup asks about BOTH orderings
/// — the write scope can only ask whether the writer is one of `a`/`b`, so the
/// server accepts a reversed row — and canonical order is what decides which
/// of the rows it finds everyone then agrees to write into.
///
/// The order is `EntityId`'s own (the ULID's bytes), NOT the base64 text form —
/// those disagree, because the base64url alphabet is not in ASCII order. This
/// test would pass under either convention for most pairs; the base64 leg below
/// is what would catch a helper quietly switching to string comparison.
#[test]
fn canonical_pair_is_symmetric_total_and_byte_ordered() {
    use community_model::canonical_pair;

    for _ in 0..256 {
        let x = ankurah::EntityId::new();
        let y = ankurah::EntityId::new();
        assert_eq!(canonical_pair(x, y), canonical_pair(y, x), "the pair must not depend on who asks");
        let (a, b) = canonical_pair(x, y);
        assert!(a <= b, "canonical order is ascending by entity id");
        assert_eq!(canonical_pair(a, b), (a, b), "already-canonical input is a fixed point");
    }

    // A self-pair is total, not an error: the helper never has to be unwrapped.
    let me = ankurah::EntityId::new();
    assert_eq!(canonical_pair(me, me), (me, me));

    // Byte order and base64 order really do disagree, so the choice is
    // load-bearing rather than cosmetic — and the witness is constructed
    // rather than searched for, so this leg cannot quietly test nothing.
    //
    // The base64url alphabet runs A-Z, a-z, 0-9, '-', '_': value 62 encodes as
    // '-' (ASCII 45) and value 0 as 'A' (ASCII 65). So an id whose first six
    // bits are 62 sorts AFTER one whose first six bits are 0 by bytes, and
    // BEFORE it as text.
    let leading_62 = ankurah::EntityId::from_bytes([0xF8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    let leading_0 = ankurah::EntityId::from_bytes([0; 16]);
    assert!(leading_0 < leading_62, "by bytes, the id starting 0x00 is the lower one");
    assert!(leading_62.to_base64() < leading_0.to_base64(), "as text, '-…' sorts before 'A…' — the orders disagree");

    let (a, b) = community_model::canonical_pair(leading_62, leading_0);
    assert_eq!(a, leading_0, "canonical order follows entity-id bytes, not base64 text");
    assert_eq!(b, leading_62);
}

/// The viewer's correspondent, from either arm; `None` for the degenerate
/// self-thread and for a non-participant (unreachable through a scoped context,
/// but the helper must not name a stranger as "the other person" if it happens).
#[test]
fn dm_partner_resolves_from_either_arm() {
    use community_model::dm_partner;
    let me = ankurah::EntityId::new();
    let them = ankurah::EntityId::new();
    let stranger = ankurah::EntityId::new();

    assert_eq!(dm_partner(me, them, me), Some(them));
    assert_eq!(dm_partner(them, me, me), Some(them));
    assert_eq!(dm_partner(me, me, me), None, "a self-thread has no other participant");
    assert_eq!(dm_partner(me, them, stranger), None, "a non-participant has no partner in this thread");
}

/// The create-path pin the model's "born with both participants" rule needs.
///
/// There is no way to construct a `DmThread`/`DmMessage` without both
/// participant fields — the struct literal would not compile — so what is worth
/// pinning at runtime is that a row created through the normal path really does
/// carry both properties, readable through the bare (non-`Option`) accessors.
/// A participant field that ever became optional, or was added to the
/// collection later, would read back as an error here — and in production it
/// would silently hide the row from BOTH participants (see the model docs and
/// `or_scope_live_tests.rs`).
#[tokio::test(flavor = "multi_thread")]
async fn dm_rows_are_born_with_both_participants_and_refs_round_trip() {
    use community_model::{canonical_pair, DmMessage, DmMessageView, DmReadState, DmReadStateView, DmThread, DmThreadView};

    let ctx = test_context().await;

    let trx = ctx.begin();
    let alice = trx.create(&User { display_name: "Alice".into(), oidc_sub: None }).await.unwrap().id();
    let bob = trx.create(&User { display_name: "Bob".into(), oidc_sub: None }).await.unwrap().id();
    trx.commit().await.unwrap();

    let (a, b) = canonical_pair(alice, bob);

    let trx = ctx.begin();
    let thread = trx.create(&DmThread { a: a.into(), b: b.into(), created_at: 1, deleted: false }).await.unwrap().id();
    trx.commit().await.unwrap();

    let trx = ctx.begin();
    let message = trx
        .create(&DmMessage {
            thread: thread.into(),
            a: a.into(),
            b: b.into(),
            user: alice.into(),
            text: "hello".into(),
            timestamp: 2,
            deleted: false,
            edited_at: None,
        })
        .await
        .unwrap()
        .id();
    let read_state =
        trx.create(&DmReadState { user: alice.into(), thread: thread.into(), last_read_ts: 2 }).await.unwrap().id();
    trx.commit().await.unwrap();

    // Both participants present on the thread, in canonical order.
    let thread_view = ctx.get::<DmThreadView>(thread).await.unwrap();
    assert_eq!(thread_view.a().unwrap().id(), a, "`a` is present and is the lower id");
    assert_eq!(thread_view.b().unwrap().id(), b, "`b` is present and is the higher id");
    assert!(thread_view.a().unwrap().id() <= thread_view.b().unwrap().id());

    // Both participants copied verbatim onto the message, plus its filing key
    // (`thread`) and its sender.
    let message_view = ctx.get::<DmMessageView>(message).await.unwrap();
    assert_eq!(message_view.a().unwrap().id(), a);
    assert_eq!(message_view.b().unwrap().id(), b);
    assert_eq!(message_view.thread().unwrap().id(), thread, "render paths key off `thread`, so it must round-trip exactly");
    assert_eq!(message_view.user().unwrap().id(), alice);
    assert_eq!(message_view.text().unwrap(), "hello");
    assert!(!message_view.deleted().unwrap());
    assert_eq!(message_view.edited_at().unwrap(), None, "a never-edited message reads None, not an error");

    // The read cursor carries no participant pair by design (it is scoped to
    // its owner, not to the thread's members) — just owner and thread.
    let read_state_view = ctx.get::<DmReadStateView>(read_state).await.unwrap();
    assert_eq!(read_state_view.user().unwrap().id(), alice);
    assert_eq!(read_state_view.thread().unwrap().id(), thread);
    assert_eq!(read_state_view.last_read_ts().unwrap(), 2);
}

/// `ModAction.actor` became `Option<Ref<User>>` when the DM rate limiter began
/// writing rows nothing human authored (#30). Both legs of that retrofit are
/// claims the model doc makes, so both are pinned here: a row written with a
/// moderator reads back as `Some(that moderator)` — the shape every row
/// created before the retrofit has, since they all carry the property with a
/// value — and a row written with `None` reads `None` rather than erroring the
/// way a bare `Ref` would on an absent property.
///
/// The `Some` leg is the one that matters for the mod log: it renders "actor
/// unknown" for a `None`, so an `Option` read that quietly collapsed every row
/// to `None` would turn the entire moderation history anonymous while every
/// test still passed.
#[tokio::test(flavor = "multi_thread")]
async fn mod_action_actor_reads_some_for_human_rows_and_none_for_automatic_ones() {
    use community_model::{ModAction, ModActionView};

    let ctx = test_context().await;

    let trx = ctx.begin();
    let moderator = trx.create(&User { display_name: "Moderator".into(), oidc_sub: None }).await.unwrap().id();
    let member = trx.create(&User { display_name: "Member".into(), oidc_sub: None }).await.unwrap().id();
    let by_hand = trx
        .create(&ModAction {
            actor: Some(moderator.into()),
            message: None,
            user: Some(member.into()),
            action: "ban".into(),
            reason: Some("spam".into()),
            created_at: 1,
        })
        .await
        .unwrap()
        .id();
    let automatic = trx
        .create(&ModAction {
            actor: None,
            message: None,
            user: Some(member.into()),
            action: "dm-rate-limit".into(),
            reason: Some("Automatic DM rate limit.".into()),
            created_at: 2,
        })
        .await
        .unwrap()
        .id();
    trx.commit().await.unwrap();

    let by_hand = ctx.get::<ModActionView>(by_hand).await.unwrap();
    assert_eq!(
        by_hand.actor().unwrap().map(|r| r.id()),
        Some(moderator),
        "a row with a moderator names them: the retrofit must not read every row as actorless"
    );

    let automatic = ctx.get::<ModActionView>(automatic).await.unwrap();
    assert_eq!(automatic.actor().unwrap().map(|r| r.id()), None, "a row nothing human wrote reads None, not an error");
}
