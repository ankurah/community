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

use leptos::prelude::*;
use std::sync::OnceLock;

/// The exclusive header surfaces. At most one is open at a time.
#[derive(Clone, Debug, PartialEq)]
pub enum Surface {
    Members,
    ModLog,
    Inbox,
    Qr,
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
