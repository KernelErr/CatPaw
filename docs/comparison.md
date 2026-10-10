# CatPaw and Playwright MCP, task by task

Twenty tasks on sites made for automation practice (Sauce Demo, Books and
Quotes to Scrape, the-internet, TodoMVC, httpbin) live in
[`tests/tasks/`](../tests/tasks/), each with its recording and the
transcript an agent sees; CI replays them twice, offline, and both runs
must equal the transcript (`--record-har` and `--replay-har`, with
`--random-seed` and `--time-origin`, make a replay give the same results
byte for byte). What an agent reads over each task, against Playwright MCP
taking the same steps:

| task | CatPaw calls | CatPaw bytes (~tokens) | Playwright MCP calls | Playwright MCP bytes (~tokens) |
|---|---|---|---|---|
| books-category | 3 | 14960 (~4274) | 6 | 64648 (~18471) |
| books-pagination | 2 | 12576 (~3593) | 4 | 64942 (~18555) |
| httpbin-form | 6 | 1736 (~496) | 9 | 7477 (~2136) |
| internet-dropdown | 2 | 394 (~113) | 4 | 1937 (~553) |
| internet-dynamic | 3 | 535 (~153) | 6 | 2922 (~835) |
| internet-entry-ad | 2 | 844 (~241) | 4 | 2305 (~659) |
| internet-frames | 2 | 630 (~180) | 3 | 980 (~280) |
| internet-keys | 4 | 575 (~164) | 6 | 2302 (~658) |
| internet-login | 5 | 1309 (~374) | 6 | 2940 (~840) |
| internet-login-declined | 5 | 1136 (~325) | - | - |
| internet-prompt | 2 | 622 (~178) | 5 | 1968 (~562) |
| internet-upload | 5 | 1377 (~393) | 8 | 3239 (~925) |
| internet-windows | 3 | 479 (~137) | 5 | 2217 (~633) |
| quotes-js-pagination | 2 | 5768 (~1648) | 4 | 9394 (~2684) |
| quotes-login | 5 | 3849 (~1100) | 6 | 12497 (~3571) |
| quotes-scroll | 3 | 4354 (~1244) | 5 | 11922 (~3406) |
| quotes-table | 2 | 5116 (~1462) | 2 | 8658 (~2474) |
| saucedemo-checkout | 11 | 6251 (~1786) | 18 | 20992 (~5998) |
| saucedemo-sort | 4 | 5418 (~1548) | 7 | 12814 (~3661) |
| todomvc | 5 | 1306 (~373) | 10 | 9128 (~2608) |
| all | 71 | 68099 (~19457) | 118 | 243282 (~69509) |

Bytes are all the tool results an agent receives over a task (tokens
estimated at 3.5 bytes each). CatPaw's numbers come from the recordings;
`@playwright/mcp` 0.0.83 with headless Chrome took the same steps live on
2026-10-08 (the median of three runs). Playwright MCP keeps the page
snapshot in a file and links it from a result whenever the page changed;
an agent reads it to see the page and find its next target, so the file
counts too, as one more call. CatPaw answers an action with what changed,
and caps a whole snapshot at 4000 tokens, folding the rest for the agent
to open; its numbers include the calls that wait for the user's approval
(logins, the form post, the upload), which Playwright MCP does not make. A
task in which the user declines has no counterpart there, so the totals
leave it out. The tool list, a cost on every turn, is 12.1 KB for CatPaw
and 20.3 KB for Playwright MCP.

On content sites, measured the same way on the same day (these
recordings stay out of the repository), the budget does most of the
work: CatPaw folds a long article to 4000 tokens, which the agent opens
part by part, where Playwright MCP's snapshot of the same article runs to
160 000.

| task | CatPaw calls | CatPaw bytes (~tokens) | Playwright MCP calls | Playwright MCP bytes (~tokens) |
|---|---|---|---|---|
| Hacker News, second page | 2 | 14618 (~4177) | 4 | 98113 (~28032) |
| Wikipedia, search to an article | 2 | 11224 (~3207) | 4 | 569358 (~162674) |

`cargo run -p xtask --features engine -- tasks report --baseline tools/baseline/playwright-mcp.json`
regenerates the first table (with `--local`, the second, from tasks and
recordings of your own in `tests/tasks/local/`), and
[`tools/baseline/playwright-mcp.mjs`](../tools/baseline/playwright-mcp.mjs)
measures the baseline.
