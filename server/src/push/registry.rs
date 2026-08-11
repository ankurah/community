//! `POST /push/register` — a member tells the server where to reach their phone.
//!
//! FOR: a device token is minted by iOS, changes without warning, and is
//! useless to anyone but the server that will send to it. So the app hands it
//! over on every launch and the server keeps the newest one per device; this is
//! the door it comes through.
//!
//! HOW THE CALLER IS RECOGNIZED. It presents the same ankurah session token
//! `/auth/session` mints, as `Authorization: Bearer <token>`, and this route
//! checks it with the same [`SigningKeys`] that signed it. That check is the
//! whole trust boundary here: the `sub` of a verified token is the caller's
//! `User` entity id, which is the key every row in the registry is filed under.
//!
//! This is the FIRST HTTP route on this server to consume a session token
//! rather than mint one. Until now the token was only ever presented over the
//! websocket, where `JwtAgent` checks it against `policy.json` for every
//! operation. There is no policy to consult here — the registry is not an
//! ankurah collection (see [`super::store`]) — so the two questions this route
//! asks are asked directly: does the token verify, and is its subject a member.
//!
//! WHY A GUEST IS REFUSED RATHER THAN ACCOMMODATED. A guest token's `sub` is
//! the literal `guest` (see `crate::guest`), which is not an entity id and
//! names no `User` row. There is nothing to file a device under, and nothing
//! would ever be addressed to it — a guest receives no notifications, because
//! notifications name a recipient and a guest is nobody's. So the refusal is
//! 403 with a sentence saying to sign in, not a silent success.
//!
//! WITHDRAWING A DEVICE, on the same route. A signed-out phone must stop
//! ringing for the account that left it: the alerts a member's mentions produce
//! would otherwise go on arriving on a device nobody is signed into, and the
//! member has no way to say so. So the body carries an `unregister` flag, and
//! the row it drops is the one filed under the VERIFIED caller — the same
//! `sub`, the same (member, token) key. A caller can withdraw nothing but their
//! own.
//!
//! One route rather than two, because everything before the last line is
//! identical: the same bearer check, the same guest refusal, the same "is this
//! a device token at all". A DELETE would repeat all of it to change one call.
//!
//! WHAT THIS DOES NOT MAKE THE REGISTRY EXACT, and why the other path stays.
//! A sign-out on a live device withdraws; a device that is offline, uninstalled
//! or wiped never sends anything, and no promise made here reaches it. Those
//! rows leave when APNs reports them gone (see `workers::push`), which remains
//! the only report that arrives without the device's cooperation. The two are
//! complements: this one is prompt where it applies, that one is the backstop
//! where nothing else can be.

use std::sync::Arc;

