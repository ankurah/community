use leptos::html::Textarea;
use leptos::prelude::*;
use std::collections::HashMap;
use web_sys::KeyboardEvent;

use ankurah_signals::{Get as AnkurahGet, Peek as AnkurahPeek};
use community_model::{Message, MessageView, RoomView, UserView};

use crate::{ctx, fmt, ws_client};

/// Cap on the auto-grown composer height (#50) — roughly eight lines of text;
/// beyond it the textarea scrolls internally instead of eating the timeline.
const MAX_COMPOSER_HEIGHT: i32 = 192;

/// Fit the composer textarea to its content, up to [`MAX_COMPOSER_HEIGHT`].
/// Collapse-to-auto first so shrinking works (scrollHeight never shrinks
/// below the styled height on its own).
fn autosize(el: &web_sys::HtmlTextAreaElement) {
    // Fully qualified: leptos' `ElementExt::style` extension shadows the
    // web_sys inherent getter in this scope.
    let style = web_sys::HtmlElement::style(el);
    let _ = style.set_property("height", "auto");
    // scrollHeight covers content + padding; the element is border-box, so
    // add the border (offset − client) to avoid a 2px internal scroll.
    let border = el.offset_height() - el.client_height();
    let content = el.scroll_height() + border;
    let clamped = content.min(MAX_COMPOSER_HEIGHT);
    let _ = style.set_property("height", &format!("{clamped}px"));
    let _ = style.set_property("overflow-y", if content > MAX_COMPOSER_HEIGHT { "auto" } else { "hidden" });
}

/// At most this many candidates in the mention popup (#18).
const MENTION_POPUP_MAX: usize = 8;

/// How far back from the caret we scan for the `@` of a mention draft.
const MENTION_SCAN_MAX: usize = 48;

/// An in-progress `@mention` being typed (#18): the utf16 index of the `@`
/// and the query text between it and the caret.
#[derive(Clone, PartialEq)]
struct MentionDraft {
    start_utf16: usize,
    query: String,
}

/// Find the mention being typed at the caret, if any: an `@` at a word start
/// (start-of-text or after whitespace) with no whitespace between it and the
/// caret. All indices are utf16 code units — the DOM's currency — so emoji
/// and other astral text before the `@` cannot skew the math; conversion to
/// Rust strings happens per-slice via `from_utf16_lossy`.
fn current_mention_draft(el: &web_sys::HtmlTextAreaElement) -> Option<MentionDraft> {
    let caret = el.selection_start().ok().flatten()? as usize;
    let units: Vec<u16> = el.value().encode_utf16().collect();
    let caret = caret.min(units.len());
    let mut i = caret;
    while i > 0 && caret - i < MENTION_SCAN_MAX {
        let unit = units[i - 1];
        // Lone surrogate halves (pieces of emoji) are ordinary non-whitespace.
        if let Some(c) = char::from_u32(unit as u32) {
            if c.is_whitespace() {
                return None;
            }
            if c == '@' {
                let at_word_start = i == 1 || char::from_u32(units[i - 2] as u32).map(|p| p.is_whitespace()).unwrap_or(false);
                if !at_word_start {
                    return None; // e.g. the @ of an email address
                }
                return Some(MentionDraft { start_utf16: i - 1, query: String::from_utf16_lossy(&units[i..caret]) });
            }
        }
        i -= 1;
    }
    None
}

/// At most this many candidates in the emoji popup (#54).
const EMOJI_POPUP_MAX: usize = 8;

/// How far back from the caret we scan for the `:` of a shortcode draft —
/// generous headroom over the longest table name.
const EMOJI_SCAN_MAX: usize = 32;

/// An in-progress `:shortcode` being typed (#54): the utf16 index of the
/// opening `:` and the query text between it and the caret.
#[derive(Clone, PartialEq)]
struct EmojiDraft {
    start_utf16: usize,
    query: String,
}

/// Characters a shortcode run may contain. Uppercase is tolerated while
/// typing (matching lowercases); `+`/`-` serve `:+1:`/`:-1:`.
fn is_shortcode_char(c: char) -> bool { c.is_ascii_alphanumeric() || matches!(c, '_' | '+' | '-') }

