//! The application boundary.
//!
//! FOR: the panel is written against no particular application, and ankurah
//! 0.9.0 cannot answer everything it needs to draw. [`XRayHost`] is where the
//! app fills those gaps — what an entity's properties currently say, what role
//! this node plays, which peers are durable, how to reach an event the browser
//! has never stored. The app implements it once and hands it to
//! [`XRay::install`] before mounting [`crate::XRayLauncher`].
//!
//! Read the trait as a ledger of missing upstream API rather than as a design
//! anyone is proud of. Every method below names the ankurah issue that deletes
//! it; ankurah/community#53 carries the whole retirement map. When the last
//! method goes, the trait goes with it, and a consuming application is back to
//! the two accommodations that issue describes: a `data-entity-id` attribute
//! on x-rayable elements, and loading the panel.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use ankurah::Context;
use ankurah::core::property::backend::LWWBackend;
use ankurah::entity::Entity;
use ankurah::proto::{Clock, CollectionId, EntityId, Event, EventId};
use ankurah_signals::{Peek, Read, Subscribe, SubscriptionGuard};
use async_trait::async_trait;

/// What the panel learns about one entity from the application that owns it.
///
/// Everything here is something ankurah will eventually materialize untyped
/// (ankurah#362 for values and heads, ankurah#337 for per-event provenance).
/// Until then only the app knows its own models, so the app fills this in.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Resolved {
    /// The entity's authoritative in-memory head, when the app has it loaded.
    /// `None` falls back to the head recorded in local storage.
    pub head: Option<Clock>,
    /// `(property, rendered value)` rows for the drawer's "Current values"
    /// card — the present state, distinct from any single event's operations.
    /// Empty is fine; the card hides itself.
    pub values: Vec<(String, String)>,
    /// Event id → the LWW properties whose *current* value that event wrote.
    /// [`lww_provenance`] builds this from an [`Entity`]; the drawer renders it
    /// as the "wrote current" line on a selected DAG node.
    pub wrote_current: HashMap<EventId, Vec<String>>,
    /// A deliberate app-side refusal to show this entity's history, with the
    /// reason to display. Not an error — the drawer renders it as a notice.
    /// Community uses it to hide deleted messages from non-moderators.
    pub refusal: Option<String>,
}

/// Node facts for the panel's "This node" card.
///
/// `Option` fields mean "this app has nothing to say" and render as nothing,
/// so an app without a policy agent does not get a misleading "policy
/// syncing…" chip. Retires under ankurah#357 (client observability ergonomics).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NodeStatus {
    /// Durable nodes keep storage authoritatively; browser clients are not.
    /// `None` = this app has nothing to say; the panel asserts nothing and
    /// draws no role label at all. A missing answer is not "ephemeral".
    pub durable: Option<bool>,
    /// Whether the policy agent finished its initial sync.
    pub policy_ready: Option<bool>,
    /// Whether the node has joined the system catalog.
    pub system_ready: Option<bool>,
    /// Short base64 of the system root's head, when there is a root.
    pub system_root_head: Option<String>,
}

