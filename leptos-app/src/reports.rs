//! Reporting a message, and the queue the reports land in.
//!
//! FOR: the only path from "a member saw something wrong" to "a moderator
//! knows about it". Moderators read no direct messages at all and browse
//! nobody's history, so a room full of members is the only thing watching most
//! of what gets said here — and a member who can see abuse but not raise it
//! has to find a moderator by hand, which most people will simply not do.
//!
//! Two halves, deliberately in one module because they are two ends of one
//! row. [`report_message`] is the filing seam: it takes a message and files a
//! `Report` naming the caller, and it is shaped like `chat_hooks`'
//! `moderator_delete` so the chat crate's actions menu can be handed it in the
//! integration pass. [`ReportsView`] is the moderator end — the queue, inside
//! the moderation panel, where the same row is read and closed.
//!
//! WHAT A REPORT ROW DISCLOSES, AND TO WHOM. Only moderators read this
//! collection (`policy.json`'s `report` entry: the read scope is a comparison
//! no row can satisfy for everyone else), so a report about a message tells a
//! normal member nothing at all about that message — including nothing about
//! whether it was ever removed. Nothing in this module is rendered outside the
//! moderator gate, and nothing in it widens the message read scope, which goes
//! on being the only thing deciding whose messages a member may read.
//!
//! NO DEEP LINK TO THE REPORTED MESSAGE, mirroring the moderation log's own
//! restraint (`mod_log_panel`, module doc): an earlier revision of that panel
//! rendered a message id and planned deep-linking, and the ruling retracted it.
//! A report row carries the message ref for moderator tooling, exactly as a
//! `ModAction` row does, and this queue renders none of it.

use std::collections::HashMap;

use leptos::prelude::*;
use web_sys::window;

use ankurah::EntityId;
use ankurah_signals::Get as AnkurahGet;
use community_model::{MessageView, Report, ReportView};

use crate::{ctx, fmt};

/// File a report about `message`, as the signed-in member doing the reporting.
///
/// Shaped for the chat crate's actions menu — `(MessageView, Box<dyn Fn())` is
/// `ChatHooks::moderator_delete`'s signature, and `close` dismisses the menu
/// on every path out of here, including the ones that file nothing.
///
/// The reason is optional and the prompt doubles as the confirm: Cancel aborts
/// the report entirely, an empty OK files it with no reason, and a browser that
/// blocks the dialog files it with no reason rather than swallowing the
/// report — the same three-way the moderator delete and the ban prompt take.
///
/// NOTHING CALLS THIS YET, and that is the one thing to know about the report
/// feature on this branch. A member reaches it through an entry in the message
/// actions menu, and that menu belongs to the chat crate, which a parallel
/// branch is reworking — so the entry (and this call) land in that integration
/// pass. Everything on the other side of it is finished: the row, the policy,
/// the queue moderators read it in.
#[allow(dead_code, reason = "the chat crate's actions menu calls this in the integration pass")]
pub fn report_message(message: MessageView, close: Box<dyn Fn()>) {
    crate::terms::demand_terms(move || {
        let reason = match window().map(|w| w.prompt_with_message("Report this message — what is wrong with it? (optional):")) {
            Some(Ok(None)) => {
                close();
                return; // prompt cancelled — no report
            }
            Some(Ok(Some(text))) => {
                let text = text.trim().to_string();
                (!text.is_empty()).then_some(text)
            }
            _ => None,
        };

        wasm_bindgen_futures::spawn_local(async move {
            match file(&message, reason).await {
                Ok(_) => {
                    tracing::info!("Reported message {}", message.id().to_base64());
                    // The one acknowledgement available: the reporter cannot
                    // read the row back (the queue is moderator reading), so
                    // silence here would be indistinguishable from a report
                    // that never landed.
                    notify("Thanks — the moderators have your report.");
                }
                Err(e) => {
                    tracing::error!("Failed to report message {}: {}", message.id().to_base64(), e);
                    notify("That report could not be filed — try again in a moment.");
                }
            }
            close();
        });
    });
}

