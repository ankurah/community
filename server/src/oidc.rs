//! Validation of idp.to OIDC ID tokens (the "federate" half of
//! federate-and-remint). We verify the RS256 signature against idp.to's JWKS,
//! then the OIDC Core §3.1.3.7 claim set: `iss` and `aud` present and correct,
//! every audience entry our own client_id (we trust no other party's
//! audience), `exp` unexpired, `nbf` respected, a present `azp` naming us,
//! `iat` not in the future, and the `nonce` when the client supplies it.
//! Only then is the extracted identity handed to the mint step in `main.rs`.
//!
//! This is deliberately *not* `ankurah_jwt_auth` — that crate verifies a single
//! local PEM (our own minting key). idp.to publishes a rotating JWKS keyed by
//! `kid`, so we validate its tokens with `jsonwebtoken` and only then mint an
//! ankurah session token signed with our own `SigningKeys`.

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use serde::Deserialize;
use tokio::sync::RwLock;

/// Clock skew tolerated on every time-based claim — `exp`, `nbf`, and our own
/// `iat`-not-in-the-future check. One minute is the usual allowance for a
/// client and the IdP disagreeing about the wall clock.
const CLOCK_SKEW_SECS: u64 = 60;

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
    /// Role keys asserted by the token's REQUIRED `roles` claim — verification
    /// fails outright when the claim is absent or malformed, so this is always
    /// the IdP's explicit assertion (possibly empty). idp.to owns user↔role
    /// management; these keys are resolved into the minted session token's
    /// roles (see `resolve_roles` in `main.rs`, which normalizes and applies
    /// the `member` floor).
    pub roles: Vec<String>,
}

/// Only the claims we read. `jsonwebtoken` validates `iss`/`aud`/`exp`/`nbf`
/// (presence and value) via `Validation`, so those are here only where we
/// ALSO inspect them ourselves — `aud` to enforce the audience-trust rule,
/// which `jsonwebtoken` does not cover.
#[derive(Debug, Deserialize)]
struct IdTokenClaims {
    sub: String,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    nonce: Option<String>,
    /// Audience, read back raw to apply the trust rule `jsonwebtoken` leaves
    /// uncovered: it enforces only that `aud` is present and CONTAINS our
    /// client_id, while §3.1.3.7 (3) also rejects audiences the client does
    /// not trust — and we trust no audience but ourselves (see
    /// [`check_audience_trust`]).
    #[serde(default)]
    aud: Option<serde_json::Value>,
    /// Authorized party. When present it must be a string naming us
    /// (§3.1.3.7 (5)); any other present shape — `null` included — is a
    /// malformed claim and invalidates the token, the same
    /// wrong-type-must-reject rule the `exp`/`nbf` handling follows. Captured
    /// presence-aware ([`present`]) for exactly the `null` case: serde's
    /// plain `Option` folds an explicit `null` into "absent", while every
    /// other wrong-typed shape already arrives as a value. The old
    /// multi-audience-requires-`azp` rule is gone on spec grounds — see
    /// [`check_audience_trust`].
    #[serde(default, deserialize_with = "present")]
    azp: Option<serde_json::Value>,
    /// Issued-at, sanity-checked against the clock: a token stamped in the
    /// future is malformed or from a badly-skewed issuer. See
    /// [`check_issued_at`].
    #[serde(default)]
    iat: Option<i64>,
    /// Per-Application `roles` claim: a JSON array of stable lowercase role
    /// keys (e.g. `["member","moderator"]`), gated by the idp.to `roles`
    /// scope. Captured as a raw `Value` — not `Vec<String>` — so token PARSING
    /// tolerates any shape; `extract_roles` then strictly validates it and
    /// rejects the sign-in with a purposeful error (a well-formed roles array
    /// is REQUIRED — absent/malformed fails verification). Presence-aware
    /// ([`present`]) like `azp`, so an explicit `"roles": null` is reported
    /// as "not an array" rather than as a missing claim or scope — the
    /// message an operator would otherwise chase into the IdP's scope
    /// config for a claim that is in fact present.
    #[serde(default, deserialize_with = "present")]
    roles: Option<serde_json::Value>,
}

