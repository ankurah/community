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

use std::collections::HashMap;

use leptos::prelude::*;
use wasm_bindgen::JsCast;

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
pub fn LinkPreviewCard(message: MessageView, previews: Memo<HashMap<String, LinkPreviewView>>) -> impl IntoView {
    move || {
        let text = message.text().unwrap_or_default();
        let urls = extract_urls(&text);
        if urls.is_empty() {
            return None;
        }
        // The map is built from an `ok = true` query, so "first URL with an
        // entry" is "first URL that unfurled successfully".
        let (url, preview) = urls.into_iter().find_map(|u| previews.with(|m| m.get(&u).cloned()).map(|p| (u, p)))?;

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
                                // on:error hides the whole thumb box: a dead
                                // image (or an http:// one the browser blocks
                                // on our https origin) would otherwise leave a
                                // permanent empty gray square on the card.
                                <img
                                    src=src
                                    alt=""
                                    loading="lazy"
                                    referrerpolicy="no-referrer"
                                    on:error=|e: leptos::ev::ErrorEvent| {
                                        if let Some(thumb) = e
                                            .target()
                                            .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
                                            .and_then(|img| img.parent_element())
                                            .and_then(|p| p.dyn_into::<web_sys::HtmlElement>().ok())
                                        {
                                            let _ = web_sys::HtmlElement::style(&thumb).set_property("display", "none");
                                        }
                                    }
                                />
                            </div>
                        }
                    })}
            </a>
        })
    }
}
