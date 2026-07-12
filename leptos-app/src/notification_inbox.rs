//! Notification inbox (issue #19) and notification preferences (issue #25).
//!
//! The bell in the header carries a live unseen count; clicking it opens this
//! panel — an anchored popover hanging from the bell on wide viewports, the
//! members-panel modal shell on narrow ones (#55; one DOM tree, the split is
//! pure CSS via `.popoverSurface`) — listing the signed-in user's own
//! `Notification` rows newest-first (the notification policy scopes reads to
//! `recipient = $jwt.sub`, so the resultset is self-scoped server-side; the
//! client filters on recipient too, belt-and-braces). Clicking a row marks it
//! seen — the one notification write a client ever makes, permitted by the
//! same scope — and deep-links to the room it points at by setting the app's
//! room-selection signal. A sliders button toggles the preferences view:
//! `mentions_only` and per-room mutes on the user's single `NotificationPref`
//! row, which the server's fan-out worker reads when deciding what to create.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use leptos::prelude::*;

use ankurah::{property::Json, EntityId, LiveQuery};
use ankurah_signals::{Get as AnkurahGet, Peek};
use community_model::{NotificationPref, NotificationPrefView, NotificationView, RoomView, UserView};

use crate::{ctx, current_user_id, fmt, queries};

/// Unseen-notification count for the header bell. Renders nothing at zero —
/// the badge only exists when there is something to see.
#[component]
pub fn NotificationBadge() -> impl IntoView {
    let me = current_user_id();
    // Rows leave this resultset as they are marked seen, so the count is live
    // in both directions. The policy already scopes notification reads to the
    // recipient; the predicate restates it and the count re-checks per row,
    // belt-and-braces.
    let unseen = ctx()
        .query::<NotificationView>(
            queries::selection("recipient = ? AND seen = false", [(&me).into()]).expect("static unseen-notifications selection parses"),
        )
        .expect("failed to create NotificationView LiveQuery");

    // App-lifetime query (the header lives for the whole session), surfaced
    // in the X-ray queries card like `rooms (app)` — id discarded.
    crate::xray::bus::bus().register("notifications (bell)", &unseen);

    move || {
        let count = unseen.get().iter().filter(|n| is_mine(n, me) && !n.seen().unwrap_or(true)).count();
        (count > 0).then(|| {
            let label = if count > 99 { "99+".to_string() } else { count.to_string() };
            // aria-hidden: the button carries aria-label="Notifications"; the
            // badge is decoration to a screen reader, not the button's name.
            view! { <span class="notifBellBadge" aria-hidden="true">{label}</span> }
        })
    }
}