/// Keeps a present-but-`null` claim distinct from an absent one: serde's
/// plain `Option` deserializes JSON `null` to `None`, indistinguishable from
/// the field not appearing at all. Mapping whatever value IS present into
/// `Some` preserves the difference, so validation can reject the wrong-typed
/// claim rather than skip it.
fn present<'de, D>(deserializer: D) -> Result<Option<serde_json::Value>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    serde::Deserialize::deserialize(deserializer).map(Some)
}

/// Pull the REQUIRED `roles` claim into a `Vec<String>`. Strict by design:
/// role authority lives in the IdP, so an id_token without a well-formed
/// roles array (array of strings, possibly empty) is a broken contract and
/// fails verification rather than minting a role-less session. Fresh
/// sign-ins therefore fail until idp.to releases the `roles` claim for this
/// Application — and start succeeding, with no change on our side, the
/// moment that release lands. Normalization (trim, lowercase, dedup) and the
/// `member` floor happen later, at mint.
fn extract_roles(claim: Option<&serde_json::Value>) -> Result<Vec<String>> {
    let value = claim.ok_or_else(|| {
        anyhow!("id_token has no `roles` claim (idp roles not yet released, or `roles` scope not requested)")
    })?;
    let arr = value
        .as_array()
        .ok_or_else(|| anyhow!("id_token `roles` claim is not an array: {value}"))?;
    arr.iter()
        .map(|entry| {
            entry
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| anyhow!("id_token `roles` claim has a non-string entry: {value}"))
        })
        .collect()
}

/// OIDC Core §3.1.3.7 (3)-(5), the audience-trust guard `jsonwebtoken` does
/// not cover. `jsonwebtoken` has already established that `aud` CONTAINS our
/// client_id; §3.1.3.7 (3) further says to reject a token carrying audiences
/// the client does not trust, and community trusts no audience but itself —
/// so every entry must be our client_id (a token minted for some other party,
/// merely listing us among its audiences, cannot be replayed into our flow).
/// The old multi-audience-requires-`azp` rule is dropped on spec grounds,
/// not merely because self-trust made it unreachable: §3.1.3.7 (4) ties
/// that check to extensions that define an authorized party, and none is in
/// use here — so widening trust to a second audience someday must not
/// resurrect it. What remains is item (5)'s check on a present `azp`: it
/// must be a string naming us (§2 defines `azp` as a string value), and any
/// other present shape — `null` included — refuses the token as wrong-typed
/// rather than being read as absent.
fn check_audience_trust(aud: Option<&serde_json::Value>, azp: Option<&serde_json::Value>, client_id: &str) -> Result<()> {
    match azp {
        None => {}
        Some(serde_json::Value::String(party)) if party == client_id => {}
        Some(serde_json::Value::String(_)) => {
            return Err(anyhow!("id_token `azp` names a different authorized party"));
        }
        Some(_) => return Err(anyhow!("id_token `azp` is present but is not a string")),
    }
    let all_trusted = match aud {
        Some(serde_json::Value::String(single)) => single == client_id,
        Some(serde_json::Value::Array(entries)) => {
            !entries.is_empty() && entries.iter().all(|entry| entry.as_str() == Some(client_id))
        }
        _ => false,
    };
    if !all_trusted {
        return Err(anyhow!("id_token lists an audience other than this client"));
    }
    Ok(())
}

/// `iat` must be present and not meaningfully in the future — a future stamp
/// means a malformed token or a badly-skewed issuer. A past `iat` is normal
/// (tokens age within their `exp`), so no lower bound is imposed here: `exp`,
/// which `jsonwebtoken` requires and validates, is what bounds token life.
fn check_issued_at(iat: Option<i64>, now: i64, leeway: i64) -> Result<()> {
    let iat = iat.ok_or_else(|| anyhow!("id_token has no `iat` claim"))?;
    if iat > now + leeway {
        return Err(anyhow!("id_token `iat` is in the future"));
    }
    Ok(())
}