use ankurah_jwt_auth::SigningKeys;
use ankurah::EntityId;
use axum::{
    body::Bytes,
    extract::State,
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use tracing::{info, warn};

use super::store::{token_prefix, DeviceTokens, Platform};

/// The most this route will buffer. A registration is two short strings, and
/// the body is materialized by the extractor before the handler can check
/// anything — so refusing at the layer (`main` mounts this as a
/// `DefaultBodyLimit`, as it does for the CI webhook) is what keeps a caller
/// from making us collect megabytes before we learn who they are.
pub const MAX_BODY_BYTES: usize = 2048;

/// Bounds on the device token a caller may file.
///
/// Apple's own guidance is not to hard-code the length — it has changed once
/// and may again — so these are a floor and a ceiling around today's 32-byte
/// (64 hex character) token rather than an equality check. What is exact is the
/// alphabet: APNs device tokens are hexadecimal, and anything else is a caller
/// sending something that is not a device token.
const MIN_TOKEN_CHARS: usize = 64;
const MAX_TOKEN_CHARS: usize = 256;

/// Everything the route needs, and nothing else: the verifying key and the
/// registry.
///
/// Held by `AppState` and handed over through `FromRef`, the shape
/// `ci_hook::CiHook` and `guest::GuestMint` already use — this handler has no
/// business with the node or the OIDC verifier, and keeping it to its own state
/// is what lets the route be tested without standing either of them up.
#[derive(Clone)]
pub struct PushRegistry {
    keys: SigningKeys,
    tokens: Arc<dyn DeviceTokens>,
}

impl PushRegistry {
    pub fn new(keys: SigningKeys, tokens: Arc<dyn DeviceTokens>) -> Self { Self { keys, tokens } }
}

/// What a caller sends.
#[derive(Deserialize)]
pub struct RegisterRequest {
    /// The device token as APNs issued it: hexadecimal, no spaces. Case is
    /// the sender's (the iOS plugin formats upper-case) and is preserved —
    /// re-casing here would file one phone under two rows.
    token: String,
    /// Which push service reaches this device. `ios` today; Google Play is a
    /// later phase and anything else is refused rather than guessed at.
    platform: String,
    /// Take this device off the caller's list instead of putting it on.
    ///
    /// Absent means false, which is what keeps every registration this server
    /// has ever received meaning what it meant: an app that predates this field
    /// sends the same two members and goes on registering.
    #[serde(default)]
    unregister: bool,
}

/// The route handler: recognize the caller, check what they sent, file it.
///
/// The body arrives as bytes and is parsed here rather than by a `Json`
/// extractor, so that the ORDER of the answers is this function's to decide: a
/// caller who has not identified themselves hears 401 whatever they sent, and
/// every malformed body is one 400 with a sentence rather than an extractor
/// rejection that varies with how the JSON was wrong.
pub async fn handle(State(registry): State<PushRegistry>, headers: HeaderMap, body: Bytes) -> Response {
    let Some(presented) = bearer_token(&headers) else {
        return (StatusCode::UNAUTHORIZED, "a session token is required: Authorization: Bearer <token>").into_response();
    };

    let claims = match registry.keys.verify(presented) {
        Ok(claims) => claims,
        Err(e) => {
            // The detail goes to the log, not to the caller: an expired token
            // and a token signed by somebody else are the same answer here.
            warn!("a device registration presented a session token that did not verify: {e}");
            return (StatusCode::UNAUTHORIZED, "session token is not valid").into_response();
        }
    };

    // A member is a caller whose subject is a `User` entity id. A guest's is
    // the literal `guest`, which does not parse — see the module header for why
    // that is a refusal and not a fallback.
    let Ok(user) = EntityId::from_base64(&claims.sub) else {
        return (
            StatusCode::FORBIDDEN,
            "a guest session has no account to reach; sign in before registering a device for notifications",
        )
            .into_response();
    };

    let Ok(request) = serde_json::from_slice::<RegisterRequest>(&body) else {
        return (StatusCode::BAD_REQUEST, "expected a JSON body: {\"token\": \"<device token>\", \"platform\": \"ios\"}").into_response();
    };

    if let Err(reason) = check_token(&request.token) {
        return (StatusCode::BAD_REQUEST, reason).into_response();
    }
    let Some(platform) = Platform::parse(&request.platform) else {
        return (StatusCode::BAD_REQUEST, "unsupported platform; this server sends to \"ios\" today").into_response();
    };

    let user = user.to_base64();
    // The ops trail, matching what the mint logs: who, and enough of the token
    // to tell one of a member's devices from another. NEVER the whole token —
    // it is the credential for waking that device.
    let device = token_prefix(&request.token);

    if request.unregister {
        // The row dropped is the caller's own, because the caller's own subject
        // is half of its key — there is no parameter here naming whose device
        // this is. An absent row is not an error: a sign-out on a device that
        // never got as far as registering arrives here too.
        if let Err(e) = registry.tokens.forget(&user, &request.token).await {
            warn!(user = %user, "failed to withdraw a device token: {e:#}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "could not withdraw this device").into_response();
        }
        info!(user = %user, device = %device, "withdrew a device from notifications");
        return StatusCode::NO_CONTENT.into_response();
    }

    if let Err(e) = registry.tokens.register(&user, &request.token, platform, crate::workers::now_ms()).await {
        warn!(user = %user, "failed to register a device token: {e:#}");
        return (StatusCode::INTERNAL_SERVER_ERROR, "could not register this device").into_response();
    }

    info!(user = %user, device = %device, platform = platform.as_str(), "registered a device for notifications");
    StatusCode::NO_CONTENT.into_response()
}

/// The token out of an `Authorization: Bearer <token>` header, if there is one.
/// The scheme is matched case-insensitively, which is what RFC 7235 asks for.
fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let token = token.trim();
    (!token.is_empty()).then_some(token)
}

/// Whether this is a device token at all. `Err` carries the sentence the caller
/// gets back.
fn check_token(token: &str) -> Result<(), &'static str> {
    if !token.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err("device token must be hexadecimal");
    }
    if token.len() < MIN_TOKEN_CHARS || token.len() > MAX_TOKEN_CHARS {
        return Err("device token is not a plausible length");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bearer_header_is_read_and_anything_else_is_not() {
        let mut headers = HeaderMap::new();
        assert_eq!(bearer_token(&headers), None, "no header at all");

        headers.insert(header::AUTHORIZATION, "Bearer abc.def.ghi".parse().unwrap());
        assert_eq!(bearer_token(&headers), Some("abc.def.ghi"));
        // RFC 7235 makes the scheme case-insensitive, and clients spell it
        // every way.
        headers.insert(header::AUTHORIZATION, "bearer abc.def.ghi".parse().unwrap());
        assert_eq!(bearer_token(&headers), Some("abc.def.ghi"));

        // A different scheme is not a bearer token, and neither is a bare value.
        headers.insert(header::AUTHORIZATION, "Basic dXNlcjpwYXNz".parse().unwrap());
        assert_eq!(bearer_token(&headers), None);
        headers.insert(header::AUTHORIZATION, "abc.def.ghi".parse().unwrap());
        assert_eq!(bearer_token(&headers), None);
        headers.insert(header::AUTHORIZATION, "Bearer   ".parse().unwrap());
        assert_eq!(bearer_token(&headers), None);
    }

    #[test]
    fn a_device_token_is_hexadecimal_and_of_a_plausible_length() {
        let real_length = "a".repeat(64);
        assert_eq!(check_token(&real_length), Ok(()));
        assert_eq!(check_token(&"0123456789abcdefABCDEF".repeat(4)), Ok(()));

        // The refusals, each with its own sentence: not hex at all, and hex but
        // far too short to be a token.
        assert!(check_token("hello, i am a device token").is_err());
        assert!(check_token(&format!("{real_length}!")).is_err());
        assert!(check_token("abcdef").is_err());
        assert!(check_token("").is_err());
        assert!(check_token(&"a".repeat(MAX_TOKEN_CHARS + 1)).is_err());
    }
}