/// Find the emoji shortcode being typed at the caret, if any: a `:` at a
/// word start (start-of-text or after whitespace — so clock times like
/// "12:30" and pasted URLs stay quiet) with 2+ shortcode chars between it
/// and the caret (so a lone `:` or a `:)` smiley stays quiet too). Indices
/// are utf16 code units, like the mention draft.
fn current_emoji_draft(el: &web_sys::HtmlTextAreaElement) -> Option<EmojiDraft> {
    let caret = el.selection_start().ok().flatten()? as usize;
    let units: Vec<u16> = el.value().encode_utf16().collect();
    let caret = caret.min(units.len());
    let mut i = caret;
    while i > 0 && caret - i < EMOJI_SCAN_MAX {
        // Lone surrogate halves (pieces of emoji) fail the shortcode-char
        // test below, exactly as they should.
        let c = char::from_u32(units[i - 1] as u32)?;
        if c == ':' {
            let at_word_start = i == 1 || char::from_u32(units[i - 2] as u32).map(|p| p.is_whitespace()).unwrap_or(false);
            if !at_word_start {
                return None;
            }
            let query = String::from_utf16_lossy(&units[i..caret]);
            return (query.len() >= 2).then_some(EmojiDraft { start_utf16: i - 1, query });
        }
        if !is_shortcode_char(c) {
            return None;
        }
        i -= 1;
    }
    None
}

/// A completed `:name:` run ending exactly at `caret`: returns the utf16
/// index of the opening `:` and the name between the colons. Same word-start
/// rule as the draft scanner; an empty name (`::`) never matches.
fn completed_shortcode(units: &[u16], caret: usize) -> Option<(usize, String)> {
    if caret < 3 || caret > units.len() || units[caret - 1] != u16::from(b':') {
        return None;
    }
    let mut i = caret - 1;
    while i > 0 && caret - i < EMOJI_SCAN_MAX {
        let c = char::from_u32(units[i - 1] as u32)?;
        if c == ':' {
            let at_word_start = i == 1 || char::from_u32(units[i - 2] as u32).map(|p| p.is_whitespace()).unwrap_or(false);
            if i == caret - 1 || !at_word_start {
                return None;
            }
            return Some((i - 1, String::from_utf16_lossy(&units[i..caret - 1])));
        }
        if !is_shortcode_char(c) {
            return None;
        }
        i -= 1;
    }
    None
}

/// Rank users for the mention popup: display-name prefix matches first, then
/// substring matches, alphabetically within each tier; at most
/// [`MENTION_POPUP_MAX`]. An empty query (bare `@`) lists everyone.
fn mention_candidates(users: &[UserView], query: &str) -> Vec<UserView> {
    let q = query.to_lowercase();
    let mut ranked: Vec<(bool, String, UserView)> = users
        .iter()
        .filter_map(|u| {
            let name = u.display_name().unwrap_or_default();
            if name.is_empty() {
                return None;
            }
            let lower = name.to_lowercase();
            if lower.starts_with(&q) {
                Some((false, lower, u.clone()))
            } else if lower.contains(&q) {
                Some((true, lower, u.clone()))
            } else {
                None
            }
        })
        .collect();
    ranked.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
    ranked.truncate(MENTION_POPUP_MAX);
    ranked.into_iter().map(|(_, _, u)| u).collect()
}

