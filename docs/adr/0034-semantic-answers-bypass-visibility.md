# 0034. Semantic exact answers of CALLS and REFERENCES bypass the linker's visibility check

## Status
Accepted 2026-10-14 (GM-567; GM-519 owner decision D1 "B: семантика мимо
видимости", GM-567 owner decisions D1-D3). Design note and measurements:
[`GM-567-semantic-answers-bypass-visibility.md`](../design/GM-567-semantic-answers-bypass-visibility.md),
[`GM-519-member-visibility-findings.md`](../results/GM-519-member-visibility-findings.md).

## Context
The symbol linker (`core/src/graph/symbol_links.rs`) admits a candidate only
when it is visible to the requester: `public` always, `container(c)` from `c`
or below, `file` only in a container scope from the candidate's own file.
The rule applied to every edge, whoever sent it.

A semantic tier's answer is different from a structural guess: the engine
resolved a name written in code it compiled and named one declaration by
`qualifiedName`. The visibility check then refused correct answers: on
excalidraw, 62 `CALLS` edges onto TS non-exported (`file`) declarations,
among them every caller of a non-exported class's methods; in Rust, a
`macro_rules!` macro used from a sibling module through `#[macro_use]`.

## Decision
A `semantic` edge of kind `CALLS` or `REFERENCES` whose placeholder has a
`qualifiedName` key is linked without the visibility check. Everything else
stays checked.

- **Option B over C.** C admitted `file`-visible nodes in a file scope for
  any `qualifiedName` key; B trusts the engine, by edge source. B also fixes
  the Rust macro case and leaves structural `qualifiedName` keys checked.
- **D1: exact answers only.** `name`-keyed semantic edges (the engine only
  said which file to look in) stay checked, keeping the self-import guard: a
  barrel round trip to the importer's own file must not make its
  non-exported namesakes candidates.
- **D2: `CALLS` and `REFERENCES` only.** Go's `SUPERTYPE_OF` is computed by
  pairing every type with every interface, not by resolving written code;
  without the check, every unexported interface would collect implementers
  from every package.
- **D3: only the member is exempt.** Under a re-exported head
  (`use crate::named::T; T::m()`), the head is walked by name with visibility
  checked, like any name walk; only the member lookup beside it is admitted.
- **Per edge source, not per placeholder.** One placeholder can carry a
  structural and a semantic edge of the same kind; each `(kind, source)` group
  is decided and moved on its own.

## Consequences
- An engine's wrong answer onto a non-visible declaration now links, bounded
  by the exact key, the kind rule and the exactly-one rule.
- Code that does not compile (a TS `private` member used from another file)
  links if the engine answers it; the edge describes what the code says.
- `link_diff` seeds a `file`-visible declaration exactly at its own file, so a
  declaration arriving after the placeholder still links it.
- A future `implements`/trait-impl engine answer onto a non-visible type stays
  refused until `SUPERTYPE_OF` is reconsidered.
