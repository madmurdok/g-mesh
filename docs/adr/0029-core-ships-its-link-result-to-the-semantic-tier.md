# 0029. Core ships its link result to the semantic tier, and the tier decides agreement with it

## Status
Accepted 2026-10-08 (GM-531, owner review). Options considered, ordering,
the edit map and the tests:
[`gm-531-reexport-placeholder-address.md`](../architecture/gm-531-reexport-placeholder-address.md).

## Context
The SDK's `LspBridge` decides whether a semantic answer agrees with the
structural edge a site `replaces` (R2 in
[`gm-489-structural-semantic-duplicate.md`](../architecture/gm-489-structural-semantic-duplicate.md)).
It compared the answer with the edge's own target and with the id the answer
would get. A structural edge through a `pub use` re-export points at a
placeholder addressed at the re-exporting container, and core's linker moves
it onto the declaring item by walking the re-export. The answered declaration
has its own address, so neither test matched: an answer that agreed with core
was treated as a contradiction. The bridge retracted the structural edge and
recorded a semantic one, and a reparse whose pass then failed left two rows
for one call.

The plugin cannot see the link: it extracts files against its own module
model and never sees other crates' declarations, and the bridge by design
never guesses. Only core knows where it linked each edge.

## Decision
We will send core's link result with every `semanticPass` request and let the
bridge decide agreement with it.

- **Core ships, it does not re-resolve.** The request carries `linkedEdges`:
  every structural edge of the plugin's language in the pass's scope that
  the linker moved (`edges.linkedFrom IS NOT NULL`, `source = 'syntactic'`),
  with its current `toId`. It is read in the unit that committed the reparse,
  or after the whole-project link, so it describes the text the plugin
  answers for. The linker itself is unchanged.
- **A third R2 test.** An answer agrees when core linked the replaced edge
  onto exactly the answered declaration. Agreement records nothing, the
  structural edge is re-sent (R1), and no semantic edge is created that a
  failed later pass would have to clean up.
- **The SDK replaces the map on every pass.** A request without the field
  clears it; an edge it does not name falls back to the other two tests.

## Consequences
- Every case where the linker and the language server agree is agreement,
  whatever re-export (named, glob, cross-crate, multi-hop) the linker walked.
  A genuine contradiction behaves as before.
- Agreeing rows read `source = syntactic` instead of `semantic`, as for the
  other R2 cases.
- Applies to every SDK plugin (Rust, TypeScript, Python) with no
  language-specific code. The Go tier ignores the field.
- One indexed query per pass, and a payload that grows with the number of
  linked edges (about 10k on g-mesh itself for a whole-project Rust pass).
  The count is logged once per whole-project pass. If it ever matters, a
  manifest capability can gate the field to the languages that need it.
