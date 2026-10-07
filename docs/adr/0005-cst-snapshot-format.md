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

## Amendment (2026-10-07): the compact form, and what M3 changed

Measured on real pages, the aria form spends a quarter of its bytes on
`- ` and `[ref=…]`, and link URLs were 40% of a Hacker News snapshot. The
format the agent tools return is therefore tuned for tokens, with the
Playwright form one option away.

1. **Compact lines by default**: `e12 link "upvote" [disabled]`, two
   spaces of indentation per level, `text: …` for page text. A lone text
   child goes on its parent's line, as in Playwright's form
   (`e13 paragraph: In stock`). `format: "aria"` gives the strict
   Playwright superset (`- link "upvote" [ref=e12]`) and stays chosen for
   later results.
2. **Optional attributes are off**: `href`, `src` and `description` appear
   only when asked for (`attrs: ["href"]`, or `read({view: "links"})`).
   `[value=…]` is left out when a field is empty (its placeholder shows
   instead); passwords are `***`.
3. **`[clickable]`** marks a generic element with `cursor: pointer` (and a
   parent without it), an `onclick` attribute or a click-like listener, and
   nothing clickable inside: the clickable `div` of single-page apps gets a
   ref.
4. **Less to read, nothing lost to act on**: generic wrappers and the
   text-level roles (`strong`, `emphasis`, `code`, `mark`, `time`,
   `subscript`, `superscript`) dissolve into their surroundings; empty
   elements are dropped; a list item around a single element is that
   element; a label's text is dropped next to the control it names; inside
   an element to act on, text and images that only repeat its name are
   dropped; a container whose children already show its name loses the
   name. Text leaves without a letter or digit are dropped, separators
   trimmed, adjacent DOM text nodes joined (`$<!-- -->29.99` is `$29.99`).
5. **Refs are keyed by frame, document epoch and node**, since a new
   document's arena reuses keys. A stale ref reports `removed`,
   `navigated away` or `frame closed`, with the live ref that most likely
   replaced it (same frame, role and name, parent of the same role and
   name). Retargeting automatically when that replacement is unambiguous
   relaxes "the agent must opt in" (M3 phase 2).
6. **Header keys in a fixed order**, each present only when it carries
   something (`settled` always): `# sN [diff-from=sM] tab doc
   [navigated-from] url title vp scroll [focus] filter [root] nodes settled
   [pending] [challenge] [budget=hit] [full=…]`. Dialogs are reported as
   consequence lines of the action that raised them, not in the header.
7. **Budget**: `maxTokens` (default 4000) at a fixed 3.5 bytes a token.
   Until containers collapse (M3 phase 2), a snapshot over budget ends in
   `[truncated: N more nodes]` and advice; `root` shows one subtree. The
   `cursor=` continuation of the original decision becomes `root`/`after`.
8. **Diffs** (M3 phase 1) add `>` for moved nodes, `text[i]` for the i-th
   text of a parent, and `(replaces eN)` for a re-rendered node.
