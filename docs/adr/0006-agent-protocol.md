# ADR 0006: The agent protocol — tools, results, confirmation

- Status: Accepted (2026-10-07)

## Context

M3 puts CatPaw in front of LLM agents. Three things decide whether an agent
does well with a browser: how easy the tools are to use correctly, how many
tokens each step costs, and how often an action does what was meant. The
tool surface is also a fixed cost: hosts put every tool description in front
of the model on every turn.

## Decision

1. **MCP over stdio first.** `catpaw mcp --stdio` speaks newline-delimited
   JSON-RPC 2.0 (`initialize`, `ping`, `tools/list`, `tools/call`;
   notifications are accepted and ignored). It is written by hand rather
   than with an MCP crate: the surface is small, the same session will
   serve JSON-RPC over WebSocket later, and the crates still change often.
   Stdout carries protocol messages only; lines are read on a thread of
   their own, so that a call can ask the client (`elicitation/create`) and
   read its answer while it runs; requests that arrive meanwhile wait
   their turn, pings are answered at once.
2. **Tools.** `navigate`, `snapshot`, `click`, `type`, `press`, `select`,
   `act` (hover, check, uncheck, focus, clear, scroll, upload), `wait`,
   `read` (markdown, text, links, forms, tables, find, html), `screenshot`,
   `evaluate`, `tabs`, `logs` and `handoff`; `session` (checkpoints) is
   listed only when the server is started with `--tools session`. Frequent actions are tools of
   their own because their required fields differ: a schema that requires
   `text` catches the commonest small-model mistake (the right action with
   a field missing) before it reaches the page. Rare actions share `act`.
3. **Hand-written schemas.** Descriptions and schemas are static text in
   `catpaw-protocol`, with no version, host or port in them, each ending in
   one example call; `protocol.json` holds the same data and
   `cargo xtask protocol --check` keeps it current. Deriving them
   (schemars, as first planned) would put words in front of the model that
   nobody chose. A test parses every example with the tool's parameter type
   and keeps the whole list under 10 000 bytes (6.4 KB for the eleven tools
   today). Unknown fields are refused with the names of the right ones.
4. **Targets** are one string: a ref `e12` (a whole snapshot line or
   `[ref=e12]` is accepted, since models copy those), `text:<visible
   text>`, `role "name"` (`button "Sign in"`: a snapshot line without its
   ref), `css:<selector>` (shadow trees and frames included) or
   `xy:<x>,<y>` (into the frame under the point). Text and names are
   matched against what a snapshot shows, frames included; an exact match
   beats a partial one and a single control beats other matches, and
   anything still tied is `error AmbiguousTarget` listing up to five
   candidates: the server never guesses. A `role "name"` target names its
   element in full (case and spacing aside), or as the snapshot cut it
   short with `…`; a name found only inside another is listed, not taken
   (`error NotFound textbox "Password" names nothing in full; in part: e5
   textbox "Username Password"`), since it is another field as often as
   not. Before acting, an element must be
   enabled (for clicks, typing, choosing and checking) and hold still while
   the page animates; one covered by something else is reported with the
   control that would dismiss the cover when there is one
   (`maybe dismiss it with e45 button "Accept all"`). `force: true` on
   `click` skips the checks and clicks the element itself.
5. **Result grammar.** The first line is `ok …`, `error <Code> …`,
   `needs_confirmation cN …` or `blocked <reason> …`; only `error` sets
   MCP's `isError`. An `ok` line echoes the element acted on as
   `eN role "name"` (the cheapest guard against acting on the wrong one)
   and how the page moved: `→ <url> (200)`, `(POST, 302)`, or
   `(same document)` for `pushState`. Lines starting with `!` report
   consequences in a fixed order (navigation failures, new tabs, closed
   tabs, dialogs, requests the page made, console errors, and what kept the
   page busy when it did not settle). What changed on the page follows
   (decision 9). Errors say what to try next on an `advice:` line; the
   codes are `BadArgument`, `NoTab`, `StaleRef`, `NotFound`,
   `AmbiguousTarget`, `NotActionable`, `Occluded`, `NavigationFailed`,
   `ScriptError`, `Timeout`, `Unsupported` and `Crashed`.
6. **Byte stability.** Header keys, attributes, consequence lines and diff
   lines come in fixed orders; ids are never reused; no wall-clock time
   appears; defaults are not printed; all wording comes from one table
   (`catpaw_protocol::wording`); estimators and limits are constants. A
   page that did not change gives a snapshot identical but for its `sN`,
   so hosts' prompt caches keep hitting.
7. **Sessions, groups and tabs.** A session is one browsing context
   (cookies, connections, storage). A page and the popups it opens form a
   group that lives on a thread of its own (pages are not `Send`); a popup
   is a tab of the group. A group that panics is closed and reported as
   `error Crashed`; the server and the other tabs carry on.
