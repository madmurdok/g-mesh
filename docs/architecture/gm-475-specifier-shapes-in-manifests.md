# GM-475: "never a symbol" query shapes move from core into plugin manifests

Status: design, revised in S6 after the owner's decisions. Not implemented.

## 1. Decision

`find_definition::is_module_specifier` and `similarity::is_specifier_query`
(search_code) both hard-code `starts_with('@') || contains('/')`. S1 showed
that a lookup of stored module keys cannot replace that rule
(`gm-474-qualified-name-segments.md` §3.5). The owner decided:

1. Each plugin declares, in `plugin.toml`, the query shapes that are **never
   its own language's symbols**. Core applies them **per candidate**: a hit
   of language L is set aside when the query matches L's shapes. A plugin can
   therefore only affect its own language.
2. Both call sites move: find_definition and search_code.
3. Python gets a new leading-`.` shape, in its own slice, with its own
   measured before/after. Every other slice must show 0 diffs.
4. TypeScript's `node:` prefix is a separate backlog task.

This extends ADR 0015 ("core never parses separators"): core then holds no
language's syntax for either tool.

## 2. Field

```toml
[plugin.non_symbol_queries]
# A query that starts with or contains any of these is never a symbol of
# this language: core drops this language's candidates for it.
starts_with = ["@"]
contains = ["/"]
```

- Name: **`[plugin.non_symbol_queries]`**. It says what the shapes mean to
  this plugin ("queries that are not my symbols") rather than why they occur
  (import syntax). Alternative: `[plugin.not_a_symbol]`.
- Literal matchers, not regex. `regex` is not a direct core dependency
  (only transitive, via `tokenizers` and `tree-sitter`). Every shape needed
  so far is a prefix or an infix. A literal cannot be subtly wrong: as a
  regex, `.` would match every query. A `matches` key can be added later
  without changing the meaning of the existing two.
- Case-sensitive. find_definition matches the raw name. search_code matches
  the trimmed query, as today.

### Shipped declarations

| Plugin | `starts_with` | `contains` | Slice |
|---|---|---|---|
| typescript | `@` | `/` | S7 |
| go | (none) | `/` | S7, see decision D1 |
| rust | `@` | `/` | S7 |
| python | `@` | `/` | S7 |
| python | `.` (added) | | S9 |

### Can a symbol of each language contain these? (measured)

Measured on g-mesh's own index, a mixed Rust/TS/Go/Python corpus: every
`nodes` row, and separately every embedded row (`vectors` join `nodes`),
grouped by language and kind.

| Character | Where it occurs in `name`/`qualifiedName` | Embedded (a semantic candidate)? |
|---|---|---|
| `@` | nowhere, in any language or kind | no |
| `/` | `File` nodes (every language: the path); Go `Module` (package paths, 569 of 851, e.g. `github.com/example/app/cmd`); Rust `Module` (`orphan:core/build.rs`); TS `Module` placeholders (`./version.generated`) | only Rust and Python **File** nodes (222 and 13) |
| leading `.` | one TS `Module` placeholder (`./version.generated`); no Python row | no |
| `#` | TS private members (`C#m`) | yes, but no shape uses `#` |

Consequences:

- **Go and `/`.** Package paths contain `/`, but they are `Module` nodes,
  which are not embedded and are answered at the first structural rung
  (exact `qualifiedName`) before the semantic rung is reached. S1 recorded
  the same. Go functions and types (`T.M`) never contain `/`. The filter
  matches the *query*, never the candidate's name, so a Go symbol cannot be
  dropped for its own spelling.
- **File nodes and `/`.** A path query that names an indexed file resolves at
  the first rung. A path query that names no indexed file is refused today
  and still is, because Rust and Python refuse `/` too.
- **TS `#` and decorators.** No shape uses `#`, so private names are
  unaffected. A decorator is a use, never a declared name, so no symbol
  starts with `@` in any language.
