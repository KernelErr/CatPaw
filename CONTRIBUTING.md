# Contributing to CatPaw

Thanks for your interest. A few ground rules keep the project coherent:

- **Read the ADRs first.** `docs/adr/` records the decisions that shape the
  codebase (JS engine, DOM ownership, identity policy, bindings, snapshot
  format, the agent protocol). Changes that contradict an ADR need a new
  ADR, not a drive-by PR.
- **No stealth features.** CatPaw identifies itself honestly and implements
  Web Bot Auth. Pull requests that add browser-impersonation fingerprint
  profiles, CAPTCHA-solving integrations, or similar evasion features will be
  closed. See ADR 0003.
- **Spec references.** Code that implements a WHATWG/W3C algorithm links the
  spec section in a comment and follows the spec's step numbering where it
  helps readers.
- **Tests.** Engine behaviour is covered by web-platform-tests subsets and
  the html5lib tree-construction suite, run from a pinned, sparse WPT
  checkout (`tests/wpt.lock`), and by the agent task set in `tests/tasks/`.
- **Generated code.** Bindings and `protocol.json` are generated and checked
  in; CI fails if the checked-in output is stale.
- **Style.** `cargo fmt` and `cargo clippy --workspace --all-targets` must be
  clean; warnings are errors in CI.

By contributing you agree that your contributions are licensed under
Apache-2.0 OR MIT, at the user's option.

## Before sending a change

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo clippy -p xtask --features wpt,engine --all-targets -- -D warnings
cargo run -p xtask -- bindgen --check
cargo run -p xtask -- protocol --check
cargo test --workspace
cargo run -p xtask --features engine -- tasks replay --twice
```

## Developer commands

- `cargo xtask wpt-fetch` fetches the web-platform-tests commit pinned in
  `tests/wpt.lock` into `tests/wpt-src/` (not checked in).
- `cargo xtask tree-construction` runs the html5lib tree-construction suite
  from that checkout against `tests/tree-construction-expectations.txt`.
- `cargo run -p xtask --features wpt -- wpt --include dom --include html/dom …`
  runs testharness.js tests in CatPaw pages, each in a process of its own,
  served by an in-process stand-in for WPT's server, against
  `tests/wpt-expectations/<dir>.txt` (the known failures;
  `--update-expectations` rewrites them).
- `cargo xtask bindgen` regenerates the JavaScript bindings from the Web IDL
  corpus and `crates/catpaw-webidl/bindings.toml` (`--check` verifies the
  checked-in output, `--list <Interface>` shows what an interface offers);
  `cargo xtask protocol` regenerates `crates/catpaw-protocol/protocol.json`.
- `cargo run -p xtask --features engine -- tasks …` works on the agent task
  set: `replay` (against the transcripts; `--update-expectations` rewrites
  them), `record` (live, keeping the recording when two offline replays
  agree), `lint`, and `report` (the table in
  [docs/comparison.md](docs/comparison.md)).
- `cargo run -p xtask --features engine -- snapshot-bench` measures snapshot
  sizes on live pages.
- `cargo about generate --locked --fail -c release/about.toml -m
  crates/catpaw/Cargo.toml release/about.hbs -o THIRD-PARTY-LICENSES.html`
  lists the licenses of what the binary includes (release archives carry
  it); CI fails on a license that `release/about.toml` does not accept.

Releases: a tag `vX.Y.Z` runs `.github/workflows/release.yml`, which builds
and packages every target and publishes the release with its `SHA256SUMS`
and the `CHANGELOG.md` section of that version. Run by hand, the workflow
builds and packages without publishing.
