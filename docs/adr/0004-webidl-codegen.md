# ADR 0004: WebIDL-driven binding generation

- Status: Accepted (2026-10-06)

## Context

A usable browser exposes ~250 Web IDL interfaces. Boa's `Class` trait has no
inheritance, Boa 0.22 exposes no custom internal object methods, and no
WebIDL-to-Rust binding generator exists outside Servo (SpiderMonkey-specific)
and wasm-bindgen (the other direction).

## Decision

1. **IDL corpus.** A vendored, curated snapshot of `@webref/idl` plus
   `idl/overlay/*.idl` with CatPaw extended attributes (`[Reflect]`,
   `[ReflectDefault=]`, `[CatPawUnimplemented]`). Parsed with `weedle2`.
2. **Generation.** `cargo xtask bindgen` runs the pipeline
   parse → merge partials/mixins → resolve types → effective overload sets →
   emit. Output is checked in; CI fails on drift. Not `build.rs` (opaque,
   re-runs on clean builds) and not proc-macros (overload sets, mixins and
   prototype chains need the whole corpus).
3. **Backends.** `emit_boa` today, `emit_v8` later. For each interface the
   generator also emits a backend-neutral `trait <Interface>Impl` listing
   exactly the functions the glue calls, so a missing member is a compile
   error.
4. **Hand-written side.** Node-derived interfaces are free functions on arena
   handles in `catpaw-dom` (inheritance = the base function takes the same
   `NodeId`, dispatching on node kind). Non-node hierarchies (Event → UIEvent
   → MouseEvent) use one native struct per concrete interface embedding its
   base, generated `As<Base>` accessor traits with blanket impls, and a
   generated `InterfaceId` brand check.
5. **Prototype chains.** Per realm, prototypes are built with
   `ObjectInitializer` and interface objects with `ConstructorBuilder::inherit
   + custom_prototype`, so `Object.getPrototypeOf(HTMLInputElement) ===
   HTMLElement` and `instanceof` work unmodified. The prototype table lives in
   `Realm::host_defined`. Wrappers use `from_proto_and_data(proto_from(new_target), …)`,
   which also yields custom-element upgrades.
6. **Exotic objects** (HTMLCollection, live NodeList, NamedNodeMap,
   DOMStringMap, Storage, named properties on form/select/window,
   WindowProxy, Location) are `JsProxyBuilder` proxies with native traps.
7. **Conversions** follow WebIDL: dictionaries → structs, unions → enums with
   the distinguishing algorithm, enums → TypeError for arguments, sequences
   via the iterator protocol, callback interfaces look up `handleEvent` at
   call time, Promise-returning operations convert throws into rejections.

## Consequences

Large but mechanical initial effort; afterwards adding an interface is "write
the Rust impl" only. Unimplemented members are counted at runtime so crawl
telemetry can prioritise work.