- **Python `.`.** No Python name starts with `.`, so S9 can only remove
  Python candidates for relative-import queries such as `.models`, never a
  real one.

### D1: Go does not declare `@`

The owner's table gives Go only `/`. In S1's harness (`plugins-root` = TS +
Go), the Go corpus contains `@Component`, `@property`, `@` and three scoped
packages. Today all of them are refused at the guard. With Go declaring no
`@`, Go hits for `@Component` would be offered whenever they clear Go's floor
(0.57). S1's lookup variant also let these queries through, and none of them
regressed, so they scored below the floor on that corpus. 0 diffs would then
hold there only because of the floor. **Recommended: Go also declares
`starts_with = ["@"]`.** No Go identifier contains `@`, and the four shipped
plugins would then refuse the same two shapes, so 0 diffs holds by
construction. Owner decides.

## 3. How the shapes reach both tools

- **Manifest.** `NonSymbolShapes { starts_with, contains }` in
  `daemon::manifest`, as a new `PluginManifest.non_symbol_queries` field.
  It is parsed from an optional table and is empty when the table is absent.
- **Map.** `QueryShapes(HashMap<String, NonSymbolShapes>)`, keyed by
  language, with two methods:
  - `refuses(language, query)`. A language with no entry refuses nothing.
  - `refused_by_all(query)`: the map is non-empty and every discovered
    language refuses the query.
- **Built once.** It is built in `GMeshMcpServer::new` from the registry's
  `discovered`, which never changes while a daemon runs, and held as
  `Arc<QueryShapes>`. Discovery roots are global, and each project in a
  multi-project folder has its own daemon, so in practice every project gets
  the same map. The front daemon answers no index tool.
- **find_definition: carried on `SemanticRung`.** Every path into the
  semantic rung already carries `SemanticRung`: the five tools through
  `anchor::resolve` / `resolve_symbol_name`, plus the CLI and plugin-check
  through `resolve_lazily`. Both variants gain `shapes: &'a QueryShapes`.
  Handler signatures between the server and the rung stay the same.
  `SemanticRung::off()` carries the shipped map (`include_str!` of the four
  manifests), so existing tests keep today's behaviour.
- **search_code: passed into the verdict.** The handler passes `&QueryShapes`
  into `similarity::verdict` and `partial_verdict`.

## 4. Where the filter sits

### find_definition: `by_semantic_neighbours`

1. **Short-circuit, before deferring.** If `shapes.refused_by_all(name)`,
   return `None`, exactly where `is_module_specifier` returns today. Nothing
   is embedded and the model is not loaded. With the shipped declarations
   this is today's behaviour, byte for byte.
2. **Per hit, after the search.** Next to the per-language floor:
   `.filter(|hit| hit.score >= floor(&hit.language) && !shapes.refuses(&hit.language, name))`.
3. The search still asks for `SEMANTIC_CANDIDATES` (3) and filters
   afterwards, so a page can now hold fewer than 3 candidates when one
   language's hits are dropped. Fetching more before filtering would change
   pages in the 0-diff slices, so it is not done.

