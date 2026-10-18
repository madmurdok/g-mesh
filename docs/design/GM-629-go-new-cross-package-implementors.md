# GM-629: an interface edit does not add implementors it newly gains in other packages

Status: design (GM-629/S1). No production or test change. Line numbers are
against `release-4.12.0` at `fc6654c` (GM-620 merged). Follows
`GM-620-go-interface-edit-implementors.md`, "What A does not cover", item 1.

## Today's behaviour

### Example (from the code; the tests slice reproduces it)

Module `example.com/x`:

```go
// lib/lib.go
type Binder interface{ Bind() string; Close() }

// other/other.go   (does not import lib)
type Plain struct{}
func (Plain) Bind() string { return "p" }
```

1. Whole-project pass: no `Plain SUPERTYPE_OF lib#Binder` (Plain lacks
   `Close`). Correct.
2. Edit `lib/lib.go` to `type Binder interface{ Bind() string }`; core sends a
   per-file pass of `lib/lib.go`.
3. Result: still no edge. `Plain` now satisfies `Binder`, but
   `find_implementations Binder` does not list it until `other/other.go` is
   re-passed *and* that pass sees `lib` (it does not: `other` does not import
   `lib`, so `lib.Binder` is not in that pass's `resolved.interfaces`), or the
   next whole-project pass (daemon start, workspace/config reindex).

The same holds for a signature change that a type elsewhere now matches
(`Bind(int)` -> `Bind() string`) and for a **newly added** interface
(`type Closer interface{ Close() }` in `lib`): types in other packages that
satisfy it get no edge until the next whole-project pass. The edge is
*missing*, never wrong.

### Why (code, `plugins/go/semantic.go`)

- `run` (1136-1215) folds only the edited file's package (`loadFor`,
  1262-1320, `file=` patterns). `answerImplements` (1593-1672) pairs only the
  named types of folded packages with the known interfaces, so `other.Plain`
  is never a candidate.
- GM-620's widening (`staleImplementorFiles`, 2080-2130) starts from
  `e.supertypes`, the records of edges *already emitted*. A type that never
  had an edge has no record, so it is never added to the second load.
- The plugin keeps no process-level knowledge of types it is not currently
  loading, and no baseline of interface signatures except inside records.

## Probe (cost estimate, one sample)

`golang.org/x/tools@v0.50.0` (copy of the module cache, 215 packages, 838
non-alias non-generic named types, 50 method-set interfaces, plugin's rules:
`T` or `*T`, pointer method set). Each "edit" is one method removed from one
interface with >= 2 methods (130 edits); "signature change" is one per
interface (50), counting types that have every method name but do not satisfy
today. Probe program and raw output: GM-629/S1 scratch
(`gm629-s1/probe/main.go`, `probe-out.txt`).

Machine: `uptime` load average 5.42 at start, 15.19 at end (the probe's own
loads). `/usr/bin/time -p`: real 78.48, user 71.25, sys 45.01 (CPU-bound, not
waiting). Timings are single runs, not a measurement.

| Per edit | p50 | p90 | p99 | max |
| --- | --- | --- | --- | --- |
| method-name candidate packages (scan below) | 0 | 1 | 5 | 50 |
| method-name candidate files | 0 | 1 | 5 | 71 |
| packages actually gaining an implementor | 0 | 0 | 3 | 50 |
| direct importers of the interface's package (option C) | 2 | 14 | 44 | 72 |
| signature change: name-candidate packages | 0 | 1 | 4 | 4 |

- 6 of 130 removals create a new cross-package implementor; in **4 of those
  6** at least one new implementor package does **not** import the interface's
  package. Reverse imports (option C) would miss them.
- Name candidates are precise: in the two largest cases every candidate
  package really gains an edge (`vta.node -Type`: 50/50, it leaves
  `String() string`; `ssa.varlist -At`: 17/17).
- Load cost, per-file-style `packages.Load` (plugin's `loadMode`,
  `Tests: true`, `file=` patterns) of the interface's file plus N candidate
  packages: N=0 1.2 s, N=1 2.3 s, N=3 6.4 s, N=10 13.1 s, N=30 41.8 s
  (~1.3 s per extra package on this machine). For comparison a
  whole-project load was 12.0 s with `Tests: false`.
- Index size for the scan: 838 types, 2,893 method names: negligible memory.
- An import-graph listing (`NeedName|NeedImports ./...`) costs 1.4 s per
  call, which option C would pay per interface edit unless it keeps a graph.

## Options

### A. Method-name candidate scan, folded into GM-620's second load (recommended)

Keep two process-level indexes, refreshed by every pass for every package it
folds from source (a whole-project pass fills them):

- `e.typeIndex[pkgPath]`: per named type of that package, its declaring file
  (`resolved.declaredIn`) and method-name set (`methodNames`, the same
  pointer-method-set rule `answerImplements` prefilters with);
