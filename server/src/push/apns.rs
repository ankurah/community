//! Talking to Apple Push Notification service: one alert, one HTTP/2 request,
//! and what Apple's answer means for the device it was addressed to.
//!
//! FOR: the sender worker knows which member to reach and what to say about the
//! event; this module is everything between that and a banner on a phone. It is
//! its own file because the two halves fail differently — a worker defect says
//! the wrong sentence, an APNs defect says nothing at all — and because
//! [`Transport`] is the seam that lets the worker be exercised with no network
//! under it.
//!
//! WHY NO APNs CRATE. Every piece the provider API needs is already in this
//! build. `jsonwebtoken` — the OIDC verifier's, on its `rust_crypto` backend —
//! signs the ES256 provider token, and `reqwest` — the unfurl worker's, already
//! rustls-only — speaks HTTP/2 once its `http2` feature is on. A dedicated
//! crate would bring a second HTTP stack and a second crypto stack alongside
//! those, to save about a hundred lines of request building.
//!
//! CONFIGURATION is four environment variables plus an optional fifth, and the
//! four are all-or-nothing: [`from_env`] answers [`FromEnv::Absent`] when none
//! of them is set, which is what leaves the sender dormant on a deployment that
//! has no Apple credentials yet. See [`REQUIRED_VARS`].
//!
//! WHAT NEVER REACHES A LOG. A device token is the credential for waking
//! someone's phone and the provider token is the credential for sending to
//! every device this app has. So the request URL — which carries the device
//! token in its path — is stripped out of any transport error before it can
//! travel into a log line, and neither the key nor the signed provider token is
//! ever formatted anywhere. `store::token_prefix` is how a device is named in
//! prose.

use std::sync::Mutex;
use std::time::Duration;

use anyhow::{anyhow, Context as _, Result};
use futures_util::future::BoxFuture;
use futures_util::FutureExt;
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use serde::Serialize;
use serde_json::{json, Map, Value};

/// Apple's production host. The default because a deployment that has APNs
/// credentials at all is a deployment shipping to real phones; the sandbox host
/// and a local stand-in are both reached by setting [`ENDPOINT_VAR`].
pub const DEFAULT_ENDPOINT: &str = "https://api.push.apple.com";

/// The key material itself, not a path to it: this server reads its other
/// secret (`ANKURAH_JWT_SIGNING_KEY`) the same way, because Cloud Run mounts
/// secrets as environment values and nothing should write a private key to the
/// container's disk on the way in.
pub const KEY_P8_VAR: &str = "APNS_KEY_P8";
/// The ten-character key identifier Apple issued alongside the .p8, which
/// travels in the provider token's `kid` header so Apple knows which of a
/// team's keys signed it.
pub const KEY_ID_VAR: &str = "APNS_KEY_ID";
/// The Apple Developer team, which is the provider token's `iss`.
pub const TEAM_ID_VAR: &str = "APNS_TEAM_ID";
/// The app the alert is for — this project's bundle identifier,
/// `org.ankurah.community`. Required rather than defaulted: a bundle id that
/// silently disagrees with the app's own turns every send into a `BadTopic`
/// refusal with nothing in the configuration to point at.
pub const TOPIC_VAR: &str = "APNS_TOPIC";
/// The four that must be set together, in the order a boot line names them.
pub const REQUIRED_VARS: [&str; 4] = [KEY_P8_VAR, KEY_ID_VAR, TEAM_ID_VAR, TOPIC_VAR];
/// Where to send. Optional — [`DEFAULT_ENDPOINT`] otherwise — and the seam a
/// development deployment points at Apple's sandbox host.
pub const ENDPOINT_VAR: &str = "APNS_ENDPOINT";

/// Everything one deployment needs to send as itself.
///
/// Deliberately NOT `Debug`: it holds a private key, and a derived `Debug` is
/// how key material ends up in a panic message.
pub struct Config {
    /// The PKCS#8 PEM Apple issued (a `.p8` file's contents, `BEGIN PRIVATE
    /// KEY` and all).
    pub key_p8: String,
    pub key_id: String,
    pub team_id: String,
    pub topic: String,
    pub endpoint: String,
}

