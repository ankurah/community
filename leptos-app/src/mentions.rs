//! Mention-token text helpers for plain-text surfaces (#23).
//!
//! The canonical `<@BASE64_ID>` token is right for storage and the render
//! pipeline (markdown.rs turns it into a live chip), but the reply surfaces —
//! the "Replying to …" chip above the composer and the embedded preview
//! inside a reply bubble — are one-line plain text, where a raw token reads
//! as base64 gibberish. These resolve tokens to `@DisplayName` text.

use std::collections::HashMap;

/// Reply snippets are clipped to this many characters (#23).
const REPLY_SNIPPET_MAX: usize = 80;

/// Replace mention tokens with plain `@DisplayName` text. Unknown ids (the
/// member isn't loaded, or a foreign id) become `@unknown` — the same
/// fallback the markdown renderer uses.
pub fn resolve_tokens(text: &str, names: &HashMap<String, String>) -> String {
    let mut out = text.to_string();
    for id in community_model::parse_mentions(text) {
        let name = names.get(&id).cloned().filter(|n| !n.is_empty()).unwrap_or_else(|| "unknown".to_string());
        out = out.replace(&format!("<@{id}>"), &format!("@{name}"));
    }
    out
}

/// One-line reply snippet: tokens resolved, whitespace runs collapsed (so
/// multiline and code messages stay a single line), clipped to
/// [`REPLY_SNIPPET_MAX`] chars with an ellipsis.
pub fn reply_snippet(text: &str, names: &HashMap<String, String>) -> String {
    let resolved = resolve_tokens(text, names);
    let mut snippet: String = resolved.split_whitespace().collect::<Vec<_>>().join(" ");
    if snippet.chars().count() > REPLY_SNIPPET_MAX {
        snippet = snippet.chars().take(REPLY_SNIPPET_MAX).collect::<String>().trim_end().to_string() + "\u{2026}";
    }
    snippet
}
