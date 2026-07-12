//! Minimal HTML metadata scanner for link previews.
//!
//! Extracts Open Graph tags (`og:title`, `og:description`, `og:image`), the
//! `<title>` element, and `<meta name="description">` from an HTML prefix.
//! Hand-rolled on purpose: a full HTML parser (scraper/html5ever) is a large
//! dependency tree for what is a bounded scan over untrusted bytes — this
//! never builds a DOM, never executes anything, and treats the input as an
//! arbitrary byte soup that merely resembles HTML. Wrong-but-safe output on
//! pathological markup is acceptable; previews are cosmetic.

/// Metadata found in a page. All fields raw-but-entity-decoded; length caps
/// and whitespace sanitation are the caller's policy.
#[derive(Debug, Default, PartialEq)]
pub struct PageMeta {
    pub og_title: Option<String>,
    pub og_description: Option<String>,
    pub og_image: Option<String>,
    pub title: Option<String>,
    pub meta_description: Option<String>,
}

impl PageMeta {
    /// Preview title: og:title, else the `<title>` element.
    pub fn best_title(&self) -> Option<&str> { self.og_title.as_deref().or(self.title.as_deref()) }

    /// Preview description: og:description, else `<meta name="description">`.
    pub fn best_description(&self) -> Option<&str> { self.og_description.as_deref().or(self.meta_description.as_deref()) }
}

/// Scan an HTML document (or truncated prefix — the unfurl worker caps the
/// body, and og/meta tags live in `<head>`) for preview metadata. For each
/// field the FIRST occurrence wins, per the Open Graph convention.
pub fn parse(html: &str) -> PageMeta {
    let mut meta = PageMeta::default();
    let bytes = html.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'<' {
            i += 1;
            continue;
        }
        let rest = &html[i..];
        if has_tag_prefix(rest, "<meta") {
            let (tag_body, after) = read_tag(html, i + "<meta".len());
            apply_meta_tag(&mut meta, tag_body);
            i = after;
        } else if has_tag_prefix(rest, "<title") {
            let (_, content_start) = read_tag(html, i + "<title".len());
            if meta.title.is_none() {
                let text = read_until_close_tag(html, content_start, "</title");
                let cleaned = decode_entities(text.trim());
                if !cleaned.is_empty() {
                    meta.title = Some(cleaned);
                }
            }
            i = content_start;
        } else {
            i += 1;
        }
    }
    meta
}

/// True when `rest` starts with `tag` (ASCII case-insensitive) followed by a
/// tag-name boundary — so `<metadata>` does not match `<meta`.
fn has_tag_prefix(rest: &str, tag: &str) -> bool {
    let bytes = rest.as_bytes();
    if bytes.len() < tag.len() || !bytes[..tag.len()].eq_ignore_ascii_case(tag.as_bytes()) {
        return false;
    }
    match bytes.get(tag.len()) {
        None => true,
        Some(b) => b.is_ascii_whitespace() || *b == b'>' || *b == b'/',
    }
}

/// Consume a tag from `start` (just past the tag name) to its closing `>`,
/// honoring quotes so `content="a > b"` does not end the tag early.
/// Returns (tag body, index just past `>`), or the remainder if unterminated.
fn read_tag(html: &str, start: usize) -> (&str, usize) {
    let bytes = html.as_bytes();
    let mut i = start;
    let mut quote: Option<u8> = None;
    while i < bytes.len() {
        let b = bytes[i];
        match quote {
            Some(q) => {
                if b == q {
                    quote = None;
                }
            }
            None => match b {
                b'"' | b'\'' => quote = Some(b),
                b'>' => return (&html[start..i], i + 1),
                _ => {}
            },
        }
        i += 1;
    }
    (&html[start..], bytes.len())
}

/// Text content up to the ASCII case-insensitive `close` marker (e.g.
/// `</title`), or the rest of the buffer when the document was truncated.
fn read_until_close_tag<'a>(html: &'a str, start: usize, close: &str) -> &'a str {
    let bytes = html.as_bytes();
    let needle = close.as_bytes();
    let mut i = start;
    while i + needle.len() <= bytes.len() {
        if bytes[i..i + needle.len()].eq_ignore_ascii_case(needle) {
            return &html[start..i];
        }
        i += 1;
    }
    &html[start..]
}

