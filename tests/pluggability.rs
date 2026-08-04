//! Proof that the panel runs against an application it was never written for.
//!
//! The fake app here shares nothing with Ankurah Community: its own model
//! (`Widget`), its own storage engine (sled on a temp dir instead of
//! IndexedDB), its own policy agent, and its own [`XRayHost`] impl. If the
//! panel can enumerate this app's live query, resolve its entity, and draw
//! itself, the extraction is complete.
//!
//! One test function, not several: the bus, the UI state, and the installed
//! host are process-global singletons — that is the design, carried over from
//! Community — so the scenario runs in one deterministic order.
//!
//! What a headless run cannot prove, and why: the inspector drawer's event-DAG
//! fetch, the drag-to-move panel header, localStorage persistence, the Alt+X
//! hotkey, and the raw-IndexedDB paths all call browser APIs, which abort the
//! process off-wasm (see `src/env.rs`). Those need a browser; this proves
//! everything up to them.

use std::sync::Arc;
use std::time::Duration;

use ankurah::policy::{DEFAULT_CONTEXT, PermissiveAgent};
use ankurah::proto::{CollectionId, EntityId, Event, EventId};
use ankurah::{Context, Model, Node, View};
use ankurah_storage_sled::SledStorageEngine;
use ankurah_xray::bus::bus;
use ankurah_xray::host::{NodeStatus, Resolved, XRayHost, lww_provenance};
use ankurah_xray::system_panel::SystemPanel;
use ankurah_xray::{InstallError, XRay, state};
use async_trait::async_trait;
use leptos::prelude::*;

/// The fake application's one model. `label` is a yrs text property (the
/// collaborative kind); `slot` is LWW, so the panel's "wrote current"
/// provenance has something to report.
#[derive(Model, Debug, serde::Serialize, serde::Deserialize)]
pub struct Widget {
    pub label: String,
    #[active_type(LWW)]
    pub slot: String,
}

type TestNode = Node<SledStorageEngine, PermissiveAgent>;

/// The fake app's ten-line boundary implementation. Nothing here knows what a
/// message or a room is.
struct WidgetHost {
    ctx: Context,
    node: TestNode,
}

#[async_trait(?Send)]
impl XRayHost for WidgetHost {
    fn context(&self) -> Context { self.ctx.clone() }

    fn collections(&self) -> Vec<CollectionId> { vec![Widget::collection()] }

    async fn resolve(&self, collection: &CollectionId, entity_id: EntityId) -> Result<Resolved, String> {
        if *collection != Widget::collection() {
            return Ok(Resolved::default());
        }
        let widget: WidgetView = self.ctx.get(entity_id).await.map_err(|e| e.to_string())?;
        Ok(Resolved {
            head: Some(widget.entity().head()),
            values: vec![("label".into(), widget.label().unwrap_or_default()), ("slot".into(), widget.slot().unwrap_or_default())],
            wrote_current: lww_provenance(widget.entity(), &["slot"]),
            // An app can refuse here; this one has nothing to hide.
            refusal: None,
        })
    }

    fn node_status(&self) -> NodeStatus {
        NodeStatus {
            durable: Some(self.node.durable),
            policy_ready: None, // PermissiveAgent has nothing to sync.
            system_ready: Some(self.node.system.is_system_ready()),
            system_root_head: self.node.system.root().map(|r| r.payload.state.head.to_base64_short()),
        }
    }

    fn durable_peers(&self) -> Vec<EntityId> { self.node.get_durable_peers() }

    /// The shape a real app uses: build a `CachedEventGetter` per call (its
    /// only state is a staging map the panel never writes) and let it check
    /// local storage, then a durable peer. This node has no peers, so an
    /// ancestor that is genuinely missing stays missing — which the drawer
    /// reports as "unavailable" rather than failing.
    async fn fetch_remote_event(&self, collection: &CollectionId, event_id: &EventId) -> Result<Event, String> {
        use ankurah::core::retrieval::{CachedEventGetter, GetEvents};
        let col = self.ctx.collection(collection).await.map_err(|e| e.to_string())?;
        let cdata = DEFAULT_CONTEXT;
        let getter = CachedEventGetter::new(collection.clone(), col, &self.node, &cdata);
        getter.get_event(event_id).await.map_err(|e| e.to_string())
    }
}

async fn boot() -> (TestNode, Context) {
    let engine = SledStorageEngine::new_test().expect("temp-dir sled engine");
    let node = Node::new_durable(Arc::new(engine), PermissiveAgent::new());
    node.system.wait_loaded().await;
    if node.system.root().is_none() {
        node.system.create().await.expect("create system root");
    }
    node.system.wait_system_ready().await;
    let ctx = node.context(DEFAULT_CONTEXT).expect("context");
    (node, ctx)
}

/// Render a component to HTML under a reactive owner, the way a server render
/// would. Returns the markup so assertions can read what a user would see. The
/// closure runs inside the owner because building the view is itself reactive
/// work — `<For>` claims the current owner as it is constructed.
fn render<V: IntoView + 'static>(build: impl FnOnce() -> V) -> String {
    let owner = Owner::new();
    let html = owner.with(|| build().into_view().to_html());
    drop(owner);
    html
}

