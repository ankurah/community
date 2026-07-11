//! Per-backend operation summaries for DAG nodes and feed rows.
//!
//! Yrs ops: ankurah's yrs backend emits `encode_diff_v2` payloads
//! (`ankurah-core/src/property/backend/yrs.rs`), so the app can decode them
//! with the same `yrs` crate (pinned to the exact version ankurah 0.9.0
//! resolves to — see leptos-app/Cargo.toml) and report insert/delete deltas.
//!
//! LWW ops: `LWWDiff`'s fields are private in ankurah 0.9.0
//! (`lww.rs:76-79`), so LWW payloads are honestly opaque here — we show byte
//! sizes only. Per-event LWW value diffs are ankurah#337 piece 2; when that
//! lands these labels upgrade to real "deleted → true" descriptions.

use ankurah::proto::Event;
use yrs::updates::decoder::Decode;

/// Insert/delete delta of one yrs diff, in yrs text units (UTF-8 bytes for
/// ankurah's default Doc config — "chars" for ASCII chat text).
pub fn yrs_delta(diff: &[u8]) -> Option<(u64, u64)> {
    let update = yrs::Update::decode_v2(diff).ok()?;
    // `IdSet` (returned by `insertions`) has no public iterator, but
    // `DeleteSet: From<IdSet>` + `DeleteSet::iter` are public — wrap to walk
    // the ranges. `include_deleted: true` counts content this diff inserted
    // even if the same diff also deleted it (the honest "this event wrote n").
    let inserted = ranges_total(&yrs::DeleteSet::from(update.insertions(true)));
    let deleted = ranges_total(update.delete_set());
    Some((inserted, deleted))
}

/// Total clock-units covered by a delete-set (Σ range lengths, all clients).
fn ranges_total(set: &yrs::DeleteSet) -> u64 {
    set.iter().map(|(_client, ranges)| ranges.iter().map(|r| (r.end - r.start) as u64).sum::<u64>()).sum()
}

/// One rendered badge per backend present in an event.
#[derive(Clone, Debug, PartialEq)]
pub struct OpBadge {
    pub backend: String,
    /// Short human summary: "+12 −3" for yrs, "41 B" for opaque backends.
    pub summary: String,
    pub op_count: usize,
    pub bytes: usize,
}

/// Summarize every backend's ops in an event.
pub fn op_badges(event: &Event) -> Vec<OpBadge> {
    event
        .operations
        .0
        .iter()
        .map(|(backend, ops)| {
            let bytes: usize = ops.iter().map(|op| op.diff.len()).sum();
            let summary = match backend.as_str() {
                "yrs" => {
                    let (ins, del) = ops
                        .iter()
                        .filter_map(|op| yrs_delta(&op.diff))
                        .fold((0u64, 0u64), |(ai, ad), (i, d)| (ai + i, ad + d));
                    match (ins, del) {
                        (0, 0) => format!("{} B", bytes),
                        (i, 0) => format!("+{}", i),
                        (0, d) => format!("\u{2212}{}", d),
                        (i, d) => format!("+{} \u{2212}{}", i, d),
                    }
                }
                // LWW (and anything unknown): payload is opaque client-side
                // pending ankurah#337 piece 2 — show honest byte sizes.
                _ => format!("{} B", bytes),
            };
            OpBadge { backend: backend.clone(), summary, op_count: ops.len(), bytes }
        })
        .collect()
}