Cost of the per-hit path: when only some languages refuse a query (Python
`.` after S9, or `@` if D1 stays as the owner's table has it), the query is
embedded where today it is not. That can be the first model load, and it
adds latency. The answers are unaffected.

### search_code: `similarity::verdict`

Today the decision is per query, and the result is one page-level `noMatch`
annotation. The rows are returned either way. The new rule decides per hit,
but the output stays page-level:

- Core keeps the language-neutral parts: trim, non-empty, not prose
  (`is_prose_query`).
- **Specifier verdict (`QueryIsAPathOrPackage`).** A non-empty first page
  gets it when every row's language refuses the query. An empty page gets it
  when `refused_by_all` holds, which keeps today's "a specifier still gets
  its verdict on an empty page".
- **Floor verdict.** In `below_floor`, a row whose language refuses the
  query counts as below the floor. It cannot be evidence of a match.
- No rows are dropped. search_code never drops rows today, and dropping them
  would change pages.

With the shipped declarations, a page's languages either all refuse `@`/`/`
or none of them do, so both verdicts equal today's. For Go and `@`, see D1.

## 5. Defaults and validation

- A plugin without the table contributes no entry and refuses nothing for
  its own candidates. It can never affect another language.
- With no plugins loaded, the map is empty and `refused_by_all` is false, so
  nothing is refused. Nothing is indexed either. There is no core fallback
  list, because that would put syntax back into core.
- Validation in `read_manifest`. These are hard errors that name the
  manifest path, which `g-mesh plugins check` inherits because it calls
  `read_manifest`:
  1. An empty string. `contains = [""]` would refuse every query for that
     language.
  2. An unknown key in the table (`#[serde(deny_unknown_fields)]` on this
     table only), so that `start_with` fails loudly. Trade-off: an older core
     refuses a newer plugin that uses a key added later.
- **The identifier-only rule and the whitespace rule are dropped.** They
  existed because a global union let one plugin silence every language. Per
  language, an over-broad declaration only harms the plugin that wrote it.
- `g-mesh plugins list` and plugin-check's report print the declaration
  (`non_symbol_queries: starts_with=@ contains=/`, or `none`). plugin-check
  cannot test this behaviourally, because it runs with
  `EmbeddingPipeline::disabled()`.

Versioning: `plugin.toml` has no schema version, and `protocol_version` is
for the wire. The change is compatible both ways, because core ignores
unknown tables and an absent table defaults to empty. No plugin version bump
is needed: TS 2.4.0 and Go 0.4.0 are already new in 3.19.0, Rust and Python
follow 3.19.0, and no plugin binary changes. `plugin.toml` is fingerprinted,
so every index is rebuilt once after the upgrade. 3.19.0 already does that
(GM-476).

## 6. Slices

| Slice | Content | Model | Gate |
|---|---|---|---|
| S7 | Field, validation, `QueryShapes`, `GMeshMcpServer` field, `SemanticRung` carrier, find_definition short-circuit and per-hit filter; TS/Go/Rust/Python manifests at `@`, `/`; `plugins list` line; docs and ADR | opus | 0 diffs (§7 A, C) |
| S8 | search_code: `verdict`, `partial_verdict` and `below_floor` take `&QueryShapes`; `is_specifier_query`'s syntax removed from core | opus | 0 diffs (§7 B) |
| S9 | Python `starts_with = ["."]`, plus its measured before/after | opus | diffs listed and explained, Python candidates only (§7 D) |
| S10 | Verify: fresh agent rebuilds every control in its own worktree and reruns §7 | opus | each control fails or differs |

Docs (in S7, extended by S8/S9):
- `multi-language-plugins.md` ("`plugin.toml` additions"): the table, with
  its semantics, default and validation.
- `plugin-modularity.md`: one line in the manifest sketch, pointing there.
- The doc comments of `is_module_specifier` and `is_specifier_query` move to
  the new predicates. They keep their measurements (0.699 vs 0.566, 42
  points of recall, 35/70 vs 0/2,275, S1's 55/401 and 1/335).
- The plugin SDK needs nothing: it reads only `extensions` and
  `exclude_dirs`.

## 7. Measurement

Every measurement compares a `before` arm (core at the merge-base) with an
`after` arm, on the same plugin root, plus a **no-shapes control arm**: the
`after` binary over copies of the manifests with the table removed (copied
`plugin.toml`s; symlinked `dist/`, `node_modules/` and plugin binaries). The
control must differ from `before`. A control that shows 0 diffs means the run
never reached the filter, for example because the model was missing
(`G_MESH_MODEL_DIR`). Use a fresh `G_MESH_HOME` per arm, because a manifest
edit changes `indexer_version` and forces a re-walk.

- **A. find_definition on S1's corpora.** `drive.py` and `compare.py` from
  `scratchpad/gm475/`, on `ts-corpus` (401 queries) and `go-corpus` (335).
  Expected: 0 diffs; control at least 55 (TS) and at least 1 (Go).
- **B. search_code on the gm-468 sweep queries.** Extend `drive.py` with a
  search_code mode that records the full response (rows and `noMatch`). Run
  it over `eval/embedding/queries/<corpus>.jsonl` and `queries/mechanical/`
  on g-mesh, plus S1's synthetic specifier queries on both S1 corpora.
  Expected: 0 diffs. The control must lose `QueryIsAPathOrPackage` on the
  specifier queries.
- **C. g-mesh itself as the mixed-language corpus**, for find_definition and
  search_code. Index a `git archive` snapshot in scratch, never the live
  checkout. The plugin root needs all four plugins; Rust and Python resolve
  `${G_MESH_BIN_DIR}`, so run the arm binary from a directory that also holds
  `g-mesh-plugin-rust` and `g-mesh-plugin-python`. Queries: `drive.py
  --gen-queries`, plus the S1 synthetic set, plus Python relative-import
  queries (`.models`, `..pkg.mod`) for D. Expected: 0 diffs for S7 and S8.
- **D. S9 (Python `.`).** Run C's g-mesh corpus with S8 as `before` and S9 as
  `after`. Expected diffs: only queries starting with `.`, and in each one
  only Python candidates removed or a Python-only page turned into a
  refusal. Every other diff is a finding. Also run a Python-only corpus
  (py-requests) to measure how often this fires.

Unit tests, each with a control (revert the code, never the test):
- Parse, absent table, and both validation rules.
- `refuses` and `refused_by_all`, including an empty map and an unknown
  language.
- The shipped map equals the table in §2, read from the committed manifests.
- find_definition, rung level. With the shipped map, `@x/y` leaves `reached`
  empty. With a map where only the hit's language is absent from the
  refusers, the hit survives. With all refusing, the page is empty.
- `verdict`: an all-refusing page gets the specifier verdict; a mixed page
  falls through to the floor; an empty page uses `refused_by_all`.
- `plugins list` render.

## 8. Risks

- **A vacuous 0-diff run** (no model, or the filter never reached).
  Mitigated by the control arm in every measurement.
- **D1.** If Go keeps only `/`, 0 diffs on Go projects holds by score, not by
  construction, and `@` queries load the model where today they do not.
- **Shorter candidate pages**, when a refusing language's hits are dropped
  from the top 3. This only happens once languages differ (S9).
- **Python `.` changes answers.** It is isolated in S9 and measured there.
- **An older core refuses a newer manifest** that uses a later key in this
  table, because of `deny_unknown_fields`.
- **Signature churn.** It is confined to `resolve_lazily*`, the 6
  synchronous `handle*` wrappers, the 5 tool methods, and the verdict
  functions.
- **A one-time re-index after the upgrade**, through the fingerprint.

## 9. ADR outline: `docs/adr/0018-non-symbol-query-shapes.md`

- **Context:** two core predicates hard-coded TS/Go import syntax (`@`, `/`).
  A lookup cannot replace them (S1). A global union would let one plugin
  silence every language.
- **Decision:** each plugin declares `[plugin.non_symbol_queries]`
  (`starts_with`, `contains`, literals). Core drops a candidate of language L
  when the query matches L's shapes. It short-circuits before embedding when
  every discovered language refuses. Both find_definition and search_code
  apply it. An absent table refuses nothing, and there is no core default.
  Validation covers empty strings and unknown keys.
- **Consequences:** no language syntax in core for these tools; a plugin can
  only affect its own candidates; queries the declarations do not cover cost
  an embedding; adding a language adds no core code. Follow-ups: TS `node:`
  (backlog).
- Also fix `docs/adr/README.md`, which still says the next free number is
  `0013`. It should be `0019`.