fn apply_meta_tag(meta: &mut PageMeta, tag_body: &str) {
    let mut key: Option<String> = None;
    let mut content: Option<String> = None;
    for (name, value) in parse_attrs(tag_body) {
        match name.as_str() {
            // property= is the OG spec; name= is what half the web uses anyway.
            "property" | "name" => key.get_or_insert(value.to_ascii_lowercase()),
            "content" => content.get_or_insert(value),
            _ => continue,
        };
    }
    let (Some(key), Some(content)) = (key, content) else { return };
    let content = decode_entities(content.trim());
    if content.is_empty() {
        return;
    }
    let slot = match key.as_str() {
        "og:title" => &mut meta.og_title,
        "og:description" => &mut meta.og_description,
        // og:image:url is the structured-property alias for og:image.
        "og:image" | "og:image:url" => &mut meta.og_image,
        "description" => &mut meta.meta_description,
        _ => return,
    };
    if slot.is_none() {
        *slot = Some(content);
    }
}

/// Split a tag body into (lowercased name, value) attribute pairs. Handles
/// double-quoted, single-quoted, and bare values; valueless attributes are
/// kept with an empty value.
fn parse_attrs(tag_body: &str) -> Vec<(String, String)> {
    let bytes = tag_body.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        // Skip whitespace and stray slashes (self-closing syntax).
        while i < bytes.len() && (bytes[i].is_ascii_whitespace() || bytes[i] == b'/') {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        let name_start = i;
        while i < bytes.len() && !bytes[i].is_ascii_whitespace() && bytes[i] != b'=' && bytes[i] != b'/' {
            i += 1;
        }
        if i == name_start {
            i += 1; // safety: never stall on unexpected bytes
            continue;
        }
        let name = tag_body[name_start..i].to_ascii_lowercase();
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() || bytes[i] != b'=' {
            out.push((name, String::new()));
            continue;
        }
        i += 1; // consume '='
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        let value = if i < bytes.len() && (bytes[i] == b'"' || bytes[i] == b'\'') {
            let quote = bytes[i];
            i += 1;
            let value_start = i;
            while i < bytes.len() && bytes[i] != quote {
                i += 1;
            }
            let v = &tag_body[value_start..i];
            i = (i + 1).min(bytes.len()); // consume closing quote
            v
        } else {
            let value_start = i;
            while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            &tag_body[value_start..i]
        };
        out.push((name, value.to_string()));
    }
    out
}

