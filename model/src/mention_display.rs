//! Display-form coding for mention tokens (#56): `<@BASE64_ID>` ↔ `@DisplayName`.
//!
//! CLIENT-side support, not a server contract: the wire/storage format stays
//! the canonical token (the [`crate::text`] scanner both sides share), but
//! the composer SHOWS plain `@DisplayName` text — encoding happens once at
//! send time, decoding once when an edit is mirrored into the textarea. The
//! module lives here, beside the token format it mirrors, so its tests run
//! with the model gate; nothing in it changes [`crate::text`] semantics.
//!
//! Coding rules:
//! - Encode rewrites an `@` run to a token only when it exactly matches a
//!   known member name (case-sensitive, longest name first — names can
//!   contain spaces) at a word boundary on both sides. Everything else —
//!   including `:` smileys, emails, and names that stopped matching — stays
//!   literal text, which the server scanner simply never notifies.
//! - Code is verbatim in both directions (the renderer shows literal tokens
//!   inside code, so the composer must too): fenced blocks and inline
//!   backtick spans are skipped via a minimal CommonMark approximation.
//!   Where the approximation disagrees with the real renderer the failure
//!   is cosmetic — a mention renders literal, or literal text renders as a
//!   mention chip — never a rewrite of unrelated text.
//! - Decode is conservative: a token stays raw in the editor unless its
//!   member is known, its name is unambiguous, and the whole decode
//!   round-trips (`encode(decode(text)) == text`); otherwise the STORED
//!   text is returned unchanged. Raw tokens in the textarea are ugly but
//!   honest — a save can then never silently rewrite a mention to a
//!   different member.
//! - Ambiguous names (two members share one) encode to the id the caller
//!   `preferred` (the composer records autocomplete picks), else to the
//!   lowest id, deterministically. The losing twin is still mentionable
//!   through autocomplete, which records the pick.

use std::collections::HashMap;
use std::ops::Range;

use crate::text::MAX_MENTION_ID_LEN;

/// Known members, indexed both ways for coding. Build one per operation from
/// the caller's current users list — construction is cheap and a snapshot is
/// exactly right (coding must not shift under a live rename mid-keystroke).
pub struct MemberDirectory {
    name_by_id: HashMap<String, String>,
    /// ids per display name, sorted — `[0]` is the deterministic fallback
    /// for ambiguous names; `len > 1` marks the ambiguity itself.
    ids_by_name: HashMap<String, Vec<String>>,
    /// Names longest-first (ties alphabetical), for greedy longest-match.
    names_desc: Vec<String>,
}

impl MemberDirectory {
    pub fn new(members: impl IntoIterator<Item = (String, String)>) -> Self {
        let mut name_by_id = HashMap::new();
        let mut ids_by_name: HashMap<String, Vec<String>> = HashMap::new();
        for (id, name) in members {
            if name.is_empty() {
                continue;
            }
            ids_by_name.entry(name.clone()).or_default().push(id.clone());
            name_by_id.insert(id, name);
        }
        for ids in ids_by_name.values_mut() {
            ids.sort();
            ids.dedup();
        }
        let mut names_desc: Vec<String> = ids_by_name.keys().cloned().collect();
        names_desc.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
        Self { name_by_id, ids_by_name, names_desc }
    }

    /// Encode display text for the wire: each `@DisplayName` run matching a
    /// known member becomes `<@id>`. Existing well-formed tokens and code
    /// spans copy through verbatim (so encode is idempotent and can never
    /// corrupt a token already present).
    pub fn encode(&self, text: &str, preferred: &HashMap<String, String>) -> String {
        let code = code_ranges(text);
        let bytes = text.as_bytes();
        let mut out = String::with_capacity(text.len());
        let mut i = 0;
        // Word boundary: start of text, after any non-alphanumeric char, or
        // right after a token/match (so adjacent mentions — "@Ann@Bob" —
        // code independently).
        let mut boundary = true;
        while i < bytes.len() {
            if !range_covers(&code, i) {
                if let Some((range, _)) = token_at(text, i) {
                    out.push_str(&text[range.clone()]);
                    i = range.end;
                    boundary = true;
                    continue;
                }
                if bytes[i] == b'@' && boundary {
                    if let Some((name, id)) = self.match_name(&text[i + 1..], preferred) {
                        out.push_str("<@");
                        out.push_str(&id);
                        out.push('>');
                        i += 1 + name.len();
                        boundary = true;
                        continue;
                    }
                }
            }
            let c = text[i..].chars().next().expect("i is a char boundary");
            out.push(c);
            boundary = !c.is_alphanumeric();
            i += c.len_utf8();
        }
        out
    }

