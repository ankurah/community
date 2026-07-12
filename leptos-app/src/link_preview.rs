//! Link preview cards (#20): a compact unfurl under a message bubble.
//!
//! The server's unfurl worker fetches URLs found in new messages and writes
//! `LinkPreview` rows (world-readable, `system`-write — clients cannot spoof
//! one). The client side is deliberately dumb: derive the message's URLs with
//! the same canonical `extract_urls` the server used, look for a matching row
//! by exact url string, and render at most ONE card per message — the first
//! URL that has a successful row. A failed unfurl (`ok = false`) or a missing
//! row renders nothing: the plain link in the bubble already covers it.
//!
//! v1 renders `image_url` as-is (it is validated http(s) server-side); an
//! image proxy is a later hardening pass.

use leptos::prelude::*;

use ankurah::LiveQuery;
use ankurah_signals::Get as AnkurahGet;
use community_model::{extract_urls, LinkPreviewView, MessageView};

/// Registrable host part of a URL, for the card's domain label — enough
/// string surgery for display purposes (no Url parser on this code path).
fn domain_of(url: &str) -> String {
    let after_scheme = url.split("://").nth(1).unwrap_or(url);
    let host = after_scheme.split(['/', '?', '#']).next().unwrap_or(after_scheme);
    let host = host.rsplit('@').next().unwrap_or(host); // strip userinfo
    let host = host.split(':').next().unwrap_or(host); // strip port
    host.strip_prefix("www.").unwrap_or(host).to_string()
}

/// The preview card for one message. Renders nothing unless some URL in the
/// message text has a successful `LinkPreview` row with a title or
/// description. Reactive on both inputs: a CRDT text edit re-derives the
/// URLs, and the shared previews LiveQuery pops the card in moments after
/// the server's unfurl worker writes the row.
#[component]
pub fn LinkPreviewCard(message: MessageView, previews: LiveQuery<LinkPreviewView>) -> impl IntoView {
    move || {
        let text = message.text().unwrap_or_default();
        let urls = extract_urls(&text);
        if urls.is_empty() {
            return None;
        }
        // The query predicate is `ok = true`, so "first URL with a row" is
        // "first URL that unfurled successfully".
        let rows = previews.get();
        let (url, preview) = urls
            .into_iter()
            .find_map(|u| rows.iter().find(|p| p.url().ok().as_deref() == Some(u.as_str())).map(|p| (u, p.clone())))?;

        let title = preview.title().ok().flatten().map(|t| t.trim().to_string()).filter(|t| !t.is_empty());
        let description = preview.description().ok().flatten().map(|d| d.trim().to_string()).filter(|d| !d.is_empty());
        if title.is_none() && description.is_none() {
            return None; // nothing worth a card; the plain link stands
        }
        // Server-side validation already drops non-http(s) images; the filter
        // here is belt in case an older/foreign row slips through.
        let image_url = preview
            .image_url()
            .ok()
            .flatten()
            .filter(|u| u.starts_with("https://") || u.starts_with("http://"));

        let domain = domain_of(&url);
        Some(view! {
            <a class="linkPreviewCard" href=url target="_blank" rel="noopener noreferrer">
                <div class="linkPreviewBody">
                    <div class="linkPreviewDomain">{domain}</div>
                    {title.map(|t| view! { <div class="linkPreviewTitle">{t}</div> })}
                    {description.map(|d| view! { <div class="linkPreviewDesc">{d}</div> })}
                </div>
                {image_url
                    .map(|src| {
                        view! {
                            <div class="linkPreviewThumb" aria-hidden="true">
                                <img src=src alt="" loading="lazy" referrerpolicy="no-referrer" />
                            </div>
                        }
                    })}
            </a>
        })
    }
}
