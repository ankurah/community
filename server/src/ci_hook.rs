//! `POST /hooks/ci` — GitHub Actions CI conclusions become messages in the
//! `#ci` room (#66).
//!
//! FOR: the community should see whether ankurah's CI is green without leaving
//! chat, and the personal-website embed (#46) wants that status as a live
//! surface. A GitHub Actions job POSTs a workflow's conclusion here the moment
//! it finishes; this module turns that POST into one chat message.
//!
//! # Trust boundary
//!
//! This is NOT the auth path. No IdP is involved, no session token is minted,
//! and the caller gets no `User` of its own. The caller instead proves it holds
//! a shared secret by signing the RAW request body with HMAC-SHA256, in the
//! header shape svix popularized — so the signer on the Actions side is three
//! lines of `openssl`:
//!
//! - `webhook-id` — opaque, unique per delivery; the replay/dedup key.
//! - `webhook-timestamp` — unix seconds, which must sit within
//!   [`TIMESTAMP_TOLERANCE_SECS`] of our clock in either direction.
//! - `webhook-signature` — one or more space-separated `v1,<base64 tag>`
//!   values, where the tag is HMAC-SHA256 over `"<id>.<timestamp>.<raw body>"`.
//!   Several values are accepted so a secret can be rotated by signing with the
//!   old and the new key at once.
//!
//! The timestamp window and the id dedup both defeat replay: a captured request
//! is worthless five minutes later, and worthless immediately because its id is
//! already spent.
//!
//! # Write path
//!
//! The message is created by the server's own privileged (Root) context and
//! authored by a seeded system `User` displayed as "CI" — the same "only the
//! server may write these rows" shape as the notification fan-out and unfurl
//! workers. Nothing here mints a token or consults policy.
//!
//! # The payload is authenticated, not trustworthy
//!
//! A valid signature proves WHO called, not that the fields are benign.
//! `branch` in particular is whatever a contributor named the head branch of a
//! pull request, on a public repo. Every field is squeezed through
//! [`sanitize`] before it reaches message text, so a branch named
//! `<@AZk3jW0RvkW8pTGnQxYzAA>` cannot spam mention notifications, a newline
//! cannot forge extra lines, and `[click](https://evil.example)` cannot become
//! a rendered link (`leptos-app/src/markdown.rs` would otherwise honor it).

use std::collections::{HashSet, VecDeque};
use std::sync::{Arc, Mutex};

use ankurah::{Context, EntityId};
use anyhow::{Context as _, Result};
use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json as AxumJson,
};
use base64::Engine as _;
use community_model::{Message, User, UserView};
use hmac::{Hmac, Mac};
use serde::Deserialize;
use sha2::Sha256;
use tracing::{info, warn};

use crate::workers::now_ms;

/// The room CI reports land in, seeded at boot exactly like the default rooms.
/// The `#` is a UI affordance; room names are stored bare.
pub const CI_ROOM_NAME: &str = "ci";

/// Synthetic OIDC subject for the message author. `User` rows are keyed on
/// `oidc_sub` (see `upsert_user`), so a reserved `system:` value gives CI a
/// stable identity across restarts that no real idp.to subject can collide
/// with — idp.to subjects are opaque provider ids, never `system:*`.
const CI_OIDC_SUB: &str = "system:ci";

/// How the CI author renders in the member list and above its messages.
const CI_DISPLAY_NAME: &str = "CI";

/// Env var carrying the shared HMAC secret. Populated in production from Secret
/// Manager by the Cloud Run deploy (see `.github/workflows/deploy.yml`), the
/// same way `ANKURAH_JWT_SIGNING_KEY` is. Unset closes the endpoint rather than
/// opening it — see [`seed`].
const SECRET_ENV: &str = "CI_HOOK_SECRET";

/// How far a delivery's `webhook-timestamp` may sit from our clock, in either
/// direction. Five minutes tolerates runner/Cloud-Run clock skew and a retry or
/// two, while keeping a captured request useless soon after capture.
const TIMESTAMP_TOLERANCE_SECS: i64 = 300;

/// Largest body we will even authenticate. A CI report is a few hundred bytes;
/// this bounds the work an unauthenticated caller can commission, since the
/// HMAC runs before we know whether the caller is legitimate.
const MAX_BODY_BYTES: usize = 16 * 1024;

/// Longest `webhook-id` we accept. The id is remembered for replay rejection,
/// so it must not be an unbounded caller-chosen string.
const MAX_WEBHOOK_ID_LEN: usize = 128;

/// How many delivery ids the replay guard remembers. Far more than the
/// timestamp window can admit — a workflow finishing every few seconds for five
/// minutes is ~100 — and that window is the real bound anyway; this cache only
/// has to cover it.
const SEEN_CAPACITY: usize = 512;

type HmacSha256 = Hmac<Sha256>;

