# ankurah-xray

A live inspector panel for [Ankurah](https://github.com/ankurah/ankurah)
applications: per-entity event DAGs, head clocks, live-query traffic, and node
internals, drawn over a running app that stays usable while you look at it.

This is [Ankurah Community](https://github.com/ankurah/community)'s in-app
x-ray mode ([community#39](https://github.com/ankurah/community/issues/39))
extracted into its own crate. That extraction is a step on a route already
written down: [community#53](https://github.com/ankurah/community/issues/53)
records the trajectory *embedded sub-application → separate library → possibly
a devtools browser extension*, and the destination — an inspector attachable to
an ankurah app that never had to know it was being inspected. The mental model
throughout is React/Redux DevTools.

**Status: local, pre-publication.** No remote, not on crates.io, no stability
promise. The public API here is shaped to shrink, not to be depended on.

---

## Where this is going, and what it costs today

community#53 names exactly two things a *finished* consuming app does:

1. put a **`data-entity-id`** attribute on every x-rayable DOM element, and
2. **load the panel**.

Everything else is supposed to come from ankurah node APIs. Today it cannot, so
today's integration is longer than that. The gap is not one trait; it is five
obligations, and every one of them is scheduled to disappear.

**The integration checklist, as of ankurah 0.9.0:**

| Step | Retires when |
| --- | --- |
| 1. Implement `XRayHost` and `install()` it once at startup | the trait empties out (see the table below) |
| 2. Mount `<XRayLauncher />` inside the ankurah-context subtree | never — this is accommodation 2 |
| 3. Link `xray.css` (and optionally `xray-tokens.css`) | never |
| 4. Put `.xrayInspectable` / `.xrayConcurrent` on x-rayable elements, and own the click handler and the multi-tip-head check yourself | accommodation 1: `data-entity-id` lets the panel install its own handlers and set its own classes |
| 5. `register` every long-lived query with the bus, and `unregister` it when its owner goes away | [ankurah#361](https://github.com/ankurah/ankurah/issues/361) — the node enumerates its own queries |

Skip 4 and 5 and the panel still works: it toggles, it draws the node,
connection and storage cards, and the inspect-by-id row opens any entity. What
you lose is element affordances and the live query feed.

`XRayHost` is the biggest of the five and the whole point of it is to
disappear. Read it as a ledger of missing upstream API: each method names the
issue that deletes it, and when the last one lands the trait is empty and goes
away.

## Mounting

```rust
use ankurah_xray::{XRay, XRayLauncher};

// Once, during app startup, before Leptos mounts. The panel is a
// process-global singleton and the first installation wins, so a rejected
// install means something else already owns the panel — worth knowing about
// rather than dropping.
XRay::new(MyHost { ctx: ctx() })
    .connection(ws_client().connection_state(), ws_url()) // optional
    .install()
    .expect("install ankurah-xray once");

// Once, inside the subtree where the ankurah context is live.
view! { <XRayLauncher /> }
```

`XRayLauncher` renders no chrome of its own. Alt+X toggles the panel from
anywhere; wire your own button to `ankurah_xray::state().toggle()` if you want
a visible one. `?xray=1` and `localStorage["xray"]` turn it on at load. The
launcher owns the observation machinery: unmounting it stops the query taps and
drops the connection subscription, and does *not* forget the visitor's choice —
remount it and observation resumes.

Link the stylesheets. `xray-tokens.css` is optional — skip it if your app
already defines the design tokens `xray.css` reads (they are all listed at the
top of the tokens file, including `--xray-panel-z` and `--xray-drawer-z`, which
are where you fit the panel and the drawer into your own stacking order):

```html
<link rel="stylesheet" href="ankurah-xray/css/xray-tokens.css" />
<link rel="stylesheet" href="ankurah-xray/css/xray.css" />
```

Three classes let the panel style your own elements while x-ray is on:

- `.xrayInspectable` — this element opens the inspector when clicked. **You own
  the handler**; call `ankurah_xray::state().open_inspector(collection, id)`.
- `.xrayConcurrent` — this element's entity has a multi-tip head right now.
  **You own the check** against the entity's head.
- `.xrayInspectCursor` — optional, for text-bearing content *inside* an
  inspectable element. `cursor` inherits, and prose usually sets `cursor: text`,
  which leaves most of the element showing a caret instead of the zoom
  affordance; put this on those spans. The panel deliberately ships no blanket
  descendant rule, which would steal the pointer cursor from nested links and
  buttons.

All three retire under accommodation 1, when the panel installs its own
handlers driven by `data-entity-id`.

## The trait an app implements

```rust
#[async_trait(?Send)]
impl XRayHost for MyHost {
    // Required.
    fn context(&self) -> ankurah::Context;
    fn collections(&self) -> Vec<CollectionId>;
    async fn resolve(&self, collection: &CollectionId, id: EntityId) -> Result<Resolved, String>;

    // Optional — each has a default that degrades to an honest empty state.
    fn node_status(&self) -> NodeStatus;
    fn durable_peers(&self) -> Vec<EntityId>;
    async fn fetch_remote_event(&self, collection: &CollectionId, id: &EventId) -> Result<Event, String>;
    fn indexeddb_database(&self) -> Option<String>;
}
```

`resolve` is the one that carries real weight. ankurah 0.9.0 materializes an
entity's values only through a generated `View`, so the panel — compiled
against no models at all — cannot read a property of anything. The app hands
back a `Resolved`: the entity's head, `(property, value)` rows for the "Current
values" card, which event wrote each LWW value still standing (build it with
`ankurah_xray::lww_provenance`), and optionally a `refusal` — a deliberate,
app-side "do not show this one" that the drawer renders as a notice rather than
an error. Community's deleted-message gate is a `refusal`. A collection the app
does not recognize returns `Resolved::default()`, and the drawer still draws the
event DAG; only the values card goes away.

`fetch_remote_event` is where an app plugs in `CachedEventGetter` — which is
generic over `Node<SE, PA>` and the policy context data, and therefore
unnameable from here. `tests/pluggability.rs` shows the whole implementation; it
is four lines. Constructing the getter per call is equivalent to holding one:
its only state is an event staging map the panel never writes.

Register your long-lived queries so they show up in the panel and feed the live
event stream — and release them when their owner goes away:

```rust
use ankurah_xray::bus::bus;

let reg = bus().register("rooms", &rooms_query);
on_cleanup(move || bus().unregister(reg));
```

**`unregister` is your obligation, and a real one.** The bus stores a clone of
the `LiveQuery`, which is an `Arc` handle to its reactor subscription (and, for
a strong query, to the node). Turning x-ray off drops the changeset tap but not
the registration, so a component that registers and then unmounts without
unregistering has parked a live subscription in a process-global registry that
nothing else will ever clean up. The crate offers no RAII guard on purpose: the
registry is a hack retiring wholesale under ankurah#361, and a guard type would
be one more thing to delete.

## What is knowingly a hack, and what retires it

Nothing below is a design. Each is a workaround for something ankurah 0.9.0
does not expose, annotated in place at the code that does it, and each has an
upstream issue. community#53 holds the authoritative map; this is the slice of
it that lives in this crate.

| Here | Retires under |
| --- | --- |
| `bus.rs` — the entire hand-registration registry, labels, changeset taps, register/drop revision counter | [ankurah#361](https://github.com/ankurah/ankurah/issues/361) — node-level query enumeration, optional labels, untyped taps, lifecycle signal |
| `XRayHost::resolve` + `lww_provenance` — typed-view resolution for the "Current values" card and LWW provenance | [ankurah#362](https://github.com/ankurah/ankurah/issues/362) (untyped value materialization) and [ankurah#337](https://github.com/ankurah/ankurah/issues/337) (per-event values) |
| `XRayHost::collections` + the panel's collection `<select>` | [ankurah#362](https://github.com/ankurah/ankurah/issues/362) asks 1 and 3 — EntityId→CollectionId and schema discovery |
| `inspector.rs::local_event_ids` — raw-IndexedDB `by_entity_id` index scan, used only when the host names its database | [ankurah#342](https://github.com/ankurah/ankurah/issues/342) (`dump_entity_events` key-shape bug), [ankurah#355](https://github.com/ankurah/ankurah/issues/355) (batched) |
| `system_panel.rs::idb_counts` — raw-IndexedDB object-store counts | [ankurah#357](https://github.com/ankurah/ankurah/issues/357) item 2 — storage stats API |
| `XRay::connection` + `bus::start_connection_log` — a `Display`-erased connection signal, because `ConnectionState` is unnameable | [ankurah#357](https://github.com/ankurah/ankurah/issues/357) item 1 — re-export `ConnectionState` / `Presence` |
| `XRayHost::node_status` / `durable_peers` — node role, readiness, peers | [ankurah#357](https://github.com/ankurah/ankurah/issues/357) — client observability ergonomics |
| `XRayHost::fetch_remote_event` — the app builds the `CachedEventGetter` | [ankurah#355](https://github.com/ankurah/ankurah/issues/355) (batched `entity_events`) |
| `decode.rs` — hand-decoding yrs diffs; LWW payloads shown as byte sizes only, because `LWWDiff`'s fields are private | [ankurah#337](https://github.com/ankurah/ankurah/issues/337) piece 2 — per-event diff descriptions |
| `dag.rs` — client-side topo layout, because ankurah-core's `event_dag` is `pub(crate)` | no issue; the layout is a rendering concern and probably stays |
| live wire traffic — not shown at all, because it is unobservable | [ankurah#356](https://github.com/ankurah/ankurah/issues/356) — node message observer |

`src/env.rs` is not on that list and is not a hack: it is a test seam. Every
wasm-bindgen import panics — or, for closure construction, aborts — when built
for the host, so the browser touchpoints route through one module that behaves
off-wasm like a browser with no window, which is exactly the state every call
site already handles. It is what makes `cargo test` able to render the panel at
all.

## Versions

`Cargo.lock` is committed. The usual "libraries don't commit a lock" convention
is about published crates whose consumers resolve their own graph; this one is
`publish = false`, so the lock governs exactly one thing — `git clone && cargo
build` here — and it is what makes a reviewer's resolve the same as the one the
suite was run against. It constrains no consumer, which is why the version
bounds below still have to carry their own weight.

- ankurah family pinned `=0.9.0`, matching Community's app exactly.
- leptos `>=0.8.12, <=0.8.14`, with **no mode feature** — `csr` / `hydrate` /
  `ssr` is the consumer's call. The ceiling is a requirement, not a
  preference: leptos 0.8.15+ reaches server_fn → wasm-streams, which wants
  js-sys `^0.3.85`, and js-sys is pinned `=0.3.82` below.
- leptos_macro `>=0.8.12, <0.8.15`, declared and never imported. It exists to
  hold the macro crate to the runtime's ceiling. leptos_macro 0.8.17 emits
  `::leptos::__as_shared_reactive_fn` when leptos turns on its
  `__internal_erase_components` feature — which leptos does automatically under
  `--cfg erase_components` — and the leptos runtime only defines that symbol
  from 0.8.20 on. leptos's own dependency on the macro is a caret range, so it
  happily resolves the broken pair; a consumer building with
  `--cfg erase_components` then fails inside leptos's own source
  (`animated_show.rs`, `E0425`). Lift this and the leptos ceiling together.
- wasm-bindgen `=0.2.105`, web-sys `=0.3.82`, js-sys `=0.3.82`,
  wasm-bindgen-futures `=0.4.55`. Deliberate and temporary: ankurah-signals
  0.9.0 does not build against newer wasm-bindgen. Unpin when ankurah does.
- yrs `=0.24.0` — the exact version ankurah-core 0.9.0 resolves to, so this
  crate's decoder reads the same wire format the producer writes. `decode.rs`
  hands raw bytes to `Update::decode_v2` and no yrs type crosses the ankurah
  boundary, so this is about format drift between versions, not about type
  identity between two copies.
- Two `getrandom` majors appear as direct wasm-only dependencies, neither of
  them called from this crate: 0.2 with `js` (reached via ankurah's rand 0.8)
  and 0.3 with `wasm_js` (via ankql's ulid → rand 0.9). They arrive with no
  randomness backend selected and nothing else in the graph selects one; drop
  either line and the wasm build fails. getrandom 0.4 is *not* listed, because
  yrs's fastrand 2.5 already declares it with `wasm_js` under its own wasm
  target block. There is no `--cfg getrandom_backend` rustflag and no
  `.cargo/config.toml`: the feature alone selects the backend in getrandom
  0.3.4 and 0.4.3, the flag adds nothing, and flag-without-feature is the one
  configuration that *creates* the `compile_error!` it looks like it prevents.

## Building and testing

```
cargo build
cargo test                                  # headless, renders the panel via leptos SSR
cargo clippy --all-targets -- -D warnings   # clean, no allow attributes
cargo build --target wasm32-unknown-unknown # the real deployment target
cargo +nightly fmt --check                  # rustfmt.toml uses nightly-only options
```

`tests/pluggability.rs` stands up a real ankurah node on temp-dir sled storage,
defines a model this crate has never heard of, registers a live query with the
bus, resolves an entity through `XRayHost`, and renders the panel to HTML —
proof that an app other than Community can drive it. The inspector drawer's
event-DAG walk, the drag-to-move header, localStorage persistence, the Alt+X
hotkey, and the raw-IndexedDB paths need a browser and are not covered.
