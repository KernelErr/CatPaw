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

## Parsing speed

Parsing was 55–70% of the CPU time of loading a single-page app. The
changes below, with those in `vendor/boa_ast`, make it about twice as
fast without changing any result: for the scripts of the recorded tasks
(jQuery, jQuery UI, Bootstrap, the Sauce Demo and TodoMVC bundles), the
bytecode, source maps and function source spans compiled from the parse
are identical, and so are the parse errors (message and position) of
thousands of randomly mutated variants of those scripts.

`src/parser/expression/primary/mod.rs`: a parenthesized expression was
parsed and then cloned into its `Parenthesized` node, so an IIFE
(`(function () { ... })()`, the wrapper of most libraries) copied its
whole body. The expression is now moved. Likewise
`src/parser/expression/assignment/mod.rs` and `update.rs` move the
left-hand side into the assignment or update target instead of cloning it
(`a.b.c = x` cloned `a.b.c`), and peek the operator without cloning the
token.

`src/parser/mod.rs`, `src/parser/function/mod.rs`,
`src/parser/statement/mod.rs`, the function and class parsers: early
errors that walk a whole statement list (`super` and `new.target` outside
functions, private names that no class declares, `{ a = 1 }` object
literals that never became patterns) ran for every function body and
again for the script, so a function nested n deep was walked n times. The
cursor now records whether a `super` keyword or a private identifier was
lexed and whether a `new.target` or a `CoverInitializedName` was parsed;
a walk for syntax that never appeared is skipped (a "seen" flag is never
reset, so a walk is only ever skipped when its answer is known).

`src/lexer/identifier.rs`, `src/lexer/cursor.rs`: identifier characters
were classified through ICU property tables, also for ASCII; ASCII now
takes a direct range check (a test checks it against the properties).
Identifier names are read into a reused buffer instead of a fresh
`String`, and are interned through a 2048-entry direct-mapped cache of
recently seen names (validated against the interned text, so the symbol
is always the interner's). `next_char` shifts its look-ahead without a
rotate.

`src/parser/expression/mod.rs`: the binary operators from `|` to `*` were
parsed by one function per precedence level (`BitwiseORExpression` down
to `MultiplicativeExpression`), eight nested calls, each moving a large
result, for every operand of every expression. They are now parsed by
precedence climbing in one function, which builds the same left-leaning
trees and keeps the quirks of the old code: the lexer goal is set to `Div`
before each operand that used to start a `MultiplicativeExpression`, a
relational expression starting with `#x in` is not continued by
relational or tighter operators, and an escaped `in` or `instanceof`
fails where the relational level would have looked at it. `||`, `&&` and
`??` keep their own parser (`a || b || c` nests to the right there).

`src/lexer/template.rs`: every template string was copied twice more to
build its raw and cooked forms, and each copy interned again, though a
string without a carriage return is its own raw form and one without a
backslash either is its own cooked form. Those are now taken as they are
(the symbols are the same ones the copies interned to). Minifiers such as
esbuild write many plain strings as template literals; in the Sauce Demo
bundle (a fifth of whose text is template literals) this was an eighth of
the parse.

`src/parser/cursor/`: `peek(0)` of an already buffered token returns
without entering the general look-ahead loop; line terminators and other
token kinds are recognised with `matches!` rather than the derived
`PartialEq`; `or_abrupt` no longer builds (and drops) an error for every
successful peek.

Measured on an Apple M5 (release build, CPU cycles of the parse, median
of repeated runs; wall times on the shared machine are noisier but agree):

| script | before | after |
| --- | --- | --- |
| Sauce Demo bundle (541 KB, module) | 329 Mcycles | 162 Mcycles |
| jQuery UI 1.11.4 (460 KB) | 137 Mcycles | 76 Mcycles |
| TodoMVC bundle (959 KB) | 264 Mcycles | 148 Mcycles |
| jQuery 1.11.3 (94 KB, minified) | 54 Mcycles | 34 Mcycles |
| all 15 scripts (2.6 MB) | 1034 Mcycles | 569 Mcycles |
