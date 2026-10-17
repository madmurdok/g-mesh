# GM-620: editing a Go interface does not re-check its implementors in other packages

Status: design (GM-620/S1). No production or test change. Line numbers are
against `release-4.11.0` at `c7ec432` (GM-590's fix already merged).

## Today's behaviour

### Example (reproduced)

Fixture of `semantic_supertype_carry_test.go` (`carryFixture`): module
`example.com/x`, `lib/lib.go` declares `type Binder interface{ Bind() string }`,
`app/app.go` imports `lib` and declares `type Mine struct{}` with
`func (Mine) Bind() string`. One long-lived `pluginState`, as in the daemon.

1. Whole-project pass: emits `app/app.go:Mine SUPERTYPE_OF lib#Binder`.
2. Edit `lib/lib.go` so the edge becomes untrue, then a per-file pass of
   `lib/lib.go` (what core sends after the reparse of that file):

   | Edit to `lib.Binder` | `Mine -> Binder` after the lib pass | after a later pass of `app/app.go` |
   | --- | --- | --- |
   | add a method `Close()` Mine lacks | kept (not deleted, not re-sent) | deleted |
   | change signature to `Bind() int` | kept | deleted |
   | delete `Binder` | kept | deleted |

   (Throwaway test, deleted after the run; log lines
   `1 file(s) ... 4/6/7 edge(s) retracted` are lib's own edges only.)

So the stale edge lives until something re-runs the semantic pass of the
*implementor's* file (an edit to `app/app.go`) or a whole-project pass (daemon
start, workspace/config reindex). In a long daemon session that is unbounded:
`find_implementations Binder` keeps listing `Mine`, and `find_references`/
`find_callers` through `Binder.Bind` keep it.

Removing a method from an interface is *not* in this list: it can only make
more types satisfy it, so no existing edge becomes untrue (see "Add
direction" below).

### Why (code)

All in `plugins/go/semantic.go`:

1. **Scope.** `semanticEngine.run` (966-1015) puts only the requested files in
   `scope`; `loadFor` (1068-1123) asks `go list` for `file=<abs>` of each, so
   only the edited file's package is folded from source (`foldPackage`,
   592-666; its one caller is `load`, 411-430).
2. **Pairing.** `answerImplements` (1395-1468) pairs every folded named type
   with every interface in `resolved.interfaces`. `app.Mine` is not a named
   type of this pass (app is not loaded), so the pair is never evaluated.
3. **Retraction is per file in scope.** `semanticDiff.finish` (1749-1789)
   retracts only ids in `e.emittedEdges[f]` for `f` in `scope`. The edge was
   emitted for `app/app.go` (`answerImplements` attributes a SUPERTYPE_OF edge
   to the *subtype's* file via `declarationNode`), and `app/app.go` is not in
   scope, so it is never looked at.

### Is retraction expressible from a pass over another file? Yes.

- Core deletes `deleteEdgeIds` by id, globally: `storage/write.rs`
  `write_diff` (390: `DELETE FROM edges WHERE id = ?1`), with no filter by the
  pass's scope. `watcher/apply.rs` doc of `apply_semantic_pass` (412-439):
  "A plugin retracts only the ids it remembers emitting in its own process".
- A cross-package SUPERTYPE_OF edge is "owned" by the subtype's file twice
  over: in the plugin (`emittedEdges["app/app.go"]`) and in core, where
  `storage/file_rows.rs` `widen` (45-96, caller
  `Writer::apply_file_diff_linked`) joins edges on `nodes.filePath` of
  `fromId`, i.e. a reparse of `app/app.go` decides its fate, a reparse of
  `lib/lib.go` never touches it.
- Upserts for files outside the requested scope are applied as any other row;
  `unfinishedFiles` outside the sent scope are ignored by core
  (`apply.rs` 696-700, "A named file outside the scope sent is ignored").

So the plugin itself can both retract and re-confirm the edge, provided it
re-evaluates the implementor's file in the same pass. No core change.

## Options

Cost figures: one measured sample, `golang.org/x/tools@v0.50.0` (215
packages, 25 with non-empty interfaces, 59 cross-package implements pairs),
counted with the plugin's own rules (non-alias, non-generic, method-set
interfaces; `T` or `*T`). Machine under load (load average 106 at start),
so no wall-clock numbers are claimed.

| Per interface-declaring package | p50 | p90 | p99 | max |
| --- | --- | --- | --- | --- |
| packages holding a SUPERTYPE_OF edge onto one of its interfaces | 0 | 1 | 2 | 9 |
| packages importing it | 1 | 10 | 44 | 72 |

### A. Re-fold the implementor files, gated by "the interface changed" (chosen)

The plugin already records, per file, every SUPERTYPE_OF edge it emitted
(`e.supertypes`, GM-590). Add to each record the interface's method-set
signature (`ifaceSig`, a `types.TypeString` qualified by package *path*, so
it compares across loads). After the normal load of a per-file pass:

- for every recorded edge of a file **not** in scope whose interface package
  was folded from source this pass (`resolved.folded`): if that interface is
  now missing, or its signature differs from `ifaceSig`, the file is a
  *stale implementor file*;
- if there is at least one: discard this pass's resolutions, widen `scope`
  with those files, and load again (`loadFor` already groups `file=`
  patterns by module into one `packages.Load`); continue the pass as today.

The implementor files are then in scope, so `answerImplements` re-evaluates
them exactly and `finish` retracts what no longer holds, re-sends what still
holds, and refreshes their `emittedEdges`/`supertypes` records.

Why one combined load and not "load the dependents alone": with both packages
as roots of one `packages.Load`, the importer sees the source-checked `lib`,
so `types.Implements` is exact (probe: `Bind(lib.Opt) string` against
`Binder{ Bind(Opt) string }` matched in a combined per-file pass of both
files). It also covers implementors that do *not* import the interface's
package (`other.Binder2` in the GM-590 fixture), which a load of the
dependent alone could only carry forward by method name.

- *Cost, common edit* (body edit, comment, unrelated decl): one in-memory scan
  of the records (O(SUPERTYPE_OF edges in the process)), no extra load.
- *Cost, interface-changing edit*: a second `packages.Load` of the edited
  file's package plus the implementor packages: p90 = 1, max = 9 extra
  packages type-checked from source in x/tools.
- *Risk*: a project interface implemented across most of the project (e.g. a
  `String() string` interface) turns one edit into a near whole-module
  type-check; the second load pays `go list` twice for that edit. Open
  decision D2 (cap).
- *Benefit*: exact, covers added methods, signature changes and deleted
  interfaces, reuses `finish`/`carryForward` unchanged.

### A'. Same, without the gate (always re-fold implementor files)

Decide the extra files *before* the load (records whose interface was declared
in a directory of a scope file) and load once. *Benefit*: one `go list`, no
signature bookkeeping. *Risk*: every save of a file in a package with
implemented interfaces type-checks its implementor packages too (p90 one
extra package, max 9 in x/tools), for edits that almost never change an
interface. Rejected for that steady per-save cost.

### B. Retract by record, no load

Record the subtype's method names too; on the interface package's pass,
retract dependents' edges whose interface is gone or gained a method name the
subtype lacked. *Benefit*: no load at all. *Risk*: misses signature changes
(`Bind() int`), which the task names; matching signatures as strings would
reimplement `types.Identical` badly (aliases, embedded interfaces).
Rejected.

### C. Re-fold every importer of the edited package

Exact and also catches *new* implementors among importers. *Cost*: p90 = 10,
p99 = 44, max = 72 packages per edit in x/tools, and still misses
non-importers. Rejected.

### D. Bound and document only

The edge is corrected by the next pass of the implementor's file or the next
whole-project pass. In a long daemon session that has no bound. Kept only as
the documented residual for the cases A does not cover (below).

## Chosen: A

### What A does not cover (documented residual)

1. **Add direction.** Removing a method or relaxing a signature can make a
   type in another package newly satisfy the interface. That type has no
   record, so A does not re-fold it: the edge is *missing* (never wrong)
   until that file's next pass (exact if it imports the package) or the next
   whole-project pass. Same as today; D3.
2. **Implementor in another Go module** (nested `go.mod`). `loadFor` loads
   per module, so the interface package is not a root of the implementor's
   load: exact if the implementor imports it (export data,
   `foldImportedInterfaces`), else GM-590's carry-forward by method name,
   which keeps an edge on a pure signature change. Same bound as GM-590.
3. **Implementor's module fails to load.** `withoutFailedModules` drops the
   file; its edge stays as today.
4. **Plugin restart.** Records live in the process. Core runs a
   whole-project pass at start, which rebuilds them; until then nothing is
   known, as today.

## Edit map

All in `plugins/go/semantic.go`.

| Where | Lines | Change |
| --- | --- | --- |
| `supertypeRecord` | 1554-1564 | add `ifaceSig string` |
| `answerImplements` | 1395-1468 | set `ifaceSig: interfaceSignature(iface)` in the record (1448-1458) |
| new `interfaceSignature(*types.Interface) string` | next to `interfaceMethodNames` (1512-1518) | `types.TypeString(iface, func(p *types.Package) string { return p.Path() })` |
| new `(e *semanticEngine) staleImplementorFiles(resolved *resolutions, scope []string) []string` | next to `carryForward` (1680-1730) | build `pkg.name -> sig` from `resolved.interfaces` whose package is in `resolved.folded`; for every file in `e.supertypes` not in scope, any record with `folded[r.ifacePkg]` and (interface missing or sig != `r.ifaceSig`) adds the file; drop files that no longer exist on disk (and their `emittedEdges`/`supertypes` entries: core already removed their rows); return sorted |
| `semanticEngine.run` | 966-1015 | per-file only (`!wholeProject`): after `loadFor`, `extra := e.staleImplementorFiles(resolved, scope)`; if non-empty, `scope = union`, `resolved = newResolutions()`, `outcome = e.loadFor(ws, resolved, false, scope)`, recompute `reason`; filter `unfinished` to the requested files; add the re-checked count to the log line |

Unchanged: `loadFor`, `foldPackage`, `foldImportedInterfaces`,
`carryForward`, `finish`, core. A carried record (`carryForward` ->
`d.supertype`) keeps its old `ifaceSig`, which is right: it was never
re-evaluated against a newer interface.

Functions to read before implementing: `run`, `loadFor`, `answerImplements`,
`supertypeRecord`, `semanticDiff.supertype`, `carryForward`, `unevaluated`,
`finish`; tests: `semantic_supertype_carry_test.go` (helpers `carryFixture`,
`carryWholePass`, `carryRewrite`, `carryDeleted`, `carryUpserted`,
`supertypeEdgesByLabel`).

### Callers / references relied on (and which call produced them)

- `foldPackage` <- `semanticEngine.load` only (g-mesh `find_callers
  semanticEngine.foldPackage`; the first call, before the Go semantic tier
  finished, returned `[]` with `semanticTier: "absent"`).
- `answerImplements` <- `semanticEngine.run` only (g-mesh `find_callers`).
- `loadFor` (where a per-file pass decides which packages to fold) <-
  `semanticEngine.run` only (g-mesh `find_callers semanticEngine.loadFor`).
- `run` <- `pluginState.handleSemanticPass` (control.go 269) only (g-mesh
  `find_callers semanticEngine.run`).
- core: `sweep_semantic_edges` <- `apply_semantic_pass_in` only; `widen` <-
  `Writer::apply_file_diff_linked` only (g-mesh `find_callers`). Delete by id:
  `storage/write.rs` `write_diff` (grep `delete_edge_ids`, non-code-graph
  question).
- `semanticEngine.supertypes` field: g-mesh `find_references` returned "no
  symbol" for the field; grep: semantic.go 225, 242, 1699, 1779 (plus the
  `semanticDiff` field 1548, 1572, 1664-1667).

