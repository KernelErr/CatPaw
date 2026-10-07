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
   their own so that later versions can answer cancellations and
   elicitations while a call runs.
2. **Tools.** `navigate`, `snapshot`, `click`, `type`, `press`, `select`,
   `act` (hover, check, uncheck, focus, clear, scroll), `read` (markdown,
   text, links, forms), `screenshot`, `evaluate`, `tabs`; `wait`, `logs`,
   `handoff` and a hidden `session` follow. Frequent actions are tools of
   their own because their required fields differ: a schema that requires
   `text` catches the commonest small-model mistake (the right action with
   a field missing) before it reaches the page. Rare actions share `act`.
3. **Hand-written schemas.** Descriptions and schemas are static text in
   `catpaw-protocol`, with no version, host or port in them, each ending in
   one example call; `protocol.json` holds the same data and
   `cargo xtask protocol --check` keeps it current. Deriving them
   (schemars, as first planned) would put words in front of the model that
   nobody chose. A test parses every example with the tool's parameter type
   and keeps the whole list under 9000 bytes (6.4 KB for the eleven tools
   today). Unknown fields are refused with the names of the right ones.
4. **Targets** are one string: a ref `e12` (a whole snapshot line or
   `[ref=e12]` is accepted, since models copy those), `css:<selector>` or
   `xy:<x>,<y>`; `text:<label>` follows, refusing to guess between several
   matches.
5. **Result grammar.** The first line is `ok …`, `error <Code> …`,
   `needs_confirmation cN …` or `blocked <reason> …`; only `error` sets
   MCP's `isError`. An `ok` line echoes the element acted on as
   `eN role "name"` (the cheapest guard against acting on the wrong one)
   and how the page moved: `→ <url> (200)`, `(POST, 302)`, or
   `(same document)` for `pushState`. Lines starting with `!` report
   consequences in a fixed order (navigation failures, new tabs, closed
   tabs, dialogs, console errors). A fresh snapshot follows (a diff from
   M3 phase 1 on). Errors say what to try next on an `advice:` line; the
   codes are `BadArgument`, `NoTab`, `StaleRef`, `NotFound`,
   `NotActionable`, `Occluded`, `NavigationFailed`, `ScriptError`,
   `Unsupported` and `Crashed`.
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
8. **Confirmation (M3 phase 4).** Navigations and script requests that a
   policy holds are stopped at the network boundary after dispatch, so a
   confirmed action is released, never repeated; uploads (and `evaluate`
   under the strict policy) are stopped before dispatch. With MCP
   elicitation the user approves within the same call. Without it the
   result is `needs_confirmation cN` with a local approval page; the agent
   re-issues the same call with `confirmation: "cN"` once the user has
   approved. Approval needs a secret the agent never sees. Presets:
   `default` (POST navigations and uploads), `strict`, `open`.

## Consequences

Agents get one predictable shape for every answer, retry from a single
error without another snapshot, and pay for the tool list once per turn at
a known size. The cost is a protocol crate to keep in sync by hand, which
the example and size tests and `protocol.json` keep honest.
