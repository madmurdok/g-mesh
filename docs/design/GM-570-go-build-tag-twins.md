# GM-570: Go build-tag twins make engine answers ambiguous

Design note for GM-570 (GM-519 owner decision D4 "Две задачи в бэклог
(Recommended)"). No production code changes here.

Sources: [code] read in this worktree (`release-4.8.0`, 03c5164); [GM-519]
`docs/results/GM-519-*.md`; [run] a query run for this note on the gin index
GM-519 built (g-mesh 4.6.0/Go plugin 0.5.0, `~/.g-mesh/projects/fab171c84b1fcefb`,
copied to a scratch dir, re-running `GM-519-count-refusals.sql` reproduced
GM-519's numbers exactly: semantic linked 1297, `refused_other` 114).
No timing was measured; `uptime` at the time of the queries: up 8 days, load
averages 416.9 / 200.6 / 151.5 (another batch was running).

## 0. Summary

- On gin, all 114 refused Go engine answers point at one of two declarations
  of the same name in one package: `binding/binding.go` (`//go:build
  !nomsgpack`) and `binding/binding_nomsgpack.go` (`//go:build nomsgpack`).
- go/types already picked one: the host build loads `binding.go` and ignores
  `binding_nomsgpack.go`. The Go plugin then drops that knowledge: it
  addresses the answer by package + qualifiedName, which names both twins,
  and core's "exactly one" rule refuses it.
- **Recommended design: the Go plugin keeps the file.** When the answered
  name is twinned (also declared in a file the host build excludes), the
  placeholder is addressed by the declaring file instead of the package.
  Core is not changed: it already resolves `qualifiedName` keys in a file
  scope. On gin this links all 114 (each has exactly one candidate in the
  engine's file [run]), plus 1 more edge whose placeholder another tier
  currently overwrites (section 1.4).
- "Link to all twins" is rejected (section 3): it adds a wrong edge to every
  answer and needs core to fan out an edge.
- Separately from the 114: 241 structural (tree-sitter) edges on gin hit
  the same twins and are refused as well. They are outside the 114, and
  D3 decides whether GM-570 covers them.

## 1. What happens today

### 1.1 The twins on gin [run]

Declarations sharing `(language, container, qualifiedName)`, excluding
placeholders:

| package | files | names |
|---|---|---|
| `gin/binding` | `binding.go` (`!nomsgpack`) / `binding_nomsgpack.go` (`nomsgpack`) | 39: interfaces `Binding`, `BindingBody`, `BindingUri`, `StructValidator` and their methods (`Binding.Bind`, `Binding.Name`, ...), func `Default`, `validate`, vars `JSON`, `Form`, `Query`, ..., the `MIME*` consts |
| `gin/codec/json` | `json.go` (default) / `go_json.go` / `jsoniter.go` / `sonic.go` | `Package` (4 files), `json` (`jsoniter.go`, `sonic.go`), `init` |
| `gin` | `context_appengine.go` (`appengine`) + 7 others | `init` only |

`init` is never addressed: `addressOf` returns nothing for it
(`plugins/go/semantic.go` 884-890). So `binding` and `codec/json` are the
only packages that matter.

The structural extractor indexes every `.go` file, whatever its build
constraint, and node ids include the file path. So the twins are two
distinct nodes, which is deliberate (`plugins/go/README.md` 129-139). [code]

### 1.2 What go/types answers [run]

`go list` from the gin checkout on this host (go1.27.0 darwin/amd64, no
tags):

```
github.com/gin-gonic/gin/binding     Ignored=[binding_nomsgpack.go]
github.com/gin-gonic/gin/codec/json  Ignored=[go_json.go jsoniter.go sonic.go]
github.com/gin-gonic/gin             Ignored=[context_appengine.go]
```

The semantic pass loads packages with `go/packages` under this same default
(`semanticEngine.load`, semantic.go 337-357; `Tests: true`, the user's own
environment plus `-mod`). Every object go/types resolves to a twinned name is
therefore the one in `binding.go` / `json.go`. A user who sets
`GOFLAGS=-tags=nomsgpack` gets the other twin: `loadEnv` (375-391) inherits
GOFLAGS and overrides only `-mod`.

### 1.3 Why the linker refuses it [code]

- `semanticEngine.addressOf` (semantic.go 859-915) turns the object into
  `(container = obj.Pkg().Path(), key = dotPath(...))`. The declaring file is
  not part of the address.
- `semanticDiff.placeholder` (1112-1145) sends
  `Target{Scope: {Container: container}, Key: {QualifiedName: ...}}`.
  `answerImplements` (959-1024) does the same for `SUPERTYPE_OF`.
- core: `Resolver::declared` (symbol_links.rs 1824-1868) with
  `Scope::Container` + `Key::QualifiedName` returns both twins. Both are
  visible (`public`) and both fit the kind, so `link`'s `fitting`
  (1290-1299) has two. `sole_non_member` applies only to `name` keys and
  `sole_accessor_getter` only to getter pairs, so `target_id = None` and the
  edge is unlinked (1309-1330). The module doc states the rule (symbol_links.rs
  59-64): a `qualifiedName` key that matches two declarations is refused,
  "not a licence to pick one".

The 114, by key [run] (scope: container `gin/binding`, key kind
`qualifiedName`):

| edge | key | edges |
|---|---|---|
| CALLS | `Binding.Bind` | 51 |
| CALLS | `Binding.Name` | 32 |
| CALLS | `BindingUri.BindUri` | 3 |
| CALLS | `StructValidator.Engine` | 2 |
| CALLS | `BindingBody.BindBody`, `BindingUri.Name`, `StructValidator.ValidateStruct` | 1 each |
| SUPERTYPE_OF | `Binding` | 14 |
| SUPERTYPE_OF | `BindingBody` | 8 |
| SUPERTYPE_OF | `BindingUri` | 1 |

Each of the 114 has exactly 2 candidates, both fitting the kind, exactly 1
of them in the file the host build loads (`binding/binding.go`) [run]. A
file-scoped lookup at `binding/binding.go` therefore yields exactly one
candidate for all 114.

### 1.4 A shared placeholder hides one more answer [run, code]

The structural extractor's `placeholderFor` (`plugins/go/uses.go` 771-797)
and the semantic `placeholder` derive the placeholder id the same way:
`nodeIDFor(relPath, Module, "<pkg>.<name>", pending_symbol)`. For a top-level
name, both tiers in one file therefore write **one** node. Its
`placeholder_targets` row is replaced wholesale on every upsert
(`core/src/storage/write.rs` 532-567), so the tier that writes last decides
the address. On gin, `binding/default_validator.go:41`
`var _ StructValidator = (*defaultValidator)(nil)` produces a structural
`REFERENCES` and a semantic `SUPERTYPE_OF` on one placeholder whose target
is now the structural `name` key. That edge (the GM-519 count's single
"semantic `structural_name_keyed`" row) is also refused on the twins.

This does no harm while both tiers' addresses mean the same declaration. It
does matter for this design: a file-scoped semantic address must not share a
node with the container-scoped structural one, or the next structural
re-extraction overwrites it (D2).

### 1.5 The same twins refuse structural edges too [run]

Of the unresolved edges on gin that land on a twinned address:

| tier | key | edges | sole candidate in a host-built file |
|---|---|---|---|
| semantic | `qualifiedName` | 114 | 114 |
| semantic | `name` (the shared placeholder, 1.4) | 1 | 1 |
| syntactic | `name` | 241 | 236 |

The 241 structural edges are `binding.JSON` (37), `binding.MIMEPOSTForm`
(23), `binding.Form` (18), `binding.Binding` (13), ... `CALLS validate` (16),
`CALLS Default` (4). The 5 with no host-built candidate are
`codec/json/go_json.go`'s `json.Marshal` etc.: there `json` is the imported
`github.com/goccy/go-json` package, which the structural tier mistook for
the package-level var `json` of `jsoniter.go`/`sonic.go`. Refusing those is
correct; linking them to any twin would be wrong. The semantic pass does not
repair the structural edges: it answers open sites and conversions only,
and leaves a structural call it agrees with to core (`answerFile`,
semantic.go 800-812).

## 2. The design

### 2.1 Plugin: twinned answers are addressed by the declaring file

1. **Record each answer's declaring file while folding.** A `token.Pos` only
   means something in its own load's `FileSet`, so the file must be recorded
   in `foldPackage` (433-497), as `declaredIn` already is for named types
   (313-318, 482). Generalise `declaredIn` to `map[types.Object]string` and
   record `obj.Pos()`'s project-relative file for every object put in `uses`
   and for every `selection.Obj()`. `positionOf` (500-510) gives the file.
   It uses `fset.Position`, which follows `//line` directives, so a
   generated file's answer can name a file that is not in the index. The
   fallback in step 3 covers that case.
2. **Find the twinned names per package.** `loadMode` already has
   `NeedFiles`, so `pkg.IgnoredFiles` lists the files the host build excludes
   (gin: `binding_nomsgpack.go`; `go_json.go`, `jsoniter.go`, `sonic.go`;
   `context_appengine.go`). For each project package, parse its ignored
   `.go` files (`go/parser`, `SkipObjectResolution`, declarations only) and
   collect the qualifiedName spellings `extract.go` gives them: `F`, `T`,
   `T.M` for a method or an interface method, and each var/const name. A
   name is **twinned** if the package's loaded files declare it too. Store
   this as `twinned map[string]map[string]bool` (package path to names) on
   `resolutions`. Skip `_test.go` files: with `Tests: true` they are loaded,
   not ignored.
3. **Address.** `addressOf` (859-915) also returns the declaring file. In
   `answerFile` (775-827) and `answerImplements` (1016), if the name is
   twinned in its package and the declaring file is known, the target is
   `Scope{File: declFile}`. Otherwise it is `Scope{Container: pkg}`, as today.
4. **Distinct id for a file-scoped placeholder.** `semanticDiff.placeholder`
   (1112-1145) adds the declaring file to the hashed string when the scope
   is a file, for example `nodeIDFor(relPath, Module, "<pkg>.<qn>@<declFile>",
   pending_symbol)`. The display `QualifiedName` stays `<pkg>.<qn>`. This
   stops the structural tier from overwriting the address (1.4), and a host
   whose tags select the other twin gets a separate placeholder. Non-twinned
   ids do not change, so no existing placeholder or edge id changes.

Core needs no change. It already handles a file-scoped `qualifiedName` key:
`Scope::File` (symbol_links.rs 430-470), `in_file_by_qualified_name`
(1434-1436), and `visible` (1975-1991), which ignores the scope for
`public` and `container` rows (Go uses only these two).
`published_scopes` (808-814) publishes Go's `public`/`container`
declarations under their file as well as their container. So `link_diff`
wakes a file-scoped placeholder when the declaring file changes. [code]

### 2.2 What the answer means

A twinned answer then means "the declaration the host build uses". This is
the "pick by the default build constraints" option, implemented with the
engine's own choice instead of a re-evaluation of the constraints. The
choice is exactly as host-dependent as the semantic pass already is
(README 129-139). The same repository indexed on Linux and macOS can
link a `sys_linux.go`/`sys_darwin.go` twin pair differently. The README's
out-of-scope note gets one sentence on that.

### 2.3 Composition with GM-568, GM-567, GM-569

Code order: GM-568, GM-567, GM-569, GM-570. With the recommended D1/D2,
GM-570 touches only `plugins/go/` (+ README, fixture), not
`core/src/graph/symbol_links.rs`, so there is no textual conflict with the
other three.

- **GM-568** (drop type members before the kind rule, `name` keys only):
  disjoint. GM-570's answers are `qualifiedName`-keyed. The 241 structural
  twins (D3) are `name`-keyed top-level names, none of them members, so
  GM-568 does not touch them either.
- **GM-567** (semantic exact answers bypass visibility; per-`(kind, source)`
  split; `choose`). Its bypass predicate is `source = semantic && key is
  qualifiedName && kind in {CALLS, REFERENCES}`, which does not depend on
  the scope kind. A file-scoped Go answer is treated the same as a
  container-scoped one. GM-567's new seed (an exact `Scope::File` seed for
  `file`-visible declarations) does not affect Go, which has no
  `file`-visible declarations. The bypass does not resolve ambiguity: after
  GM-567 the 114 are still refused, now as `refused_other` with the check
  skipped, so GM-567's measure must expect gin's 114 unchanged. Step 4's
  distinct id removes 1 of the shared Go placeholders GM-567 counted
  (9 on gin [GM-519]). That helps GM-567's split, because fewer placeholders
  carry both tiers.
- **GM-569** (engine `CALLS` accept `Function`, then `Variable`, then
  `Type`): Go's twinned `CALLS` targets are `Function`s (interface methods,
  `Default`, `validate`), so the first kind wins as today. Go re-states
  conversions as `REFERENCES` (semantic.go 814-826), so GM-569 changes no Go
  answer. GM-569's note places "GM-570's twin rule" inside `choose` after
  the kind filter. That applies only if the owner picks D1 option B (a core
  tie-break). With the recommended option A, `choose` is unchanged and
  receives one candidate.
- **The measure baseline** for GM-570 is the branch base with all three
  merged. Only the Go plugin differs between the arms.

## 3. Options considered

| option | what | 114 on gin | wrong links | size |
|---|---|---|---|---|
| **A. Plugin addresses twinned answers by declaring file** (recommended) | 2.1 | +114 (+1 via 1.4) | none: one candidate, the engine's own | plugin only, ~120 lines + tests |
| B. Core tie-break hint | placeholder target gains an optional `preferFile`; `choose` picks the sole candidate in it when several fit | +115 | none | wire field (Go wire.go, SDK `TargetScope`), `placeholder_targets` column + migration, linker, conformance kit |
| C. Link to all twins | a refused `qualifiedName` answer fans out to every twin | +114 edges become 228 | 114 wrong (an edge into `binding_nomsgpack.go` that the host build does not compile); on the structural side, 5 more wrong edges onto `json` (1.5) | core: one usage edge becomes N rows (edge identity, `linkedFrom`, unlink) |
| D. Excluded files get their own container | the plugin puts `binding_nomsgpack.go` in container `binding#nomsgpack` | +114 | none, but same-package lookups *from* the excluded file break (it uses `jsonBinding` from `json.go`) | plugin, visibility chain rules |
| E. Keep refusing | - | 0 | none | none |

## 4. Owner decisions

### D1. How does an engine answer reach one of two build-tag twins?

- **Today:** gin declares `type Binding interface { Bind(...); Name() }` in
  both `binding/binding.go` (built by default) and
  `binding/binding_nomsgpack.go` (built only with `-tags nomsgpack`). Go's
  type checker resolves `c.ShouldBindWith(obj, b)`'s `b.Bind(...)` to the one
  in `binding.go`. The Go plugin sends only "package `binding`, name
  `Binding.Bind`". Core finds two declarations and links nothing. That is
  114 edges on gin (8.1% of Go engine answers): `find_callers Binding.Bind`
  is empty, and `find_implementations Binding` misses 14 types.
- **Change:** the answer names the file go/types resolved to.
- **Example:** `find_callers Binding.Bind` (in `binding.go`) lists the 51
  call sites; the twin in `binding_nomsgpack.go` lists none.
- **Consequence:** the edge follows the host's build (and the user's
  GOFLAGS tags). A twin in a file the host does not build has no engine
  callers, which matches the semantic pass's existing scope: such files
  get no semantic layer at all today.