/// Signature tags travel base64-encoded (the svix convention, and one `openssl
/// … -binary | base64` away in a shell).
const SIGNATURE_B64: base64::engine::general_purpose::GeneralPurpose = base64::engine::general_purpose::STANDARD;

/// Everything the endpoint needs: the write context, the identities resolved at
/// boot, the secret, and the replay guard. `AppState` holds one and hands it to
/// the handler through `FromRef`; the `Arc` keeps the guard shared across every
/// request.
///
/// Owning the privileged context here — rather than reaching into `AppState`
/// for it — is the same shape the workers use (each owns a cloned `Context`),
/// and it is what lets the route be tested without standing up the auth state
/// the endpoint has nothing to do with.
#[derive(Clone)]
pub struct CiHook(Arc<Inner>);

struct Inner {
    /// The durable node's privileged (Root) context — the only writer of CI
    /// messages.
    ctx: Context,
    /// `None` when [`SECRET_ENV`] is unset or blank. The endpoint then refuses
    /// every delivery with 503: an unconfigured webhook accepts nothing, rather
    /// than accepting everything.
    secret: Option<String>,
    /// The seeded system `User` that authors CI messages.
    author: EntityId,
    /// The seeded `#ci` room. Resolved once at boot and cached, so no request
    /// ever depends on a name lookup landing on the right row.
    room: EntityId,
    seen: Mutex<SeenDeliveries>,
}

/// Seed the CI identity and read the shared secret. Idempotent, like
/// `ensure_default_rooms`, and run under the privileged (Root) context.
///
/// Seeding happens whether or not the secret is configured, so `#ci` and its
/// author exist from the first boot — only the HTTP endpoint depends on the
/// secret.
pub async fn seed(ctx: &Context) -> Result<CiHook> {
    let secret = std::env::var(SECRET_ENV).ok().filter(|s| !s.trim().is_empty());
    if secret.is_none() {
        warn!("{SECRET_ENV} is unset — POST /hooks/ci answers 503 until it is set (see .github/workflows/deploy.yml)");
    }
    seed_with_secret(ctx, secret).await
}

/// [`seed`] minus the environment read, so tests can seed a configured hook
/// without mutating process-wide state.
async fn seed_with_secret(ctx: &Context, secret: Option<String>) -> Result<CiHook> {
    let author = ensure_ci_user(ctx).await.context("seed the CI system user")?;
    let room = crate::ensure_room(ctx, CI_ROOM_NAME).await.context("seed the #ci room")?;
    info!(user = %author, room = %room, configured = secret.is_some(), "ci hook seeded");
    Ok(CiHook(Arc::new(Inner { ctx: ctx.clone(), secret, author, room, seen: Mutex::new(SeenDeliveries::default()) })))
}

/// Find the system CI `User`, or create it. Scan-and-filter on `oidc_sub` for
/// the same reasons as `upsert_user`: AnkQL has no string-escape syntax, this
/// sidesteps Option-field indexing edge cases, and it runs once per boot over a
/// small user set.
async fn ensure_ci_user(ctx: &Context) -> Result<EntityId> {
    for user in ctx.fetch::<UserView>("true").await? {
        if user.oidc_sub()?.as_deref() == Some(CI_OIDC_SUB) {
            return Ok(user.id());
        }
    }
    info!("Creating the '{CI_DISPLAY_NAME}' system user");
    let trx = ctx.begin();
    let created =
        trx.create(&User { display_name: CI_DISPLAY_NAME.to_string(), oidc_sub: Some(CI_OIDC_SUB.to_string()) }).await?;
    let id = created.id();
    trx.commit().await?;
    Ok(id)
}

/// A signed CI report. Unknown fields — the reporter also sends `run_id` and
/// `actor` — are accepted and ignored by serde's default, so the reporter and
/// the server can be deployed independently and the message renders only what
/// fits on two lines.
#[derive(Debug, Deserialize)]
struct CiReport {
    /// `owner/repo`, e.g. `ankurah/ankurah`.
    repo: String,
    /// Workflow display name, e.g. `Tests`.
    workflow: String,
    /// Head branch of the run.
    branch: String,
    /// Head commit; only the leading hex is rendered.
    sha: String,
    /// GitHub's conclusion string: `success`, `failure`, `cancelled`, …
    conclusion: String,
    /// Link to the run page on github.com.
    run_url: String,
}

/// Why a delivery was refused. `reason` is client-visible on purpose — someone
/// debugging their signer needs to know which check failed — and never carries
/// anything derived from the secret or from the expected tag.
#[derive(Debug, PartialEq, Eq)]
struct Rejected {
    status: StatusCode,
    reason: &'static str,
}

