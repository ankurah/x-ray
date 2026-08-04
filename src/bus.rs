//! The x-ray bus: an app-side registry of LiveQueries plus a bounded live
//! event feed built from their changesets.
//!
//! HACK, retires under ankurah/ankurah#361. This whole module exists because
//! ankurah 0.9.0 keeps the reactor's subscription table `pub(crate)`
//! (`ankurah-core/src/node.rs`), so a client cannot enumerate *all* node
//! subscriptions — only the queries the app itself holds and hands to
//! `register` by name. When #361 lands (query enumeration, optional labels,
//! untyped changeset taps, a register/drop lifecycle signal), the panel reads
//! the node directly, every `bus().register(...)` call in every consuming app
//! is deleted, and this file goes with them. The algorithm is Community's, kept
//! that way on purpose: it is the working reference for the data shape an
//! inspector needs, which is what #361 was specified against. Only the locking
//! changed on extraction — see [`Registry`]. The panel's query card labels
//! itself honestly in the meantime.
//!
//! Registration stores cheap introspection handles (`query_id`, the reactive
//! selection, the untyped resultset, the error signal) plus a clone of the
//! `LiveQuery` itself, inside the tap factory. The changeset *tap*
//! (`LiveQuery::subscribe`, which is what feeds the event stream) is installed
//! only while x-ray is enabled and dropped on disable, so toggling x-ray off
//! costs a registered query nothing — but the REGISTRATION still costs, because
//! `LiveQuery` is an `Arc`-backed handle to a reactor subscription (and, for a
//! strong query, to the node). While an entry is in this registry the bus keeps
//! that subscription alive whether x-ray is on or off.
//!
//! Which makes `unregister` the caller's obligation, and a real one: a
//! component that registers and then unmounts without unregistering has leaked
//! its query into a process-global registry that nothing else will ever clean
//! up. See [`BusHandle::register`].
//!
//! Integration: call `ankurah_xray::bus::bus().register("rooms", &query)` right
//! where the app creates each long-lived query, and pair it with an
//! `on_cleanup` that unregisters.
//!
//! ## Re-entrancy
//!
//! [`Registry`] is a plain `std::sync::Mutex`, which is not reentrant, and tap
//! factories run while it is held. Nothing re-enters today — a tap callback
//! only pushes onto the feed signal — but a changeset delivered synchronously
//! inside `LiveQuery::subscribe` whose handler called `register`/`unregister`
//! would deadlock. Inherited from Community verbatim; a note for whoever
//! rewrites this against #361, not a live defect.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};

use leptos::prelude::{ArcRwSignal, Set, Update};

use ankurah::changes::{ChangeSet, ItemChange};
use ankurah::core::livequery::EntityLiveQuery;
use ankurah::core::resultset::EntityResultSet;
use ankurah::error::RetrievalError;
use ankurah::proto::{Attested, Clock, CollectionId, EntityId, Event, EventId, QueryId};
use ankurah::{LiveQuery, View};
use ankurah_signals::{Read, Subscribe, SubscriptionGuard};

use crate::env::now_ms;

/// Handle for removing a registry entry (returned by [`BusHandle::register`]).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct RegistrationId(u64);

/// One registered LiveQuery: introspection handles + the deferred tap.
pub struct QueryEntry {
    pub id: RegistrationId,
    pub label: String,
    pub query_id: QueryId,
    pub collection: CollectionId,
    /// Reactive (selection, version) — the version bumps on predicate updates.
    pub selection: Read<(ankurah::ankql::ast::Selection, u32)>,
    /// Untyped resultset; `len()` / `is_loaded()` track reactively.
    pub resultset: EntityResultSet,
    pub error: Read<Option<RetrievalError>>,
    /// Changesets seen by the tap since registration (activity indicator).
    pub changes_seen: ArcRwSignal<u64>,
    /// Installs the changeset tap. Kept as a factory so taps can be created
    /// and dropped as x-ray toggles without re-registering.
    make_tap: Box<dyn Fn() -> SubscriptionGuard + Send + Sync>,
    /// The live tap, present exactly while [`Registry::tapping`] is true.
    /// Guarded by the registry lock, not its own — see [`Registry`].
    tap: Option<SubscriptionGuard>,
}

