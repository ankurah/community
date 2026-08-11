use ankurah::{property::Json, Context, EntityId, Node};
use ankurah_jwt_auth::{AuthError, Duration, JwtAgent, JwtClaims, JwtContext, SigningKeys};
use ankurah_websocket_server::WebsocketServer;
use anyhow::{Context as _, Result};
use axum::{
    body::Body,
    extract::{DefaultBodyLimit, State},
    http::{Request, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json as AxumJson, Router,
};
use community_model::{BanView, Room, RoomView, User, UserRoles, UserRolesView, UserView};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tower::ServiceExt as _;
use tower_http::{
    cors::CorsLayer,
    services::{ServeDir, ServeFile},
    trace::TraceLayer,
};
use tracing::{info, warn, Level};

mod ci_hook;
mod guest;
mod oidc;
mod push;
mod workers;
use oidc::{OidcVerifier, VerifiedIdentity};

#[cfg(all(feature = "sled", not(feature = "postgres")))]
use ankurah_storage_sled::SledStorageEngine;
#[cfg(feature = "postgres")]
use ankurah_storage_postgres::Postgres;

// Storage engine selected at generate time (the crate's default feature).
// dev.sh reads that choice back to decide whether to run a Postgres container.
#[cfg(all(feature = "sled", not(feature = "postgres")))]
type Storage = SledStorageEngine;
#[cfg(feature = "postgres")]
type Storage = Postgres;

/// Open the durable node's storage AND the server-owned device-token registry,
/// which is why one function returns both.
///
/// They come out together because they share one handle to one backend, and
/// that sharing is not a convenience. Sled takes a file lock on its directory,
/// so a second open of the same path fails outright; Postgres would otherwise
/// carry a second connection pool against the same database. The registry is
/// not an ankurah collection — see `push::store` for why a device token must
/// not be one — so it needs the backend directly rather than through the node.
#[cfg(all(feature = "sled", not(feature = "postgres")))]
async fn make_storage() -> Result<(Storage, Arc<dyn push::DeviceTokens>)> {
    let engine = SledStorageEngine::with_homedir_folder(".community")?;
    let device_tokens = push::store::open_sled(&engine)?;
    Ok((engine, device_tokens))
}

#[cfg(feature = "postgres")]
async fn make_storage() -> Result<(Storage, Arc<dyn push::DeviceTokens>)> {
    use ankurah_storage_postgres::{DEFAULT_CONNECTION_TIMEOUT_SECS, DEFAULT_POOL_SIZE};
    use bb8_postgres::{tokio_postgres::NoTls, PostgresConnectionManager};

    // DATABASE_URL is provided by dev.sh (dev: randomized-port container) or by
    // Cloud Run (prod: Cloud SQL socket). The fallback is only for direct runs.
    let uri = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://postgres:ankurah@localhost:5432/community".to_string());

    // The pool is built here rather than by `Postgres::open` so that both
    // readers of this database share it. `Postgres::new` is the storage
    // engine's own sanctioned entry point for exactly this — its `open` is the
    // convenience wrapper around these three lines — and the sizes come from
    // the engine's constants so the two cannot drift.
    let manager = PostgresConnectionManager::new_from_stringlike(uri.as_str(), NoTls)?;
    let pool = bb8::Pool::builder()
        .max_size(DEFAULT_POOL_SIZE)
        .connection_timeout(std::time::Duration::from_secs(DEFAULT_CONNECTION_TIMEOUT_SECS))
        .build(manager)
        .await?;

    let device_tokens = push::store::open_postgres(pool.clone()).await?;
    Ok((Postgres::new(pool)?, device_tokens))
}

/// Ankurah access-token lifetime. Long-ish because there is no refresh flow yet:
/// when it expires the client re-runs the OIDC dance (usually click-free while
/// the idp.to session is still valid).
const TOKEN_TTL_HOURS: u64 = 12;

/// Shared HTTP state.
#[derive(Clone)]
struct AppState {
    /// Where the built SPA (`trunk build`) lives on disk.
    static_dir: PathBuf,
    /// Privileged (Root) context, for room seeding and `User` upserts.
    system_ctx: Context,
    /// Our RS256 keypair — mints ankurah session tokens after federation.
    signing_keys: SigningKeys,
    /// Validates incoming idp.to ID tokens against their JWKS.
    oidc: Arc<OidcVerifier>,
    /// Seeded identities + shared secret for `POST /hooks/ci` (#66).
    ci: ci_hook::CiHook,
    /// Signing key + mint budgets for `POST /auth/guest` (#79).
    guest: guest::GuestMint,
    /// Verifying key + device-token store for `POST /push/register`.
    push: push::PushRegistry,
}

/// The CI webhook handler asks for its own state, not the whole `AppState` —
/// it has no business with the signing keys or the OIDC verifier. This is what
/// lets axum hand it just the piece it needs.
impl axum::extract::FromRef<AppState> for ci_hook::CiHook {
    fn from_ref(state: &AppState) -> Self { state.ci.clone() }
}

/// Same shape for the guest mint: it wants the signing key and its own mint
/// budgets, and has no business with the OIDC verifier or the node.
impl axum::extract::FromRef<AppState> for guest::GuestMint {
    fn from_ref(state: &AppState) -> Self { state.guest.clone() }
}

/// And for the device-token registry: it wants the verifying key and the token
/// store, and has no business with the node — which is what lets the route be
/// tested without one.
impl axum::extract::FromRef<AppState> for push::PushRegistry {
    fn from_ref(state: &AppState) -> Self { state.push.clone() }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().with_max_level(Level::INFO).init();

    // Initialize storage engine (Sled or Postgres — see this crate's features),
    // and the device-token registry that shares its handle.
    let (storage, device_tokens) = make_storage().await?;

    // RS256 signing key: env PEM in prod (Secret Manager), generated dev key otherwise.
    let signing_keys = load_signing_keys()?;

    // Policy file: `/app/policy.json` in the image (POLICY_PATH), repo-root
    // `policy.json` in dev (server runs with cwd = repo root). `new_durable`
    // reads it now; the `watcher` feature publishes it to the `jwtpolicy`
    // collection so ephemeral clients can sync roles + verifying key.
    let policy_path = std::env::var("POLICY_PATH").unwrap_or_else(|_| "policy.json".to_string());
    let agent = JwtAgent::new_durable(signing_keys.clone(), &policy_path)
        .with_context(|| format!("failed to load policy from {policy_path}"))?;

    let node = Node::new_durable(Arc::new(storage), agent);

    node.system.wait_loaded().await;
    if node.system.root().is_none() {
        node.system.create().await?;
    }
    node.system.wait_system_ready().await;

    // Privileged context that bypasses RBAC — used for seeding + upserts.
    let system_ctx = node.context_async(JwtContext::system()).await;

    // Seed the default community rooms (idempotent).
    ensure_default_rooms(&system_ctx).await?;

    // Seed the #ci room and the system "CI" user that authors its messages,
    // and read the webhook secret. Also idempotent; see ci_hook.rs.
    let ci = ci_hook::seed(&system_ctx).await?;

    // Server-side reactive workers (mention fan-out → notification rows,
    // link unfurl → linkpreview rows, notification rows → push alerts): a
    // standing message LiveQuery on the privileged context — the durable-node
    // sibling of the policy watcher that `JwtAgent` already started via
    // on_node_ready. See workers/mod.rs.
    //
    // The push sender is the one worker a deployment can be without: with no
    // APNs credentials in the environment it is never started, and the line
    // saying so is written by `delivery_from_env` here rather than at the first
    // send. The registry keeps filing device tokens either way.
    let push_delivery = workers::push::delivery_from_env(device_tokens.clone());
    workers::start(system_ctx.clone(), push_delivery);

    // Where the built SPA lives. In prod the container copies `trunk build`
    // output here; in dev the dir is absent and trunk serves the SPA itself
    // (only /ws is proxied to this server), so the SPA fallback simply 404s.
    let static_dir = std::env::var("STATIC_DIR").unwrap_or_else(|_| "static".to_string());
    let state = AppState {
        static_dir: PathBuf::from(static_dir),
        system_ctx,
        guest: guest::GuestMint::new(signing_keys.clone()),
        push: push::PushRegistry::new(signing_keys.clone(), device_tokens),
        signing_keys,
        oidc: Arc::new(OidcVerifier::from_env()),
        ci,
    };

    // One axum app serves the Ankurah /ws endpoint, the OIDC mint endpoint, AND
    // the static SPA on a single port, so the browser client is same-origin
    // (mirrors idp.to). The ws upgrade handler comes straight from
    // ankurah-websocket-server.
    let ws_server = WebsocketServer::new(node);
    let app = Router::new()
        .route("/ws", get(ws_server.route_handler()))
        .route("/health", get(health))
        .route("/auth/session", post(auth_session))
        // Guest sessions (#79): no IdP round-trip, no body, any browser may
        // ask. The handler rate limits itself — with identity free and nothing
        // persisted, a mint budget is what stands in for a ban. See guest.rs.
        .route("/auth/guest", post(guest::handle))
        // CI status webhook (#66). Not an auth route: it authenticates itself
        // with an HMAC over the raw body, never an IdP token. See ci_hook.rs.
        //
        // The body limit is what keeps an unauthenticated caller from making us
        // buffer: the handler takes `Bytes`, so the request is fully
        // materialized before the handler can check anything — at axum's 2 MiB
        // default, times this service's concurrency of 40. Refusing at the
        // layer means the bytes are never collected in the first place.
        .route("/hooks/ci", post(ci_hook::handle).layer(DefaultBodyLimit::max(ci_hook::MAX_BODY_BYTES)))
        // Device-token registry for mobile push. Unlike the two auth routes it
        // CONSUMES a session token rather than minting one — the first route
        // here to do so — and it accepts only a member's: a guest has no
        // account to file a device under. The body limit is the CI webhook's
        // reasoning at a registration's size: the handler reads bytes, so the
        // request is buffered before it can check who is calling.
        .route(
            "/push/register",
            post(push::registry::handle).layer(DefaultBodyLimit::max(push::registry::MAX_BODY_BYTES)),
        )
        .fallback(spa_fallback)
        .with_state(state)
        // Permissive CORS so cross-origin callers (e.g. a native/RN client on a
        // different origin) can POST /auth/session. Same-origin web is unaffected.
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http());

    // Cloud Run injects PORT; dev.sh sets SERVER_PORT; default 8080.
    let port = std::env::var("PORT")
        .or_else(|_| std::env::var("SERVER_PORT"))
        .unwrap_or_else(|_| "8080".to_string());
    let bind_addr: SocketAddr = format!("0.0.0.0:{port}").parse().context("invalid bind address")?;

    let listener = tokio::net::TcpListener::bind(bind_addr)
        .await
        .with_context(|| format!("failed to bind community-server on {bind_addr}"))?;
    info!("community-server listening on {}", listener.local_addr()?);

    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>()).await?;

    Ok(())
}