/// What the environment said about push, so the caller can log one honest line
/// for each case rather than treating "not set up" as a failure.
pub enum FromEnv {
    Ready(Config),
    /// None of [`REQUIRED_VARS`] is set. Not an error: a deployment without
    /// Apple credentials runs the rest of the server exactly as before.
    Absent,
    /// Some are set and some are not, which is a deployment that meant to
    /// configure push and got it half-done. Sending is impossible either way,
    /// but this case deserves a louder line than [`FromEnv::Absent`].
    Incomplete { missing: Vec<&'static str> },
}

/// Read the configuration out of the process environment.
pub fn from_env() -> FromEnv { from_vars(|name| std::env::var(name).ok()) }

/// The same reading over any lookup, which is what lets the all-or-nothing rule
/// be tested without a test mutating the process environment underneath its
/// neighbours.
pub(crate) fn from_vars(get: impl Fn(&str) -> Option<String>) -> FromEnv {
    // An empty or whitespace-only value is an unset one. Deployment tooling
    // writes empty strings for absent secrets, and a Config carrying "" would
    // fail at the first send instead of at boot.
    let read = |name: &str| get(name).map(|value| value.trim().to_string()).filter(|value| !value.is_empty());
    let (key_p8, key_id, team_id, topic) = (read(KEY_P8_VAR), read(KEY_ID_VAR), read(TEAM_ID_VAR), read(TOPIC_VAR));
    let missing: Vec<&'static str> =
        [(KEY_P8_VAR, key_p8.is_some()), (KEY_ID_VAR, key_id.is_some()), (TEAM_ID_VAR, team_id.is_some()), (TOPIC_VAR, topic.is_some())]
            .into_iter()
            .filter(|(_, present)| !present)
            .map(|(name, _)| name)
            .collect();
    match (key_p8, key_id, team_id, topic) {
        (Some(key_p8), Some(key_id), Some(team_id), Some(topic)) => FromEnv::Ready(Config {
            key_p8,
            key_id,
            team_id,
            topic,
            endpoint: read(ENDPOINT_VAR).unwrap_or_else(|| DEFAULT_ENDPOINT.to_string()),
        }),
        _ if missing.len() == REQUIRED_VARS.len() => FromEnv::Absent,
        _ => FromEnv::Incomplete { missing },
    }
}

/// The member of the JSON document that carries [`Target`], beside Apple's own
/// `aps`. Named for the app because that is what Apple's convention asks: every
/// key outside `aps` belongs to the app that receives it.
pub const TARGET_KEY: &str = "community";

/// Where tapping the alert should land, for the client leg that will read it.
///
/// FOR: the alert says a thing happened somewhere, and the app that opens has
/// to get to that somewhere without guessing from the text. These are the same
/// ids the in-app inbox row deep-links from
/// (`leptos-app/src/notification_inbox.rs`): a room-bearing notification opens
/// its room, and a `dm` one opens the conversation with `actor`, resolved
/// through the same find-or-create the member card's "Message" button uses.
///
/// `notification` is here so the client can mark the inbox row seen when the
/// member acts on the alert rather than leaving a dot behind.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Target {
    /// `Notification.kind`, verbatim — `mention`, `dm`, or a kind a later
    /// server minted and this client predates.
    pub kind: String,
    /// The `Notification` entity id, base64.
    pub notification: String,
    /// The `Room` entity id, when the event happened in one.
    pub room: Option<String>,
    /// The `Message` entity id, when the notification names one.
    pub message: Option<String>,
    /// The `User` entity id of whoever caused it — and, for a `dm`, the
    /// deep-link target, because `Notification` has no typed slot a DM thread
    /// fits in.
    pub actor: Option<String>,
}

/// One visible alert: what it says, which banner it replaces, and where it
/// leads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Alert {
    /// The bold first line — the place the event happened.
    pub title: String,
    /// The sentence under it — who did what.
    pub body: String,
    /// The `apns-collapse-id` header. See `workers::push::collapse_id` for what
    /// it is keyed on and, just as importantly, what it is not.
    pub collapse_id: String,
    pub target: Target,
}