/// Renderable clone of one registry entry (see [`BusHandle::snapshot`]).
#[derive(Clone)]
pub struct QuerySnapshot {
    pub id: RegistrationId,
    pub label: String,
    pub query_id: QueryId,
    pub collection: CollectionId,
    pub selection: Read<(ankurah::ankql::ast::Selection, u32)>,
    pub resultset: EntityResultSet,
    pub error: Read<Option<RetrievalError>>,
    pub changes_seen: ArcRwSignal<u64>,
}

/// One event as it appeared in a changeset, summarized for the feed.
#[derive(Clone, Debug)]
pub struct FeedEvent {
    pub id: EventId,
    pub parent: Clock,
    /// Per-backend op summaries (yrs deltas decoded, LWW byte sizes).
    pub badges: Vec<crate::decode::OpBadge>,
}

/// One row of the live feed: a membership change from a registered query.
#[derive(Clone, Debug)]
pub struct FeedEntry {
    pub seq: u64,
    pub at_ms: f64,
    pub query_label: String,
    pub collection: CollectionId,
    /// `None` for coalesced initial-load batches.
    pub entity_id: Option<EntityId>,
    pub kind: &'static str,
    /// Number of items this row covers (>1 only for initial batches).
    pub count: usize,
    /// The entity's head clock after the change (short form).
    pub head_short: String,
    pub events: Vec<FeedEvent>,
}

pub const FEED_CAP: usize = 100;
const CONN_LOG_CAP: usize = 24;

/// The registered queries and the tap switch, under one lock.
///
/// They are one fact — "these queries are registered, and their changeset taps
/// are (or are not) installed" — and keeping them apart made that fact
/// tearable. `register` used to sample an `AtomicBool` and only then take the
/// entries lock, while `set_tapping` stored the flag and only then took it, so
/// a registration interleaved with a toggle could land an entry tapped while
/// x-ray was off, or untapped while it was on. Under one lock the flag and the
/// entry it decides always move together.
///
/// The registry itself retires under ankurah#361: once the node can enumerate
/// its own queries there is nothing here to keep consistent.
struct Registry {
    tapping: bool,
    entries: Vec<QueryEntry>,
}

struct BusInner {
    registry: Mutex<Registry>,
    /// Bumped on register/unregister so the panel re-reads the entry list.
    entries_rev: ArcRwSignal<u64>,
    feed: ArcRwSignal<VecDeque<FeedEntry>>,
    next_id: AtomicU64,
    /// Connection-state transition log (timestamp ms, description).
    conn_log: ArcRwSignal<VecDeque<(f64, String)>>,
    /// The latest connection state, as the app's signal rendered it.
    conn_state: ArcRwSignal<String>,
    conn_guard: Mutex<Option<SubscriptionGuard>>,
}

