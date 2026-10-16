# GM-589: the Go semantic pass re-answers structural uses of build-tag twins

Follow-up of GM-570 D3. Plugin-only change in `plugins/go`; no core, wire or
schema change.

Evidence tags: **[run]** measured on an index, **[code]** read in the source
at branch `fix/GM-589-go-twin-structural-reanswer` (off `release-4.10.0`).

## 0. Summary

The structural tier addresses `binding.JSON` as "name `JSON` in package
`binding`". gin declares `JSON` in two build-tag twins, so core's linker sees
two candidates and refuses the edge. GM-570 fixed the semantic tier's own
answers (114 edges), but the semantic pass leaves structural edges alone
unless they are conversions. This design records every structural use onto a
placeholder, and lets the semantic pass replace exactly the ones `go/types`
resolves to a twinned name: retract the structural edge, emit a file-scoped
semantic edge onto the host-built twin. Expected on gin: +236 linked edges
(semantic tier on), 5 stay refused correctly.

## 1. What happens today

### 1.1 The refused edges on gin [run]

Source: GM-570/S13's after-index (gin `73726dc`, GM-570 plugin `1b9b5ef`),
`gm570-s13/index-after.db`, the GM-570 twin query restricted to syntactic
`name`-keyed container placeholders, broken down by name (`gm589/tw.sql` in
the scratchpad).

| | edges |
|---|---|
| syntactic, unresolved, onto an address with declarations in >=2 files | **241** |
| of which `REFERENCES` | 221 |
| of which `CALLS` | 20 (`validate` 16, `Default` 4) |
| distinct `from` symbols | 152 |

By name: `JSON` 37, `MIMEPOSTForm` 23, `Form` 18, `validate` 16, `Binding`
13, `XML` 12, `YAML` 12, `TOML` 11, `FormMultipart` 10, ... 32 names, all in
`binding/binding.go` + `binding/binding_nomsgpack.go`, except `json` (5, in
`codec/json/jsoniter.go` + `sonic.go`) and `Package` (1, four
`codec/json/*.go` files).

By referencing file: `binding/binding_test.go` 132, `context.go` 45,
`context_test.go` 30, `binding/validate_test.go` 7, `codec/json/go_json.go`
5, 12 more files with 1-4 each.

The 5 `json` rows all come from `codec/json/go_json.go` (`//go:build
go_json`), which the host build does not compile. There `json` is the
imported `github.com/goccy/go-json` package, which the structural tier
mistook for the package-level var `json` of its twins. go/types never
type-checks that file, so it has no answer; they stay refused, correctly.
The remaining **236** have a host-built answer: 235 onto
`binding/binding.go` (`//go:build !nomsgpack`), 1 (`json.Package` in
`context_test.go`) onto `codec/json/json.go`.

### 1.2 One example, end to end [code]

`context.go`'s `Context.ShouldBindJSON` uses `binding.JSON`:

1. `useSelector` (uses.go 704-749): the base `binding` is an import, so
   `emitUse(ctx, false, placeholderFor("github.com/gin-gonic/gin/binding",
   "JSON"), ...)`.
2. `emitUse` (uses.go 751-760): not a call, so it adds one `REFERENCES`
   edge from `Context.ShouldBindJSON` onto the placeholder, keyed `name`
   `JSON` in container `.../binding`. Nothing is recorded for the semantic
   pass: only a `CALLS` onto a placeholder is recorded (`placeholderCall`).
3. Core links it: `Resolver::choose` (core/src/graph/symbol_links.rs
   1792-1818) gets two `Variable` candidates (binding.go, binding_nomsgpack.go),
   `several =>` and no accessor-getter rule applies, so `None`: refused.
4. The semantic pass knows the answer (`types.Info.Uses` at that position is
   `binding.JSON` declared in binding.go), but `answerFile` (semantic.go
   1100-1153) only walks `file.openSites` and `file.placeholderCalls`. The
   `REFERENCES` site is in neither list.

For `validate(obj)` in `binding/form.go` (a bare same-package name, `useIdent`
step 4, uses.go 653-700) the site *is* recorded as a `placeholderCall`, but
`answerFile` skips it at `if !ok || isFunc { continue }`: "the structural
tier was right and core has already linked its edge". With twins core has
not.

### 1.3 How a retraction reaches core [code]

