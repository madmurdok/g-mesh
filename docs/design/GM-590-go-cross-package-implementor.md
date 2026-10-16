# GM-590: a per-file Go semantic pass drops cross-package implementors

Status: diagnosis + design (GM-590/S1). No production or test change in this
commit. Line numbers are against `release-4.10.0` at `19ab0de`; GM-589 shifts
them (see "Interaction with GM-589").

## Repro

Smallest setup: a throwaway `_test.go` in `plugins/go` (deleted after the
run), one long-lived `pluginState`, so the per-file pass runs in the same
process as the whole-project pass, as in the daemon.

```
go.mod            module example.com/x
lib/lib.go        type Binder interface{ Bind() string }; type impl struct{}; func (impl) Bind() string
other/other.go    type Binder2 interface{ Bind() string }        // not imported by app
app/local.go      type Local interface{ Bind() string }          // same package as Mine
app/app.go        import lib; type Mine struct{}; func (Mine) Bind() string; func Use(b lib.Binder) string { return b.Bind() }
```

1. `handleSemanticPass(nil)` (whole project) upserts, among others:
   `app/app.go:Mine SUPERTYPE_OF example.com/x/lib#Binder`,
   `... other#Binder2`, `... app#Local`.
2. `handleSemanticPass(["app/app.go"])` (file unchanged) upserts only
   `Mine SUPERTYPE_OF app#Local` and the `Use CALLS lib#Binder.Bind` edge, and
   **lists the two other SUPERTYPE_OF ids in `DeleteEdgeIds`** (both were
   emitted by step 1). Log: `1 file(s), 1 resolved site(s), 0 implements
   edge(s) ... 1 edge(s) retracted` in the two-package variant.
3. `handleSemanticPass(["lib/lib.go"])` re-emits only `lib/lib.go:impl
   SUPERTYPE_OF lib#Binder`; nothing about `app.Mine` either way.

So the loss needs no build tags and no edit: any per-file pass of the file
declaring the implementor drops every SUPERTYPE_OF onto an interface outside
that file's package. A same-package interface (`Local`) survives. After a
plugin restart `emittedEdges` is empty and nothing is retracted, which is why
the bug shows only within one process lifetime (daemon, `plugins check`).

## Cause (evidence)

Three pieces of `plugins/go/semantic.go` combine:

1. **The interface universe of a per-file pass is the loaded root packages
   only.** `loadFor` (985-1040) asks `go list` for `file=<abs>`; `load`
   (376-397) folds only the returned roots; `foldPackage` (556-620) adds a
   type to `resolved.interfaces` only for a project package it folds itself.
   Imported packages come from export data and are never folded, and
   packages the file does not import are not loaded at all.
2. **`answerImplements` (1286-1352) pairs every folded type with every folded
   interface**, and attributes the edge to the *subtype's* file
   (`declarationNode` -> `builder.edge(file, node.ID, ...)`). In step 2 the
   only interface is `app.Local`.
3. **`semanticDiff.finish` (1523-1554) retracts, per file in scope, every id in
   `e.emittedEdges[relPath]` not produced this time**, then overwrites
   `emittedEdges[relPath]` with this pass's edges. It cannot tell "this pass
   re-evaluated `Mine` against `lib.Binder` and it no longer holds" from
   "this pass never looked at `lib.Binder`". The whole-project pass recorded
   `Mine -> lib#Binder` under `app/app.go`, so the per-file pass retracts it.

Not the cause: `emittedEdges` keying (per file, which is right: edges are
attributed to the subtype's file), `answerFile` (only CALLS/uses), core.
Core's `file_rows.rs` keeps a `semantic` edge out of a surviving node on a
`Complete` reparse; the delete comes from the plugin's own `DeleteEdgeIds`.

The same retraction also fires in a **whole-project pass with a failed
module**: interfaces of the failed module are not folded, but files of the
loaded modules stay in scope (`withoutFailedModules`, 940-951), so their
SUPERTYPE_OF edges onto the failed module's interfaces are retracted.

Related, separate gap (not GM-590): a per-file pass of the *interface's* file
(`lib/lib.go`) never re-evaluates implementors in other packages. When
`Binder` gained a method `Mine` lacks, the pass upserted `impl`'s edge and
deleted nothing: `Mine SUPERTYPE_OF lib#Binder` stays stale until the next
whole-project pass. Fixing it needs reverse dependents (load the project or
the importers), so it is proposed as a backlog task, not folded in here.

Confidence: high. Reproduced deterministically; the ids deleted in step 2 are
exactly the ids step 1 emitted for the two out-of-package interfaces.

## Fix options

**A. Widen the universe to imported project packages.** In `foldPackage`,
walk `pkg.Types.Imports()` (transitively) and add project interfaces from
export data to `resolved.interfaces`. *Benefit*: exact re-evaluation against
imported interfaces, no extra `go list`, the types share one importer so
`types.Implements` works. *Risk*: does not cover an interface the
implementor's package does not import (`other.Binder2`, implicit
satisfaction), which is still retracted; adds import-graph walking and twin
addressing for those packages (`referenced` must include them so
`targetScopeOf` sees their twins).

