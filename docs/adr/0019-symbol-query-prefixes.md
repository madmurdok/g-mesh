# 0019. Plugins declare the prefixes that, stripped, leave one of their symbols

## Status
Accepted (2026-10-03)

## Context

A decorator is a plain function, indexed under its own name (`Component`).
Agents copy the use site and ask `find_definition("@Component")`. Nothing is
named that, every structural rung misses, and the semantic rung stops because
every shipped plugin refuses a leading `@` (ADR 0018). The answer was
`no symbol named '@Component' found` although the index holds the
declaration.

Measured on fixtures and requests
([`gm-482-decorator-queries.md`](../architecture/gm-482-decorator-queries.md),
section 1): every bare remainder that resolved did so on a structural rung
(`qualifiedName`, `name`); every semantic page for a bare remainder was wrong,
in both TypeScript and Python. The query carries no language, so in a mixed
project `@Component` can name a TypeScript and a Python decorator at once.

## Decision

- A plugin declares, in its own table, the literal prefixes that, stripped
  once, can leave a name of its language:

  ```toml
  [plugin.symbol_query_prefixes]
  strip = ["@"]
  ```

  A separate table rather than a key inside `[plugin.non_symbol_queries]`:
  that table denies unknown fields, so a new key there would make an older
  core reject the manifest, while an unknown table is ignored by it.
- Core treats each prefix as an opaque literal and holds no identifier rule.
- In `find_definition::resolve_symbol_name`, the query as typed runs every
  structural rung first (exact qualifiedName, name, qualifiedName suffix),
  unchanged. Only when all of them miss, each `(language, remainder)` pair is
  retried on those same three rungs, among that language's declarations only.
  A pair is skipped when the remainder is empty or is refused by that
  language's own `non_symbol_queries`, so `@@X` is stripped once, and
  `@scope/pkg` or `@src/app.ts` is never looked up. The rows are unioned over
  the languages: one resolves; several give the ranked candidate page, whose
  language filter sits in the SQL (`n.language IN (...)`) so `hasMore` and the
  cursor count only kept rows; none lets the ladder continue with the
  original query.
- The file-name rung, the import note and the semantic rung are not
  rewritten: they always see the original query.
- A rewritten answer keeps the rung's `resolvedBy` and adds
  `queriedAs: "<remainder>"`, so a client that reads `resolvedBy` as "exact"
  is still right.
- `search_code` does not apply the rewrite: one query vector serves every
  language, so a per-language rewrite has nothing to attach to.
- Validation, in `read_manifest` (and so `g-mesh plugins check`): an empty
  prefix, an unknown key, and a prefix that is not also in the same
  manifest's `non_symbol_queries.starts_with` are hard errors. The last rule
  means a query is never both "maybe a symbol as typed" and "a symbol once
  stripped", and never reaches the semantic rung with its prefix on.
- TypeScript and Python declare `strip = ["@"]`. Rust (attributes are
  `#[...]`) and Go (no decorators) declare nothing.

## Consequences

- `@Component`, `@register`, `@Widget.size` resolve where the remainder is a
  declaration of an opting language. A query that resolves as typed is never
  changed: answers move only from a refusal to an answer.
- In a mixed project where both languages opt in, `@Component` is the honest
  two-language page. A language that does not opt in never contributes rows.
- `@Component()` and `@retry(3)` stay refused: stripping a call would need
  syntax in core.
- `@angular/core`-style imports and `@scope/pkg` keep their answers.
- A coincidental exact hit is possible (`@types` finds something named
  `types`); it is an exact name match, and rare.
- `plugin.toml` is fingerprinted, so each index is rebuilt once.
