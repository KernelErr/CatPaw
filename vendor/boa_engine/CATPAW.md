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

`src/realm.rs`, `src/builtins/math/mod.rs`: `Realm::set_random_seed(Some(seed))`
makes that realm's `Math.random` a repeatable SplitMix64 sequence (for
recorded runs that must replay byte for byte); `None` restores `rand`.
The state belongs to the realm, so documents, frames and workers each
keep their own sequence, as the specification asks of distinct realms.

`src/context/icu.rs`, `src/context/mod.rs`, and the `DefaultLocale()`
callers in `src/builtins/intl/locale/utils.rs`, `src/builtins/array/mod.rs`,
`src/builtins/typed_array/builtin.rs` and `src/builtins/string/mod.rs`:
`Context::set_default_locale(Some("en-US"))` gives the locale `Intl` and
the `toLocale…` methods use when script names none. The published crate
always takes the host's (`sys_locale`), so the same page printed numbers
and dates differently from one machine to the next, and told sites what
the machine was set to.

`src/context/icu.rs`, `src/context/mod.rs`,
`src/builtins/intl/date_time_format/mod.rs`:
`Context::set_default_time_zone(Some("+05:30"))` gives the time zone
`Intl.DateTimeFormat` and the `toLocale…` methods of `Date` use when
script names none (`SystemTimeZoneIdentifier()`; an offset, or a name the
time zone data knows). The published crate always takes UTC there, even
when the host hooks give `Date` another zone, so `getHours()` and
`toLocaleTimeString()` disagreed. `resolvedOptions().timeZone` of an
offset zone also lost its sign below an hour (`-00:30` read `+00:30`).
`Temporal.Now` still takes UTC.

## Sorting with any comparator

`src/builtins/array/mod.rs`: `SortIndexedProperties` (behind
`Array.prototype.sort`, `toSorted` and the typed array sorts) sorted with
`slice::sort_by`, which since Rust 1.81 may panic when the comparison is
no total order. Pages pass comparators that are none (`() => Math.random()
- 0.5` shuffles), and the panic took the page down. A stable merge sort of
its own (`merge_sort_by`) now sorts: it stays bounded whatever the
comparator answers, and the result is then some order of the items, as
ECMAScript allows.

## A stack overflow script can catch

`src/vm/mod.rs`, `check_runtime_limits`: reaching the recursion or stack
size limit threw an uncatchable engine error, which ended the page's
whole task. It is now `RangeError: Maximum call stack size exceeded`, as
browsers throw: pages recurse until that throws, to measure the stack or
to stop a deep recursion, and catch it. Recursing again forever from the
`catch` is stopped by the host's wall-clock deadline. (Boa's own tests of
uncatchable recursion errors no longer hold.)

## More dates `Date.parse` takes

`src/builtins/date/utils.rs`, `normalize_legacy`: besides the forms
above, `YYYY/MM/DD` and `YYYY-MM-DD` dates with a time after a space and
a trailing zone (`2026/10/10 15:51:46+00:00`, `2026-10-10 15:51:46
UTC`), a zone right after the time, and parenthesized text left out, so
that `Date.parse(date.toString())` gives the date back, as ECMAScript
requires (`… GMT+0000 (Coordinated Universal Time)`).

## The legacy static properties of `RegExp`

`src/builtins/regexp/mod.rs`, `src/realm.rs`: `RegExp.$1` to `$9`,
`input` (`$_`), `lastMatch` (`$&`), `lastParen` (`$+`), `leftContext`
(`` $` ``) and `rightContext` (`$'`), as in the TC39 legacy RegExp features
proposal: `RegExpBuiltinExec` records each successful match in the realm
(`Realm::legacy_regexp`, the string and the ranges; substrings are made
when read), and the accessors on the constructor read it, throwing a
`TypeError` on any other receiver. Pages that format dates with
`RegExp.$1` failed without them.

## `Intl.DateTimeFormat.prototype.formatToParts`

`src/builtins/intl/date_time_format/mod.rs`: the published crate has
`format` only. `formatToParts` formats as `format` does and collects the
parts ICU4X marks while writing (`writeable::PartsWrite`): a `datetime`
part's name is ECMA-402's `type` (`year`, `month`, `day`, `weekday`,
`hour`, `minute`, `second`, `dayPeriod`, `era`, `timeZoneName`, …), and
the text between parts is `literal`. Fractional seconds stay part of
`second`. Sites that format dates by parts failed without it.

`date_time_format/options.rs`, `best_fit_date_time_format`: `year:
"numeric"` (also the default) asks ICU4X for the whole year
(`YearStyle::Full`, or `WithEra` with an `era` option) and `2-digit`
months, days and hours for column alignment, which pads them: `1/1/1970`
and `01/01/1970` as browsers give, where the published crate gave
`1/1/70` for both.

## Parsing and compiling speed

With the parser changes in `vendor/boa_parser` and `vendor/boa_ast`, these
keep the compiled bytecode identical and make loading a script cheaper:

`src/optimizer/mod.rs`: `Script::parse` runs constant folding and strength
reduction over every expression statement, each pass repeated until a walk
changes nothing. One postorder walk already reaches that fixed point (a
node is rewritten after its children, into a literal, into one of its
already rewritten children, or into a node the pass leaves alone), so the
repeat walk never found anything; for a library wrapped in a function
expression it walked the whole script twice more. Each pass now walks
once; debug builds still take the second walk and assert that it changes
nothing. Statistics count the passes as before. This is 6–7% of parsing a
classic script.

`src/bytecompiler/declarations.rs`, `src/module/source.rs`: the bytecode
compiler, like the scope analyzer, copied every function declaration and
variable initializer (whole function bodies) at each level of nesting to
list a body's var-scoped declarations; it now borrows them
(`var_scoped_declarations_ref`). Module var names were deduplicated by a
linear search over a list of strings, quadratic for bundles with thousands
of top-level names; they now go into a hash set.

`src/bytecompiler/mod.rs`: `Sym::to_js_string` narrows short Latin-1
names on the stack instead of in a temporary `Vec`.

Together these take 7–14% off parsing and compiling the scripts of the
recorded tasks (CPU instructions; most for the Sauce Demo module bundle).
