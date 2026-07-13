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
