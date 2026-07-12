//! Pure text scanners shared by the server workers and the wasm client.
//!
//! Both sides must agree byte-for-byte on what counts as a mention token and
//! what counts as a URL: the client writes mention tokens (autocomplete) and
//! looks `LinkPreview` rows up by URL string equality, while the server parses
//! the same text to fan out notifications and to decide which URLs to unfurl.
//! Keeping the scanners here — pure functions, no ankurah types, no I/O — is
//! what guarantees that agreement.

/// Longest mention-id payload we accept. A real `EntityId` base64
/// (URL_SAFE_NO_PAD over 16 bytes) is exactly 22 chars; the headroom tolerates
/// future id-size changes without silently truncating, while still bounding
/// pathological `<@aaaa…>` runs. Consumers validate the payload with
/// `EntityId::from_base64` anyway — this is a scanner, not a validator.
const MAX_MENTION_ID_LEN: usize = 64;

/// Longest URL we will extract. Anything longer is far more likely to be an
/// abuse payload than a shareable link, and downstream consumers (the unfurl
/// worker, LinkPreview rows) shouldn't carry unbounded strings.
const MAX_URL_LEN: usize = 2048;

/// Extract mention tokens from message text, in order of first appearance,
/// deduplicated.
///
/// Canonical token form: `<@BASE64_ENTITY_ID>` — `<@`, then the base64url
/// form of a User entity id (`EntityId::to_base64`: URL_SAFE_NO_PAD, charset
/// `[A-Za-z0-9_-]`, 22 chars for today's 16-byte ids), then `>`. The returned
/// strings are the raw id payloads (no `<@`/`>`).
///
/// The scan is deliberately permissive about payload length (1..=64 chars of
/// the base64url charset) and strict about everything else: an unterminated
/// `<@abc`, a payload with characters outside the charset, or an empty `<@>`
/// yields nothing. Duplicate mentions of the same id collapse to one entry so
/// a message that shouts `<@X> <@X> <@X>` produces one notification, not three.
pub fn parse_mentions(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] != b'<' || bytes[i + 1] != b'@' {
            i += 1;
            continue;
        }
        // Candidate token: consume the base64url run after `<@`.
        let start = i + 2;
        let mut end = start;
        while end < bytes.len() && is_base64url_byte(bytes[end]) {
            end += 1;
        }
        let len = end - start;
        if (1..=MAX_MENTION_ID_LEN).contains(&len) && end < bytes.len() && bytes[end] == b'>' {
            // All boundary bytes (`<`, `@`, `>`, base64url) are ASCII, so these
            // indices are guaranteed char boundaries and the slice is valid UTF-8.
            let id = &text[start..end];
            if !out.iter().any(|existing| existing == id) {
                out.push(id.to_string());
            }
            i = end + 1;
        } else {
            // Not a token (bad charset, empty, overlong, or unterminated).
            // Resume scanning INSIDE the failed candidate rather than past it,
            // so `<@<@abc>` still finds the inner token.
            i += 1;
        }
    }
    out
}

fn is_base64url_byte(b: u8) -> bool { b.is_ascii_alphanumeric() || b == b'-' || b == b'_' }

/// Extract http(s) URLs from message text, in order of first appearance,
/// deduplicated, with trailing punctuation trimmed.
///
/// A URL starts at a literal `http://` or `https://` (case-insensitive) and
/// runs until whitespace or a character that cannot appear raw in a URL and
/// overwhelmingly means "the sentence continues" (`<`, `>`, `"`, `'`, `` ` ``).
/// Trailing sentence punctuation (`.,;:!?`) is trimmed, and a trailing closing
/// bracket is trimmed only when unbalanced — so `(see https://en.wikipedia.org/wiki/Rust_(film))`
/// keeps the paren that belongs to the URL and sheds the one that belongs to
/// the sentence.
///
/// The returned string is otherwise the URL exactly as written (no lowercasing,
/// no query stripping): it is the `LinkPreview.url` dedup key, and any
/// normalization here would have to be mirrored forever by every consumer.
pub fn extract_urls(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let rest = &text[i..];
        let scheme_len = if starts_with_ignore_case(rest, "https://") {
            8
        } else if starts_with_ignore_case(rest, "http://") {
            7
        } else {
            i += next_char_len(text, i);
            continue;
        };
        // Consume until a terminator byte. Multi-byte UTF-8 continuation bytes
        // are all >= 0x80 and never match the ASCII terminators, so a byte scan
        // is safe; we only slice at ASCII positions.
        let start = i;
        let mut end = i + scheme_len;
        while end < bytes.len() && !is_url_terminator(bytes[end]) {
            end += 1;
        }
        let candidate = trim_trailing_punctuation(&text[start..end]);
        // Must have SOMETHING after the scheme (a bare "http://" is prose),
        // and stay within sane length bounds.
        if candidate.len() > scheme_len && candidate.len() <= MAX_URL_LEN {
            if !out.iter().any(|existing| existing == candidate) {
                out.push(candidate.to_string());
            }
        }
        i = end.max(start + scheme_len);
    }
    out
}

