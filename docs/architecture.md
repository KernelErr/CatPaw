# CatPaw architecture

This is the engineering summary. The full design discussion (in Chinese) is in
[design.zh-CN.md](design.zh-CN.md); the binding decisions are in the
[ADRs](adr/).

## Priorities

1. Headless operation that lets LLM agents browse real sites reliably.
2. JavaScript and DOM fidelity: SPAs, iframes, postMessage, fetch/XHR,
   WebSocket, storage, Web Crypto, Canvas 2D.
3. Agent ergonomics: semantic snapshots, settledness, cheap contexts.
4. Rendering on demand — correct, not pretty.
5. Performance.

Non-goals for v1: WebGL/WebGPU, media playback, extensions, WebRTC, printing,
bfcache, HTTP/3, SharedArrayBuffer.

## Product principles

CatPaw's first user is an LLM agent. Humans do two things: install with one
sentence (`catpaw setup <host>` writes the host's MCP config, or the host
connects to a hosted server) and occasionally take over.

- One step, one round trip: an action returns the settled diff snapshot and
  its consequences; batches run several actions per call.
- Token-frugal and byte-stable output: compact CST, diffs, cursors, and
  deterministic ordering so prompt caches hit.
- Failures name their cause (in-flight requests, occluding elements,
  blocking dialogs); nothing ever hangs, dialogs and downloads are events.
- Persistent profiles: a human logs in once. The hand-off viewer shows the
  agent's tab live in the human's own browser and relays input; keystrokes
  are never logged and password values are always masked.
- Auditability is enforced by the browser, not by the agent: an append-only
  per-session journal (actions, consequences, snapshots, later screenshots)
  with `catpaw log`/`catpaw replay`; a policy file that is conservative about
  side effects by default (submissions, POST navigations, uploads, downloads
  need confirmation unless a domain is allowed); confirmation through MCP
  elicitation or the local viewer; attribution of host, model and task; and
  rate/page quotas that stop runaway agents.

## Crate map

```
catpaw (CLI) → catpaw-server (MCP over stdio: sessions, tab groups on threads of their own, actions,
                              policies and confirmations with a local approval page, hand-off viewer,
                              flight journal, profiles and checkpoints; later JSON-RPC/WS)
  → catpaw-protocol (tool definitions, parameters, result wording; protocol.json)
  → catpaw-agent (CST snapshots, refs, read views; depends on catpaw-dom only)
  → catpaw-engine (engine threads, backend selection, embedding API)
    → catpaw-bindings-boa (generated glue, proxies, WindowProxy/Location)
      → catpaw-web (event loop, navigables, session history, document lifecycle, timers, workers,
                    storage, XHR/fetch/WebSocket/crypto/streams; reaches JS only through traits)
      → catpaw-js-boa (realms, job executor, clock, module loader)
        → catpaw-dom (arena, tree ops, element state, html5ever sink, events, forms, MutationObserver)
        → catpaw-style (Stylo traits, UA stylesheet, restyle driver)
        → catpaw-text (fontique/parley contexts, bundled fonts)
        → catpaw-layout (Taffy/Parley box tree, hit-testing, scrolling, geometry)
        → catpaw-paint (display list, tiny-skia, screenshots) → catpaw-canvas
        → catpaw-fetch (Fetch spec: CORS, redirects, referrer, cache) → catpaw-net (hyper/rustls/h2,
                    cookies + PSL, proxies, HAR, Web Bot Auth signing)
        → catpaw-js (engine-neutral runtime traits)
catpaw-webidl (IDL model + emitters, used by xtask)
xtask (bindgen, protocol.json, WPT and html5lib runners, snapshot-bench, the agent task set)
```

`catpaw-agent` is published on its own and stays free of the engine: what
needs a live page (tabs, actions, the style and form-state oracle) lives in
`catpaw-server` (ADR 0006).

`catpaw-web` talks to JavaScript only through `catpaw-js` traits, so there is
no cycle with the bindings crate. `catpaw-style` and `catpaw-layout` are the
only crates that depend on Stylo (the latter through a vendored `stylo_taffy`),
isolating its monthly breaking releases.

## Key mechanisms

