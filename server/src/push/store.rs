//! Where the device tokens live: one row per (member, device).
//!
//! DELIBERATELY NOT AN ANKURAH COLLECTION. Every other row this server keeps is
//! a synced model, and a device token is the one thing here that must not be:
//! it is the whole credential for waking someone's phone, so putting it in a
//! collection would mean writing a policy entry for it and trusting that entry
//! forever. A plain server-side table has no read scope to get wrong — nothing
//! syncs it, and the only code that can read it is in this process.
//!
//! There was no precedent for such a table when this landed: the server had
//! kept every piece of state in ankurah collections, and its two storage
//! engines were reached only through `StorageEngine`. So this module opens one
//! shape per engine behind [`DeviceTokens`], and both share the handle the
//! ankurah node already holds rather than opening a second one — sled refuses a
//! second open of the same directory outright, and Postgres would otherwise
//! carry two connection pools against one database.

use anyhow::Result;
use futures_util::future::BoxFuture;

/// Which push service reaches a device. An enum rather than a string so that
/// adding Google Play (a later phase) is a compile error at every place that
/// has to learn about it, rather than a string comparison nobody updated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Platform {
    Ios,
}

impl Platform {
    /// The wire spelling, stored verbatim and accepted verbatim from
    /// `POST /push/register`.
    pub fn as_str(self) -> &'static str {
        match self {
            Platform::Ios => "ios",
        }
    }

    /// Parse a platform a caller named. `None` is a refusal, not a default: a
    /// client asking to be reached over a service this server cannot reach it
    /// over should hear so, not have its token filed under iOS.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "ios" => Some(Platform::Ios),
            _ => None,
        }
    }
}

/// One registered device: what the sender addresses, over which service, and
/// when its owner last said it was theirs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceToken {
    pub token: String,
    pub platform: Platform,
    /// ms since epoch, the project's timestamp unit.
    pub last_registered_at: i64,
}

/// The device-token registry, as the rest of the server sees it.
///
/// Three operations, which is everything the two callers need: the route files
/// a token, the sender reads a member's devices, and the sender drops one APNs
/// has told us is gone. No listing, no counting, no delete-by-user — a surface
/// this small is one nobody can misuse into a token dump.
///
/// `BoxFuture` rather than `async fn` in the trait, matching
/// `workers::supervise`'s consumer signature: the repo already boxes futures at
/// its one other dynamic-dispatch seam, and this avoids a proc-macro dependency
/// for three methods.
pub trait DeviceTokens: Send + Sync + 'static {
    /// File a token for this member, refreshing the row if it is already
    /// there. The member's entity id (base64) is the key alongside the token,
    /// so the same device registering twice updates one row rather than
    /// accumulating them.
    fn register<'a>(&'a self, user: &'a str, token: &'a str, platform: Platform, at_ms: i64) -> BoxFuture<'a, Result<()>>;

    /// Every device this member has registered.
    fn for_user<'a>(&'a self, user: &'a str) -> BoxFuture<'a, Result<Vec<DeviceToken>>>;

    /// Drop one token, because APNs said the app is no longer installed on
    /// that device (410 Unregistered) or the token is not one it will accept.
    /// Absent rows are not an error: two sends racing the same dead token both
    /// arrive here.
    fn forget<'a>(&'a self, user: &'a str, token: &'a str) -> BoxFuture<'a, Result<()>>;
}

/// The leading characters of a device token, for a log line.
///
/// FOR: an operator watching tokens age out needs to tell one device from
/// another, and a log that carried whole tokens would be a file of live
/// credentials — the same rule the auth routes follow when they log a mint
/// without the token (`main::auth_session`). Eight characters distinguish the
/// handful of devices one member has and reconstruct nothing.
pub fn token_prefix(token: &str) -> String { token.chars().take(8).collect() }

#[cfg(feature = "postgres")]
pub use self::postgres::open as open_postgres;
#[cfg(all(feature = "sled", not(feature = "postgres")))]
pub use self::sled::open as open_sled;

#[cfg(feature = "postgres")]
mod postgres {
    //! The Postgres shape: one table, on the pool the ankurah node already
    //! holds.

