//! `POST /auth/guest` — a session token for a visitor who has not signed in (#79).
//!
//! FOR: someone who follows a link into the community should be able to read
//! along without making an account. Reading needs a token, because every
//! ankurah operation is authorized from claims and there is no such thing as a
//! request without them — so the visitor needs one that says, in the same shape
//! a member's does, that nobody signed in. This endpoint mints exactly that.
//!
//! WHAT IT MINTS: the token `auth_session` mints, with two differences. The
//! same RS256 keypair signs it, the same [`crate::mint_session_token`] call
//! assembles it, and it is the same `{ "token": ... }` on the wire — but `sub`
//! is the literal [`GUEST_SUB`] and the only role is [`GUEST_ROLE`]. The
//! literal is not an id and cannot be read as one, which is the property the
//! whole design rests on: a scope filter comparing a row's owner against
//! `$jwt.sub` is FALSE for a guest rather than erroring, and no guest can land
//! on a member's row by accident. A token with no `sub` at all would fail that
//! comparison by hard error instead, taking the surrounding rule down with it.
//!
//! WHAT IT DOES NOT DO: no IdP round-trip, no nonce, no request body, and not
//! one row written — no `User`, no `UserRoles`. A guest is a claim set with a
//! short life and no history anywhere on the server. The client mints a fresh
//! one when the old one expires or the connection comes back.
//!
//! WHAT A GUEST TOKEN GRANTS, and where that is decided — `policy.json`, not
//! here. The `guest` role holds one privilege, `view`, and it leaves four
//! collections readable: `room`, `message`, `reaction`, `linkpreview` — the
//! conversation, and what renders alongside it. That is the whole tier, and it
//! is what TWO refusals leave behind rather than what one privilege opens.
//!
//! The collection gate refuses three outright: the roster and the moderation
//! log (`user`, `userroles`, `modaction`) are keyed to the `signed_in`
//! privilege a guest does not hold, so the query never reaches a row. `view`
//! passes that gate on the other eleven.
//!
//! The row scopes then empty seven of those eleven — DMs, inboxes, read
//! cursors, bans — with no rule written about guests anywhere. Those scopes
//! compare `$jwt.sub` against an entity id, and [`GUEST_SUB`] never equals one,
//! so a query matches nothing and a get by id is refused. Four are left, which
//! is the list above.
//!
//! And a guest holds no `post` privilege, so a guest writes nothing anywhere.
//! `policy.json` and `server/tests/guest_policy_live_tests.rs` are the
//! authority on all of that; this module only mints the claims those rules are
//! then applied to.
//!
//! # Why the mint is rate limited
//!
//! A guest has no account to suspend. Identity here is free and per-session, so
//! the ban table — a member tool, pointed at a `User` row — has nothing to point
//! at. This endpoint's own budget stands in for it. What the budget actually
//! bounds is the one resource a caller spends by asking: an RSA-4096 signature
//! on the instance's single vCPU, which that instance shares with every
//! websocket sync in the community.
//!
//! It is friction, not a wall. A caller with many source addresses gets through
//! it, and the honest answer to that one is network-level limiting in front of
//! the service, which this deployment does not have (Cloud Run, reached
//! directly, with no load balancer to hang such a thing off).

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ankurah_jwt_auth::{JwtClaims, SigningKeys};
use axum::{
    extract::{ConnectInfo, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json as AxumJson,
};
use tracing::{debug, info, warn};

use crate::SessionResponse;

/// The `sub` every guest token carries. A literal, never an id: an `EntityId`
/// does not parse out of it, so a scope filter that compares a row's owner
/// against `$jwt.sub` simply does not match for a guest.
pub const GUEST_SUB: &str = "guest";

/// The only role a guest token carries. `policy.json` grants it the `view`
/// privilege and nothing else — the read tier the module header describes: the
/// conversation, no roster, no moderation log, no private row, and no write.
pub const GUEST_ROLE: &str = "guest";

/// Guest session lifetime — hours, and deliberately far short of the member
/// token's [`crate::TOKEN_TTL_HOURS`]. A member renewing pays an OIDC
/// round-trip, so their token is long-lived on purpose; a guest re-mints with
/// one unattended POST, so a short life costs the visitor nothing and keeps a
/// copied token from being a durable credential. Two hours still covers an
/// ordinary visit — arrive, read, come back after lunch — without a re-mint.
pub const GUEST_TOKEN_TTL_HOURS: u64 = 2;

/// The header Google's front end writes the connecting address into. The `http`
/// crate has constants for the standard header names; this one is not among
/// them.
const FORWARDED_FOR: &str = "x-forwarded-for";

/// Everything the endpoint needs: the signing key, and the mint budgets.
///
/// `AppState` holds one and hands it to the handler through `FromRef` — the
/// same shape the CI webhook uses, and for the same reason: this handler has no
/// business with the OIDC verifier or the node, and keeping it to its own state
/// is what lets the route be tested without standing either of them up.
#[derive(Clone)]
pub struct GuestMint(Arc<Inner>);

struct Inner {
    /// The same RS256 keypair `auth_session` signs with. One issuer, one key,
    /// one verifying key for clients to check both kinds of token against.
    keys: SigningKeys,
    /// Shared across every request, which is what makes the budgets budgets.
    limiter: Mutex<Limiter>,
}

impl GuestMint {
    pub fn new(keys: SigningKeys) -> Self {
        Self(Arc::new(Inner { keys, limiter: Mutex::new(Limiter::new(Instant::now())) }))
    }

    /// Decide one mint request. `Err` carries how long until the budget that
    /// refused it reopens.
    ///
    /// A poisoned lock is taken anyway (the jwt-auth idiom, used at every lock
    /// in that crate): the guarded state is two integers and a map of them, so
    /// the worst a panicking predecessor leaves behind is a count that is
    /// slightly off. Refusing the lock instead would turn one panic into an
    /// endpoint that 500s forever.
    fn admit(&self, client: IpAddr, now: Instant) -> Result<(), Duration> {
        self.0.limiter.lock().unwrap_or_else(|e| e.into_inner()).admit(client, now)
    }
}

/// The route handler: count the request against the budgets, then sign.
///
/// It reads no body and no query, because #79's contract is that any browser
/// may ask — there is nothing a caller could send that would change the answer,
/// so there is nothing to validate.
pub async fn handle(State(mint): State<GuestMint>, ConnectInfo(peer): ConnectInfo<SocketAddr>, headers: HeaderMap) -> Response {
    let client = client_address(&headers, peer.ip());

    if let Err(reopens_in) = mint.admit(client, Instant::now()) {
        // One log line per refused request would make a flood cheaper to send
        // than to record, so a refusal is DEBUG — invisible at the INFO the
        // server runs at, and one level away when an operator wants it. Every
        // mint that succeeds is INFO, and the budgets bound how many that is.
        debug!("guest mint refused; the budget reopens in {reopens_in:?}");
        // Whole seconds, rounded UP: a `Retry-After: 0` invites an immediate
        // retry that the same budget would refuse again.
        let retry_after = reopens_in.as_secs() + 1;
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [(header::RETRY_AFTER, retry_after.to_string())],
            "too many guest sessions minted; retry shortly",
        )
            .into_response();
    }

    match crate::mint_session_token(&mint.0.keys, &guest_claims(), GUEST_TOKEN_TTL_HOURS) {
        Ok(token) => {
            // Its own message, distinct from the member path's "minted session
            // token": a sign-in and a guest session are different events, and
            // an operator counting one must not be counting the other. The
            // claims are constants, so the only thing worth carrying is how
            // long this one lives. Never the token.
            info!(user = GUEST_SUB, ttl_hours = GUEST_TOKEN_TTL_HOURS, "minted a guest session token");
            AxumJson(SessionResponse { token }).into_response()
        }
        Err(e) => {
            warn!("failed to mint a guest token: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, format!("failed to mint guest token: {e}")).into_response()
        }
    }
}

/// The claims of every guest token: the guest subject, the guest role, and
/// nothing else.
///
/// No email and no display name, because a guest has neither. No `oidc_sub`
/// custom claim either: a guest owns no profile row to edit, and the policy's
/// user-collection write scope — which pins an edit to the caller's own
/// `$jwt.custom.oidc_sub` — refuses outright when that claim is absent, which
/// is the right answer for a caller with no profile.
fn guest_claims() -> JwtClaims {
    JwtClaims {
        sub: GUEST_SUB.to_string(),
        roles: vec![GUEST_ROLE.to_string()],
        email: String::new(),
        name: None,
        custom: serde_json::Map::new(),
    }
}

/// The address this mint is counted against.
///
/// Google's front end APPENDS the address it saw the client connect from to
/// whatever `X-Forwarded-For` arrived, so the RIGHTMOST entry is the only one
/// this service can believe: everything to its left is caller-written text that
/// no proxy checked. Reading the leftmost entry — the usual "the client comes
/// first" reading of this header — would hand every caller a key of its own
/// choosing, which is the same as having no budget at all.
///
/// ASSUMPTION, and what would invalidate it: the service is reached straight
/// through Cloud Run with no external load balancer in front, which is what
/// `.github/workflows/deploy.yml` deploys. A Google external Application Load
/// Balancer appends TWO entries — the client's address and its own — so putting
/// one in front would make the rightmost entry the load balancer's, and collapse
/// every caller into a single budget. Whoever adds one has to move this to the
/// second entry from the right.
///
/// With no usable `X-Forwarded-For`, the socket peer is what the mint counts
/// against. Locally that peer IS the client (dev, and the smoke). In production
/// it is the front end, so every caller in that situation shares one budget —
/// which is the safe way to be wrong, and the reason the two unusable cases
/// below take this route rather than reading on.
fn client_address(headers: &HeaderMap, peer: IpAddr) -> IpAddr {
    let mut lines = headers.get_all(FORWARDED_FOR).into_iter();
    let Some(forwarded) = lines.next() else { return peer };
    // A SECOND header line makes the whole header unusable. A caller may send
    // its own `X-Forwarded-For`, and the front end appends its address to one
    // line rather than merging them — this service cannot tell which line came
    // back with that address on the end, and reading the wrong one is reading a
    // value the caller chose. Two lines therefore count against the peer.
    if lines.next().is_some() {
        return peer;
    }
    let Ok(forwarded) = forwarded.to_str() else { return peer };
    // Only the last entry of that one line is consulted. An unparseable last
    // entry falls back to the peer rather than walking further left, because
    // further left is exactly the part a caller writes.
    forwarded.rsplit(',').map(str::trim).find(|entry| !entry.is_empty()).and_then(parse_address).unwrap_or(peer)
}

/// One `X-Forwarded-For` entry as an address. Google's front end writes a bare
/// address; the `address:port` form is accepted too, because other proxies
/// write that and the port is no part of the identity being counted.
fn parse_address(entry: &str) -> Option<IpAddr> {
    entry.parse::<IpAddr>().ok().or_else(|| entry.parse::<SocketAddr>().ok().map(|addr| addr.ip()))
}

/// The trailing window both budgets are counted over.
const WINDOW: Duration = Duration::from_secs(60);

/// How many guest tokens one client address may mint per [`WINDOW`]. A browser
/// needs one per visit and then holds it for [`GUEST_TOKEN_TTL_HOURS`], so ten
/// leaves room for a reload storm, a flapping connection re-minting, and the
/// several people who share one address behind a home or office router.
const MAX_PER_CLIENT: u32 = 10;

/// How many guest tokens this instance mints per [`WINDOW`], every caller
/// together. Sized against what a mint costs rather than against demand: an
/// RSA-4096 signature is tens of milliseconds of the instance's single vCPU,
/// which it shares with every websocket sync in the community, so two mints a
/// second is a few percent of one core — and a crowd arriving from one shared
/// link still gets in. Reaching it takes at least twelve distinct addresses.
const MAX_TOTAL: u32 = 120;

/// The mint budgets: one window per client address that has minted recently,
/// and one for the instance as a whole.
///
/// Memory-only and per-instance, which today is the same as per-service: the
/// deployment pins `--max-instances 1`. The two stop being the same during a
/// rollout, when the old and new revisions serve at once, and they part company
/// for good if that ceiling is ever raised — every instance would carry its own
/// budget, and the effective limits would multiply by the instance count.
struct Limiter {
    /// Every caller's mints together.
    total: Window,
    /// Per client address. Only an ADMITTED mint puts an address in here, and
    /// [`Limiter::forget_expired`] drops the aged-out entries on every request,
    /// so the table holds at most two windows' worth of admitted callers. A
    /// refused caller leaves nothing behind, which is what keeps a flood of
    /// distinct addresses from growing it.
    clients: HashMap<IpAddr, Window>,
}

impl Limiter {
    fn new(now: Instant) -> Self { Self { total: Window::opening(now), clients: HashMap::new() } }

    /// Decide one mint request, spending a slot of each budget when it is
    /// admitted. `Err` carries how long until the budget that refused reopens.
    ///
    /// The client's budget is consulted BEFORE the instance's, and neither is
    /// spent unless both have room. Both halves matter: checking the instance
    /// first would let one address spend the budget everyone is sharing before
    /// its own ran out, and spending the client's slot on a request the
    /// instance then refuses would charge a caller for a mint they never got.
    fn admit(&mut self, client: IpAddr, now: Instant) -> Result<(), Duration> {
        self.forget_expired(now);
        if let Some(window) = self.clients.get(&client) {
            if let Some(reopens_in) = window.exhausted(MAX_PER_CLIENT, now) {
                return Err(reopens_in);
            }
        }
        if let Some(reopens_in) = self.total.exhausted(MAX_TOTAL, now) {
            return Err(reopens_in);
        }
        self.clients.entry(client).or_insert_with(|| Window::opening(now)).record(now);
        self.total.record(now);
        Ok(())
    }

    /// Forget the addresses whose windows have run out, so the table carries
    /// only what is still being counted.
    fn forget_expired(&mut self, now: Instant) {
        self.clients.retain(|_, window| now.saturating_duration_since(window.opened) < WINDOW);
    }
}

/// One fixed window: when it opened, and how many mints it has admitted since.
///
/// Fixed rather than sliding, and that is a real concession: a caller who
/// spends a budget at the end of one window and again at the start of the next
/// fits twice the limit into a couple of seconds. What it buys is counting that
/// costs two integers per address instead of a list of timestamps, on a path
/// that has to stay cheap precisely when it is being hammered.
#[derive(Clone, Copy)]
struct Window {
    opened: Instant,
    admitted: u32,
}

impl Window {
    fn opening(now: Instant) -> Self { Self { opened: now, admitted: 0 } }

    /// How long until this window reopens, or `None` when it has room —
    /// including when it has aged out entirely and the next [`Window::record`]
    /// will start a fresh one.
    fn exhausted(&self, limit: u32, now: Instant) -> Option<Duration> {
        let age = now.saturating_duration_since(self.opened);
        if age >= WINDOW || self.admitted < limit {
            return None;
        }
        Some(WINDOW - age)
    }

    /// Count one admitted mint, opening a fresh window first if this one has
    /// aged out.
    fn record(&mut self, now: Instant) {
        if now.saturating_duration_since(self.opened) >= WINDOW {
            *self = Self::opening(now);
        }
        self.admitted += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn address(last_octet: u8) -> IpAddr { IpAddr::from([203, 0, 113, last_octet]) }

    #[test]
    fn an_address_gets_its_budget_and_then_a_reopening_time() {
        let start = Instant::now();
        let mut limiter = Limiter::new(start);
        for n in 0..MAX_PER_CLIENT {
            assert_eq!(limiter.admit(address(1), start), Ok(()), "mint {n} is inside the budget");
        }
        let reopens_in = limiter.admit(address(1), start).unwrap_err();
        assert!(reopens_in > Duration::ZERO && reopens_in <= WINDOW, "{reopens_in:?}");
    }

    #[test]
    fn an_exhausted_budget_reopens_when_the_window_rolls() {
        let start = Instant::now();
        let mut limiter = Limiter::new(start);
        for _ in 0..MAX_PER_CLIENT {
            limiter.admit(address(1), start).unwrap();
        }
        assert!(limiter.admit(address(1), start + WINDOW - Duration::from_millis(1)).is_err(), "still inside the window");
        assert_eq!(limiter.admit(address(1), start + WINDOW), Ok(()), "the window has rolled");
    }

    #[test]
    fn one_noisy_address_does_not_spend_another_address_budget() {
        // The reason the per-address budget is consulted first: a caller who
        // has run out must not have cost anybody else a mint.
        let start = Instant::now();
        let mut limiter = Limiter::new(start);
        for _ in 0..MAX_PER_CLIENT + 5 {
            let _ = limiter.admit(address(1), start);
        }
        assert_eq!(limiter.admit(address(2), start), Ok(()));
    }

    #[test]
    fn the_instance_budget_bounds_every_address_together() {
        // Each address stays inside its own budget; together they exhaust the
        // instance's, and the next caller — a fresh address with a full
        // per-address budget — is refused all the same.
        let start = Instant::now();
        let mut limiter = Limiter::new(start);
        let addresses = MAX_TOTAL / MAX_PER_CLIENT;
        for n in 0..addresses {
            for _ in 0..MAX_PER_CLIENT {
                assert_eq!(limiter.admit(address(n as u8), start), Ok(()));
            }
        }
        assert!(limiter.admit(address(200), start).is_err(), "the instance budget is spent");
    }

    #[test]
    fn a_flood_of_distinct_addresses_does_not_grow_the_table() {
        // Refusals leave no state behind, so the table is bounded by what the
        // instance budget admits rather than by how many addresses ask.
        let start = Instant::now();
        let mut limiter = Limiter::new(start);
        for n in 0..10_000u32 {
            let _ = limiter.admit(IpAddr::from(n.to_be_bytes()), start);
        }
        assert_eq!(limiter.clients.len(), MAX_TOTAL as usize);
    }

    #[test]
    fn the_counted_address_is_the_last_forwarded_entry() {
        let peer = IpAddr::from([10, 0, 0, 1]);
        let mut headers = HeaderMap::new();

        // No header at all: the socket peer is the client (dev, and the smoke).
        assert_eq!(client_address(&headers, peer), peer);

        // What Cloud Run delivers when the caller sent nothing.
        headers.insert(FORWARDED_FOR, "203.0.113.7".parse().unwrap());
        assert_eq!(client_address(&headers, peer), address(7));

        // What it delivers when the caller sent entries of its own: the front
        // end's entry is last, and the caller's are ignored.
        headers.insert(FORWARDED_FOR, "1.2.3.4, 9.9.9.9, 203.0.113.7".parse().unwrap());
        assert_eq!(client_address(&headers, peer), address(7));

        // A proxy that writes address:port, and an IPv6 client.
        headers.insert(FORWARDED_FOR, "1.2.3.4, 203.0.113.7:41234".parse().unwrap());
        assert_eq!(client_address(&headers, peer), address(7));
        headers.insert(FORWARDED_FOR, "1.2.3.4, 2001:db8::1".parse().unwrap());
        assert_eq!(client_address(&headers, peer), "2001:db8::1".parse::<IpAddr>().unwrap());

        // An unusable last entry counts against the peer rather than against
        // whatever the caller wrote to the left of it.
        headers.insert(FORWARDED_FOR, "203.0.113.7, not-an-address".parse().unwrap());
        assert_eq!(client_address(&headers, peer), peer);
    }

    #[test]
    fn two_forwarded_for_lines_count_against_the_peer() {
        // A caller may send its own `X-Forwarded-For` line, and the front end
        // appends its address to ONE line rather than merging the two. Which
        // line came back carrying it is not knowable here, so picking either
        // one risks counting an address the caller wrote — the budget would be
        // whatever that caller wanted it to be. Both lines are therefore
        // discarded and the peer is counted, which puts every caller doing this
        // into one shared budget.
        let peer = IpAddr::from([10, 0, 0, 1]);
        let mut headers = HeaderMap::new();
        headers.append(FORWARDED_FOR, "203.0.113.7".parse().unwrap());
        headers.append(FORWARDED_FOR, "198.51.100.9".parse().unwrap());
        assert_eq!(client_address(&headers, peer), peer);

        // One line with the same content is the ordinary case and still counts
        // the front end's entry — so the refusal above is about the SECOND
        // line, not about the addresses in it.
        let mut single = HeaderMap::new();
        single.insert(FORWARDED_FOR, "198.51.100.9, 203.0.113.7".parse().unwrap());
        assert_eq!(client_address(&single, peer), address(7));
    }
}

/// The endpoint over its real axum route: what a caller gets back, and what
/// happens once it has asked too often.
///
/// Separate from the unit tests above because these mint for real, and a real
/// mint needs an RS256 keypair — seconds to generate in a debug build, so the
/// module makes exactly one and shares it.
#[cfg(test)]
mod route_tests {
    use super::*;
    use axum::{body::Body, http::Request, routing::post, Router};
    use base64::Engine as _;
    use std::sync::OnceLock;
    use std::time::{SystemTime, UNIX_EPOCH};
    use tower::ServiceExt as _;

    /// One keypair for the whole module. `SigningKeys::generate` builds a
    /// 4096-bit RSA key, which takes seconds unoptimized; every test here wants
    /// the same key anyway, since the point is that one verifying key checks
    /// what this endpoint signs.
    fn test_keys() -> SigningKeys {
        static KEYS: OnceLock<SigningKeys> = OnceLock::new();
        KEYS.get_or_init(|| SigningKeys::generate().expect("generate a test signing key")).clone()
    }

    /// The real route on its own state. `main` mounts `handle` on a router
    /// stated with `AppState` and axum hands it the `GuestMint` through
    /// `FromRef`, so a router stated directly on the mint runs the same handler
    /// through the same extractors.
    fn app() -> Router {
        Router::new().route("/auth/guest", post(handle)).with_state(GuestMint::new(test_keys()))
    }

    /// A request as the front end would deliver it: `forwarded` is the whole
    /// `X-Forwarded-For` value, whose last entry is the address it saw.
    fn request(forwarded: &str) -> Request<Body> {
        let mut request = Request::builder().method("POST").uri("/auth/guest").body(Body::empty()).unwrap();
        request.headers_mut().insert(FORWARDED_FOR, forwarded.parse().unwrap());
        // `main` serves with `into_make_service_with_connect_info`, so the
        // handler's `ConnectInfo` always resolves in production; supply it here
        // the way the server would.
        request.extensions_mut().insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 54321))));
        request
    }

    async fn token_of(response: Response) -> String {
        let bytes = axum::body::to_bytes(response.into_body(), 8 * 1024).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        body["token"].as_str().expect("the response carries a token").to_string()
    }

    /// The `exp` of a token whose signature has already been checked, in unix
    /// seconds. `SigningKeys::verify` hands back the custom claims only, so the
    /// lifetime is read out of the very payload that verification accepted.
    fn expiry_secs(token: &str) -> u64 {
        let payload = token.split('.').nth(1).expect("a JWT has three segments");
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload).expect("the payload is base64url");
        let claims: serde_json::Value = serde_json::from_slice(&bytes).expect("the payload is JSON");
        claims["exp"].as_u64().expect("a minted token carries exp")
    }

    #[tokio::test]
    async fn a_guest_token_names_the_guest_subject_and_role_and_expires_within_hours() {
        let response = app().oneshot(request("203.0.113.7")).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let token = token_of(response).await;

        // The verifying key is the whole contract with the client: a token this
        // endpoint mints must check out against the same key `/auth/session`'s
        // tokens do.
        let claims = test_keys().verify(&token).expect("the minted token verifies against the signing key");

        // THE LITERAL STRINGS, DELIBERATELY, not GUEST_SUB and GUEST_ROLE.
        // Asserting the constants against themselves proves only that the
        // handler used them: a rename of `GUEST_ROLE` to "member" would leave
        // this test, the constants and the live policy suite all green while
        // production minted member-role tokens. These two words are the wire
        // contract — what `policy.json`'s `guest` role is keyed on and what the
        // scopes compare against — so they are spelled out here, and changing
        // them has to be a deliberate act with this line edited to match.
        assert_eq!(claims.sub, "guest");
        assert_eq!(claims.roles, vec!["guest".to_string()]);
        assert!(claims.email.is_empty(), "a guest has no email");
        assert_eq!(claims.name, None);

        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        let ttl = GUEST_TOKEN_TTL_HOURS * 3600;
        let exp = expiry_secs(&token);
        assert!(exp <= now + ttl, "exp is at most the TTL out: {exp} vs {now}");
        assert!(exp > now + ttl - 60, "exp is the full TTL out, minus the moment spent minting: {exp} vs {now}");
        assert!(exp < now + crate::TOKEN_TTL_HOURS * 3600, "a guest session is far shorter than a member's");
    }

    #[tokio::test]
    async fn mints_past_the_budget_are_refused_with_a_reopening_time() {
        // Every request carries a DIFFERENT leading `X-Forwarded-For` entry and
        // the SAME trailing one, which is the shape a caller varying the header
        // produces: varying the left of the header buys no extra budget.
        //
        // What this does NOT show is that the trailing entry is what was
        // counted — every request here also shares one `ConnectInfo`, so peer
        // counting would produce the same eleven results.
        // `the_counted_address_is_the_last_forwarded_entry` is what separates
        // those two; this test is about the route returning 429 with a
        // reopening time once a budget is spent.
        let app = app();
        for n in 0..MAX_PER_CLIENT {
            let forwarded = format!("198.51.100.{n}, 203.0.113.7");
            let response = app.clone().oneshot(request(&forwarded)).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK, "mint {n} is inside the budget");
        }

        let response = app.oneshot(request("198.51.100.99, 203.0.113.7")).await.unwrap();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        let retry_after: u64 =
            response.headers().get(header::RETRY_AFTER).expect("a refusal says when to come back").to_str().unwrap().parse().unwrap();
        assert!(retry_after > 0 && retry_after <= WINDOW.as_secs() + 1, "{retry_after}");
    }
}
