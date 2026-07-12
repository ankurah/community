//! Link unfurling: http(s) URLs in messages become `LinkPreview` cache rows
//! (refs #20).
//!
//! Consumes `MessageView`s from the standing message LiveQuery, extracts URLs
//! with the shared [`community_model::extract_urls`] scanner (the client uses
//! the same function to look previews up, so the strings match exactly), and
//! fetches each never-seen URL server-side behind the [`super::ssrf`] guard.
//!
//! Every fetch attempt writes exactly one row per URL:
//! - `ok: true` with whatever og:/title metadata the page offered, or
//! - `ok: false` on ANY failure (guard trip, DNS, timeout, non-HTML, HTTP
//!   error) — persisted deliberately so the client falls back to a plain
//!   link and this worker never refetches a known-bad URL. The row's
//!   existence, success or not, IS the idempotency check.
//!
//! Fetch hardening (see ssrf.rs for the address policy itself):
//! - redirects are never followed automatically; each hop (max 3) is re-vetted
//!   and re-pinned, so a public page cannot bounce us into private space;
//! - DNS results are pinned onto the connection (`resolve_to_addrs`), closing
//!   the resolve-then-connect TOCTOU;
//! - one 5s wall-clock budget for the whole chain and a 512 KiB body cap
//!   (bounded read — og tags live in `<head>`, so we parse the prefix);
//! - only `text/html`/`application/xhtml+xml` bodies are parsed, scripts and
//!   sub-resources are never fetched, non-http(s) hops are refused.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Duration;

use ankurah::ankql::{ast::Expr, parser::parse_selection};
use ankurah::{Context, EntityId};
use anyhow::{Context as _, Result};
use community_model::{extract_urls, LinkPreview, LinkPreviewView, MessageView};
use tokio::sync::mpsc::UnboundedReceiver;
use tracing::{debug, info, warn};
use url::Url;

use super::{now_ms, og, remember, signature, ssrf};

/// Wall-clock budget for one URL's entire fetch chain (all hops).
const TOTAL_BUDGET: Duration = Duration::from_secs(5);
/// Per-hop TCP connect budget (the outer budget still bounds everything).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
/// Redirect hops we are willing to chase after the initial request.
const MAX_REDIRECTS: usize = 3;
/// Body bytes we read and parse; the rest of the response is dropped.
const MAX_BODY_BYTES: usize = 512 * 1024;
/// Stored-field caps: a hostile page must not bloat rows.
const MAX_TITLE_CHARS: usize = 300;
const MAX_DESCRIPTION_CHARS: usize = 600;
const MAX_IMAGE_URL_CHARS: usize = 2048;
/// Honest bot identity for server-side fetches.
const USER_AGENT: &str = concat!("community-linkpreview/", env!("CARGO_PKG_VERSION"));

/// Consumer loop: one message at a time, errors contained per message/URL.
/// The receiver is borrowed from the supervisor (`workers::supervise`), which
/// respawns this loop if it ever panics.
pub async fn run(ctx: Context, rx: &mut UnboundedReceiver<MessageView>) {
    info!("link-unfurl worker started (message URLs → linkpreview rows)");
    // message id → signature of the URL list already fully handled (same
    // optimization-only role as the mention worker's cache).
    let mut handled: HashMap<EntityId, u64> = HashMap::new();
    while let Some(msg) = rx.recv().await {
        let message_id = msg.id();
        if let Err(e) = process_message(&ctx, &msg, &mut handled).await {
            warn!(message = %message_id, "link unfurl failed (retries on the message's next change): {e:#}");
        }
    }
    warn!("link-unfurl worker: message stream closed; exiting");
}

async fn process_message(ctx: &Context, msg: &MessageView, handled: &mut HashMap<EntityId, u64>) -> Result<()> {
    let text = msg.text().context("read message text")?;
    let urls = extract_urls(&text);
    let sig = signature(&urls);
    if handled.get(&msg.id()) == Some(&sig) {
        return Ok(());
    }
    if urls.is_empty() {
        remember(handled, msg.id(), sig);
        return Ok(());
    }

    let mut all_handled = true;
    for url in &urls {
        match ensure_preview(ctx, url).await {
            Ok(Outcome::Fetched { ok, note }) => match note {
                // Fetch outcome log line: the URL and how it went, never body content.
                Some(reason) => info!(url = %url, ok, "link preview cached ({reason})"),
                None => info!(url = %url, ok, "link preview cached"),
            },
            Ok(Outcome::AlreadyCached) => debug!(url = %url, "link preview already cached"),
            Err(e) => {
                // Storage-level failure (the fetch itself can't error — it
                // folds into ok:false). Leave uncached so a later change retries.
                all_handled = false;
                warn!(url = %url, message = %msg.id(), "link preview persist failed: {e:#}");
            }
        }
    }
    if all_handled {
        remember(handled, msg.id(), sig);
    }
    Ok(())
}