impl Rejected {
    fn new(status: StatusCode, reason: &'static str) -> Self { Self { status, reason } }
}

/// What an accepted delivery did.
#[derive(Debug, PartialEq, Eq)]
enum Accepted {
    /// A new message row, carrying its id.
    Posted(EntityId),
    /// This `webhook-id` was already delivered; nothing was written.
    Duplicate,
}

/// The route handler: authenticate, then post. Thin on purpose — everything
/// worth testing lives in [`deliver`], [`authenticate`] and [`format_message`].
pub async fn handle(State(hook): State<CiHook>, headers: HeaderMap, body: Bytes) -> Response {
    match deliver(&hook, &headers, &body, now_secs()).await {
        Ok(Accepted::Posted(id)) => {
            (StatusCode::OK, AxumJson(serde_json::json!({ "status": "ok", "message": id.to_base64() }))).into_response()
        }
        Ok(Accepted::Duplicate) => (StatusCode::OK, AxumJson(serde_json::json!({ "status": "duplicate" }))).into_response(),
        Err(rejected) => {
            // Ops trail: every refusal is visible, naming the check that
            // failed. Never log the body or a header value — the body is
            // unverified and the signature header is a credential artifact.
            warn!(status = %rejected.status, "ci hook rejected a delivery: {}", rejected.reason);
            (rejected.status, AxumJson(serde_json::json!({ "error": rejected.reason }))).into_response()
        }
    }
}

/// Authenticate a delivery and, if it is new, write its message.
///
/// `now_secs` is a parameter rather than a clock read so the timestamp window
/// is testable.
async fn deliver(hook: &CiHook, headers: &HeaderMap, body: &[u8], now_secs: i64) -> Result<Accepted, Rejected> {
    let Some(secret) = hook.0.secret.as_deref() else {
        return Err(Rejected::new(StatusCode::SERVICE_UNAVAILABLE, "ci hook is not configured"));
    };
    if body.len() > MAX_BODY_BYTES {
        return Err(Rejected::new(StatusCode::PAYLOAD_TOO_LARGE, "body too large"));
    }

    let id = authenticate(secret, headers, body, now_secs)?;

    // Claim the id BEFORE writing, so two concurrent copies of one delivery
    // cannot both post. A failed write releases the claim again, so a genuine
    // retry of a delivery that never landed still gets through.
    if !hook.claim(&id) {
        return Ok(Accepted::Duplicate);
    }

    let report: CiReport = match serde_json::from_slice(body) {
        Ok(report) => report,
        Err(_) => {
            hook.release(&id);
            return Err(Rejected::new(StatusCode::BAD_REQUEST, "body is not a valid CI report"));
        }
    };

    match post_message(hook, &format_message(&report)).await {
        Ok(message_id) => {
            info!(
                message = %message_id,
                repo = %report.repo,
                workflow = %report.workflow,
                conclusion = %report.conclusion,
                "ci report posted to #ci"
            );
            Ok(Accepted::Posted(message_id))
        }
        Err(e) => {
            hook.release(&id);
            warn!("ci hook failed to write the message: {e:#}");
            Err(Rejected::new(StatusCode::INTERNAL_SERVER_ERROR, "failed to write the message"))
        }
    }
}

/// Verify the svix-style headers against the raw body. Returns the
/// `webhook-id` on success — the caller uses it as the replay key.
///
/// Order matters: cheap structural checks first, then the timestamp window,
/// then the tag. The tag comparison itself is [`Mac::verify_slice`], which is
/// constant-time.
fn authenticate(secret: &str, headers: &HeaderMap, body: &[u8], now_secs: i64) -> Result<String, Rejected> {
    let id = header(headers, "webhook-id")?;
    let timestamp = header(headers, "webhook-timestamp")?;
    let signature = header(headers, "webhook-signature")?;

    if id.is_empty() || id.len() > MAX_WEBHOOK_ID_LEN {
        return Err(Rejected::new(StatusCode::UNAUTHORIZED, "webhook-id is empty or too long"));
    }

    let sent_at: i64 = timestamp
        .parse()
        .map_err(|_| Rejected::new(StatusCode::UNAUTHORIZED, "webhook-timestamp is not a unix second count"))?;
    if (now_secs - sent_at).abs() > TIMESTAMP_TOLERANCE_SECS {
        return Err(Rejected::new(StatusCode::UNAUTHORIZED, "webhook-timestamp is outside the accepted window"));
    }

    // The signed content binds the id and the timestamp to the body, so none of
    // the three can be swapped between deliveries.
    let mut signed = Vec::with_capacity(id.len() + timestamp.len() + body.len() + 2);
    signed.extend_from_slice(id.as_bytes());
    signed.push(b'.');
    signed.extend_from_slice(timestamp.as_bytes());
    signed.push(b'.');
    signed.extend_from_slice(body);

    // Any `v1,` value may match (several support secret rotation), and a
    // malformed candidate is skipped rather than fatal, for the same reason.
    for candidate in signature.split_whitespace() {
        let Some(encoded) = candidate.strip_prefix("v1,") else { continue };
        let Ok(tag) = SIGNATURE_B64.decode(encoded) else { continue };
        let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC accepts a key of any length");
        mac.update(&signed);
        if mac.verify_slice(&tag).is_ok() {
            return Ok(id.to_string());
        }
    }
    Err(Rejected::new(StatusCode::UNAUTHORIZED, "webhook-signature does not match"))
}

/// Read a header as UTF-8. Absent and non-UTF-8 are the same refusal: an
/// unauthenticated caller learns only that its headers were unusable.
fn header<'h>(headers: &'h HeaderMap, name: &'static str) -> Result<&'h str, Rejected> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| Rejected::new(StatusCode::UNAUTHORIZED, missing_header_reason(name)))
}