`semanticDiff.retract` (semantic.go 1513-1518) marks the edge id; `finish`
(1523-1554) puts it in `DeleteEdgeIds` unless the pass also upserts that id.
Core applies the semantic diff in `apply_semantic_diff`
(core/src/storage/write.rs 342-357), and `write_diff` runs
`DELETE FROM edges WHERE id = ?1` for each id (write.rs 390-392). When the
file is next re-extracted structurally, core replaces the file's syntactic
edges (the structural edge comes back) and keeps its semantic edges
(core/src/storage/file_rows.rs 82-91, `keep_semantic`); the semantic pass that
follows the edit retracts it again. This is exactly the conversion flow
(`TestSemanticPassRetractsCallsOntoAType`, semantic_test.go 231).

## 2. The design

### 2.1 Record every structural use onto a placeholder

`placeholderCall` becomes `placeholderUse` with one more field, `Kind`
(`CALLS` or `REFERENCES`, the kind of the structural edge `EdgeID` names).
`emitUse` records a use whenever its target is a placeholder, on both
branches:

```go
if isCall && targetMayBeFunction && ctx.callerID != "" {
    e.addEdge(ctx.callerID, edgeKindCalls, toID)
    e.recordPlaceholderUse(at, ctx, edgeKindCalls, ctx.callerID, toID)
    return
}
e.addEdge(ctx.symbolID, edgeKindReferences, toID)
e.recordPlaceholderUse(at, ctx, edgeKindReferences, ctx.symbolID, toID)
```

`recordPlaceholderUse` keeps today's guards (no record when `at` is nil, the
from id is empty, or `toID` is not a placeholder) and computes `EdgeID` as
`edgeIDFor(from, kind, toID, nil)`, the id `addEdgeWithSpecifier` gave the
edge. Every site is recorded, as today for calls: several sites of one
`from` share one structural edge, and the retraction below is idempotent.

Covers both paths that mint a placeholder: `useSelector`'s `pkg.Name` and
`useIdent`'s sibling-file bare name. Not covered, deliberately: dot-import
bare names (already open sites) and same-file declarations (no placeholder).

### 2.2 `answerFile` re-answers a use whose answer is file-scoped

The second loop of `answerFile` walks `file.placeholderUses`:

| go/types answer at the site | today | after |
|---|---|---|
| none, or not a project declaration (`addressOf` not ok) | leave | leave |
| a func, structural edge `CALLS`, scope container | leave | leave |
| a non-func, structural edge `CALLS` (conversion) | retract + `REFERENCES` (container or file scope) | unchanged |
| a non-func, structural edge `REFERENCES`, scope container | (not recorded) | leave |
| **a func, structural `CALLS`, scope file** | leave (refused) | **retract + semantic `CALLS` onto the file-scoped placeholder** |
| **any, structural `REFERENCES`, scope file** | (not recorded) | **retract + semantic `REFERENCES` onto the file-scoped placeholder** |

"Scope file" is `resolved.targetScopeOf(obj, container, key).File != ""`,
GM-570's own rule: the name is twinned in its package and the declaring file
is one the host build compiles. Only those uses are touched, so the diff
grows by the twinned uses only (236 retractions and 236 edges on gin, against
4146 structural `name`-keyed edges that stay untouched).

