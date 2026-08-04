//! X-ray: a live inspector panel for Ankurah applications — event DAGs, head
//! clocks, peer/sync state, and live query traffic, drawn over a running app
//! while the app stays usable.
//!
//! This is Ankurah Community's in-app x-ray mode (ankurah/community#39) lifted
//! into its own crate, which is the "separate application/library" step of the
//! trajectory ankurah/community#53 records. The destination that issue names is
//! an inspector any ankurah app can mount with **two accommodations**: a
//! `data-entity-id` attribute on x-rayable elements, and loading the panel.
//! Today a third thing is required — an [`XRayHost`] implementation — because
//! ankurah 0.9.0 cannot enumerate a node's queries, materialize an entity's
//! values untyped, or report storage stats. Every hack that gap forces is
//! annotated in place with the upstream issue that deletes it.
//!
//! ## Mounting
//!
//! ```ignore
//! ankurah_xray::XRay::new(MyHost { ctx })
//!     .connection(ws_client.connection_state(), ws_url())
//!     .install()
//!     .expect("install ankurah-xray once");
//!
//! view! { <XRayLauncher /> }
//! ```
//!
//! The launcher renders no chrome of its own — put a toggle wherever the app
//! wants one and call [`state()`]`.toggle()`, or press Alt+X.
//!
//! ## Architecture
//!
//! A tiny always-mounted launcher (this module) toggles the feature and owns
//! the observation machinery's lifetime: query taps, the connection-state log
//! and event fetches are created on enable and dropped on disable *or* on the
//! launcher's own unmount, so an app that never turns x-ray on never pays for
//! it, and one that turns it on does not keep paying after the panel is gone.
//! (A query the app registered with [`bus`] is the one thing that outlives
//! that — the registry holds it until the app unregisters it.) Sibling
//! modules:
//! - [`host`]: the application boundary, and the ledger of missing upstream API
//! - [`bus`]: app-side LiveQuery registry + bounded live event feed
//! - [`system_panel`]: the L2 slide-over (node / connection / queries cards)
//! - [`feed`]: the live changeset feed card
//! - [`inspector`]: the L1 per-entity drawer (event DAG)
//! - [`dag`]: topo-sort layout + SVG rendering
//! - [`decode`]: per-backend op summaries (yrs deltas, LWW byte sizes)

pub mod bus;
pub mod dag;
pub mod decode;
mod env;
pub mod feed;
pub mod host;
pub mod inspector;
pub mod system_panel;

use leptos::prelude::*;
use std::sync::OnceLock;

use ankurah::proto::{CollectionId, EntityId};

use inspector::XRayInspector;
use system_panel::SystemPanel;

pub use host::{InstallError, NodeStatus, Resolved, XRay, XRayHost, is_installed, lww_provenance};

/// What the L1 inspector drawer is pointed at.
#[derive(Clone, Debug, PartialEq)]
pub struct InspectTarget {
    pub collection: CollectionId,
    pub entity_id: EntityId,
}

/// Global x-ray UI state. Held in `ArcRwSignal`s (reference-counted, not
/// arena-allocated) so it can live in a `static` without a reactive owner and
/// be reached from anywhere — including from the app's own components, which is
/// how a header toggle and per-element inspect affordances work.
///
/// Integration points read/write exactly these signals:
/// - a toggle button: `xray::state().toggle()`
/// - x-rayable elements: `xray::state().enabled.get()`
/// - an "Inspect" affordance: `xray::state().open_inspector(collection, id)`
#[derive(Clone)]
pub struct XRayState {
    /// Master switch. Persisted to `localStorage["xray"]`; `?xray=1` sets it on load.
    /// X-ray is ONE mode: on shows everything (panel, chips, inspector
    /// affordances), off shows nothing. A dismissable-panel half-state was
    /// tried and read as "x-ray is stuck on" — every close affordance now
    /// flips this one switch.
    pub enabled: ArcRwSignal<bool>,
    /// Current L1 inspector target, if any.
    pub inspect: ArcRwSignal<Option<InspectTarget>>,
}

static STATE: OnceLock<XRayState> = OnceLock::new();

/// The global x-ray state (created on first use).
pub fn state() -> XRayState {
    STATE.get_or_init(|| XRayState { enabled: ArcRwSignal::new(false), inspect: ArcRwSignal::new(None) }).clone()
}

