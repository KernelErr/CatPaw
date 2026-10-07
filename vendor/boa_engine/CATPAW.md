# Vendored `boa_engine` 0.22.0

This is the `boa_engine` 0.22.0 crate from crates.io (Unlicense OR MIT, see
`ABOUT.md`), used through `[patch.crates-io]` in the workspace `Cargo.toml`
until a release carries the fix below. It goes with the vendored `boa_ast`.

## Changes from the published crate

`src/context/mod.rs`, `src/vm/mod.rs`, `src/error/mod.rs`: a wall-clock
deadline. `Context::set_deadline(Some(instant))` makes the interpreter
check the clock every 4096 instructions and stop with an uncatchable
`EngineError::DeadlinePassed` (a `try`/`catch` cannot swallow it) once the
moment has passed; `set_deadline(None)` lifts it. The published crate can
only count instructions, and only with the `fuzz` feature; a browser needs
to end a runaway task without knowing how fast the machine is. The cost
is one decrement and compare per instruction while a deadline is set.

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

`src/builtins/date/utils.rs`: `Date.parse` took only the Date Time String
Format and the two `toString` forms, so timestamps with microseconds
(`2025-10-01T12:34:56.789123+00:00`), a space instead of `T`, a `+0000`
offset, and the legacy `Oct 7, 2026`, `7 Oct 2026 10:00:00 GMT` and
`10/07/2026` forms were `NaN`, where every browser accepts them. Those
are now rewritten into the strict format and parsed by the same parser.
