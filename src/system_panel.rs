//! The L2 system panel: a non-blocking right slide-over with four cards —
//! this node, connection & peers, registered live queries, and the live
//! event feed. The app stays fully usable while it's open (that's the demo
//! point: kill the server and watch the connection card narrate the reconnect
//! while the app keeps working).

use leptos::prelude::*;
use wasm_bindgen::{JsCast, JsValue};

use ankurah::proto::EntityId;
use ankurah_signals::{Get as AnkurahGet, With as AnkurahWith};

use crate::bus::{QuerySnapshot, bus};
use crate::env;
use crate::feed::FeedCard;
use crate::host::host;
use crate::state;

#[component]
pub fn SystemPanel() -> impl IntoView {
    // The panel's × IS the off switch — x-ray is one mode, not a panel plus a
    // residue of chips (the dismiss-panel-only half-state read as "stuck on").
    let close = move |_| state().set_enabled(false);

    // Drag-to-move: grab the header to reposition the panel so it's not stuck
    // over the thing you want to inspect. `pos` None = the default top/right
    // anchor; Some = a viewport-clamped top-left the drag sets. `grab` holds
    // the pointer's offset within the panel while a drag is live.
    let header_ref = NodeRef::<leptos::html::Div>::new();
    let pos = RwSignal::new(None::<(f64, f64)>);
    let grab = RwSignal::new(None::<(f64, f64)>);

    let panel_rect = move || header_ref.get_untracked().and_then(|h| h.parent_element()).map(|p| p.get_bounding_client_rect());

    let on_pointer_down = move |ev: web_sys::PointerEvent| {
        if ev.button() != 0 {
            return;
        }
        // Don't start a drag from the close button.
        if let Some(t) = ev.target().and_then(|t| t.dyn_into::<web_sys::Element>().ok())
            && t.closest("button").ok().flatten().is_some()
        {
            return;
        }
        if let Some(rect) = panel_rect() {
            grab.set(Some((ev.client_x() as f64 - rect.left(), ev.client_y() as f64 - rect.top())));
            pos.set(Some((rect.left(), rect.top())));
            if let Some(h) = header_ref.get_untracked() {
                let _ = h.set_pointer_capture(ev.pointer_id());
            }
        }
    };
    let on_pointer_move = move |ev: web_sys::PointerEvent| {
        let Some((gx, gy)) = grab.get_untracked() else { return };
        let Some(rect) = panel_rect() else { return };
        let Some(win) = env::window() else { return };
        let iw = win.inner_width().ok().and_then(|v| v.as_f64()).unwrap_or(rect.width());
        let ih = win.inner_height().ok().and_then(|v| v.as_f64()).unwrap_or(rect.height());
        let x = (ev.client_x() as f64 - gx).clamp(0.0, (iw - rect.width()).max(0.0));
        let y = (ev.client_y() as f64 - gy).clamp(0.0, (ih - rect.height()).max(0.0));
        pos.set(Some((x, y)));
    };
    let on_pointer_up = move |_ev: web_sys::PointerEvent| grab.set(None);
    let reset_pos = move |_ev: web_sys::MouseEvent| pos.set(None);

    let panel_style = move || match pos.get() {
        Some((x, y)) => {
            format!("left:{x}px;top:{y}px;right:auto;bottom:auto;height:calc(100dvh - 128px);")
        }
        None => String::new(),
    };

    view! {
        <aside class="xrayPanel" role="complementary" aria-label="X-ray system panel" style=panel_style>
            <div
                node_ref=header_ref
                class="xrayPanelHeader"
                class:xrayPanelHeaderDragging=move || grab.get().is_some()
                on:pointerdown=on_pointer_down
                on:pointermove=on_pointer_move
                on:pointerup=on_pointer_up
                on:dblclick=reset_pos
                title="Drag to move · double-click to reset"
            >
                <div>
                    <h2 class="xrayTitle">"X-ray"</h2>
                    <p class="xrayPanelSub">"live node internals · Alt+X"</p>
                </div>
                <button class="xrayClose" aria-label="Close X-ray panel" on:click=close>"×"</button>
            </div>

            <div class="xrayPanelBody">
                <InspectEntityRow />
                <NodeCard />
                <ConnectionCard />
                <QueriesCard />
                <FeedCard />
            </div>
        </aside>
    }
}

