//! Device registrations as Ankurah entities.
//!
//! A phone writes its own [`community_model::PushDevice`] row through the same
//! ephemeral node and policy agent as every other client write. The collection
//! is self-scoped in `policy.json`: a signed-in member can see and change rows
//! whose `user` is their JWT subject, and nobody else's. The server's durable
//! Root context reads those rows to address alerts and deactivates a row when
//! APNs says its token is gone.
//!
//! No backend-specific persistence lives here. Sled and Postgres are Ankurah
//! storage engines, and this collection reaches both through [`Context`]. The
//! in-memory implementation below is only a test double for the HTTP/2 sender;
//! it is not a production persistence path.

use std::collections::HashMap;
use std::sync::Arc;

use ankurah::ankql::{ast::Expr, parser::parse_selection};
use ankurah::{Context, EntityId};
use anyhow::{Context as _, Result};
use community_model::PushDeviceView;
use futures_util::future::BoxFuture;
use futures_util::FutureExt;

/// How many devices one member can cause the sender to address, newest kept.
///
/// The honest client also deactivates rows past this cap when it registers.
/// The read path applies the cap again because a client controls its own rows:
/// a hand-written client may skip the cleanup, but it may not turn one inbox
/// event into an unbounded number of outbound APNs requests.
pub const MAX_DEVICES_PER_USER: usize = community_model::MAX_PUSH_DEVICES_PER_USER;

const MIN_TOKEN_CHARS: usize = 32;
const MAX_TOKEN_CHARS: usize = 256;

/// Which push service reaches a device. An enum rather than a string so adding
/// Google Play is a compile error at every transport-aware call site.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Platform {
    Ios,
}

impl Platform {
    #[cfg(test)]
    pub fn as_str(self) -> &'static str {
        match self {
            Platform::Ios => "ios",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "ios" => Some(Platform::Ios),
            _ => None,
        }
    }
}

/// One active delivery address read from the collection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceToken {
    pub token: String,
    pub platform: Platform,
    pub last_registered_at: i64,
}

/// The narrow surface the sender needs from the registration collection.
///
/// Registration is deliberately absent from this server-side surface: the
/// ephemeral client writes `PushDevice` directly. The sender only reads active
/// addresses and deactivates an address APNs has invalidated.
pub trait DeviceTokens: Send + Sync + 'static {
    fn for_user<'a>(&'a self, user: &'a str) -> BoxFuture<'a, Result<Vec<DeviceToken>>>;
    fn forget<'a>(&'a self, user: &'a str, token: &'a str) -> BoxFuture<'a, Result<()>>;
}

/// Open the registration collection over the durable node's Root context.
pub fn open(ctx: Context) -> Arc<dyn DeviceTokens> { Arc::new(AnkurahDeviceTokens { ctx }) }

struct AnkurahDeviceTokens {
    ctx: Context,
}

fn user_predicate(user: &str) -> Result<ankurah::ankql::ast::Predicate> {
    let user = EntityId::from_base64(user).context("PushDevice owner is not an entity id")?;
    Ok(parse_selection("user = ?")?.predicate.populate([Expr::from(&user)])?)
}

async fn rows_for_user(ctx: &Context, user: &str) -> Result<Vec<PushDeviceView>> {
    Ok(ctx.fetch::<PushDeviceView>(user_predicate(user)?).await?)
}

impl DeviceTokens for AnkurahDeviceTokens {
    fn for_user<'a>(&'a self, user: &'a str) -> BoxFuture<'a, Result<Vec<DeviceToken>>> {
        async move {
            // Deduplicate defensively. Two independently booting clients can
            // race their first create because entity ids, not property pairs,
            // are unique; one APNs address must still receive one request.
            let mut newest: HashMap<String, DeviceToken> = HashMap::new();
            for row in rows_for_user(&self.ctx, user).await? {
                if !row.active()? {
                    continue;
                }
                let token = row.token()?;
                if !token_is_plausible(&token) {
                    continue;
                }
                let Some(platform) = Platform::parse(&row.platform()?) else { continue };
                let candidate = DeviceToken { token: token.clone(), platform, last_registered_at: row.last_registered_at()? };
                match newest.get(&token) {
                    Some(existing) if existing.last_registered_at >= candidate.last_registered_at => {}
                    _ => {
                        newest.insert(token, candidate);
                    }
                }
            }
            let mut devices: Vec<DeviceToken> = newest.into_values().collect();
            devices.sort_by(|a, b| {
                b.last_registered_at.cmp(&a.last_registered_at).then_with(|| b.token.cmp(&a.token))
            });
            devices.truncate(MAX_DEVICES_PER_USER);
            Ok(devices)
        }
        .boxed()
    }