/// Create the row. `reporter` is the caller and can be nothing else — the
/// write scope refuses any other value — so a caller with no viewer is refused
/// here rather than filing something the server would reject anyway. The room
/// is copied off the message so the queue can name a place without resolving
/// every reported message.
async fn file(message: &MessageView, reason: Option<String>) -> Result<EntityId, Box<dyn std::error::Error>> {
    let reporter = crate::viewer().ok_or("no signed-in member to attribute this report to")?;
    let room = message.room()?.id();
    let trx = ctx().begin();
    let id = trx
        .create(&Report {
            reporter: reporter.into(),
            message: ankurah::Ref::from(message),
            room: room.into(),
            reason,
            created_at: js_sys::Date::now() as i64,
            resolved: false,
            resolved_by: None,
            resolved_at: None,
        })
        .await?
        .id();
    trx.commit().await?;
    Ok(id)
}

/// Say one sentence to the reader. A blocked dialog costs the acknowledgement
/// and nothing else — the report has already committed by the time this runs.
fn notify(message: &str) {
    if let Some(w) = window() {
        let _ = w.alert_with_message(message);
    }
}

/// The moderator queue: open reports first, newest first within each half.
///
/// Mounted only inside the moderation panel's moderator-only tab, so this
/// component does no role gating of its own — the same division the ban
/// controls keep, where `can_moderate()` decides at the call site and the
/// policy decides for real. `names_by_user` comes from the panel's own roster
/// query rather than a second one: the two lists name the same people.
#[component]
pub fn ReportsView(names_by_user: Memo<HashMap<String, String>>) -> impl IntoView {
    let chat = ankurah_chat_leptos::chat();

    // The whole queue, live. Small by nature (one row per report, ever) and
    // read only by moderators, so a full subscription is fine — the mod log's
    // reasoning, and revisit both together if either ever grows.
    //
    // A member's query would answer empty rather than `Err` (the collection
    // gate admits them through the write privilege, and the read scope then
    // composes the query to nothing), so an error here means something else
    // went wrong entirely. Degrading rather than unwrapping is the panel
    // idiom: the cost of being wrong about the gate must not be an app-wide
    // panic.
    let Ok(reports) = ctx().query::<ReportView>("true ORDER BY created_at DESC") else {
        tracing::error!("the report queue could not open its query");
        return view! { <div class="membersState">"The report queue is unavailable right now."</div> }.into_any();
    };
    let query_reg = crate::query_registry::register("reports (moderation panel)", &reports);
    on_cleanup(move || drop(query_reg));

    // Room names come from the chat handshake's rooms query, which owns that
    // subscription for the session — a second one here would be a second copy
    // of the same rows.
    let names_by_room = Memo::new(move |_| {
        chat.rooms()
            .map(|q| q.get().iter().map(|r| (r.id().to_base64(), r.name().unwrap_or_default())).collect())
            .unwrap_or_else(HashMap::<String, String>::new)
    });

    let reports_for_loading = reports.clone();
    let reports_for_empty = reports.clone();
    let reports_for_list = reports.clone();

    view! {
        <div class="membersList modLogList">
            <Show when=move || !reports_for_loading.loaded()>
                <div class="membersState">"Loading reports\u{2026}"</div>
            </Show>
            <Show when={
                let reports = reports_for_empty.clone();
                move || reports.loaded() && reports.get().is_empty()
            }>
                <div class="membersState">"No reports — nothing waiting."</div>
            </Show>
            <For
                each={
                    let reports = reports_for_list.clone();
                    move || {
                        let mut items = reports.get();
                        // Open reports first, then newest first within each
                        // half: the queue is a work list, and a resolved row is
                        // history. The query's ORDER BY does the second half,
                        // but resultset iteration order is not contractual and
                        // it cannot do the first at all.
                        items.sort_by_cached_key(|r| {
                            (r.resolved().unwrap_or(false), std::cmp::Reverse(r.created_at().unwrap_or(0)))
                        });
                        items
                    }
                }
                key=|report: &ReportView| report.id()
                children=move |report: ReportView| {
                    view! { <ReportRow report names_by_user names_by_room /> }
                }
            />
        </div>
    }
    .into_any()
}

