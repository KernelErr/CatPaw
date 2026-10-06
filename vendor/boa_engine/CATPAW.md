# Vendored `boa_engine` 0.22.0

This is the `boa_engine` 0.22.0 crate from crates.io (Unlicense OR MIT, see
`ABOUT.md`), used through `[patch.crates-io]` in the workspace `Cargo.toml`
until a release carries the fix below. It goes with the vendored `boa_ast`.

## Changes from the published crate

`src/bytecompiler/declarations.rs`, function declaration instantiation,
step 28: in a function whose parameter list has a default value, a body
`var` sharing a parameter's name must start out with the parameter's value.
The compiler read that value through the var environment being set up, so
the read hit the uninitialised binding it was about to initialise, and
then stored `undefined` over whatever it had read:

```js
function f(a, b = 1) { var x = b; var b = 5; return x; }
f(1, 2); // threw "access of uninitialized binding"; now 2
```

The value is now read from the scope enclosing the var environment, where
the parameters live, and stored as read.
