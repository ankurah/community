//! X-ray mode (ankurah/community#39): a public lens over the live Ankurah
//! machinery — event DAGs, head clocks, peer/sync state, and live query
//! traffic. v0 ships against today's published 0.9.0 APIs only; see
//! `community-artifacts/xray-design.md` for the staged plan.
//!
//! Architecture: a tiny always-mounted launcher pill (this module) toggles the
//! feature. All observation machinery (query taps, connection-state log,
//! event fetches) is created lazily on enable and dropped on disable — x-ray
//! costs nothing while off. Sibling modules:
//! - [`bus`]: app-side LiveQuery registry + bounded live event feed
//! - [`system_panel`]: the L2 slide-over (node / connection / queries cards)
//! - [`feed`]: the live changeset feed card
//! - [`inspector`]: the L1 per-entity drawer (event DAG)
//! - [`dag`]: topo-sort layout + SVG rendering
//! - [`decode`]: per-backend op summaries (yrs deltas, LWW byte sizes)

pub mod bus;
pub mod dag;
pub mod decode;
pub mod feed;
pub mod inspector;
pub mod system_panel;

use leptos::prelude::*;
use std::sync::OnceLock;

use ankurah::proto::{CollectionId, EntityId};

use inspector::XRayInspector;
use system_panel::SystemPanel;

/// What the L1 inspector drawer is pointed at.
#[derive(Clone, Debug, PartialEq)]
pub struct InspectTarget {
    pub collection: CollectionId,
    pub entity_id: EntityId,
}

/// Global x-ray UI state. Held in `ArcRwSignal`s (reference-counted, not
/// arena-allocated) so it can live in a `static` without a reactive owner and
/// be reached from anywhere — the same global-accessor style the app already
/// uses for `ctx()` / `ws_client()`.
///
/// Post-merge integration points read/write exactly these signals:
/// - header toggle: `xray::state().toggle()`
/// - message bubble head chips: `xray::state().enabled.get()`
/// - context-menu "Inspect": `xray::state().open_inspector(collection, id)`
#[derive(Clone)]
pub struct XRayState {
    /// Master switch. Persisted to `localStorage["xray"]`; `?xray=1` sets it on load.
    pub enabled: ArcRwSignal<bool>,
    /// Whether the L2 system panel is showing (independent of `enabled` so the
    /// panel can be closed while chips/taps stay live).
    pub panel_open: ArcRwSignal<bool>,
    /// Current L1 inspector target, if any.
    pub inspect: ArcRwSignal<Option<InspectTarget>>,
}

static STATE: OnceLock<XRayState> = OnceLock::new();

/// The global x-ray state (created on first use).
pub fn state() -> XRayState {
    STATE
        .get_or_init(|| XRayState {
            enabled: ArcRwSignal::new(false),
            panel_open: ArcRwSignal::new(false),
            inspect: ArcRwSignal::new(None),
        })
        .clone()
}

impl XRayState {
    /// Flip the master switch. Enabling starts the observation machinery
    /// (query taps + connection-state log) and opens the panel; disabling
    /// tears all of it down. Persists across reloads.
    pub fn set_enabled(&self, on: bool) {
        self.enabled.set(on);
        if on {
            bus::bus().set_tapping(true);
            bus::start_connection_log();
            self.panel_open.set(true);
        } else {
            self.panel_open.set(false);
            self.inspect.set(None);
            bus::bus().set_tapping(false);
            bus::stop_connection_log();
            bus::bus().clear_history();
        }
        persist_enabled(on);
    }

    /// The header lens / Alt+X master switch: a plain binary on↔off. Enabling
    /// opens the panel; disabling hides everything (chips, panel, inspector).
    /// This is deliberately NOT tri-state: the panel's own `×` hides just the
    /// panel (leaving ambient chips), so the master switch must always be a
    /// one-press OFF — otherwise closing the panel then toggling would reopen
    /// it and x-ray could never be switched off in a single action.
    pub fn toggle(&self) {
        self.set_enabled(!self.enabled.get_untracked());
    }

    /// Point the L1 drawer at an entity (enables x-ray if it wasn't on).
    pub fn open_inspector(&self, collection: CollectionId, entity_id: EntityId) {
        if !self.enabled.get_untracked() {
            self.set_enabled(true);
        }
        self.inspect.set(Some(InspectTarget { collection, entity_id }));
    }
}

const STORAGE_KEY: &str = "xray";

fn persist_enabled(on: bool) {
    if let Some(storage) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
        if on {
            let _ = storage.set_item(STORAGE_KEY, "1");
        } else {
            let _ = storage.remove_item(STORAGE_KEY);
        }
    }
}

/// `localStorage["xray"] == "1"` or a `?xray=1` URL param (demo deep links).
fn initially_enabled() -> bool {
    let Some(window) = web_sys::window() else { return false };
    if let Some(storage) = window.local_storage().ok().flatten()
        && storage.get_item(STORAGE_KEY).ok().flatten().as_deref() == Some("1")
    {
        return true;
    }
    window
        .location()
        .search()
        .ok()
        .and_then(|s| web_sys::UrlSearchParams::new_with_str(&s).ok())
        .and_then(|p| p.get("xray"))
        .map(|v| v == "1")
        .unwrap_or(false)
}

/// The x-ray host: restores persisted state, owns the Alt+X hotkey, and
/// mounts the system panel + inspector drawer. The visible toggle lives in
/// the header (integration pass); this component renders no chrome of its
/// own so signed-in users who never touch x-ray never see it.
#[component]
pub fn XRayLauncher() -> impl IntoView {
    let st = state();

    // Restore persisted / URL-requested state once at mount.
    if initially_enabled() && !st.enabled.get_untracked() {
        st.set_enabled(true);
    }

    // Alt+X toggles from anywhere (physical key, so macOS Alt-symbol input
    // doesn't swallow it). Registered once; the launcher lives as long as the
    // signed-in app does.
    let handle = window_event_listener(leptos::ev::keydown, move |ev| {
        if ev.alt_key() && !ev.repeat() && ev.code() == "KeyX" {
            ev.prevent_default();
            state().toggle();
        }
    });
    on_cleanup(move || handle.remove());

    let panel_open = st.panel_open.clone();
    let inspect = st.inspect.clone();

    view! {
        <Show when=move || panel_open.get()>
            <SystemPanel />
        </Show>

        {move || {
            inspect.get().map(|target| view! { <XRayInspector target /> })
        }}
    }
}