/// Static refusal text per header, so `Rejected.reason` stays a `&'static str`
/// and the unauthenticated path never allocates.
fn missing_header_reason(name: &'static str) -> &'static str {
    match name {
        "webhook-id" => "missing or malformed webhook-id header",
        "webhook-timestamp" => "missing or malformed webhook-timestamp header",
        _ => "missing or malformed webhook-signature header",
    }
}

/// Write one message into `#ci`, authored by the CI system user, through the
/// hook's privileged context.
async fn post_message(hook: &CiHook, text: &str) -> Result<EntityId> {
    let trx = hook.0.ctx.begin();
    let created = trx
        .create(&Message {
            user: hook.0.author.into(),
            room: hook.0.room.into(),
            text: text.to_string(),
            timestamp: now_ms(),
            deleted: false,
            edited_at: None,
            collaborative: None,
            re: None,
        })
        .await
        .context("create the ci message")?;
    let id = created.id();
    trx.commit().await.context("commit the ci message")?;
    Ok(id)
}

/// Render a report as chat text: a status line, then the run link.
///
/// Every interpolated field is [`sanitize`]d first — see the module header for
/// why a signed payload is still untrusted content.
fn format_message(report: &CiReport) -> String {
    let icon = match report.conclusion.as_str() {
        "success" => "✅",
        // A timeout is a red build to everyone who reads this channel.
        "failure" | "timed_out" => "❌",
        // cancelled, skipped, neutral, action_required, stale, and whatever
        // GitHub adds later: neither green nor broken.
        _ => "⚪",
    };
    let repo = sanitize(&report.repo, 64);
    let workflow = sanitize(&report.workflow, 64);
    let branch = sanitize(&report.branch, 64);
    let conclusion = sanitize(&report.conclusion, 24);
    let sha = short_sha(&report.sha);

    let status = format!("{icon} {repo} · {workflow} · {branch} @ {sha} — {conclusion}");
    match safe_run_url(&report.run_url) {
        // One newline is a markdown soft break, which the renderer turns into a
        // literal "\n" inside a `white-space: pre-wrap` bubble — two visual
        // lines, no blank line between them.
        Some(url) => format!("{status}\n{url}"),
        None => status,
    }
}

/// Punctuation allowed to survive into message text. What is missing is the
/// point:
///
/// - `<` and `>`, so no mention token and no raw HTML can form;
/// - `[` and `]`, so no markdown link label can form (parens alone cannot make
///   a link), along with `*`, `_`, `` ` ``, `!` and `|`;
/// - `:`, so no URL scheme can form. A branch name may not contain `:` (git
///   forbids it in refnames), and dropping it is what stops a payload field
///   from smuggling a link into message text — which would not only render,
///   but be FETCHED by the server's own unfurl worker. The cost is cosmetic: a
///   workflow named `Build: release` renders as `Build release`.
///
/// What remains is what real branch and workflow names are built from.
const ALLOWED_PUNCTUATION: &str = "-./+#,()";

/// Squeeze an untrusted payload field into something safe to splice into a
/// message: drop every character outside the allowlist, fold whitespace runs to
/// single spaces, and cap the length with an ellipsis. An empty result becomes
/// `unknown`, because a blank slot in the status line reads as a bug.
///
/// Unicode letters and digits pass — a workflow may legitimately be named in
/// any script — while symbols and emoji do not, because "renders as itself" is
/// not a property this function can check.
fn sanitize(raw: &str, max_chars: usize) -> String {
    let mut kept = String::new();
    let mut pending_space = false;
    for c in raw.chars() {
        if c.is_whitespace() {
            pending_space = !kept.is_empty();
            continue;
        }
        if !c.is_alphanumeric() && !ALLOWED_PUNCTUATION.contains(c) {
            continue;
        }
        if pending_space {
            kept.push(' ');
            pending_space = false;
        }
        kept.push(c);
    }
    if kept.is_empty() {
        return "unknown".to_string();
    }
    if kept.chars().count() > max_chars {
        let mut truncated: String = kept.chars().take(max_chars).collect();
        truncated.push('…');
        return truncated;
    }
    kept
}

/// The leading hex of a commit id, at git's customary 7 characters. A non-hex
/// character ends the run, so a `sha` field carrying prose contributes nothing.
fn short_sha(raw: &str) -> String {
    let hex: String = raw.chars().take_while(char::is_ascii_hexdigit).take(7).collect();
    if hex.is_empty() {
        "unknown".to_string()
    } else {
        hex
    }
}

/// The only run link we will echo: an `https://github.com/…` URL built from URL
/// characters and nothing else.
///
/// Anchoring on the host matters twice over. The renderer would make any
/// http(s) URL clickable, and the server's own unfurl worker fetches URLs it
/// finds in message text — so an unchecked `run_url` would let a signed caller
/// aim both a reader's click and the server's fetch wherever it liked.
fn safe_run_url(raw: &str) -> Option<&str> {
    const MAX_URL_LEN: usize = 300;
    const URL_PUNCTUATION: &str = "-._~:/?#[]@!$&'()*+,;=%";

    let url = raw.trim();
    if url.len() > MAX_URL_LEN || !url.starts_with("https://github.com/") {
        return None;
    }
    url.chars().all(|c| c.is_ascii_alphanumeric() || URL_PUNCTUATION.contains(c)).then_some(url)
}

/// Unix seconds. Distinct from `workers::now_ms` because the signed timestamp
/// is in seconds (the svix convention) while `Message.timestamp` is in ms.
fn now_secs() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

impl CiHook {
    /// Claim a delivery id. `false` means it was already spent — a replay, or
    /// the sender retrying a delivery we already posted.
    fn claim(&self, id: &str) -> bool { self.0.seen.lock().expect("ci hook replay guard poisoned").claim(id) }

    /// Give a claimed id back after a failed write, so a genuine retry is not
    /// mistaken for a replay.
    fn release(&self, id: &str) { self.0.seen.lock().expect("ci hook replay guard poisoned").release(id) }
}

/// Bounded record of the delivery ids already accepted, oldest evicted first.
///
/// Memory-only and per-process, which is exactly enough: the Cloud Run service
/// runs a single instance (`--max-instances 1`), and the timestamp window
/// already bounds how long a replay can be attempted — so the worst a restart
/// costs is one duplicate line, for a delivery replayed within five minutes of
/// a boot.
#[derive(Default)]
struct SeenDeliveries {
    order: VecDeque<String>,
    ids: HashSet<String>,
}

impl SeenDeliveries {
    fn claim(&mut self, id: &str) -> bool {
        if !self.ids.insert(id.to_string()) {
            return false;
        }
        self.order.push_back(id.to_string());
        if self.order.len() > SEEN_CAPACITY {
            if let Some(evicted) = self.order.pop_front() {
                self.ids.remove(&evicted);
            }
        }
        true
    }

    fn release(&mut self, id: &str) {
        if self.ids.remove(id) {
            self.order.retain(|seen| seen != id);
        }
    }
}

/// Sign exactly the way the reporter workflow's `openssl` pipeline does:
/// HMAC-SHA256 over `"<id>.<timestamp>.<body>"`, base64-encoded. Shared by both
/// test modules — if this and `.github/workflows/ci-report.yml` ever disagree,
/// these tests pass while production 401s, so keep them literally the same
/// construction.
#[cfg(test)]
fn test_signature(secret: &str, id: &str, timestamp: i64, body: &[u8]) -> String {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(format!("{id}.{timestamp}.").as_bytes());
    mac.update(body);
    format!("v1,{}", SIGNATURE_B64.encode(mac.finalize().into_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const NOW: i64 = 1_800_000_000;
    const BODY: &[u8] = br#"{"repo":"ankurah/ankurah","workflow":"Tests","branch":"main","sha":"abc1234def","conclusion":"success","run_url":"https://github.com/ankurah/ankurah/actions/runs/42"}"#;
    const RUN_URL: &str = "https://github.com/ankurah/ankurah/actions/runs/42";

    fn headers(id: &str, timestamp: i64, signature: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("webhook-id", id.parse().unwrap());
        headers.insert("webhook-timestamp", timestamp.to_string().parse().unwrap());
        headers.insert("webhook-signature", signature.parse().unwrap());
        headers
    }

    /// Headers a well-behaved sender would produce for [`BODY`].
    fn signed_headers(timestamp: i64) -> HeaderMap {
        headers("delivery-1", timestamp, &test_signature(SECRET, "delivery-1", timestamp, BODY))
    }

    #[test]
    fn a_correctly_signed_delivery_authenticates() {
        assert_eq!(authenticate(SECRET, &signed_headers(NOW), BODY, NOW), Ok("delivery-1".to_string()));
        // Skew inside the window, both directions.
        assert!(authenticate(SECRET, &signed_headers(NOW - TIMESTAMP_TOLERANCE_SECS), BODY, NOW).is_ok());
        assert!(authenticate(SECRET, &signed_headers(NOW + TIMESTAMP_TOLERANCE_SECS), BODY, NOW).is_ok());
    }

    #[test]
    fn a_tag_from_a_different_secret_is_refused() {
        let hdrs = headers("delivery-1", NOW, &test_signature("some-other-secret", "delivery-1", NOW, BODY));
        assert_eq!(
            authenticate(SECRET, &hdrs, BODY, NOW),
            Err(Rejected::new(StatusCode::UNAUTHORIZED, "webhook-signature does not match"))
        );
    }

    #[test]
    fn a_tampered_body_is_refused() {
        // The signature covers the RAW bytes, so flipping `success` to
        // `failure` after signing must fail — this is the whole reason the
        // handler hashes the body before parsing it.
        let tampered = br#"{"repo":"ankurah/ankurah","workflow":"Tests","branch":"main","sha":"abc1234def","conclusion":"failure","run_url":"https://github.com/ankurah/ankurah/actions/runs/42"}"#;
        assert!(authenticate(SECRET, &signed_headers(NOW), tampered, NOW).is_err());
    }

    #[test]
    fn the_id_and_timestamp_are_bound_to_the_tag() {
        // A tag minted for one delivery cannot be replayed under a fresh id or
        // a fresh timestamp — both sit inside the signed content.
        let signature = test_signature(SECRET, "delivery-1", NOW, BODY);
        assert!(authenticate(SECRET, &headers("delivery-2", NOW, &signature), BODY, NOW).is_err());
        assert!(authenticate(SECRET, &headers("delivery-1", NOW + 1, &signature), BODY, NOW).is_err());
    }

    #[test]
    fn a_stale_or_future_timestamp_is_refused() {
        assert_eq!(
            authenticate(SECRET, &signed_headers(NOW - TIMESTAMP_TOLERANCE_SECS - 1), BODY, NOW),
            Err(Rejected::new(StatusCode::UNAUTHORIZED, "webhook-timestamp is outside the accepted window"))
        );
        // A far-future timestamp is refused just as hard: otherwise a captured
        // request could be parked and replayed whenever the holder liked.
        assert!(authenticate(SECRET, &signed_headers(NOW + TIMESTAMP_TOLERANCE_SECS + 1), BODY, NOW).is_err());
    }

    #[test]
    fn missing_headers_are_refused_by_name() {
        for absent in ["webhook-id", "webhook-timestamp", "webhook-signature"] {
            let mut hdrs = signed_headers(NOW);
            hdrs.remove(absent);
            let rejection = authenticate(SECRET, &hdrs, BODY, NOW).unwrap_err();
            assert_eq!(rejection.status, StatusCode::UNAUTHORIZED);
            assert!(rejection.reason.contains(absent), "{absent}: {}", rejection.reason);
        }
        assert!(authenticate(SECRET, &HeaderMap::new(), BODY, NOW).is_err());
    }

    #[test]
    fn malformed_headers_are_refused() {
        let good = test_signature(SECRET, "delivery-1", NOW, BODY);

        let mut bad_timestamp = signed_headers(NOW);
        bad_timestamp.insert("webhook-timestamp", "not-a-number".parse().unwrap());
        assert!(authenticate(SECRET, &bad_timestamp, BODY, NOW).is_err());

        assert!(authenticate(SECRET, &headers("", NOW, &good), BODY, NOW).is_err());
        assert!(authenticate(SECRET, &headers(&"x".repeat(MAX_WEBHOOK_ID_LEN + 1), NOW, &good), BODY, NOW).is_err());
        // Wrong version prefix, not base64, and no values at all.
        assert!(authenticate(SECRET, &headers("delivery-1", NOW, &good.replace("v1,", "v2,")), BODY, NOW).is_err());
        assert!(authenticate(SECRET, &headers("delivery-1", NOW, "v1,not base64!!"), BODY, NOW).is_err());
        assert!(authenticate(SECRET, &headers("delivery-1", NOW, ""), BODY, NOW).is_err());
    }

    #[test]
    fn one_matching_value_among_several_is_enough() {
        // Secret rotation: during the overlap the sender signs with both keys.
        let both = format!(
            "{} {}",
            test_signature("the-old-secret", "delivery-1", NOW, BODY),
            test_signature(SECRET, "delivery-1", NOW, BODY)
        );
        assert!(authenticate(SECRET, &headers("delivery-1", NOW, &both), BODY, NOW).is_ok());
    }

    #[test]
    fn a_delivery_id_is_spent_once_and_can_be_released() {
        let mut seen = SeenDeliveries::default();
        assert!(seen.claim("a"));
        assert!(!seen.claim("a"));
        seen.release("a");
        assert!(seen.claim("a"), "a released id is claimable again — that is the retry-after-failed-write path");

        for n in 0..SEEN_CAPACITY {
            seen.claim(&format!("id-{n}"));
        }
        assert_eq!(seen.order.len(), SEEN_CAPACITY, "eviction keeps the record bounded");
        assert_eq!(seen.order.len(), seen.ids.len(), "the queue and the set never drift");
    }

    fn report(branch: &str, conclusion: &str, run_url: &str) -> CiReport {
        CiReport {
            repo: "ankurah/ankurah".into(),
            workflow: "Tests".into(),
            branch: branch.into(),
            sha: "abc1234def5678".into(),
            conclusion: conclusion.into(),
            run_url: run_url.into(),
        }
    }

    #[test]
    fn a_green_run_renders_as_two_lines() {
        assert_eq!(
            format_message(&report("main", "success", RUN_URL)),
            format!("✅ ankurah/ankurah · Tests · main @ abc1234 — success\n{RUN_URL}")
        );
    }

    #[test]
    fn conclusions_map_to_three_icons() {
        assert!(format_message(&report("main", "failure", RUN_URL)).starts_with("❌"));
        assert!(format_message(&report("main", "timed_out", RUN_URL)).starts_with("❌"));
        assert!(format_message(&report("main", "cancelled", RUN_URL)).starts_with("⚪"));
        assert!(format_message(&report("main", "something-new", RUN_URL)).starts_with("⚪"));
    }

    #[test]
    fn a_branch_name_cannot_smuggle_a_mention_token() {
        // The exact attack the sanitizer exists for: someone names a PR branch
        // after a mention token, and every line the reporter posts notifies
        // that member. `<` and `>` never survive, so no token can form.
        let text = format_message(&report("<@AZk3jW0RvkW8pTGnQxYzAA>", "success", RUN_URL));
        assert!(community_model::parse_mentions(&text).is_empty(), "{text}");
    }

    #[test]
    fn a_branch_name_cannot_forge_lines_or_links() {
        // Newlines fold to spaces, so the message stays two lines...
        let text = format_message(&report("main\n❌ everything is broken", "success", RUN_URL));
        assert_eq!(text.lines().count(), 2, "{text}");

        // ...and no link can form: not a markdown link (the label brackets are
        // gone) and not a bare one (the scheme's colon is gone). The second
        // matters most — a bare URL in message text is fetched by the server's
        // own unfurl worker, so the only URL left to fetch must be the run
        // link.
        let text = format_message(&report("[docs](https://evil.example)", "success", RUN_URL));
        assert!(!text.contains('['), "{text}");
        assert_eq!(community_model::extract_urls(&text), vec![RUN_URL.to_string()], "{text}");
    }

    #[test]
    fn a_run_url_off_github_is_dropped_rather_than_rendered() {
        // Dropping the line loses a link; echoing it would aim both the
        // reader's click and the unfurl worker's fetch at a caller-chosen host.
        for hostile in [
            "https://evil.example/runs/42",
            "http://github.com/ankurah/ankurah/actions/runs/42",
            "https://github.com.evil.example/x",
            "javascript:alert(1)",
            "https://github.com/a b",
        ] {
            let text = format_message(&report("main", "success", hostile));
            assert_eq!(text.lines().count(), 1, "{hostile} -> {text}");
            assert!(community_model::extract_urls(&text).is_empty(), "{hostile} -> {text}");
        }
    }

    #[test]
    fn empty_and_junk_fields_degrade_to_unknown() {
        let text = format_message(&report("***", "", RUN_URL));
        assert!(text.contains("unknown"), "{text}");
        assert_eq!(short_sha("abc1234def5678"), "abc1234");
        assert_eq!(short_sha("zzz"), "unknown");
        assert_eq!(short_sha(""), "unknown");
    }

    #[test]
    fn long_fields_are_capped() {
        let text = format_message(&report(&"a".repeat(500), "success", RUN_URL));
        assert!(text.contains('…'), "{text}");
        assert!(text.lines().next().unwrap().chars().count() < 160, "{text}");
    }
}

/// End-to-end proof on a real (sled) node, through the real axum route: a
/// correctly signed POST becomes one `Message` row in `#ci` authored by the
/// seeded CI user; a bad signature becomes a 401 and no row; a replayed
/// delivery writes nothing the second time.
///
/// Sled-gated like the worker test (`workers/mod.rs`): run with
/// `cargo test -p community-server --no-default-features --features sled`. The
/// default (postgres) test run compiles this module to nothing.
#[cfg(all(test, feature = "sled"))]
mod route_tests {
    use super::*;
    use ankurah::policy::{PermissiveAgent, DEFAULT_CONTEXT};
    use ankurah::Node;
    use ankurah_storage_sled::SledStorageEngine;
    use axum::body::Body;
    use axum::http::Request;
    use axum::routing::post;
    use axum::Router;
    use community_model::{MessageView, RoomView};
    use tower::ServiceExt as _;

    const SECRET: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    /// A realistic delivery, `run_id` and `actor` included — the fields the
    /// reporter sends and this module deliberately ignores.
    const BODY: &str = r#"{"repo":"ankurah/ankurah","workflow":"Tests","branch":"main","sha":"abc1234def","conclusion":"success","run_url":"https://github.com/ankurah/ankurah/actions/runs/42","run_id":"42","actor":"dnorman"}"#;
    const EXPECTED_TEXT: &str =
        "✅ ankurah/ankurah · Tests · main @ abc1234 — success\nhttps://github.com/ankurah/ankurah/actions/runs/42";

    /// The durable-node init dance from `main()` with the permissive agent —
    /// the endpoint writes through a privileged context, and which agent grants
    /// that privilege is irrelevant to what these tests prove.
    async fn test_context() -> Context {
        let node = Node::new_durable(Arc::new(SledStorageEngine::new_test().unwrap()), PermissiveAgent::new());
        node.system.wait_loaded().await;
        if node.system.root().is_none() {
            node.system.create().await.unwrap();
        }
        node.system.wait_system_ready().await;
        node.context_async(DEFAULT_CONTEXT).await
    }

    /// The real route over a freshly seeded node. `main` mounts `handle` on a
    /// router whose state is `AppState`, and axum hands it the `CiHook` through
    /// `FromRef` — so a router stated directly on the hook exercises the same
    /// handler with the same extractors, without standing up the auth state
    /// this endpoint has nothing to do with.
    async fn app() -> (Router, Context, CiHook) {
        let ctx = test_context().await;
        let ci = seed_with_secret(&ctx, Some(SECRET.to_string())).await.unwrap();
        (Router::new().route("/hooks/ci", post(handle)).with_state(ci.clone()), ctx, ci)
    }

    fn request(id: &str, timestamp: i64, signature: &str) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri("/hooks/ci")
            .header("content-type", "application/json")
            .header("webhook-id", id)
            .header("webhook-timestamp", timestamp.to_string())
            .header("webhook-signature", signature)
            .body(Body::from(BODY))
            .unwrap()
    }

    async fn json_body(response: Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(response.into_body(), MAX_BODY_BYTES).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    async fn messages(ctx: &Context) -> Vec<MessageView> { ctx.fetch::<MessageView>("true").await.unwrap() }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_signed_post_becomes_one_ci_message() {
        let (app, ctx, _ci) = app().await;
        let now = now_secs();
        let signature = test_signature(SECRET, "run-42", now, BODY.as_bytes());

        let response = app.clone().oneshot(request("run-42", now, &signature)).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(json_body(response).await["status"], "ok");

        let rows = messages(&ctx).await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].text().unwrap(), EXPECTED_TEXT);

        // Authored by the seeded CI user, in the seeded #ci room — the two
        // identities `seed` is responsible for.
        let author = ctx.get::<UserView>(rows[0].user().unwrap().id()).await.unwrap();
        assert_eq!(author.display_name().unwrap(), CI_DISPLAY_NAME);
        assert_eq!(author.oidc_sub().unwrap().as_deref(), Some(CI_OIDC_SUB));
        let room = ctx.get::<RoomView>(rows[0].room().unwrap().id()).await.unwrap();
        assert_eq!(room.name().unwrap(), CI_ROOM_NAME);

        // The same delivery id again writes nothing.
        let response = app.oneshot(request("run-42", now, &signature)).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(json_body(response).await["status"], "duplicate");
        assert_eq!(messages(&ctx).await.len(), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_unsigned_or_wrongly_signed_post_writes_nothing() {
        let (app, ctx, _ci) = app().await;
        let now = now_secs();

        let forged = test_signature("not-the-secret", "run-43", now, BODY.as_bytes());
        let response = app.clone().oneshot(request("run-43", now, &forged)).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        let bare = Request::builder().method("POST").uri("/hooks/ci").body(Body::from(BODY)).unwrap();
        let response = app.clone().oneshot(bare).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        let stale = now - TIMESTAMP_TOLERANCE_SECS - 1;
        let signature = test_signature(SECRET, "run-44", stale, BODY.as_bytes());
        let response = app.clone().oneshot(request("run-44", stale, &signature)).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        assert!(messages(&ctx).await.is_empty(), "no refusal may write a row");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_unconfigured_hook_refuses_even_a_valid_signature() {
        // The fail-closed posture: no secret means no deliveries, not
        // unauthenticated ones. 503 (not 401) so an operator can tell "you
        // signed wrong" from "this server has no secret yet".
        let ctx = test_context().await;
        let ci = seed_with_secret(&ctx, None).await.unwrap();
        let now = now_secs();
        let headers = {
            let mut headers = HeaderMap::new();
            headers.insert("webhook-id", "run-45".parse().unwrap());
            headers.insert("webhook-timestamp", now.to_string().parse().unwrap());
            headers.insert("webhook-signature", test_signature(SECRET, "run-45", now, BODY.as_bytes()).parse().unwrap());
            headers
        };
        let rejected = deliver(&ci, &headers, BODY.as_bytes(), now).await.unwrap_err();
        assert_eq!(rejected.status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(messages(&ctx).await.is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn seeding_twice_reuses_the_same_ci_identity() {
        let (_app, ctx, ci) = app().await;
        let again = seed_with_secret(&ctx, Some(SECRET.to_string())).await.unwrap();
        assert_eq!(ci.0.author, again.0.author, "the CI user is found, not re-created");
        assert_eq!(ci.0.room, again.0.room, "the #ci room is found, not re-created");
    }
}