#[tokio::test(flavor = "multi_thread")]
async fn panel_runs_against_a_foreign_app() {
    let (node, ctx) = boot().await;

    // ---- the app's own data -------------------------------------------------
    let trx = ctx.begin();
    let widget = trx.create(&Widget { label: "left flange".into(), slot: "A7".into() }).await.expect("create widget");
    let widget_id = widget.id();
    trx.create(&Widget { label: "right flange".into(), slot: "B2".into() }).await.expect("create widget");
    trx.commit().await.expect("commit");

    // ---- install the boundary ----------------------------------------------
    assert!(!ankurah_xray::is_installed(), "nothing should be installed before the app installs itself");
    XRay::new(WidgetHost { ctx: ctx.clone(), node: node.clone() }).install().expect("first installation wins");
    assert!(ankurah_xray::is_installed());
    assert_eq!(
        XRay::new(WidgetHost { ctx: ctx.clone(), node: node.clone() }).install(),
        Err(InstallError),
        "a second installation is rejected whole rather than replacing the first",
    );

    // Enable BEFORE registering, so the changeset tap is in place when the
    // query first loads and the feed sees its initial batch.
    state().set_enabled(true);
    assert!(state().enabled.get_untracked());

    // ---- register a live query with the bus --------------------------------
    // Registered the way a component registers: under the owner that created
    // the query, with an `on_cleanup` that releases it. The bus keeps a clone
    // of the LiveQuery — an `Arc` handle to its reactor subscription — so a
    // registration nobody releases outlives the app code that made it, in a
    // process-global registry with no other janitor. `on_cleanup` is the whole
    // discipline; the crate offers no guard type, because the registry itself
    // is a hack retiring under ankurah#361.
    let widgets = ctx.query::<WidgetView>("true").expect("query");
    let app_owner = Owner::new();
    app_owner.with(|| {
        let registration = bus().register("widgets (test app)", &widgets);
        on_cleanup(move || bus().unregister(registration));
    });

    let snapshot = bus().snapshot();
    assert_eq!(snapshot.len(), 1, "the app's query should be the only registry entry");
    assert_eq!(snapshot[0].label, "widgets (test app)");
    assert_eq!(snapshot[0].collection, Widget::collection());
    assert_eq!(snapshot[0].query_id, widgets.query_id());

    // The tap turns the reactor's changesets into feed rows. The initial load
    // is coalesced into one row, so waiting for a non-empty feed proves the
    // whole path: reactor → typed changeset → bus tap → renderable entry.
    let feed = bus().feed();
    for _ in 0..200 {
        if !feed.with_untracked(|f| f.is_empty()) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let rows = feed.get_untracked();
    assert!(!rows.is_empty(), "the changeset tap should have produced at least one feed row");
    assert!(rows.iter().all(|r| r.query_label == "widgets (test app)"));
    assert!(rows.iter().any(|r| r.kind == "initial" && r.count == 2), "both widgets should appear in the initial batch: {rows:?}");

    // ---- resolve a fake entity through the trait ---------------------------
    let host = WidgetHost { ctx: ctx.clone(), node: node.clone() };
    let resolved = host.resolve(&Widget::collection(), widget_id).await.expect("resolve");
    assert_eq!(resolved.values, vec![("label".to_string(), "left flange".to_string()), ("slot".to_string(), "A7".to_string())]);
    assert!(resolved.head.is_some(), "the app knows the entity's authoritative head");
    assert!(resolved.refusal.is_none());
    assert!(
        resolved.wrote_current.values().any(|props| props.iter().any(|p| p == "slot")),
        "the creating event should be recorded as the writer of the current `slot` value",
    );

    // A collection the app does not know resolves to nothing rather than
    // failing, so the drawer still draws the event DAG.
    let unknown = host.resolve(&CollectionId::from("sprocket"), widget_id).await.expect("unknown collection is not an error");
    assert_eq!(unknown, Resolved::default());

    // ---- render the panel --------------------------------------------------
    let html = render(|| view! { <SystemPanel /> });

    assert!(html.contains("X-ray"), "panel header");
    // The node card reads identity straight off the app's Context.
    assert!(html.contains(&ctx.node_id().to_base64_short()), "node card shows this node's id");
    assert!(html.contains("durable"), "node card shows the role the host reported");
    assert!(!html.contains("policy syncing"), "a host reporting no policy state should render no policy chip");
    // The inspect row offers the app's collections, not community's.
    assert!(html.contains(">widget<"), "collection select is driven by XRayHost::collections");
    assert!(!html.contains(">message<") && !html.contains(">room<"), "no community collections survive the extraction");
    // The queries card lists what the app registered.
    assert!(html.contains("widgets (test app)"), "queries card lists the registered query");
    // The connection card degrades honestly when the app supplied no signal.
    assert!(html.contains("not reported"), "no connection signal installed, so the card says so");
    assert!(html.contains("Live event feed"), "feed card");

    // ---- teardown ----------------------------------------------------------
    // Dropping the owner is the component unmounting: its cleanup runs, and the
    // registration goes with it.
    drop(app_owner);
    assert!(bus().snapshot().is_empty(), "the owner's cleanup released the registration");
    state().set_enabled(false);
    assert!(bus().feed().get_untracked().is_empty(), "disabling drops accumulated observations");
}
