# CatPaw

**A headless-first browser for AI agents, written from scratch in Rust.**

CatPaw is not a Chromium wrapper and not a rendering engine with an
automation API bolted on. It is a browser whose first user is an LLM agent:
it executes JavaScript and the DOM faithfully, computes layout only when
something observes it, and exposes what agents actually need — a compact
semantic snapshot with stable element references, precise "the page has
settled" signals, diffs instead of re-dumps, deterministic time, and cheap
isolated contexts.

> Status: **pre-alpha**. Milestones M0 ("fetch & read"), M1 ("scripts
> run", JavaScript via Boa) and M2 ("interact") are complete; M3 (the
> agent API) is under way: `catpaw mcp --stdio` serves the agent tools over
> MCP, and a task set on practice sites replays from recorded traffic in CI
> (see [From an agent](#from-an-agent-mcp)). See the roadmap below.
>
> What M2 brought: layout. Block, flex and grid boxes are laid out by Taffy and
> inline content shaped and line-broken by Parley over a bundled font set,
> only when something asks for geometry; the CSSOM View answers from it
> (`getBoundingClientRect`, `getClientRects`, `offset*`/`client*`/`scroll*`,
> `scrollTo`, `scrollIntoView`, `elementFromPoint`), as do
> `IntersectionObserver` and `ResizeObserver`. Screenshots
> (`catpaw fetch --js --screenshot out.png [--full-page]`) paint backgrounds,
> borders and text with tiny-skia. Input: trusted pointer and keyboard
> sequences with focus, typing, activation (links, buttons, labels,
> `details`), form submission in every encoding and the navigations that
> follow, driven by `--action "click <selector>"`, `fill`, `type`, `press`,
> `check`, `select`. Frames: every `iframe` is a page of its own (document,
> scripts, event loop) in the same thread, sized by its element; frames
> see each other only through `postMessage`, `parent`/`top`/`contentWindow`
> and the `load` events, as cross-origin frames do; `--action "frame <selector>"`
> addresses a frame for the actions and `--eval` that follow (`frame top`,
> `frame parent` go back). Popups: `window.open()` after a click or key press
> opens a page of its own with `opener` set; `frame popup` addresses the one
> opened last, and `window.close()` closes it. Canvas: `getContext('2d')` draws with tiny-skia (paths, arcs,
> rounded rects, fills, strokes, dashes, clips, gradients, transforms,
> compositing, text through the same fonts as layout, `drawImage` from
> other canvases, `getImageData`/`putImageData`, `toDataURL`/`toBlob`),
> and canvases are painted into screenshots. Web Crypto: `crypto.subtle`
> with HMAC, AES-GCM/CBC/CTR, PBKDF2, HKDF, ECDSA and ECDH on P-256 and
> P-384, RSA (PKCS#1 v1.5, PSS, OAEP), Ed25519 and X25519, in raw, JWK,
> PKCS#8 and SPKI formats, over the RustCrypto crates. Workers: dedicated workers (`new Worker`, from
> same-origin, `blob:` and `data:` scripts; `postMessage` both ways,
> `importScripts`, `close`, `terminate`, errors relayed to the owner) run as
> realms of their own on the page's thread, in turns with the page and its
> frames. With frames and workers in place, the Cloudflare Turnstile widget
> loads its challenge frame and completes against the test site key, and the
> page's callback receives the token. Channels and sockets: `MessageChannel`,
> `MessagePort` and `BroadcastChannel` within a page; `WebSocket` over the
> same transport as HTTP (proxy, TLS, cookies and the private-network policy
> apply), text and binary both ways, close codes and reasons; a page whose
> only pending work is an open socket counts as settled after a second of
> silence. Session: `--action back` / `forward` traverse the session history
> across documents (and within one, for `pushState` entries), as
> `history.back()` does from script; `--storage <file>` keeps `localStorage`
> by origin between runs, as `--cookie-jar` keeps cookies. Network: response bodies are capped on the wire and
> after decoding (`--max-response-mb`), loopback and private addresses are
> refused unless `--allow-private-network` says otherwise, HTTP `CONNECT`
> and SOCKS5 proxies (`--proxy`), cookie files kept between runs
> (`--cookie-jar`).
>
> With `catpaw fetch --js`: classic scripts (inline, external, `defer`,
> `async`, script-inserted, `document.write`) and module scripts (static and
> dynamic imports, import maps) run interleaved with the parser on Boa,
> against bindings generated from Web IDL for the core DOM (every HTML and
> SVG element interface, attributes, traversal, XPath, `Range` and
> `Selection`, `DOMParser`, `document.implementation`, the document
> collections and named access such as `document.forms` and
> `document.myForm`), shadow trees and slots, custom elements, events,
> mutation, intersection and performance observers, timers, history,
> navigation timing, inline and computed styles (style sheets are fetched and
> cascaded by Stylo) with the CSSOM (`CSSStyleSheet`, `adoptedStyleSheets`,
> `CSS.supports`), `fetch`/`XMLHttpRequest` (CORS, preflights, redirects and
> referrer policy handled by the page as the Fetch standard has them),
> streams, `data:` URLs, `sendBeacon`, `URL`, storage, encoding, `crypto`
> random values and digests, the font-loading API in its no-layout form, and
> console APIs, on an event loop with virtual time. A wall-clock script
> budget (`--script-budget`, 10 s by default) stops a runaway script.
> React, Vue, Svelte, Lit, htmx and Alpine sites run; the Boa engine is
> vendored with fixes described in `vendor/`. web-platform-tests run in CI
> against recorded expectations, served by an in-process stand-in for WPT's
> server and the Python handlers its fetch and XHR tests use: `dom` 3019 of
> 4246 subtests pass, `html/dom` 582 of 1066, `fetch/api` 1908 of 2237,
> `xhr` 868 of 1203, `css/cssom-view` 478 of 1198 (much of the rest needs
> frames or workers the test harness does not run, layout, or server
> behaviour the stand-in does not emulate). Not there yet: images,
> gradients and rounded corners in screenshots, tables as a grid, images' intrinsic sizes, media,
> WebAssembly, and the members of HTML elements that go beyond
> their attributes.
>
> What works without JavaScript: HTTP/1.1 and HTTP/2 over rustls,
> redirects, cookies, gzip/brotli/zstd, encoding sniffing, Web Bot Auth
> request signing (verified against Cloudflare's test endpoint), HTML parsing
> into the arena DOM (1858 of 1968 WPT tree-construction tests; the rest are
> documented upstream gaps), Stylo-resolved `display`/`visibility` from UA,
> linked and inline stylesheets, CST snapshots with stable refs, and
> markdown/text/links/forms views.

[中文说明](README.zh-CN.md) · [Architecture](docs/architecture.md) ·
[Decision records](docs/adr/) · [Full design (zh-CN)](docs/design.zh-CN.md)

## What makes it different

- **Agent-native API.** `snapshot` returns a CatPaw Snapshot Text (CST) tree —
  a superset of Playwright's aria snapshot — with refs that never get reused;
  every action waits for the page to settle and can return a diff, so one
  agent step is one round trip. JSON-RPC over WebSocket and a built-in MCP
  server; a CDP subset comes later for Puppeteer compatibility.
- **Exact settledness.** Because CatPaw owns the event loop it knows every
  pending fetch, timer (and its source line), animation frame and microtask.
  Timeouts name the culprit instead of failing silently.
- **Layout on demand.** Style (Stylo) and layout (Taffy + Parley) run only when
  a script or the agent asks for geometry or a screenshot. Headless pages that
  never do are nearly free.
- **Honest identity.** CatPaw identifies itself, implements
  [Web Bot Auth](https://datatracker.ietf.org/wg/webbotauth/about/) (RFC 9421
  HTTP Message Signatures) in its fetch layer, and hands off to a human when a
  site asks for something an agent cannot legitimately provide. It ships no
  fingerprint-impersonation profiles and no CAPTCHA solvers — see
  [ADR 0003](docs/adr/0003-identity-bot-auth-and-challenges.md).
- **Pure Rust by default.** Boa is the default JavaScript engine; V8 is a
  planned optional backend ([ADR 0001](docs/adr/0001-js-engine-boa-default.md)).

## Quick start

```sh
cargo install catpaw            # published on crates.io; from a checkout use `cargo run -p catpaw --`
catpaw fetch https://example.com --snapshot
catpaw fetch https://news.ycombinator.com --markdown
catpaw fetch https://httpbin.org/forms/post --forms
# From a checkout (not in the published 0.0.1 yet): run the page's scripts first
cargo run -p catpaw -- fetch https://news.ycombinator.com --js --console
cargo run -p catpaw -- fetch https://example.com --js --eval "document.title"
# Log in through a form and keep the session for the next run
cargo run -p catpaw -- fetch https://site.example/login --js --cookie-jar ./jar.json \
    --action "fill #username bob" --action "fill #password secret" --action "press Enter" --text
catpaw keygen --out ./agent-key.json
catpaw fetch https://crawltest.com/cdn-cgi/web-bot-auth \
    --bot-auth-key ./agent-key.json --signature-agent https://your-agent.example --text
```

### From an agent (MCP)

`catpaw mcp --stdio` serves the browser to an agent over the Model Context
Protocol. From a checkout, build it and register it with your host, for
example Claude Code:

```sh
cargo build --release -p catpaw
claude mcp add catpaw -- "$PWD/target/release/catpaw" mcp --stdio
```

The tools are `navigate`, `snapshot`, `click`, `type`, `press`, `select`,
`act` (hover, check, uncheck, focus, clear, scroll, upload), `wait`, `read`
(markdown, text, links, forms, tables, find, html), `screenshot`,
`evaluate`, `tabs` and `logs`; windows a page opens become tabs. Elements
are named by refs that stay valid until the element leaves the page. An
action answers with what happened and what changed on the page, once the
page has settled (analytics and polling are not waited for):

```text
ok click e16 button "Add to cart"
# s4 diff-from=s3 tab=t1 doc=d1 url=(same) scroll=0,0 settled=yes changed=1 added=1 removed=1 unchanged=27
~ e11 button "Cart, empty" → "Cart, 1 items"
+ e37 button "Remove" (in e13, after e15)
- e16 button "Add to cart"
```

A new document comes back whole. When something is still loading,
`wait({"for":"text","text":"Order placed"})` runs the page until it shows;
time a page spends only on timers passes at once.

Errors say what to try next (`error StaleRef e13 button "Remove"
(removed)`, then the likely replacement and an `advice:` line). The format
and the protocol are described in
[ADR 0005](docs/adr/0005-cst-snapshot-format.md) and
[ADR 0006](docs/adr/0006-agent-protocol.md);
`cargo run -p xtask --features engine -- snapshot-bench` measures snapshot sizes on
live pages.

Side effects wait for the user. Under the default policy a navigation
that sends data (a form submission) and an upload stop with
`needs_confirmation c1: click e8 button "Login" would submit → POST
https://…/authenticate (fields: username=tomsmith, password=***)`. A host
that supports MCP elicitation asks its user there and then; otherwise the
user approves on a local page whose address the result gives, with a key
the agent never sees, and the agent repeats the call with
`confirmation: "c1"`: the held submission goes, once, and nothing is
clicked again. `--policy strict` also asks before scripts send data to
other sites and before `evaluate`, `--policy open` asks for nothing, and
`--trust <host>` and `--allowed-domain <domain>` adjust either.
`--flight-log <dir>` keeps a journal of every call (`--flight-screens`
adds a screenshot per action; typed passwords are kept as their length),
`--profile <dir>` keeps cookies, localStorage, checkpoints and the journal
between sessions, and `--tools session` adds a tool that saves and
restores checkpoints. `act` with `kind: "upload"` chooses local files in a
file input.

`--record-har run.har.zst` keeps a session's traffic and `--replay-har
run.har.zst` serves it back with no network; with `--random-seed` and
`--time-origin` as well, a replay gives the same results byte for byte.
Eighteen tasks on sites made for automation practice (Sauce Demo, Books
and Quotes to Scrape, the-internet, TodoMVC, httpbin) live in [`tests/tasks/`](tests/tasks/),
each with its recording and the transcript an agent sees; CI replays them
twice, offline, and both runs must equal the transcript. What an agent
reads over each task, against Playwright MCP taking the same steps:

| task | CatPaw calls | CatPaw bytes (~tokens) | Playwright MCP calls | Playwright MCP bytes (~tokens) |
|---|---|---|---|---|
| books-category | 3 | 14814 (~4233) | 6 | 64648 (~18471) |
| books-pagination | 2 | 13445 (~3841) | 4 | 64942 (~18555) |
| httpbin-form | 6 | 2248 (~642) | 9 | 7477 (~2136) |
| internet-dropdown | 2 | 720 (~206) | 4 | 1937 (~553) |
| internet-dynamic | 3 | 1030 (~294) | 6 | 2922 (~835) |
| internet-entry-ad | 2 | 1174 (~335) | 4 | 2305 (~659) |
| internet-frames | 2 | 859 (~245) | 3 | 980 (~280) |
| internet-login | 5 | 2118 (~605) | 6 | 2940 (~840) |
| internet-prompt | 2 | 980 (~280) | 5 | 1968 (~562) |
| internet-upload | 5 | 1829 (~523) | 8 | 3239 (~925) |
| internet-windows | 3 | 820 (~234) | 5 | 2217 (~633) |
| quotes-js-pagination | 2 | 1452 (~415) | 4 | 9394 (~2684) |
| quotes-login | 5 | 4409 (~1260) | 6 | 12497 (~3571) |
| quotes-scroll | 3 | 906 (~259) | 5 | 11922 (~3406) |
| quotes-table | 2 | 5179 (~1480) | 2 | 8658 (~2474) |
| saucedemo-checkout | 11 | 7397 (~2113) | 18 | 20992 (~5998) |
| saucedemo-sort | 4 | 5651 (~1615) | 7 | 12814 (~3661) |
| todomvc | 5 | 1833 (~524) | 10 | 9128 (~2608) |
| all | 67 | 66864 (~19104) | 112 | 240980 (~68851) |

Bytes are all the tool results an agent receives over a task (tokens
estimated at 3.5 bytes each). CatPaw's numbers come from the recordings;
`@playwright/mcp` 0.0.83 with headless Chrome took the same steps live on
2026-10-08 (the median of three runs). Playwright MCP keeps the page
snapshot in a file and links it from a result whenever the page changed;
an agent reads it to see the page and find its next target, so the file
counts too, as one more call. CatPaw answers an action with what changed,
and caps a whole snapshot at 4000 tokens, folding the rest for the agent
to open; its numbers include the calls that wait for the user's approval
(logins, the form post, the upload), which Playwright MCP does not make.
The tool list, a cost on every turn, is 10.0 KB for CatPaw and 20.3 KB for
Playwright MCP.
`cargo run -p xtask --features engine -- tasks report --baseline tools/baseline/playwright-mcp.json`
regenerates the table, and
[`tools/baseline/playwright-mcp.mjs`](tools/baseline/playwright-mcp.mjs)
measures the baseline.

The library crates are published too: `catpaw-net`, `catpaw-fetch`, `catpaw-dom`,
`catpaw-style`, `catpaw-agent`.

Developer tasks: `cargo xtask tree-construction` runs the html5lib
tree-construction suite from a pinned, sparse web-platform-tests checkout
(`tests/wpt.lock`) against `tests/tree-construction-expectations.txt`;
`cargo xtask wpt --include dom --include html/dom …` (with `--features wpt`) runs testharness.js
tests from the same checkout in CatPaw pages, served by an in-process stand-in
for WPT's server, against `tests/wpt-expectations/<dir>.txt` (the known
failures; `--update-expectations` rewrites them).
`cargo xtask bindgen` regenerates the JavaScript bindings from the Web IDL
corpus and `crates/catpaw-webidl/bindings.toml` (`--check` verifies the
checked-in output, `--list <Interface>` shows what an interface offers).

## Roadmap

| Milestone | Scope | Done when |
|---|---|---|
| M0 fetch & read (done) | HTTP/1.1+2, cookies, Web Bot Auth signing, HTML parsing into the arena DOM, UA + author stylesheets via Stylo, CST snapshot v0, markdown/text/forms views, CLI | `catpaw fetch … --snapshot` works on real pages; WPT tree-construction suite runs in CI with recorded expectations |
| M1 scripts run (done) | Boa realms, generated bindings, event loop with virtual time, parser/script interleaving, fetch/XHR, script budget, in-process WPT runner (in place of the WebDriver subset first planned) | WPT `dom/`, `html/dom/`, `fetch/api/`, `xhr/` subsets pass against recorded expectations; React and Vue apps hydrate server-rendered markup (timed in the test suite) |
| M2 interact (done) | Layout, hit-testing, input events, forms, navigation and history, iframes and popups, storage, observers, screenshots, Canvas 2D, Web Crypto, WebSocket, Workers | Log in to a real site; the Turnstile widget completes (done with the test site key: the widget's frame and worker run, the page's callback receives the token) |
| M3 agent API (in progress) | MCP over stdio with compact snapshots, diffs and token budgets, settledness with pending reports, action consequences, read views, popups as tabs, HAR record/replay with virtual time, confirmation policies, flight recorder, checkpoints and profiles, human hand-off, `catpaw setup`; JSON-RPC/WS and SDKs later | A task set on practice sites completes over MCP and replays byte for byte from recorded HARs; confirmation and hand-off work from Claude Code |
| M4 fidelity & challenges | Challenge detection, test zone with each Cloudflare challenge mode, Signed Agent registration | Measured pass rates |
| M5 scale & compat | Multi-tenant limits, OpenTelemetry, Docker, CDP subset, V8 backend parity | 1000 contexts on one host; puppeteer-core smoke tests |

## Repository layout

```
crates/            one crate per subsystem (net, fetch, dom, style, layout, paint, js, web, agent, server, cli)
docs/              architecture notes and ADRs
tests/             web-platform-tests and html5lib-tests submodules, expectations
xtask/             bindgen, test runners, IDL sync
```

## License

Apache-2.0 OR MIT, at your option. Dependencies carry their own licenses
(Stylo is MPL-2.0).