    /// Decode stored text for the editor: qualifying `<@id>` tokens become
    /// `@DisplayName`. Falls back to the stored text wholesale when the
    /// result would not re-encode byte-identically (see the module doc).
    pub fn decode(&self, text: &str) -> String {
        let decoded = self.decode_unchecked(text);
        if decoded != text && self.encode(&decoded, &HashMap::new()) != text {
            // Round-trip hazard: e.g. plain-text @Name runs already in the
            // stored message (pre-#56 or hand-typed), or one decoded name
            // shadowing another. Raw tokens are the honest fallback.
            return text.to_string();
        }
        decoded
    }

    fn decode_unchecked(&self, text: &str) -> String {
        let code = code_ranges(text);
        let bytes = text.as_bytes();
        let mut out = String::with_capacity(text.len());
        let mut i = 0;
        let mut boundary = true;
        while i < bytes.len() {
            if boundary && !range_covers(&code, i) {
                if let Some((range, id)) = token_at(text, i) {
                    // The decoded `@Name` must sit where encode would re-match
                    // it: name known, unambiguous, and a word boundary after
                    // the token (otherwise "<@a>x" would decode into "@Annx").
                    let name = self.name_by_id.get(id).filter(|n| self.ids_by_name[*n].len() == 1);
                    let end_ok = text[range.end..].chars().next().map(|c| !c.is_alphanumeric()).unwrap_or(true);
                    if let (Some(name), true) = (name, end_ok) {
                        out.push('@');
                        out.push_str(name);
                        i = range.end;
                        // boundary stays true: a following token decodes too.
                        continue;
                    }
                }
            }
            let c = text[i..].chars().next().expect("i is a char boundary");
            out.push(c);
            boundary = !c.is_alphanumeric();
            i += c.len_utf8();
        }
        out
    }

    /// Longest known name prefixing `after_at`, with a word boundary after
    /// it, and the id it encodes to.
    fn match_name(&self, after_at: &str, preferred: &HashMap<String, String>) -> Option<(&str, String)> {
        for name in &self.names_desc {
            let Some(rest) = after_at.strip_prefix(name.as_str()) else { continue };
            if rest.chars().next().map(|c| c.is_alphanumeric()).unwrap_or(false) {
                continue; // "@Dan" must not eat the front of "@Daniel"
            }
            let ids = &self.ids_by_name[name];
            let id = preferred.get(name).filter(|p| ids.contains(p)).cloned().unwrap_or_else(|| ids[0].clone());
            return Some((name, id));
        }
        None
    }
}

/// A well-formed mention token starting at byte `i`: `(byte range incl.
/// delimiters, id payload)`. Mirrors the [`crate::text::parse_mentions`]
/// token rules (base64url payload of 1..=[`MAX_MENTION_ID_LEN`], then `>`),
/// positionally.
fn token_at(text: &str, i: usize) -> Option<(Range<usize>, &str)> {
    let bytes = text.as_bytes();
    if i + 1 >= bytes.len() || bytes[i] != b'<' || bytes[i + 1] != b'@' {
        return None;
    }
    let start = i + 2;
    let mut end = start;
    while end < bytes.len() && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'-' || bytes[end] == b'_') {
        end += 1;
    }
    let len = end - start;
    ((1..=MAX_MENTION_ID_LEN).contains(&len) && end < bytes.len() && bytes[end] == b'>').then(|| (i..end + 1, &text[start..end]))
}