/// Load the RS256 signing key. In prod `ANKURAH_JWT_SIGNING_KEY` holds the PEM
/// (from Secret Manager); in dev we generate an ephemeral key and warn. We
/// deliberately do NOT persist a generated key to disk, to keep private keys out
/// of the working tree — the tradeoff is that dev sessions reset on restart.
fn load_signing_keys() -> Result<SigningKeys> {
    match std::env::var("ANKURAH_JWT_SIGNING_KEY") {
        Ok(pem) if !pem.trim().is_empty() => {
            info!("loading JWT signing key from ANKURAH_JWT_SIGNING_KEY");
            SigningKeys::from_pem(&pem).context("parse ANKURAH_JWT_SIGNING_KEY as PEM")
        }
        _ => {
            warn!("ANKURAH_JWT_SIGNING_KEY unset — generating an ephemeral dev signing key (sessions reset on restart)");
            SigningKeys::generate().context("generate dev signing key")
        }
    }
}

async fn health() -> &'static str {
    "ok"
}

#[derive(Deserialize)]
struct SessionRequest {
    /// The idp.to ID token obtained by the client's PKCE code exchange.
    id_token: String,
    /// The nonce the client stashed before redirecting — REQUIRED, and
    /// checked against the token's `nonce` claim. This is what makes a leaked
    /// or replayed id_token useless here: only the browser that started the
    /// sign-in knows the nonce that the token was minted against.
    nonce: String,
}

