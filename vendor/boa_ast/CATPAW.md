# Vendored `boa_ast` 0.22.0

This is the `boa_ast` 0.22.0 crate from crates.io (Unlicense OR MIT, see
`ABOUT.md`), used through `[patch.crates-io]` in the workspace `Cargo.toml`
until a release carries the fix below.

## Changes from the published crate

`src/scope.rs`, `src/scope_analyzer.rs`: `this` read from an arrow function
nested in another arrow function resolved to the global object.

The scope analyser decides which functions must keep `this` in a function
environment (where closures can find it) by marking the first enclosing
function scope of any arrow that reads `this`. When that scope belonged to
an arrow function itself, the mark landed on a function with no `this` of
its own, and the non-arrow function whose `this` was actually meant never
got one, so `() => () => this` read the global object:

```js
class A { f() { return (() => () => this)()(); } }
new A().f() === globalThis; // true before the fix
```

Function scopes now record whether they belong to an arrow function, and
the mark goes to the nearest enclosing non-arrow function.