/// The inbox surface: an anchored popover under the header bell on wide
/// viewports, the full-screen overlay on narrow ones (#55). `selected_room`
/// is the app's room selection signal (threaded from ChatApp through Header)
/// so clicking a room-bearing notification can navigate the chat behind the
/// surface.
#[component]
pub fn NotificationInbox(
    selected_room: RwSignal<Option<RoomView>>,
    // `Send + Sync` (unlike the other panels' `on_close`) because this one is
    // captured inside `Show`/`For` children, which leptos stores as
    // thread-safe closures. The header's `move || panels().close()`
    // satisfies it.
    on_close: impl Fn() + Clone + Send + Sync + 'static,
) -> impl IntoView {
    let me = current_user_id();

    // The user's inbox, newest-first. Self-scoped by policy. Notification
    // rows are never deleted (`seen` is the lifecycle), so the LIMIT is what
    // keeps a long-lived account's panel-open cost flat — 200 covers weeks of
    // mentions; anyone scrolling past that wants search, not more inbox.
    let notifications = ctx()
        .query::<NotificationView>(
            queries::selection("recipient = ? ORDER BY created_at DESC LIMIT 200", [(&me).into()])
                .expect("static notifications selection parses"),
        )
        .expect("failed to create NotificationView LiveQuery");

    // The user's single preferences row (created on first change).
    let prefs = ctx()
        .query::<NotificationPrefView>(queries::selection("user = ?", [(&me).into()]).expect("static notificationpref selection parses"))
        .expect("failed to create NotificationPrefView LiveQuery");

    // Actor names via the users-map idiom (members panel / mod log), and room
    // names for the "in #room" fragment + the preferences mute list.
    let users = ctx().query::<UserView>("true").expect("failed to create UserView LiveQuery");
    let rooms = ctx().query::<RoomView>("true ORDER BY name ASC").expect("failed to create RoomView LiveQuery");

    // Surface the panel's own collections in the X-ray queries card while it
    // is open (transient registrations, dropped on close).
    let xray_regs = (
        crate::xray::bus::bus().register("notifications (inbox)", &notifications),
        crate::xray::bus::bus().register("notificationprefs (inbox)", &prefs),
    );
    on_cleanup(move || {
        let bus = crate::xray::bus::bus();
        bus.unregister(xray_regs.0);
        bus.unregister(xray_regs.1);
    });

    let names_by_user = Memo::new(move |_| {
        users.get().iter().map(|u| (u.id().to_base64(), u.display_name().unwrap_or_default())).collect::<HashMap<String, String>>()
    });
    let room_names = Memo::new({
        let rooms = rooms.clone();
        move |_| rooms.get().iter().map(|r| (r.id().to_base64(), r.name().unwrap_or_default())).collect::<HashMap<String, String>>()
    });

    // Inbox list vs preferences view, swapped by the sliders button.
    let show_prefs = RwSignal::new(false);

    let unseen_count = {
        let notifications = notifications.clone();
        move || notifications.get().iter().filter(|n| is_mine(n, me) && !n.seen().unwrap_or(true)).count()
    };

    // "Loading…" until the query settles, then the live unseen tally.
    let subtitle = {
        let notifications = notifications.clone();
        let unseen_count = unseen_count.clone();
        move || {
            if show_prefs.get() {
                "Delivered everywhere you're signed in.".to_string()
            } else if !notifications.loaded() {
                "Loading\u{2026}".to_string()
            } else {
                match unseen_count() {
                    0 => "You're all caught up.".to_string(),
                    1 => "1 unseen".to_string(),
                    n => format!("{} unseen", n),
                }
            }
        }
    };

    let unseen_count_for_button = unseen_count.clone();
    let notifications_for_loading = notifications.clone();
    let notifications_for_empty = notifications.clone();
    let notifications_for_list = notifications.clone();
    let notifications_for_mark_all = notifications.clone();
    let rooms_for_rows = rooms.clone();

    let on_close_overlay = on_close.clone();
    let on_close_button = on_close.clone();
    let on_close_rows = on_close.clone();

    view! {
        // .popoverSurface (#55) re-presents the modal shell as a popover
        // anchored to the bell on wide viewports — one DOM tree, the split is
        // entirely in CSS (MembersPanel.css). The backdrop click below only
        // exists on narrow viewports, where the overlay still covers the
        // screen; in popover mode this element shrink-wraps the panel and
        // outside-mousedown dismissal lives with the bell anchor (header.rs).
        <div class="membersOverlay popoverSurface" on:click=move |_| on_close_overlay()>
            <div
                class="membersContent notificationContent"
                role="dialog"
                aria-label="Notifications"
                on:click=|e| e.stop_propagation()
            >
                <div class="membersHeader">
                    <div class="membersTitles">
                        <h2>"Notifications"</h2>
                        <p class="membersSubtitle">{subtitle}</p>
                    </div>
                    {move || {
                        (!show_prefs.get() && unseen_count_for_button() > 0)
                            .then(|| {
                                let notifications = notifications_for_mark_all.clone();
                                view! {
                                    <button
                                        class="notifMarkAll"
                                        on:click=move |_| {
                                            mark_seen(
                                                notifications
                                                    .get()
                                                    .iter()
                                                    .filter(|n| is_mine(n, me) && !n.seen().unwrap_or(true))
                                                    .cloned()
                                                    .collect(),
                                            )
                                        }
                                    >
                                        "Mark all as seen"
                                    </button>
                                }
                            })
                    }}
                    <button
                        class="notifPrefsButton"
                        title="Notification preferences"
                        aria-label="Notification preferences"
                        aria-pressed=move || show_prefs.get().to_string()
                        on:click=move |_| show_prefs.update(|v| *v = !*v)
                    >
                        // Sliders — tune what reaches you.
                        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"
                            stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
                            <path d="M4 21v-7" />
                            <path d="M4 10V3" />
                            <path d="M12 21v-9" />
                            <path d="M12 8V3" />
                            <path d="M20 21v-5" />
                            <path d="M20 12V3" />
                            <path d="M1 14h6" />
                            <path d="M9 8h6" />
                            <path d="M17 16h6" />
                        </svg>
                    </button>
                    <button class="membersCloseButton" aria-label="Close" on:click=move |_| on_close_button()>
                        "×"
                    </button>
                </div>

                <Show when=move || !show_prefs.get()>
                    <div class="membersList notifList">
                        <Show when={
                            let q = notifications_for_loading.clone();
                            move || !q.loaded()
                        }>
                            <div class="membersState">"Loading notifications\u{2026}"</div>
                        </Show>
                        <Show when={
                            let q = notifications_for_empty.clone();
                            move || q.loaded() && !q.get().iter().any(|n| is_mine(n, me))
                        }>
                            <div class="membersState">"You're all caught up."</div>
                        </Show>
                        <For
                            each={
                                let q = notifications_for_list.clone();
                                move || {
                                    let mut items: Vec<NotificationView> =
                                        q.get().into_iter().filter(|n| is_mine(n, me)).collect();
                                    // Belt and braces: the query orders newest-first, but
                                    // resultset iteration order is not contractual.
                                    items.sort_by_cached_key(|n| std::cmp::Reverse(n.created_at().unwrap_or(0)));
                                    items
                                }
                            }
                            key=|n: &NotificationView| n.id()
                            children={
                                let rooms = rooms_for_rows.clone();
                                let on_close = on_close_rows.clone();
                                move |notification: NotificationView| {
                                    view! {
                                        <NotificationRow
                                            notification
                                            names_by_user
                                            room_names
                                            rooms=rooms.clone()
                                            selected_room
                                            on_close=on_close.clone()
                                        />
                                    }
                                }
                            }
                        />
                    </div>
                </Show>
                <Show when=move || show_prefs.get()>
                    <NotificationPrefs rooms=rooms.clone() prefs=prefs.clone() />
                </Show>

                <p class="membersNote">
                    {move || {
                        if show_prefs.get() {
                            "Preferences apply to new notifications, on every device you're signed in on."
                        } else {
                            "You're notified when someone mentions you."
                        }
                    }}
                </p>
            </div>
        </div>
    }
}

