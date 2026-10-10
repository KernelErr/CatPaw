# ADR 0003: Identity, Web Bot Auth and challenge handling

- Status: Accepted (2026-10-06)

## Context

Agents get blocked by bot management. The cooperative path exists and is
maturing: HTTP Message Signatures for bots ("Web Bot Auth") is now an IETF
working-group item (`draft-ietf-webbotauth-httpsig-protocol`), Cloudflare
verifies it, lets operators self-register as *Signed Agents*, exposes the
result to site owners as WAF fields, and runs a Browser Developer Program for
new engines. Cloudflare's documentation also states that automated browsers
are not supported for solving production challenges and that custom engines
have limited support; niche engines have been caught in challenge loops.

The alternative path — impersonating Chrome's TLS/HTTP2/JS fingerprints and
buying CAPTCHA solutions — works until it is detected, after which the
project's reputation and every cooperative channel are gone.

## Decision

1. **Honest identity.** CatPaw sends its own User-Agent (configurable by the
   operator), consistent `navigator.*` values, and `navigator.webdriver ===
   true` as the WebDriver spec requires for automation. The engine ships no
   profiles that imitate other browsers' fingerprints and does not tune
   TLS/HTTP2 parameters to match them.
2. **Web Bot Auth in the fetch layer.** Each context may carry an Ed25519 key
   and a key-directory URL; requests (navigations, subresources, redirects)
   are signed per RFC 9421 with `Signature-Agent`, `tag=web-bot-auth`, short
   `expires`, and a nonce. `catpaw keygen` produces keys and the directory
   document.
3. **Operational path.** The project publishes a stable UA and key directory,
   registers as a Signed Agent (intermediary), and applies to the Browser
   Developer Program. Deployers are told to do the same for their own
   identities.
4. **Be a real browser.** Challenge scripts run like on any other browser:
   cross-origin iframes, postMessage, partitioned cookies, DOM storage, Web
   Crypto, Canvas 2D, consistent timing. A Turnstile checkbox is clicked
   through the ordinary user-action path. The UA never changes mid-session.
5. **Surface, don't hide.** Challenge responses (`cf-mitigated: challenge`,
   challenge-platform scripts, Turnstile iframes, other vendors' markers) are
   reported as a structured `challenge` event. The agent may wait, click, or
   request a **human hand-off** (remote view + input relay).
6. **Out of scope, permanently:** CAPTCHA-solving services, fingerprint
   impersonation, residential-proxy rotation features. PRs adding them are
   closed with a pointer to this ADR.

## Consequences

CatPaw will not pass every site. That is the site owner's decision, and the
project says so plainly. Pass rates are measured on a Cloudflare zone the
project controls, with each challenge mode enabled, and reported as numbers
rather than promised.

## Amendment (2026-10-10): who signs and registers

CatPaw is software people run themselves. A key of the project's would
have to ship in every copy, where anyone could take it, and a signature
made with it would say nothing about who sent a request. Decision 3 is
replaced:

3. **Operational path.** The project signs nothing and registers with no
   one. Its User-Agent, `CatPaw/<version> (+https://catpaw.sh/bot)`,
   points site owners to a page that says what CatPaw traffic is and how
   to block or allow it. Deployers who want a verified identity make a key
   of their own (`catpaw keygen`), publish its directory on their own
   domain, sign with it, and register as Signed Agents themselves (Direct
   for their own use, Intermediary when they run CatPaw for others); the
   project documents how. The Browser Developer Program was not taking new
   applications when checked (2026-10-10); the project applies if it
   reopens.
