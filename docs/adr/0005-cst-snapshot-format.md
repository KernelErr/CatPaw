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
   name). When that replacement is certain (the node diffs paired with the
   stale one, or the only node of the same frame, role and name under a
   parent of the same role and name), an action on the stale ref goes to
   it and says so (`(e13 re-rendered → e52)`): the original "the agent
   must opt in" is relaxed for removals, never across navigations.
6. **Header keys in a fixed order**, each present only when it carries
   something (`settled` always): `# sN [diff-from=sM] tab doc
   [navigated-from] url title vp scroll [focus] filter [root] nodes settled
   [pending] [challenge] [budget=hit] [full=…]`. Dialogs are reported as
   consequence lines of the action that raised them, not in the header.
7. **Budget**: `maxTokens` (default 4000) at a fixed 3.5 bytes a token.
   Over budget, long lists show their first ten items and
   `[more=N nodes after eX]`; then containers fold, deepest first and
   those with the fewest things to act on first, into
   `eN role [collapsed=K]`; only then is the end cut
   (`[truncated: N more nodes]`). `snapshot({root: "eN"})` opens a folded
   container and `snapshot({root: "eL", after: "eX"})` the rest of a list,
   replacing the original `cursor=`. Diffs are computed on whole snapshots,
   never on folded ones.
8. **Frames** are shown under their `iframe` line,
   `e20 iframe "Payment" [frame=f2 origin=pay.example]` (the origin when
   it differs from the parent's), their lines one level deeper. Refs are
   unique across a tab's frames, and every tool takes a ref of any frame.
9. **Diffs** are what actions return (ADR 0006). They compare the trees
   two snapshots describe, elements by ref and texts by parent and
   position, and add `>` for moved nodes, `text[i]` for the i-th text of a
   parent, and `(replaces eN)` for a node the page rendered again (same
   place, role and name, new ref). Their header gives `diff-from=sN`,
   `url=(same)` when the URL did not change, and the counts
   (`changed=3 added=1 removed=1 unchanged=33`, or `no changes`). Diff
   lines use the compact form whatever form snapshots use.

## Amendment (2026-10-08): after the M3 review

A review of real transcripts found the format hiding content and paying
for news it did not have. These replace the points above they name.

1. **Text keeps its blocks and its length.** Texts of different blocks
   (`div`, `p`, `li`, table cells, anything laid out as a block) are
   separate lines; only text of one block is joined. A text stays whole
   in the model diffs compare, and is shown whole while the snapshot fits
   its budget; over budget, long texts are the first thing cut, to their
   start and `… [+N chars]`. A diff of a long text shows the stretch
   where it changed. (Replaces the 200-character cap of point 4.)
2. **Said once.** Besides point 4: a text that repeats its container's
   name (a fieldset's legend under its group) is dropped; a container
   named by the heading it starts with loses the name; an image beside
   the button or link of its name is dropped; a paragraph around a single
   element is that element; a heading that is one link is that link,
   marked with its level (`e66 link "A Light in the …" [heading=3]`).
3. **More state on the line.** A select lists its options when they are
   at most ten and short (`[options: "Name (A to Z)", "Price (low to
   high)"]`), else counts them; an element that takes text without being
   a form control (`contenteditable`) is `[editable]`, and a `textbox`
   target finds it. A label whose control is shown is never
   `[clickable]`: its clicks go to the control.
4. **Replacements are never rows alike.** A stale ref goes to the node a
   diff saw the page render in its place, or else to the only node of
   the same frame, role and name, under a parent of the same role and
   name, that was first shown after the stale one was last seen. A node
   shown alongside it (the next row of a list) is never its replacement,
   and is not suggested either. (Replaces the rule of point 5.)
5. **Header keys carry news.** A full snapshot's header gives its id and
   only what is not the usual: `tab` when the session has more than one,
   the URL unless the status line gave it, the title, `vp` when not
   1280x720, `scroll` when not 0,0, `focus`, `filter` when not
   `interesting`, `root`, `nodes` and `budget=hit` when the budget left
   nodes out, `settled=no` with `pending`, `challenge`, `full`. A diff's
   header gives what changed besides its lines (the URL, the title,
   scroll, focus other than onto the element acted on) and the counts
   that are not zero (`changed=1 removed=1`, or `no changes`).
   `diff-from`, `doc`, `navigated-from` and `url=(same)` are gone; focus on
   an element the page no longer shows is never given. (Replaces points 6
   and 9's header.)
6. **Budget.** A long list keeps as many items as fit, not ten, before
   `[more=N nodes after eX]`; `snapshot({after: "eX"})` continues the list
   `eX` is an item of without naming its root. (Amends point 7.)
7. **What a tab keeps.** A tab keeps its latest snapshot of each filter
   to diff against. (Replaces the "last eight" of point 4.)
