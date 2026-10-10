# Vendored `stylo_taffy` 0.3.0-beta.2

This is the `stylo_taffy` 0.3.0-beta.2 crate from crates.io (part of
[Blitz](https://github.com/dioxuslabs/blitz), MIT OR Apache-2.0 OR MPL-2.0,
see `LICENSE-MIT` and `LICENSE-APACHE`), published as `catpaw-stylo-taffy`
(the library keeps the name `stylo_taffy`). It converts Stylo's computed values into Taffy
styles.

## Changes from the published crate

`Cargo.toml`: depends on `stylo` 0.22 (the published crate pins 0.20, which
cannot coexist with the 0.22 the rest of the workspace uses) and enables
Taffy's `flexbox_balance` feature.

`src/convert.rs`, `flex_wrap`: Stylo 0.22 represents `flex-wrap` as a set
of flags (`wrap`, `wrap-reverse`, `balance`) rather than an enum; the
conversion reads the flags and maps `balance` to Taffy's balanced wrapping.
The Taffy `Style` initialiser sets `flex_line_count` to 1, Taffy 0.14's
field for a property Stylo 0.22 does not have yet.

A later published release built against Stylo 0.22 can replace this copy.
