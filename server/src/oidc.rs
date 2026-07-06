//! Validation of idp.to OIDC ID tokens (the "federate" half of
//! federate-and-remint). We verify the RS256 signature against idp.to's JWKS,
//! plus `iss` / `aud` / `exp` (and the `nonce` when the client supplies it),
//! then hand the extracted identity to the mint step in `main.rs`.
//!
//! This is deliberately *not* `ankurah_jwt_auth` — that crate verifies a single
//! local PEM (our own minting key). idp.to publishes a rotating JWKS keyed by
//! `kid`, so we validate its tokens with `jsonwebtoken` and only then mint an
//! ankurah session token signed with our own `SigningKeys`.

use std::collections::HashMap;

use anyhow::{anyhow, Context, Result};
use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use serde::Deserialize;
use tokio::sync::RwLock;

/// idp.to config, overridable by env for testing / future re-pointing.
const DEFAULT_ISSUER: &str = "https://id.idp.to";
const DEFAULT_CLIENT_ID: &str = "app_HsW5XyYWbr0KQrHZb5iejw";
const DEFAULT_JWKS_URI: &str = "https://id.idp.to/oidc/jwks";

/// The identity we trust after validating an idp.to ID token.
pub struct VerifiedIdentity {
    /// Stable idp.to subject — the key we store on `User.oidc_sub`.
    pub sub: String,
    pub email: Option<String>,
    pub name: Option<String>,
}

/// Only the claims we read. `jsonwebtoken` validates `iss`/`aud`/`exp`
/// separately via `Validation`, so they need not appear here.
#[derive(Debug, Deserialize)]
struct IdTokenClaims {
    sub: String,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    nonce: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Jwks {
    keys: Vec<Jwk>,
}

/// One RSA key from the JWKS. Extra members (`kty`, `alg`, `use`) are ignored.
#[derive(Debug, Deserialize)]
struct Jwk {
    kid: String,
    n: String,
    e: String,
}

/// Verifies idp.to ID tokens, caching the JWKS and refetching on an unknown `kid`.
pub struct OidcVerifier {
    issuer: String,
    /// Expected `aud` — our public client_id.
    client_id: String,
    jwks_uri: String,
    http: reqwest::Client,
    /// kid -> decoding key.
    keys: RwLock<HashMap<String, DecodingKey>>,
}

impl OidcVerifier {
    /// Build from env with idp.to defaults.
    pub fn from_env() -> Self {
        let issuer = env_or("OIDC_ISSUER", DEFAULT_ISSUER);
        let client_id = env_or("OIDC_CLIENT_ID", DEFAULT_CLIENT_ID);
        let jwks_uri = env_or("OIDC_JWKS_URI", DEFAULT_JWKS_URI);
        Self { issuer, client_id, jwks_uri, http: reqwest::Client::new(), keys: RwLock::new(HashMap::new()) }
    }

    /// Validate an ID token and return the verified identity.
    ///
    /// `expected_nonce` (the value the client stashed before redirecting) is
    /// checked against the token's `nonce` when supplied — defense in depth
    /// against replay of a token minted for a different sign-in attempt.
    pub async fn verify(&self, id_token: &str, expected_nonce: Option<&str>) -> Result<VerifiedIdentity> {
        let header = decode_header(id_token).context("decode ID token header")?;
        let kid = header.kid.ok_or_else(|| anyhow!("ID token has no `kid` header"))?;

        let key = self.key_for_kid(&kid).await?;

        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_issuer(&[self.issuer.as_str()]);
        validation.set_audience(&[self.client_id.as_str()]);
        // `exp` is validated by default.

        let data = decode::<IdTokenClaims>(id_token, &key, &validation).context("ID token failed validation")?;
        let claims = data.claims;

        if let Some(expected) = expected_nonce {
            match claims.nonce.as_deref() {
                Some(actual) if actual == expected => {}
                _ => return Err(anyhow!("ID token nonce does not match the expected value")),
            }
        }

        Ok(VerifiedIdentity { sub: claims.sub, email: claims.email, name: claims.name })
    }

    /// Get a decoding key by `kid`, refetching the JWKS once if we don't have it
    /// cached (handles key rotation without a restart).
    async fn key_for_kid(&self, kid: &str) -> Result<DecodingKey> {
        if let Some(key) = self.keys.read().await.get(kid).cloned() {
            return Ok(key);
        }
        self.refresh_jwks().await?;
        self.keys
            .read()
            .await
            .get(kid)
            .cloned()
            .ok_or_else(|| anyhow!("no JWKS key for kid `{kid}` after refresh"))
    }

    /// Fetch and cache the JWKS. Only swaps the cache on full success.
    async fn refresh_jwks(&self) -> Result<()> {
        let jwks: Jwks = self
            .http
            .get(&self.jwks_uri)
            .send()
            .await
            .context("fetch JWKS")?
            .error_for_status()
            .context("JWKS endpoint returned an error status")?
            .json()
            .await
            .context("parse JWKS JSON")?;

        let mut map = HashMap::new();
        for jwk in jwks.keys {
            match DecodingKey::from_rsa_components(&jwk.n, &jwk.e) {
                Ok(key) => {
                    map.insert(jwk.kid, key);
                }
                Err(e) => tracing::warn!("skipping malformed JWKS key {}: {}", jwk.kid, e),
            }
        }
        if map.is_empty() {
            return Err(anyhow!("JWKS contained no usable RSA keys"));
        }
        *self.keys.write().await = map;
        Ok(())
    }
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}