- **Options:**
  - **A: the Go plugin addresses a twinned answer by its declaring file
    (Recommended).** Benefit: +114 gin edges (+1, see 1.4), no wrong
    links, no core change, no conflict with GM-567/568/569. Risk: plugin
    logic to detect twins (parse ignored files). The answer depends on the
    host, which is already true of every Go semantic edge.
  - **B: a core tie-break hint (`preferFile` on the placeholder target).**
    Benefit: same delta. The address stays container-scoped, so a
    declaration moving between files still re-links at once. Other plugins
    could reuse it (C/C++ `#ifdef` twins). Risk: wire and schema change
    (Go wire.go, SDK, `placeholder_targets` column, conformance kit), and a
    second tie-break inside `choose`, which three other tasks are editing
    in this batch.
  - **C: link to all twins.** Benefit: the excluded twin gets callers too.
    Risk: 114 wrong edges onto code the host does not build, and core must
    fan one usage edge out to N rows.

### D2. Which Go engine answers become file-scoped: twinned names only, or all?

- **Today:** every Go engine answer is addressed by package + name.
- **Change:** (A in D1) some answers are addressed by file instead.
- **Example:** `gin.go` calls `binding.Default(method, contentType)`.
  `Default` is twinned, so the answer becomes "`Default` in
  `binding/binding.go`". `render.JSON{...}.Render` is not twinned, so its
  answer stays "`JSON.Render` in package `render`".