    use super::{DeviceToken, DeviceTokens, Platform};
    use anyhow::{anyhow, Context as _, Result};
    use bb8_postgres::{tokio_postgres::NoTls, PostgresConnectionManager};
    use futures_util::future::BoxFuture;
    use futures_util::FutureExt;
    use std::sync::Arc;

    type Pool = bb8::Pool<PostgresConnectionManager<NoTls>>;

    /// Server-owned, and named apart from the ankurah collection tables on
    /// purpose: the storage engine names a table after each collection id
    /// (`message`, `notification`, …), all singular, so a plural name with a
    /// subsystem prefix cannot be mistaken for one or collide with a
    /// collection added later.
    const TABLE: &str = "push_device_tokens";

    /// Open the registry, creating its table on first boot.
    ///
    /// DDL at startup rather than through a migration tool, because this server
    /// has no migration tool and its storage engine does the same thing — the
    /// ankurah Postgres engine creates a collection's table the first time it
    /// is touched. `IF NOT EXISTS` is what makes every later boot a no-op.
    pub async fn open(pool: Pool) -> Result<Arc<dyn DeviceTokens>> {
        let client = pool.get().await.map_err(|e| anyhow!("connect to Postgres for the device-token registry: {e}"))?;
        client
            .execute(
                &format!(
                    "CREATE TABLE IF NOT EXISTS {TABLE} (
                        user_id            text   NOT NULL,
                        device_token       text   NOT NULL,
                        platform           text   NOT NULL,
                        last_registered_at bigint NOT NULL,
                        PRIMARY KEY (user_id, device_token)
                    )"
                ),
                &[],
            )
            .await
            .context("create the device-token table")?;
        drop(client);
        Ok(Arc::new(PostgresDeviceTokens { pool }))
    }

    struct PostgresDeviceTokens {
        pool: Pool,
    }

    impl DeviceTokens for PostgresDeviceTokens {
        fn register<'a>(&'a self, user: &'a str, token: &'a str, platform: Platform, at_ms: i64) -> BoxFuture<'a, Result<()>> {
            async move {
                let client = self.pool.get().await.map_err(|e| anyhow!("connect to Postgres: {e}"))?;
                // The upsert the route promises: the same device registering
                // again refreshes its row instead of adding one. The primary
                // key is the pair, so a member with three phones keeps three
                // rows and a phone that reinstalls keeps one.
                client
                    .execute(
                        &format!(
                            "INSERT INTO {TABLE} (user_id, device_token, platform, last_registered_at)
                             VALUES ($1, $2, $3, $4)
                             ON CONFLICT (user_id, device_token)
                             DO UPDATE SET platform = EXCLUDED.platform, last_registered_at = EXCLUDED.last_registered_at"
                        ),
                        &[&user, &token, &platform.as_str(), &at_ms],
                    )
                    .await
                    .context("register a device token")?;
                Ok(())
            }
            .boxed()
        }

        fn for_user<'a>(&'a self, user: &'a str) -> BoxFuture<'a, Result<Vec<DeviceToken>>> {
            async move {
                let client = self.pool.get().await.map_err(|e| anyhow!("connect to Postgres: {e}"))?;
                let rows = client
                    .query(&format!("SELECT device_token, platform, last_registered_at FROM {TABLE} WHERE user_id = $1"), &[&user])
                    .await
                    .context("read a member's device tokens")?;
                Ok(rows
                    .into_iter()
                    .filter_map(|row| {
                        let platform: String = row.get(1);
                        // A row naming a service this build cannot reach is
                        // skipped rather than failing the read: an older
                        // server writing a platform this one predates must
                        // not take the whole send down.
                        Platform::parse(&platform).map(|platform| DeviceToken {
                            token: row.get(0),
                            platform,
                            last_registered_at: row.get(2),
                        })
                    })
                    .collect())
            }
            .boxed()
        }

        fn forget<'a>(&'a self, user: &'a str, token: &'a str) -> BoxFuture<'a, Result<()>> {
            async move {
                let client = self.pool.get().await.map_err(|e| anyhow!("connect to Postgres: {e}"))?;
                client
                    .execute(&format!("DELETE FROM {TABLE} WHERE user_id = $1 AND device_token = $2"), &[&user, &token])
                    .await
                    .context("forget a device token")?;
                Ok(())
            }
            .boxed()
        }
    }
}