impl Alert {
    /// The JSON document APNs delivers: Apple's `aps` for the parts iOS itself
    /// renders, and [`TARGET_KEY`] for the parts only this app understands.
    ///
    /// Built by hand rather than by `Serialize` so the shape a client will read
    /// is stated in one place, and so absent ids are absent members rather than
    /// nulls the client has to distinguish from missing ones.
    ///
    /// NO BADGE NUMBER, deliberately. A badge count means an unread total,
    /// which means asking what this member has already seen — the awareness
    /// this generation set aside (see `crate::push`). The alert is a knock on
    /// the door; the inbox keeps the count.
    pub fn payload(&self) -> Value {
        let mut target = Map::new();
        target.insert("kind".to_string(), Value::String(self.target.kind.clone()));
        target.insert("notification".to_string(), Value::String(self.target.notification.clone()));
        for (name, value) in [("room", &self.target.room), ("message", &self.target.message), ("actor", &self.target.actor)] {
            if let Some(value) = value {
                target.insert(name.to_string(), Value::String(value.clone()));
            }
        }

        let mut root = Map::new();
        root.insert("aps".to_string(), json!({ "alert": { "title": self.title, "body": self.body }, "sound": "default" }));
        root.insert(TARGET_KEY.to_string(), Value::Object(target));
        Value::Object(root)
    }
}

/// What APNs said about one send.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// APNs took it. Not a promise it was shown — the phone may be off — but
    /// the last word this server gets.
    Delivered,
    /// APNs will never accept this device token again, so the registry row for
    /// it is dead weight and the caller drops it. `reason` is Apple's own word,
    /// carried for the log line.
    TokenGone { reason: &'static str },
    /// APNs refused for some other stated reason. The alert is lost; the
    /// registration stands.
    Refused { status: u16, reason: String },
}

/// Read Apple's answer to one send.
///
/// THE TWO REFUSALS THAT MEAN A DEVICE IS GONE, and no others. `410
/// Unregistered` is Apple reporting that the app is no longer installed on that
/// device, and `400 BadDeviceToken` that the token is not one APNs issued for
/// this environment. Both are permanent for that row.
///
/// `DeviceTokenNotForTopic` reads like a third and must not be treated as one:
/// it says the token and the configured topic disagree, which a wrong
/// [`TOPIC_VAR`] produces for EVERY device at once. Pruning on it would empty
/// the registry on a typo, and the members would have to reinstall to get back
/// in it.
pub(crate) fn classify(status: u16, body: &str) -> Outcome {
    if status == 200 {
        return Outcome::Delivered;
    }
    // Apple's error body is `{"reason": "..."}`; anything else (a proxy's HTML
    // error page, an empty body) leaves the reason blank rather than failing,
    // because the status alone is still worth logging.
    let reason = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|body| body.get("reason").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_default();
    match (status, reason.as_str()) {
        (410, "Unregistered") => Outcome::TokenGone { reason: "Unregistered" },
        (400, "BadDeviceToken") => Outcome::TokenGone { reason: "BadDeviceToken" },
        _ => Outcome::Refused { status, reason },
    }
}

/// The one thing the sender worker needs from Apple, as a trait so a test can
/// stand in for Apple.
///
/// `BoxFuture` rather than `async fn` in the trait, matching
/// `push::store::DeviceTokens` and `workers::supervise`: this repo boxes futures
/// at its dynamic-dispatch seams.
pub trait Transport: Send + Sync + 'static {
    fn send<'a>(&'a self, device_token: &'a str, alert: &'a Alert) -> BoxFuture<'a, Result<Outcome>>;
}

/// How long one provider token is reused before a fresh one is signed.
///
/// Apple accepts a provider token for an hour and refuses a replacement minted
/// less than twenty minutes after the last one
/// (`TooManyProviderTokenUpdates`), so the usable refresh window is between
/// those two numbers. Fifty minutes sits inside both: far enough below the hour
/// that a request already in flight cannot age out of validity, and far enough
/// above twenty minutes that no burst of sends can mint two.
const PROVIDER_TOKEN_REUSE_MS: i64 = 50 * 60 * 1000;

/// What the provider token claims. Apple asks for exactly these two.
#[derive(Serialize)]
struct ProviderClaims<'a> {
    /// The team the signing key belongs to.
    iss: &'a str,
    /// Seconds since epoch. Apple measures the token's age from this, which is
    /// why it is the same number [`ProviderTokens`] counts the reuse window
    /// from.
    iat: i64,
}

