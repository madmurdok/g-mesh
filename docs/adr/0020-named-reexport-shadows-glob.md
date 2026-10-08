# 0020. Plugins declare whether a named re-export shadows a glob

## Status
Accepted (2026-10-04)

## Context

The symbol linker (`graph::symbol_links`) looks a name up through a scope's
re-export rows when the scope does not declare it. A row either forwards one
name (`use a::T;`, `export { T } from "./a"`, `from a import T`) or a whole
scope (`*`).

GM-479 made a named row win over every `*` row of the same scope, before any
visibility check, even when the named row leads nowhere. That is the
language's rule in Rust (an explicit `use` shadows a glob; rustc never
considers the glob's item) and in ES modules (a local or explicit export beats
`export *`). It is not Python's: there the later import binds the name, so
`from a import T` followed by `from b import *` that also exports `T` leaves
`b.T` bound, and the GM-479 rule linked `a.T` - a wrong edge. Before GM-479,
named and glob rows sat at the same depth with no winner: both are followed,
and when both reach a declaration the linker's ambiguity rule refuses to move
the edge. The rows carry no import order, so core cannot apply Python's own
rule; no winner is the answer that does not pick one wrongly.

Core holds no language's syntax or rules (ADRs 0015, 0018, 0019), so it cannot
know which languages have this shadowing.

## Decision

- A plugin declares it in its own table:

  ```toml
  [plugin.reexports]
  named_shadows_glob = true
  ```

  A separate table, as in ADR 0019: it denies unknown fields, and an older
  core ignores an unknown table. Absent means false.
- `read_manifest` parses it into `PluginManifest::reexports`;
  `daemon::manifest::link_rules` collects the languages that declare it into
  `graph::symbol_links::LinkRules`. Every composition root that builds an
  `IndexStore` from discovered plugins passes them with
  `IndexStore::with_link_rules` (the daemon, `g-mesh init`, `g-mesh reindex`,
  a workspace reindex's staging store copying the live store's,
  `g-mesh plugins check` with the manifest under check). A store built
  without them links with no shadowing.
- `Resolver::hops` marks each re-export row with its own language's rule, and
  `Resolver::walk` drops a scope's `*` rows only when a named row of a
  language that declares the rule is present. Rows of a language with no
  rule, or with no known language, keep the base behaviour: named and `*`
  rows are followed at the same depth.
- Rust and TypeScript declare `named_shadows_glob = true` (the installed
  TypeScript manifest written by `scripts/bundle-plugin.sh` too). Python and
  Go declare nothing.

## Consequences

- Python gets its pre-GM-479 behaviour back: an explicit import and a star
  import that both reach a declaration of the name give two candidates, and
  the edge is left where it is rather than moved to the explicit one.
- Rust and TypeScript keep GM-479's behaviour unchanged.
- A new language plugin gets no shadowing until it opts in, so the default
  can only leave an edge unmoved, never move it wrongly.
- `plugin.toml` is fingerprinted, so each index is rebuilt once.

## Addendum (GM-496): the second rule, `later_import_binds`

Python now declares the opposite answer instead of none:
`[plugin.reexports] later_import_binds = true`. In one module the later of a
named import and a star import that provides the name binds it, so the
linker orders one scope's rows by the row node's start position (compared
within one file only) and keeps the latest row that binds the name: a named
row always binds, a `*` row binds only when a sub-walk from it finds the
name. Star-vs-star also resolves to the later provider. Rows from different
files, or two at one position, keep the base behaviour. A manifest setting
both keys to `true` is refused. Rust and TypeScript are unchanged. The Python plugin
places a named (`__all__`) re-export node at the import statement that bound
the name, so its position is that statement's order. Design:
`docs/architecture/gm-496-python-later-import-binds.md`.