/// What both mints return — `auth_session` for a member, `guest::handle` for a
/// visitor — so a client parses one shape whichever way it got its session.
#[derive(Serialize)]
struct SessionResponse {
    /// A freshly minted ankurah session token (RS256, signed by us).
    token: String,
}

/// Sign one ankurah session token.
///
/// FOR: both auth routes end here, so there is exactly one place where a claim
/// set and a lifetime become a signed token — the member path with a federated
/// idp.to identity, the guest path with nothing to federate.
///
/// The ops line is NOT written here, deliberately. A member signing in and a
/// visitor taking a guest session are different events, and an operator
/// filtering the log for one must not be counting the other — so each route
/// writes its own line under its own message. Neither writes the token.
pub(crate) fn mint_session_token(keys: &SigningKeys, claims: &JwtClaims, ttl_hours: u64) -> Result<String, AuthError> {
    keys.sign(claims, Duration::from_hours(ttl_hours))
}

/// Federate-and-remint: validate an idp.to ID token, upsert the `User` keyed on
/// its `sub`, and return an ankurah session token the client attaches to its
/// node context. This is the only trust boundary — everything downstream is
/// enforced by `JwtAgent` against `policy.json`.
async fn auth_session(
    State(state): State<AppState>,
    AxumJson(req): AxumJson<SessionRequest>,
) -> Result<AxumJson<SessionResponse>, (StatusCode, String)> {
    let identity = state
        .oidc
        .verify(&req.id_token, Some(req.nonce.as_str()))
        .await
        .map_err(|e| (StatusCode::UNAUTHORIZED, format!("invalid ID token: {e}")))?;

    let user_id = upsert_user(&state.system_ctx, &identity)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("user upsert failed: {e}")))?;

    // Ban gate: an actively banned user cannot mint a new session. (Live
    // enforcement on existing connections is the guarded-agent follow-up.)
    if let Some(reason) = active_ban_reason(&state.system_ctx, &user_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("ban check failed: {e}")))?
    {
        return Err((StatusCode::FORBIDDEN, format!("This account is banned: {reason}")));
    }

    // Roles come straight from the verified id_token (idp.to owns them); this
    // just normalizes and applies the `member` floor. No storage read.
    let roles = resolve_roles(&identity.roles);

    // Mirror the resolved roles into the server-maintained `UserRoles` cache so
    // the UI can render badges without decoding the caller's JWT. Best-effort:
    // the roles baked into the token below are the source of truth, so a cache
    // write failure must not fail an otherwise-valid sign-in (badges may lag).
    if let Err(e) = upsert_user_roles(&state.system_ctx, &user_id, &roles).await {
        warn!(user = %user_id, "failed to update UserRoles cache (badges may be stale): {e}");
    }

    // `oidc_sub` rides along as a custom claim so the policy's user-collection
    // write scope (`oidc_sub = $jwt.custom.oidc_sub`) pins profile edits to the
    // caller's own row.
    let mut custom = serde_json::Map::new();
    custom.insert("oidc_sub".to_string(), serde_json::Value::String(identity.sub.clone()));

    let claims = JwtClaims { sub: user_id, roles, email: identity.email.unwrap_or_default(), name: identity.name, custom };

    let token = mint_session_token(&state.signing_keys, &claims, TOKEN_TTL_HOURS)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("failed to mint session token: {e}")))?;

    // Ops trail: every mint is visible (who, which entity, which roles) so an
    // unexpected role would show up here. Never log the token itself.
    info!(user = %claims.sub, email = %claims.email, roles = ?claims.roles, "minted session token");

    Ok(AxumJson(SessionResponse { token }))
}