- **Consequence:** a file-scoped answer goes stale when its declaration
  moves to another file. Suppose `Default` moves from `binding.go` to
  `default.go`. A per-file pass re-answers only the edited files
  (semantic.go 101-105), so callers in `gin.go` keep pointing at
  `binding.go` and are unlinked (a missing edge, never a wrong one) until
  `gin.go` is re-checked. A container-scoped answer re-links at once.
- **Options:**
  - **Twinned names only, detected by parsing the package's
    `IgnoredFiles` (Recommended).** Benefit: the move-staleness is limited
    to twinned names (39 of gin's ~1400 answered addresses). Every other id
    and address is unchanged, so the diff after upgrade is small. Risk: about 60
    lines of detection. A twin spelled in a way the parser does not collect
    stays refused, which is no worse than today.
  - **All Go engine answers.** Benefit: simplest (about 10 lines, no
    detection). Risk: any declaration moved between files leaves other
    files' engine edges unlinked until they are re-checked. On upgrade, every Go
    semantic placeholder changes its target. Step 4's distinct id applies
    to all of them, so the first pass re-emits every Go placeholder.

### D3. The 241 structural (tree-sitter) edges on the same twins?

- **Today:** without go/types, the structural tier addresses `binding.JSON`
  as "name `JSON` in package `binding`". Two twins match, so it is refused:
  241 edges on gin, `JSON` ×37, `MIMEPOSTForm` ×23, `Form` ×18, ...
  [run]. The semantic pass does not replace them (1.5).
- **Change (option b):** the semantic pass re-answers a structural use of
  a twinned name. It retracts the structural edge and emits a
  file-scoped semantic one, as it already does for conversions (`answerFile`
  814-826, `TestSemanticPassRetractsCallsOntoAType`).
- **Example:** `context.go`'s `binding.JSON` becomes a resolved
  `REFERENCES` onto `JSON` in `binding/binding.go`. `go_json.go`'s
  `json.Marshal` stays refused, because go/types resolves `json` to the
  go-json import, which is not a project declaration.
- **Consequence:** up to +236 gin edges, but only with the semantic tier
  on. Structural-only indexing keeps refusing them.
- **Options:**
  - **(a) A separate backlog task (Recommended).** Benefit: GM-570 stays the
    114 named in the task, with a plugin-only, measurable change. Risk: 241
    edges stay refused for now; the task must reach structural `REFERENCES`
    sites, which `structuralFile` does not list today (only open sites and
    placeholder calls).
  - **(b) In GM-570.** Benefit: one change covers both tiers' twin losses on
    gin. Risk: more scope. It needs a new list of structural uses per file,
    and it changes the size of the semantic diff.
  - **(c) Leave it.** Benefit: none to build. Risk: `find_references
    binding.JSON` stays empty without a caveat.

## 5. Edit map (D1 A, D2 twinned-only, D3 a)

`plugins/go/semantic.go`:

| what | where | change |
|---|---|---|
| package doc, "What this pass does not answer" | 89-105 | one bullet: a twinned name is answered at the host-built file |
| `resolutions` | 300-319 | `declaredIn` -> `map[types.Object]string` (doc kept: why recorded while folding); add `twinned map[string]map[string]bool` |
| `newResolutions` | 321-329 | initialise both |
| `foldPackage` | 433-497 | record `declaredIn` for every `Uses` object and `selection.Obj()`; collect twinned names from `pkg.IgnoredFiles` (new helper) |
| new `twinnedNames(pkg, loaded) map[string]bool` | beside `foldPackage` | parse ignored non-test files, spell `F`/`T`/`T.M`/var/const as `extract.go` does, intersect with the package scope's names (methods via the loaded `types.Named` method sets / interface methods) |
| `addressOf` | 859-915 | also return the declaring file (from `declaredIn`), or "" |
| `answerFile` | 775-827 | pass a file scope to `placeholder` when twinned and known |
| `answerImplements` | 1016-1018 | the interface's file from `declaredIn[ifaceObj]` when twinned |
| `semanticDiff.placeholder` | 1112-1145 | take a `targetScope`; file scope adds the declaring file to the hashed id |

Other files:

- `plugins/go/README.md` 129-139: one sentence. A twinned name's engine
  edges land on the twin the host builds, and which twin that is depends on
  the host and on GOFLAGS tags.
- `plugins/go/semantic_test.go`: new tests (section 6), modelled on
  `TestSemanticPassRetractsCallsOntoAType` (231).
- Conformance fixture `docs/results/GM-519-fixtures/go/g02-build-tag-twins/`
  (`binding.go` `//go:build !tagx` / `binding_tagx.go` `//go:build tagx`,
  an interface, a call through it, an implementor), with `[[callers]]` for
  the method in the host-built file and an empty list for the twin, plus
  an `[[implementations]]` row if the kit supports it.

Read for context (no change): `core/src/graph/symbol_links.rs` `link`
1238-1332, `Resolver::declared` 1824-1868, `Resolver::visible` 1975-1991,
`published_scopes` 808-814, `Scope` 430-470; `plugins/go/uses.go`
`placeholderFor` 771-797; `plugins/go/wire.go` `targetScope` 124-127;
`core/src/storage/write.rs` 532-567 (target replaced on upsert).

g-mesh calls this note relied on: `select_project g-mesh`;
`get_file_outline core/src/graph/symbol_links.rs` (linker function map);
`find_callers semanticEngine.addressOf` -> only `answerFile`;
`find_callers semanticDiff.placeholder` -> `answerFile`, `answerImplements`
(the only two sites to change). `find_references resolutions.declaredIn`
returned no match (the Go plugin emits no nodes for struct fields). Its
uses were taken from a grep of `semantic.go`: written at 482, read at 1034.

## 6. Behaviours the tests must pin, with candidate controls

| # | behaviour | control (revert code, the test must fail) |
|---|---|---|
| T1 | a call through an interface whose declaration is twinned emits a `CALLS` placeholder with `scope.file` = the host-built file, `qualifiedName` key | `answerFile` always passes the container scope |
| T2 | a non-twinned answer keeps `scope.container` and the same placeholder id as today | treat every answer as twinned |
| T3 | `SUPERTYPE_OF` onto a twinned interface is file-scoped | `answerImplements` keeps the container scope |
| T4 | a file-scoped semantic placeholder's id differs from the structural `placeholderFor` id of the same name in the same file (1.4) | drop the declaring file from the hashed id |
| T5 | twin detection covers a method / interface method (`T.M`), not only top-level names | collect only `FuncDecl`s without receivers and `TypeSpec`s |
| T6 | a twinned answer whose declaring file is not a project file (`//line`, outside the root) falls back to the container scope rather than dropping the edge | return no address when the file is unknown |
| T7 | end to end (fixture g02, `plugins check` with the Go plugin): callers of the host-built twin's method are listed, the excluded twin's are empty | as T1, observed through core |

Seven controls, all in the Go plugin's test binary (`go test ./...` in
`plugins/go`, one build). T7 runs under the verify slice's fixture run.
Repeats: T1-T6 build no processes or timers beyond the existing
`packages.Load` harness. They follow the 5x rule only if the harness
counts as a process (it shells out to `go list`), and they do: run them 5x.