/// The endpoint over its real axum route: who is let in, who is turned away,
/// and what a second registration of the same device does.
///
/// Separate from the unit tests above because these verify real tokens, and
/// that needs an RS256 keypair — seconds to generate in a debug build, so the
/// module makes exactly one and shares it, as `guest::route_tests` does.
#[cfg(test)]
mod route_tests {
    use super::*;
    use crate::push::store::memory::MemoryDeviceTokens;
    use ankurah_jwt_auth::JwtClaims;
    use axum::{body::Body, http::Request, routing::post, Router};
    use std::sync::OnceLock;
    use tower::ServiceExt as _;

    fn test_keys() -> SigningKeys {
        static KEYS: OnceLock<SigningKeys> = OnceLock::new();
        KEYS.get_or_init(|| SigningKeys::generate().expect("generate a test signing key")).clone()
    }

    /// A member's session token, as `/auth/session` mints one: the subject is
    /// the caller's `User` entity id.
    fn member_token(user: EntityId) -> String {
        let claims = JwtClaims {
            sub: user.to_base64(),
            roles: vec!["member".to_string()],
            email: "member@example.invalid".to_string(),
            name: Some("Member".to_string()),
            custom: serde_json::Map::new(),
        };
        crate::mint_session_token(&test_keys(), &claims, 1).unwrap()
    }

    /// A guest's, as `/auth/guest` mints one: the subject is the guest literal,
    /// which is not an entity id.
    fn guest_token() -> String {
        let claims = JwtClaims {
            sub: crate::guest::GUEST_SUB.to_string(),
            roles: vec![crate::guest::GUEST_ROLE.to_string()],
            email: String::new(),
            name: None,
            custom: serde_json::Map::new(),
        };
        crate::mint_session_token(&test_keys(), &claims, 1).unwrap()
    }

    /// The real route on its own state — `main` mounts `handle` on a router
    /// stated with `AppState` and axum hands it the `PushRegistry` through
    /// `FromRef`, so a router stated directly on the registry runs the same
    /// handler through the same extractors.
    fn app(tokens: Arc<dyn DeviceTokens>) -> Router {
        Router::new().route("/push/register", post(handle)).with_state(PushRegistry::new(test_keys(), tokens))
    }