/// Entity-id input → open the L1 inspector directly, for when the app has no
/// click-to-inspect affordance on the element you care about.
#[component]
fn InspectEntityRow() -> impl IntoView {
    // The collection list is the app's, because ankurah 0.9.0 cannot enumerate
    // collections or resolve an EntityId to one (ankurah#362 asks 1 and 3 —
    // when either lands this select becomes unnecessary, since the id alone
    // names the collection).
    let collections: Vec<String> = host().collections().iter().map(|c| c.to_string()).collect();
    let first = collections.first().cloned().unwrap_or_default();

    let id_input = RwSignal::new(String::new());
    let collection = RwSignal::new(first);
    let error = RwSignal::new(None::<String>);

    let submit = move || {
        let raw = id_input.get_untracked();
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return;
        }
        match EntityId::from_base64(trimmed) {
            Ok(id) => {
                error.set(None);
                state().open_inspector(collection.get_untracked().as_str().into(), id);
            }
            Err(e) => error.set(Some(format!("not a valid entity id: {}", e))),
        }
    };
    let submit_click = submit; // Copy closure (captures only Copy signals)

    view! {
        <div class="xrayInspectRow">
            <select
                class="xrayInspectSelect"
                aria-label="Collection"
                on:change=move |ev| collection.set(event_target_value(&ev))
            >
                {collections
                    .into_iter()
                    .map(|name| view! { <option value=name.clone()>{name.clone()}</option> })
                    .collect_view()}
            </select>
            <input
                class="xrayInspectInput xrayMono"
                type="text"
                placeholder="entity id (base64) — paste to inspect"
                prop:value=move || id_input.get()
                on:input=move |ev| id_input.set(event_target_value(&ev))
                on:keydown=move |ev| { if ev.key() == "Enter" { submit(); } }
            />
            <button class="xrayInspectGo" on:click=move |_| submit_click()>"Inspect"</button>
            {move || error.get().map(|e| view! { <p class="xrayStateNote xrayError">{e}</p> })}
        </div>
    }
}

/// Card 1: the local node — identity, durability, policy, system, storage.
#[component]
fn NodeCard() -> impl IntoView {
    let node_id = host().context().node_id();
    let crate::NodeStatus { durable, policy_ready, system_ready, system_root_head } = host().node_status();

    // Storage counts via raw IndexedDB `count()`.
    //
    // HACK, retires under ankurah/ankurah#357 item 2 (storage stats API).
    // `StorageCollection` reports no counts in 0.9.0, so this reads the two
    // object stores the wasm engine maintains, by name, from the database the
    // host names. Labeled "local cache": it counts what this browser has
    // synced, not the system's total. An app that names no IndexedDB database
    // simply has no counts to show.
    let database = host().indexeddb_database();
    let counts = RwSignal::new(None::<Result<(f64, f64), String>>);
    let refresh = {
        let database = database.clone();
        move || {
            let Some(database) = database.clone() else { return };
            env::spawn_local(async move {
                let result = idb_counts(&database).await;
                // `try_`: closing the panel mid-count disposes this signal's
                // reactive arena, and the count is then nobody's business.
                let _ = counts.try_set(Some(result));
            });
        }
    };
    let recount = refresh.clone();
    refresh();

    view! {
        <section class="xrayCard">
            <h3 class="xrayCardTitle">"This node"</h3>
            <div class="xrayDetailRow">
                <span class="xrayMetaLabel">"node"</span>
                <span class="xrayMono xraySelectAll" title=node_id.to_base64()>{node_id.to_base64_short()}</span>
            </div>
            <div class="xrayDetailRow">
                <span class="xrayMetaLabel">"role"</span>
                // No chip when the host reported no durability: a host that
                // stayed silent is not thereby an ephemeral one.
                {durable.map(|durable| view! {
                    <span class="xrayChip">{if durable { "durable" } else { "ephemeral" }}</span>
                })}
                {policy_ready.map(|ready| view! {
                    <span class="xrayChip" class:xrayChipOk=ready>
                        {if ready { "policy ready" } else { "policy syncing…" }}
                    </span>
                })}
                {system_ready.map(|ready| view! {
                    <span class="xrayChip" class:xrayChipOk=ready>
                        {if ready { "system ready" } else { "joining system…" }}
                    </span>
                })}
            </div>
            <div class="xrayDetailRow">
                <span class="xrayMetaLabel">"system root"</span>
                {match system_root_head {
                    Some(head) => view! { <span class="xrayMono">{head}</span> }.into_any(),
                    None => view! { <span class="xrayFaint">"none"</span> }.into_any(),
                }}
            </div>
            {database.map(|_| view! {
                <div class="xrayDetailRow">
                    <span class="xrayMetaLabel">"local cache"</span>
                    {move || match counts.get() {
                        None => view! { <span class="xrayFaint">"counting…"</span> }.into_any(),
                        Some(Ok((entities, events))) => view! {
                            <span>{format!("{} entities · {} events", entities, events)}</span>
                        }.into_any(),
                        Some(Err(e)) => view! { <span class="xrayFaint" title=e>"unavailable"</span> }.into_any(),
                    }}
                    <button class="xrayMiniButton" title="Recount" on:click=move |_| recount()>"↻"</button>
                </div>
            })}
        </section>
    }
}