/// Decode the handful of HTML entities that actually appear in titles and
/// descriptions. Unknown entities pass through literally — this is a
/// best-effort cosmetic decode, not an HTML conformance exercise.
fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(pos) = rest.find('&') {
        out.push_str(&rest[..pos]);
        rest = &rest[pos..];
        // Byte-scan for the terminator: a `[..12]` STR slice could split a
        // multibyte char and panic; ';' and '&' are ASCII so byte positions
        // found this way are always char boundaries.
        let window = &rest.as_bytes()[..rest.len().min(12)];
        let Some(semi) = window.iter().position(|&b| b == b';') else {
            // No terminator nearby: not an entity, emit '&' and move on.
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let entity = &rest[1..semi];
        let decoded: Option<String> = match entity {
            "amp" => Some("&".into()),
            "lt" => Some("<".into()),
            "gt" => Some(">".into()),
            "quot" => Some("\"".into()),
            "apos" => Some("'".into()),
            "nbsp" => Some(" ".into()),
            _ if entity.starts_with("#x") || entity.starts_with("#X") => decode_numeric(u32::from_str_radix(&entity[2..], 16).ok()),
            _ if entity.starts_with('#') => decode_numeric(entity[1..].parse::<u32>().ok()),
            _ => None,
        };
        match decoded {
            Some(d) => {
                out.push_str(&d);
                rest = &rest[semi + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Numeric entity policy: a valid codepoint decodes to itself, EXCEPT control
/// characters, which decode to nothing — `Some(empty)` consumes the entity
/// so a crafted `&#0;`/`&#x1b;` can neither smuggle a control byte nor leave
/// its raw text in stored rows. An unparseable number returns `None`
/// (pass-through as literal text, like any unknown entity).
fn decode_numeric(codepoint: Option<u32>) -> Option<String> {
    let c = char::from_u32(codepoint?)?;
    Some(if c.is_control() { String::new() } else { String::from(c) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_og_page() {
        let html = r#"<html><head>
            <meta property="og:title" content="The Title" />
            <meta property="og:description" content="A description."/>
            <meta property="og:image" content="https://cdn.example.com/img.png">
            <title>Fallback Title</title>
        </head><body>hi</body></html>"#;
        let m = parse(html);
        assert_eq!(m.og_title.as_deref(), Some("The Title"));
        assert_eq!(m.og_description.as_deref(), Some("A description."));
        assert_eq!(m.og_image.as_deref(), Some("https://cdn.example.com/img.png"));
        assert_eq!(m.title.as_deref(), Some("Fallback Title"));
        assert_eq!(m.best_title(), Some("The Title"));
    }

    #[test]
    fn title_fallback_when_no_og() {
        let m = parse("<head><title>Just a Title</title></head>");
        assert_eq!(m.best_title(), Some("Just a Title"));
        assert_eq!(m.best_description(), None);
    }

    #[test]
    fn meta_name_variants_and_attr_order() {
        // name= instead of property=, content BEFORE the key, single quotes,
        // uppercase tag/attr names — all common in the wild.
        let html = r#"<META CONTENT='Swapped' NAME='og:title'>
                      <meta name="description" content="plain desc">"#;
        let m = parse(html);
        assert_eq!(m.og_title.as_deref(), Some("Swapped"));
        assert_eq!(m.best_description(), Some("plain desc"));
    }

    #[test]
    fn first_occurrence_wins() {
        let html = r#"<meta property="og:title" content="First">
                      <meta property="og:title" content="Second">"#;
        assert_eq!(parse(html).og_title.as_deref(), Some("First"));
    }

    #[test]
    fn quoted_gt_does_not_truncate_tag() {
        let html = r#"<meta property="og:title" content="A > B < C">"#;
        assert_eq!(parse(html).og_title.as_deref(), Some("A > B < C"));
    }

    #[test]
    fn entities_are_decoded() {
        let html = r#"<meta property="og:title" content="Fish &amp; Chips &#8212; caf&#xE9; &quot;menu&quot;">"#;
        assert_eq!(parse(html).og_title.as_deref(), Some("Fish & Chips — café \"menu\""));
    }

    #[test]
    fn unknown_and_malformed_entities_pass_through() {
        let html = r#"<title>AT&T &notanentity; &unterminated</title>"#;
        assert_eq!(parse(html).title.as_deref(), Some("AT&T &notanentity; &unterminated"));
    }

    #[test]
    fn control_char_entities_are_dropped() {
        // A crafted page must not smuggle control bytes into stored rows.
        let html = r#"<meta property="og:title" content="clean&#0;&#x1b;value">"#;
        assert_eq!(parse(html).og_title.as_deref(), Some("cleanvalue"));
    }

    #[test]
    fn metadata_tag_is_not_meta() {
        let html = r#"<metadata property="og:title" content="nope"></metadata>"#;
        assert_eq!(parse(html).og_title, None);
    }

    #[test]
    fn truncated_document_still_yields_prefix_data() {
        // The unfurl worker caps the body at a fixed byte budget; a cut-off
        // tag or title must not panic and earlier finds must survive.
        let html = r#"<meta property="og:title" content="Kept"><title>cut off midw"#;
        let m = parse(html);
        assert_eq!(m.og_title.as_deref(), Some("Kept"));
        assert_eq!(m.title.as_deref(), Some("cut off midw"));
    }

    #[test]
    fn og_image_url_alias() {
        let html = r#"<meta property="og:image:url" content="https://x/i.png">"#;
        assert_eq!(parse(html).og_image.as_deref(), Some("https://x/i.png"));
    }

    #[test]
    fn empty_content_is_ignored() {
        let html = r#"<meta property="og:title" content="">
                      <meta property="og:title" content="Real">"#;
        assert_eq!(parse(html).og_title.as_deref(), Some("Real"));
    }

    #[test]
    fn bare_and_valueless_attributes_do_not_derail() {
        let html = r#"<meta property=og:title content=BareWord data-x>"#;
        assert_eq!(parse(html).og_title.as_deref(), Some("BareWord"));
    }

    #[test]
    fn non_html_soup_is_harmless() {
        assert_eq!(parse(""), PageMeta::default());
        assert_eq!(parse("{\"json\": true}"), PageMeta::default());
        assert_eq!(parse("<<<>>>< meta >"), PageMeta::default());
        // Binary-ish input via lossy decode: must not panic.
        let soup = String::from_utf8_lossy(&[0x3c, 0xff, 0xfe, 0x3e, 0x3c]).into_owned();
        let _ = parse(&soup);
    }
}
