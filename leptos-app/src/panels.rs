//! Panel manager (#58): one open surface at a time.
//!
//! Every header surface (members, mod log, notification inbox, QR code) used
//! to be its own independent `show_*` signal — nothing enforced exclusivity,
//! and overlays could stack. This module owns the one `Option<Surface>` that
//! replaces them: opening any surface closes the current one by construction,
//! and Escape closes the open surface from a single window-level listener in
//! the header (the app-wide Escape gap flagged in the wave-2 review).
//!
//! The x-ray system panel deliberately lives OUTSIDE this system: it is an
//! inspection overlay meant to coexist with any surface (see `xray`).
//!
//! Held in an `ArcRwSignal` behind a `static` — the same global-accessor
//! style as `ctx()` / `xray::state()` — so deep components (member rows,
//! mention chips) can open surfaces without prop-drilling.

use ankurah::proto::EntityId;
use leptos::prelude::*;
use std::sync::OnceLock;

/// What a header surface renders when it cannot open: its own frame, its own
/// ×, and a line saying so where its list would be.
///
/// FOR: three of these surfaces read collections `policy.json` keys on the
/// `signed_in` privilege — the member roster, the role cache and the
/// moderation log — and a guest session is refused all three at the collection
/// gate, synchronously, when the query is created. The header does not offer
/// those buttons to a guest, so nothing should reach this; what it buys is the
/// cost of being wrong about that. Before, each panel unwrapped its query with
/// `.expect`, and one panel mounted under the wrong session took the whole
/// wasm app down with it. Now it costs the reader one panel, still closable.
#[component]
pub fn PanelUnavailable(
    /// The heading the surface would have carried, so the reader can see which
    /// one refused to open.
    title: &'static str,
    /// One sentence about why there is nothing here.
    note: &'static str,
    /// Extra classes on the content box, so a surface keeps its own
    /// presentation (the inbox is a popover on wide viewports, not a modal).
    #[prop(default = "")]
    content_class: &'static str,
    on_close: impl Fn() + Clone + 'static,
) -> impl IntoView {
    let on_close_overlay = on_close.clone();
    view! {
        <div class="membersOverlay" on:click=move |_| on_close_overlay()>
            <div class=format!("membersContent {content_class}") on:click=|e| e.stop_propagation()>
                <div class="membersHeader">
                    <div class="membersTitles">
                        <h2>{title}</h2>
                    </div>
                    <button class="membersCloseButton" aria-label="Close" on:click=move |_| on_close()>
                        "×"
                    </button>
                </div>
                <div class="membersList">
                    <div class="membersState">{note}</div>
                </div>
            </div>
        </div>
    }
}

/// The exclusive header surfaces. At most one is open at a time.
#[derive(Clone, Debug, PartialEq)]
pub enum Surface {
    Members,
    ModLog,
    Inbox,
    Qr,
    /// Member detail sidebar (#57), reachable from members rows, the profile
    /// popover, and mention chips — anywhere a user is on screen.
    UserDetail(EntityId),
}

/// Owner of the single open-surface slot.
#[derive(Clone)]
pub struct PanelManager {
    open: ArcRwSignal<Option<Surface>>,
}

static STATE: OnceLock<PanelManager> = OnceLock::new();

/// The global panel manager (created on first use).
pub fn panels() -> PanelManager {
    STATE.get_or_init(|| PanelManager { open: ArcRwSignal::new(None) }).clone()
}

impl PanelManager {
    /// Open a surface, closing whatever else was open — the exclusivity is
    /// this assignment.
    pub fn open(&self, surface: Surface) {
        self.open.set(Some(surface));
    }

    /// Close the open surface (no-op when none is).
    pub fn close(&self) {
        self.open.set(None);
    }

    /// Header-button behavior: the button of the open surface closes it,
    /// any other button switches to its own surface.
    pub fn toggle(&self, surface: Surface) {
        if self.open.get_untracked() == Some(surface.clone()) {
            self.close();
        } else {
            self.open(surface);
        }
    }

    /// The open surface, reactively (drives the header's render match).
    pub fn current(&self) -> Option<Surface> {
        self.open.get()
    }

    /// The open surface, without subscribing (event-handler reads).
    pub fn current_untracked(&self) -> Option<Surface> {
        self.open.get_untracked()
    }

    /// Whether `surface` is the open one, reactively (button aria-pressed).
    pub fn is_open(&self, surface: &Surface) -> bool {
        self.current().as_ref() == Some(surface)
    }
}
