# 0024. A semantic tier refines an edge by binding a declaration, all or nothing per edge

## Status
Accepted 2026-10-04 (GM-348, owner review). Traces, the edit map and the
behaviours the tests pin:
[`gm-348-bridge-overload-binding.md`](../architecture/gm-348-bridge-overload-binding.md).

## Context
`CALLS` edges can carry `toDeclaration`, the ordinal of the overload a call
binds (`WireDeclaration`, `WireEdge.to_declaration`). Only the Node
TypeScript plugin fills it today, from its own semantic pass. The Rust
`LspBridge` that every SDK plugin's semantic tier uses had no way to:
its open sites either *replace* a structural edge (an answer where none was
emitted) or *contradict* one (Go's `placeholderCall`: a guessed call that turns
out to be a conversion), and both rules decide which target an edge has. An
overloaded call has no target problem. The structural edge names the right
function and cannot say which of its overloads the call binds.

The servers answer the question differently. tsserver's `definition` returns
the one bound overload. pyright's returns the whole set, implementation
included, in an order that is not stable across files, and its
`signatureHelp` does not track the binding either. Only `hover` at the call
carries it: pyright prints the bound overload's signature, through the same
printer that renders each declaration's own hover.

## Decision
We will add a third kind of semantic answer that **refines** a structural edge
instead of replacing or contradicting it: `OpenSiteKind::OverloadCall`, with
`replaces` required.

- **Refining never moves a call.** An answer either binds one declaration of
  the structural edge's own target by ordinal, or it changes nothing. An answer
  that lands elsewhere, on several nodes, or nowhere is not evidence against
  the structural edge. That is the difference from contradiction.
- **All or nothing per edge.** The structural edge is retracted only when every
  `OverloadCall` site that names it, in a file the pass finished, bound an
  ordinal; each binding becomes its own `CALLS` edge onto the target's
  placeholder, with `toDeclaration` set and the ordinal hashed into the id. If
  any site stays unbound or unanswered, the structural edge is re-sent and
  every binding onto it is dropped. A graph never shows a call both as bound
  and as unbound.
- **The ordinal comes from containment** in the target's `declarations`
  ranges, tightest match. A declaration with a body is never bound in a set
  that has bodiless ones: no call binds an implementation.
- **Hover is acceptable evidence only where a manifest says so, and fails
  closed.** With `overload_disambiguation = "hover"`, an answer naming several
  declarations is narrowed by comparing the call's hover with each candidate
  declaration's own hover, after whitespace normalisation, and equal once the
  candidate's first parameter is dropped (a bound receiver). Exactly one match
  binds. None or several leave the call unbound. Comparing hover against
  hover, not against the extractor's source signature, keeps both sides in one
  printer (`Union[int, str]` against `int | str` would never match).

Rejected: binding by `definition` order (pyright's order changes across files);
`signatureHelp` (pyright reports `activeSignature: 0` for every call);
comparing hover text with the extractor's signature (two printers); and
binding per site with the structural edge left in place (one call would be
both bound and unbound).

## Consequences
- The bridge settles three kinds of open site: replace (R1-R3 of
  `gm-489-structural-semantic-duplicate.md`), contradict, and refine. R2 does
  not apply to refinement, since its answer always lands on the structural
  target, and R3 never drops a bound edge.
- An untyped receiver call whose answer lands on an overload set is bound the
  same way, and keeps its plain edge when it cannot be bound. A receiver call
  that already carries a structural guess (`replaces`, emitted today only by
  the Rust plugin, which has no overloading) is not bound: it keeps that edge.
  A plugin with overloads that starts guessing receiver targets has to lift
  this limit.
- The pyright path depends on prose. A pyright upgrade that changes its printer
  turns bindings into unbound calls, not into wrong ones, and the real-server
  test makes that drift a red test.
- Cost: one `definition` per site whose target really is overloaded (the
  bridge filters the rest), plus `1 + k` hovers per site for pyright, with
  the `k` declaration hovers cached per pass.
- Out of scope: `.pyi`-only overloads (stubs contribute no declarations),
  `__init__` overloads reached through `C(...)` (a `REFERENCES` edge), Rust and
  Go (no overloading), and exposing `toDeclaration` through MCP.