#[cfg(all(feature = "sled", not(feature = "postgres")))]
mod sled {
    //! The sled shape: one tree on the database the ankurah node already
    //! opened.

    use super::{DeviceToken, DeviceTokens, Platform};
    use anyhow::{Context as _, Result};
    use ankurah_storage_sled::SledStorageEngine;
    use futures_util::future::BoxFuture;
    use futures_util::FutureExt;
    use serde::{Deserialize, Serialize};
    use std::sync::Arc;

    /// Server-owned, and outside the `collection_*` namespace the ankurah sled
    /// engine keeps its collections in (see its `list_collections`), so this
    /// tree is never mistaken for one.
    const TREE: &str = "push_device_tokens";

    /// The key separator. An entity id is base64 and a device token is hex, so
    /// neither can contain a NUL — which is what makes `{user}\0{token}` an
    /// unambiguous key and `{user}\0` an unambiguous prefix scan.
    const SEPARATOR: u8 = 0;

    /// What the value holds. The key already carries the member and the token,
    /// so the value is only what the key does not say.
    #[derive(Serialize, Deserialize)]
    struct Row {
        platform: String,
        last_registered_at: i64,
    }

    /// Open the registry on the node's own sled database.
    ///
    /// Borrowing the engine's handle is not a convenience: sled takes a file
    /// lock on its directory, so a second `sled::open` of the same path fails.
    /// One database, one lock, a tree of our own inside it.
    pub fn open(engine: &SledStorageEngine) -> Result<Arc<dyn DeviceTokens>> {
        let tree = engine
            .database
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .db
            .open_tree(TREE)
            .context("open the device-token tree")?;
        Ok(Arc::new(SledDeviceTokens { tree }))
    }

    struct SledDeviceTokens {
        tree: ::sled::Tree,
    }

    fn key(user: &str, token: &str) -> Vec<u8> {
        let mut key = Vec::with_capacity(user.len() + 1 + token.len());
        key.extend_from_slice(user.as_bytes());
        key.push(SEPARATOR);
        key.extend_from_slice(token.as_bytes());
        key
    }

    fn prefix(user: &str) -> Vec<u8> {
        let mut prefix = Vec::with_capacity(user.len() + 1);
        prefix.extend_from_slice(user.as_bytes());
        prefix.push(SEPARATOR);
        prefix
    }