impl XRayState {
    /// Flip the master switch. Enabling starts the observation machinery
    /// (query taps + connection-state log) and shows the panel; disabling
    /// tears all of it down and forgets what it saw. Persists across reloads.
    pub fn set_enabled(&self, on: bool) {
        self.enabled.set(on);
        if on {
            start_observing();
        } else {
            self.inspect.set(None);
            stop_observing();
            bus::bus().clear_history();
        }
        persist_enabled(on);
    }

    /// The toggle switch (a header lens, Alt+X): a plain binary on↔off.
    pub fn toggle(&self) { self.set_enabled(!self.enabled.get_untracked()); }

    /// Point the L1 drawer at an entity (enables x-ray if it wasn't on).
    pub fn open_inspector(&self, collection: CollectionId, entity_id: EntityId) {
        if !self.enabled.get_untracked() {
            self.set_enabled(true);
        }
        self.inspect.set(Some(InspectTarget { collection, entity_id }));
    }
}

/// Install the changeset taps on every registered query and start recording
/// connection-state transitions. Idempotent: re-running it while observation is
/// already live changes nothing and does NOT re-subscribe (which would replay
/// every query's initial load into the feed).
fn start_observing() {
    bus::bus().set_tapping(true);
    bus::start_connection_log();
}

/// Drop every handle x-ray holds on the running app: the changeset tap on each
/// registered query, and the connection-state subscription.
///
/// Separate from the enabled flag on purpose. [`XRayLauncher`] calls this
/// unconditionally when it unmounts — a route change can take the panel's owner
/// away while x-ray is on, and taps that outlive their owner keep app reactive
/// state alive and keep doing work with nowhere to put it. It deliberately does
/// NOT write `localStorage`: unmounting is not the visitor deciding to turn
/// x-ray off, and their choice has to survive a remount.
fn stop_observing() {
    bus::bus().set_tapping(false);
    bus::stop_connection_log();
}

const STORAGE_KEY: &str = "xray";

fn persist_enabled(on: bool) {
    if let Some(storage) = env::window().and_then(|w| w.local_storage().ok().flatten()) {
        if on {
            let _ = storage.set_item(STORAGE_KEY, "1");
        } else {
            let _ = storage.remove_item(STORAGE_KEY);
        }
    }
}

/// `localStorage["xray"] == "1"` or a `?xray=1` URL param (demo deep links).
fn initially_enabled() -> bool {
    let Some(window) = env::window() else { return false };
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

/// The x-ray host component: restores persisted state, owns the Alt+X hotkey,
/// and mounts the system panel + inspector drawer. Mount it once, inside the
/// subtree where the app's ankurah context is live. It renders no chrome of its
/// own, so users who never touch x-ray never see it.
#[component]
pub fn XRayLauncher() -> impl IntoView {
    let st = state();

    // Restore persisted / URL-requested state once at mount.
    if initially_enabled() && !st.enabled.get_untracked() {
        st.set_enabled(true);
    }

    // Alt+X toggles from anywhere (physical key, so macOS Alt-symbol input
    // doesn't swallow it). Registered once; the launcher lives as long as the
    // app subtree that mounted it.
    let handle = window_event_listener(leptos::ev::keydown, move |ev| {
        if ev.alt_key() && !ev.repeat() && ev.code() == "KeyX" {
            ev.prevent_default();
            state().toggle();
        }
    });
    on_cleanup(move || handle.remove());

    // Observation follows the enabled flag, and the launcher owns it. Reading
    // the flag here is what restarts the taps when the launcher REMOUNTS with
    // x-ray already on — the persisted-state branch above cannot, because it
    // only fires when the flag was off. Cleanup stops observing unconditionally
    // so nothing survives the unmount.
    let enabled_observed = st.enabled.clone();
    Effect::new(move |_| {
        if enabled_observed.get() {
            start_observing();
        } else {
            stop_observing();
        }
    });
    on_cleanup(stop_observing);

    let enabled = st.enabled.clone();
    let inspect = st.inspect.clone();

    view! {
        <Show when=move || enabled.get()>
            <SystemPanel />
        </Show>

        {move || {
            inspect.get().map(|target| view! { <XRayInspector target /> })
        }}
    }
}
