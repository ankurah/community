//! Emoji reactions (#14): the chip bar under message bubbles, the toggle
//! write path, and the fixed reaction set offered by pickers.
//!
//! Query shape: ONE `active = true` LiveQuery for the whole message list
//! (built in `message_list.rs`), grouped client-side into per-message chips.
//! `Reaction` carries no room ref in the wave-1 model, so a room-scoped
//! predicate is inexpressible; the alternative — a LiveQuery per row — would
//! churn subscriptions constantly as the virtual scroller mounts and unmounts
//! rows. One standing subscription with an O(rows) regroup on change is the
//! cheaper steady state at community scale. Revisit if Reaction ever gains a
//! room ref.

use leptos::prelude::*;

use community_model::{MessageView, Reaction, ReactionView};

/// The fixed reaction set (#14 deliberately ships a small picker, not a
/// full emoji keyboard).
pub const REACTION_EMOJIS: [&str; 6] = ["\u{1F44D}", "\u{2764}\u{FE0F}", "\u{1F602}", "\u{1F389}", "\u{1F615}", "\u{1F440}"];

/// Stable chip ordering: picker order first, then anything else (from older
/// clients or future pickers) lexicographically.
pub fn picker_index(emoji: &str) -> usize {
    REACTION_EMOJIS.iter().position(|e| *e == emoji).unwrap_or(REACTION_EMOJIS.len())
}

/// One rendered chip: an emoji, how many distinct users reacted with it, and
/// whether the viewer is among them.
#[derive(Clone, PartialEq)]
pub struct ReactionChip {
    pub emoji: String,
    pub count: usize,
    pub mine: bool,
}

/// Toggle the viewer's reaction. Reaction rows are never deleted (ankurah
/// 0.9.0 has no entity deletion): the first toggle creates {active: true},
/// later toggles flip `active`. A one-shot fetch finds prior rows — including
/// inactive ones the live `active = true` query no longer carries. Duplicate
/// rows (concurrent first-toggles) are tolerated: all matching rows flip to
/// the opposite of "any active", and the chip grouping counts distinct users.
pub fn toggle_reaction(message: &MessageView, emoji: &str) {
    let message = message.clone();
    let emoji = emoji.to_string();
    wasm_bindgen_futures::spawn_local(async move {
        let result = async {
            let ctx = crate::ctx();
            let me = crate::current_user_id();
            let selection = crate::queries::selection(
                "message = ? AND user = ? AND emoji = ?",
                [(&message.id()).into(), (&me).into(), emoji.as_str().into()],
            )?;
            let existing = ctx.fetch::<ReactionView>(selection).await?;

            let trx = ctx.begin();
            if existing.is_empty() {
                trx.create(&Reaction {
                    message: ankurah::Ref::from(&message),
                    user: me.into(),
                    emoji: emoji.clone(),
                    active: true,
                })
                .await?;
            } else {
                let any_active = existing.iter().any(|r| r.active().unwrap_or(false));
                for row in &existing {
                    row.edit(&trx)?.active().set(&!any_active)?;
                }
            }
            trx.commit().await?;
            Ok::<_, Box<dyn std::error::Error>>(())
        }
        .await;
        if let Err(e) = result {
            tracing::error!("Failed to toggle reaction: {}", e);
        }
    });
}

/// The chip row under a bubble. Renders nothing of its own accord when
/// `chips` is empty — the caller already gates on that, but keep it safe.
#[component]
pub fn ReactionBar(message: MessageView, #[prop(into)] chips: Signal<Vec<ReactionChip>>) -> impl IntoView {
    view! {
        <div class="reactionBar">
            {move || {
                let message = message.clone();
                chips
                    .get()
                    .into_iter()
                    .map(move |chip| {
                        let emoji = chip.emoji.clone();
                        let message = message.clone();
                        let noun = if chip.count == 1 { "reaction" } else { "reactions" };
                        let hint = if chip.mine { "Click to remove yours" } else { "Click to react" };
                        let label = format!("{} {} {}. {}.", chip.count, chip.emoji, noun, hint);
                        view! {
                            <button
                                type="button"
                                class=if chip.mine { "reactionChip mine" } else { "reactionChip" }
                                aria-pressed=if chip.mine { "true" } else { "false" }
                                aria-label=label
                                on:click=move |_| toggle_reaction(&message, &emoji)
                            >
                                <span class="reactionEmoji" aria-hidden="true">{chip.emoji.clone()}</span>
                                <span class="reactionCount">{chip.count}</span>
                            </button>
                        }
                    })
                    .collect_view()
            }}
        </div>
    }
}