/// One queue row: where it happened, who raised it, what they said, when — and
/// Resolve while it is still open.
///
/// The reported message is named by the row's `message` ref and rendered
/// nowhere, mirroring the moderation log (see the module doc).
#[component]
fn ReportRow(
    report: ReportView,
    names_by_user: Memo<HashMap<String, String>>,
    names_by_room: Memo<HashMap<String, String>>,
) -> impl IntoView {
    let reporter_id = report.reporter().ok().map(|r| r.id().to_base64());
    let hue = fmt::hue_class(reporter_id.as_deref().unwrap_or(""));

    let reporter_id_for_name = reporter_id.clone();
    let reporter_name = move || match &reporter_id_for_name {
        None => "Unknown".to_string(),
        Some(id) => names_by_user.with(|map| map.get(id).filter(|n| !n.trim().is_empty()).cloned().unwrap_or_else(|| "Unknown".to_string())),
    };
    let reporter_name_for_initials = reporter_name.clone();

    // A room the viewer's rooms query has not (yet) produced renders as "a
    // room" rather than as an id: an id names nothing to a reader, and the
    // report is still worth showing without it.
    let room_id = report.room().ok().map(|r| r.id().to_base64());
    let room_name = move || match &room_id {
        None => "a room".to_string(),
        Some(id) => names_by_room.with(|map| {
            map.get(id).filter(|n| !n.trim().is_empty()).map(|n| format!("#{n}")).unwrap_or_else(|| "a room".to_string())
        }),
    };

    let ts = report.created_at().unwrap_or(0);
    let when = format!("{} · {}", fmt::day_label(ts), fmt::clock_time(ts));
    let when_title = fmt::full_stamp(ts);
    let reason = report.reason().ok().flatten().filter(|r| !r.trim().is_empty());

    let report_for_resolved = report.clone();
    let resolved = move || report_for_resolved.resolved().unwrap_or(false);
    let resolved_for_button = resolved.clone();

    // Who closed it, for the resolved half of the list. Live like the reporter
    // name, so a row resolved while the panel is open names its moderator
    // without a reopen.
    let report_for_closer = report.clone();
    let closed_by = move || {
        let id = report_for_closer.resolved_by().ok().flatten()?.id().to_base64();
        Some(names_by_user.with(|map| map.get(&id).filter(|n| !n.trim().is_empty()).cloned().unwrap_or_else(|| "Unknown".to_string())))
    };

    let report_for_click = report.clone();

    view! {
        <div class=move || if resolved() { "modLogRow reportRow reportRowResolved" } else { "modLogRow reportRow" }>
            <div class=format!("memberAvatar {}", hue) aria-hidden="true">
                {move || fmt::initials(&reporter_name_for_initials())}
            </div>
            <div class="modLogBody">
                <div class="modLogLine">
                    <span class="modLogActor">{reporter_name}</span>
                    " "
                    <span class="modLogVerb">"reported a message in"</span>
                    " "
                    <span class="modLogActor">{room_name}</span>
                </div>
                {reason.map(|r| view! { <div class="modLogReason">{format!("“{}”", r)}</div> })}
                <div class="modLogWhen" title=when_title>{when}</div>
                {move || closed_by().map(|who| view! { <div class="reportResolvedBy">{format!("Resolved by {who}")}</div> })}
            </div>
            <Show when=move || !resolved_for_button()>
                {
                    let report = report_for_click.clone();
                    view! {
                        <button class="reportResolveBtn" on:click=move |_| resolve(report.clone())>
                            "Resolve"
                        </button>
                    }
                }
            </Show>
        </div>
    }
}

/// Close a report: flip `resolved` and stamp who closed it and when.
///
/// NO `ModAction` ROW, deliberately, and against the surrounding idiom —
/// ban, unban and message removal each write one (see `user_detail_panel` and
/// `chat_hooks`). Two reasons. Nothing happened to a message or to a member:
/// the acts a moderator takes because of a report are the removal or the ban,
/// and those write their own rows through their own paths, so lights-on
/// moderation stays complete without this one. And `modaction` is readable by
/// every signed-in member, so a row here would announce to the community that
/// somebody was reported — the single fact the `report` policy withholds from
/// them.
fn resolve(report: ReportView) {
    crate::terms::demand_terms(move || {
        wasm_bindgen_futures::spawn_local(async move {
            match (|| async {
                let actor = crate::viewer().ok_or("no signed-in moderator to attribute this resolution to")?;
                let trx = ctx().begin();
                let mutable = report.edit(&trx)?;
                mutable.resolved().set(&true)?;
                mutable.resolved_by().set(&Some(actor.into()))?;
                mutable.resolved_at().set(&Some(js_sys::Date::now() as i64))?;
                trx.commit().await?;
                Ok::<_, Box<dyn std::error::Error>>(())
            })()
            .await
            {
                Ok(_) => tracing::info!("Report resolved"),
                Err(e) => tracing::error!("Failed to resolve report: {}", e),
            }
        });
    });
}
