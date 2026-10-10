# Changelog

## 0.1.0 (unreleased)

The first preview: a browser an agent drives over MCP, with the user
asked before anything is sent on their behalf. The tools and their
results may still change before 1.0. What does not work as a browser's
would yet is listed in [Known gaps](docs/architecture.md#known-gaps).

Binaries for Linux (x86_64, aarch64), macOS on Apple silicon and Windows
(x86_64); Intel Macs build from source.

### For agents

- `catpaw mcp --stdio` serves fifteen tools: `navigate`, `snapshot`,
  `click`, `type`, `fill`, `press`, `select`, `act`, `wait`, `read`,
  `screenshot`, `evaluate`, `tabs`, `logs` and `handoff` (and, on
  request, `session` for checkpoints). `catpaw setup claude-code`,
  `codex` or `cursor` registers it with a host.
- Snapshots in a compact text form with refs that are never reused; an
  action answers with what happened and what changed, once the page has
  settled, and a page that did not settle says what it waits on.
- Confirmations: a form posted, a file uploaded (and, under `--policy
  strict`, script sending data to another site, and `evaluate`) waits
  for the user. A host with MCP elicitation asks in its own prompt;
  otherwise CatPaw opens an approval page in the user's browser.
- Hand-off: the user takes over a tab in their own browser (a login, a
  check meant for people), and the agent gets it back with what they
  typed masked.
- A flight journal of every call, profiles that keep cookies and
  storage between sessions, checkpoints, and traffic recorded to HAR and
  replayed byte for byte.

### The engine

- HTTP/1.1 and HTTP/2 over rustls, cookies, proxies, private addresses
  refused by default, and Web Bot Auth request signing for deployers with
  their own key.
- HTML parsing, style by Stylo, JavaScript by Boa with bindings generated
  from Web IDL, an event loop with virtual time, frames, popups, workers,
  WebSocket, storage, Web Crypto and Canvas 2D.
- Shadow trees styled and shown as they render: their own and adopted
  style sheets, slots, and document rules kept out of them.
- Media elements that play nothing and say so, no plugins and no PDF
  viewer; `:has()` and `:nth-child(… of …)` selectors; `RegExp.$1` and the
  other legacy static properties of `RegExp`; `Intl.DateTimeFormat`
  `formatToParts`, and dates with the whole year and `2-digit` padding as
  asked for.
- `Date.parse` takes `2026/10/10 15:51:46+00:00`, `2026-10-10 15:51:46 UTC`
  and its own `toString()` back; a stack overflow is a `RangeError` script
  can catch, as in browsers.
- The modules a module imports load side by side, as a browser loads a
  module graph: x.com's 590 modules took 100 seconds one by one, now 10.
- Layout by Taffy and Parley when something asks for geometry, and
  screenshots by tiny-skia.
- web-platform-tests run in CI against recorded expectations.

### Identity

CatPaw says what it is: its User-Agent is `CatPaw/0.1.0
(+https://catpaw.sh/bot)`, and it ships no fingerprint impersonation and
no CAPTCHA solving.
