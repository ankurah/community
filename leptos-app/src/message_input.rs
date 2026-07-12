use leptos::html::Textarea;
use leptos::prelude::*;
use web_sys::KeyboardEvent;

use ankurah_signals::Get as AnkurahGet;
use community_model::{Message, MessageView, RoomView, UserView};

use crate::{ctx, fmt, ws_client};

/// Cap on the auto-grown composer height (#50) — roughly eight lines of text;
/// beyond it the textarea scrolls internally instead of eating the timeline.
const MAX_COMPOSER_HEIGHT: i32 = 192;

/// Reply quotes are clipped to this many characters (#23).
const REPLY_SNIPPET_MAX: usize = 140;

/// Pending composer prefill (#23 reply). Plain shared state, deliberately NOT
/// a lazily-created leptos signal: a signal first created inside a context-menu
/// event handler would be registered under that menu's reactive Owner and
/// disposed when the menu closes. The requester instead pokes the composer by
/// re-setting `editing_message` (an unconditional notify), whose effect
/// consumes this. A module global (not a prop) because the composer is
/// instantiated in chat.rs, which wave-2 message work does not touch.
static COMPOSE_PREFILL: std::sync::RwLock<Option<String>> = std::sync::RwLock::new(None);

fn take_compose_prefill() -> Option<String> { COMPOSE_PREFILL.write().ok().and_then(|mut slot| slot.take()) }