/// The signed provider token, kept between sends.
///
/// FOR: every request to APNs carries one, and signing is both wasted work and
/// — past Apple's refresh floor — an outright refusal if done per request. So
/// one is signed, held, and handed out until it is nearly stale.
pub(crate) struct ProviderTokens {
    /// Parsed once, at construction, so a key that is not a PKCS#8 EC private
    /// key is a boot failure with a sentence rather than a mystery at the first
    /// send.
    key: EncodingKey,
    key_id: String,
    team_id: String,
    /// The token in hand and the instant it was signed, in ms since epoch —
    /// the project's timestamp unit, and a plain number so the reuse window can
    /// be tested without waiting fifty minutes.
    current: Mutex<Option<(String, i64)>>,
}

impl ProviderTokens {
    pub(crate) fn new(config: &Config) -> Result<Self> {
        let key = EncodingKey::from_ec_pem(config.key_p8.as_bytes())
            .map_err(|e| anyhow!("read {KEY_P8_VAR} as a PKCS#8 EC private key: {e}"))?;
        Ok(Self { key, key_id: config.key_id.clone(), team_id: config.team_id.clone(), current: Mutex::new(None) })
    }

    /// The token to present at `now_ms`, signing a fresh one only when the one
    /// in hand has reached [`PROVIDER_TOKEN_REUSE_MS`].
    pub(crate) fn at(&self, now_ms: i64) -> Result<String> {
        let mut current = self.current.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((token, minted_at)) = current.as_ref() {
            if now_ms.saturating_sub(*minted_at) < PROVIDER_TOKEN_REUSE_MS {
                return Ok(token.clone());
            }
        }
        let mut header = Header::new(Algorithm::ES256);
        header.kid = Some(self.key_id.clone());
        let token = encode(&header, &ProviderClaims { iss: &self.team_id, iat: now_ms / 1000 }, &self.key)
            .map_err(|e| anyhow!("sign the APNs provider token: {e}"))?;
        *current = Some((token.clone(), now_ms));
        Ok(token)
    }
}

/// How long one send may take before it is abandoned. APNs answers in
/// milliseconds when it is healthy; this is the ceiling that keeps a stalled
/// connection from parking the sender worker, which processes one notification
/// at a time.
const SEND_TIMEOUT: Duration = Duration::from_secs(10);

/// The real transport: direct HTTP/2 to Apple.
pub struct ApnsClient {
    /// No trailing slash, so the request path is built by one `format!`.
    endpoint: String,
    topic: String,
    tokens: ProviderTokens,
    http: reqwest::Client,
}

impl ApnsClient {
    pub fn new(config: Config) -> Result<Self> {
        let http = reqwest::Client::builder().timeout(SEND_TIMEOUT).build().context("build the APNs HTTP client")?;
        Ok(Self {
            endpoint: config.endpoint.trim_end_matches('/').to_string(),
            topic: config.topic.clone(),
            tokens: ProviderTokens::new(&config)?,
            http,
        })
    }
}