/// Belt-and-braces recipient check (the policy already scopes the resultset).
fn is_mine(n: &NotificationView, me: EntityId) -> bool { n.recipient().map(|r| r.id() == me).unwrap_or(false) }

/// One human sentence fragment per notification kind — future kinds slot in
/// here as the server's fan-out worker learns to create them.
fn kind_verb(kind: &str) -> String {
    match kind {
        "mention" => "mentioned you".to_string(),
        // Honest fallback for kinds this build predates.
        other => format!("sent you a \u{201c}{}\u{201d} notification", other),
    }
}

/// One inbox row: actor avatar + name, a human sentence for the kind, the
/// room it happened in, and when — with the unseen state as an amber dot
/// (seen rows dim instead). Clicking marks the row seen and deep-links to
/// its room.
#[component]
fn NotificationRow(
    notification: NotificationView,
    names_by_user: Memo<HashMap<String, String>>,
    room_names: Memo<HashMap<String, String>>,
    rooms: LiveQuery<RoomView>,
    selected_room: RwSignal<Option<RoomView>>,
    on_close: impl Fn() + Clone + Send + Sync + 'static,
) -> impl IntoView {
    let actor_id = notification.actor().ok().flatten().map(|a| a.id().to_base64());
    let hue = fmt::hue_class(actor_id.as_deref().unwrap_or(""));

    // Reactive: names update live as members rename themselves.
    let actor_id_for_name = actor_id.clone();
    let actor_name = move || {
        actor_id_for_name
            .as_ref()
            .and_then(|id| names_by_user.with(|map| map.get(id).filter(|n| !n.trim().is_empty()).cloned()))
            .unwrap_or_else(|| "Someone".to_string())
    };
    let actor_name_for_initials = actor_name.clone();

    let verb = kind_verb(&notification.kind().unwrap_or_default());

    // "in #room-name", resolved live from the rooms map; omitted entirely if
    // the notification carries no room.
    let room_fragment = notification.room().ok().flatten().map(|r| r.id().to_base64()).map(|id| {
        let name =
            move || room_names.with(|map| map.get(&id).filter(|n| !n.trim().is_empty()).cloned().unwrap_or_else(|| "a room".to_string()));
        view! {
            " "
            <span class="notifRoomName">{move || format!("in #{}", name())}</span>
        }
    });

    let ts = notification.created_at().unwrap_or(0);
    let when = format!("{} · {}", fmt::day_label(ts), fmt::clock_time(ts));
    let when_title = fmt::full_stamp(ts);

    // Reactive: the row restyles the moment `seen` flips — from this click,
    // "mark all", or another device.
    let notification_for_seen = notification.clone();
    let seen = move || notification_for_seen.seen().unwrap_or(true);
    let seen_for_class = seen.clone();
    let seen_for_dot = seen.clone();

    let notification_for_click = notification.clone();
    let handle_click = move |_| {
        // Mark seen first — even if there is nothing to navigate to.
        if !notification_for_click.seen().unwrap_or(false) {
            mark_seen(vec![notification_for_click.clone()]);
        }
        // Deep-link: select the room behind the overlay and close. Falls
        // through quietly (mark-seen only, panel stays open) if the room
        // isn't in the resultset.
        if let Ok(Some(room_ref)) = notification_for_click.room() {
            let room_eid = room_ref.id();
            if let Some(room) = rooms.peek().iter().find(|r| r.id() == room_eid).cloned() {
                selected_room.set(Some(room));
                on_close();
            }
        }
    };

    view! {
        <div
            class=move || if seen_for_class() { "notifRow notifRowSeen" } else { "notifRow" }
            on:click=handle_click
        >
            <div class=format!("memberAvatar {}", hue) aria-hidden="true">
                {move || fmt::initials(&actor_name_for_initials())}
            </div>
            <div class="notifBody">
                <div class="notifLine">
                    <span class="notifActor">{actor_name}</span>
                    " "
                    <span class="notifVerb">{verb}</span>
                    {room_fragment}
                </div>
                <div class="notifWhen" title=when_title>{when}</div>
            </div>
            {move || (!seen_for_dot()).then(|| view! { <span class="notifDot" aria-hidden="true"></span> })}
        </div>
    }
}

