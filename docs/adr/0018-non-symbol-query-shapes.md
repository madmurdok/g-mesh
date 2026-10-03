# 0018. Plugins declare the query shapes that are never their symbols

## Status
Accepted (2026-10-03)

## Context

The name resolver's last rung, `find_definition::by_semantic_neighbours`,
offers semantic neighbours for a name nothing declares. Core refused it for
any query that started with `@` or contained `/`: an import specifier such
as `@excalidraw/element` scores 0.699 against unrelated code, above every
floor, because only doc comments and signatures are embedded. That rule was
TypeScript and Go import syntax written into core, and `search_code` holds a
second copy of it (`similarity::is_specifier_query`).

A lookup of stored module keys cannot replace the rule. The specifiers that
reach the rung are exactly the ones no table holds: relative specifiers and
packages the project never imports. Measured, a lookup turned 55 of 401
refusals into candidate pages on the TypeScript plugin's sources, and 1 of
335 on the Go plugin's
([`gm-474-qualified-name-segments.md`](../architecture/gm-474-qualified-name-segments.md),
section 3.5).

A single list that every plugin adds to would let one plugin's shapes
silence every other language.

## Decision

- Each plugin declares, in `plugin.toml`, the query shapes that are never
  its own language's symbols:

  ```toml
  [plugin.non_symbol_queries]
  starts_with = ["@"]
  contains = ["/"]
  ```

  Literal matchers, case-sensitive. Not regex: every shape needed so far is
  a prefix or an infix, and a literal cannot be subtly wrong (as a regex,
  `.` matches every query). A `matches` key can be added later without
  changing these two.
- Core applies them **per candidate**: a candidate of language L is dropped
  when the query matches L's shapes, next to L's similarity floor. When every
  discovered language refuses the query, the rung stops before embedding it.
  A plugin can therefore only affect its own language's answers.
- An absent table refuses nothing, and core has no default list.
- Validation is limited to an empty string (it would match every query) and
  an unknown key in the table (so a misspelt key fails loudly). An
  over-broad declaration only harms the plugin that wrote it, so there is no
  rule restricting which characters a shape may use.
- The map is built once per daemon from the discovered manifests and carried
  to the rung on `SemanticRung`.
- The four shipped plugins (TypeScript, Go, Rust, Python) declare `@` and `/`.
  No function, type or member of any of them is spelled with either.

## Consequences

- Core holds no language's syntax for this rung, and a new language adds no
  core code.
- With the shipped declarations every language refuses the same shapes, so
  answers are unchanged.
- When only some languages refuse a query, it is embedded where it was not
  before: that can be the first model load, and it adds latency.
- A candidate page can hold fewer than three candidates when one language's
  hits are dropped; the rung does not fetch more to fill it.
- A daemon that discovers no plugin refuses nothing by shape. It also has
  nothing indexed.
- An older core refuses a newer manifest that uses a key added to this table
  later.
- `plugin.toml` is fingerprinted, so adding the table rebuilds each index
  once.
- `search_code` still has its own copy of the two shapes until it moves onto
  these declarations.
- Follow-up: TypeScript's `node:` prefix.

## Alternatives considered

- **A lookup of stored module keys and paths.** Rejected on the measurement
  above.
- **A union of every plugin's shapes, applied to every candidate.** Rejected:
  one plugin could silence every language.
- **Regex matchers.** Rejected for now: no shape needs one, and a literal
  cannot match by accident.