fn now_unix_secs() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
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
        Self::new(
            env_or("OIDC_ISSUER", DEFAULT_ISSUER),
            env_or("OIDC_CLIENT_ID", DEFAULT_CLIENT_ID),
            env_or("OIDC_JWKS_URI", DEFAULT_JWKS_URI),
        )
    }

    /// Construct with explicit config. `from_env` is the production path; tests
    /// use this to point the verifier at a locally-generated key instead of
    /// idp.to's JWKS.
    pub(crate) fn new(issuer: String, client_id: String, jwks_uri: String) -> Self {
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
        self.verify_with_key(id_token, &key, expected_nonce)
    }

    /// The claim-validation half, given the decoding key already resolved —
    /// the seam `verify` reaches after the JWKS fetch, and the one the tests
    /// drive against a locally-generated key. Everything OIDC Core §3.1.3.7
    /// asks of an id_token happens here.
    fn verify_with_key(&self, id_token: &str, key: &DecodingKey, expected_nonce: Option<&str>) -> Result<VerifiedIdentity> {
        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_issuer(&[self.issuer.as_str()]);
        validation.set_audience(&[self.client_id.as_str()]);
        // Require these PRESENT, not merely valid-when-present: jsonwebtoken
        // defaults to requiring `exp` alone, which would admit a token that
        // simply omits `iss` or `aud`. An OIDC id_token must carry both.
        validation.set_required_spec_claims(&["exp", "iss", "aud"]);
        // Honor `nbf` (off by default): a token is not usable before its
        // not-before time. Optional in OIDC, so this checks it only when
        // present rather than requiring it.
        validation.validate_nbf = true;
        // One explicit skew for every time-based check.
        validation.leeway = CLOCK_SKEW_SECS;

        let data = decode::<IdTokenClaims>(id_token, key, &validation).context("ID token failed validation")?;
        let claims = data.claims;

        check_audience_trust(claims.aud.as_ref(), claims.azp.as_ref(), &self.client_id)?;
        check_issued_at(claims.iat, now_unix_secs(), CLOCK_SKEW_SECS as i64)?;

        if let Some(expected) = expected_nonce {
            match claims.nonce.as_deref() {
                Some(actual) if actual == expected => {}
                _ => return Err(anyhow!("ID token nonce does not match the expected value")),
            }
        }

        let roles = extract_roles(claims.roles.as_ref())?;
        Ok(VerifiedIdentity { sub: claims.sub, email: claims.email, name: claims.name, roles })
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extract_roles_present() {
        let value = json!(["member", "moderator"]);
        assert_eq!(
            extract_roles(Some(&value)).unwrap(),
            vec!["member".to_string(), "moderator".to_string()]
        );
    }

    #[test]
    fn extract_roles_empty_array_is_ok() {
        // Present-but-empty is a valid claim shape: the user simply has no
        // roles; the `member` floor is applied at mint.
        assert!(extract_roles(Some(&json!([]))).unwrap().is_empty());
    }

    #[test]
    fn extract_roles_absent_is_rejected() {
        assert!(extract_roles(None).is_err());
        // An explicit JSON null is not a roles array either.
        assert!(extract_roles(Some(&serde_json::Value::Null)).is_err());
    }

    #[test]
    fn extract_roles_wrong_type_is_rejected() {
        assert!(extract_roles(Some(&json!("moderator"))).is_err());
        assert!(extract_roles(Some(&json!({ "member": true }))).is_err());
        assert!(extract_roles(Some(&json!(42))).is_err());
    }

    #[test]
    fn extract_roles_rejects_non_string_array_entries() {
        let value = json!(["member", 7, null, "moderator", { "x": 1 }]);
        assert!(extract_roles(Some(&value)).is_err());
    }

    #[test]
    fn id_token_claims_deserialize_without_roles() {
        // A token with no `roles` claim still PARSES (roles is a raw Value
        // capture) — verification then rejects it in extract_roles, which is
        // where the useful error message lives.
        let claims: IdTokenClaims = serde_json::from_value(json!({
            "sub": "idp-sub-123",
            "email": "a@example.com",
            "name": "A"
        }))
        .expect("token without roles must still parse");
        assert_eq!(claims.sub, "idp-sub-123");
        assert!(extract_roles(claims.roles.as_ref()).is_err());
    }

    #[test]
    fn id_token_claims_deserialize_with_malformed_roles() {
        // A present-but-malformed `roles` claim must not fail token
        // deserialization; it fails extraction with a clear error.
        let claims: IdTokenClaims = serde_json::from_value(json!({
            "sub": "idp-sub-123",
            "roles": "moderator"
        }))
        .expect("malformed roles claim must not fail token parsing");
        assert!(extract_roles(claims.roles.as_ref()).is_err());
    }

    #[test]
    fn id_token_claims_deserialize_with_roles_array() {
        let claims: IdTokenClaims = serde_json::from_value(json!({
            "sub": "idp-sub-123",
            "roles": ["member", "moderator"]
        }))
        .expect("well-formed roles claim parses");
        assert_eq!(
            extract_roles(claims.roles.as_ref()).unwrap(),
            vec!["member".to_string(), "moderator".to_string()]
        );
    }

    // ---- audience-trust guard (OIDC §3.1.3.7 (3)-(5)) ----------------------

    #[test]
    fn audience_trust_matrix() {
        let me = "client-abc";
        // Sole audience is us, no azp: fine (string or one-element array).
        assert!(check_audience_trust(Some(&json!("client-abc")), None, me).is_ok());
        assert!(check_audience_trust(Some(&json!(["client-abc"])), None, me).is_ok());
        // A present azp must be a string naming us; when it is, a
        // sole-audience token passes.
        assert!(check_audience_trust(Some(&json!("client-abc")), Some(&json!("client-abc")), me).is_ok());
        assert!(check_audience_trust(Some(&json!("client-abc")), Some(&json!("other")), me).is_err());
        // A present-but-wrong-typed azp is malformed, not absent: refused
        // (`null` is the shape serde's plain Option would have swallowed).
        assert!(check_audience_trust(Some(&json!("client-abc")), Some(&json!(null)), me).is_err());
        assert!(check_audience_trust(Some(&json!("client-abc")), Some(&json!(42)), me).is_err());
        assert!(check_audience_trust(Some(&json!("client-abc")), Some(&json!(["client-abc"])), me).is_err());
        // Any audience entry that is not us: refused, whatever the azp says —
        // we trust no other audience (§3.1.3.7 (3)).
        assert!(check_audience_trust(Some(&json!(["client-abc", "other"])), None, me).is_err());
        assert!(check_audience_trust(Some(&json!(["client-abc", "other"])), Some(&json!("client-abc")), me).is_err());
        assert!(check_audience_trust(Some(&json!(["client-abc", "other"])), Some(&json!("other")), me).is_err());
        // A degenerate repeat of us alone still lists only trusted parties.
        assert!(check_audience_trust(Some(&json!(["client-abc", "client-abc"])), None, me).is_ok());
        // Malformed audience shapes: refused.
        assert!(check_audience_trust(Some(&json!([])), None, me).is_err());
        assert!(check_audience_trust(Some(&json!(["client-abc", 7])), None, me).is_err());
        assert!(check_audience_trust(Some(&json!(42)), None, me).is_err());
        assert!(check_audience_trust(None, None, me).is_err());
    }

    #[test]
    fn iat_guard() {
        assert!(check_issued_at(Some(100), 100, 60).is_ok(), "iat == now");
        assert!(check_issued_at(Some(40), 100, 60).is_ok(), "past iat is normal");
        assert!(check_issued_at(Some(160), 100, 60).is_ok(), "at the leeway boundary");
        assert!(check_issued_at(Some(161), 100, 60).is_err(), "beyond leeway into the future");
        assert!(check_issued_at(None, 100, 60).is_err(), "absent iat");
    }

    // ---- end-to-end token validation against a locally-generated key -------
    //
    // A runtime-generated RSA keypair (see `test_pems`) stands in for idp.to's
    // JWKS: we sign tokens with the private half and drive `verify_with_key`
    // with a `DecodingKey` from the public half, so the whole §3.1.3.7 check
    // runs with no network.

    use jsonwebtoken::{encode, EncodingKey, Header};
    use std::sync::OnceLock;

    const TEST_ISS: &str = "https://issuer.test";
    const TEST_AUD: &str = "client-abc";

    /// A throwaway RSA keypair, minted once per test run rather than committed:
    /// this repo's `.gitignore` forbids key material in the tree, so the test
    /// fixtures are generated at runtime instead. Returns the (private, public)
    /// PEM pair that `sign` and `test_key` share.
    fn test_pems() -> &'static (String, String) {
        static PEMS: OnceLock<(String, String)> = OnceLock::new();
        PEMS.get_or_init(|| {
            let keys = ankurah_jwt_auth::SigningKeys::generate().expect("generate test signing keys");
            (keys.private_key_pem().expect("test private pem"), keys.public_key_pem().expect("test public pem"))
        })
    }

    fn test_verifier() -> OidcVerifier {
        OidcVerifier::new(TEST_ISS.to_string(), TEST_AUD.to_string(), "unused".to_string())
    }

    fn test_key() -> DecodingKey {
        DecodingKey::from_rsa_pem(test_pems().1.as_bytes()).expect("test public key parses")
    }

    fn sign(claims: serde_json::Value) -> String {
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some("test-kid".to_string());
        encode(&header, &claims, &EncodingKey::from_rsa_pem(test_pems().0.as_bytes()).expect("test private key parses"))
            .expect("test token signs")
    }

    fn base_claims() -> serde_json::Value {
        let now = now_unix_secs();
        json!({
            "sub": "idp-sub-1",
            "iss": TEST_ISS,
            "aud": TEST_AUD,
            "exp": now + 3600,
            "iat": now,
            "nonce": "n-1",
            "roles": ["member"],
        })
    }

    fn without(mut claims: serde_json::Value, key: &str) -> serde_json::Value {
        claims.as_object_mut().unwrap().remove(key);
        claims
    }

    fn with(mut claims: serde_json::Value, key: &str, value: serde_json::Value) -> serde_json::Value {
        claims[key] = value;
        claims
    }

    #[test]
    fn valid_token_accepted() {
        let identity = test_verifier().verify_with_key(&sign(base_claims()), &test_key(), Some("n-1")).unwrap();
        assert_eq!(identity.sub, "idp-sub-1");
        assert_eq!(identity.roles, vec!["member".to_string()]);
    }

    #[test]
    fn wrong_key_signature_rejected() {
        // Signed with a second, unrelated keypair: the signature gate — not
        // any claim check — must refuse it, however valid the claims read.
        let other = ankurah_jwt_auth::SigningKeys::generate().expect("generate second test keypair");
        let other_private = other.private_key_pem().expect("second private pem");
        let other_key = EncodingKey::from_rsa_pem(other_private.as_bytes()).expect("second private key parses");
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some("test-kid".to_string());
        let token = encode(&header, &base_claims(), &other_key).expect("second-key token signs");
        assert!(test_verifier().verify_with_key(&token, &test_key(), None).is_err());
    }

    #[test]
    fn wrong_algorithm_rejected() {
        // The classic RS256→HS256 confusion shape: an HS256 token HMAC'd
        // over the very public-key bytes the verifier holds. Refused
        // outright — the verifier pins RS256, so no symmetric-key route
        // exists regardless of the claims carried.
        let mut header = Header::new(Algorithm::HS256);
        header.kid = Some("test-kid".to_string());
        let token = encode(&header, &base_claims(), &EncodingKey::from_secret(test_pems().1.as_bytes()))
            .expect("HS256 token signs");
        assert!(test_verifier().verify_with_key(&token, &test_key(), None).is_err());
    }

    #[test]
    fn missing_iss_rejected() {
        assert!(test_verifier().verify_with_key(&sign(without(base_claims(), "iss")), &test_key(), None).is_err());
    }

    #[test]
    fn missing_aud_rejected() {
        assert!(test_verifier().verify_with_key(&sign(without(base_claims(), "aud")), &test_key(), None).is_err());
    }

    #[test]
    fn wrong_iss_rejected() {
        let token = sign(with(base_claims(), "iss", json!("https://evil.test")));
        assert!(test_verifier().verify_with_key(&token, &test_key(), None).is_err());
    }

    #[test]
    fn foreign_single_aud_rejected() {
        let token = sign(with(base_claims(), "aud", json!("someone-else")));
        assert!(test_verifier().verify_with_key(&token, &test_key(), None).is_err());
    }

    #[test]
    fn multi_aud_with_matching_azp_rejected() {
        // Flipped from `..._accepted` by the audience-trust tightening:
        // §3.1.3.7 (3) says to reject audiences the client does not trust,
        // and we trust only ourselves — a matching `azp` no longer admits
        // the extra audience.
        let claims = with(with(base_claims(), "aud", json!([TEST_AUD, "other"])), "azp", json!(TEST_AUD));
        assert!(test_verifier().verify_with_key(&sign(claims), &test_key(), Some("n-1")).is_err());
    }

    #[test]
    fn null_roles_rejected() {
        // An explicit `"roles": null` must refuse the token AND say why
        // accurately: presence-aware capture routes it to extract_roles's
        // "not an array" arm instead of the missing-claim/scope message.
        let claims = with(base_claims(), "roles", json!(null));
        let err = match test_verifier().verify_with_key(&sign(claims), &test_key(), Some("n-1")) {
            Err(err) => err,
            Ok(_) => panic!("null roles must refuse the token"),
        };
        assert!(err.to_string().contains("not an array"), "got: {err}");
    }

    #[test]
    fn null_azp_rejected() {
        // An explicit `"azp": null` is a malformed claim and must refuse the
        // token — presence-aware capture keeps it from being read as absent
        // (the wrong-type-must-reject rule `exp`/`nbf` already follow).
        let claims = with(base_claims(), "azp", json!(null));
        assert!(test_verifier().verify_with_key(&sign(claims), &test_key(), Some("n-1")).is_err());
    }

    #[test]
    fn multi_aud_with_foreign_azp_rejected() {
        let claims = with(with(base_claims(), "aud", json!([TEST_AUD, "other"])), "azp", json!("other"));
        assert!(test_verifier().verify_with_key(&sign(claims), &test_key(), None).is_err());
    }

    #[test]
    fn multi_aud_without_azp_rejected() {
        let claims = with(base_claims(), "aud", json!([TEST_AUD, "other"]));
        assert!(test_verifier().verify_with_key(&sign(claims), &test_key(), None).is_err());
    }

    #[test]
    fn future_nbf_rejected() {
        let token = sign(with(base_claims(), "nbf", json!(now_unix_secs() + 3600)));
        assert!(test_verifier().verify_with_key(&token, &test_key(), None).is_err());
    }

    #[test]
    fn past_nbf_accepted() {
        let token = sign(with(base_claims(), "nbf", json!(now_unix_secs() - 60)));
        assert!(test_verifier().verify_with_key(&token, &test_key(), Some("n-1")).is_ok());
    }

    #[test]
    fn string_nbf_rejected() {
        // jsonwebtoken before 10.3.0 classified a wrong-JSON-typed `nbf` as
        // absent (CVE-2026-25537), silently skipping the not-before check for
        // exactly the malformed tokens it should catch; 10.3+ rejects the
        // token instead. This pins the rejection.
        let token = sign(with(base_claims(), "nbf", json!("99999999999")));
        assert!(test_verifier().verify_with_key(&token, &test_key(), None).is_err());
    }

    #[test]
    fn string_exp_rejected() {
        // A string `exp` must also invalidate the token — though unlike the
        // `nbf` case above, this never regressed: pre-10.3 the wrong-typed
        // value was classified absent, and `exp` sits in
        // `required_spec_claims`, so the token failed on missing-`exp`
        // instead. Pinned so the rejection survives either mechanism.
        let token = sign(with(base_claims(), "exp", json!("99999999999")));
        assert!(test_verifier().verify_with_key(&token, &test_key(), None).is_err());
    }

    #[test]
    fn future_iat_rejected() {
        let token = sign(with(base_claims(), "iat", json!(now_unix_secs() + 3600)));
        assert!(test_verifier().verify_with_key(&token, &test_key(), None).is_err());
    }

    #[test]
    fn missing_iat_rejected() {
        assert!(test_verifier().verify_with_key(&sign(without(base_claims(), "iat")), &test_key(), None).is_err());
    }

    #[test]
    fn expired_token_rejected() {
        let token = sign(with(base_claims(), "exp", json!(now_unix_secs() - 3600)));
        assert!(test_verifier().verify_with_key(&token, &test_key(), None).is_err());
    }

    #[test]
    fn nonce_mismatch_rejected() {
        assert!(test_verifier().verify_with_key(&sign(base_claims()), &test_key(), Some("wrong")).is_err());
    }
}