/// Byte ranges covered by code — fenced blocks and inline backtick spans —
/// where mention coding must not look. Minimal CommonMark approximation:
/// - A line whose first non-space (≤3) run is 3+ backticks/tildes opens a
///   fence; a later line that is just an equal-or-longer run of the same
///   char (plus whitespace) closes it. Unclosed runs to the end.
/// - Outside fences, a run of N backticks opens an inline span closed by
///   the next run of exactly N backticks; an unmatched opener is literal.
fn code_ranges(text: &str) -> Vec<Range<usize>> {
    let mut ranges: Vec<Range<usize>> = Vec::new();
    let mut fence: Option<(u8, usize, usize)> = None; // (char, run len, block start)
    let mut plain_start = 0;
    let mut pos = 0;
    for line in text.split_inclusive('\n') {
        match (fence, fence_run(line)) {
            (None, Some((ch, len))) => {
                inline_code_spans(&text[plain_start..pos], plain_start, &mut ranges);
                fence = Some((ch, len, pos));
            }
            (Some((ch, len, start)), Some((c, l))) if c == ch && l >= len && closes_fence(line) => {
                ranges.push(start..pos + line.len());
                fence = None;
                plain_start = pos + line.len();
            }
            _ => {}
        }
        pos += line.len();
    }
    match fence {
        Some((_, _, start)) => ranges.push(start..text.len()),
        None => inline_code_spans(&text[plain_start..], plain_start, &mut ranges),
    }
    ranges
}

/// The fence run opening `line`, if any: (fence char, run length).
fn fence_run(line: &str) -> Option<(u8, usize)> {
    let trimmed = line.trim_start_matches(' ');
    if line.len() - trimmed.len() > 3 {
        return None; // 4+ leading spaces is indented code, not a fence
    }
    let ch = *trimmed.as_bytes().first()?;
    if ch != b'`' && ch != b'~' {
        return None;
    }
    let len = trimmed.bytes().take_while(|&b| b == ch).count();
    (len >= 3).then_some((ch, len))
}

/// Whether a fence-run line qualifies as a CLOSER: nothing but the run and
/// whitespace (an info string re-opens, it doesn't close).
fn closes_fence(line: &str) -> bool {
    line.trim().bytes().all(|b| b == b'`' || b == b'~')
}

/// Inline backtick spans within `segment` (at byte offset `base` of the full
/// text), appended to `out`.
fn inline_code_spans(segment: &str, base: usize, out: &mut Vec<Range<usize>>) {
    let bytes = segment.as_bytes();
    let run_len = |from: usize| bytes[from..].iter().take_while(|&&b| b == b'`').count();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'`' {
            i += 1;
            continue;
        }
        let open = run_len(i);
        let mut j = i + open;
        let mut closed = false;
        while j < bytes.len() {
            if bytes[j] == b'`' {
                let close = run_len(j);
                if close == open {
                    out.push(base + i..base + j + close);
                    i = j + close;
                    closed = true;
                    break;
                }
                j += close;
            } else {
                j += 1;
            }
        }
        if !closed {
            i += open; // unmatched opener: literal backticks
        }
    }
}

fn range_covers(ranges: &[Range<usize>], pos: usize) -> bool { ranges.iter().any(|r| r.contains(&pos)) }

#[cfg(test)]
mod tests {
    use super::*;

    const ANN: &str = "AZk3jW0RvkW8pTGnQxYzAA";
    const BOB: &str = "AZk3jW0RvkW8pTGnQxYzBB";
    const CAL: &str = "AZk3jW0RvkW8pTGnQxYzCC";

    fn dir(members: &[(&str, &str)]) -> MemberDirectory {
        MemberDirectory::new(members.iter().map(|(id, name)| (id.to_string(), name.to_string())))
    }

    fn none() -> HashMap<String, String> { HashMap::new() }

    #[test]
    fn encode_simple_and_boundaries() {
        let d = dir(&[(ANN, "Ann"), (BOB, "Bob Smith")]);
        assert_eq!(d.encode("hey @Ann look", &none()), format!("hey <@{ANN}> look"));
        // Names can contain spaces; the match is greedy against the longest name.
        assert_eq!(d.encode("@Bob Smith rocks", &none()), format!("<@{BOB}> rocks"));
        // No boundary after the run: "@Ann" must not eat the front of "@Anna".
        assert_eq!(d.encode("hello @Anna", &none()), "hello @Anna");
        // Mid-word @ (emails) stays literal.
        assert_eq!(d.encode("mail me@Ann now", &none()), "mail me@Ann now");
        // Case-sensitive: hand-typed lowercase stays literal text.
        assert_eq!(d.encode("hi @ann", &none()), "hi @ann");
        // Punctuation before @ is a boundary ("(@Ann)" mentions).
        assert_eq!(d.encode("(@Ann)", &none()), format!("(<@{ANN}>)"));
    }

