//! Small presentation helpers shared by the chat views: timestamp formatting,
//! avatar initials, and deterministic per-user avatar hues. Pure formatting —
//! no state, no I/O.

use wasm_bindgen::JsValue;

const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

fn date(ts_ms: i64) -> js_sys::Date { js_sys::Date::new(&JsValue::from_f64(ts_ms as f64)) }

fn ymd(d: &js_sys::Date) -> (u32, u32, u32) { (d.get_full_year(), d.get_month(), d.get_date()) }

/// Local calendar-day key for a timestamp, used to detect day boundaries.
pub fn day_key(ts_ms: i64) -> (u32, u32, u32) { ymd(&date(ts_ms)) }

/// "3:07 PM" style clock time (local).
pub fn clock_time(ts_ms: i64) -> String {
    let d = date(ts_ms);
    let h = d.get_hours();
    let m = d.get_minutes();
    let (h12, ampm) = match h {
        0 => (12, "AM"),
        1..=11 => (h, "AM"),
        12 => (12, "PM"),
        _ => (h - 12, "PM"),
    };
    format!("{}:{:02} {}", h12, m, ampm)
}

/// "Today" / "Yesterday" / "Jul 5" / "Jul 5, 2025" day-separator label.
pub fn day_label(ts_ms: i64) -> String {
    let d = date(ts_ms);
    let now = js_sys::Date::new_0();
    if ymd(&d) == ymd(&now) {
        return "Today".to_string();
    }
    let yesterday = js_sys::Date::new_0();
    // set_date(0) rolls into the previous month, matching JS Date semantics.
    yesterday.set_date(yesterday.get_date().saturating_sub(1));
    if ymd(&d) == ymd(&yesterday) {
        return "Yesterday".to_string();
    }
    let month = MONTHS[(d.get_month() as usize) % 12];
    if d.get_full_year() == now.get_full_year() {
        format!("{} {}", month, d.get_date())
    } else {
        format!("{} {}, {}", month, d.get_date(), d.get_full_year())
    }
}

/// "Jul 2026" — profile "first seen" granularity.
pub fn month_year(ts_ms: i64) -> String {
    let d = date(ts_ms);
    format!("{} {}", MONTHS[(d.get_month() as usize) % 12], d.get_full_year())
}

/// Full stamp for hover titles: "Jul 5, 2026 · 3:07 PM".
pub fn full_stamp(ts_ms: i64) -> String {
    let d = date(ts_ms);
    let month = MONTHS[(d.get_month() as usize) % 12];
    format!("{} {}, {} · {}", month, d.get_date(), d.get_full_year(), clock_time(ts_ms))
}

/// Up-to-two-letter initials from a display name ("Ada Lovelace" → "AL").
pub fn initials(name: &str) -> String {
    let mut words = name.split_whitespace();
    let first = words.next().and_then(|w| w.chars().next());
    let last = words.last().and_then(|w| w.chars().next());
    match (first, last) {
        (Some(a), Some(b)) => format!("{}{}", a.to_uppercase(), b.to_uppercase()),
        (Some(a), None) => a.to_uppercase().to_string(),
        _ => "?".to_string(),
    }
}

/// "moderator" → "Moderator" for role-badge labels (role keys are lowercase
/// ASCII). Shared by the members panel, profile popover, and user detail.
pub fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// Deterministic avatar hue class (`hue-0`..`hue-7`) from a stable id string.
pub fn hue_class(id: &str) -> &'static str {
    let sum: u32 = id.bytes().map(u32::from).sum();
    match sum % 8 {
        0 => "hue-0",
        1 => "hue-1",
        2 => "hue-2",
        3 => "hue-3",
        4 => "hue-4",
        5 => "hue-5",
        6 => "hue-6",
        _ => "hue-7",
    }
}