enum Outcome {
    AlreadyCached,
    Fetched { ok: bool, note: Option<String> },
}

/// Create the `LinkPreview` row for `url` if none exists. The existence
/// check makes the worker idempotent across the boot backlog sweep, message
/// edits, and concurrent messages sharing a URL (this consumer is serial, so
/// two ensure calls for one URL can never interleave).
async fn ensure_preview(ctx: &Context, url: &str) -> Result<Outcome> {
    let predicate = parse_selection("url = ?")?.predicate.populate([Expr::from(url)])?;
    if !ctx.fetch::<LinkPreviewView>(predicate).await?.is_empty() {
        return Ok(Outcome::AlreadyCached);
    }

    let fetched = fetch_preview(url).await;
    let trx = ctx.begin();
    trx.create(&LinkPreview {
        url: url.to_string(),
        title: fetched.title,
        description: fetched.description,
        image_url: fetched.image_url,
        fetched_at: now_ms(),
        ok: fetched.ok,
    })
    .await
    .context("create linkpreview")?;
    trx.commit().await.context("commit linkpreview")?;
    Ok(Outcome::Fetched { ok: fetched.ok, note: fetched.note })
}

struct Fetched {
    ok: bool,
    title: Option<String>,
    description: Option<String>,
    image_url: Option<String>,
    /// Why the fetch failed (or a benign remark). Log-only — never stored,
    /// so a hostile server can't inject content into our ops trail beyond
    /// what reqwest error strings already carry.
    note: Option<String>,
}

impl Fetched {
    fn failed(note: String) -> Self { Self { ok: false, title: None, description: None, image_url: None, note: Some(note) } }
}

/// Fetch and parse one URL under the SSRF guard. Infallible by design: every
/// failure mode folds into an `ok: false` result so the caller always has a
/// row to persist.
async fn fetch_preview(url: &str) -> Fetched {
    match tokio::time::timeout(TOTAL_BUDGET, fetch_preview_inner(url)).await {
        Ok(Ok(fetched)) => fetched,
        Ok(Err(deny)) => Fetched::failed(deny),
        Err(_) => Fetched::failed(format!("total budget of {TOTAL_BUDGET:?} exceeded")),
    }
}

async fn fetch_preview_inner(url_str: &str) -> Result<Fetched, String> {
    let mut url = Url::parse(url_str).map_err(|e| format!("unparseable URL: {e}"))?;

    // `0..=MAX_REDIRECTS`: the initial request plus up to MAX_REDIRECTS hops.
    for _hop in 0..=MAX_REDIRECTS {
        // EVERY hop is fully re-vetted: scheme/credentials, then address
        // classification with DNS pinning (see module docs).
        ssrf::vet_url(&url)?;
        let pin = ssrf::vet_target(&url).await?;
        let client = build_client(pin)?;

        let response = client
            .get(url.clone())
            .header(reqwest::header::ACCEPT, "text/html,application/xhtml+xml")
            .send()
            .await
            .map_err(|e| format!("request failed: {e}"))?;

        let status = response.status();
        if status.is_redirection() {
            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .ok_or_else(|| format!("redirect {status} without Location"))?
                .to_str()
                .map_err(|_| "redirect Location is not valid UTF-8".to_string())?;
            // join() resolves relative Locations against the current hop.
            url = url.join(location).map_err(|e| format!("bad redirect target: {e}"))?;
            continue;
        }
        if !status.is_success() {
            return Err(format!("http status {status}"));
        }

        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        if !(content_type.starts_with("text/html") || content_type.starts_with("application/xhtml+xml")) {
            // Not an HTML page (image, PDF, JSON API…): nothing to parse.
            // ok:false → the client shows a plain link; we never sniff bytes.
            return Err(format!("unsupported content-type '{content_type}'"));
        }

        let body = read_capped(response).await?;
        let meta = og::parse(&String::from_utf8_lossy(&body));

        return Ok(Fetched {
            ok: true,
            title: sanitize(meta.best_title(), MAX_TITLE_CHARS),
            description: sanitize(meta.best_description(), MAX_DESCRIPTION_CHARS),
            image_url: resolve_image_url(&url, meta.og_image.as_deref()),
            note: None,
        });
    }
    Err(format!("more than {MAX_REDIRECTS} redirects"))
}