The semantic edge's `from` is the structural edge's own `from`
(`EnclosingCallerID` for `CALLS`, `EnclosingSymbolID` for `REFERENCES`; for a
call these are the same node whenever a `CALLS` edge was emitted, as
`placeholderCall`'s comment already notes). Its placeholder comes from
`builder.placeholder(...)` with the file scope, so its id hashes the file in
(GM-570 1.4) and cannot collide with the structural placeholder.

### 2.3 What is still refused, and why that is right

- `go_json.go`'s 5 `json.X` uses: the file is not host-built, so go/types has
  no position there. Linking them to any twin would be wrong (1.1).
- Structural-only indexing (no `go` toolchain, semantic tier off): nothing
  changes; the 241 stay refused.
- A twinned name whose host-built declaring file is outside the project or
  behind a `//line` directive: `targetScopeOf` keeps the container scope, so
  the use is left alone (GM-570 T6).

## 3. Options

| | benefit | risk | gin |
|---|---|---|---|
| **A. Record placeholder uses, re-answer only file-scoped ones (2.1-2.2)** | Reuses GM-570's `targetScopeOf` and the conversion retraction path; diff grows only by twinned uses; no core change | Semantic tier only; one more per-site list held per file during a pass; the gap in D2 | +236 |
| B. Same recording, re-answer every resolved placeholder use | One rule, no twin check | Thousands of retract/re-emit pairs for edges core already links (4146 `name`-keyed structural edges on gin), much larger diffs, every edit churns them | +236, same |
| C. Core picks the host-built twin for a structural `name` key, from a plugin-supplied "host-built files" fact | Also fixes structural-only indexing | New wire fact and core linker rule; core would choose between candidates by a hint, against "a missing edge beats a wrong one" that GM-570 D1 kept | +236 (+ structural-only) |
| D. Leave it | Nothing to build | `find_references binding.JSON` stays empty on gin without a caveat | 0 |

**Recommendation: A.**

## 4. Owner decisions

### D1. Which option? (Recommended: A)

See section 3.

### D2. A name that stops being twinned (Recommended: a)

- **Today (with A):** context.go's structural `REFERENCES binding.JSON` is
  retracted and replaced by a semantic edge onto `binding/binding.go`.
- **Change that exposes a gap:** `binding_nomsgpack.go` is deleted. `JSON` is
  no longer twinned. If the same plugin process then runs a whole-project
  pass *without* core re-extracting context.go, the pass answers the use
  container-scoped, so per 2.2 it emits nothing; `finish` retracts the earlier
  semantic edge through `emittedEdges`; the structural edge it once retracted
  is not re-sent. The edge is missing until context.go is next edited (its
  re-extraction restores the structural edge, which now links).
- A per-file pass of the deleted file does not touch context.go, and a new
  plugin process does not know the earlier semantic edge (`emittedEdges` is
  in memory), so in those paths the file-scoped edge stays and still links
  correctly onto binding.go.
- **Options:**
  - **(a) Accept and document (Recommended).** Benefit: no extra state. Risk:
    a missing (never a wrong) edge after a rare sequence: a twin deleted, then
    an in-process whole-project pass, before the referencing file is edited.
  - **(b) Track retracted structural ids per file** (`semanticEngine.
    retractedUses[relPath]`) and, when a later pass answers such a use
    container-scoped, re-emit it via `builder.placeholder` with the container
    scope. That placeholder id and edge id equal the structural ones (GM-570
    `TestSemanticPassGivesAFileScopedPlaceholderItsOwnID`, the Meters case),
    so the upsert restores the edge. Benefit: no gap. Risk: one more map in
    the engine with its own lifecycle; the restored edge is labelled
    `semantic`; one more test and control.

## 5. Edit map (option A, D2 a)

| file | function / type | lines today | change |
|---|---|---|---|
| plugins/go/open_sites.go | `placeholderCall` (doc + type) | 87-118 | rename to `placeholderUse`, add `Kind string`; doc: recorded for every edge onto a placeholder; the semantic pass retracts it for a conversion or a twinned answer |
| plugins/go/open_sites.go | `recordPlaceholderCall` | 150-163 | becomes `recordPlaceholderUse(at, ctx, kind, fromID, toID)`; computes `EdgeID`; skips non-placeholder `toID` |
| plugins/go/uses.go | `emitUse` | 751-760 | record on both branches (2.1); doc comment |
| plugins/go/extract.go | `fileGraph.placeholderCalls` | 158-161, 217 | rename field `placeholderUses` |
| plugins/go/extract.go | `extractor.placeholderCalls` | 259 | rename |
| plugins/go/semantic.go | `structuralFile` | 253-260 | field `placeholderUses` |
| plugins/go/semantic.go | `extractStructural` | 272-293 | copy the renamed field |
| plugins/go/semantic.go | `answerFile` | 1100-1153 | second loop per the 2.2 table: conversion branch unchanged; new file-scope branch (retract, placeholder with file scope, edge of the structural kind) |
| plugins/go/semantic_test.go | `TestSemanticPassRetractsCallsOntoAType` | 231-283 | follow the rename (`graph.placeholderUses`, filter `Kind == CALLS`) |
| plugins/go/semantic_twins_test.go | `TestSemanticPassAddressesBuildTagTwinsByTheHostBuiltFile` | 272-306 | exact `want` list gains the fixture's twinned structural uses (e.g. `UseBinder CALLS file:lib/binding.go#New` if `New` is twinned, `Codec` type uses if twinned); the implementer derives them from the fixture, not from the output |

`targetScopeOf`, `semanticDiff.placeholder/edge/retract/finish`, `addressOf`
are used unchanged. Untouched: `loadFor`, `loadReferencedTwins`,
`collectTwins`, `recordBuiltFiles`, `twinnedNames`, `answerImplements`.

### 5.1 Overlap with GM-590 and GM-592

- **GM-592** (cross-module union of twin views; twin marking when a nested
  module fails to load) changes what `resolutions.twinned` / `builtFiles`
  hold (`collectTwins`, `recordBuiltFiles`, `loadReferencedTwins`,
  `loadFor`/`withoutFailedModules`). This design only *reads* them through
  `targetScopeOf`, so no textual conflict, and its behaviour follows
  automatically: whatever GM-592 marks twinned is re-answered here. One
  semantic coupling: a use is retracted only when `targetScopeOf` says
  "file", so a GM-592 change that un-marks a twin turns this design's
  re-answer off for it (the D2 gap applies).
- **GM-590** (find_implementations loses a cross-package implementor after a
  per-file pass) most likely touches `answerImplements` and/or
  `finish`/`emittedEdges` retraction. This design does not edit either, but
  relies on `finish` (a) not deleting an id it also upserts and (b) retracting
  the previous pass's semantic edges per file. If GM-590 changes `finish`'s
  per-file retraction, its design must keep (a) and (b) for this design's
  edges; if it chooses D2-(b)-like tracking, share one structure.
- Shared hot file: `answerFile` is edited here only; ask GM-590/592 not to
  touch it, or to rebase on this branch.

## 6. Behaviours the tests must pin, with controls

Fixture: extend `twinFixture` (semantic_twins_test.go 230) or a new probe
with, in package `lib`, a twinned package-level `var JSON`, a twinned `func
validate` used bare by a sibling file, and a non-twinned var used the same
way; in `app`, `lib.JSON` as a value and `lib.Validate()` as a call. One
whole-project pass, compared against the structural edges of the same files.

| # | behaviour | control (revert in code) |
|---|---|---|
| B1 | a structural `REFERENCES` onto a twinned var (`lib.JSON`) is retracted, and a semantic `REFERENCES` onto `file:lib/binding.go#JSON` is emitted | drop the file-scope branch for `REFERENCES` in `answerFile` |
| B2 | a structural `CALLS` onto a twinned func is retracted, and a semantic `CALLS` onto the file-scoped placeholder is emitted | restore `if !ok \|\| isFunc { continue }` |
| B3 | a bare same-package twinned name (`validate(x)` in a sibling file, `useIdent` step 4) is re-answered like B2 | record uses only in `useSelector`'s branch (skip in `useIdent`) |
| B4 | a use of a non-twinned project name is neither retracted nor re-emitted (diff small) | drop the `scope.File != ""` test (re-answer every resolved use) |
| B5 | a use go/types resolves outside the project (or has no answer for) is left alone | move `builder.retract` before the `addressOf` ok check |
| B6 | a twinned name whose declaring file is not host-built (GM-570's phantom/template case) keeps its structural edge | test `r.twinned` alone instead of `targetScopeOf` in the new branch |
| B7 | the recorded `EdgeID` of every `placeholderUse` is an edge the structural tier emitted (both kinds) | compute `EdgeID` with `edgeKindCalls` for every use |
| B8 | conversions unchanged (existing `TestSemanticPassRetractsCallsOntoAType`, `Convert REFERENCES file:...#Kelvin`) | none new: existing tests |
| B9 | a per-file pass of the referencing file re-answers the same way (imported package's twins via `loadReferencedTwins`) | none new if `TestSemanticPassPerFileKeepsTwinsOfAnImportedPackage` gains one `REFERENCES` row; its existing control covers it |

Controls: B1-B7 (7), grouped in one test binary (`plugins/go`, `go test
-run`). No processes, threads or timers beyond the existing `go list` probe
pattern, so no 5x repeats needed beyond the existing twin tests' policy.

## 7. Measure plan

Reuse GM-570/S13's `run.sh` on gin `73726dc` (clean), core release build of
`release-4.10.0`, `G_MESH_PLUGIN_ROOTS_OVERRIDE` per arm. Before: plugin at
`release-4.10.0` head; after: this branch. Record `uptime` and
`/usr/bin/time -p` for each reindex; confirm the semantic pass completed in
both arms.

Expected (from 1.1):

| count | before | after |
|---|---|---|
| twin query, syntactic `name` | 241 | **5** (all `codec/json/go_json.go`) |
| twin query, semantic | 0 | 0 |
| semantic linked (Go) | 1412 | **1648** (+236) |
| syntactic structural_name_keyed (Go) | 4146 | **3910** (-236) |
| syntactic linked (Go) | 1604 | 1604 |
| semantic Go edge rows | 1413 | 1649 |

Placement check: the 236 new resolved edges land on `binding/binding.go`
(235) and `codec/json/json.go` (1); none on `binding_nomsgpack.go`,
`jsoniter.go`, `sonic.go` or `go_json.go`.

Cheap observation (tells the arms apart): resolved edges onto the `JSON` var
node of `binding/binding.go`: 0 -> 37.

Any count off by more than 0 from this table is a finding to explain before
merging (e.g. a structural edge whose sites resolve differently).
