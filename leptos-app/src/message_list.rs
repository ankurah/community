use std::collections::{HashMap, HashSet};

use leptos::prelude::*;

use ankurah::LiveQuery;
use ankurah_signals::Get as AnkurahGet;
use community_model::{LinkPreviewView, MessageView, ReactionView, UserView};

use crate::fmt;
use crate::message_row::MessageRow;
use crate::reactions::{picker_index, ReactionChip};

/// Consecutive messages by the same author are visually grouped. A group breaks
/// when the author changes, the local calendar day changes, or the gap between
/// two messages exceeds this many milliseconds (keeps timestamps honest).
const GROUP_GAP_MS: i64 = 5 * 60 * 1000;

/// One renderable row: a message plus its computed grouping context.
#[derive(Clone)]
struct RowCtx {
    message: MessageView,
    first_in_group: bool,
    last_in_group: bool,
    /// Day-separator label rendered above this row when the calendar day changes.
    day_label: Option<String>,
}

fn author_id(m: &MessageView) -> String { m.user().map(|r| r.id().to_base64()).unwrap_or_default() }

fn ts(m: &MessageView) -> i64 { m.timestamp().unwrap_or(0) }

/// Compute grouping flags for an oldest-first message list.
fn group_rows(msgs: &[MessageView]) -> Vec<RowCtx> {
    let n = msgs.len();
    (0..n)
        .map(|i| {
            let t = ts(&msgs[i]);
            let author = author_id(&msgs[i]);

            let new_day = match i.checked_sub(1).map(|p| &msgs[p]) {
                Some(prev) => fmt::day_key(ts(prev)) != fmt::day_key(t),
                None => true,
            };
            let first_in_group = new_day
                || match i.checked_sub(1).map(|p| &msgs[p]) {
                    Some(prev) => author_id(prev) != author || t.saturating_sub(ts(prev)) > GROUP_GAP_MS,
                    None => true,
                };
            let last_in_group = match msgs.get(i + 1) {
                Some(next) => {
                    let nt = ts(next);
                    author_id(next) != author
                        || fmt::day_key(nt) != fmt::day_key(t)
                        || nt.saturating_sub(t) > GROUP_GAP_MS
                }
                None => true,
            };

            RowCtx {
                message: msgs[i].clone(),
                first_in_group,
                last_in_group,
                day_label: new_day.then(|| fmt::day_label(t)),
            }
        })
        .collect()
}

