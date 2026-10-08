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

## Parsing speed

Parsing is most of the CPU time of loading a script-heavy page, and scope
analysis (run as part of every parse) was a quarter of it. These changes
keep every result the same (the bytecode compiled from a corpus of real
bundles is byte-for-byte unchanged) and make it cheaper; see
`vendor/boa_parser/CATPAW.md` for the numbers.

`src/scope.rs`: a scope found a binding by comparing its name with every
binding's name in turn, each comparison two indirect calls into the string
type. Every identifier the escape analysis visits walks the scope chain
that way, and the top level of a bundled module holds thousands of
bindings, so lookups were quadratic. Bindings now keep a hash of their
name (over UTF-16 code units, so the Latin-1 and the UTF-16 form of a name
hash alike), a scan compares hashes first, and a scope with more than
eight bindings indexes them in an open-addressing table. Declaration order,
binding indices and `Debug` output are unchanged.

`src/scope_analyzer.rs`: the escape analysis built a `JsString` (two
allocations) for every identifier it visited, only to look it up; it now
looks up the interned UTF-16 text directly. Module var names are
deduplicated with a hash set instead of a list.

`src/operations/mod.rs`: `var_scoped_declarations` returns copies of the
declarations, which means copying every function declaration and every
variable initializer (whole function bodies) at each level of nesting. The
new `var_scoped_declarations_ref` borrows them; the scope analyzer uses it,
and the copying version is now built on it.

`src/lib.rs`: `Sym::to_js_string` narrows short Latin-1 names on the stack
instead of in a temporary `Vec`.

`src/expression/operator/assign/mod.rs`, `src/expression/parenthesized.rs`:
`AssignTarget::from_expression_owned` / `from_expression_simple_owned` and
`Parenthesized::into_expression`, so that the parser can move an
assignment's left-hand side into its target instead of cloning it.