**B. Carry forward what the pass could not re-evaluate (recommended).**
Record, per emitted SUPERTYPE_OF edge, the interface's package path, the
subtype's name and the interface's method names, plus the wire edge and its
placeholder node. In `finish`, for each prior SUPERTYPE_OF id of a file in
scope that was not re-produced: if the interface's package was **not folded
this pass**, the subtype still has a type node in this file and is still in
`namedTypes`, and it still has every recorded method name, re-upsert the
stored edge + placeholder and keep it in `emittedEdges`; otherwise retract as
today. *Benefit*: closes the drop for every package (imported or not) and for
the failed-module case; no extra load; local to `answerImplements`/`finish`.
*Risk*: a signature-only change that breaks satisfaction of an out-of-universe
interface leaves a stale edge until the next whole-project pass (name-check
catches removed/renamed methods); a little per-edge memory in the engine.
Re-upserting (not just "not retracting") matters: the placeholder node lives
in the importer's file, and core retires placeholder nodes a `Complete`
structural reparse does not upsert, so an edge kept only by omission could
lose its target.

**C. Load every project package's types on each per-file pass.** Exact, but a
whole-project `go list` + type-check per edit; unacceptable on large repos.

**Recommendation: B, optionally plus A.** B alone fixes the reported loss and
its siblings. A makes imported interfaces exact (no name-check
approximation) at the cost of more code; owner decision whether it rides in
this task (D1 below).

## Edit map (B)

| Where | Lines | Change |
| --- | --- | --- |
| `semanticEngine` struct / `newSemanticEngine` | 195-237 | add `supertypes map[string]map[string]supertypeRecord` (file -> edge id -> record); keep `emittedEdges` as is |
| `resolutions` struct / `newResolutions` | 310-368 | add `folded map[string]bool` (project package paths whose interfaces this pass folded) |
| `foldPackage` | 556-620 | set `into.folded[pkg.PkgPath] = true` after the `isProjectContainer` / `pkg.Types != nil` checks |
| `answerImplements` | 1286-1352 | after `builder.edge`, record `{ifacePkg, subtypeName, ifaceMethods, edge, placeholder node}` on the builder (new `semanticDiff.supertypes` map per file) |
| `semanticDiff` struct / `newSemanticDiff` | 1414-1430 | add the per-pass record map |
| `semanticDiff.finish` | 1523-1554 | signature gains `resolved *resolutions, structural map[string]structuralFile`; before retracting a prior id, apply the carry-forward rule; carried ids go back into `d.edges`/`d.nodes` and into `emittedEdges[relPath]`; update `e.supertypes` |
| `semanticEngine.run` | 883-933 | pass `resolved, structural` to `finish` |

Callers (grep fallback; g-mesh `find_callers` on `semanticDiff.finish`,
`semanticEngine.run` and `answerImplements` returned `[]` with
`provenance.semanticTier: "absent"`, i.e. method calls through receivers not
yet indexed in this worktree): `finish` <- `run` (semantic.go:924);
`answerImplements` <- `run` (semantic.go:922); `run` <- `pluginState.handleSemanticPass`
(control.go:270) <- control loop (control.go:322). `emittedEdges`: only
semantic.go 219/235/1541/1546 (grep; g-mesh `find_references
semanticEngine.emittedEdges` returned "no symbol").

## Interaction with GM-589

GM-589 (`a766ed6`, branch `fix/GM-589-go-twin-structural-reanswer`) touches
`semantic.go` hunks at 247-259 (`structuralFile`), 278-287
(`extractStructural`), 1095-1101 and 1123-1176 (`answerFile`), plus
extract.go/open_sites.go/uses.go. It does **not** touch `finish`,
`emittedEdges`, `answerImplements`, `foldPackage`, `loadFor` or `run`. It
relies on `finish` retracting, through `emittedEdges`, a semantic use edge it
did not re-produce; option B keeps that behaviour for every edge kind other
than SUPERTYPE_OF and changes SUPERTYPE_OF only when the interface's package
was not folded. GM-590 does not touch `answerFile`. If GM-589 changes the
`structuralFile` shape, B only reads `typeNodes`, which GM-589 leaves alone.
Implement after GM-589 merges; expect no textual conflict.

## Behaviours to pin (each with its control)

1. **Per-file pass keeps an imported-package implementor** (two packages,
   whole pass then per-file pass of the implementor's file: the
   `Mine SUPERTYPE_OF lib#Binder` id is not in `DeleteEdgeIds` and is in
   `UpsertEdges`). Control: make `finish` retract unconditionally (today's
   code) -> fails.
2. **Same for a non-imported package's interface** (`other#Binder2`).
   Same control. If A is taken, a separate control for A: drop the import
   walk -> the imported case must still pass by carry-forward, so A's own
   pin is "a signature change against an imported interface retracts"
   (control: remove A -> edge is carried, test fails).
3. **A removed method still retracts**: edit `Mine` to drop `Bind`, per-file
   pass retracts `Mine SUPERTYPE_OF lib#Binder`. Control: skip the
   method-name check -> edge carried, fails.
4. **A removed/renamed subtype is not resurrected**: rename `Mine`, per-file
   pass must not upsert an edge out of the old node id. Control: skip the
   "subtype still has a type node" check -> fails.
5. **Carry-forward survives a second per-file pass** (two consecutive
   per-file passes keep the edge; guards `emittedEdges` being overwritten
   without the carried ids). Control: do not append carried ids to
   `emittedEdges` -> second pass loses it.
6. Existing `TestSemanticPassRetractsWhatAReCheckNoLongerProduces` must stay
   green (non-SUPERTYPE_OF retraction unchanged).
7. End to end: restore `app.Mine` in fixture
   `docs/results/GM-519-fixtures/go/g02-build-tag-twins` and its expectation
   for `find_implementations Binder` after the kit's per-file pass.

## Decisions for the owner

- **D1**: B alone, or B + A (exact for imported interfaces, more code)?
  Recommended: B alone now; A only if the stale-on-signature-change window
  matters.
- **D2**: file the reverse gap (interface edit does not re-evaluate
  implementors in other packages; stale edges until a whole-project pass) as
  a backlog task?