/// Resolve the roles to mint into a user's session token.
///
/// Source of truth: the idp.to `roles` claim on the verified id_token (passed
/// through as `identity.roles`). user↔role management is 100% idp.to's
/// responsibility — the claim carries stable lowercase role keys (e.g.
/// `["member","moderator"]`), aud-scoped to our Application and gated by the
/// `roles` scope. We take those keys verbatim, only normalizing defensively
/// (trim, lowercase, dedup) and always ensuring `member` as the floor so every
/// authenticated user can view/post.
///
/// An absent/empty claim → `["member"]` only. This is the pre-rollout default
/// (prod ships before idp.to emits the claim; today no one has roles) and stays
/// the steady-state behavior for ordinary members.
fn resolve_roles(identity_roles: &[String]) -> Vec<String> {
    let mut roles = vec!["member".to_string()];
    for raw in identity_roles {
        let key = raw.trim().to_ascii_lowercase();
        if !key.is_empty() && !roles.contains(&key) {
            roles.push(key);
        }
    }
    roles
}

/// The reason of an active ban for this user, if any.
async fn active_ban_reason(ctx: &Context, user_id: &str) -> Result<Option<String>> {
    for ban in ctx.fetch::<BanView>("true").await? {
        if ban.active()? && ban.user()?.id().to_base64() == user_id {
            return Ok(Some(ban.reason()?));
        }
    }
    Ok(None)
}