/// Flip `seen` on the given rows in one transaction. The notification write
/// scope (`recipient = $jwt.sub`) lets the recipient write their own rows —
/// this is the one notification write a client ever makes.
fn mark_seen(rows: Vec<NotificationView>) {
    if rows.is_empty() {
        return;
    }
    wasm_bindgen_futures::spawn_local(async move {
        match (|| async {
            let trx = ctx().begin();
            for row in &rows {
                row.edit(&trx)?.seen().set(&true)?;
            }
            trx.commit().await?;
            Ok::<_, Box<dyn std::error::Error>>(())
        })()
        .await
        {
            Ok(_) => {}
            Err(e) => tracing::error!("Failed to mark notification(s) seen: {}", e),
        }
    });
}

/// A preference change to apply to the user's `NotificationPref` row.
enum PrefChange {
    MentionsOnly(bool),
    /// (room id base64, muted?)
    Mute(String, bool),
}

/// The preferences view: a `mentions_only` toggle and per-room mutes. Both
/// live on the user's single `NotificationPref` row, created on first change;
/// `muted_rooms` is a JSON array of room id strings the server's fan-out
/// worker reads.
#[component]
fn NotificationPrefs(rooms: LiveQuery<RoomView>, prefs: LiveQuery<NotificationPrefView>) -> impl IntoView {
    let me = current_user_id();

    // The id of a row this client created, so a second change racing the
    // LiveQuery round-trip edits that row instead of creating a twin (the
    // read_state upsert idiom, sized down).
    let created_id = Arc::new(Mutex::new(None::<EntityId>));

    // The user's row. If duplicates ever race in from two devices, reads and
    // writes both pin the lowest id, so the twin is simply inert.
    let own_pref = {
        let prefs = prefs.clone();
        move || prefs.get().into_iter().filter(|p| p.user().map(|u| u.id() == me).unwrap_or(false)).min_by_key(|p| p.id().to_base64())
    };

    let own_pref_for_mentions = own_pref.clone();
    let mentions_only = move || own_pref_for_mentions().map(|p| p.mentions_only().unwrap_or(false)).unwrap_or(false);

    // muted_rooms as a set of room id strings (absent row → empty set).
    let muted_rooms = move || {
        own_pref()
            .and_then(|p| p.muted_rooms().ok())
            .map(|json| {
                json.as_array()
                    .map(|arr| arr.iter().filter_map(|v| v.as_str()).map(|s| s.to_string()).collect::<HashSet<String>>())
                    .unwrap_or_default()
            })
            .unwrap_or_default()
    };

    let prefs_for_mentions = prefs.clone();
    let created_for_mentions = created_id.clone();

    view! {
        <div class="membersList notifPrefs">
            <label class="notifPrefRow">
                <span class="notifPrefLabel">
                    "Only notify me when I'm mentioned"
                    <span class="notifPrefHint">"Skip everything except direct @mentions."</span>
                </span>
                <input
                    type="checkbox"
                    prop:checked=mentions_only
                    on:change=move |ev| {
                        update_pref(
                            prefs_for_mentions.clone(),
                            created_for_mentions.clone(),
                            PrefChange::MentionsOnly(event_target_checked(&ev)),
                        )
                    }
                />
            </label>

            <div class="notifPrefsSection">"Muted rooms"</div>
            // Honest scope note: the fan-out worker consults mutes when it
            // creates notification rows; the per-message sound chime is a
            // separate (wave-1) path that does not read prefs yet.
            <div class="notifPrefsSectionHint">"Stops inbox notifications from a room. Message sounds are not affected yet."</div>
            <For
                each={
                    let rooms = rooms.clone();
                    move || rooms.get()
                }
                key=|room: &RoomView| room.id()
                children={
                    let prefs = prefs.clone();
                    let created_id = created_id.clone();
                    let muted_rooms = muted_rooms.clone();
                    move |room: RoomView| {
                        let room_id = room.id().to_base64();
                        let name = room.name().unwrap_or_default();
                        let muted_rooms = muted_rooms.clone();
                        let room_id_for_checked = room_id.clone();
                        let checked = move || muted_rooms().contains(&room_id_for_checked);
                        let prefs = prefs.clone();
                        let created_id = created_id.clone();
                        view! {
                            <label class="notifPrefRow">
                                <span class="notifPrefLabel">
                                    <span>
                                        <span class="notifPrefRoomHash" aria-hidden="true">"# "</span>
                                        {name}
                                    </span>
                                </span>
                                <input
                                    type="checkbox"
                                    prop:checked=checked
                                    on:change=move |ev| {
                                        update_pref(
                                            prefs.clone(),
                                            created_id.clone(),
                                            PrefChange::Mute(room_id.clone(), event_target_checked(&ev)),
                                        )
                                    }
                                />
                            </label>
                        }
                    }
                }
            />
        </div>
    }
}

