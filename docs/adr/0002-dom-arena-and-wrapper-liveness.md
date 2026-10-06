# ADR 0002: DOM ownership — Rust arena, one JS wrapper per node, tree tokens

- Status: Accepted (2026-10-06)

## Context

Two ways to own DOM nodes when the JS engine has a tracing GC:

- **JS-managed DOM** (Servo style): nodes live in `boa_gc::Gc<...>`; identity,
  expandos, detached-subtree liveness and listener cycles come for free.
  But every Rust traversal pays `borrow + downcast + GcBox` indirection,
  boa_gc is a thread-local non-generational non-incremental mark-sweep that
  would mark the whole DOM (100k+ nodes on real sites) on every collection,
  Stylo's `TElement: Copy` would have to wrap raw GC pointers, and the DOM
  would be welded to boa_gc, so a V8 backend would need a second memory model.
- **Rust-owned arena** (WebKit/Blink style): nodes live in a `SlotMap`, JS sees
  wrappers. Liveness must be engineered.

## Decision

1. One `SlotMap<NodeId, Node>` per engine thread (not per document, so
   `adoptNode` keeps ids stable). `NodeId` is index + generation; stale holders
   fail safely. Stylo, Taffy and html5ever work on `(&Dom, NodeId)` handles.
2. One wrapper per node, created on first exposure to script, in the realm of
   the node's document. `el === el` holds across same-origin frames.
3. **Tree tokens.** Every node tree (document tree, each detached subtree,
   template contents; shadow trees belong to their host's tree) has at most
   one `Gc<TreeToken>`. A wrapper strongly holds its token; the token
   strongly holds every wrapper of its tree; Rust holds tokens only weakly.
   Hence: expandos survive detach/re-attach (the wrapper is never recreated);
   a detached subtree stays alive iff any wrapper in it is reachable; `WeakRef`
   on nodes behaves like in browsers.
4. State that references JS values (event listeners, compiled `onX` handlers,
   custom-element instances) lives in GC-traced wrapper data, never in the
   arena. A Rust-held `JsObject` is a GC root; putting listeners in the arena
   would recreate IE6-style leaks.
5. Wrapper-less detached subtrees are freed on removal; wrappered ones in
   `TreeToken::finalize`. Insert/remove already walks the moved subtree for
   connectedness and custom-element reactions; the same walk moves wrappers
   between tokens.
6. Rust code that will later fire an event at a node or hand it back to script
   (pending fetches, Range, MutationRecord, focus) holds a `NodeHandle`
   (a rooted wrapper clone created on demand); everything else holds a bare
   `NodeId`.
7. Never allocate a JS object while holding an arena `RefMut`; GC may run at
   allocation and finalizers touch the arena. Allocation is reachable only
   through `&mut` methods on the DOM context type.
8. V8 later: same arena; the wrapper slot becomes `v8::TracedReference`, the
   token a cppgc `GarbageCollected` whose trace visits its wrappers.

## Consequences

Fast Rust-side style/layout traversals, browser-equivalent liveness
semantics, and a memory model that does not depend on the JS engine. The
price is discipline around rooting, enforced by API shape and by CI leak
tests (`force_collect` + arena census after each integration test).
