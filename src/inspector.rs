//! The L1 per-entity inspector drawer: fetches an entity's full event history
//! (everything stored locally first, then a backward walk over missing
//! ancestors through the host, which decides for itself where to get them and
//! whether to keep them), lays it out as a DAG, and shows raw event detail for
//! a selected node.
//!
//! Fetches happen only when the drawer opens — never per visible element. One
//! entity is typically 1–10 events; the walk is capped at [`FETCH_CAP`] as a
//! guard against pathological histories.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use leptos::prelude::*;

use ankurah::core::storage::StorageCollectionWrapper;
use ankurah::proto::{Attested, Clock, EntityId, Event, EventId};

use crate::dag::{DagModel, DagNodeInput, DagView, layout};
use crate::decode::op_badges;
use crate::env;
use crate::host::host;
use crate::{InspectTarget, state};

/// Hard cap on events fetched per inspection (dump + walk combined).
const FETCH_CAP: usize = 500;

#[derive(Clone, Debug, PartialEq)]
struct History {
    dag: DagModel,
    head: Clock,
    total: usize,
    local: usize,
    fetched: usize,
    /// Parent ids that could not be retrieved (offline / policy / cap).
    unresolved: usize,
    /// The entity's current materialized property values (name, value) — the
    /// present state, distinct from any single event's operations. Empty for
    /// collections the application declined to resolve.
    current_state: Vec<(String, String)>,
}

#[derive(Clone, Debug, PartialEq)]
enum Phase {
    Loading,
    /// The application declined to show this entity's history, with a reason.
    /// Not an error — `XRayHost::resolve` returns it deliberately.
    Refused(String),
    Failed(String),
    Ready(History),
}

/// Which history walk the drawer is currently listening to.
///
/// FOR: making sure exactly one walk can ever write into the drawer — the
/// newest one, and only while the drawer is still open. A walk is a chain of
/// awaits (resolve, open the collection, dump local events, one request per
/// missing ancestor, read back attestations), and three things can happen
/// underneath it: the visitor closes the drawer, they hit Retry, or the live
/// feed reports new events and triggers a refresh. Each of those takes the next
/// generation, which strands every walk already in flight. A stranded walk stops
/// at its next await and writes nothing, so a slow early walk can never land its
/// stale `Resolved` on top of a newer one, and no walk touches signals whose
/// reactive arena is already disposed.
#[derive(Clone)]
struct Generations(Arc<AtomicU64>);

/// One walk's claim on the drawer, taken from [`Generations::claim`].
#[derive(Clone)]
struct Claim {
    all: Arc<AtomicU64>,
    mine: u64,
}

impl Generations {
    fn new() -> Self { Self(Arc::new(AtomicU64::new(0))) }

    /// Strand every walk in flight and claim the drawer for a new one.
    fn claim(&self) -> Claim { Claim { all: self.0.clone(), mine: self.0.fetch_add(1, Ordering::Relaxed) + 1 } }

    /// Strand every walk in flight without starting one — the drawer closed.
    fn strand_all(&self) { self.0.fetch_add(1, Ordering::Relaxed); }
}

impl Claim {
    fn is_current(&self) -> bool { self.all.load(Ordering::Relaxed) == self.mine }
}

/// Event ids for one entity, straight off the `events` store's `by_entity_id`
/// index.
///
/// HACK, retires under ankurah/ankurah#342 (with #355 for the batched form).
/// Works around `dump_entity_events` in ankurah-storage-indexeddb-wasm 0.9.0,
/// which builds its IDBKeyRange from the OWNED `EntityId → JsValue` conversion
/// — a wasm-bindgen class object, which IndexedDB rejects ("parameter is not a
/// valid key") — while rows index the base64 STRING form. Querying the index
/// for primary KEYS (base64 event-id strings, exactly as `add_event` wrote
/// them) lets the storage layer's working `get_events` do all the decoding.
///
/// Reached only when the host names its IndexedDB database; see
/// [`local_events`].
async fn local_event_ids(database: &str, entity_id: EntityId) -> Result<Vec<EventId>, String> {
    use crate::system_panel::{await_idb, js_err};
    use wasm_bindgen::{JsCast, JsValue};

    let window = env::window().ok_or("no window")?;
    let factory = window.indexed_db().map_err(js_err)?.ok_or("IndexedDB unavailable")?;
    let open = factory.open(database).map_err(js_err)?;
    let db: web_sys::IdbDatabase = await_idb(open.into()).await?.dyn_into().map_err(|_| "open did not yield a database".to_string())?;
    let result: Result<Vec<EventId>, String> = async {
        let tx = db.transaction_with_str("events").map_err(js_err)?;
        let store = tx.object_store("events").map_err(js_err)?;
        let index = store.index("by_entity_id").map_err(js_err)?;
        let key = JsValue::from_str(&entity_id.to_base64());
        let req = index.get_all_keys_with_key(&key).map_err(js_err)?;
        let keys = await_idb(req).await?;
        let arr: js_sys::Array = keys.dyn_into().map_err(|_| "getAllKeys did not return an array".to_string())?;
        let mut ids = Vec::with_capacity(arr.length() as usize);
        for key in arr.iter() {
            let s = key.as_string().ok_or("event key was not a string")?;
            ids.push(EventId::from_base64(&s).map_err(|e| format!("bad event id: {e}"))?);
        }
        Ok(ids)
    }
    .await;
    db.close();
    result
}