/// Queue one pref change for the strictly-serialized writer. Writes must not
/// overlap: two in-flight changes would both read the pre-write mute set and
/// the second whole-array commit would drop the first (lost update), and two
/// racing FIRST changes would each create a row (twins the UI then can't
/// repair — rows are not deletable). Pushes are synchronous and wasm is
/// single-threaded (tasks interleave only at awaits), so the queue + pump
/// flag below serialize without locks; the pump applies one change at a time
/// and each application peeks state AFTER its predecessor committed.
fn update_pref(prefs: LiveQuery<NotificationPrefView>, created_id: Arc<Mutex<Option<EntityId>>>, change: PrefChange) {
    PREF_QUEUE.with(|q| q.borrow_mut().push_back(change));
    if PREF_PUMP_RUNNING.with(|r| r.replace(true)) {
        return; // a pump is already draining; it will pick this change up
    }
    wasm_bindgen_futures::spawn_local(async move {
        while let Some(change) = PREF_QUEUE.with(|q| q.borrow_mut().pop_front()) {
            if let Err(e) = apply_pref_change(&prefs, &created_id, change).await {
                tracing::error!("Failed to update notification preferences: {}", e);
            }
        }
        // No await between the final pop and this reset, so a push cannot
        // slip into the gap and strand a change with no pump.
        PREF_PUMP_RUNNING.with(|r| r.set(false));
    });
}