/// Message list component that displays messages grouped by author and day.
#[component]
pub fn MessageList(
    #[prop(into)] messages: Signal<Vec<MessageView>>,
    users: LiveQuery<UserView>,
    current_user_id: Option<String>,
    editing_message: RwSignal<Option<MessageView>>,
    /// The composer's reply state (#23), armed from the rows' context menus.
    replying_to: RwSignal<Option<MessageView>>,
) -> impl IntoView {
    let rows = Signal::derive(move || group_rows(&messages.get()));

    // Mention rendering (#18): one id → display-name map shared by every row,
    // rebuilt when the users list (or any display name — View field reads are
    // tracked) changes. Rows' text closures read it through `.with`, so a
    // rename re-renders mentions live without per-row user lookups.
    let mention_names = Memo::new({
        let users = users.clone();
        move |_| {
            users
                .get()
                .iter()
                .filter_map(|u| {
                    let name = u.display_name().unwrap_or_default();
                    (!name.is_empty()).then(|| (u.id().to_base64(), name))
                })
                .collect::<HashMap<String, String>>()
        }
    });

    // Link previews (#20): one standing LiveQuery for the whole list, same
    // shape and rationale as the reactions query below — LinkPreview rows are
    // keyed by url with no room ref, and per-row queries would churn with the
    // virtual scroller. `ok = false` rows are excluded: the client renders
    // failed unfurls as plain links, so it never needs them. Grouped into a
    // url-keyed map (the mention_names/reaction_chips idiom) so each row's
    // lookup is O(its own URLs), not a scan of every preview ever cached.
    let link_previews =
        crate::ctx().query::<LinkPreviewView>("ok = true").expect("failed to create LinkPreviewView LiveQuery");
    let previews_by_url = Memo::new(move |_| {
        link_previews.get().into_iter().filter_map(|p| p.url().ok().map(|u| (u, p))).collect::<HashMap<String, LinkPreviewView>>()
    });

    // Reactions (#14): one standing LiveQuery over active reactions, grouped
    // into render-ready chips per message id. Reaction has no room ref, so a
    // room-scoped predicate is inexpressible; per-row queries would churn
    // subscriptions with every virtual-scroll mount/unmount. See reactions.rs.
    let reactions = crate::ctx().query::<ReactionView>("active = true").expect("failed to create ReactionView LiveQuery");
    let viewer_id = current_user_id.clone();
    let reaction_chips = Memo::new(move |_| {
        // Distinct users per (message, emoji): duplicate rows (possible under
        // concurrent first-toggles) count once.
        let mut sets: HashMap<String, HashMap<String, HashSet<String>>> = HashMap::new();
        for row in reactions.get().iter() {
            if !row.active().unwrap_or(false) {
                continue;
            }
            let (Ok(message), Ok(user), Ok(emoji)) = (row.message(), row.user(), row.emoji()) else {
                continue;
            };
            sets.entry(message.id().to_base64())
                .or_default()
                .entry(emoji)
                .or_default()
                .insert(user.id().to_base64());
        }
        sets.into_iter()
            .map(|(message_id, by_emoji)| {
                let mut chips: Vec<ReactionChip> = by_emoji
                    .into_iter()
                    .map(|(emoji, users)| ReactionChip {
                        mine: viewer_id.as_deref().map(|id| users.contains(id)).unwrap_or(false),
                        count: users.len(),
                        emoji,
                    })
                    .collect();
                chips.sort_by(|a, b| (picker_index(&a.emoji), &a.emoji).cmp(&(picker_index(&b.emoji), &b.emoji)));
                (message_id, chips)
            })
            .collect::<HashMap<String, Vec<ReactionChip>>>()
    });

    view! {
        <Show
            when=move || !messages.get().is_empty()
            fallback=|| {
                view! {
                    <div class="emptyState">
                        <div class="emptyStateArt" aria-hidden="true">
                            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8"
                                stroke-linecap="round" stroke-linejoin="round">
                                <path d="M12 22v-9" />
                                <path d="M9.5 9.4c1.1.8 1.8 2.2 2.3 3.7-2 .4-3.5.4-4.8-.3-1.2-.6-2.3-1.9-3-4.2 2.8-.5 4.4 0 5.5.8z" />
                                <path d="M14.1 6a7 7 0 0 0-1.1 4c1.9-.1 3.3-.6 4.3-1.4 1-1 1.6-2.3 1.7-4.6-2.7.1-4 1-4.9 2z" />
                            </svg>
                        </div>
                        <div class="emptyStateTitle">"No messages yet"</div>
                        <div class="emptyStateHint">"Be the first to say hello — plant the seed."</div>
                    </div>
                }
            }
        >
            <For
                each=move || rows.get()
                key=|row: &RowCtx| {
                    // Grouping context is part of the key so a row re-renders when
                    // a neighbor changes its group shape (e.g. a follow-up arrives).
                    format!(
                        "{}|{}{}{}",
                        row.message.id().to_base64(),
                        row.first_in_group as u8,
                        row.last_in_group as u8,
                        row.day_label.is_some() as u8
                    )
                }
                children={
                    let users = users.clone();
                    let current_user_id = current_user_id.clone();
                    move |row: RowCtx| {
                        view! {
                            <MessageRow
                                message=row.message
                                users=users.clone()
                                current_user_id=current_user_id.clone()
                                editing_message=editing_message
                                replying_to=replying_to
                                first_in_group=row.first_in_group
                                last_in_group=row.last_in_group
                                day_label=row.day_label
                                reaction_chips=reaction_chips
                                mention_names=mention_names
                                link_previews=previews_by_url
                            />
                        }
                    }
                }
            />
        </Show>
    }
}