/// Every event this browser already holds for one entity.
///
/// `dump_entity_events` is the honest API and the path any non-IndexedDB app
/// takes. An app that names its IndexedDB database gets the index-scan
/// workaround above instead, because that one API is broken on that one engine
/// in 0.9.0.
async fn local_events(col: &StorageCollectionWrapper, entity_id: EntityId) -> Result<Vec<Attested<Event>>, String> {
    match host().indexeddb_database() {
        Some(database) => {
            let ids = local_event_ids(&database, entity_id).await?;
            col.get_events(ids).await.map_err(|e| e.to_string())
        }
        None => col.dump_entity_events(entity_id).await.map_err(|e| e.to_string()),
    }
}

/// Walk one entity's history. `None` means this walk was stranded — the drawer
/// closed, or a newer walk claimed it — and the caller must write nothing.
async fn fetch_history(target: InspectTarget, claim: Claim) -> Option<Phase> {
    let InspectTarget { collection, entity_id } = target;
    let host = host();

    // Ask the application what this entity currently says. That supplies the
    // authoritative in-memory head, the LWW provenance the DAG cross-references,
    // the "Current values" card, and any app-side refusal to show the history
    // at all — community hides deleted messages from non-moderators this way.
    // A collection the app does not know resolves to nothing and the drawer
    // still draws the DAG. See `XRayHost::resolve` (retires under ankurah#362).
    let resolved = match host.resolve(&collection, entity_id).await {
        Ok(resolved) => resolved,
        Err(e) => return Some(Phase::Failed(format!("Could not load entity: {}", e))),
    };
    if !claim.is_current() {
        return None;
    }
    if let Some(reason) = resolved.refusal {
        return Some(Phase::Refused(reason));
    }
    let mut head: Option<Clock> = resolved.head;
    let lww_current: HashMap<EventId, Vec<String>> = resolved.wrote_current;
    let current_state: Vec<(String, String)> = resolved.values;

    let col = match host.context().collection(&collection).await {
        Ok(col) => col,
        Err(e) => return Some(Phase::Failed(format!("Could not open collection: {}", e))),
    };
    if !claim.is_current() {
        return None;
    }

    // 1) Everything already local, hydrated through the storage layer's
    //    `get_events` path.
    let dumped = match local_events(&col, entity_id).await {
        Ok(events) => events,
        Err(e) => return Some(Phase::Failed(format!("Local event dump failed: {}", e))),
    };
    if !claim.is_current() {
        return None;
    }

    // Fall back to the locally-stored state head if the app resolved none.
    if head.is_none() {
        head = col.get_state(entity_id).await.ok().map(|s| s.payload.state.head);
        if !claim.is_current() {
            return None;
        }
    }

    struct Rec {
        event: Event,
        attestations: Option<usize>,
        fetched: bool,
    }
    let mut known: HashMap<EventId, Rec> = HashMap::new();
    for attested in dumped {
        known.insert(
            attested.payload.id(),
            Rec { attestations: Some(attested.attestations.0.len()), fetched: false, event: attested.payload },
        );
    }
    let local_count = known.len();

    // 2) Walk backwards from the head over parents we don't have locally,
    // asking the host for each one. A host built on `CachedEventGetter` checks
    // local storage, then asks a durable peer (server-side `check_read_event`
    // applies) and persists what it fetches — one request per event, fine at
    // single-entity scale. ankurah#355 is the batched replacement.
    let mut frontier: VecDeque<EventId> = VecDeque::new();
    let mut seen: HashSet<EventId> = known.keys().cloned().collect();
    let enqueue_parents = |event: &Event, seen: &mut HashSet<EventId>, frontier: &mut VecDeque<EventId>| {
        for parent in event.parent.iter() {
            if seen.insert(parent.clone()) {
                frontier.push_back(parent.clone());
            }
        }
    };
    for rec in known.values() {
        for parent in rec.event.parent.iter() {
            if !seen.contains(parent) {
                seen.insert(parent.clone());
                frontier.push_back(parent.clone());
            }
        }
    }
    if let Some(head) = &head {
        for tip in head.iter() {
            if seen.insert(tip.clone()) {
                frontier.push_back(tip.clone());
            }
        }
    }

    let mut unresolved = 0usize;
    if !frontier.is_empty() {
        let mut fetched_ids: Vec<EventId> = Vec::new();
        while let Some(id) = frontier.pop_front() {
            // Drawer closed, or a newer walk started → stop. Nothing will read
            // what this walk produces, and the point is not to keep issuing
            // per-event fetches in the background.
            if !claim.is_current() {
                return None;
            }
            if known.len() >= FETCH_CAP {
                unresolved += 1 + frontier.len();
                break;
            }
            match host.fetch_remote_event(&collection, &id).await {
                Ok(event) => {
                    enqueue_parents(&event, &mut seen, &mut frontier);
                    fetched_ids.push(id.clone());
                    known.insert(id, Rec { event, attestations: None, fetched: true });
                }
                Err(_) => unresolved += 1,
            }
        }
        // A host that persisted what it fetched (`CachedEventGetter` does) can
        // have the attestation counts read back in one batched local read; a
        // host that did not leaves them "unknown", which the detail pane says.
        if !fetched_ids.is_empty()
            && let Ok(attested) = col.get_events(fetched_ids).await
        {
            for a in attested {
                let count = a.attestations.0.len();
                if let Some(rec) = known.get_mut(&a.payload.id()) {
                    rec.attestations = Some(count);
                }
            }
        }
        if !claim.is_current() {
            return None;
        }
    }

    if known.is_empty() {
        return Some(Phase::Failed("No events found for this entity (nothing stored locally and no peer had it).".to_string()));
    }

    // Head fallback of last resort: tips = events that are no known event's parent.
    let head = head.unwrap_or_else(|| {
        let mut parented: HashSet<EventId> = HashSet::new();
        for rec in known.values() {
            parented.extend(rec.event.parent.iter().cloned());
        }
        Clock::new(known.keys().filter(|id| !parented.contains(*id)).cloned().collect::<Vec<_>>())
    });

    let total = known.len();
    let fetched = known.values().filter(|r| r.fetched).count();
    let inputs: Vec<DagNodeInput> = known
        .into_values()
        .map(|rec| {
            let id = rec.event.id();
            DagNodeInput {
                badges: op_badges(&rec.event),
                wrote_current: lww_current.get(&id).cloned().unwrap_or_default(),
                parent: rec.event.parent.clone(),
                attestations: rec.attestations,
                fetched: rec.fetched,
                id,
            }
        })
        .collect();

    Some(Phase::Ready(History { dag: layout(inputs, &head), head, total, local: local_count, fetched, unresolved, current_state }))
}

