//! The rule that decides where a message timeline breaks into visual groups
//! and where it grows a day divider.
//!
//! Room chat and DM threads must group identically — a reader should not have
//! to learn two layouts — so the rule lives here as one pure function over
//! (author, timestamp) pairs rather than once per list component.

use crate::fmt;

/// Consecutive messages by the same author are visually grouped. A group breaks
/// when the author changes, the local calendar day changes, or the gap between
/// two messages exceeds this many milliseconds (which keeps timestamps honest:
/// a reply an hour later should not hide under the first message's clock).
const GROUP_GAP_MS: i64 = 5 * 60 * 1000;

/// Where one row sits in its group, and whether it opens a new day.
#[derive(Clone, Debug, PartialEq)]
pub struct GroupFlags {
    pub first_in_group: bool,
    pub last_in_group: bool,
    /// Day-separator label rendered above this row when the calendar day
    /// changes. `Some` on the first row of each day, including the first row.
    pub day_label: Option<String>,
}

/// Compute grouping flags for an OLDEST-FIRST list of (author id, timestamp).
pub fn group_flags(rows: &[(String, i64)]) -> Vec<GroupFlags> {
    (0..rows.len())
        .map(|i| {
            let (author, t) = (&rows[i].0, rows[i].1);

            let new_day = match i.checked_sub(1).map(|p| &rows[p]) {
                Some((_, prev_ts)) => fmt::day_key(*prev_ts) != fmt::day_key(t),
                None => true,
            };
            let first_in_group = new_day
                || match i.checked_sub(1).map(|p| &rows[p]) {
                    Some((prev_author, prev_ts)) => prev_author != author || t.saturating_sub(*prev_ts) > GROUP_GAP_MS,
                    None => true,
                };
            let last_in_group = match rows.get(i + 1) {
                Some((next_author, next_ts)) => {
                    next_author != author
                        || fmt::day_key(*next_ts) != fmt::day_key(t)
                        || next_ts.saturating_sub(t) > GROUP_GAP_MS
                }
                None => true,
            };

            GroupFlags { first_in_group, last_in_group, day_label: new_day.then(|| fmt::day_label(t)) }
        })
        .collect()
}