/// Card 2: connection & peers — the app's connection-state signal rendered as
/// a status line plus a transition log, and the durable peer set.
///
/// HACK, retires under ankurah/ankurah#357 item 1. The `ConnectionState` enum
/// lives in a private module of ankurah-websocket-client-wasm 0.9.0, so nothing
/// here can match its variants or reach the `Presence` payload — only the strum
/// `Display` name survives, which is why the panel renders strings the app's
/// signal produced (see `crate::XRay::connection` and `bus::start_connection_log`).
/// Server identity therefore comes from the host's `durable_peers()` (the
/// durable peer *is* the server) and the endpoint from the string the app
/// passed in. Structured presence returns when upstream re-exports the enum.
#[component]
fn ConnectionCard() -> impl IntoView {
    let conn_log = bus().conn_log();
    let endpoint = crate::host::connection().map(|c| c.endpoint.clone());

    // Reading the state signal here makes the whole card re-render on every
    // transition, which is also what re-lists the peers. Captures nothing, so
    // the closure is Copy and can be reused freely below.
    let current = move || {
        let line = bus().conn_state().get();
        if line.is_empty() { "not reported".to_string() } else { line }
    };
    let status_class = move || match current().as_str() {
        "Connected" => "xrayConnState xrayConnOk",
        "Closed" | "Error" => "xrayConnState xrayConnBad",
        _ => "xrayConnState",
    };

    let peers = move || {
        // Re-list peers whenever the connection transitions.
        let _ = current();
        host().durable_peers()
    };

    view! {
        <section class="xrayCard">
            <h3 class="xrayCardTitle">"Connection & peers"</h3>
            <div class="xrayDetailRow">
                <span class="xrayMetaLabel">"state"</span>
                <span class=status_class>{current}</span>
                <span class="xrayFaint xrayMono">{endpoint}</span>
            </div>
            <div class="xrayDetailRow">
                <span class="xrayMetaLabel">"durable peers"</span>
                {move || {
                    let list = peers();
                    if list.is_empty() {
                        view! { <span class="xrayFaint">"none connected"</span> }.into_any()
                    } else {
                        list.into_iter()
                            .map(|peer| view! {
                                <span class="xrayChip xrayMono" title=peer.to_base64()>{peer.to_base64_short()}</span>
                            })
                            .collect_view()
                            .into_any()
                    }
                }}
            </div>
            <div class="xrayConnLog">
                <span class="xrayMetaLabel">"transitions"</span>
                <ol class="xrayConnLogList">
                    <For
                        each=move || conn_log.get()
                        key=|(ts, line)| (ts.to_bits(), line.clone())
                        children=|(ts, line)| {
                            let stamp = env::clock_hms(ts);
                            view! {
                                <li class="xrayConnLogRow">
                                    <span class="xrayFeedTime xrayMono">{stamp}</span>
                                    <span>{line}</span>
                                </li>
                            }
                        }
                    />
                </ol>
            </div>
        </section>
    }
}

/// Card 3: the LiveQuery registry. Honestly labeled: these are the queries the
/// app registered with the x-ray bus, not the node's full reactor table (which
/// is `pub(crate)` in ankurah 0.9.0). Retires under ankurah#361 along with the
/// bus itself, at which point the card lists every query the node holds.
#[component]
fn QueriesCard() -> impl IntoView {
    let handle = bus();
    let rev = handle.entries_rev();
    let snapshot = move || {
        let _ = rev.get(); // re-snapshot on register/unregister
        bus().snapshot()
    };
    let snapshot_for_empty = snapshot.clone();

    view! {
        <section class="xrayCard">
            <h3 class="xrayCardTitle">"Live queries"</h3>
            <p class="xrayCardSub">"queries this app holds and registered with x-ray"</p>
            <Show when=move || snapshot_for_empty().is_empty()>
                <p class="xrayStateNote">"No queries registered."</p>
            </Show>
            <For
                each=snapshot
                key=|q: &QuerySnapshot| q.id
                children=move |q: QuerySnapshot| {
                    let QuerySnapshot { label, query_id, collection, selection, resultset, error, changes_seen, .. } = q;
                    let resultset_len = resultset.clone();
                    let loaded = resultset;
                    let selection_text = {
                        let selection = selection.clone();
                        move || {
                            let (sel, version) = selection.get();
                            format!("{} · v{}", sel, version)
                        }
                    };
                    view! {
                        <div class="xrayQueryRow">
                            <div class="xrayQueryHead">
                                <span class="xrayQueryLabel">{label}</span>
                                <span class="xrayChip xrayMono" title="query id">{query_id.to_string()}</span>
                                <span class="xrayChip">{collection.to_string()}</span>
                            </div>
                            <code class="xraySelection">{selection_text}</code>
                            <div class="xrayQueryStats">
                                <span>{move || format!("{} results", resultset_len.len())}</span>
                                <span>{move || if loaded.is_loaded() { "loaded" } else { "loading…" }}</span>
                                <span>{move || format!("{} changesets", changes_seen.get())}</span>
                                {move || {
                                    // RetrievalError isn't Clone, so read it in place.
                                    error
                                        .with(|e| e.as_ref().map(|e| e.to_string()))
                                        .map(|msg| view! { <span class="xrayError">{format!("error: {}", msg)}</span> })
                                }}
                            </div>
                        </div>
                    }
                }
            />
        </section>
    }
}