**DOM ownership (ADR 0002).** Nodes live in a `SlotMap` per engine thread.
Each node gets at most one JS wrapper; wrappers of one tree share a GC-managed
*tree token* that keeps the tree alive while any wrapper is reachable
(WebKit's opaque-root rule). JS-referencing state lives in wrapper data only.

*Where M1 stands:* tree tokens are not implemented yet. A node's wrapper,
once created, is kept for the life of the page, and detached nodes are not
freed until the page goes away. Other platform objects (events, collections,
`URL` objects, ...) live in a per-page arena; the runtime holds their
wrappers weakly and frees an object once script has dropped its wrapper and
nothing in Rust has it pinned. Listeners and other callbacks held by Rust are
strong roots, so a callback that captures its own target keeps it alive
until the page is dropped.

**Bindings (ADR 0004).** `cargo xtask bindgen` turns a vendored WebIDL corpus
into Boa glue plus one `XImpl` trait per interface. Prototype chains are built
with `ObjectInitializer`/`ConstructorBuilder`; exotic objects are proxies.

**Event loop and threads.** N engine threads, each a plain OS thread with a
message loop. One Boa `Context` per frame (top-level page, each iframe,
popups), all frames of a page on the same thread; groups never migrate.
Frames are cross-origin to each other whatever their origins: a frame sees
another's window as a remote window (`postMessage`, `parent`/`top`,
`closed`, focus) and never its document, which keeps one realm per
context and spares the WindowProxy machinery. Messages between frames
cross as JSON. The engine runs the frames' loops in turns of virtual
time, carries their commands (open, close, message) and tells a parent
when a child has loaded; a document's `load` waits for its frames. A
popup (`window.open()`, allowed within five seconds of a trusted click
or key press, as browsers gate it) is a frame without an element: a top
of its own in the same tree, with an opener instead of a parent. A
dedicated worker is a realm of the same kind, with a
`DedicatedWorkerGlobalScope` global: the bindings are installed per
`[Exposed]` set, so a worker sees neither `window` nor `document`; it
shares its owner's object-URL store and network client, and the engine
schedules it in the same turns (a worker that spins without yielding
blocks its page, as a long script does). `MessageChannel` ports and
`BroadcastChannel` work within one realm; ports do not transfer across
realms yet. WebSockets are opened by `catpaw-net` over the same
connector as HTTP (proxy, TLS, address policy, cookies) and driven by a
task on the network runtime; the engine host relays frames to the page
as events, and the event loop, with nothing else to do, listens to open
sockets for a second before calling the page idle. The engine keeps
the session history across documents: `history.go()` past the
document's own entries becomes a navigation request with a traversal
delta, which the engine resolves against its session list; the page
is told how many entries lie before and after it, so `history.length`
is right. `localStorage` is a per-origin map the engine seeds each new
document from and refreshes from documents as they are left; the CLI's
`--storage` file carries it between runs. Task sources with fixed priority,
microtask checkpoints after every task and every re-entry from native code,
own timer heap, rendering opportunities only when something animates or
observes. A shared `Clock` offers real time or deterministic virtual time.
Network I/O runs on tokio; response bodies are pull-based so a slow page
cannot flood its engine thread. Workers get their own thread and `Context`.

