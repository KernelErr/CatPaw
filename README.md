# CatPaw

**A headless-first browser for AI agents, written from scratch in Rust.**

CatPaw is not a Chromium wrapper and not a rendering engine with an
automation API bolted on. It is a browser whose first user is an LLM agent:
it executes JavaScript and the DOM faithfully, computes layout only when
something observes it, and exposes what agents actually need — a compact
semantic snapshot with stable element references, precise "the page has
settled" signals, diffs instead of re-dumps, deterministic time, and cheap
isolated contexts.

> Status: **pre-alpha**. Milestone M0 ("fetch & read") is complete; M1
> (JavaScript via Boa) is in progress. See the roadmap below.
>
> M1 so far, behind `catpaw fetch --js`: classic scripts (inline, external,
> `defer`, `async`, script-inserted, `document.write`) and module scripts
> (static and dynamic imports, import maps) run interleaved with the parser
> on Boa, against bindings generated from Web IDL for the core DOM (every
> HTML and SVG element interface, attributes, traversal, XPath, `DOMParser`,
> `document.implementation`), shadow trees and slots, custom elements,
> events, mutation, intersection and performance observers, timers, history,
> navigation timing, inline and computed styles (style sheets are fetched and
> cascaded by Stylo) with the CSSOM (`CSSStyleSheet`, `adoptedStyleSheets`,
> `CSS.supports`), `fetch`/`XMLHttpRequest` (with CORS enforced), streams,
> `data:` URLs, `sendBeacon`, `URL`, storage, encoding, `crypto` random
> values and digests, the font-loading and selection APIs in their
> no-layout forms, and console APIs, on an event loop with virtual time.
> React, Vue, Svelte, Lit, htmx and Alpine sites run; the Boa engine is
> vendored with fixes described in `vendor/`. web-platform-tests run in CI
> against recorded expectations: `dom` 2753 of 4164 subtests pass, `html/dom`
> 498 of 1066, `fetch/api` 1037 of 2172, `xhr` 330 of 974 (much of the rest
> needs iframes, layout or WPT's Python handlers). Not there yet: layout,
> canvas, media, workers, WebAssembly, `Range`, and the members of HTML
> elements that go beyond their attributes.
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
catpaw keygen --out ./agent-key.json
catpaw fetch https://crawltest.com/cdn-cgi/web-bot-auth \
    --bot-auth-key ./agent-key.json --signature-agent https://your-agent.example --text
```

The library crates are published too: `catpaw-net`, `catpaw-fetch`, `catpaw-dom`,
`catpaw-style`, `catpaw-agent`.

Developer tasks: `cargo xtask tree-construction` runs the html5lib
tree-construction suite from a pinned, sparse web-platform-tests checkout
(`tests/wpt.lock`) against `tests/tree-construction-expectations.txt`;
`cargo xtask wpt --include dom --include html/dom …` runs testharness.js
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
| M1 scripts run | Boa realms, generated bindings, event loop with virtual time, parser/script interleaving, fetch/XHR, minimal WebDriver for WPT | WPT `dom/`, `html/dom/`, `fetch/api/` subsets pass; a Next.js and a Vue app hydrate |
| M2 interact | Layout, hit-testing, input events, forms, navigation and history, iframes and popups, storage, observers, screenshots, Canvas 2D, Web Crypto, WebSocket, Workers | Log in to a real site; a Turnstile checkbox click completes |
| M3 agent API | JSON-RPC/WS, MCP, snapshot diffs, settledness, action consequences, checkpoints, HAR record/replay, SDKs | An agent completes WebArena tasks over MCP |
| M4 fidelity & challenges | Challenge detection, human hand-off, test zone with each Cloudflare challenge mode, Signed Agent registration | Measured pass rates; hand-off end to end |
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