/// Everything the panel needs from the application that mounts it.
///
/// `Send + Sync` on the trait object (the panel parks it in a process global,
/// same as its bus and its UI state); the *futures* are deliberately not Send,
/// because the browser drives them from `spawn_local` and ankurah's wasm
/// futures are not Send either.
#[async_trait(?Send)]
pub trait XRayHost: Send + Sync + 'static {
    /// The authenticated context the panel reads storage through. The panel
    /// never mutates; it opens collections, reads events, and reads states.
    fn context(&self) -> Context;

    /// The collections the app can resolve, offered in the panel's
    /// "inspect by id" row. Retires under ankurah#362 (collection schema
    /// discovery), which lets the panel enumerate them itself.
    fn collections(&self) -> Vec<CollectionId>;

    /// Resolve one entity to its current values, head, and access ruling.
    ///
    /// THE typed-view gap: ankurah 0.9.0 can materialize an entity's values
    /// only through a generated `View`, so the panel cannot read properties of
    /// a model it was not compiled against. Community implements this over
    /// `MessageView` / `RoomView` / `UserView`; any app implements it over its
    /// own. Returning `Ok(Resolved::default())` for an unknown collection is
    /// correct — the drawer still renders the event DAG, just without a
    /// "Current values" card. Retires under ankurah#362.
    async fn resolve(&self, collection: &CollectionId, entity_id: EntityId) -> Result<Resolved, String>;

    /// Node identity and readiness for the "This node" card. Default: nothing
    /// known, which renders no role and no readiness chips at all.
    /// Retires under ankurah#357.
    fn node_status(&self) -> NodeStatus { NodeStatus::default() }

    /// The durable peers this node is talking to (`Node::get_durable_peers`,
    /// which is generic over the storage engine and policy agent, so the panel
    /// cannot call it). Retires under ankurah#357 item 1, which makes
    /// `Presence` nameable and carries peer identity properly.
    fn durable_peers(&self) -> Vec<EntityId> { Vec::new() }

    /// Fetch one event the browser has not stored, from a durable peer.
    ///
    /// The inspector walks an entity's history backwards and calls this for
    /// every ancestor missing locally. `ankurah::core::retrieval::CachedEventGetter`
    /// does exactly this but is generic over `Node<SE, PA>` and the policy
    /// context data, so the app constructs it — see the README for the ten
    /// lines that takes. Default: refuse, and the drawer reports the ancestors
    /// as unavailable rather than failing.
    ///
    /// Constructing a `CachedEventGetter` per call is equivalent to holding
    /// one: its only state is an event staging map the inspector never writes.
    async fn fetch_remote_event(&self, collection: &CollectionId, event_id: &EventId) -> Result<Event, String> {
        let _ = (collection, event_id);
        Err("no durable-peer event source configured".to_string())
    }

    /// The IndexedDB database name the app's storage engine opens, which turns
    /// on two raw-IndexedDB workarounds: the per-entity event index scan
    /// (`dump_entity_events` is broken in ankurah-storage-indexeddb-wasm
    /// 0.9.0 — ankurah#342) and the object-store row counts for the "local
    /// cache" line (no storage-stats API — ankurah#357 item 2).
    ///
    /// `None` — the right answer for any non-IndexedDB app — takes the honest
    /// path instead: `dump_entity_events` for events, and no storage counts.
    fn indexeddb_database(&self) -> Option<String> { None }
}