## Behaviours the tests must pin (later slice)

New tests in `plugins/go/semantic_supertype_carry_test.go` (or a sibling
`semantic_iface_edit_test.go`), all: whole-project pass, rewrite
`lib/lib.go` (or `other/other.go`), per-file pass of that file only.

1. **Added method the implementor lacks retracts** `Mine SUPERTYPE_OF
   lib#Binder` (id in `DeleteEdgeIds`, not upserted).
2. **Signature change retracts** (`Bind() int`).
3. **Interface deleted retracts.**
4. **Still-satisfied change keeps the edge**: add `Close()` to `Binder` *and*
   `Mine` already has `Close()`: the id is upserted with its placeholder, not
   deleted (guards against "retract every dependent edge" as the fix).
5. **Non-importer implementor**: add a method to `other.Binder2`; the pass of
   `other/other.go` retracts `Mine SUPERTYPE_OF other#Binder2`.
6. **Body-only edit does not widen**: change a function body in
   `lib/lib.go`; the diff carries no edge out of an `app/app.go` node (pins
   the gate's cost promise).
7. Existing GM-590 tests and `TestSemanticPassRetractsWhatAReCheckNoLongerProduces`
   stay green.

These spawn `go list`: run the new tests 5 times in the tests slice and in
verify (standing rule).

## Proposed controls (revert code, never the test)

| # | Revert | Must fail |
| --- | --- | --- |
| C1 | `staleImplementorFiles` returns nil | 1, 2, 3, 5 |
| C2 | gate compares method *names* (via `ifaceMethods`) instead of `ifaceSig` | 2 |
| C3 | instead of widening the scope, retract every dependent edge onto a changed interface | 4 |
| C4 | add the files to `scope` but not to the second load (load the original scope only) | 5 (Binder2 then carried by name; in 1 the import makes it exact, so C4 must be judged on 5) |
| C5 | gate always true (every recorded dependent of a folded package) | 6 |

Five controls; C1 and C5 touch the same function, C2-C4 different lines, all
in one test binary (`go test -run` in `plugins/go`).

## Decisions for the owner

- **D1** Gated re-fold (A, two loads on an interface edit, none otherwise)
  vs ungated (A', one load, extra packages on every save). Recommended: A.
- **D2** Cap the number of re-folded implementor packages (e.g. 64; above
  it, log and leave the edges for the whole-project pass)? x/tools max was 9.
  Recommended: no cap, log the count; revisit if a measured repo shows a
  hot interface.
- **D3** Add direction (newly-satisfied types in other packages): leave as
  documented residual, or file a backlog task (would need reverse imports,
  option C's cost)? Recommended: backlog task, low priority (a missing edge,
  never a wrong one).