    fn forget<'a>(&'a self, user: &'a str, token: &'a str) -> BoxFuture<'a, Result<()>> {
        async move {
            let matching: Vec<PushDeviceView> = rows_for_user(&self.ctx, user)
                .await?
                .into_iter()
                .filter(|row| row.token().map(|stored| stored == token).unwrap_or(false))
                .collect();
            if matching.is_empty() {
                return Ok(());
            }
            let trx = self.ctx.begin();
            for row in matching {
                row.edit(&trx)?.active().set(&false)?;
            }
            trx.commit().await?;
            Ok(())
        }
        .boxed()
    }
}

/// The leading characters safe to put in an operational log line.
pub fn token_prefix(token: &str) -> String { token.chars().take(8).collect() }

fn token_is_plausible(token: &str) -> bool {
    token.len() >= MIN_TOKEN_CHARS && token.len() <= MAX_TOKEN_CHARS && token.chars().all(|c| c.is_ascii_hexdigit())
}

/// In-memory sender test double. Production always uses [`open`].
#[cfg(test)]
pub mod memory {
    use super::{DeviceToken, DeviceTokens, Platform, MAX_DEVICES_PER_USER};
    use anyhow::Result;
    use futures_util::future::BoxFuture;
    use futures_util::FutureExt;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    pub struct MemoryDeviceTokens {
        rows: Mutex<HashMap<(String, String), DeviceToken>>,
    }

    impl MemoryDeviceTokens {
        pub fn new() -> Arc<Self> { Arc::new(Self::default()) }