fn starts_with_ignore_case(haystack: &str, prefix: &str) -> bool {
    haystack.len() >= prefix.len() && haystack.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
}

fn next_char_len(text: &str, i: usize) -> usize {
    // Advance one full character so we never split a UTF-8 sequence.
    text[i..].chars().next().map(char::len_utf8).unwrap_or(1)
}

fn is_url_terminator(b: u8) -> bool { b.is_ascii_whitespace() || matches!(b, b'<' | b'>' | b'"' | b'\'' | b'`') }

/// Trim characters that end a sentence, not a URL. Runs to a fixed point so
/// `https://example.com).` sheds both the paren and the period.
fn trim_trailing_punctuation(mut url: &str) -> &str {
    loop {
        let before = url;
        url = url.trim_end_matches(['.', ',', ';', ':', '!', '?']);
        // A closing bracket is part of the URL only if it has an unmatched
        // opener inside the URL (Wikipedia-style paths); otherwise it closed
        // a bracket in the surrounding prose.
        for (open, close) in [('(', ')'), ('[', ']'), ('{', '}')] {
            if url.ends_with(close) {
                let opens = url.matches(open).count();
                let closes = url.matches(close).count();
                if closes > opens {
                    url = &url[..url.len() - 1];
                }
            }
        }
        if url == before {
            return url;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A realistic 22-char base64url entity id (16 bytes, URL_SAFE_NO_PAD).
    const ID_A: &str = "AZk3jW0RvkW8pTGnQxYzAA";
    const ID_B: &str = "AZk3jW0RvkW8pTGnQxYzBB";

    #[test]
    fn no_mentions_in_plain_text() {
        assert!(parse_mentions("hello world, no tokens here").is_empty());
        assert!(parse_mentions("").is_empty());
        assert!(parse_mentions("email@example.com <b>bold</b>").is_empty());
    }

    #[test]
    fn single_mention_extracts_payload() {
        let text = format!("hey <@{ID_A}> take a look");
        assert_eq!(parse_mentions(&text), vec![ID_A.to_string()]);
    }

    #[test]
    fn multiple_mentions_in_order() {
        let text = format!("<@{ID_A}> meet <@{ID_B}>");
        assert_eq!(parse_mentions(&text), vec![ID_A.to_string(), ID_B.to_string()]);
    }

    #[test]
    fn duplicate_mentions_collapse_to_one() {
        // One notification per (recipient, message), no matter how many times
        // the token repeats — dedup is part of the parser contract.
        let text = format!("<@{ID_A}> <@{ID_A}> <@{ID_A}>");
        assert_eq!(parse_mentions(&text), vec![ID_A.to_string()]);
    }

    #[test]
    fn adjacent_tokens_and_no_whitespace() {
        let text = format!("<@{ID_A}><@{ID_B}>trailing");
        assert_eq!(parse_mentions(&text), vec![ID_A.to_string(), ID_B.to_string()]);
    }

    #[test]
    fn unterminated_token_is_ignored() {
        let text = format!("dangling <@{ID_A} and more text");
        assert!(parse_mentions(&text).is_empty());
    }

    #[test]
    fn empty_and_overlong_payloads_are_ignored() {
        assert!(parse_mentions("<@>").is_empty());
        let overlong = format!("<@{}>", "a".repeat(MAX_MENTION_ID_LEN + 1));
        assert!(parse_mentions(&overlong).is_empty());
        // Exactly at the cap still parses (the cap is inclusive headroom).
        let at_cap = format!("<@{}>", "a".repeat(MAX_MENTION_ID_LEN));
        assert_eq!(parse_mentions(&at_cap).len(), 1);
    }

    #[test]
    fn charset_violations_are_ignored() {
        // '+' and '/' are STANDARD base64 but not base64url — to_base64 never
        // emits them, so a token carrying them is foreign and must not parse.
        assert!(parse_mentions("<@abc+def>").is_empty());
        assert!(parse_mentions("<@abc/def>").is_empty());
        assert!(parse_mentions("<@abc def>").is_empty());
        assert!(parse_mentions("<@abc=>").is_empty());
    }

    #[test]
    fn failed_candidate_does_not_swallow_inner_token() {
        // The outer `<@` never terminates validly, but the inner token must
        // still be found — the scanner resumes inside failed candidates.
        let text = format!("<@<@{ID_A}>");
        assert_eq!(parse_mentions(&text), vec![ID_A.to_string()]);
    }

    #[test]
    fn mentions_survive_surrounding_unicode() {
        let text = format!("héllo 世界 <@{ID_A}> 🎉");
        assert_eq!(parse_mentions(&text), vec![ID_A.to_string()]);
    }

    #[test]
    fn no_urls_in_plain_text() {
        assert!(extract_urls("no links here, not even ftp://x.com or file:///etc").is_empty());
        assert!(extract_urls("").is_empty());
        // Bare scheme with nothing after it is prose, not a URL.
        assert!(extract_urls("the prefix is http:// and that's it").is_empty());
    }

    #[test]
    fn extracts_http_and_https() {
        assert_eq!(
            extract_urls("see https://example.com/a and http://other.org"),
            vec!["https://example.com/a".to_string(), "http://other.org".to_string()]
        );
    }

    #[test]
    fn scheme_match_is_case_insensitive_but_url_is_verbatim() {
        // The scheme match tolerates HTTPS:// but the stored key is the text
        // exactly as written — consumers dedup by string equality.
        assert_eq!(extract_urls("HTTPS://Example.COM/Path"), vec!["HTTPS://Example.COM/Path".to_string()]);
    }

    #[test]
    fn trailing_sentence_punctuation_is_trimmed() {
        assert_eq!(extract_urls("read https://example.com/doc."), vec!["https://example.com/doc".to_string()]);
        assert_eq!(extract_urls("what about https://example.com/x?!"), vec!["https://example.com/x".to_string()]);
        assert_eq!(extract_urls("list: https://example.com/y,"), vec!["https://example.com/y".to_string()]);
    }

    #[test]
    fn query_strings_keep_their_innards() {
        // '?' and '&' INSIDE the URL are load-bearing; only trailing ones trim.
        assert_eq!(extract_urls("https://example.com/s?q=rust&x=1 next"), vec!["https://example.com/s?q=rust&x=1".to_string()]);
    }

    #[test]
    fn wrapping_parens_shed_but_balanced_parens_stay() {
        assert_eq!(extract_urls("(see https://example.com/plain)"), vec!["https://example.com/plain".to_string()]);
        assert_eq!(
            extract_urls("(see https://en.wikipedia.org/wiki/Rust_(film))"),
            vec!["https://en.wikipedia.org/wiki/Rust_(film)".to_string()]
        );
        assert_eq!(extract_urls("[link](https://example.com/md)"), vec!["https://example.com/md".to_string()]);
    }

    #[test]
    fn quotes_and_angle_brackets_terminate() {
        assert_eq!(extract_urls("<https://example.com/wrapped>"), vec!["https://example.com/wrapped".to_string()]);
        assert_eq!(extract_urls("\"https://example.com/quoted\""), vec!["https://example.com/quoted".to_string()]);
    }

    #[test]
    fn duplicate_urls_collapse_to_one() {
        assert_eq!(extract_urls("https://example.com https://example.com"), vec!["https://example.com".to_string()]);
    }

    #[test]
    fn urls_survive_surrounding_unicode() {
        assert_eq!(extract_urls("链接 https://example.com/路径 🎉"), vec!["https://example.com/路径".to_string()]);
    }

    #[test]
    fn overlong_urls_are_skipped() {
        let long = format!("https://example.com/{}", "a".repeat(MAX_URL_LEN));
        assert!(extract_urls(&long).is_empty());
    }
}
