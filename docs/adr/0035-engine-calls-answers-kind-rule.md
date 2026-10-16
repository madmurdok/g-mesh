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
  "nothing this index holds" and the structural edge stands. GM-593 holds a
  `CALLS` answer landing on its own caller (a `Function`) to the same check:
  such a landing is the encloser fallback for a declaration with no node (a
  default import, a nested fn) unless it is at the caller's name (recursion).
  The same check settles a `CALLS` answer whose locations land on different
  nodes (GM-596): only the locations at their node's own name count, and one
  node left is the target. TypeScript answers a call onto a const produced
  by a factory (`const f = throttle(g)`) with the const's name and the call
  signature's declaration inside the factory's body, which names no node.
  Locations at two different names (`cfg` or overload twins) still record
  nothing.
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
- Python `Foo()` and TS `new Foo()` on a class with no explicit constructor
  now produce `CALLS` onto the class, so `find_callers` on a class lists its
  constructions, which some users read as "references". None measured on
  py-requests (0 refusals).
- Nested-fn and local-binding calls no longer produce `CALLS` self-loops on
  their encloser; recursion stays (GM-593).
- A direct named import of a const-bound function (`import { handler }`,
  `handler()`) now links too (GM-596): the bridge asks a `CALLS` hop whose
  file declares the name only as a non-function (and a `SUPERTYPE_OF` hop
  whose file declares it as neither type nor function), and a name-keyed
  placeholder and a qualified-name-keyed one never share an id - the label
  renders a `qualifiedName` key as `=<key>` - so the engine answer is
  recorded instead of being taken for agreement with the structural edge.
- An engine implementor answer (`SUPERTYPE_OF`) whose file already holds a
  structural `SUPERTYPE_OF` edge from the same type that reaches the anchor
  records nothing (GM-596): the edge targets the anchor, core linked it onto
  the anchor, or its placeholder is name-keyed in the anchor's scope with the
  anchor's `qualifiedName` as the name. The `=<key>` label made such an
  answer a second row next to the structural one; the structural row stays,
  so these edges show `source=syntactic`.
