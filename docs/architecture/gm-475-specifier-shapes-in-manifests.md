# GM-475: import-specifier shapes move from core into plugin manifests

Status: design (GM-475 slice S3). Not implemented.

## 1. Problem and decision already taken

`core/src/mcp/find_definition.rs::is_module_specifier` is a spelling rule,
`name.starts_with('@') || name.contains('/')`, that keeps import-specifier
shaped queries away from the semantic-neighbour rung. S1 showed that a lookup
of stored module keys cannot replace it (55 of 401 TS answers and 1 of 335 Go
answers regressed; `gm-474-qualified-name-segments.md` §3.5). The owner chose
option (d): keep the rule's behaviour, but have each plugin declare its
language's specifier shapes in `plugin.toml`, and have core apply the union.
This follows ADR 0015's principle ("core never parses separators") one step
further: core holds no language's import syntax either.

Acceptance criteria are in the task; this note answers how.

## 2. What exists today (g-mesh calls behind each claim)

| Question | Answer | Produced by |
|---|---|---|
| Callers of `is_module_specifier` | only `by_semantic_neighbours` (the GM-360 `qualifiedName`-arm caller is gone) | `find_callers(is_module_specifier)`, complete |
| Callers of `by_semantic_neighbours` | only `by_file_name` | `find_callers(by_semantic_neighbours)`, complete |
| Who reaches the ladder | `resolve_symbol_name` <- `find_definition::by_name` and `mcp::anchor::resolve` (the latter serves find_references, find_callers, find_callees, find_implementations) | `find_callers(resolve_symbol_name)`; `find_references(SemanticRung)` lists the five `handle_in`/`dispatch_in` entry points |
| How a query gets its rung | every handler runs inside `resolve_lazily` (CLI, tests, plugin-check: 6 callers) or `resolve_lazily_off_worker` (the 5 tool methods in `core/src/mcp/mod.rs`), both of which build the `SemanticRung` | `find_callers(resolve_lazily)`, `find_references(SemanticRung)`, `mcp/mod.rs` read directly |
| Where `plugin.toml` is parsed | `daemon::manifest::read_manifest` -> `PluginManifest` (raw serde shape `RawPlugin`, no `deny_unknown_fields`); `discover` builds `DiscoveredPlugins` | `find_references(PluginManifest)`: 40 rows in 10 files (daemon/manifest, plugin, lifecycle, bulk_index, workspace_reindex, cli/plugin_check/{mod,session,checks}, two integration tests) |
| How manifests reach MCP handlers | `PluginRegistry` owns `discovered`; `GMeshMcpServer` holds `Arc<PluginRegistry>`. Precedent: `[plugin.workspace] entry_points` is unioned by `PluginRegistry::entry_points()` and passed into `get_dependencies::handle` | `find_callers(PluginRegistry::entry_points)` -> `GMeshMcpServer::get_dependencies` only |
| plugin-check's path into `find_definition` | `cli::plugin_check::expectations::call_definition` -> `find_definition::handle`, with `EmbeddingPipeline::disabled()` | `find_callers(find_definition::handle)` (1 non-test caller, 10 tests) |
| Same syntax elsewhere in core | `mcp::similarity::is_specifier_query` (search_code's verdict) repeats `starts_with('@') || contains('/')` behind a whitespace guard | grep for `'@'` in `core/src/mcp`; `find_callers(is_specifier_query)` -> `similarity::verdict` only |

Lifetime facts (read from `registry.rs` and `daemon/front.rs`):

- `discovered` is read once at daemon start and never changes while the
  daemon runs (`PluginRegistry::receiver_call_capabilities`' doc says so;
  installing a plugin already needs a daemon restart for routing).
- Discovery roots are global, not per project: `~/.g-mesh/plugins/`, then the
  bundled roots. A multi-project folder is served by a *front* daemon that
  holds no plugins and answers every index tool with an error; `select_project`
  re-points the shim at that project's own daemon, which has its own
  registry. So "per project or global" is: one union per daemon, which in a
  real install is the same set for every project.

## 3. Field name and format (Q1)

### Option A: literal matchers (recommended)

```toml
[plugin.import_specifiers]
starts_with = ["@"]
contains = ["/"]
```

A query is specifier-shaped if it starts with any `starts_with` entry or
contains any `contains` entry of any loaded plugin. Case-sensitive, no
trimming, plain `str::starts_with` / `str::contains`.

### Option B: regex

`regex` is **not** a direct core dependency. It is in `Cargo.lock`
transitively (`tokenizers` and `tree-sitter` pull 1.13.1, per
`cargo tree -i regex`), so making it direct adds no new crate or binary
weight. The `regex` crate is finite-automaton based, so ReDoS by
backtracking is not possible; the remaining risks are compile size limits
(handled by `RegexBuilder::size_limit`) and the per-query cost of N
automata, both small. Compile cost at load time is microseconds per pattern.

### Recommendation: A

- Every shape any shipped or foreseeable plugin needs is a prefix or an
  infix: `@scope/`, `./`, `../`, `github.com/x`, Python's leading `.`,
  Node's `node:`. Nothing measured needs an anchored alternation.
- A literal is impossible to get subtly wrong (`.` in a regex is "any
  character": `contains = ["."]` as a regex would refuse every query).
- The union is just two concatenated, sorted, deduplicated lists, and
  `g-mesh plugins list` can print it verbatim.
- Additive later: a `matches = [...]` regex key can be added beside the two
  lists without changing their meaning.

### TOML each shipped plugin carries

Preserving today's union exactly (`starts_with('@') || contains('/')`):

| Plugin | Declaration | Why |
|---|---|---|
| typescript | `starts_with = ["@"]`, `contains = ["/"]` | scoped packages (`@excalidraw/math`), relative (`./extract.js`) and subpath (`lodash/fp`) specifiers |
| go | `contains = ["/"]` | module paths (`github.com/x/y`) and relative imports (`./extract`) |
| rust | none (no table) | `use` paths are `a::b::C`, which are symbol paths the suffix rung resolves (GM-469). Declaring `::` would refuse legitimate qualified queries |
| python | none (no table) | see below |

The union over the four is `starts_with = ["@"]`, `contains = ["/"]`, which
is today's predicate byte for byte. Go's `/` duplicates TypeScript's; the
union deduplicates it.

**Python.** Relative imports (`.models`, `..pkg.mod`) start with `.` and
are *not* refused today. Declaring `starts_with = ["."]` would change
answers, which "0 diffs" forbids. Absolute imports (`os.path`) are
indistinguishable from qualified names, so nothing can be declared for them.

**Missing shapes, proposed as separate follow-ups (each changes answers and
needs its own before/after):**

- python: `starts_with = ["."]` (relative imports).
- typescript: `starts_with = ["node:"]` (Node built-ins such as `node:fs`).
  Not `#` (subpath imports): `#count` is also a private-member query, and
  TS's own qualified names use `#` (`C#m`).
- go: standard-library paths without a slash (`fmt`, `strings`) cannot be
  told from identifiers by shape; nothing to declare.

## 4. How the declarations reach the query path (Q2)

```mermaid
flowchart LR
  T[plugin.toml x N] -->|read_manifest| M[PluginManifest.import_specifiers]
  M -->|discover| D[DiscoveredPlugins]
  D -->|once, at startup| U["SpecifierShapes::union (Arc)"]
  U --> S[GMeshMcpServer field]
  S -->|resolve_lazily_off_worker| R[SemanticRung::Deferred / Embedded]
  R --> B["by_semantic_neighbours: shapes.matches(name)"]
```

1. **Manifest.** New `SpecifierShapes { starts_with: Vec<String>, contains:
   Vec<String> }` in `daemon::manifest`, a new `PluginManifest` field
   `import_specifiers: SpecifierShapes` (default empty), parsed from an
   optional `RawPlugin.import_specifiers` table.
2. **Union.** `SpecifierShapes::union<'a>(impl IntoIterator<Item = &'a
   SpecifierShapes>)`: concatenate, sort, dedup (the `entry_points()`
   precedent, for the same HashMap-order reason). Exposed as
   `PluginRegistry::specifier_shapes()`.
3. **Once per daemon, not per call.** `discovered` never changes while the
   daemon runs, so `GMeshMcpServer::new` computes the union once into an
   `Arc<SpecifierShapes>` field. (`entry_points()` recomputes per call; that
   is fine there, but here the value must cross into a `'static` closure
   anyway, so an `Arc` clone is the natural shape.)
4. **Carrier: `SemanticRung`.** The guard is the semantic rung's own input,
   and `SemanticRung` already reaches `by_semantic_neighbours` through every
   path (all five tools, anchor resolution, CLI, plugin-check). Both variants
   gain `shapes: &'a SpecifierShapes`; `resolve_lazily` and
   `resolve_lazily_off_worker` take the shapes beside the embedding pipeline
   and put them on both passes. No handler signature between the server and
   the rung changes (`handle_in`, `anchor::resolve`, `resolve_symbol_name`,
   `by_file_name` keep theirs).
   - Rejected: threading a `&SpecifierShapes` parameter through the five
     `handle_in`s, `anchor::resolve`, `resolve_symbol_name` and
     `by_file_name`. More signatures, and every one of them already has the
     rung.
5. **The predicate.** `is_module_specifier(name)` becomes
   `SpecifierShapes::matches(&self, name) -> bool` (owned by the type, which
   lives in `daemon::manifest`), and `by_semantic_neighbours` calls
   `semantic.shapes().matches(name)`. `find_definition.rs` keeps the
   *reasoning* (the doc comment) and no syntax.
6. **Synchronous callers.** `find_definition::handle` and its siblings
   (`find_callers_callees::handle_callers` etc., the 6 `resolve_lazily`
   callers) take the shapes as a parameter. plugin-check passes the checked
   plugin's own `manifest.import_specifiers` (it loads exactly one manifest;
   its embedding is `disabled()`, so the rung never answers there anyway).
   Tests use `SemanticRung::off()`, which can carry a `LazyLock` of the
   shipped union (read from the four committed manifests with
   `include_str!`), so existing tests keep today's behaviour unedited.

**Multi-project.** The union is per daemon. Each project's daemon discovers
the same global roots, so in practice the same union everywhere. The front
daemon builds no union it would use (no index tool runs there). A user
plugin in `~/.g-mesh/plugins/` that shadows a bundled one replaces that
language's declaration too, which is the existing shadowing rule.

**Union, not per language.** The query carries no language, so a shape from
one plugin also refuses queries aimed at another language (`@property` in a
Python project is refused because TypeScript is loaded). That is today's
behaviour and is not changed here.

## 5. Defaults (Q3)

- A plugin without `[plugin.import_specifiers]`, or with an empty table:
  empty lists, contributes nothing. Same "says nothing means does least"
  rule as ADR 0005 decision 6; the least here is "refuse nothing on shape".
- Core with no plugins loaded: empty union, the guard never fires. No core
  fallback list, which would put syntax back into core. Harmless: with no
  plugins, nothing is indexed and the rung has nothing to offer.
- Configurations where TypeScript and Go are both absent (only possible with
  `G_MESH_PLUGIN_ROOTS_OVERRIDE` or a stripped install) lose the `/` and `@`
  guard. Those queries then reach the score floor, which still refuses most
  of them. This is the one behavioural difference the design admits, and no
  shipped configuration has it.

## 6. Validation (Q4)

In `read_manifest`, hard errors naming the manifest path and the offending
entry (ADR 0005 decision 1):

1. An empty string (`contains = [""]` matches every query and would switch
   the semantic rung off for every language).
2. An entry containing whitespace (no import specifier contains it, and
   Rust qualified names such as `<X as Tr>::m` do).
3. An entry made only of identifier characters (Unicode alphanumerics and
   `_`), e.g. `starts_with = ["get"]`: it would refuse ordinary symbol names
   in every language. Every real shape has a punctuation character.
4. Unknown keys inside the table are an error
   (`#[serde(deny_unknown_fields)]` on this sub-table only), so a typo like
   `start_with` fails loudly instead of silently declaring nothing. Trade-off:
   an older core given a newer plugin that uses a key added later fails to
   load it. Accepted, the same way an unknown `readiness` value is a hard
   error today. The SDK's own manifest reader does not look at this table, so
   it is unaffected.

`g-mesh plugins check` calls `read_manifest` (`plugin_check/mod.rs:133`), so
it inherits rules 1-4 with no code of its own. Additionally:

- `g-mesh plugins list` prints the declaration next to the capabilities
  (`import_specifiers: starts_with=@ contains=/`, or `none`), so an author can
  see what core will apply.
- plugin-check's report carries the same line as a note. A behavioural
  plugin-check (asking the rung) is not possible: plugin-check runs with
  `EmbeddingPipeline::disabled()`, so the semantic rung never answers there.

## 7. Docs (Q5)

- `docs/architecture/multi-language-plugins.md`, "`plugin.toml` additions":
  add the table with a comment block (meaning, union across plugins,
  default, validation), and a short paragraph after the code block.
- `docs/architecture/plugin-modularity.md`, the manifest sketch around line
  246: one commented line for `[plugin.import_specifiers]`, pointing at the
  above, as it does for `[plugin.workspace]`.
- Plugin SDK: no change. `plugins/sdk/src/manifest.rs` documents only the
  subset the SDK itself reads (`extensions`, `exclude_dirs`), and nothing in
  the SDK reads this table.
- `find_definition.rs`: the `is_module_specifier` doc comment moves onto the
  call site in `by_semantic_neighbours` (or `SpecifierShapes::matches`),
  keeping the measurements (0.699 vs 0.566; 42 points of recall; S1's
  55/401 and 1/335), replacing "starts with `@` or contains `/`" with "matches
  a shape some loaded plugin declares", and naming the manifest table.
  `similarity.rs`'s three cross-references to `is_module_specifier` follow
  the rename.
- **ADR: yes, a short `docs/adr/0018-import-specifier-shapes-in-manifests.md`.**
  It is a new boundary decision (which side owns a piece of language syntax),
  the kind ADR 0015 records, and it adds a manifest field with defaults and
  validation, the kind ADR 0005 records. It links here for the reasoning.
  Also fix the README's stale "next free number is `0013`" to `0019`.

## 8. Manifest versioning (Q6)

- `plugin.toml` has no schema version. `protocol_version` versions the wire
  protocol and a mismatch is a hard error, so bumping it for an optional
  manifest field would break every third-party plugin for nothing.
- The change is compatible both ways: core's `RawPlugin` ignores unknown
  tables (an older core reading the new manifests ignores the field), and an
  absent table defaults to empty.
- Plugin versions: no further bump. TypeScript 2.4.0 and Go 0.4.0 were bumped
  in this unreleased release by GM-476; Rust and Python follow 3.19.0. The
  plugin binaries do not change at all.
- Side effect to expect: `plugin.toml` is inside each plugin directory, so it
  is part of `plugin::fingerprint` and hence of `indexer_version`. Every
  index built before the change is rebuilt once after the upgrade. A release
  that changes any plugin file already does this, and 3.19.0 does
  (GM-476).

## 9. Test plan and before/after method (Q7)

### Unit and integration tests, each with its control

| Test | Asserts | Control (revert code, not the test) |
|---|---|---|
| manifest parses the table | TS-shaped table -> `SpecifierShapes { ["@"], ["/"] }` | drop the field from `RawPlugin` -> empty, test fails |
| absent table | no table -> empty, no error | default the field to `["@"],["/"]` -> fails |
| validation x4 | `""`, `"a b"`, `"get"`, `start_with = [...]` each rejected with the path in the message | remove each rule -> its case loads, test fails |
| union | two manifests -> sorted, deduplicated union; a plugin with none adds nothing | concatenate without dedup / take only the first -> fails |
| shipped union is today's rule | union of the four committed `plugin.toml`s (`include_str!`) == `starts_with ["@"]`, `contains ["/"]` | delete TS's `starts_with` -> fails |
| rung guard | `Deferred` pass with the shipped union: `@excalidraw/math`, `./extract.js` leave `reached` empty; with an empty union, `reached` is `Some(name)` | make `matches` return `false` -> the first half fails |
| existing `is_module_specifier` cases (`tests.rs:886`) | same five assertions, against the shipped union | as above |
| `plugins list` render | prints `import_specifiers:` line, `none` for an empty table | drop the line -> fails |

### Before/after on S1's corpora (reuse `scratchpad/gm475/`)

- Arms: `before` = core built at the merge-base of the implementation
  branch; `after` = core built from it. Same `plugins-root` (symlinks to the
  worktree's `plugins/go` and `plugins/typescript`, which carry the new
  tables; the before binary ignores them). The TS ∪ Go union equals the
  four-plugin union because Rust and Python declare nothing, and the
  "shipped union" unit test pins that.
- `drive.py <arm> <binary> corpora/ts-corpus out/ts-corpus` and the same for
  `go-corpus`, then `compare.py before.json after.json`.
  **Expected: 0 diffs of 401 (TS) and 0 of 335 (Go).**
- The model must be present (`G_MESH_MODEL_DIR`), otherwise the rung never
  answers and 0 diffs measures nothing. The control below proves it was.
- **Control arm** (`after-noshapes`): the `after` binary with a
  `plugins-root-control/` whose `plugin.toml`s are copies without the table
  (TS: symlink `dist/` and `node_modules/`; Go: symlink `g-mesh-plugin-go`).
  Expected: a non-zero diff count on TS, at least the 55 S1 saw for the
  lookup variant, and at least 1 on Go. Zero here means the harness did not
  exercise the guard.
- A manifest edit changes `indexer_version`, so each arm re-walks its
  corpus. Use a fresh `G_MESH_HOME` per arm, or accept the re-walk.
- Fixtures: the full suite once at the end, including the four plugins'
  conformance runs through `plugin_check`.

## 10. Open decisions for the owner

1. **`search_code`'s `similarity::is_specifier_query`** holds the same
   `@`/`/` syntax. The amended criteria name only `find_definition.rs`.
   Recommended: a follow-up task that feeds the same union into
   `similarity::verdict`. It needs its own 0-diff run on the search_code
   sweep (`gm-468`). Alternative: include it in GM-475 (about +1 call site
   and +1 measurement).
2. **Validation rule 3** (identifier-only entries rejected). Recommended. It
   blocks the most damaging third-party mistake, because the union is global.
3. **Python `.` and TS `node:`**: separate follow-ups, as above.

## 11. Cost and risks (Q8)

Cost: one implement slice (opus) of moderate size. `daemon/manifest.rs`
(field, raw table, validation, union, tests), `find_definition.rs`
(`SemanticRung` field, predicate, doc), the two `resolve_lazily*`
functions, the 5 tool methods in `mcp/mod.rs`, the 6 synchronous `handle*`
wrappers and their callers (plugin-check's `EvalContext`, about 12 test call
sites, most absorbed by `SemanticRung::off()`), `cli/plugins.rs` render,
four manifests, three docs, and one ADR. A verify slice (opus) that
rebuilds the controls, and a measure run comparable to S1's (two corpora x
three arms).

Risks:

- **A vacuous 0-diff run** (model missing, or the guard never reached).
  Mitigated by the no-shapes control arm.
- **Global union**: one plugin's broad declaration switches the rung off
  for every language. Mitigated by validation rules 1-3 and by the `plugins
  list` line. Not eliminated: `starts_with = ["$"]` passes and would refuse
  `$`-prefixed JS names.
- **Stripped installs** without the TS and Go plugins lose the guard (§5).
  No shipped configuration does this.
- **Hard error on unknown keys** in the table makes an older core refuse a
  newer plugin that uses a later key (§6, rule 4).
- **One-time re-index** after upgrade, through the fingerprint (§8).
- **Signature churn** in the synchronous `handle*` wrappers. Mechanical
  but spread across several files. Carrying the shapes on `SemanticRung`
  keeps it out of the handlers between the server and the rung.