/// Find the `User` for this idp.to subject, or create one. Returns the User
/// entity id (base64) — this becomes the ankurah token `sub`, which the message
/// write-scope (`user = $jwt.sub`) matches against `Message.user`.
///
/// We scan-and-filter rather than query by predicate: AnkQL has no string-escape
/// syntax and this sidesteps Option-field indexing edge cases. Sign-in is not a
/// hot path and the community user set is small.
async fn upsert_user(ctx: &Context, identity: &VerifiedIdentity) -> Result<String> {
    for user in ctx.fetch::<UserView>("true").await? {
        if user.oidc_sub()?.as_deref() == Some(identity.sub.as_str()) {
            return Ok(user.id().to_base64());
        }
    }

    let display_name = identity
        .name
        .clone()
        .or_else(|| identity.email.clone())
        .unwrap_or_else(|| "Member".to_string());

    let trx = ctx.begin();
    let created = trx.create(&User { display_name, oidc_sub: Some(identity.sub.clone()) }).await?;
    let user_id = created.id().to_base64();
    trx.commit().await?;
    Ok(user_id)
}

/// Upsert the server-maintained `UserRoles` display cache for this user.
///
/// Scan-and-filter by the user ref (same rationale as `upsert_user`: sign-in is
/// cold, the community user set is small, and this sidesteps AnkQL Ref-predicate
/// edge cases). Creates the row when absent; otherwise edits it only when the
/// role set changed, to avoid pointless writes/syncs.
///
/// Runs under the privileged Root context (`system_ctx`), which bypasses policy.
/// That is the whole point of the `userroles` write privilege being `system` (a
/// privilege no role holds): only this local server path may write the cache,
/// so remote clients can never spoof their own role badges.
async fn upsert_user_roles(ctx: &Context, user_id: &str, roles: &[String]) -> Result<()> {
    let roles_value =
        serde_json::Value::Array(roles.iter().map(|r| serde_json::Value::String(r.clone())).collect());

    for existing in ctx.fetch::<UserRolesView>("true").await? {
        if existing.user()?.id().to_base64() == user_id {
            // Already current — skip the write to avoid churn.
            if existing.roles()?.into_inner() == roles_value {
                return Ok(());
            }
            let trx = ctx.begin();
            let mutable = existing.edit(&trx)?;
            mutable.roles().set(&Json::new(roles_value.clone()))?;
            trx.commit().await?;
            return Ok(());
        }
    }

    let trx = ctx.begin();
    trx.create(&UserRoles { user: EntityId::from_base64(user_id)?.into(), roles: Json::new(roles_value) })
        .await?;
    trx.commit().await?;
    Ok(())
}