thread_local! {
    static PREF_QUEUE: std::cell::RefCell<std::collections::VecDeque<PrefChange>> =
        std::cell::RefCell::new(std::collections::VecDeque::new());
    static PREF_PUMP_RUNNING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Apply one pref change: upsert the user's `NotificationPref` row. Prefers
/// the row from the LiveQuery (lowest id — the canonical row if twins exist),
/// then a row this client created that the LiveQuery hasn't delivered yet;
/// only creates when neither exists.
async fn apply_pref_change(
    prefs: &LiveQuery<NotificationPrefView>,
    created_id: &Arc<Mutex<Option<EntityId>>>,
    change: PrefChange,
) -> Result<(), Box<dyn std::error::Error>> {
    let me = current_user_id();
    let existing =
        prefs.peek().into_iter().filter(|p| p.user().map(|u| u.id() == me).unwrap_or(false)).min_by_key(|p| p.id().to_base64());
    let existing = match existing {
        Some(row) => Some(row),
        None => {
            let recorded = *created_id.lock().unwrap();
            match recorded {
                Some(id) => ctx().get::<NotificationPrefView>(id).await.ok(),
                None => None,
            }
        }
    };

    let trx = ctx().begin();
    match existing {
        Some(row) => match &change {
            PrefChange::MentionsOnly(v) => {
                row.edit(&trx)?.mentions_only().set(v)?;
            }
            PrefChange::Mute(room_id, muted) => {
                let mut set: HashSet<String> = row
                    .muted_rooms()
                    .ok()
                    .and_then(|json| {
                        json.as_array().map(|arr| arr.iter().filter_map(|v| v.as_str()).map(|s| s.to_string()).collect())
                    })
                    .unwrap_or_default();
                if *muted {
                    set.insert(room_id.clone());
                } else {
                    set.remove(room_id);
                }
                row.edit(&trx)?.muted_rooms().set(&muted_set_to_json(&set))?;
            }
        },
        None => {
            let (mentions_only, set) = match &change {
                PrefChange::MentionsOnly(v) => (*v, HashSet::new()),
                PrefChange::Mute(room_id, muted) => {
                    let mut set = HashSet::new();
                    if *muted {
                        set.insert(room_id.clone());
                    }
                    (false, set)
                }
            };
            let created = trx.create(&NotificationPref { user: me.into(), mentions_only, muted_rooms: muted_set_to_json(&set) }).await?;
            *created_id.lock().unwrap() = Some(created.id());
        }
    }
    trx.commit().await?;
    Ok(())
}

/// Stable JSON encoding for `muted_rooms`: a sorted array of room id strings.
fn muted_set_to_json(set: &HashSet<String>) -> Json {
    let mut ids: Vec<&String> = set.iter().collect();
    ids.sort();
    Json::new(serde_json::Value::Array(ids.into_iter().map(|s| serde_json::Value::String(s.clone())).collect()))
}