// ---------------------------------------------------------------------------
// Raw IndexedDB. HACK, retires under ankurah/ankurah#357 item 2 (a storage
// stats API) — and the shared helpers below also serve `inspector`'s index
// scan, which retires under ankurah#342. Carried across from Community as-is:
// reaching around the storage engine to its object stores by name is exactly
// as fragile as it looks, and the fix is upstream, not here.
// ---------------------------------------------------------------------------

pub(crate) fn js_err(e: JsValue) -> String { format!("{:?}", e) }

/// Wrap an IDBRequest's success/error events in a JS Promise. One-shot
/// closures pass ownership to the JS GC (`once_into_js`), so nothing leaks.
fn idb_request_promise(req: web_sys::IdbRequest) -> js_sys::Promise {
    js_sys::Promise::new(&mut |resolve, reject| {
        let req_ok = req.clone();
        let ok = wasm_bindgen::closure::Closure::once_into_js(move |_: web_sys::Event| {
            let _ = resolve.call1(&JsValue::NULL, &req_ok.result().unwrap_or(JsValue::NULL));
        });
        req.set_onsuccess(Some(ok.unchecked_ref()));
        let err = wasm_bindgen::closure::Closure::once_into_js(move |_: web_sys::Event| {
            let _ = reject.call1(&JsValue::NULL, &JsValue::from_str("IndexedDB request failed"));
        });
        req.set_onerror(Some(err.unchecked_ref()));
    })
}

pub(crate) async fn await_idb(req: web_sys::IdbRequest) -> Result<JsValue, String> {
    wasm_bindgen_futures::JsFuture::from(idb_request_promise(req)).await.map_err(js_err)
}

/// Count the `entities` / `events` object stores of the database the host
/// named. Store names per ankurah-storage-indexeddb-wasm 0.9.0 `database.rs`.
async fn idb_counts(database: &str) -> Result<(f64, f64), String> {
    let window = env::window().ok_or("no window")?;
    let factory = window.indexed_db().map_err(js_err)?.ok_or("IndexedDB unavailable")?;
    let open = factory.open(database).map_err(js_err)?;
    let db: web_sys::IdbDatabase = await_idb(open.unchecked_into()).await?.dyn_into().map_err(|_| "unexpected open result".to_string())?;

    // One exit discipline, same shape as `inspector::local_event_ids`: every
    // post-open step runs in the inner block, and the connection is closed on
    // the way out however that block ended. An open IndexedDB connection that
    // an error path abandoned would accumulate per recount and can block a
    // later schema-version upgrade.
    let result: Result<(f64, f64), String> = async {
        let stores = js_sys::Array::of2(&"entities".into(), &"events".into());
        let tx = db.transaction_with_str_sequence_and_mode(&stores, web_sys::IdbTransactionMode::Readonly).map_err(js_err)?;
        // Issue both counts before awaiting: an IDB transaction auto-commits once
        // control returns to the event loop with no requests pending.
        let entities_req = tx.object_store("entities").map_err(js_err)?.count().map_err(js_err)?;
        let events_req = tx.object_store("events").map_err(js_err)?.count().map_err(js_err)?;
        let entities = await_idb(entities_req).await?.as_f64().ok_or("count was not a number")?;
        let events = await_idb(events_req).await?.as_f64().ok_or("count was not a number")?;
        Ok((entities, events))
    }
    .await;
    db.close();
    result
}
