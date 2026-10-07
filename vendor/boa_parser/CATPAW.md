# Vendored `boa_parser` 0.22.0

This is the `boa_parser` 0.22.0 crate from crates.io (Unlicense OR MIT, see
`ABOUT.md`), used through `[patch.crates-io]` in the workspace `Cargo.toml`
until a release carries the fix below.

## Changes from the published crate

`src/parser/statement/mod.rs`, `src/parser/expression/primary/mod.rs`:
`using` as an identifier.

`using` is a contextual keyword: it starts a `UsingDeclaration` (explicit
resource management) only when a binding identifier follows it on the same
line. The parser took every statement that starts with `using` for a
declaration, and did not accept `using` as an identifier reference at all,
so a script that assigns to a variable of that name failed to parse as a
whole. jQuery UI's position plugin does exactly that:

```js
using = function( props ) { ... };
// SyntaxError: expected token 'identifier', got '=' in identifier parsing
```

A statement now starts a declaration only when `using` is followed, on the
same line, by an identifier (or `let`, `yield`, `async`, `of`); otherwise
it is an expression statement, and `using` is accepted wherever an
identifier reference is.
