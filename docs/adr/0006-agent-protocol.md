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
   notifications are accepted, and `notifications/cancelled` stops a call
   that waits: for the user's decision, a hand-off, an elicitation). It is
   written by hand rather than with an MCP crate: the surface is small,
   the same session will serve JSON-RPC over WebSocket later, and the
   crates still change often. Stdout carries protocol messages only;
   lines are read on a thread of their own, so that a call can ask the
   client (`elicitation/create`) and read its answer while it runs;
   requests that arrive meanwhile wait their turn, pings are answered at
   once (while a call runs on a page, as a `wait` of up to 120 s does,
   they are answered when it returns), and a cancelled call gets no
   answer. A line that is not UTF-8
   gets a parse error and the session goes on. SIGINT or SIGTERM (or
   stdout closing) ends the session once the call under way returns,
   writing what it keeps (recording, cookies, storage, profile); a second
   signal ends the process at once.
2. **Tools.** `navigate`, `snapshot`, `click` (any button, a double
   click, modifier keys), `type`, `fill` (several fields at once: text,
   checked or not, options), `press`, `select`,
   `act` (hover, check, uncheck, focus, clear, scroll — the window, or
   inside an element with a target and `dy` — upload, drag),
   `wait`,
   `read` (markdown, text, links, forms, tables, find, html, and download:
   the text of a file a navigation brought, which stays in memory),
   `screenshot`,
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
   and keeps the whole list under 11 000 bytes (10.9 KB for the fifteen
   standard tools today; `fill` earns its kilobyte by saving a turn per
   form). Unknown fields are refused with the names of the right ones.
4. **Targets** are one string: a ref `e12` (a whole snapshot line or
   `[ref=e12]` is accepted, since models copy those), `text:<visible
   text>`, `role "name"` (`button "Sign in"`: a snapshot line without its
   ref), `css:<selector>` (shadow trees and frames included; like a
   strict locator it names one element, the shown matches counting first,
   and several are `AmbiguousTarget`) or
   `xy:<x>,<y>` (into the frame under the point). Text and names are
   matched against what a snapshot shows, frames included; an exact match
   beats a partial one and a single control beats other matches, and
   anything still tied is `error AmbiguousTarget` listing up to five
   candidates: the server never guesses. A `role "name"` target names its
   element in full (case and spacing aside), or as the snapshot cut it
   short with `…`; a name found only inside another is listed, not taken
   (`error NotFound textbox "Password" names nothing in full; in part: e5
   textbox "Username Password"`), since it is another field as often as
   not. A role that matches nothing lists the elements of that name under
   other roles (`with that name: e5 link "Log in"`). Before acting, an
   element must be
   enabled (for clicks, typing, choosing and checking) and hold still while
   the page animates; one covered by something else is reported with the
   control that would dismiss the cover when there is one
   (`maybe dismiss it with e45 button "Accept all"`). `force: true` on
   `click` skips the checks and clicks the element itself.
5. **Result grammar.** The first line is `ok …`, `error <Code> …`,
   `needs_confirmation cN …` or `blocked <reason> …`; only `error` sets
   MCP's `isError`. An `ok` line echoes the element acted on as
   `eN role "name"` (the cheapest guard against acting on the wrong one),
   with where it is when other shown elements have its role and name
   (`(in e12 listitem "Hats")`, or `(after e14 button "View details for
   …")`), and how the page moved: `→ <url> (200)`, `(POST, 302)`, or
   `(same document)` for `pushState`. A lone change to that element ends
   the line (`ok type e2 textbox "Name" [value=- → Ada]`). Lines starting
   with `!` report consequences in a fixed order (navigation failures,
   what the policy blocked, new tabs, closed tabs, downloads, dialogs,
   requests the page made —
   a polling timer's routine ones only counted — console errors, and what
   kept the page busy when it did not settle). What changed on the page
   follows (decision 9). Errors say what to try next on an `advice:`
   line; the codes are `BadArgument`, `NoTab`, `StaleRef`, `NotFound`,
   `AmbiguousTarget`, `NotActionable`, `Occluded`, `NavigationFailed`,
   `ScriptError`, `Timeout`, `Unsupported`, `Busy` (the tab is with the
   user) and `Crashed`.
6. **Byte stability.** Header keys, attributes, consequence lines and diff
   lines come in fixed orders; ids are never reused; no wall-clock time
   appears; defaults are not printed (a snapshot's header gives only what
   is not the usual: a URL the status line did not give, the title, an
   unusual viewport, scroll, focus, a filter other than the default,
   counts when the budget left nodes out, `settled=no`); fixed words,
   codes and advice come from one table (`catpaw_protocol::wording`), and
   messages that carry details are put together the same way every time;
   estimators and limits are constants. A
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
   `--allowed-domain <domain>` limits the documents tabs may show: pages,
   popups and frames, every redirect hop judged as the first (anything
   else is `blocked policy: …`, or `! blocked frame …` for a frame). It
   does not limit the requests a page makes for its scripts, styles,
   images and data. Navigations and script requests (synchronous ones, and
   WebSocket connections, are refused rather than held: they cannot wait)
   are held where they would leave for the network, after the action ran,
   so an approved action is let go and never carried out again; uploads
   and `evaluate` are stopped before they run. Holds are numbered per
   page, and a confirmation is tied to the holds its action made:
   approving lets exactly those go (with their preflights and redirect
   hops), declining or running out drops them, and one whose holds the
   page dropped (it navigated, the frame asked for another navigation)
   no longer applies (`blocked superseded`). The result names what would
   happen, with secrets masked:
   `needs_confirmation c1: click e8 button "Login" would submit → POST
   https://…/authenticate (fields: username=tomsmith, password=***)`. A
   host that offers MCP elicitation asks its user within the same call
   (a boolean `approve`; declining gives `blocked user: declined c1`).
   Otherwise the result gives the address of a page on 127.0.0.1 where
   the user approves with a key kept in a file (made on first use in the
   user's data directory, or `--approval-key-file`; readable by its owner
   alone). The page is served on a port that stays the same between
   sessions while it is free (47115, or `--approval-port`), so the browser
   that approved once keeps the key in its storage for that origin —
   never in a cookie, which every port of the host would receive; the
   server answers several connections at a time, refuses oversized
   requests, other host names and other origins, and stops with the
   session. The agent never sees the key: no result prints it, its
   browser refuses private addresses, and an upload of the key file (or a
   link to it, or a copy) is refused. The result gives the exact call to
   repeat with `confirmation: "c1"`; the repeat must match it or it is
   refused, a repeat sent before the user decided waits for the decision
   (up to 45 seconds, then `(still pending)`), and confirmations run out
   after ten minutes. With elicitation, only an explicit yes approves; a
   cancelled question is `blocked user: cancelled`, and a host that does
   not answer falls back to the approval page. This bounds an agent that
   holds only the browser tools; an agent with a shell is bounded by its
   host's own permission prompts.

