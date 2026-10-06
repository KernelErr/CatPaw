# ADR 0005: CST — the CatPaw Snapshot Text format and stable refs

- Status: Accepted (2026-10-06)

## Context

Agents see pages best as a compact semantic tree. Playwright's aria snapshot
(`- role "name" [ref=eN]`) is the de facto format used by Playwright MCP,
Vercel agent-browser and others; LLMs read it well and existing prompts
assume it. Chromium-based tools recompute refs per snapshot and cannot say
precisely what changed.

## Decision

1. **CST is a strict superset of Playwright's aria-snapshot grammar.**
   `- role "name" [attr=value]`, indentation for nesting, `[ref=eN]`.
   Additions: a header line (`# s14 tab=t1 doc=d3 url=… title=… vp=… scroll=…
   filter=… nodes=38/412 settled=yes challenge=none dialog=none`),
   `[offscreen]`, `[occluded-by=eN]` (opt-in), `[frame=fN origin=…]` on
   iframe hosts with children inline, `[shadow]` on shadow hosts (closed roots
   included — the agent layer is privileged), `[value=…]` with passwords
   always `***`, `[collapsed=N nodes] [cursor=…]` for budget truncation.
   A JSON form exists for tooling.
2. **Refs are global per tab, monotonic, never reused.** A ref is allocated the
   first time a node is emitted and maps to the engine's generational node
   key. The DOM's disconnect hook marks refs `Stale{Removed}`; cross-document
   navigation marks them `Stale{Navigated}`. Using a stale ref returns the
   reason and an optional best-effort replacement the agent must opt into.
3. **Filters and budgets.** `filter: all | interesting | interactive`
   (default `interesting`), `root`, `depth`, `maxTokens`; wrapper-only
   `generic` nodes are elided and adjacent text merged.
4. **Diffs.** The server keeps the last eight snapshots per tab (refs and
   attributes only). `snapshot({diffFrom})` emits `~` changed, `+` added,
   `-` removed lines; a navigation yields a full snapshot instead.
5. **Alternative views** share the ref space: markdown (`[text](ref:e12)`),
   plain text, forms-as-schema, tables-as-JSON, HTML slices, text search.

## Consequences

Prompt compatibility with the Playwright ecosystem, diffs of a few hundred
tokens per step instead of full re-dumps, and refs that mean the same thing
for the life of a tab.