## 7. Measure slice: needed

The change moves corpus edges, and the expected delta is exact enough to
check.

- **Arms:** before = branch base with GM-568, GM-567, GM-569 merged; after =
  this branch. Only the Go plugin is rebuilt per arm (`go build` in
  `plugins/go`). Use the gin v1.12.0 checkout `73726dc6` (warm, clean).
  `g-mesh reindex` per arm, semantic pass `completed: true`. Record `uptime` and
  `/usr/bin/time -p` real/user/sys per reindex. The machine was at load
  ~400 when this note was written; note the load, and do not compare times
  across loads.
- **Counts per arm:** `GM-519-count-refusals.sql`; this note's twin query
  (section 1.5: unresolved edges on twinned addresses, by tier and key kind);
  total semantic Go edges; syntactic linked edges.
- **Expected (D1 A, D2 twinned-only, D3 a):** semantic linked +115
  (114 + the 1.4 edge); semantic `refused_other` 114 -> 0; semantic
  `structural_name_keyed` 1 -> 0; total semantic edge rows unchanged;
  syntactic linked unchanged and syntactic twin refusals 241 -> 241;
  `refused_visibility_container` 1 -> 1 (GM-567 D2 keeps `SUPERTYPE_OF`
  checked). Any other row moving is a finding.
- **Cheap observation where the arms differ:** on gin, `find_callers
  Binding.Bind` (the `binding/binding.go` one, by `symbol_id`) is empty before
  and lists 51 sites after. `find_implementations Binding` gains 14. If
  neither moves, the Go plugin binary was not rebuilt; check its
  `plugins/go/g-mesh-plugin-go` mtime.
- **Other corpora:** not needed. Only the Go plugin changes, and excalidraw,
  ripgrep and py-requests have no Go files.
