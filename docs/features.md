# What works

What CatPaw does in v0.1, by area. What it does not do yet, or does
differently from a browser, is listed under
[Known gaps](architecture.md#known-gaps).

## Network

- HTTP/1.1 and HTTP/2 over rustls; redirects, cookies, gzip, brotli and
  zstd, encoding sniffing.
- HTTP `CONNECT` and SOCKS5 proxies (`--proxy`); cookie files kept between
  runs (`--cookie-jar`).
- Response bodies capped on the wire and after decoding
  (`--max-response-mb`).
- Loopback and private addresses refused unless `--allow-private-network`
  says otherwise.
- Web Bot Auth request signing (RFC 9421 HTTP Message Signatures),
  verified against Cloudflare's test endpoint, for deployers with a key of
  their own (`catpaw keygen`, `--bot-auth-key`, `--signature-agent`).

## HTML and CSS

- HTML parsed by html5ever into CatPaw's own DOM: 1858 of the 1968 WPT
  tree-construction tests pass; the rest are documented upstream gaps.
- Style resolved by Stylo from the UA style sheet and the page's linked and
  inline style sheets; inline and computed styles, and the CSSOM
  (`CSSStyleSheet`, `adoptedStyleSheets`, `CSS.supports`). Shadow trees
  are styled by their own and adopted style sheets, render through their
  slots, and show in snapshots and reading as they render.

## JavaScript

- Boa, vendored with the fixes described in `vendor/`, against bindings
  generated from Web IDL.
- Classic scripts (inline, external, `defer`, `async`, script-inserted,
  `document.write`) and module scripts (static and dynamic imports, import
  maps), run interleaved with the parser.
- An event loop with virtual time. A wall-clock script budget
  (`--script-budget`, 10 s by default) stops a runaway script.
- React, Vue, Svelte, Lit, htmx and Alpine sites run.

## DOM and Web APIs

- The core DOM: every HTML and SVG element interface, attributes,
  traversal, XPath, `Range` and `Selection`, `DOMParser`,
  `document.implementation`, the document collections and named access
  (`document.forms`, `document.myForm`); shadow trees and slots; custom
  elements; events.
- Mutation, intersection, resize and performance observers; timers;
  history; navigation timing; console.
- `fetch` and `XMLHttpRequest`, with CORS, preflights, redirects and
  referrer policy handled by the page as the Fetch standard has them;
  streams, `data:` URLs, `sendBeacon`, `URL`, storage, encoding.
- Web Crypto: `crypto.subtle` with HMAC, AES-GCM/CBC/CTR, PBKDF2, HKDF,
  ECDSA and ECDH on P-256 and P-384, RSA (PKCS#1 v1.5, PSS, OAEP), Ed25519
  and X25519, in raw, JWK, PKCS#8 and SPKI formats, over the RustCrypto
  crates; random values and digests.
- Canvas 2D, drawn with tiny-skia: paths, arcs, rounded rects, fills,
  strokes, dashes, clips, gradients, transforms, compositing, text in the
  same fonts as layout, `drawImage` from other canvases,
  `getImageData`/`putImageData`, `toDataURL`/`toBlob`.
- Dedicated workers (from same-origin, `blob:` and `data:` scripts;
  `postMessage` both ways, `importScripts`, `close`, `terminate`, errors
  relayed to the owner), run as realms of their own on the page's thread.
- `MessageChannel`, `MessagePort` and `BroadcastChannel` within a page;
  `WebSocket` over the same transport as HTTP (proxy, TLS, cookies and the
  private-network policy apply), text and binary, close codes and reasons.
- The font-loading API in its no-layout form.
- Media elements that play nothing: `<video>` and `<audio>` stay paused
  with no data, `canPlayType` answers "" and `play()` rejects with
  `NotSupportedError`; what scripts set (time, volume, rate, text tracks)
  reads back. No plugins and no PDF viewer (`navigator.plugins` is empty).

## Layout and screenshots

- Block, flex and grid boxes laid out by Taffy, and inline content shaped
  and line-broken by Parley over a bundled font set, only when something
  asks for geometry.
- The CSSOM View answers from that layout (`getBoundingClientRect`,
  `getClientRects`, `offset*`/`client*`/`scroll*`, `scrollTo`,
  `scrollIntoView`, `elementFromPoint`), as do `IntersectionObserver` and
  `ResizeObserver`.
- Screenshots (the `screenshot` tool, and `catpaw fetch --js --screenshot
  out.png [--full-page]`) paint backgrounds, borders, text and canvases with
  tiny-skia, and what form controls hold: values, checked boxes, the chosen
  option, a ring and a caret on the focused field.

## Input, frames and tabs

- Trusted pointer and keyboard sequences with focus, typing and activation
  (links, buttons, labels, `details`); form submission in every encoding,
  and the navigations that follow.
- Every `iframe` is a page of its own (document, scripts, event loop) on the
  same thread, sized by its element. Frames see each other only through
  `postMessage`, `parent`/`top`/`contentWindow` and `load` events, as
  cross-origin frames do.
- `window.open()` after a click or key press opens a page of its own with
  `opener` set (a new tab, for an agent); `window.close()` closes it.
- Session history across documents and within one (`pushState`).
- The Cloudflare Turnstile widget loads its challenge frame and completes
  against the test site key, and the page's callback receives the token.

## On the command line

`catpaw fetch <url>` prints a view of a page (`--snapshot`, `--markdown`,
`--text`, `--html`, `--links`, `--forms`); with `--js` it runs the page's
scripts first. `--action` acts on the page before the view (`click
<selector>`, `fill <selector> <text>`, `type`, `press`, `check`, `uncheck`,
`select`, `hover`, `focus`, `back`, `forward`); `--action "frame
<selector>"` addresses a frame for the actions and `--eval` that follow
(`frame top`, `frame parent`, `frame popup`). `--storage <file>` keeps
`localStorage` between runs, as `--cookie-jar` keeps cookies. `catpaw fetch
--help` lists the rest.

## Tests

web-platform-tests run in CI against recorded expectations, served by an
in-process stand-in for WPT's server and the Python handlers its fetch and
XHR tests use:

| suite | subtests passing |
|---|---|
| `dom` | 3019 of 4246 |
| `html/dom` | 582 of 1066 |
| `fetch/api` | 1908 of 2237 |
| `xhr` | 868 of 1203 |
| `css/cssom-view` | 485 of 1198 |

Much of the rest needs frames or workers the test harness does not run,
layout, or server behaviour the stand-in does not emulate. Twenty agent
tasks on practice sites replay from recorded traffic in CI, twice (see
[the comparison](comparison.md)).

## Not there yet

Images, gradients and rounded corners in screenshots; tables as a grid;
images' intrinsic sizes; media playback; IndexedDB; WebAssembly; the
Web Animations API; the members of HTML elements
that go beyond their attributes; and the [Known gaps](architecture.md#known-gaps).