- `e.ifaceSigs[pkgPath]`: `name -> interfaceSignature(iface)` for its
  interfaces (the GM-620 canonical string).

In a per-file pass, after the first load: an interface of a folded package is
*loosened-or-new* when the package has a baseline in `e.ifaceSigs` and the
interface is absent from it or its signature differs. For each such
interface, candidates are the files of indexed types that

- are in another package and not already in scope (or in GM-620's extra set),
- have every method name of the interface's new method set,
- have no `e.supertypes` record onto that interface (those are GM-620's).

Candidate files join GM-620's `extra` set, so there is still **at most one**
second load; `answerImplements` then decides exactly with `types.Implements`
in one universe, and `finish` records the new edges in `emittedEdges`/
`supertypes` as for any scope file.

- *Cost, common edit* (body, comment, unrelated decl): one map comparison per
  folded interface, no load. Same gate promise as GM-620.
- *Cost, loosening edit*: the second load plus the candidate packages:
  p90 one package (~+1 s), p99 5, max 50 (~+65 s, extrapolated) in x/tools.
- *Benefit*: exact; covers importers **and** non-importers, method removal,
  signature change, new interfaces; reuses GM-620's second load and `finish`.
- *Risk*: a loosening to a small, common method set (`String() string`)
  re-folds every package with such a type; one pass then holds the plugin's
  sequential control loop for tens of seconds. Hence the bound.
- *Risk*: the indexes are as fresh as the last pass of each package; a type
  added since, in a package not re-passed, is missed (same residual as today,
  smaller window).

**The bound**: at most `K` candidate *packages* per pass (owner decision D1,
proposed 16). Above `K`, add none of them, log one line naming the
interface(s) and the count, and leave those edges to the next whole-project
pass (the documented residual). All-or-nothing keeps the result independent of
map order. GM-620's own implementor re-checks are not counted against `K`
(they correct wrong edges; GM-620 D2 decided separately).

### B. Bounded reverse-import scan (option C of GM-620, bounded)

Re-fold the direct importers of the edited package, capped. *Cost*: p90 14,
p99 44, max 72 packages per edit in x/tools, paid on every interface change
(not only loosening ones), plus an import graph to keep or list (1.4 s). It
still misses non-importers: 4 of the 6 real cases in the sample. Rejected:
more cost for less coverage than A.

### C. Accept and document

Leave the residual: the edge is missing until the next whole-project pass.
*Benefit*: no code, no cost. *Risk*: in a long daemon session that has no
bound in time; `find_implementations` under-reports after an interface is
narrowed or added. Kept as the fallback for what A's bound leaves out.

## Recommendation

A with `K = 16` (D1). x/tools: 128 of 130 removal edits are under the bound;
the two over it (50 and 17 packages) are logged and left to the whole-project
pass. Include newly added interfaces (D2).

## Edit map (implement slice)

All in `plugins/go/semantic.go`.

| Where | Lines | Change |
| --- | --- | --- |
| `semanticEngine` struct | 195-241 | add `typeIndex map[string][]indexedType` (`file`, `name`, `methods map[string]bool`), `ifaceSigs map[string]map[string]string`, `newImplementorFiles []string` (read by tests, like `recheckedImplementors` 227-231), `skippedNewImplementors int` (count over the bound, for tests and the log) |
| `newSemanticEngine` | 243-250 | init the two maps |
| new `foldedInterfaceSigs(resolved) map[string]map[string]string` | next to `staleImplementorFiles` | factor the `sigs` build of `staleImplementorFiles` (2097-2108) so both use one rendering |
| `staleImplementorFiles` | 2080-2130 | use the helper; behaviour unchanged |
| new `(e) newImplementorCandidates(resolved, scope, extra []string) (files []string, overBound int)` | next to `staleImplementorFiles` | gate on baseline + changed/new sig; candidate rule above; group by package; `len(pkgs) > maxNewImplementorPkgs` -> nil, count; drop files gone from disk (`os.Stat`, as 2117-2124) |
| new `var maxNewImplementorPkgs = 16` | package level | a `var` so a test can lower it |
| new `(e) refreshIndexes(resolved, failed)` | after `answerImplements` in `run` | for each `resolved.folded` package: replace `typeIndex[pkg]` from `resolved.namedTypes`/`declaredIn`, replace `ifaceSigs[pkg]` from the helper; skip packages of failed modules |
| `run` | 1171-1185 | compute `cand := e.newImplementorCandidates(resolved, scope, extra)` right after `extra`; widen with `unionSorted(extra, cand)`; one second load as today; log both counts; call `refreshIndexes` after `answerImplements` (1203) in both whole-project and per-file passes |

The baseline must be read *before* `refreshIndexes` overwrites it in the same
pass; the order in `run` above guarantees that.

Unchanged: `loadFor`, `foldPackage`, `answerImplements`, `carryForward`,
`finish`, `withoutFailedModules`/`keepPathsIn` (a candidate in a failed module
is dropped and not reported unfinished, as GM-620's extra files), core.

Functions to read before implementing: `run`, `staleImplementorFiles`,
`answerImplements`, `methodNames`, `hasAllMethods`, `interfaceSignature`,
`finish`; tests: `semantic_iface_edit_test.go` (`ifaceEditPass`,
`requireRechecked`), `semantic_supertype_carry_test.go` (`carryWholePass`,
`carryRewrite`, `supertypeEdgesByLabel`).

### Callers / references relied on (and which call produced them)

- `staleImplementorFiles` <- `semanticEngine.run` only (g-mesh `find_callers
  semanticEngine.staleImplementorFiles`, main checkout index = release code).
- `run` <- `pluginState.handleSemanticPass` (control.go 269) only (g-mesh
  `find_callers semanticEngine.run`).
- `hasAllMethods` <- `answerImplements` only (g-mesh `find_callers
  hasAllMethods`): reusing it in the scan adds a second caller, no signature
  change.

## Behaviours the tests must pin (tests slice)

New tests in `plugins/go/semantic_iface_edit_test.go` (or a sibling
`semantic_new_implementor_test.go`). Each: whole-project pass, rewrite
`lib/lib.go`, per-file pass of `lib/lib.go` only, unless stated.

1. **Removed method, non-importer gains the edge**: the example above;
   `Plain SUPERTYPE_OF lib#Binder` upserted; `other/other.go` in
   `newImplementorFiles`.
2. **Removed method, importer gains the edge** (type in `app`, which imports
   `lib`).
3. **Signature change gains the edge** (`Bind(int)` -> `Bind() string`), while
   GM-620 retracts the old implementor in the same pass.
4. **New interface gains implementors** in another package (if D2 = yes).
5. **Name match, type mismatch**: candidate has `Bind() int`; it is loaded
   (in `newImplementorFiles`) and gets no edge.
6. **Body-only edit loads no candidate** (`newImplementorFiles` nil, no
   second load).
7. **Over the bound adds nothing**: `maxNewImplementorPkgs = 1`, two
   candidate packages -> no new edge, `skippedNewImplementors == 2`.
8. **No baseline, no scan**: a per-file pass on a fresh `pluginState` (no
   whole-project pass) does not widen.
9. **Index follows per-file passes**: per-file pass of `other/other.go` that
   adds `Plain.Bind`, then the loosening of `lib` -> edge added.
10. Existing GM-590/GM-620 tests stay green.

These spawn `go list`: run them 5 times in the tests slice and in verify.

## Proposed controls (revert code, never the test)

| # | Revert | Must fail |
| --- | --- | --- |
| C1 | `newImplementorCandidates` returns nil | 1, 2, 3, 4 |
| C2 | gate always true (scan every folded interface) | 6 |
| C3 | bound check removed | 7 |
| C4 | missing baseline treated as "changed" | 8 |
| C5 | `refreshIndexes` only on whole-project passes | 9 |
| C6 | scan uses the interface's *old* method names (from the baseline) | 1 |

Six controls, all in one test binary (`go test -run` in `plugins/go`).

## Must confirm (later slices)

- **Cross-module candidates**: `loadFor` loads per module; a candidate in
  another module is in a separate `packages.Load`, so `types.Implements`
  compares across universes. Exact for signatures of basic/universe types
  only; a test or probe must show what happens otherwise (expected: missing,
  never wrong).
- **Value of K** on a second, larger repo (measure slice): candidate-package
  distribution and seconds per extra package; record `uptime` and
  `time -p`.
- **Test variants**: types in `_test.go` files and external `_test`
  packages appear in `typeIndex` with the right `declaredIn` file.
- **Generic/alias/constraint rules** match `foldPackage`'s (the index is
  built from `resolved.namedTypes`, which already applies them).
- **Interplay with GM-620 D2** (cap on implementor re-checks), if one was
  adopted: whether one combined cap or two.

## Decisions for the owner

- **D1 Bound K.** Today: no candidate is ever loaded. Change: load up to K
  candidate packages per interface edit, none above it. Example: removing
  `Type()` from `vta.node` leaves `String() string`; 50 packages qualify.
  Consequence: K = 16 bounds a pass to ~+20 s here and leaves that case to
  the whole-project pass; K = 64 is exact in x/tools but one save can cost
  ~+65 s. Recommended: 16.
- **D2 New interfaces.** Today: adding `type Closer interface{ Close() }`
  gives types elsewhere no edge until a whole-project pass. Change: treat a
  new interface like a loosened one (same scan, same bound). Consequence:
  adding an interface with a common method set triggers the scan.
  Recommended: yes.
- **D3 Recommendation itself**: A (scan, exact, bounded) vs C (document the
  residual, no cost). Recommended: A.