/// Serve the SPA: real files from `static_dir`, with `index.html` as the
/// client-side-routing fallback (this is what serves `/auth/callback`). Backend
/// paths never fall through to the SPA.
async fn spa_fallback(State(state): State<AppState>, request: Request<Body>) -> Response {
    let path = request.uri().path();
    if is_backend_path(path) {
        return StatusCode::NOT_FOUND.into_response();
    }

    let index = state.static_dir.join("index.html");
    let service = ServeDir::new(&state.static_dir).fallback(ServeFile::new(index));

    service.oneshot(request).await.map(IntoResponse::into_response).unwrap_or_else(|error| {
        (StatusCode::INTERNAL_SERVER_ERROR, format!("failed to serve assets: {error}")).into_response()
    })
}

/// Backend routes, listed one by one rather than matched by prefix: `/auth/` is
/// NOT a backend namespace — `/auth/callback` is a client-side route the SPA
/// fallback has to serve.
fn is_backend_path(path: &str) -> bool {
    path == "/ws"
        || path.starts_with("/ws/")
        || path == "/health"
        || path == "/auth/session"
        || path == "/auth/guest"
        || path.starts_with("/hooks/")
        || path.starts_with("/push/")
}

/// Seed the default rooms for the community. Idempotent — only creates rooms
/// that don't already exist, so it's safe to run on every boot. Runs under the
/// privileged system context (bypasses RBAC).
///
/// `#ci` is deliberately not in this list: it is seeded by `ci_hook::seed`,
/// which also needs the room's id, so that subsystem owns its own room.
async fn ensure_default_rooms(ctx: &Context) -> Result<()> {
    const DEFAULT_ROOMS: &[&str] = &["general", "support", "announcements", "introductions"];

    for name in DEFAULT_ROOMS {
        ensure_room(ctx, name).await?;
    }

    Ok(())
}

/// Create a room by name if it does not exist; return its id either way.
/// Idempotent, and the id is what lets a caller (the CI hook) hold onto the
/// room it seeded instead of looking it up by name per request.
pub(crate) async fn ensure_room(ctx: &Context, name: &str) -> Result<EntityId> {
    // Parameterized rather than spliced into the query text (#17) — the
    // names are constants today, but predicates built with format! are
    // exactly the idiom that issue bans.
    let selection = ankurah::ankql::parser::parse_selection("name = ?")?
        .predicate
        .populate([ankurah::ankql::ast::Expr::from(name)])?;
    if let Some(existing) = ctx.fetch::<RoomView>(selection).await?.into_iter().next() {
        return Ok(existing.id());
    }

    info!("Creating '{name}' room");
    let trx = ctx.begin();
    let created = trx.create(&Room { name: name.to_string(), created_by: None, topic: None }).await?;
    let id = created.id();
    trx.commit().await?;
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::resolve_roles;

    #[test]
    fn absent_claim_is_member_only() {
        // Today's prod reality: no roles claim → member floor only.
        assert_eq!(resolve_roles(&[]), vec!["member".to_string()]);
    }

    #[test]
    fn member_is_always_the_floor() {
        // A claim that omits member still yields member (plus the granted role).
        assert_eq!(resolve_roles(&["moderator".to_string()]), vec!["member".to_string(), "moderator".to_string()]);
    }

    #[test]
    fn roles_are_normalized_trim_and_lowercase() {
        let input = vec!["  Moderator ".to_string(), "ADMIN".to_string()];
        assert_eq!(
            resolve_roles(&input),
            vec!["member".to_string(), "moderator".to_string(), "admin".to_string()]
        );
    }

    #[test]
    fn duplicates_are_deduped_including_the_member_floor() {
        let input = vec!["member".to_string(), "Member".to_string(), "moderator".to_string(), "moderator".to_string()];
        assert_eq!(resolve_roles(&input), vec!["member".to_string(), "moderator".to_string()]);
    }

    #[test]
    fn empty_and_whitespace_only_entries_are_dropped() {
        let input = vec!["".to_string(), "   ".to_string(), "moderator".to_string()];
        assert_eq!(resolve_roles(&input), vec!["member".to_string(), "moderator".to_string()]);
    }
}