    fn request(authorization: Option<&str>, body: &str) -> Request<Body> {
        let mut request = Request::builder()
            .method("POST")
            .uri("/push/register")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        if let Some(authorization) = authorization {
            request.headers_mut().insert(header::AUTHORIZATION, authorization.parse().unwrap());
        }
        request
    }

    const DEVICE: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const OTHER_DEVICE: &str = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";

    #[tokio::test]
    async fn a_member_registers_a_device_and_a_second_registration_refreshes_it() {
        let store = MemoryDeviceTokens::new();
        let app = app(store.clone());
        let user = EntityId::new();
        let authorization = format!("Bearer {}", member_token(user));

        let body = format!(r#"{{"token":"{DEVICE}","platform":"ios"}}"#);
        let response = app.clone().oneshot(request(Some(&authorization), &body)).await.unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);

        let rows = store.all();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, (user.to_base64(), DEVICE.to_string()), "the row is filed under the token's own subject");
        assert_eq!(rows[0].1.platform, Platform::Ios);
        let first_seen = rows[0].1.last_registered_at;

        // A second device is a second row.
        let body = format!(r#"{{"token":"{OTHER_DEVICE}","platform":"ios"}}"#);
        assert_eq!(app.clone().oneshot(request(Some(&authorization), &body)).await.unwrap().status(), StatusCode::NO_CONTENT);
        assert_eq!(store.all().len(), 2, "a member with two devices has two rows");

        // The SAME device again is not: the app re-registers on every launch,
        // and each launch must refresh one row rather than add one.
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        let body = format!(r#"{{"token":"{DEVICE}","platform":"ios"}}"#);
        assert_eq!(app.oneshot(request(Some(&authorization), &body)).await.unwrap().status(), StatusCode::NO_CONTENT);
        let rows = store.all();
        assert_eq!(rows.len(), 2, "re-registering refreshes rather than adds");
        let refreshed = rows.iter().find(|(key, _)| key.1 == DEVICE).unwrap().1.last_registered_at;
        assert!(refreshed >= first_seen, "and the row records when it was last claimed: {refreshed} vs {first_seen}");
    }

    /// The withdrawal half: a member takes their own device off, and can take
    /// nobody else's off. The second claim is the whole of what makes this
    /// route safe to expose — the row's key is (subject, token), and the
    /// subject comes from the verified token rather than from the body, so a
    /// caller naming a device that is not theirs is naming a row that does not
    /// exist.
    #[tokio::test]
    async fn a_member_withdraws_their_own_device_and_reaches_nobody_elses() {
        let store = MemoryDeviceTokens::new();
        let app = app(store.clone());
        let (alice, bob) = (EntityId::new(), EntityId::new());
        let alices = format!("Bearer {}", member_token(alice));
        let bobs = format!("Bearer {}", member_token(bob));

        // Both members register the same-shaped device; one phone each.
        for (authorization, token) in [(&alices, DEVICE), (&bobs, OTHER_DEVICE)] {
            let body = format!(r#"{{"token":"{token}","platform":"ios"}}"#);
            assert_eq!(app.clone().oneshot(request(Some(authorization), &body)).await.unwrap().status(), StatusCode::NO_CONTENT);
        }
        assert_eq!(store.all().len(), 2);

        // Alice aims at Bob's device. The route answers the same 204 it answers
        // for a row that was never there — there is nothing to tell her, and
        // nothing happens.
        let body = format!(r#"{{"token":"{OTHER_DEVICE}","platform":"ios","unregister":true}}"#);
        assert_eq!(app.clone().oneshot(request(Some(&alices), &body)).await.unwrap().status(), StatusCode::NO_CONTENT);
        assert_eq!(store.all().len(), 2, "a withdrawal names the caller's own row or no row at all");
        assert!(store.all().iter().any(|(key, _)| key.0 == bob.to_base64()), "Bob's device is still Bob's");

        // Her own comes off.
        let body = format!(r#"{{"token":"{DEVICE}","platform":"ios","unregister":true}}"#);
        assert_eq!(app.clone().oneshot(request(Some(&alices), &body)).await.unwrap().status(), StatusCode::NO_CONTENT);
        let rows = store.all();
        assert_eq!(rows.len(), 1, "the caller's own row is gone");
        assert_eq!(rows[0].0 .0, bob.to_base64());

        // And a second sign-out on the same device is not an error: the app
        // sends this best-effort and may send it twice.
        assert_eq!(app.clone().oneshot(request(Some(&alices), &body)).await.unwrap().status(), StatusCode::NO_CONTENT);
        assert_eq!(store.all().len(), 1);

        // A withdrawal is refused everywhere a registration is: an unverified
        // caller cannot name a subject, so there is no row to reach.
        assert_eq!(app.oneshot(request(None, &body)).await.unwrap().status(), StatusCode::UNAUTHORIZED);
        assert_eq!(store.all().len(), 1);
    }

    /// A guest is turned away whichever way the flag is set: the refusal is
    /// about having no account to file a device under, which is equally true of
    /// taking one off.
    #[tokio::test]
    async fn a_guest_session_may_not_withdraw_a_device_either() {
        let store = MemoryDeviceTokens::new();
        let authorization = format!("Bearer {}", guest_token());
        let body = format!(r#"{{"token":"{DEVICE}","platform":"ios","unregister":true}}"#);
        let response = app(store.clone()).oneshot(request(Some(&authorization), &body)).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn a_guest_session_is_refused_with_a_sentence_that_says_what_to_do() {
        // A guest's subject is not an entity id, so there is no account to file
        // a device under — and a guest is nobody's notification recipient, so
        // nothing would ever be sent to it. The refusal says so.
        let store = MemoryDeviceTokens::new();
        let authorization = format!("Bearer {}", guest_token());
        let body = format!(r#"{{"token":"{DEVICE}","platform":"ios"}}"#);

        let response = app(store.clone()).oneshot(request(Some(&authorization), &body)).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let message = String::from_utf8(axum::body::to_bytes(response.into_body(), 4096).await.unwrap().to_vec()).unwrap();
        assert!(message.contains("sign in"), "the refusal tells a guest what would change the answer, got: {message}");
        assert!(store.all().is_empty(), "and nothing was filed");
    }

    #[tokio::test]
    async fn a_caller_with_no_usable_session_token_is_not_recognized() {
        let store = MemoryDeviceTokens::new();
        let body = format!(r#"{{"token":"{DEVICE}","platform":"ios"}}"#);

        // No header at all.
        let response = app(store.clone()).oneshot(request(None, &body)).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        // A token this server did not sign. The point of the check is that a
        // caller cannot name a subject of their choosing: this one is a
        // well-formed JWT claiming a real-looking user.
        let claimed = SigningKeys::generate().unwrap();
        let claims = JwtClaims {
            sub: EntityId::new().to_base64(),
            roles: vec!["member".to_string()],
            email: String::new(),
            name: None,
            custom: serde_json::Map::new(),
        };
        let elsewhere = crate::mint_session_token(&claimed, &claims, 1).unwrap();
        let response = app(store.clone()).oneshot(request(Some(&format!("Bearer {elsewhere}")), &body)).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "a token signed by another key is not a session here");

        // And something that is not a JWT at all.
        let response = app(store.clone()).oneshot(request(Some("Bearer not-a-token"), &body)).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        assert!(store.all().is_empty(), "none of these filed anything");
    }

    #[tokio::test]
    async fn a_body_that_is_not_a_device_registration_is_refused() {
        let store = MemoryDeviceTokens::new();
        let authorization = format!("Bearer {}", member_token(EntityId::new()));

        for body in [
            // Not a device token: the recognized caller is beside the point.
            r#"{"token":"hello, i am a device token","platform":"ios"}"#.to_string(),
            r#"{"token":"","platform":"ios"}"#.to_string(),
            r#"{"token":"abcdef","platform":"ios"}"#.to_string(),
            // Hex, right length, but one character outside the alphabet.
            format!(r#"{{"token":"{}z","platform":"ios"}}"#, &DEVICE[1..]),
            // A service this server cannot reach.
            format!(r#"{{"token":"{DEVICE}","platform":"android"}}"#),
            format!(r#"{{"token":"{DEVICE}","platform":""}}"#),
            // Not a registration at all.
            r#"{"platform":"ios"}"#.to_string(),
            "not json".to_string(),
        ] {
            let response = app(store.clone()).oneshot(request(Some(&authorization), &body)).await.unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "refused: {body}");
        }

        assert!(store.all().is_empty(), "nothing was filed for any of them");
    }
}