    #[test]
    fn encode_prefers_longest_name() {
        let d = dir(&[(ANN, "Dan"), (BOB, "Dan Brown")]);
        assert_eq!(d.encode("@Dan Brown wrote", &none()), format!("<@{BOB}> wrote"));
        assert_eq!(d.encode("@Dan wrote", &none()), format!("<@{ANN}> wrote"));
    }

    #[test]
    fn encode_skips_code() {
        let d = dir(&[(ANN, "Ann")]);
        assert_eq!(d.encode("`@Ann` and @Ann", &none()), format!("`@Ann` and <@{ANN}>"));
        let fenced = "```\n@Ann\n```\n@Ann";
        assert_eq!(d.encode(fenced, &none()), format!("```\n@Ann\n```\n<@{ANN}>"));
        // Unclosed fence runs to the end.
        assert_eq!(d.encode("```rust\n@Ann", &none()), "```rust\n@Ann");
        // An unmatched single backtick is literal, not an open span.
        assert_eq!(d.encode("a ` b @Ann", &none()), format!("a ` b <@{ANN}>"));
    }

    #[test]
    fn encode_is_idempotent_on_existing_tokens() {
        let d = dir(&[(ANN, "Ann")]);
        let wire = format!("hi <@{ANN}> again");
        assert_eq!(d.encode(&wire, &none()), wire);
    }

    #[test]
    fn ambiguous_names_use_preference_then_lowest_id() {
        let d = dir(&[(BOB, "Ann"), (ANN, "Ann")]);
        // Deterministic fallback: lowest id.
        assert_eq!(d.encode("@Ann", &none()), format!("<@{ANN}>"));
        // The recorded autocomplete pick wins.
        let picks = HashMap::from([("Ann".to_string(), BOB.to_string())]);
        assert_eq!(d.encode("@Ann", &picks), format!("<@{BOB}>"));
        // A stale pick pointing at a non-member is ignored.
        let stale = HashMap::from([("Ann".to_string(), CAL.to_string())]);
        assert_eq!(d.encode("@Ann", &stale), format!("<@{ANN}>"));
    }

    #[test]
    fn decode_round_trips() {
        let d = dir(&[(ANN, "Ann"), (BOB, "Bob Smith")]);
        let wire = format!("hey <@{ANN}> meet <@{BOB}>!");
        let display = d.decode(&wire);
        assert_eq!(display, "hey @Ann meet @Bob Smith!");
        assert_eq!(d.encode(&display, &none()), wire);
        // Adjacent tokens decode and re-encode independently.
        let adjacent = format!("<@{ANN}><@{BOB}>");
        assert_eq!(d.decode(&adjacent), "@Ann@Bob Smith");
        assert_eq!(d.encode(&d.decode(&adjacent), &none()), adjacent);
    }

    #[test]
    fn decode_leaves_code_and_unknown_ids_raw() {
        let d = dir(&[(ANN, "Ann")]);
        let in_code = format!("`<@{ANN}>` fine");
        assert_eq!(d.decode(&in_code), in_code);
        let unknown = format!("hi <@{CAL}>");
        assert_eq!(d.decode(&unknown), unknown);
    }

    #[test]
    fn decode_falls_back_on_round_trip_hazards() {
        // Ambiguous name: decoding could re-encode to the twin — stay raw.
        let d = dir(&[(ANN, "Ann"), (BOB, "Ann")]);
        let wire = format!("hi <@{BOB}>");
        assert_eq!(d.decode(&wire), wire);
        // A token glued to word chars would decode into a different name run.
        let d = dir(&[(ANN, "Ann")]);
        let glued = format!("x<@{ANN}>y");
        assert_eq!(d.decode(&glued), glued);
        // Pre-existing plain-text @Name beside a token: decoding is safe only
        // if re-encoding wouldn't promote the plain run — so stay raw.
        let mixed = format!("plain @Ann and <@{ANN}>");
        assert_eq!(d.decode(&mixed), mixed);
    }

    #[test]
    fn tilde_fences_and_closing_rules() {
        let d = dir(&[(ANN, "Ann")]);
        let fenced = "~~~\n@Ann\n~~~\n@Ann";
        assert_eq!(d.encode(fenced, &none()), format!("~~~\n@Ann\n~~~\n<@{ANN}>"));
        // A shorter run does not close; the fence runs on.
        let nested = "````\n@Ann\n```\n@Ann";
        assert_eq!(d.encode(nested, &none()), nested);
    }
}