    impl DeviceTokens for SledDeviceTokens {
        fn register<'a>(&'a self, user: &'a str, token: &'a str, platform: Platform, at_ms: i64) -> BoxFuture<'a, Result<()>> {
            async move {
                // An insert on an existing key replaces the value, which is
                // the upsert the route promises — the key is the (member,
                // token) pair, so a device re-registering refreshes one row.
                let row = serde_json::to_vec(&Row { platform: platform.as_str().to_string(), last_registered_at: at_ms })?;
                self.tree.insert(key(user, token), row).context("register a device token")?;
                Ok(())
            }
            .boxed()
        }

        fn for_user<'a>(&'a self, user: &'a str) -> BoxFuture<'a, Result<Vec<DeviceToken>>> {
            async move {
                let mut devices = Vec::new();
                let prefix = prefix(user);
                for entry in self.tree.scan_prefix(&prefix) {
                    let (key, value) = entry.context("read a member's device tokens")?;
                    let Ok(token) = std::str::from_utf8(&key[prefix.len()..]) else { continue };
                    let Ok(row) = serde_json::from_slice::<Row>(&value) else { continue };
                    // Same rule as the Postgres read: a row naming a service
                    // this build cannot reach is skipped, not fatal.
                    let Some(platform) = Platform::parse(&row.platform) else { continue };
                    devices.push(DeviceToken { token: token.to_string(), platform, last_registered_at: row.last_registered_at });
                }
                Ok(devices)
            }
            .boxed()
        }

        fn forget<'a>(&'a self, user: &'a str, token: &'a str) -> BoxFuture<'a, Result<()>> {
            async move {
                self.tree.remove(key(user, token)).context("forget a device token")?;
                Ok(())
            }
            .boxed()
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn store() -> Arc<dyn DeviceTokens> { open(&SledStorageEngine::new_test().unwrap()).unwrap() }

        #[tokio::test]
        async fn a_device_registers_once_and_refreshes_in_place() {
            let store = store();
            let alice = "AZk3jW0RvkW8pTGnQxYzRR";
            let bob = "BZk3jW0RvkW8pTGnQxYzRR";

            store.register(alice, "aa11", Platform::Ios, 100).await.unwrap();
            store.register(alice, "bb22", Platform::Ios, 200).await.unwrap();
            store.register(bob, "cc33", Platform::Ios, 300).await.unwrap();

            let mut alices = store.for_user(alice).await.unwrap();
            alices.sort_by(|a, b| a.token.cmp(&b.token));
            assert_eq!(alices.len(), 2, "two devices, two rows");
            assert_eq!(alices[0].last_registered_at, 100);

            // The same device again: one row, a newer time.
            store.register(alice, "aa11", Platform::Ios, 400).await.unwrap();
            let mut alices = store.for_user(alice).await.unwrap();
            alices.sort_by(|a, b| a.token.cmp(&b.token));
            assert_eq!(alices.len(), 2, "re-registering refreshes rather than adds");
            assert_eq!(alices[0].last_registered_at, 400);

            // The prefix scan really is per member: Bob's device is his own,
            // and dropping one of Alice's leaves the other standing.
            assert_eq!(store.for_user(bob).await.unwrap().len(), 1);
            store.forget(alice, "aa11").await.unwrap();
            assert_eq!(store.for_user(alice).await.unwrap(), vec![DeviceToken {
                token: "bb22".to_string(),
                platform: Platform::Ios,
                last_registered_at: 200
            }]);
            // Forgetting a row that is already gone is not an error: two sends
            // racing the same dead token both arrive here.
            store.forget(alice, "aa11").await.unwrap();
        }
    }
}

/// An in-memory registry, so the route and the sender can be tested without a
/// storage engine under them.
#[cfg(test)]
pub mod memory {
    use super::{DeviceToken, DeviceTokens, Platform};
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

        /// Every row, for assertions.
        pub fn all(&self) -> Vec<((String, String), DeviceToken)> {
            let mut rows: Vec<_> = self.rows.lock().unwrap().iter().map(|(k, v)| (k.clone(), v.clone())).collect();
            rows.sort_by(|a, b| a.0.cmp(&b.0));
            rows
        }
    }

    impl DeviceTokens for MemoryDeviceTokens {
        fn register<'a>(&'a self, user: &'a str, token: &'a str, platform: Platform, at_ms: i64) -> BoxFuture<'a, Result<()>> {
            async move {
                self.rows.lock().unwrap().insert(
                    (user.to_string(), token.to_string()),
                    DeviceToken { token: token.to_string(), platform, last_registered_at: at_ms },
                );
                Ok(())
            }
            .boxed()
        }

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

    #[test]
    fn a_logged_token_is_a_prefix_and_never_the_whole_thing() {
        // The rule this exists to keep: a log line identifies a device without
        // carrying anything that could wake it.
        let token = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let prefix = token_prefix(token);
        assert_eq!(prefix, "01234567");
        assert!(token.len() > prefix.len() * 4, "the prefix is a small fraction of a real token");
        // Short and empty inputs do not panic (`forget` accepts whatever a
        // send reported on).
        assert_eq!(token_prefix("ab"), "ab");
        assert_eq!(token_prefix(""), "");
    }

    #[test]
    fn a_platform_this_build_cannot_reach_is_refused_rather_than_defaulted() {
        assert_eq!(Platform::parse("ios"), Some(Platform::Ios));
        assert_eq!(Platform::Ios.as_str(), "ios");
        // Google Play is a later phase, and until it lands a caller naming it
        // hears a refusal rather than having its token filed under iOS.
        assert_eq!(Platform::parse("android"), None);
        assert_eq!(Platform::parse("iOS"), None, "the spelling is exact");
        assert_eq!(Platform::parse(""), None);
    }
}
