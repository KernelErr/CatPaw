# CatPaw

**A headless-first browser for AI agents, written from scratch in Rust.**

CatPaw is a browser whose first user is an LLM agent. It runs a page's
JavaScript and DOM as a browser does, lays the page out only when
something asks for geometry, and gives the agent what it needs: a compact
snapshot of the page with stable element references, a precise signal
that the page has settled, and what changed after each action instead of
the whole page again. It asks the user before anything is sent on their
behalf, and hands the tab to them when a site needs a person.

**Status: v0.1, a preview.** The agent tools work today over MCP
(milestones M0 to M3 of the [roadmap](#roadmap)); the tools and their
results may still change before 1.0. What does not yet work as a
browser's would is listed under
[Known gaps](docs/architecture.md#known-gaps).

[中文说明](README.zh-CN.md) · [What works](docs/features.md) ·
[Architecture](docs/architecture.md) · [Decision records](docs/adr/) ·
[Changelog](CHANGELOG.md) · [catpaw.sh](https://catpaw.sh)

## Install

> v0.1.0 is being prepared: until it is out, build from source (below).

macOS (Apple silicon) and Linux (x86_64, aarch64):

```sh
curl -fsSL https://catpaw.sh/install.sh | sh
```

Windows (PowerShell):

```powershell
irm https://catpaw.sh/install.ps1 | iex
```

The scripts download a [release](https://github.com/KernelErr/CatPaw/releases)
for your system, check it against the release's `SHA256SUMS`, show what
they will write and where (one file, `catpaw`, in `~/.catpaw/bin`; on
Windows `catpaw.exe` in `%LOCALAPPDATA%\Programs\CatPaw`, added to your
PATH), and ask before writing it. `sh -s -- --dir <dir>` (PowerShell:
`-Dir`) installs elsewhere, `--yes` answers for you, and `--uninstall`
removes CatPaw again; `--help` lists the options. Intel Macs and other
systems build from source, with Rust 1.89 or later:

```sh
git clone https://github.com/KernelErr/CatPaw && cd CatPaw
cargo build --release -p catpaw        # target/release/catpaw
```

The crates on crates.io (`catpaw` 0.0.1 and five library crates) are an
early preview from before CatPaw ran scripts.

## Use it from an agent

Register `catpaw mcp --stdio` with the agent's host:

```sh
catpaw setup claude-code                # prints the `claude mcp add` command
catpaw setup codex --write              # adds it to ~/.codex/config.toml
catpaw setup cursor -- --policy strict  # options after -- go to `catpaw mcp`
```

The agent gets fifteen tools: `navigate`, `snapshot`, `click`, `type`,
`fill`, `press`, `select`, `act` (hover, check, uncheck, focus, clear,
scroll, upload, drag), `wait`, `read` (markdown, text, links, forms,
tables, find, html, download), `screenshot`, `evaluate`, `tabs`, `logs` and
`handoff`. Elements are named by refs that stay valid while the element
is on the page. An action answers once the page has settled, with what
happened and what changed:

```text
ok click e15 button "Add to cart" (after e14 button "View details for Sauce Labs Backpack")
# s3 changed=1 added=1 removed=1
~ e10 button "Cart, empty" → "Cart, 1 items"
+ e36 button "Remove" (in e12, after e14)
- e15 button "Add to cart"
```

- **The user approves what is sent for them.** Under the default policy a
  form post and a file upload wait for the user's approval:
  `needs_confirmation c1: click e8 button "Login" would submit → POST
  https://…/authenticate (fields: username=tomsmith, password=***)`. A host
  that supports MCP elicitation (Claude Code does) asks in its own prompt;
  otherwise CatPaw opens an approval page in the user's browser. Approved,
  the held submission goes once, and nothing is clicked again.
  `--policy strict` also asks before scripts send data to other sites and
  before `evaluate`; `--trust <host>` exempts a host; `--allowed-domain`
  keeps tabs on the domains given.
- **Hand-off.** When a site needs a person (a login, a check meant for
  people), `handoff` opens the tab for the user in their own browser. They
  click and type as on the page itself; what they submit to the site goes,
  and anything else the page would send waits for them. The agent gets the
  page back with what the user typed masked.
- **A record of every call.** `--flight-log <dir>` keeps a journal
  (`--flight-screens` adds a screenshot per action; passwords are kept as
  their length), and `--profile <dir>` keeps cookies, storage, checkpoints
  and the journal between sessions.
- **Replays.** `--record-har run.har.zst` keeps a session's traffic, and
  `--replay-har` serves it back with no network; with `--random-seed` and
  `--time-origin`, a replay gives the same results byte for byte.

Errors say what to try next, and a page that did not settle says what it
is waiting on. The protocol is described in
[ADR 0006](docs/adr/0006-agent-protocol.md) and the snapshot format in
[ADR 0005](docs/adr/0005-cst-snapshot-format.md).

## Use it from the command line

```sh
catpaw fetch https://example.com --snapshot
catpaw fetch https://news.ycombinator.com --markdown
catpaw fetch https://news.ycombinator.com --js --console   # run the page's scripts first
catpaw fetch https://example.com --js --eval "document.title"
catpaw fetch https://example.com --js --screenshot page.png
# Log in through a form and keep the session for the next run
catpaw fetch https://site.example/login --js --cookie-jar ./jar.json \
    --action "fill #username bob" --action "fill #password secret" --action "press Enter" --text
```

To drive CatPaw from a script of your own (a scraper, a daily job, a
check in CI), see [Using CatPaw from scripts](docs/scripting.md).

## What makes it different

- **An agent-native API.** Snapshots are a compact text tree, a superset of
  Playwright's aria snapshot, with refs that are never reused; every action
  waits for the page to settle and answers with a diff, so one agent step is
  one round trip.
- **Exact settledness.** CatPaw owns the event loop, so it knows every
  pending request, timer (and the line that set it), animation frame and
  microtask, and a timeout names what the page is still doing.
- **Layout on demand.** Style (Stylo) and layout (Taffy and Parley) run only
  when a script or the agent asks for geometry or a screenshot.
- **Honest identity.** CatPaw says what it is (`CatPaw/0.1.0
  (+https://catpaw.sh/bot)`) and ships no fingerprint impersonation and no
  CAPTCHA solving. Deployers who want a verified identity sign requests
  with [Web Bot Auth](https://datatracker.ietf.org/wg/webbotauth/about/)
  using a key of their own; when a site wants a person, the user steps in
  ([ADR 0003](docs/adr/0003-identity-bot-auth-and-challenges.md)).
- **Pure Rust.** Boa is the JavaScript engine; V8 is a planned optional
  backend ([ADR 0001](docs/adr/0001-js-engine-boa-default.md)).

## How it compares

Over twenty tasks on sites made for automation practice, an agent reads
68 KB (about 19 500 tokens) of CatPaw results, against 243 KB (about
69 500 tokens) from Playwright MCP taking the same steps; on a long
article, CatPaw folds the page to 4000 tokens where Playwright MCP's
snapshot runs to 160 000. The tasks, the method and every number are in
[docs/comparison.md](docs/comparison.md).

## Roadmap

| Milestone | Scope | Done when |
|---|---|---|
| M0 fetch & read (done) | HTTP/1.1+2, cookies, Web Bot Auth signing, HTML parsing, style by Stylo, snapshots, markdown/text/forms views, CLI | `catpaw fetch … --snapshot` works on real pages; the WPT tree-construction suite runs in CI |
| M1 scripts run (done) | Boa realms, bindings generated from Web IDL, an event loop with virtual time, parser and script interleaving, fetch/XHR, script budget, an in-process WPT runner | WPT `dom/`, `html/dom/`, `fetch/api/`, `xhr/` pass against recorded expectations; React and Vue apps hydrate |
| M2 interact (done) | Layout, hit-testing, input, forms, navigation and history, iframes and popups, storage, observers, screenshots, Canvas 2D, Web Crypto, WebSocket, workers | Log in to a real site; the Turnstile widget completes with the test site key |
| M3 agent API (done) | MCP with snapshots, diffs and budgets, settledness, action consequences, read views, tabs, HAR record/replay, confirmations, flight recorder, checkpoints and profiles, hand-off, `catpaw setup` | A task set on practice sites completes over MCP and replays byte for byte; confirmation and hand-off work from Claude Code |
| M4 fidelity & challenges | Challenge detection, a test zone with each Cloudflare challenge mode, Web Bot Auth documentation for deployers, Intl and CSSOM | Measured pass rates |
| M5 scale & compat | Multi-tenant limits, OpenTelemetry, Docker, a CDP subset, V8 backend parity | 1000 contexts on one host; puppeteer-core smoke tests |

## Repository layout

```
crates/   one crate per subsystem (net, fetch, dom, style, layout, paint, js, web, agent, server, cli)
docs/     architecture notes, decision records, what works, the comparison, scripting
tests/    the agent task set, web-platform-tests expectations
xtask/    bindgen, test runners, the task set's tools
release/  what release archives carry
```

## Contributing and security

See [CONTRIBUTING.md](CONTRIBUTING.md) for the ground rules and the
developer commands, and [SECURITY.md](SECURITY.md) to report a
vulnerability.

## License

Apache-2.0 OR MIT, at your option. Dependencies carry their own licenses
(Stylo is MPL-2.0); release archives list them in
`THIRD-PARTY-LICENSES.html`.
