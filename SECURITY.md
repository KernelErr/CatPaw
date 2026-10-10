# Security

CatPaw runs the scripts of pages it is sent to, holds an agent's
browsing session, and serves pages on 127.0.0.1 for its user (the
approval and hand-off pages). Reports about any of these are welcome.

## Reporting a vulnerability

Report it privately on GitHub:
<https://github.com/KernelErr/CatPaw/security/advisories/new>. Please do
not open a public issue for it. Say which version (`catpaw --version`)
and platform, and how to reproduce it. A fix ships in a release, and the
advisory is published with it.

## Supported versions

While CatPaw is a preview (0.x), only the latest release gets fixes.

## What counts

- Anything sent on the user's behalf without their approval when the
  policy asks for it: a submission, an upload, a request held for
  confirmation.
- The agent getting the approval key or a page's pass, or acting on the
  approval and hand-off pages without them; those pages answering other
  sites (DNS rebinding, cross-origin requests).
- A page reaching private addresses or local files against the policy.
- Journals, recordings or results keeping what they say they mask
  (passwords typed, what the user typed in a hand-off, secrets in
  requests and cookies).
- Memory-safety bugs. A crash on a crafted page is a bug: report it as
  an issue unless it looks exploitable.

Not in scope: what an agent does through tools other than CatPaw's (an
agent with a shell is bounded by its host's permissions), sites choosing
to block CatPaw, and the limits listed under
[Known gaps](docs/architecture.md#known-gaps) (the local pages assume a
computer not shared with other accounts; through a proxy, names are
resolved by the proxy).