        pub async fn register(&self, user: &str, token: &str, platform: Platform, at_ms: i64) -> Result<()> {
            let mut rows = self.rows.lock().unwrap();
            rows.insert(
                (user.to_string(), token.to_string()),
                DeviceToken { token: token.to_string(), platform, last_registered_at: at_ms },
            );
            let mut mine: Vec<(String, i64)> = rows
                .iter()
                .filter(|((owner, _), _)| owner == user)
                .map(|((_, token), device)| (token.clone(), device.last_registered_at))
                .collect();
            if mine.len() > MAX_DEVICES_PER_USER {
                mine.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| b.0.cmp(&a.0)));
                for (token, _) in &mine[MAX_DEVICES_PER_USER..] {
                    rows.remove(&(user.to_string(), token.clone()));
                }
            }
            Ok(())
        }
    }

    impl DeviceTokens for MemoryDeviceTokens {
        fn for_user<'a>(&'a self, user: &'a str) -> BoxFuture<'a, Result<Vec<DeviceToken>>> {
            async move {
                let rows = self.rows.lock().unwrap();
                let mut devices: Vec<DeviceToken> =
                    rows.iter().filter(|((owner, _), _)| owner == user).map(|(_, device)| device.clone()).collect();
                devices.sort_by(|a, b| a.token.cmp(&b.token));
                Ok(devices)
            }
            .boxed()
        }

        fn forget<'a>(&'a self, user: &'a str, token: &'a str) -> BoxFuture<'a, Result<()>> {
            async move {
                self.rows.lock().unwrap().remove(&(user.to_string(), token.to_string()));
                Ok(())
            }
            .boxed()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "sled")]
    use ankurah::policy::{PermissiveAgent, DEFAULT_CONTEXT};
    #[cfg(feature = "sled")]
    use ankurah::Node;
    #[cfg(feature = "sled")]
    use ankurah_storage_sled::SledStorageEngine;
    #[cfg(feature = "sled")]
    use community_model::{PushDevice, User};

    #[cfg(feature = "sled")]
    async fn test_context() -> Context {
        let node = Node::new_durable(Arc::new(SledStorageEngine::new_test().unwrap()), PermissiveAgent::new());
        node.system.wait_loaded().await;
        if node.system.root().is_none() {
            node.system.create().await.unwrap();
        }
        node.system.wait_system_ready().await;
        node.context_async(DEFAULT_CONTEXT).await
    }

    #[test]
    fn a_logged_token_is_a_prefix_and_never_the_whole_thing() {
        let token = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let prefix = token_prefix(token);
        assert_eq!(prefix, "01234567");
        assert!(token.len() > prefix.len() * 4);
        assert_eq!(token_prefix("ab"), "ab");
        assert_eq!(token_prefix(""), "");
    }

    #[test]
    fn only_plausible_apns_tokens_reach_the_transport() {
        assert!(token_is_plausible(&"a".repeat(64)));
        assert!(token_is_plausible(&"0123456789abcdefABCDEF".repeat(4)));
        assert!(!token_is_plausible("hello"));
        assert!(!token_is_plausible(&format!("{}!", "a".repeat(64))));
        assert!(!token_is_plausible(&"a".repeat(MAX_TOKEN_CHARS + 1)));
    }

    #[test]
    fn a_platform_this_build_cannot_reach_is_refused_rather_than_defaulted() {
        assert_eq!(Platform::parse("ios"), Some(Platform::Ios));
        assert_eq!(Platform::Ios.as_str(), "ios");
        assert_eq!(Platform::parse("android"), None);
        assert_eq!(Platform::parse("iOS"), None);
        assert_eq!(Platform::parse(""), None);
    }

    #[cfg(feature = "sled")]
    #[tokio::test(flavor = "multi_thread")]
    async fn ankurah_rows_are_filtered_deduplicated_capped_and_deactivated() {
        let ctx = test_context().await;
        let trx = ctx.begin();
        let user = trx.create(&User { display_name: "Alice".to_string(), oidc_sub: None }).await.unwrap().id();
        trx.commit().await.unwrap();

        let trx = ctx.begin();
        for n in 0..=MAX_DEVICES_PER_USER {
            trx.create(&PushDevice {
                user: user.into(),
                token: format!("{n:064x}"),
                platform: "ios".to_string(),
                last_registered_at: n as i64,
                active: true,
            })
            .await
            .unwrap();
        }
        // A racing twin keeps the newest claim for one token, while malformed,
        // unsupported, and inactive rows never reach the transport.
        for (token, platform, at, active) in [
            (format!("{:064x}", MAX_DEVICES_PER_USER), "ios", 999, true),
            ("not-a-device-token".to_string(), "ios", 1_000, true),
            ("c".repeat(64), "android", 1_001, true),
            ("d".repeat(64), "ios", 1_002, false),
        ] {
            trx.create(&PushDevice {
                user: user.into(),
                token,
                platform: platform.to_string(),
                last_registered_at: at,
                active,
            })
            .await
            .unwrap();
        }
        trx.commit().await.unwrap();

        let tokens = open(ctx.clone());
        let devices = tokens.for_user(&user.to_base64()).await.unwrap();
        assert_eq!(devices.len(), MAX_DEVICES_PER_USER, "one member can address no more than the delivery cap");
        assert!(!devices.iter().any(|device| device.token == format!("{:064x}", 0)), "the oldest unique token falls off");
        let newest = devices.iter().find(|device| device.token == format!("{:064x}", MAX_DEVICES_PER_USER)).unwrap();
        assert_eq!(newest.last_registered_at, 999, "duplicate rows collapse to the newest claim");
        assert!(devices.iter().all(|device| device.platform == Platform::Ios));

        tokens.forget(&user.to_base64(), &newest.token).await.unwrap();
        let after = tokens.for_user(&user.to_base64()).await.unwrap();
        assert_eq!(after.len(), MAX_DEVICES_PER_USER, "the next-newest token fills the delivery cap");
        assert!(!after.iter().any(|device| device.token == newest.token), "APNs invalidation deactivates every twin");
    }
}