8. **Confirmation.** A policy decides what waits for the user. Under
   `default`, a navigation that sends data (a form posted by a click, a
   key or script) and choosing files to upload; `strict` adds script
   requests that send data to another site and `evaluate`; `open` asks
   for nothing (test runs). `--trust <host>` exempts a host,
   `--allowed-domain <domain>` limits what tabs may show (anything else is
   `blocked policy: …`). Navigations and script requests are held where
   they would leave for the network, after the action ran, so an
   approved action is let go and never carried out again; uploads and
   `evaluate` are stopped before they run. The result names what would
   happen, with secrets masked:
   `needs_confirmation c1: click e8 button "Login" would submit → POST
   https://…/authenticate (fields: username=tomsmith, password=***)`. A
   host that offers MCP elicitation asks its user within the same call
   (a boolean `approve`; declining gives `blocked user: declined c1`).
   Otherwise the result gives the address of a page on 127.0.0.1 where
   the user approves with a key kept in a file (made on first use in the
   user's data directory, or `--approval-key-file`); the browser keeps the
   key in a cookie after the first time, and the page refuses other hosts
   and other origins. The agent never sees the key: no result prints it,
   and its browser refuses private addresses. It then repeats the call
   with `confirmation: "c1"`; the repeat must match the original call
   (tool and arguments) or it is refused, an unanswered one says
   `(still pending)`, and confirmations run out after ten minutes. This
   bounds an agent that holds only the browser tools; an agent with a
   shell is bounded by its host's own permission prompts.

9. **Diffs after actions.** An action answers with what changed since
   the tab's last snapshot (ADR 0005, amended): `~` changed, `+` added
   with its subtree, `-` removed, `>` moved, `(replaces eN)` for a node
   the page rendered again. A new document, a page that changed more than
   it stayed (the diff over 60% of the whole), or no snapshot to compare
   with gives the whole snapshot, its header saying why (`full=navigated`,
   `full=large`, `full=no-baseline`). `snapshot: "full" | "none"` on any
   action says otherwise. A tab keeps its last eight snapshots.
10. **Settling.** An action is done when the page has settled under a
    policy, not when its event loop is empty (real pages never empty
    it). The policy waits for requests the page waits on, timers due
    within a second, animation frames that change the document, and a
    document quiet for 100 ms of page time. It does not wait for
    analytics and telemetry hosts, beacons, requests started by polling
    timers (a site that armed five timers of 100 ms or more), style sheets
    and fonts slower than two seconds, requests to other sites slower than
    three, or anything open ten seconds. Timers and requests remember the
    script position that made them, so a page that did not settle is
    reported with its causes (`pending fetch GET /api/cart (9.8s, from
    recalc (app.js:1203))`). `wait` runs the page on until a text appears
    or goes, an element shows, the URL changes, or some time passes; page
    time spent only on timers passes at once, and a wait for what an idle
    page cannot bring fails at once.
11. **Dialogs** are dismissed unless the action says `dialog: "accept"`
    (`promptText` answers a prompt and implies accepting); each is a
    consequence line with its answer.
12. **Recording and replay.** `--record-har <file>` keeps a session's
    traffic as HAR 1.2 (compressed with zstd when the name ends in
    `.zst`), without request cookies, credentials or signatures;
    `--replay-har <file>` answers from it with no network. A request
    matches on method, URL and body, or failing that on method and path;
    repeated requests get the recorded answers in order, and one the
    recording lacks fails (`--replay-misses-live` sends it instead).
    Answers arrive at once, in the order the page asked for them;
    `--random-seed` seeds `Math.random` and `crypto.getRandomValues` for
    each document, `--time-origin` fixes where the page clock starts, and
    cookie lifetimes count from the recorded time. A replay therefore gives
    the same results byte for byte, on any platform. WebSockets are refused
    during replay; they are not recorded yet.
13. **The task set.** `tests/tasks/<id>/` holds a goal on a site made for
    automation practice, the calls an agent would make (and where the user
    approves a confirmation, on the approval page with the key), checks
    of the outcome, the recording and the expected transcript (each call
    and its result). `cargo xtask tasks record` runs a task live and keeps the
    recording only when two replays of it agree; CI replays every task
    twice against its transcript, offline, so a change in what agents see
    shows up as a diff of the transcript. Content sites (Hacker News,
    Wikipedia, MDN) are tasks too, but their recordings stay out of the
    repository (`tests/tasks/local/`).
14. **Flight recorder, profiles and checkpoints.** `--flight-log <dir>`
    keeps a journal: one JSON line per call (tool, arguments, first line
    of the result, consequences, URL, time taken) and per confirmation
    and decision; `--flight-screens` adds a screenshot per page action.
    It never holds cookies or the bodies of held requests, and text typed
    into a password field becomes its length. `--profile <dir>` keeps the
    cookie jar, `localStorage`, saved checkpoints and the journals between
    sessions, written after every call that acts. The `session` tool
    saves a checkpoint (cookies, storage, each tab's URL and scroll),
    restores it (tabs load again, refs start afresh) and lists them.
15. **Hand-off.** `handoff({reason})` gives the user the current tab on a
    page served at 127.0.0.1 (its address carries a one-time token): a
    screenshot of the tab, kept current, that passes the user's clicks,
    typing, keys and scrolling to it, and a Done button. What the user
    does is theirs to decide, so what the policy would hold goes through
    (what it refuses stays refused). The agent calls
    `wait({for: "handoff"})`, which returns once the user is done, with
    what happened meanwhile (`→ https://…/welcome (POST, 200)`) and the
    whole page; it never learns what was typed, and the journal keeps no
    hand-off input. For logins, checks meant for a person (ADR 0003), and
    anything else the agent should not do or see.
16. **Setting up a host.** `catpaw setup claude-code|codex|cursor` prints
    the command or configuration that registers `catpaw mcp --stdio`
    (with any `catpaw mcp` options after `--`); `--write` writes it:
    `.mcp.json` in the current project for Claude Code,
    `~/.codex/config.toml` for Codex, `~/.cursor/mcp.json` for Cursor,
    keeping what those files already hold.

## Consequences

Agents get one predictable shape for every answer, retry from a single
error without another snapshot, and pay for the tool list once per turn at
a known size. The cost is a protocol crate to keep in sync by hand, which
the example and size tests and `protocol.json` keep honest.