**Layout.** Style and layout run only when something observes geometry:
a CSSOM View call, a scroll, an intersection or resize observer's frame.
The page then restyles the whole document and builds a box tree apart
from the DOM (`catpaw-layout`): block, flex and grid containers go to
Taffy; a block container with only inline content becomes an inline root
whose text, inline elements and atomic inline boxes Parley shapes and
breaks into lines; replaced elements are leaves sized by their attributes
and defaults. The tree keeps the computed styles it was built with and is
dropped when the document, the style sheets or a scroll position change.
Fonts come from `catpaw-text`: a bundled DejaVu set stands in for the
generic families so that layout is the same on every machine (the
`system-fonts` feature adds the machine's fonts behind them). Boxes hold
document coordinates; fixed boxes keep viewport ones. `catpaw-paint` draws
a tree with tiny-skia for screenshots: backgrounds, borders and glyph
outlines, in tree order, clipped by overflow, and the bitmaps of
replaced elements that have one. `catpaw-paint::canvas` is the Canvas
2D raster backend (state stack, user-space paths, strokes, clips,
gradients, text shaped by Parley over the same fonts, pixel access);
`catpaw-web` maps `CanvasRenderingContext2D` onto it and keeps one
bitmap per `<canvas>`, reset when its size attributes change. Not
there yet: patterns, shadows, filters, SVG path data, images as
sources (images are not decoded).

**Web Crypto.** `crypto.subtle` (`catpaw-web/src/webcrypto.rs`) keeps
keys as platform objects holding their material (secret bytes, EC
scalars and points, RSA keys, OKP keys) and runs every operation to
completion on the page thread over the RustCrypto crates, settling the
promise at once. The specification's algorithm dictionaries are read
from the `any` the overlay IDL declares, since the parser does not read
the spec's own IDL. OKP PKCS#8 and SPKI are written by hand (fixed
shapes); everything else goes through the crates' encoders.

**Navigation.** html5ever drives parsing in time-budgeted tasks with the
spec's script pauses (`document.write`, parser-blocking, defer/async/module).
Session history, `pushState`, iframes with cross-origin `WindowProxy` and
`Location` subsets, popups gated on transient activation, dialogs that never
block the engine thread, downloads to a sandboxed store, full form submission.

**Style and layout on demand.** Stylo runs over `(&Dom, NodeId)` handles;
Taffy (block/flex/grid/float, tables mapped onto grid) and Parley (inline)
compute layout only when geometry is observed or a screenshot is requested.
Painting goes through tiny-skia on the blocking pool with a bundled
deterministic font set. Canvas 2D is backed by the same rasterizer. WebGL
returns a null context.

*Where M1 stands:* there is no layout yet. Geometry reads as empty boxes at
the origin, and the observers of rendering follow from that by their own
rules: an `IntersectionObserver` sees a target as intersecting whenever it
is in its root's tree, and a `ResizeObserver` never has a size to report.

**Network and identity (ADR 0003).** A Fetch-spec implementation over hyper +
rustls with per-context cookie jars (including partitioned cookies), caches
and proxies. Every request can be signed per Web Bot Auth. Challenge
responses are surfaced as events; a human hand-off is available.

**Agent layer (ADR 0005).** CST snapshots with global, never-reused refs;
diffs; readable views (markdown, forms, tables, text search); actions with
real event sequences and reported consequences; a settledness predicate
evaluated by the scheduler itself; contexts with identity, proxy, time mode,
resource policy, URL policy and confirm-before hooks; checkpoints and HAR
record/replay.

## Roadmap

M0 fetch & read → M1 scripts run → M2 interact → M3 agent API → M4 fidelity &
challenges → M5 scale & compat. Exit criteria live in the README.

## Testing

- `cargo test --workspace`
- `cargo xtask tree-construction` — html5lib tree-construction tests (from WPT html/syntax/parsing) through our
  `TreeSink`
- `cargo xtask wpt --include <dir> …` — testharness.js tests from web-platform-tests
  run in CatPaw pages (the checkout served by an in-process stand-in for WPT's
  server; `.any.js` wrappers, `// META:` lines and `.sub.` substitutions as it
  makes them; `xtask/src/wpt_handlers.rs` stands in for the Python handlers
  the fetch and XHR tests use), with the known failures in
  `tests/wpt-expectations/<dir>.txt`. The expectations are recorded on
  Linux, where CI runs the suites; the few tests whose outcome depends on
  the platform are skipped.
- `cargo xtask tasks replay --twice` (with `--features engine`) — the agent
  task set in `tests/tasks/`: each task's recorded traffic is replayed
  offline through the MCP server, twice, with a fixed random seed and
  clock origin, and both transcripts must equal `expected.txt` byte for
  byte (ADR 0006). `tasks record` takes new recordings from the live
  sites, `tasks lint` checks sizes and that no local path or secret header
  is kept, and `tasks report --baseline tools/baseline/playwright-mcp.json`
  sets what the agent reads beside what Playwright MCP sends for the same
  steps.
- Leak census after integration tests: force a GC, then assert the arena is
  empty.