9. **Diffs after actions.** An action answers with what changed since
   the tab's last snapshot (ADR 0005, amended): `~` changed, `+` added
   with its subtree, `-` removed, `>` moved, `(replaces eN)` for a node
   the page rendered again. A new document, a page that changed more than
   it stayed (the diff over 60% of the whole), or no snapshot to compare
   with gives the whole snapshot, its header saying why (`full=navigated`,
   `full=large`, `full=no-baseline`). `snapshot: "full" | "none"` on any
   action says otherwise. A tab keeps its latest snapshot of each filter.
10. **Settling.** An action is done when the page has settled under a
    policy, not when its event loop is empty (real pages never empty
    it). The policy waits for requests the page waits on, timers due
    within a second, animation frames that change the document, and a
    document quiet for 100 ms of page time. It does not wait for
    analytics and telemetry hosts, beacons, requests started by polling
    timers (one that set itself again, from its own callback, five times
    with 100 ms or more; a debounce set again from each key press is still
    waited for), WebSocket handshakes and style sheets
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
    `.zst`) in a file only its owner can read: entries in the order their
    requests started, bodies whole (base64 when not UTF-8). Credentials
    stay out: request cookies, credentials and signatures are not
    written, form fields that look secret (passwords, tokens, card
    numbers, one-time codes, PINs) are written as `redacted`, and the
    values of response cookies and credential headers become
    `redacted-<hash>` placeholders (equal values, equal placeholders) that
    a replayed session keeps and sends back; `cargo xtask tasks lint`
    fails on a recording that holds any of them. `--replay-har <file>`
    answers from it with no network. A request matches on method, URL and
    body (as sent, not as written), or failing that on method, host, port
    and path; each recorded answer is given once, in order, however it
    was matched, the last one repeating when they run out; a request the
    recording lacks fails (`--replay-misses-live` sends it instead), and
    so does one whose body an older recording left out. Answers arrive at
    once, in the order the page asked for them. `--random-seed` gives
    each document, frame and worker sequences of its own for
    `Math.random`, `crypto` and Web Crypto keys; `--time-origin` fixes
    where the page clock starts; dates show UTC (or `--timezone`) and
    `Intl` the page's language, never the host's; cookies go out in
    RFC 6265 order; cookie lifetimes count from the recorded time. A
    replay therefore gives the same results byte for byte, on any
    platform. WebSockets are refused at once during replay; they are not
    recorded yet.
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
    and decision; `--flight-screens` adds a screenshot per page action
    (in a profile's journal too). A journal asked for must open, or the
    session does not start; it is readable by its owner alone, and a write
    that fails is said once in a result. It never holds cookies or the
    bodies of held requests, and text typed into a password field becomes
    its length. `--profile <dir>` keeps the cookie jar, `localStorage`,
    saved checkpoints and the journals between sessions, written after
    every call that acts and when the session ends; one session at a time
    may use a profile (a lock). The `session` tool saves a checkpoint
    (cookies, storage, each tab's URL and scroll), restores it (checked
    first: a damaged one leaves the session as it was; tabs load again,
    refs start afresh) and lists them.
15. **Hand-off.** `handoff({reason})` gives the user the current tab on a
    page served by the same local server as the approval page (its address
    carries a token; acting needs the approval key as well, so the link
    alone lets nobody act): a screenshot of the tab, kept current, that
    passes the user's clicks, typing, keys and scrolling to it, and a Done
    button. Whatever the page would send while the user has the tab (a
    form they submit, a request a script makes) waits on that page, which
    names it, with what they typed masked, for the user to allow or block:
    a click of theirs never sends what a script the agent planted would
    send behind it. What the agent's own calls held stays held, and what
    is still held when the tab comes back is asked about as after any
    call. Until the tab is given back, the agent's calls on it (switching
    to it, and repeating a confirmed call there) are `error Busy`. The
    agent calls `wait({for: "handoff"})`, which returns once the user is
    done (or after 50 seconds, to be called again; at most 30 minutes; it
    answers pings, stops when cancelled, and notices a tab that closed),
    with what happened meanwhile (`→ https://…/welcome (POST, 200)`, a
    popup the user opened) and the whole page. What the user typed into
    fields shows `***` in snapshots, reads and confirmations until the
    agent sets the field itself (adding to it keeps it masked), and an
    `evaluate` in a page that holds it asks the user first, since a script
    could read it; the journal keeps no hand-off input. A hand-off the
    user keeps past 30 minutes lapses, and the tab is the agent's again.
    For logins, checks meant for a person (ADR 0003), and anything else
    the agent should not do or see.
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