/// The drawer itself. Fetches on open; refetches (cheap — local) when the
/// live feed shows new events for this entity while the drawer is open.
#[component]
pub fn XRayInspector(target: InspectTarget) -> impl IntoView {
    let phase = RwSignal::new(Phase::Loading);
    let selected = RwSignal::new(None::<EventId>);

    // See `Generations`: every walk takes the next one, unmount strands them
    // all, and only the walk holding the current generation may write.
    let generations = Generations::new();
    on_cleanup({
        let generations = generations.clone();
        move || generations.strand_all()
    });
    let fetch_target = target.clone();
    let fetch_generations = generations.clone();
    let run_fetch = move || {
        let target = fetch_target.clone();
        let claim = fetch_generations.claim();
        crate::env::spawn_local(async move {
            let Some(result) = fetch_history(target, claim.clone()).await else { return };
            if !claim.is_current() {
                return;
            }
            // On first load, select the newest head tip so the detail pane
            // opens on the latest change rather than empty. A later refetch
            // (new events) leaves an existing selection alone. `try_` on both:
            // the signals live in a reactive arena the drawer's owner can have
            // disposed while this walk was awaiting.
            if let Phase::Ready(h) = &result
                && matches!(selected.try_get_untracked(), Some(None))
                && let Some(tip) = h.head.iter().next()
            {
                let _ = selected.try_set(Some(tip.clone()));
            }
            let _ = phase.try_set(result);
        });
    };
    run_fetch();

    // Live append: when the feed reports events for this entity, refresh.
    // The events were just persisted locally by the applier, so this re-runs
    // the local dump — no extra network. A burst of changesets can start
    // several refreshes; each one strands the last, so however they interleave
    // only the newest walk's result is the one that lands.
    let feed = crate::bus::bus().feed();
    let watched_id = target.entity_id;
    let last_seen = StoredValue::new(None::<u64>);
    let refetch = run_fetch.clone();
    let retry_fetch = run_fetch.clone();
    Effect::new(move |_| {
        let newest = feed.with(|entries| entries.iter().find(|e| e.entity_id == Some(watched_id)).map(|e| e.seq));
        if let Some(seq) = newest {
            let is_new = last_seen.get_value().map(|prev| seq > prev).unwrap_or(true);
            last_seen.set_value(Some(seq));
            if is_new && matches!(phase.get_untracked(), Phase::Ready(_)) {
                refetch();
            }
        }
    });

    // `close` captures nothing, so it's Copy — reuse it freely.
    let close = move || state().inspect.set(None);
    let close_scrim = close;
    let close_button = close;

    // Escape closes the drawer (scrim click and × also work). The listener is
    // document-level and CONSUMING (preventDefault) because the drawer is the
    // innermost dismissable surface: an app's own window-level Escape handling
    // should skip events the drawer already took. Document listeners run before
    // window ones, so one press closes only the drawer.
    let escape = env::on_document_keydown(move |e: web_sys::KeyboardEvent| {
        if e.key() == "Escape" && !e.default_prevented() {
            e.prevent_default();
            state().inspect.set(None);
        }
    });
    on_cleanup(move || drop(escape));

    let collection_label = target.collection.to_string();
    let id_full = target.entity_id.to_base64();
    let id_short = target.entity_id.to_base64_short();

    view! {
        <div class="xrayDrawerScrim" on:click=move |_| close_scrim()>
            <aside
                class="xrayDrawer"
                role="dialog"
                aria-label="Entity X-ray"
                on:click=|e| e.stop_propagation()
            >
                <div class="xrayDrawerHeader">
                    <div>
                        <h2 class="xrayTitle">"Entity X-ray"</h2>
                        <p class="xrayDrawerSub">
                            <span class="xrayChip xrayChipCollection">{collection_label}</span>
                            <span class="xrayMono xraySelectAll" title=id_full.clone()>{id_short}</span>
                        </p>
                    </div>
                    <button class="xrayClose" aria-label="Close inspector" on:click=move |_| close_button()>"×"</button>
                </div>

                {move || match phase.get() {
                    Phase::Loading => view! {
                        <div class="xrayStateNote">"Loading event history…"</div>
                    }.into_any(),
                    Phase::Refused(reason) => view! {
                        <div class="xrayStateNote xrayRefused">
                            <strong>"Not shown. "</strong>
                            {reason}
                        </div>
                    }.into_any(),
                    Phase::Failed(error) => {
                        let retry = retry_fetch.clone();
                        view! {
                            <div class="xrayStateNote xrayError">{error}</div>
                            <button class="xrayInspectGo" on:click=move |_| { phase.set(Phase::Loading); retry(); }>
                                "Retry"
                            </button>
                        }.into_any()
                    }
                    Phase::Ready(history) => {
                        let tips: Vec<String> = history.head.iter().map(|id| id.to_base64_short()).collect();
                        let concurrent = tips.len() > 1;
                        let provenance = if history.fetched > 0 {
                            format!("{} events · {} local · {} fetched from peer", history.total, history.local, history.fetched)
                        } else {
                            format!("{} events · all local", history.total)
                        };
                        let current_state = history.current_state.clone();
                        view! {
                            <div class="xrayDrawerBody">
                                {(!current_state.is_empty()).then(|| view! {
                                    <section class="xrayCard">
                                        <h3 class="xrayCardTitle">"Current values"</h3>
                                        {current_state.into_iter().map(|(k, v)| view! {
                                            <div class="xrayDetailRow xrayValueRow">
                                                <span class="xrayMetaLabel">{k}</span>
                                                <span class="xrayValue">{v}</span>
                                            </div>
                                        }).collect_view()}
                                    </section>
                                })}
                                <div class="xrayMetaRow">
                                    <span class="xrayMetaLabel">"head"</span>
                                    <span class="xrayHeadChips" class:xrayHeadConcurrent=concurrent>
                                        {tips.into_iter().map(|tip| view! {
                                            <span class="xrayChip xrayMono">{tip}</span>
                                        }).collect_view()}
                                    </span>
                                    {concurrent.then(|| view! {
                                        <span class="xrayConcurrencyNote">"2+ tips — concurrent edits, not yet merged"</span>
                                    })}
                                </div>
                                <p class="xrayProvenance">
                                    {provenance}
                                    {(history.unresolved > 0)
                                        .then(|| format!(" · {} ancestor(s) unavailable", history.unresolved))}
                                </p>
                                <DagView model=history.dag.clone() selected />
                                <NodeDetail dag=history.dag selected />
                                <p class="xrayFootnote">
                                    "yrs deltas decoded in-app; LWW payloads are opaque client-side until ankurah#337. Events carry no author or wall-clock — that metadata is #337 piece 3."
                                </p>
                            </div>
                        }.into_any()
                    }
                }}
            </aside>
        </div>
    }
}