/// Start a reply to `author`'s message (#23, v1 text convention): prefill the
/// composer with an editable quoted snippet — Slack-style — rather than a
/// durable reference. A `re: Ref<Message>` model field is a fast-follow; no
/// Message field stores the referenced id today.
///
/// Cancels any in-progress edit: a reply composes a NEW message, and the
/// `editing_message` write doubles as the composer's wake-up call.
pub fn request_reply_prefill(author: &str, text: &str, editing_message: RwSignal<Option<MessageView>>) {
    // Single-line snippet: newlines and runs of whitespace collapse, so the
    // quote stays one `>` line even when quoting multiline/code messages.
    let mut snippet: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if snippet.chars().count() > REPLY_SNIPPET_MAX {
        snippet = snippet.chars().take(REPLY_SNIPPET_MAX).collect::<String>().trim_end().to_string() + "\u{2026}";
    }
    let prefill = format!("> **{author}**: {snippet}\n\n");
    if let Ok(mut slot) = COMPOSE_PREFILL.write() {
        *slot = Some(prefill);
    }
    editing_message.set(None);
}

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
/// inserts a newline, Escape cancels an edit, and Cmd/Ctrl+Up/Down navigates
/// the viewer's own messages for editing. Typing `@` opens the mention
/// autocomplete (#18).
#[component]
pub fn MessageInput(
    room: RoomView,
    current_user: Option<UserView>,
    editing_message: RwSignal<Option<MessageView>>,
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

    // Re-derive the draft from the caret. Cheap; called on input and on
    // caret-moving keys/clicks so the anchor can never go stale silently.
    let refresh_mention_draft = move || {
        let Some(el) = textarea_ref.get_untracked() else { return };
        let next = current_mention_draft(&el);
        if next != mention_draft.get_untracked() {
            mention_draft.set(next);
            mention_selected.set(0);
        }
    };

    // Replace the draft (`@que`) with the canonical token `<@BASE64_ID> ` —
    // the exact format community_model::parse_mentions (and the server's
    // notification fan-out) recognizes. Splicing is done in utf16 space.
    let insert_mention = move |user: &UserView| {
        let Some(el) = textarea_ref.get_untracked() else { return };
        let Some(draft) = mention_draft.get_untracked() else { return };
        let units: Vec<u16> = el.value().encode_utf16().collect();
        let caret = el.selection_start().ok().flatten().map(|c| c as usize).unwrap_or(units.len()).min(units.len());
        let start = draft.start_utf16;
        if start >= caret || units.get(start) != Some(&(b'@' as u16)) {
            mention_draft.set(None); // stale anchor: the text changed under us
            return;
        }
        let token: Vec<u16> = format!("<@{}> ", user.id().to_base64()).encode_utf16().collect();
        let mut next: Vec<u16> = Vec::with_capacity(units.len() - (caret - start) + token.len());
        next.extend_from_slice(&units[..start]);
        next.extend_from_slice(&token);
        next.extend_from_slice(&units[caret..]);
        let new_caret = (start + token.len()) as u32;
        let new_value = String::from_utf16_lossy(&next);
        // Write the DOM first (value + caret), then the signal. The render
        // effect re-assigns .value with the identical string; some engines
        // reset the caret to the end on any assignment, so re-pin it a
        // frame later for mid-text insertions.
        el.set_value(&new_value);
        let _ = el.set_selection_range(new_caret, new_caret);
        message_input.set(new_value);
        autosize(&el);
        mention_draft.set(None);
        let _ = el.focus();
        request_animation_frame(move || {
            if let Some(el) = textarea_ref.get_untracked() {
                let _ = el.set_selection_range(new_caret, new_caret);
            }
        });
    };

    // One effect owns every externally-driven composer fill — the edit mirror
    // and the reply prefill (#23) — so the two sources cannot clobber each
    // other across effect runs. `prev` carries the editing-message id from
    // the previous run: the mirror only fires on an actual edit transition,
    // and the prefill branch returns the current id so the transition that
    // delivered it (reply cancels any edit) is not re-mirrored into a clear.
    Effect::new(move |prev: Option<Option<String>>| {
        let editing = editing_message.get();
        let editing_id = editing.as_ref().map(|m| m.id().to_base64());

        if let Some(text) = take_compose_prefill() {
            mention_draft.set(None);
            message_input.set(text);
            if let Some(el) = textarea_ref.get_untracked() {
                let _ = el.focus();
            }
            return editing_id;
        }

        if prev.map(|p| p != editing_id).unwrap_or(true) {
            mention_draft.set(None); // programmatic fill invalidates any draft anchor
            match &editing {
                Some(edit_msg) => message_input.set(edit_msg.text().unwrap_or_default()),
                None => message_input.set(String::new()),
            }
        }
        editing_id
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
        move || {
            mention_draft.set(None);
            let input_text = message_input.get();
            if input_text.trim().is_empty() {
                return;
            }
            let Some(user) = current_user.clone() else {
                tracing::info!("Cannot send: no user");
                return;
            };

            if let Some(edit_msg) = editing_message.get() {
                // Edit existing message via a CRDT text replace.
                let input_text = input_text.trim().to_string();
                wasm_bindgen_futures::spawn_local(async move {
                    let result = async {
                        // No-op edits commit nothing (and earn no "(edited)" marker).
                        if edit_msg.text().unwrap_or_default() == input_text {
                            return Ok(());
                        }
                        let trx = ctx().begin();
                        let mutable = edit_msg.edit(&trx)?;
                        mutable.text().replace(&input_text)?;
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
                // Create a new message. ankurah stores user/room as typed Refs.
                let user_ref = ankurah::Ref::from(&user);
                let room_ref = ankurah::Ref::from(&room);
                let input_text = input_text.trim().to_string();
                wasm_bindgen_futures::spawn_local(async move {
                    let result = async {
                        let trx = ctx().begin();
                        let timestamp = js_sys::Date::now() as i64;
                        trx.create(&Message {
                            user: user_ref,
                            room: room_ref,
                            text: input_text,
                            timestamp,
                            deleted: false,
                            edited_at: None,
                            collaborative: None,
                        })
                        .await?;
                        trx.commit().await?;
                        Ok::<_, Box<dyn std::error::Error>>(())
                    }
                    .await;
                    match result {
                        Ok(_) => message_input.set(String::new()),
                        Err(e) => tracing::error!("Failed to send message: {}", e),
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
                        if !e.is_composing() {
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
            // Enter sends; Shift+Enter falls through to the textarea's native
            // newline (#50). An Enter that confirms an IME composition
            // (isComposing) must not send the message.
            if e.key() == "Enter" && !e.shift_key() && !e.is_composing() {
                e.prevent_default();
                send();
            } else if e.key() == "Escape" && editing_message.get().is_some() {
                e.prevent_default();
                editing_message.set(None);
                message_input.set(String::new());
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
                        refresh_mention_draft();
                    }
                    on:keydown=handle_key_down
                    // Caret moves without input events (#18): arrows/Home/End
                    // keyup and mouse clicks re-derive the mention draft.
                    on:keyup=move |e: KeyboardEvent| {
                        if matches!(
                            e.key().as_str(),
                            "ArrowLeft" | "ArrowRight" | "ArrowUp" | "ArrowDown" | "Home" | "End"
                        ) {
                            refresh_mention_draft();
                        }
                    }
                    on:click=move |_| refresh_mention_draft()
                    on:blur=move |_| mention_draft.set(None)
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
