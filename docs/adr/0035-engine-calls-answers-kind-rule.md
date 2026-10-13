# 0035. Engine CALLS answers link to functions, variables and types; the bridge drops enclosing-node landings

## Status
Accepted 2026-10-14 (GM-569, owner decisions D1-D4). Design note:
[`GM-569-kind-rule-engine-answers.md`](../design/GM-569-kind-rule-engine-answers.md).

## Context
The linker's kind rule lands a `CALLS` edge only on a `Function`. A semantic
tier's exact answer can correctly name something else: a TypeScript function
bound to a const (`export const handler = () => ..`), or a Rust tuple struct
called as its constructor (`Limited(..)`). The kind rule refused those
answers. Others of the refused answers were wrong: the LSP bridge lands a
location on the tightest node containing it, so a definition at a class
field or an object-literal method (no node of its own) landed on the class
or const around it, the encloser.

## Decision
- **D1: the bridge checks only non-Function landings.** A `CALLS` answer
  that lands on a node of any kind but `Function` is kept only when the
  location is at that node's own name: the first whole-word occurrence of the
  name at or after the node's range start. Otherwise the answer is treated as
  "nothing this index holds" and the structural edge stands.
- **D2: engine `CALLS` may land on a `Variable`.**
- **D3: engine `CALLS` may land on a `Type`.** The accepted kinds, in order
  of preference, are `Function`, `Variable`, `Type`: the first kind any
  candidate has decides, as for `SUPERTYPE_OF`.
- **D4: engine answers only.** The widened list applies to a `semantic`
  `CALLS` edge under a `qualifiedName` key (ADR 0034's engine answer).
  Structural `CALLS` edges still land only on a `Function`.

## Consequences
- Callers of const-bound functions and of tuple-struct constructors appear
  where a semantic tier ran.
- Enclosing-node answers no longer reach the linker for `CALLS`; a false
  "not at its name" only drops an edge, never adds a wrong one.
- A `qualifiedName` shared by a function and a type (TS declaration merging)
  still links the function; one shared by a const and a type links the const.
- Rust tuple-variant constructors (`Enum::Variant(..)`) have no node and land
  on the enum away from its name, so they stay unlinked.