/// Message input component for sending and editing messages.
/// The composer is a multiline textarea (#50): Enter sends, Shift+Enter
/// inserts a newline, Escape cancels an edit (or an armed reply), and
/// Cmd/Ctrl+Up/Down navigates the viewer's own messages for editing. Typing
/// `@` opens the mention autocomplete (#18). While a reply is armed (#23) a
/// "Replying to …" chip sits above the input; sending attaches the referenced
/// message as `re`.
///
/// The draft holds plain `@DisplayName` text, never raw tokens (#56): the
/// autocomplete inserts the name, send re-encodes matching `@Name` runs to
/// canonical `<@id>` tokens (mention_display::MemberDirectory), and the edit
/// mirror decodes stored tokens back for the textarea. The wire format is
/// untouched — the server scanner sees exactly what it always saw.
#[component]
pub fn MessageInput(
    room: RoomView,
    current_user: Option<UserView>,
    editing_message: RwSignal<Option<MessageView>>,
    /// The message the next send replies to (#23), armed by the context
    /// menu's Reply. Independent of the draft text: arming, canceling, or
    /// sending a reply never rewrites what the user has typed.
    replying_to: RwSignal<Option<MessageView>>,
    /// Current visible messages (oldest-first), used for Cmd/Ctrl+Up/Down navigation.
    #[prop(into)] messages: Signal<Vec<MessageView>>,
) -> impl IntoView {
    let message_input = RwSignal::new(String::new());
    let textarea_ref = NodeRef::<Textarea>::new();

    // Live connection state from the WebSocket client (reactive via the observer bridge).
    let connection_status = move || ws_client().connection_state().get().to_string();
    let is_connected = move || connection_status() == "Connected";
    let can_send = move || !message_input.get().trim().is_empty() && is_connected();

    // Mention autocomplete (#18): a composer-local users LiveQuery (reading
    // users is world-readable; the list component holds its own copy) plus
    // the draft being typed. Candidates derive from both, so the popup
    // tracks the users collection live.
    let mention_users = ctx().query::<UserView>("true").expect("failed to create UserView LiveQuery");
    let mention_draft = RwSignal::new(None::<MentionDraft>);
    let mention_selected = RwSignal::new(0usize);
    let mention_matches = Signal::derive({
        let mention_users = mention_users.clone();
        move || match mention_draft.get() {
            Some(draft) => mention_candidates(&mention_users.get(), &draft.query),
            None => Vec::new(),
        }
    });

    // Emoji autocomplete (#54): the same draft/selection/matches trio as
    // mentions, over the static shortcode table.
    let emoji_draft = RwSignal::new(None::<EmojiDraft>);
    let emoji_selected = RwSignal::new(0usize);
    let emoji_matches = Signal::derive(move || match emoji_draft.get() {
        Some(draft) => crate::emoji::candidates(&draft.query, EMOJI_POPUP_MAX),
        None => Vec::new(),
    });

    // Which member an autocompleted name meant (#56): name → id, recorded at
    // pick time so an ambiguous display name re-encodes to the member the
    // user actually chose. Session-scoped; a name never picked falls back to
    // the directory's deterministic choice.
    let mention_picks = StoredValue::new(HashMap::<String, String>::new());

    // Snapshot of the member list for coding (#56). Untracked on purpose:
    // send/mirror moments want the list as of NOW, and must not re-run when
    // users change.
    let directory = {
        let users = mention_users.clone();
        move || {
            community_model::mention_display::MemberDirectory::new(
                users.peek().iter().map(|u| (u.id().to_base64(), u.display_name().unwrap_or_default())),
            )
        }
    };

    // id → display-name map for the reply chip (#23): the author line and
    // token resolution in the snippet. Rebuilt live (renames included) from
    // the same users query the mention popup holds.
    let member_names = Memo::new({
        let users = mention_users.clone();
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

    // Re-derive both drafts from the caret. Cheap; called on input and on
    // caret-moving keys/clicks so an anchor can never go stale silently.
    // The two can never be Some at once: a mention draft tolerates no
    // whitespace back to its `@`, an emoji draft no non-shortcode char back
    // to its `:`, and each trigger disqualifies the other's scan.
    let refresh_drafts = move || {
        let Some(el) = textarea_ref.get_untracked() else { return };
        let next = current_mention_draft(&el);
        if next != mention_draft.get_untracked() {
            mention_draft.set(next);
            mention_selected.set(0);
        }
        let next = current_emoji_draft(&el);
        if next != emoji_draft.get_untracked() {
            emoji_draft.set(next);
            emoji_selected.set(0);
        }
    };

    // Splice `replacement` over utf16 units [start, end) of the textarea and
    // pin the caret after it. Write the DOM first (value + caret), then the
    // signal: the render effect re-assigns .value with the identical string,
    // and some engines reset the caret to the end on any assignment, so it
    // is re-pinned a frame later for mid-text insertions.
    let splice_units = move |el: &web_sys::HtmlTextAreaElement, start: usize, end: usize, replacement: &str| {
        let units: Vec<u16> = el.value().encode_utf16().collect();
        let end = end.min(units.len());
        let rep: Vec<u16> = replacement.encode_utf16().collect();
        let mut next: Vec<u16> = Vec::with_capacity(units.len() - (end - start) + rep.len());
        next.extend_from_slice(&units[..start]);
        next.extend_from_slice(&rep);
        next.extend_from_slice(&units[end..]);
        let new_caret = (start + rep.len()) as u32;
        let new_value = String::from_utf16_lossy(&next);
        el.set_value(&new_value);
        let _ = el.set_selection_range(new_caret, new_caret);
        message_input.set(new_value);
        autosize(el);
        let _ = el.focus();
        request_animation_frame(move || {
            if let Some(el) = textarea_ref.get_untracked() {
                let _ = el.set_selection_range(new_caret, new_caret);
            }
        });
    };

    // Replace the draft (`@que`) with plain `@DisplayName ` — the draft
    // shows names, never tokens (#56); send re-encodes to the canonical
    // `<@BASE64_ID>` wire format. The pick is recorded so a shared display
    // name still re-encodes to the member chosen here.
    let insert_mention = move |user: &UserView| {
        let Some(el) = textarea_ref.get_untracked() else { return };
        let Some(draft) = mention_draft.get_untracked() else { return };
        let units: Vec<u16> = el.value().encode_utf16().collect();
        let caret = el.selection_start().ok().flatten().map(|c| c as usize).unwrap_or(units.len()).min(units.len());
        let start = draft.start_utf16;
        if start >= caret || units.get(start) != Some(&u16::from(b'@')) {
            mention_draft.set(None); // stale anchor: the text changed under us
            return;
        }
        let name = user.display_name().unwrap_or_default();
        if name.is_empty() {
            return; // candidates are name-filtered; belt for a race
        }
        mention_picks.update_value(|picks| {
            picks.insert(name.clone(), user.id().to_base64());
        });
        splice_units(&el, start, caret, &format!("@{name} "));
        mention_draft.set(None);
    };

    // Replace the draft (`:que`) with the chosen unicode glyph — input-time
    // replacement only; what is stored is the plain emoji (#54 contract).
    let insert_emoji = move |glyph: &str| {
        let Some(el) = textarea_ref.get_untracked() else { return };
        let Some(draft) = emoji_draft.get_untracked() else { return };
        let units: Vec<u16> = el.value().encode_utf16().collect();
        let caret = el.selection_start().ok().flatten().map(|c| c as usize).unwrap_or(units.len()).min(units.len());
        let start = draft.start_utf16;
        if start >= caret || units.get(start) != Some(&u16::from(b':')) {
            emoji_draft.set(None); // stale anchor: the text changed under us
            return;
        }
        splice_units(&el, start, caret, glyph);
        emoji_draft.set(None);
    };

    // Inline completion (#54): a fully typed `:name:` becomes its glyph the
    // moment the closing colon lands — no popup interaction required. IME
    // composition updates are exempt (composing text must never be spliced
    // mid-flight); names outside the table stay as typed, per the contract.
    let complete_typed_shortcode = move |ev: &leptos::ev::Event| {
        use wasm_bindgen::JsCast;
        if ev.dyn_ref::<web_sys::InputEvent>().map(|e| e.is_composing()).unwrap_or(false) {
            return;
        }
        let Some(el) = textarea_ref.get_untracked() else { return };
        let Some(caret) = el.selection_start().ok().flatten().map(|c| c as usize) else { return };
        let units: Vec<u16> = el.value().encode_utf16().collect();
        let Some((start, name)) = completed_shortcode(&units, caret.min(units.len())) else { return };
        let Some(glyph) = crate::emoji::lookup(&name) else { return };
        splice_units(&el, start, caret, glyph);
        emoji_draft.set(None);
    };

    // Edit-session snapshot (#56): the decoded editor text and the member set
    // the decode ran against, captured when an edit enters the composer. The
    // save's no-op check and re-encode use THIS snapshot — decode's lossless
    // guard (encode(decode(x)) == x) only holds within one directory, so a
    // member joining, leaving, or renaming while the editor is open could
    // otherwise retarget or destroy a mention on a save the user never
    // touched.
    let edit_snapshot = StoredValue::new(None::<(String, Vec<(String, String)>)>);

    // Mirror the edit target into the composer. `prev` carries the previous
    // run's editing id, so only actual transitions rewrite the draft (signal
    // re-notifications must never clobber typing). Entering an edit also
    // disarms a pending reply (#23): send() would EDIT, not create, so a
    // lingering chip would promise a `re` that never attaches. The reverse
    // (Reply canceling an edit) is handled at the Reply action itself.
    Effect::new({
        let users = mention_users.clone();
        move |prev: Option<Option<String>>| {
            let editing = editing_message.get();
            let editing_id = editing.as_ref().map(|m| m.id().to_base64());
            if prev.map(|p| p != editing_id).unwrap_or(true) {
                // Programmatic fill invalidates any draft anchor.
                mention_draft.set(None);
                emoji_draft.set(None);
                match &editing {
                    Some(edit_msg) => {
                        replying_to.set(None);
                        // Stored tokens decode to `@names` for the textarea
                        // (#56); tokens that can't decode safely stay raw
                        // (see MemberDirectory::decode). Snapshot the result
                        // and the members it was computed against for the
                        // save side (see `edit_snapshot`).
                        let members: Vec<(String, String)> =
                            users.peek().iter().map(|u| (u.id().to_base64(), u.display_name().unwrap_or_default())).collect();
                        let dir = community_model::mention_display::MemberDirectory::new(members.iter().cloned());
                        let decoded = dir.decode(&edit_msg.text().unwrap_or_default());
                        message_input.set(decoded.clone());
                        edit_snapshot.set_value(Some((decoded, members)));
                    }
                    None => {
                        edit_snapshot.set_value(None);
                        message_input.set(String::new());
                    }
                }
            }
            editing_id
        }
    });

    // Arming a reply (#23) focuses the composer — the chip itself is chrome,
    // and the user's next act is typing.
    Effect::new(move |_| {
        if replying_to.get().is_some() {
            if let Some(el) = textarea_ref.get_untracked() {
                let _ = el.focus();
            }
        }
    });

    // Refit the textarea after every programmatic content change (edit-mirror
    // fill, clear-after-send). Effects run after the render effect has pushed
    // the new value into the DOM, so scrollHeight is current. Typing is
    // covered separately by the on:input handler for zero-lag growth.
    Effect::new(move |_| {
        let _ = message_input.get();
        if let Some(el) = textarea_ref.get_untracked() {
            autosize(&el);
        }
    });

    let send = {
        let current_user = current_user.clone();
        let room = room.clone();
        let mention_users = mention_users.clone();
        move || {
            mention_draft.set(None);
            emoji_draft.set(None);
            let input_text = message_input.get();
            if input_text.trim().is_empty() {
                return;
            }
            let Some(user) = current_user.clone() else {
                tracing::info!("Cannot send: no user");
                return;
            };

            if let Some(edit_msg) = editing_message.get() {
                // Edit existing message via a CRDT text replace. The editor
                // held display text; the wire gets the re-encoded form (#56),
                // built against the EDIT-ENTRY snapshot — the same directory
                // that produced the editor text (see `edit_snapshot`), so a
                // membership change mid-edit can't shift what a save means.
                let input_text = input_text.trim().to_string();
                let (entry_decoded, members) = edit_snapshot.get_value().unwrap_or_else(|| {
                    // Defensive only: the snapshot is written by the same
                    // effect that filled the editor. Absent, the current
                    // members are the best approximation of what it shows.
                    let members: Vec<(String, String)> =
                        mention_users.peek().iter().map(|u| (u.id().to_base64(), u.display_name().unwrap_or_default())).collect();
                    let dir = community_model::mention_display::MemberDirectory::new(members.iter().cloned());
                    (dir.decode(&edit_msg.text().unwrap_or_default()), members)
                });
                let picks = mention_picks.get_value();
                wasm_bindgen_futures::spawn_local(async move {
                    let result = async {
                        let stored = edit_msg.text().unwrap_or_default();
                        // No-op edits commit nothing (and earn no "(edited)"
                        // marker). "Unchanged" means the editor still shows
                        // exactly what the edit-entry decode produced —
                        // comparing wire forms would stamp a phantom edit
                        // whenever decode had fallen back to raw tokens, and
                        // a fresh decode would judge against a directory the
                        // user never saw.
                        if entry_decoded == input_text {
                            return Ok(());
                        }
                        let dir = community_model::mention_display::MemberDirectory::new(members.into_iter());
                        let wire_text = dir.encode(&input_text, &picks);
                        if wire_text == stored {
                            return Ok(()); // byte-identical outcome (e.g. a re-typed mention)
                        }
                        let trx = ctx().begin();
                        let mutable = edit_msg.edit(&trx)?;
                        mutable.text().replace(&wire_text)?;
                        // Stamp the edit for the "(edited)" indicator (#11).
                        mutable.edited_at().set(&Some(js_sys::Date::now() as i64))?;
                        trx.commit().await?;
                        Ok::<_, Box<dyn std::error::Error>>(())
                    }
                    .await;
                    match result {
                        Ok(_) => {
                            editing_message.set(None);
                            message_input.set(String::new());
                        }
                        Err(e) => tracing::error!("Failed to update message: {}", e),
                    }
                });
            } else {
                // Create a new message. ankurah stores user/room as typed Refs;
                // an armed reply (#23) rides along as `re`. The wire text is
                // the display draft with `@Name` runs re-encoded to canonical
                // tokens (#56) — what the server's mention scanner fans out.
                let user_ref = ankurah::Ref::from(&user);
                let room_ref = ankurah::Ref::from(&room);
                let input_text = input_text.trim().to_string();
                let wire_text = directory().encode(&input_text, &mention_picks.get_value());
                let reply_to = replying_to.get_untracked();
                let re = reply_to.as_ref().map(ankurah::Ref::from);
                // Clear synchronously: clearing only in the async completion
                // left a window where a second Enter re-sent the same text.
                // The reply chip clears with it — this send owns it now.
                message_input.set(String::new());
                replying_to.set(None);
                wasm_bindgen_futures::spawn_local(async move {
                    let result = async {
                        let trx = ctx().begin();
                        let timestamp = js_sys::Date::now() as i64;
                        trx.create(&Message {
                            user: user_ref,
                            room: room_ref,
                            text: wire_text,
                            timestamp,
                            deleted: false,
                            edited_at: None,
                            collaborative: None,
                            re,
                        })
                        .await?;
                        trx.commit().await?;
                        Ok::<_, Box<dyn std::error::Error>>(())
                    }
                    .await;
                    if let Err(e) = result {
                        tracing::error!("Failed to send message: {}", e);
                        // Put the failed text back — above anything typed since,
                        // never over it — and re-arm the reply unless a new one
                        // was chosen meanwhile, or an edit began (the chip and
                        // an edit are mutually exclusive; resurrecting it under
                        // an open editor would promise a `re` on the NEXT new
                        // message instead).
                        message_input.update(|current| {
                            if current.trim().is_empty() {
                                *current = input_text;
                            } else {
                                *current = format!("{input_text}\n{current}");
                            }
                        });
                        if replying_to.get_untracked().is_none() && editing_message.get_untracked().is_none() {
                            replying_to.set(reply_to);
                        }
                    }
                });
            }
        }
    };

    // Select the previous/next message authored by the current user (for editing).
    let navigate_own = {
        let current_user = current_user.clone();
        move |backward: bool| {
            let Some(user) = current_user.clone() else { return };
            let user_id = user.id().to_base64();
            let msgs = messages.get_untracked();
            if msgs.is_empty() {
                return;
            }
            // Tombstones are not editable — skip them while navigating (#10).
            let is_own = |m: &MessageView| {
                m.user().ok().map(|r| r.id().to_base64()).as_deref() == Some(user_id.as_str())
                    && !m.deleted().unwrap_or(false)
            };

            let current_idx = editing_message
                .get()
                .and_then(|em| {
                    let id = em.id().to_base64();
                    msgs.iter().position(|m| m.id().to_base64() == id)
                });

            if backward {
                // Cmd/Ctrl+Up: search toward older messages (lower indices).
                let start = current_idx.unwrap_or(msgs.len());
                for i in (0..start).rev() {
                    if is_own(&msgs[i]) {
                        editing_message.set(Some(msgs[i].clone()));
                        return;
                    }
                }
            } else if let Some(start) = current_idx {
                // Cmd/Ctrl+Down: only meaningful while editing; search toward newer messages.
                for i in (start + 1)..msgs.len() {
                    if is_own(&msgs[i]) {
                        editing_message.set(Some(msgs[i].clone()));
                        return;
                    }
                }
                // Past the newest own message: exit edit mode.
                editing_message.set(None);
                message_input.set(String::new());
            }
        }
    };

    let handle_key_down = {
        let send = send.clone();
        move |e: KeyboardEvent| {
            // While the mention popup is open (#18) it captures its keys —
            // but never modifier'd ones, so Cmd/Ctrl+Up edit-nav still works.
            let matches = mention_matches.get_untracked();
            if !matches.is_empty() && !e.meta_key() && !e.ctrl_key() && !e.alt_key() {
                match e.key().as_str() {
                    "ArrowDown" => {
                        e.prevent_default();
                        mention_selected.update(|i| *i = (*i + 1) % matches.len());
                        return;
                    }
                    "ArrowUp" => {
                        e.prevent_default();
                        mention_selected.update(|i| *i = (*i + matches.len() - 1) % matches.len());
                        return;
                    }
                    "Enter" | "Tab" if !e.shift_key() => {
                        // keyCode 229: WebKit fires the composition-commit
                        // keydown AFTER compositionend with isComposing=false
                        // — without this check Safari IME users select a
                        // candidate and we treat it as popup confirmation.
                        if !e.is_composing() && e.key_code() != 229 {
                            e.prevent_default();
                            let idx = mention_selected.get_untracked().min(matches.len() - 1);
                            insert_mention(&matches[idx]);
                        }
                        return;
                    }
                    "Escape" => {
                        // Closes only the popup; a second Escape cancels the edit.
                        e.prevent_default();
                        mention_draft.set(None);
                        return;
                    }
                    _ => {}
                }
            }
            // The emoji popup (#54) captures the same keys while open, with
            // the same IME guards. Never open at the same time as the mention
            // popup (the drafts are mutually exclusive by construction).
            let ematches = emoji_matches.get_untracked();
            if !ematches.is_empty() && !e.meta_key() && !e.ctrl_key() && !e.alt_key() {
                match e.key().as_str() {
                    "ArrowDown" => {
                        e.prevent_default();
                        emoji_selected.update(|i| *i = (*i + 1) % ematches.len());
                        return;
                    }
                    "ArrowUp" => {
                        e.prevent_default();
                        emoji_selected.update(|i| *i = (*i + ematches.len() - 1) % ematches.len());
                        return;
                    }
                    "Enter" | "Tab" if !e.shift_key() => {
                        // Same WebKit guard as the mention popup: the
                        // composition-commit keydown (keyCode 229) must not
                        // read as popup confirmation.
                        if !e.is_composing() && e.key_code() != 229 {
                            e.prevent_default();
                            let idx = emoji_selected.get_untracked().min(ematches.len() - 1);
                            insert_emoji(ematches[idx].1);
                        }
                        return;
                    }
                    "Escape" => {
                        // Consumed: the window-level Escape (panel manager)
                        // skips defaultPrevented events, so only the popup
                        // closes.
                        e.prevent_default();
                        emoji_draft.set(None);
                        return;
                    }
                    _ => {}
                }
            }
            // Enter sends; Shift+Enter falls through to the textarea's native
            // newline (#50). An Enter that confirms an IME composition must
            // not send: isComposing covers Chrome/Firefox, and keyCode 229
            // covers WebKit, which fires the commit keydown after
            // compositionend with isComposing already false. repeat() drops
            // key-autorepeat so holding Enter sends once, not once per repeat.
            if e.key() == "Enter" && !e.shift_key() && !e.is_composing() && e.key_code() != 229 && !e.repeat() {
                e.prevent_default();
                send();
            } else if e.key() == "Escape" && editing_message.get().is_some() {
                e.prevent_default();
                editing_message.set(None);
                message_input.set(String::new());
            } else if e.key() == "Escape" && replying_to.get().is_some() {
                // Cancel the armed reply (#23); the draft text is untouched.
                // preventDefault keeps the window-level Escape (panel manager)
                // from also acting on this press.
                e.prevent_default();
                replying_to.set(None);
            } else if e.key() == "ArrowUp" && (e.meta_key() || e.ctrl_key()) {
                e.prevent_default();
                navigate_own(true);
            } else if e.key() == "ArrowDown" && (e.meta_key() || e.ctrl_key()) && editing_message.get().is_some() {
                e.prevent_default();
                navigate_own(false);
            }
        }
    };

    let send_click = send.clone();
    view! {
        <div class="inputContainer">
            // Mention autocomplete popup (#18): floats above the composer.
            // mousedown is prevented throughout so the textarea keeps focus
            // (its blur handler would otherwise close the popup pre-click).
            <Show when=move || !mention_matches.get().is_empty()>
                <div
                    class="mentionPopup"
                    role="listbox"
                    aria-label="Mention a member"
                    on:mousedown=|e: leptos::ev::MouseEvent| e.prevent_default()
                >
                    {move || {
                        mention_matches
                            .get()
                            .into_iter()
                            .enumerate()
                            .map(|(i, user)| {
                                let name = user
                                    .display_name()
                                    .unwrap_or_default();
                                let initials = fmt::initials(&name);
                                let hue = fmt::hue_class(&user.id().to_base64());
                                view! {
                                    <button
                                        type="button"
                                        class=move || {
                                            if mention_selected.get() == i {
                                                "mentionItem active"
                                            } else {
                                                "mentionItem"
                                            }
                                        }
                                        role="option"
                                        aria-selected=move || {
                                            if mention_selected.get() == i { "true" } else { "false" }
                                        }
                                        on:mouseenter=move |_| mention_selected.set(i)
                                        on:click=move |_| insert_mention(&user)
                                    >
                                        <span class=format!("mentionAvatar {hue}") aria-hidden="true">
                                            {initials}
                                        </span>
                                        <span class="mentionName">{name.clone()}</span>
                                    </button>
                                }
                            })
                            .collect_view()
                    }}
                </div>
            </Show>
            // Emoji autocomplete popup (#54): same shell and interaction
            // contract as the mention popup, over the shortcode table.
            <Show when=move || !emoji_matches.get().is_empty()>
                <div
                    class="mentionPopup"
                    role="listbox"
                    aria-label="Insert an emoji"
                    on:mousedown=|e: leptos::ev::MouseEvent| e.prevent_default()
                >
                    {move || {
                        emoji_matches
                            .get()
                            .into_iter()
                            .enumerate()
                            .map(|(i, (name, glyph))| {
                                view! {
                                    <button
                                        type="button"
                                        class=move || {
                                            if emoji_selected.get() == i {
                                                "mentionItem active"
                                            } else {
                                                "mentionItem"
                                            }
                                        }
                                        role="option"
                                        aria-selected=move || {
                                            if emoji_selected.get() == i { "true" } else { "false" }
                                        }
                                        on:mouseenter=move |_| emoji_selected.set(i)
                                        on:click=move |_| insert_emoji(glyph)
                                    >
                                        <span class="emojiGlyph" aria-hidden="true">{glyph}</span>
                                        <span class="mentionName">{format!(":{name}:")}</span>
                                    </button>
                                }
                            })
                            .collect_view()
                    }}
                </div>
            </Show>
            // Reply chip (#23): compact "Replying to …" state above the input.
            // Live reads: a rename, edit, or delete of the original while the
            // chip is up re-renders it (a deleted original still sends — `re`
            // points at the tombstone, which the preview renders honestly).
            <Show when=move || replying_to.get().is_some()>
                {move || {
                    replying_to
                        .get()
                        .map(|orig| {
                            let author_id = orig.user().map(|r| r.id().to_base64()).unwrap_or_default();
                            let author = member_names
                                .with(|names| names.get(&author_id).cloned())
                                .filter(|n| !n.is_empty())
                                .unwrap_or_else(|| "Unknown".to_string());
                            let snippet = if orig.deleted().unwrap_or(false) {
                                "Removed message".to_string()
                            } else {
                                member_names
                                    .with(|names| crate::mentions::reply_snippet(&orig.text().unwrap_or_default(), names))
                            };
                            view! {
                                <div class="replyingNotice">
                                    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"
                                        stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
                                        <polyline points="9 14 4 9 9 4" />
                                        <path d="M20 20v-7a4 4 0 0 0-4-4H4" />
                                    </svg>
                                    <span class="replyingNoticeLabel">"Replying to " {author}</span>
                                    <span class="replyingNoticeSnippet">{snippet}</span>
                                    <button
                                        class="replyingNoticeCancel"
                                        aria-label="Cancel reply"
                                        title="Cancel reply"
                                        on:click=move |_| replying_to.set(None)
                                    >
                                        "×"
                                    </button>
                                </div>
                            }
                        })
                }}
            </Show>
            <Show when=move || editing_message.get().is_some()>
                <div class="editingNotice">
                    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"
                        stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
                        <path d="M17 3a2.8 2.8 0 1 1 4 4L7.5 20.5 2 22l1.5-5.5z" />
                    </svg>
                    <span>"Editing message"</span>
                    <span class="editingNoticeHint">
                        <kbd>"Esc"</kbd>
                        " to cancel"
                    </span>
                </div>
            </Show>
            <div class="inputRow">
                // Multiline composer (#50). Keeps class="input" + the same
                // placeholder: e2e locates it by `.input[placeholder=...]`.
                <textarea
                    node_ref=textarea_ref
                    class="input"
                    placeholder="Type a message..."
                    rows="1"
                    aria-label="Message"
                    prop:value=move || message_input.get()
                    on:input=move |ev| {
                        message_input.set(event_target_value(&ev));
                        if let Some(el) = textarea_ref.get_untracked() {
                            autosize(&el);
                        }
                        // Order matters: a just-completed `:name:` splices
                        // first (#54); the drafts re-derive from the result.
                        complete_typed_shortcode(&ev);
                        refresh_drafts();
                    }
                    on:keydown=handle_key_down
                    // Caret moves without input events (#18): arrows/Home/End
                    // keyup and mouse clicks re-derive the drafts.
                    on:keyup=move |e: KeyboardEvent| {
                        if matches!(
                            e.key().as_str(),
                            "ArrowLeft" | "ArrowRight" | "ArrowUp" | "ArrowDown" | "Home" | "End"
                        ) {
                            refresh_drafts();
                        }
                    }
                    on:click=move |_| refresh_drafts()
                    on:blur=move |_| {
                        mention_draft.set(None);
                        emoji_draft.set(None);
                    }
                    prop:disabled=move || !is_connected()
                ></textarea>
                <Show when=move || editing_message.get().is_some()>
                    <button
                        class="button buttonGhost"
                        on:click=move |_| {
                            editing_message.set(None);
                            message_input.set(String::new());
                        }
                    >
                        "Cancel"
                    </button>
                </Show>
                <button class="button sendButton" on:click=move |_| send_click() prop:disabled=move || !can_send()>
                    {move || if editing_message.get().is_some() { "Update" } else { "Send" }}
                </button>
            </div>
        </div>
    }
}