/// Cheap clonable handle to the process-wide bus.
#[derive(Clone)]
pub struct BusHandle(&'static BusInner);

static BUS: OnceLock<BusInner> = OnceLock::new();
static FEED_SEQ: AtomicU64 = AtomicU64::new(0);

/// The global x-ray bus (created on first use).
pub fn bus() -> BusHandle {
    BusHandle(BUS.get_or_init(|| BusInner {
        registry: Mutex::new(Registry { tapping: false, entries: Vec::new() }),
        entries_rev: ArcRwSignal::new(0),
        feed: ArcRwSignal::new(VecDeque::new()),
        next_id: AtomicU64::new(1),
        conn_log: ArcRwSignal::new(VecDeque::new()),
        conn_state: ArcRwSignal::new(String::new()),
        conn_guard: Mutex::new(None),
    }))
}

impl BusHandle {
    /// The registry lock. A poisoned lock recovers the inner value: a panic in
    /// one tap must not take the panel's registry down with it.
    fn registry(&self) -> MutexGuard<'_, Registry> { self.0.registry.lock().unwrap_or_else(|e| e.into_inner()) }

    /// Register a LiveQuery under a human label. Introspection is immediate;
    /// the changeset tap is installed only while x-ray is enabled.
    ///
    /// **The caller must `unregister`.** The entry holds a clone of `lq`, and a
    /// `LiveQuery` is an `Arc` handle to a reactor subscription — so an entry
    /// left behind keeps that subscription alive in a process-global registry
    /// for the rest of the process, whether or not x-ray is ever turned on. In
    /// Leptos, pair the call with the component's own teardown:
    ///
    /// ```ignore
    /// let reg = bus().register("rooms", &rooms_query);
    /// on_cleanup(move || bus().unregister(reg));
    /// ```
    ///
    /// No guard type is offered on purpose: this whole registry is a hack
    /// retiring under ankurah#361, and hardening it would make it harder to
    /// delete. The obligation is documented rather than enforced.
    pub fn register<R>(&self, label: &str, lq: &LiveQuery<R>) -> RegistrationId
    where R: View + Clone + Send + Sync + 'static {
        let id = RegistrationId(self.0.next_id.fetch_add(1, Ordering::Relaxed));
        let changes_seen = ArcRwSignal::new(0u64);

        let make_tap: Box<dyn Fn() -> SubscriptionGuard + Send + Sync> = {
            let lq = lq.clone();
            let feed = self.0.feed.clone();
            let label = label.to_string();
            let collection = R::collection();
            let changes_seen = changes_seen.clone();
            Box::new(move || {
                let feed = feed.clone();
                let label = label.clone();
                let collection = collection.clone();
                let changes_seen = changes_seen.clone();
                lq.subscribe(move |cs: ChangeSet<R>| {
                    changes_seen.update(|n| *n += 1);
                    push_changeset(&feed, &label, &collection, &cs);
                })
            })
        };

        // Untyped resultset via the EntityLiveQuery deref (the typed
        // `LiveQuery::resultset` would pin us to R here for no benefit).
        let elq: &EntityLiveQuery = lq;
        let mut entry = QueryEntry {
            id,
            label: label.to_string(),
            query_id: lq.query_id(),
            collection: R::collection(),
            selection: lq.selection(),
            resultset: elq.resultset(),
            error: lq.error(),
            changes_seen,
            tap: None,
            make_tap,
        };

        {
            // Read the switch and insert under the same lock, so the entry
            // cannot land on the wrong side of a concurrent toggle.
            let mut registry = self.registry();
            if registry.tapping {
                entry.tap = Some((entry.make_tap)());
            }
            registry.entries.push(entry);
        }
        self.0.entries_rev.update(|r| *r += 1);
        id
    }

    /// Drop a registry entry (and its tap, if installed). Releases the bus's
    /// clone of the query — see [`BusHandle::register`] for why that matters.
    pub fn unregister(&self, id: RegistrationId) {
        self.registry().entries.retain(|e| e.id != id);
        self.0.entries_rev.update(|r| *r += 1);
    }

    /// Install or drop the changeset taps on every registered query.
    /// Called from `crate::start_observing` / `crate::stop_observing` — not
    /// part of the public API.
    ///
    /// A call that does not change the switch returns without touching the
    /// taps. That is what makes turning x-ray on twice harmless: re-running
    /// `make_tap` would drop and re-create every subscription, and each fresh
    /// subscription replays its query's initial load into the feed.
    pub(crate) fn set_tapping(&self, on: bool) {
        let mut registry = self.registry();
        if registry.tapping == on {
            return;
        }
        registry.tapping = on;
        for entry in registry.entries.iter_mut() {
            let tap = on.then(|| (entry.make_tap)());
            entry.tap = tap;
        }
    }

    /// Reactive revision counter for the entry list (bumps on (un)register).
    pub fn entries_rev(&self) -> ArcRwSignal<u64> { self.0.entries_rev.clone() }

    /// Clone-out snapshot of the registry's reactive handles for rendering.
    /// (Entries themselves hold the tap and are not `Clone`.)
    pub fn snapshot(&self) -> Vec<QuerySnapshot> {
        self.registry()
            .entries
            .iter()
            .map(|e| QuerySnapshot {
                id: e.id,
                label: e.label.clone(),
                query_id: e.query_id,
                collection: e.collection.clone(),
                selection: e.selection.clone(),
                resultset: e.resultset.clone(),
                error: e.error.clone(),
                changes_seen: e.changes_seen.clone(),
            })
            .collect()
    }

    /// The live feed ring buffer (newest first).
    pub fn feed(&self) -> ArcRwSignal<VecDeque<FeedEntry>> { self.0.feed.clone() }

    /// The connection-state transition log (newest first).
    pub fn conn_log(&self) -> ArcRwSignal<VecDeque<(f64, String)>> { self.0.conn_log.clone() }

    /// The latest connection state, empty until the app's tap is installed.
    pub fn conn_state(&self) -> ArcRwSignal<String> { self.0.conn_state.clone() }

    /// Drop accumulated observations (live feed + connection log) so a
    /// re-enable starts fresh instead of replaying stale rows.
    pub fn clear_history(&self) {
        self.0.feed.update(|f| f.clear());
        self.0.conn_log.update(|l| l.clear());
    }
}

