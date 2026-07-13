//! `:shortcode:` → unicode emoji for the composer (#54).
//!
//! Contract: replacement happens at INPUT time only — storage stays plain
//! unicode, so no consumer (server scanners, notifications, future search)
//! ever needs a render-time `:name:` mapping. A `:name:` that was never
//! autocompleted or table-matched just stays as typed.
//!
//! The table is curated, not exhaustive: common expressions plus the
//! dev-flavored set a project chat reaches for. Names follow the widespread
//! GitHub/Slack shortcodes (lowercase `[a-z0-9_+-]`), one glyph per row —
//! duplicates like `+1`/`thumbsup` are separate rows on purpose. Glyphs are
//! literal (the name beside each is its documentation); codepoints that
//! default to TEXT presentation carry an explicit U+FE0F variation selector.
//! The reaction picker's six (reactions.rs) are referenced by index so they
//! stay reachable by shortcode no matter how that set evolves.

use crate::reactions::REACTION_EMOJIS as R;

/// (shortcode, glyph), sorted by shortcode. `R[..]` entries are the reaction
/// picker's set: 👍 ❤️ 😂 🎉 😕 👀.
pub const EMOJI: [(&str, &str); 100] = [
    ("+1", R[0]),
    ("-1", "👎"),
    ("100", "💯"),
    ("alien", "👽"),
    ("angry", "😠"),
    ("beer", "🍺"),
    ("bell", "🔔"),
    ("blush", "😊"),
    ("boom", "💥"),
    ("brain", "🧠"),
    ("broken_heart", "💔"),
    ("bug", "🐛"),
    ("bulb", "💡"),
    ("cake", "🎂"),
    ("calendar", "📅"),
    ("cat", "🐱"),
    ("check", "✅"),
    ("clap", "👏"),
    ("clipboard", "📋"),
    ("coffee", "☕"),
    ("confused", R[4]),
    ("cool", "😎"),
    ("crab", "🦀"),
    ("crossed_fingers", "🤞"),
    ("cry", "😢"),
    ("dart", "🎯"),
    ("dog", "🐶"),
    ("exclamation", "❗"),
    ("exploding_head", "🤯"),
    ("eyes", R[5]),
    ("facepalm", "🤦"),
    ("fire", "🔥"),
    ("fist", "✊"),
    ("flushed", "😳"),
    ("gear", "⚙\u{FE0F}"),
    ("gift", "🎁"),
    ("grimacing", "😬"),
    ("grin", "😁"),
    ("handshake", "🤝"),
    ("heart", R[1]),
    ("heart_eyes", "😍"),
    ("joy", R[2]),
    ("key", "🔑"),
    ("laughing", "😆"),
    ("link", "🔗"),
    ("lock", "🔒"),
    ("mag", "🔍"),
    ("memo", "📝"),
    ("muscle", "💪"),
    ("ok_hand", "👌"),
    ("package", "📦"),
    ("partying_face", "🥳"),
    ("pensive", "😔"),
    ("pin", "📌"),
    ("pizza", "🍕"),
    ("pleading_face", "🥺"),
    ("point_down", "👇"),
    ("point_right", "👉"),
    ("poop", "💩"),
    ("pray", "🙏"),
    ("question", "❓"),
    ("raised_hands", "🙌"),
    ("relieved", "😌"),
    ("robot", "🤖"),
    ("rocket", "🚀"),
    ("rofl", "🤣"),
    ("roll_eyes", "🙄"),
    ("rotating_light", "🚨"),
    ("scream", "😱"),
    ("seedling", "🌱"),
    ("ship", "🚢"),
    ("shrug", "🤷"),
    ("skull", "💀"),
    ("sleeping", "😴"),
    ("smile", "😄"),
    ("smiley", "😃"),
    ("smirk", "😏"),
    ("snowflake", "❄\u{FE0F}"),
    ("sob", "😭"),
    ("sparkles", "✨"),
    ("star", "⭐"),
    ("star_struck", "🤩"),
    ("sun", "☀\u{FE0F}"),
    ("sweat_smile", "😅"),
    ("tada", R[3]),
    ("thinking", "🤔"),
    ("thumbsdown", "👎"),
    ("thumbsup", R[0]),
    ("tools", "🛠\u{FE0F}"),
    ("unamused", "😒"),
    ("upside_down", "🙃"),
    ("v", "✌\u{FE0F}"),
    ("warning", "⚠\u{FE0F}"),
    ("wave", "👋"),
    ("wink", "😉"),
    ("worried", "😟"),
    ("wrench", "🔧"),
    ("x", "❌"),
    ("yum", "😋"),
    ("zap", "⚡"),
];

/// Rank shortcodes for the popup, mirroring the mention ranking (#18):
/// prefix matches first, then substring matches, alphabetical within each
/// tier; at most `max`. Matching is case-insensitive (the table is lowercase).
pub fn candidates(query: &str, max: usize) -> Vec<(&'static str, &'static str)> {
    let q = query.to_ascii_lowercase();
    let mut ranked: Vec<(bool, &'static str, &'static str)> = EMOJI
        .iter()
        .filter_map(|(name, glyph)| {
            if name.starts_with(&q) {
                Some((false, *name, *glyph))
            } else if name.contains(&q) {
                Some((true, *name, *glyph))
            } else {
                None
            }
        })
        .collect();
    ranked.sort_by(|a, b| (a.0, a.1).cmp(&(b.0, b.1)));
    ranked.truncate(max);
    ranked.into_iter().map(|(_, name, glyph)| (name, glyph)).collect()
}

/// Exact-name lookup for a completed `:name:` run (case-insensitive).
pub fn lookup(name: &str) -> Option<&'static str> {
    let q = name.to_ascii_lowercase();
    EMOJI.iter().find(|(n, _)| *n == q).map(|(_, glyph)| *glyph)
}