impl Transport for ApnsClient {
    fn send<'a>(&'a self, device_token: &'a str, alert: &'a Alert) -> BoxFuture<'a, Result<Outcome>> {
        async move {
            let provider_token = self.tokens.at(crate::workers::now_ms())?;
            let response = self
                .http
                .post(format!("{}/3/device/{device_token}", self.endpoint))
                // Lowercase `bearer` is what Apple's documentation spells, and
                // the scheme is case-insensitive either way (RFC 7235).
                .header(reqwest::header::AUTHORIZATION, format!("bearer {provider_token}"))
                .header("apns-topic", &self.topic)
                // A visible alert, sent now. `apns-push-type` is required on
                // iOS 13 and later and must agree with the document's `aps`;
                // priority 10 is what "wake the phone and show it" means.
                .header("apns-push-type", "alert")
                .header("apns-priority", "10")
                .header("apns-collapse-id", &alert.collapse_id)
                // No `apns-expiration`: leaving it off asks APNs to keep trying
                // for its own default window, which is what a phone that was in
                // a tunnel needs. Zero would mean deliver now or never.
                .json(&alert.payload())
                .send()
                .await
                // NEVER let the URL into the error: it carries the device token
                // in its path, and every caller of this method logs the error.
                // `without_url` is reqwest's own way to say so.
                .map_err(|e| anyhow!("send to APNs: {}", e.without_url()))?;
            let status = response.status().as_u16();
            // A body we cannot read is an empty reason, not a failed send: the
            // status is what decides the outcome.
            let body = response.text().await.unwrap_or_default();
            Ok(classify(status, &body))
        }
        .boxed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// A P-256 key made here and now, so no key material is ever committed —
    /// the rule the repo's `.gitignore` states and this module's own comments
    /// repeat. Returns the private PKCS#8 PEM (what Apple's `.p8` file holds)
    /// and the matching public PEM, for verifying what we signed.
    fn throwaway_key() -> (String, String) {
        use p256::pkcs8::{EncodePrivateKey, EncodePublicKey, LineEnding};
        let secret = p256::SecretKey::random(&mut rand_core::OsRng);
        let private = secret.to_pkcs8_pem(LineEnding::LF).expect("encode the throwaway key as PKCS#8").to_string();
        let public = secret.public_key().to_public_key_pem(LineEnding::LF).expect("encode the throwaway public key");
        (private, public)
    }

    fn config_with(key_p8: String) -> Config {
        Config {
            key_p8,
            key_id: "TESTKEYID1".to_string(),
            team_id: "TESTTEAMID".to_string(),
            topic: "org.ankurah.community".to_string(),
            endpoint: DEFAULT_ENDPOINT.to_string(),
        }
    }

    fn vars(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(name, value)| (name.to_string(), value.to_string())).collect()
    }

    fn read_from(vars: &HashMap<String, String>) -> FromEnv {
        from_vars(|name| vars.get(name).cloned())
    }

    #[test]
    fn an_environment_that_names_no_apns_credentials_leaves_push_unconfigured() {
        // The case every deployment without Apple credentials is in, and the
        // one that has to be quiet rather than an error: the caller logs one
        // line and the sender stays dormant while the registry keeps filing
        // tokens.
        assert!(matches!(read_from(&vars(&[])), FromEnv::Absent));
        // Set-but-empty is unset. Deployment tooling writes empty strings for
        // secrets it has no value for.
        let blank = vars(&[(KEY_P8_VAR, ""), (KEY_ID_VAR, "  "), (TEAM_ID_VAR, ""), (TOPIC_VAR, "")]);
        assert!(matches!(read_from(&blank), FromEnv::Absent));
        // An endpoint override on its own configures nothing.
        assert!(matches!(read_from(&vars(&[(ENDPOINT_VAR, "http://localhost:9000")])), FromEnv::Absent));
    }

    #[test]
    fn a_half_configured_environment_names_what_is_missing_rather_than_going_quiet() {
        let half = vars(&[(KEY_ID_VAR, "TESTKEYID1"), (TEAM_ID_VAR, "TESTTEAMID")]);
        match read_from(&half) {
            FromEnv::Incomplete { missing } => assert_eq!(missing, vec![KEY_P8_VAR, TOPIC_VAR]),
            _ => panic!("half-configured must not read as absent or ready"),
        }
    }

    #[test]
    fn a_configured_environment_defaults_to_apples_production_host_and_accepts_an_override() {
        let (private, _) = throwaway_key();
        let configured =
            vars(&[(KEY_P8_VAR, private.as_str()), (KEY_ID_VAR, "TESTKEYID1"), (TEAM_ID_VAR, "TESTTEAMID"), (TOPIC_VAR, "org.ankurah.community")]);

        match read_from(&configured) {
            FromEnv::Ready(config) => {
                assert_eq!(config.endpoint, DEFAULT_ENDPOINT, "production is the default, not the sandbox");
                assert_eq!(config.topic, "org.ankurah.community");
                assert_eq!(config.key_id, "TESTKEYID1");
            }
            _ => panic!("all four set must read as ready"),
        }

        let mut overridden = configured;
        overridden.insert(ENDPOINT_VAR.to_string(), "http://127.0.0.1:9000/".to_string());
        match read_from(&overridden) {
            // The trailing slash survives here and is trimmed where the request
            // path is built, so both spellings reach the same URL.
            FromEnv::Ready(config) => assert_eq!(config.endpoint, "http://127.0.0.1:9000/"),
            _ => panic!("an endpoint override must not unconfigure push"),
        }
    }

    #[test]
    fn a_provider_token_is_an_es256_jwt_the_matching_public_key_verifies() {
        use jsonwebtoken::{decode, DecodingKey, Validation};

        let (private, public) = throwaway_key();
        let tokens = ProviderTokens::new(&config_with(private)).expect("a freshly generated P-256 key is usable");

        let minted_at_ms = 1_700_000_000_000;
        let token = tokens.at(minted_at_ms).expect("sign a provider token");

        let header = jsonwebtoken::decode_header(&token).expect("read the provider token header");
        assert_eq!(header.alg, Algorithm::ES256, "APNs accepts ES256 and nothing else");
        assert_eq!(header.kid.as_deref(), Some("TESTKEYID1"), "the key id tells Apple which of a team's keys signed this");

        // The signature really is over these claims with this key: verifying
        // with the public half is the only check that cannot pass by accident.
        let mut validation = Validation::new(Algorithm::ES256);
        validation.required_spec_claims.clear();
        validation.validate_exp = false;
        let decoded = decode::<serde_json::Value>(&token, &DecodingKey::from_ec_pem(public.as_bytes()).unwrap(), &validation)
            .expect("the matching public key verifies the provider token");
        assert_eq!(decoded.claims["iss"], "TESTTEAMID", "the issuer is the team");
        assert_eq!(decoded.claims["iat"], minted_at_ms / 1000, "issued-at is seconds, not milliseconds");

        // A key that is not one is refused where it is read, not at the first
        // send.
        assert!(ProviderTokens::new(&config_with("-----BEGIN PRIVATE KEY-----\nnope\n-----END PRIVATE KEY-----".to_string())).is_err());
    }

    #[test]
    fn a_provider_token_is_reused_until_it_nears_its_hour_and_then_replaced() {
        let (private, _) = throwaway_key();
        let tokens = ProviderTokens::new(&config_with(private)).unwrap();

        let start = 1_700_000_000_000;
        let first = tokens.at(start).unwrap();
        assert_eq!(tokens.at(start + 60_000).unwrap(), first, "a minute later is the same token");
        assert_eq!(
            tokens.at(start + PROVIDER_TOKEN_REUSE_MS - 1).unwrap(),
            first,
            "right up to the reuse window it is still the same token — Apple refuses a replacement minted too soon"
        );

        // Past the window, a new one — and it is genuinely new, not the same
        // string re-signed: the issued-at moved.
        let second = tokens.at(start + PROVIDER_TOKEN_REUSE_MS).unwrap();
        assert_ne!(second, first, "past the reuse window a fresh token is signed, well before Apple's hour");
        assert_eq!(tokens.at(start + PROVIDER_TOKEN_REUSE_MS + 1000).unwrap(), second, "and that one is then held in turn");
    }

    #[test]
    fn apples_answer_decides_whether_a_device_token_survives() {
        assert_eq!(classify(200, ""), Outcome::Delivered);

        // The app is gone from that device, or the token was never one APNs
        // issued: both are permanent, and the caller drops the row.
        assert_eq!(classify(410, r#"{"reason":"Unregistered"}"#), Outcome::TokenGone { reason: "Unregistered" });
        assert_eq!(classify(400, r#"{"reason":"BadDeviceToken"}"#), Outcome::TokenGone { reason: "BadDeviceToken" });

        // A topic that disagrees with the token looks like a dead device and is
        // not one — a wrong APNS_TOPIC says this about every device at once, so
        // pruning on it would empty the registry on a typo.
        assert_eq!(
            classify(400, r#"{"reason":"DeviceTokenNotForTopic"}"#),
            Outcome::Refused { status: 400, reason: "DeviceTokenNotForTopic".to_string() }
        );
        assert_eq!(classify(403, r#"{"reason":"ExpiredProviderToken"}"#), Outcome::Refused {
            status: 403,
            reason: "ExpiredProviderToken".to_string()
        });
        // The same words under the wrong status are not the refusal they name.
        assert_eq!(classify(400, r#"{"reason":"Unregistered"}"#), Outcome::Refused { status: 400, reason: "Unregistered".to_string() });

        // A body that is not Apple's JSON leaves the reason blank rather than
        // losing the status.
        assert_eq!(classify(503, "<html>gateway</html>"), Outcome::Refused { status: 503, reason: String::new() });
        assert_eq!(classify(500, ""), Outcome::Refused { status: 500, reason: String::new() });
    }
}