/// Convert one changeset into feed rows. Add/Update/Remove get a row each
/// (with their events); the initial load is coalesced into a single row so
/// opening a query doesn't flood the feed.
fn push_changeset<R>(feed: &ArcRwSignal<VecDeque<FeedEntry>>, label: &str, collection: &CollectionId, cs: &ChangeSet<R>)
where R: View + Clone {
    let now = now_ms();
    let mut rows: Vec<FeedEntry> = Vec::new();
    let mut initial_count = 0usize;

    for change in &cs.changes {
        let kind = match change {
            ItemChange::Initial { .. } => {
                initial_count += 1;
                continue;
            }
            ItemChange::Add { .. } => "add",
            ItemChange::Update { .. } => "update",
            ItemChange::Remove { .. } => "remove",
        };
        let item = change.entity();
        rows.push(FeedEntry {
            seq: FEED_SEQ.fetch_add(1, Ordering::Relaxed),
            at_ms: now,
            query_label: label.to_string(),
            collection: collection.clone(),
            entity_id: Some(item.id()),
            kind,
            count: 1,
            head_short: item.entity().head().to_base64_short(),
            events: change.events().iter().map(summarize_event).collect(),
        });
    }

    if initial_count > 0 {
        rows.push(FeedEntry {
            seq: FEED_SEQ.fetch_add(1, Ordering::Relaxed),
            at_ms: now,
            query_label: label.to_string(),
            collection: collection.clone(),
            entity_id: None,
            kind: "initial",
            count: initial_count,
            head_short: String::new(),
            events: Vec::new(),
        });
    }

    if rows.is_empty() {
        return;
    }
    feed.update(|q| {
        for row in rows {
            q.push_front(row);
        }
        q.truncate(FEED_CAP);
    });
}

fn summarize_event(attested: &Attested<Event>) -> FeedEvent {
    let event = &attested.payload;
    FeedEvent { id: event.id(), parent: event.parent.clone(), badges: crate::decode::op_badges(event) }
}

/// Start recording connection-state transitions (idempotent). The demo beat:
/// kill the server and the connection card narrates Closed → Connecting →
/// Connected in order, with timestamps.
///
/// Does nothing when the app supplied no connection signal to
/// [`crate::XRay::connection`] — the connection card then renders "not
/// reported" rather than a lie.
///
/// HACK, retires under ankurah/ankurah#357 item 1. Everything about this path
/// is shaped by `ConnectionState` living in a private module of
/// ankurah-websocket-client-wasm 0.9.0 (`lib.rs` re-exports only
/// `WebsocketClient`), so no signature in this crate can name the type. What
/// survives is the value's strum `Display` ("Connected", "Closed", …), which is
/// why the app hands the signal in generically and the panel stores strings.
/// Structured `Presence` (server url / node id / system root) needs the
/// upstream re-export; until then the connection card derives server identity
/// from the host's `durable_peers()`.
pub(crate) fn start_connection_log() {
    let Some(tap) = crate::host::connection() else { return };
    let handle = bus();
    let mut guard = handle.0.conn_guard.lock().unwrap_or_else(|e| e.into_inner());
    if guard.is_some() {
        return;
    }
    let log = handle.0.conn_log.clone();
    let current = handle.0.conn_state.clone();

    // Seed with the current state so the log never starts empty.
    push_conn(&log, &current, (tap.peek)());

    *guard = Some((tap.subscribe)(Box::new(move |line| push_conn(&log, &current, line))));
}

pub(crate) fn stop_connection_log() {
    let handle = bus();
    *handle.0.conn_guard.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

fn push_conn(log: &ArcRwSignal<VecDeque<(f64, String)>>, current: &ArcRwSignal<String>, line: String) {
    current.set(line.clone());
    log.update(|q| {
        // Collapse consecutive duplicates (the signal can re-emit the same state).
        if q.front().map(|(_, l)| l == &line).unwrap_or(false) {
            return;
        }
        q.push_front((now_ms(), line));
        q.truncate(CONN_LOG_CAP);
    });
}
