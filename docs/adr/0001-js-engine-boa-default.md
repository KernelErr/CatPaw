# ADR 0001: JavaScript engine — Boa by default, V8 as an optional backend

- Status: Accepted (2026-10-06)
- Deciders: project owner

## Context

CatPaw needs a JavaScript engine that runs modern sites. The candidates in
October 2026:

| Engine | Binding | Pros | Cons |
|---|---|---|---|
| Boa 0.22 | `boa_engine` (pure Rust) | 95.6 % test262, no C/C++ toolchain, embeddable (`Context`, realms, host hooks, `JobExecutor`, `Clock`, `JsProxyBuilder`) | interpreter only; third-party benchmark ~200× slower than V8, ~3× slower than QuickJS-NG; `Context` is `!Send`; no cross-thread interrupt |
| V8 152 | `v8` / `deno_core` | fastest, most compatible | prebuilt C++ static library, large; isolate/handle-scope model |
| QuickJS-NG | `rquickjs` | small, complete | C toolchain; no JIT either |
| SpiderMonkey | `mozjs` | battle-tested in Servo | C++ build, Servo-specific |

Existing Rust headless browsers (Moli, Obscura) picked V8. A browser whose
identity is "pure Rust, from scratch" loses that identity if V8 is the only
engine; a browser that cannot hydrate a React app in reasonable time loses
its users.

## Decision

1. `catpaw-js` defines an engine-neutral `JsRuntime` surface (callbacks,
   promises, exceptions, interrupts, structured-clone visitor). `catpaw-web`
   only talks to JS through these traits.
2. `catpaw-js-boa` is the default backend and the only one in M1.
3. The binding generator (ADR 0004) emits per-backend glue (`emit_boa`, later
   `emit_v8`) from the same IDL corpus, so adding V8 does not touch DOM code.
4. A `js-v8` feature is planned as a first-class backend; once it lands both
   backends run the same CI suites and hydration benchmarks.
5. Small Boa gaps (atomic interrupt flag, `JsProxy::target()`, V8-style
   `Error.prototype.stack`) are carried in a fork (published as `catpaw-boa-engine` and its
   siblings) and upstreamed.

## Consequences

- Default builds need only a Rust toolchain and cross-compile trivially.
- JS throughput is the project's top technical risk; hydration timings of a
  Next.js and a Vue application enter CI in M1 so regressions and the
  Boa/V8 gap are visible numbers, not folklore.
- Switching the default engine later is a configuration change, not a rewrite.