/// A per-hop client. Built per request on purpose: the DNS pin
/// (`resolve_to_addrs`) is a builder-level setting, and each hop must be
/// pinned to ITS OWN vetted addresses. Unfurls are low-volume, so the build
/// cost is irrelevant next to the network round-trip.
fn build_client(pin: Option<(String, Vec<SocketAddr>)>) -> Result<reqwest::Client, String> {
    let mut builder = reqwest::Client::builder()
        // Redirects are handled manually so every hop is re-vetted.
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(TOTAL_BUDGET)
        .user_agent(USER_AGENT)
        // reqwest honors HTTP(S)_PROXY env vars by default; a proxy resolves
        // hostnames itself, which would sidestep the vetted-address pin. No
        // deployment of ours sets a proxy — refuse ambient ones outright.
        .no_proxy();
    if let Some((domain, addrs)) = pin {
        builder = builder.resolve_to_addrs(&domain, &addrs);
    }
    builder.build().map_err(|e| format!("http client build failed: {e}"))
}

/// Stream the body up to [`MAX_BODY_BYTES`]; anything beyond is dropped, not
/// an error — og/meta tags live in `<head>`, so the prefix is what we need.
async fn read_capped(mut response: reqwest::Response) -> Result<Vec<u8>, String> {
    // Trust Content-Length only as a pre-allocation hint, never as a limit.
    let mut body: Vec<u8> = Vec::with_capacity(8 * 1024);
    while let Some(chunk) = response.chunk().await.map_err(|e| format!("body read failed: {e}"))? {
        if body.len() + chunk.len() >= MAX_BODY_BYTES {
            body.extend_from_slice(&chunk[..MAX_BODY_BYTES - body.len()]);
            break;
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Clamp extracted text for storage: strip control characters, collapse
/// whitespace runs (og content sometimes carries raw newlines/tabs), and cap
/// the length in characters. Empty results become `None`.
fn sanitize(text: Option<&str>, max_chars: usize) -> Option<String> {
    let text = text?;
    let mut out = String::with_capacity(text.len().min(max_chars));
    let mut chars_out = 0usize;
    let mut last_was_space = true; // leading whitespace is dropped
    for c in text.chars() {
        let c = if c.is_control() || c.is_whitespace() { ' ' } else { c };
        if c == ' ' {
            if last_was_space {
                continue;
            }
            last_was_space = true;
        } else {
            last_was_space = false;
        }
        out.push(c);
        chars_out += 1;
        if chars_out >= max_chars {
            break;
        }
    }
    let trimmed = out.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// og:image resolved to an absolute http(s) URL against the FINAL hop (og
/// images are often relative). Anything else — other schemes, unparseable,
/// oversized — is dropped: this string ends up in an `<img src>` on every
/// viewer's client, so it must at least be a plain web URL.
fn resolve_image_url(base: &Url, image: Option<&str>) -> Option<String> {
    let raw = image?.trim();
    if raw.is_empty() {
        return None;
    }
    let resolved = base.join(raw).ok()?;
    if !matches!(resolved.scheme(), "http" | "https") {
        return None;
    }
    let s = resolved.to_string();
    if s.len() > MAX_IMAGE_URL_CHARS {
        return None;
    }
    Some(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_collapses_and_caps() {
        // Control chars become separators (then collapse), never survive.
        assert_eq!(sanitize(Some("  a\n\t b\u{0}c  "), 100), Some("a b c".to_string()));
        assert_eq!(sanitize(Some(""), 10), None);
        assert_eq!(sanitize(Some("   \n  "), 10), None);
        assert_eq!(sanitize(None, 10), None);
        let long = "x".repeat(500);
        assert_eq!(sanitize(Some(&long), 10).unwrap().chars().count(), 10);
        // Multibyte-safe: caps by characters, not bytes.
        let uni = "é".repeat(500);
        assert_eq!(sanitize(Some(&uni), 10).unwrap().chars().count(), 10);
    }

    #[test]
    fn image_urls_resolve_relative_and_reject_non_http() {
        let base = Url::parse("https://example.com/article/1").unwrap();
        assert_eq!(resolve_image_url(&base, Some("/img/x.png")), Some("https://example.com/img/x.png".to_string()));
        assert_eq!(resolve_image_url(&base, Some("https://cdn.example.com/y.png")), Some("https://cdn.example.com/y.png".to_string()));
        // Protocol-relative resolves against the base's scheme.
        assert_eq!(resolve_image_url(&base, Some("//cdn.example.com/z.png")), Some("https://cdn.example.com/z.png".to_string()));
        // A crafted page must not smuggle non-web schemes into <img src>.
        assert_eq!(resolve_image_url(&base, Some("javascript:alert(1)")), None);
        assert_eq!(resolve_image_url(&base, Some("data:image/png;base64,AAAA")), None);
        assert_eq!(resolve_image_url(&base, Some("file:///etc/passwd")), None);
        assert_eq!(resolve_image_url(&base, Some("")), None);
        assert_eq!(resolve_image_url(&base, None), None);
        let oversized = format!("https://example.com/{}", "a".repeat(MAX_IMAGE_URL_CHARS));
        assert_eq!(resolve_image_url(&base, Some(&oversized)), None);
    }
}