/// Build the `wrote_current` map for [`Resolved`] from an entity's LWW
/// backend: for each named property, which event last wrote the value standing
/// today.
///
/// `LWWBackend` has no property enumeration in ankurah 0.9.0, so callers pass
/// their model's LWW property names. Retires under ankurah#337 piece 2, which
/// describes per-event values directly.
pub fn lww_provenance(entity: &Entity, props: &[&str]) -> HashMap<EventId, Vec<String>> {
    let mut out: HashMap<EventId, Vec<String>> = HashMap::new();
    if let Ok(backend) = entity.get_backend::<LWWBackend>() {
        for prop in props {
            if let Some(event_id) = backend.get_event_id(&(*prop).to_string()) {
                out.entry(event_id).or_default().push((*prop).to_string());
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Connection state: passed as a signal, not implemented as a trait method
// ---------------------------------------------------------------------------

/// Sink the panel installs to receive connection-state transitions.
pub(crate) type ConnListener = Box<dyn Fn(String) + Send + Sync + 'static>;

/// The app's connection-state signal, already erased to `String`.
///
/// Why this is not an `XRayHost` method: ankurah-websocket-client-wasm 0.9.0
/// keeps its `ConnectionState` module private (`lib.rs` re-exports only
/// `WebsocketClient`), so no signature anywhere can name the type. A generic
/// builder method can still *take* it and render it through `Display`, which
/// is all the connection card ever did. Retires under ankurah#357 item 1,
/// which re-exports `ConnectionState` and `Presence` — then this becomes an
/// ordinary trait method returning structured presence.
pub(crate) struct ConnectionTap {
    pub endpoint: String,
    pub peek: Box<dyn Fn() -> String + Send + Sync>,
    pub subscribe: Box<dyn Fn(ConnListener) -> SubscriptionGuard + Send + Sync>,
}

/// Subscribe to a `Read<T>` rendering values through `Display`. Exists because
/// `T` is the ws client's unnameable `ConnectionState`: a bare closure cannot
/// annotate its parameter, which `IntoSubscribeListener`'s two-generic blanket
/// impl needs — a generic parameter pinned by `&Read<T>` resolves it.
fn subscribe_display<T>(read: &Read<T>, sink: ConnListener) -> SubscriptionGuard
where T: std::fmt::Display + Clone + Send + Sync + 'static {
    read.subscribe(move |value: T| sink(value.to_string()))
}

// ---------------------------------------------------------------------------
// Installation
// ---------------------------------------------------------------------------

pub(crate) struct Installed {
    pub host: Arc<dyn XRayHost>,
    pub connection: Option<ConnectionTap>,
}

static INSTALLED: OnceLock<Installed> = OnceLock::new();

/// [`XRay::install`] was called when an installation already existed.
///
/// The panel is a process-global singleton — the same shape as its bus and its
/// UI state — so the FIRST installation wins and every later one is rejected
/// whole. Nothing is replaced or merged. A process that reaches this has two
/// applications (or two Leptos roots, or two tests) trying to own one panel,
/// and whichever host lost is not the one the panel will read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InstallError;

impl std::fmt::Display for InstallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ankurah_xray: an x-ray installation already exists (process-global singleton; the first call wins)")
    }
}

impl std::error::Error for InstallError {}

/// The application's x-ray installation: its [`XRayHost`] plus the optional
/// connection signal. Build it once during app startup, before Leptos mounts.
///
/// ```ignore
/// ankurah_xray::XRay::new(MyHost { ctx })
///     .connection(ws_client.connection_state(), ws_url())
///     .install()
///     .expect("install ankurah-xray once");
/// ```
pub struct XRay {
    host: Arc<dyn XRayHost>,
    connection: Option<ConnectionTap>,
}

impl XRay {
    pub fn new(host: impl XRayHost) -> Self { Self { host: Arc::new(host), connection: None } }

    /// Feed the connection card from any `Display`-able reactive state. The
    /// panel subscribes while x-ray is enabled and drops the subscription when
    /// it is disabled, so an app that never opens x-ray pays nothing.
    pub fn connection<T>(mut self, state: Read<T>, endpoint: impl Into<String>) -> Self
    where T: std::fmt::Display + Clone + Send + Sync + 'static {
        let peek_state = state.clone();
        self.connection = Some(ConnectionTap {
            endpoint: endpoint.into(),
            peek: Box::new(move || Peek::peek(&peek_state).to_string()),
            subscribe: Box::new(move |sink| subscribe_display(&state, sink)),
        });
        self
    }

    /// Publish this installation to the panel, or report that one already
    /// exists. First call wins — see [`InstallError`]. The result is worth
    /// handling rather than dropping: silently losing the race means the panel
    /// reads a host the app never meant it to.
    #[must_use = "a rejected installation means the panel is reading someone else's host"]
    pub fn install(self) -> Result<(), InstallError> {
        INSTALLED.set(Installed { host: self.host, connection: self.connection }).map_err(|_| InstallError)
    }
}

/// The installed host. Panics if the app mounted the panel without installing.
pub(crate) fn host() -> Arc<dyn XRayHost> { installed().host.clone() }

/// The installed connection tap, if the app supplied one. Tolerates nothing
/// being installed at all, so a stray `state().toggle()` before install fails
/// where the message is good — at [`host`] — instead of here.
pub(crate) fn connection() -> Option<&'static ConnectionTap> { INSTALLED.get().and_then(|i| i.connection.as_ref()) }

/// Whether an application has installed itself yet.
pub fn is_installed() -> bool { INSTALLED.get().is_some() }

fn installed() -> &'static Installed { INSTALLED.get().expect("ankurah_xray: call XRay::new(host).install() before mounting XRayLauncher") }