/// Raw-event detail for the selected DAG node.
#[component]
fn NodeDetail(dag: DagModel, selected: RwSignal<Option<EventId>>) -> impl IntoView {
    move || {
        let Some(id) = selected.get() else {
            return view! { <p class="xrayDetailHint">"Select a node to see the raw event."</p> }.into_any();
        };
        let Some(node) = dag.nodes.iter().find(|n| n.input.id == id) else {
            return view! { <p class="xrayDetailHint">"Select a node to see the raw event."</p> }.into_any();
        };
        let input = node.input.clone();
        let attestation_line = match input.attestations {
            // Report the count, not a reading of it. Whether zero is routine or
            // a signal depends on the app's policy agent, which the panel does
            // not know: Community's JwtAgent attests nothing at all today
            // (`check_event → Ok(None)`), so zero is its normal value, while an
            // app whose agent does attest would want to look twice.
            Some(0) => "0 (no attestations recorded)".to_string(),
            Some(n) => format!("{}", n),
            None => "unknown (payload fetched without attestations)".to_string(),
        };
        view! {
            <div class="xrayDetail">
                <div class="xrayDetailRow">
                    <span class="xrayMetaLabel">"event"</span>
                    <span class="xrayMono xraySelectAll">{input.id.to_base64()}</span>
                </div>
                <div class="xrayDetailRow">
                    <span class="xrayMetaLabel">"parents"</span>
                    {if input.parent.is_empty() {
                        view! { <span class="xrayChip">"none — creation event"</span> }.into_any()
                    } else {
                        input.parent.iter().map(|p| view! {
                            <span class="xrayChip xrayMono" title=p.to_base64()>{p.to_base64_short()}</span>
                        }).collect_view().into_any()
                    }}
                </div>
                <div class="xrayDetailRow">
                    <span class="xrayMetaLabel">"ops"</span>
                    <span class="xrayDetailOps">
                        {input.badges.iter().map(|b| view! {
                            <span class="xrayChip">
                                <span class="xrayBackendTag">{b.backend.clone()}</span>
                                {format!(" {} · {} op(s) · {} B", b.summary, b.op_count, b.bytes)}
                            </span>
                        }).collect_view()}
                    </span>
                </div>
                <div class="xrayDetailRow">
                    <span class="xrayMetaLabel">"attested"</span>
                    <span>{attestation_line}</span>
                </div>
                <div class="xrayDetailRow">
                    <span class="xrayMetaLabel">"source"</span>
                    // Not "cached locally": Community's `CachedEventGetter`
                    // guaranteed persistence, `XRayHost::fetch_remote_event`
                    // does not, and the panel cannot see which it got.
                    <span>{if input.fetched { "fetched via host" } else { "local storage" }}</span>
                </div>
                {(!input.wrote_current.is_empty()).then(|| view! {
                    <div class="xrayDetailRow">
                        <span class="xrayMetaLabel">"wrote current"</span>
                        <span>{input.wrote_current.join(", ")} " (LWW values still standing)"</span>
                    </div>
                })}
            </div>
        }
        .into_any()
    }
}
