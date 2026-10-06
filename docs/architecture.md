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
catpaw (CLI) → catpaw-server (JSON-RPC/WS, MCP, hand-off) → catpaw-agent (CST snapshots, actions, settledness)
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
catpaw-webidl (IDL model + emitters, used by xtask)   catpaw-protocol (protocol.json via schemars)
xtask (bindgen, IDL sync, WPT and html5lib runners)
```

`catpaw-web` talks to JavaScript only through `catpaw-js` traits, so there is
no cycle with the bindings crate. `catpaw-style` is the only crate that
depends on Stylo, isolating its monthly breaking releases.

## Key mechanisms

**DOM ownership (ADR 0002).** Nodes live in a `SlotMap` per engine thread.
Each node gets at most one JS wrapper; wrappers of one tree share a GC-managed
*tree token* that keeps the tree alive while any wrapper is reachable
(WebKit's opaque-root rule). JS-referencing state lives in wrapper data only.

**Bindings (ADR 0004).** `cargo xtask bindgen` turns a vendored WebIDL corpus
into Boa glue plus one `XImpl` trait per interface. Prototype chains are built
with `ObjectInitializer`/`ConstructorBuilder`; exotic objects are proxies.

**Event loop and threads.** N engine threads, each a plain OS thread with a
message loop. One Boa `Context` per browsing-context group (top-level page +
its iframes + popups); groups never migrate. Task sources with fixed priority,
microtask checkpoints after every task and every re-entry from native code,
own timer heap, rendering opportunities only when something animates or
observes. A shared `Clock` offers real time or deterministic virtual time.
Network I/O runs on tokio; response bodies are pull-based so a slow page
cannot flood its engine thread. Workers get their own thread and `Context`.

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
- `cargo xtask wpt --include <dirs>` (from M1) — web-platform-tests subsets
  with expectations in `tests/wpt/meta`
- Leak census after integration tests: force a GC, then assert the arena is
  empty.
