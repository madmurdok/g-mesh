# GM-633: Go SUPERTYPE_OF edges across modules that share no build

Found by GM-621/S11 verify. Pre-existing (same on base `53e6314`),
independent of build-tag twins.

## Today

Probe: a copy of `plugins/go` in the slice scratch dir with a throwaway
`_test.go` calling `newPluginState(root).handleSemanticPass(scope)`,
`GOWORK=` and `GOFLAGS=` cleared (as `modulesPass` does).

```
a/go.mod   module example.com/a
a/j/j.go   type J interface{ Error() string }
           type W interface{ To(io.Writer) }
b/go.mod   module example.com/b                 // no require, no replace
b/s/s.go   type S struct{ error }; func (S) To(io.Writer) {}
```

| pass | SUPERTYPE_OF from `b/s/s.go` |
|---|---|
| whole project (fresh process) | `S SUPERTYPE_OF container:example.com/a/j#J` |
| per-file `[b/s/s.go]`, fresh process | none |
| per-file `[b/s/s.go]` after the whole pass, same process | `... a/j#J` (carried forward, GM-590) |

`S` satisfies **both** `J` and `W` by method set, yet only `J` gets an edge.

### Why

- `loadFor` (semantic.go:1262) runs one `packages.Load` per module root for a
  whole pass; `load` (425) folds every root package of each load into one
  shared `resolutions`. `foldPackage` (718) puts every project named type in
  `namedTypes` and every project interface in `interfaces`, from whichever
  load produced it; `foldImportedInterfaces` (794) adds the interfaces of
  imported project packages from that load's export data.
- `answerImplements` (1593) then tests the full cross product
  `namedTypes x interfaces` with `types.Implements`, with no regard for which
  load each object came from. Module `a`'s `J` and module `b`'s `S` meet
  there although no build of either module sees both.
- `types.Implements` across two loads is only meaningful when every type in
  the method signatures is predeclared (`string`, `error`, `int`, ...): those
  are the shared `types.Universe`. Any other type (`io.Writer`, a project
  type) is a distinct `*types.Named` per load, so the check fails. That is why
  `J` matches and `W` does not. The cross-module edges today are an accident
  of signature shape, not a policy.
- A per-file pass loads only the modules of the files in scope, and the
  file's package folds only its own interfaces plus those it imports. `a/j`
  is in neither, so nothing is produced. In the same process `carryForward`
  (2026) keeps the whole pass's edge (the interface is "unevaluated"), so the
  two disagree only for a fresh plugin process or a scope that loads the
  interface's module separately.

The same mechanism produces the GM-621/S11 fork case: `nested/lib/t.go:T`
(loaded by module `nested`) gets `SUPERTYPE_OF file:nested-fork/lib/i.go#I`,
an interface of the fork module (`nested-fork`, reached through root's
`replace`), which `nested`'s build never sees. Test 10
(`TestSemanticPerFilePassListsCopiesOfAFoldedPackage`,
semantic_twins_modules_test.go:439) already works around it: "Only this
edge is compared with the whole-project pass".

### Same build vs different builds (probe)

| fixture | whole pass | fresh per-file |
|---|---|---|
| one module, `s` and `j` packages, no import between them | `s.S -> m/j#J` and `s.S -> m/j#W` | none (carry-forward in process) |
| `b` requires `a` via `replace ../a`; `b/u` imports `a/j`; `b/s` does not | `b/s.S -> a/j#J`, `b/u.U -> a/j#J` | `[b/u/u.go]`: `U -> a/j#J`; `[b/s/s.go]`: none |
| unrelated `a`, `b` (above) | `S -> a/j#J` only | none |

Inside one load, implicit satisfaction across packages that do not import
each other is exact and intended (GM-590 keeps it). Only pairs from two
different loads are the problem.

## Is a cross-build implements meaningful?

`SUPERTYPE_OF` answers `find_implementations`: "which types can be used as
this interface". Across two modules with no build in common the answer is
"none can": `b` cannot name `a/j.J`, `a` cannot name `b/s.S`. It is noise for
unrelated forks, vendored copies and sample modules, and it is misleading
even for a monorepo because it is incomplete: it lists only implementors
whose signatures are predeclared-only (above). For a monorepo whose modules
require each other (`replace`/`go.work`), the case that matters, the
interface's package is in the implementor module's load whenever any package
of that module imports it, and that pair is kept by every option below.

What no option here gives: module `app` imports `lib` (interface) and `impl`
(type), `impl` does not import `lib`. `impl.S` is usable as `lib.J` only in
`app`'s build. Today it is edged only by accident (predeclared signatures);
it would need `app`'s load to fold the *types* of imported project packages
too (see D2).

## Options

**A. Same-load rule in `answerImplements` (recommended).** Tag every named
type and interface with the load it came from (the module dir `loadFor`
loaded it from), and test a pair only when both tags are equal. Whole pass
and per-file pass apply the same rule. *Benefit*: deterministic, matches what
the compiler can actually assign, removes the signature-shape accident and
the fork-copy edge; a per-file pass with files of two modules in scope stops
pairing them too; smaller cross product. *Risk*: users who saw the accidental
cross-module edges lose them (they were incomplete anyway); the `app`/`impl`
case stays unanswered (D2).

**B. Add the edges in the per-file pass.** Load every module whose interfaces
might match on each per-file pass, as the whole pass does. *Benefit*: per-file
agrees with today's whole pass. *Risk*: a `go list` + type-check of every
module per edit (GM-590 option C, rejected for cost), and it entrenches an
answer that is wrong for `W`-shaped interfaces.

**C. Scope by `go.work` / `require` / `replace`.** Pair a type with an
interface when the type's module requires the interface's module (or both
are `use`d by one `go.work`). *Benefit*: keeps cross-module pairs in a
monorepo even when no package of the implementor module imports the
interface. *Risk*: needs a go.mod require graph the plugin does not keep
(`workspace.go` reads only the `module` line); the pairs still come from
different loads, so `types.Implements` still fails for non-predeclared
signatures: same accident, fewer modules. A "required but not imported"
interface is also not one the implementor's build can assign to.

## Recommendation

A. Close the per-file side as it is: no change needed, it already applies the
rule implicitly (one module per load, folded package plus imports).

## Edit map (A)

| site | file:line | change |
|---|---|---|
| `resolutions` | semantic.go:323 (fields 335-343) | add `loadOf map[*types.TypeName]string` (the module dir of the load an object was folded from); init in `newResolutions` (399) |
| `loadFor` | semantic.go:1262 | pass `dir` to `load` (1283) |
| `load` | semantic.go:425 | take the module dir, hand it to `foldPackage` (441) |
| `foldPackage` | semantic.go:718 | record `loadOf[typeName] = dir` next to `namedTypes` (774) / `interfaces` (781); pass `dir` to `foldImportedInterfaces` (751) |
| `foldImportedInterfaces` | semantic.go:794 | record `loadOf[typeName] = dir` at 819; pass `dir` in the recursion (827). `importsWalked` is keyed by `*types.Package`, already per load |
| `answerImplements` | semantic.go:1593, pair loop 1631-1650 | `continue` when `loadOf[subtype.obj] != loadOf[ifaceObj]`, before `hasAllMethods`; comment says why (types.Implements across loads matches only predeclared signatures; no build sees both) |
| plugin README | plugins/go/README.md:149-152 | the "Dependents are refreshed" bullet: `SUPERTYPE_OF` pairs a type only with interfaces its own module's build sees; no edges between modules that share no build |
| test 10 | semantic_twins_modules_test.go:433-456 | can now compare every `SUPERTYPE_OF` from `t.go` with the whole pass, and assert the fork-copy edge is absent |

Read, not change: `carryForward` (2026), `staleImplementorFiles` (2080),
`unevaluated` (2171). Records are only produced for same-load pairs, so
carry-forward carries nothing cross-load. A stale cross-load record held by a
running plugin from before the upgrade cannot exist: the plugin restarts with
the new binary, and core's whole-pass sweep (core/src/watcher/apply.rs:787,
`sweep_semantic_edges`) deletes the old rows.

`loadReferencedTwins` (497) / `listUncoveredCopies` (560) list files only
(no fold), so they add no tagged objects.

## Behaviours tests must pin (one control each)

1. **Unrelated modules: no edge, whole and per-file agree.** Fixture above:
   whole pass has no `b/s/s.go:S SUPERTYPE_OF` row; fresh per-file
   `[b/s/s.go]` has none either; mixed per-file `[a/j/j.go, b/s/s.go]` has
   none. Control: drop the `loadOf` comparison -> whole pass and the mixed
   pass emit `container:example.com/a/j#J`.
2. **Same module, packages that do not import each other: kept.** Whole pass
   still emits `s.S -> m/j#J` and `s.S -> m/j#W` (implicit satisfaction,
   GM-590). Control: tag by package path instead of load dir -> edges vanish.
3. **Cross-module in one build: kept.** `replace` fixture: whole pass emits
   `b/u/u.go:U` and `b/s/s.go:S SUPERTYPE_OF container:example.com/a/j#J`
   (the latter via `b/u`'s import into `b`'s load). Control: tag
   `foldImportedInterfaces` objects with a different value (or skip tagging
   them) -> both vanish.
4. **Fork copy: whole equals per-file.** `forkedRootWithLibImplementor`:
   the whole pass's `SUPERTYPE_OF` rows from `nested/lib/t.go` equal the
   per-file `[nested/lib/t.go]` rows, and include no `nested-fork` target.
   Control: same as 1.

Controls 1 and 4 share one revert; 2 and 3 are separate patches. All land in
the `plugins/go` test binary: one `go test` per control.

## Overlap in this batch

- **GM-631** edits `writeCanonicalType` (1743-1860) / `interfaceSignature`
  (1726). No shared lines with this edit map.
- **GM-629** (newly satisfied implementors after an interface edit) will add
  a candidate scan feeding SUPERTYPE_OF and likely touches `answerImplements`,
  `staleImplementorFiles` or `run` (1136), and the same README bullet
  (149-152). Its candidates must obey the same-load rule, or it reintroduces
  the cross-module edges in a per-file pass. Whichever lands second rebases
  over the other's `answerImplements` hunk and the README bullet.

## Must confirm (owner)

- **D1 - policy.** Today: the whole pass emits `S SUPERTYPE_OF J` between two
  Go modules that share no build, only when the method signatures use
  predeclared types; a per-file pass after a restart does not. Change: pair a
  type only with interfaces its own module's load sees (option A).
  Example: unrelated `a` (`J{Error() string}`) and `b` (`S struct{error}`):
  no edge in any pass; `b` requiring `a` with a `b` package importing `a/j`:
  edge kept. Consequence: `find_implementations("J")` stops listing types of
  modules that cannot use them; nothing else in the graph changes.
  Recommended: A.
- **D2 - follow-up for the `app`/`impl` case.** Today: a type in module
  `impl` that satisfies an interface of module `lib` gets an edge only by
  accident, even when module `app` imports both. Change (separate task): fold
  imported project packages' named types from export data, as
  `foldImportedInterfaces` already does for interfaces, so `app`'s load pairs
  them exactly. Consequence: complete cross-module implements in monorepos, a
  larger cross product per load, and a per-file pass of `impl` still cannot
  see it (carried forward in process, like GM-590). Recommended: file to
  backlog, not in 4.12.0.

## Addendum (S6): D2 in GM-633

Owner decisions: **D1** "Да, только своя загрузка (Recommended)": a type is
paired only with interfaces of its own load. **D2** "Сделать в GM-633": the
`app`/`impl` case is fixed here, not deferred.

Line numbers below are at `b5c62b6` (= release-4.12.0 for `plugins/go`).
GM-631 (`5a561ad`) rewrites 1727-1858 and shifts every later line by +94
(`carryForward` 2026 -> ~2120, `unevaluated` 2171 -> ~2265).

### D2 design

**What is folded.** Each load already walks, from every project root package
it type-checked, the project packages reachable through `types.Package.Imports()`
(`foldImportedInterfaces`, 794): transitive, stopping at the first non-project
package. D2 reuses exactly that walk and, besides the interfaces, folds into
`namedTypes` every type name of a walked package that is

- exported (`typeName.Exported()`),
- not an alias and not generic (the same two rules as `foldPackage`, 762-773),
- has a non-empty pointer method set (`types.NewMethodSet(types.NewPointer(named)).Len() > 0`),
  interfaces included (an imported interface can satisfy another, as a
  source-folded one already can),
- not already in `namedTypes` from this load's source fold (an in-module
  import of a root package is the same `*types.TypeName`, so the map key
  dedups it anyway; the check only avoids re-tagging),
- has a position `positionOf` maps into the project (it records `declaredIn`
  as the interface branch already does, 821-825).

*Why exported only.* Export data is guaranteed to hold every exported
declaration; an unexported type is in it only when the exported API reaches
it, which depends on the package's shape. Folding unexported ones would make
the answer depend on the export-data encoding. `unevaluated` (2171) already
relies on the same guarantee for interfaces. The cost is that a consumer gets no edge
for an unexported implementor it can only obtain through a constructor.

*Why non-empty method set.* A type with no methods satisfies no interface the
fold keeps (`NumMethods() > 0`), so dropping it changes no answer and keeps
the cross product small.

*What "the import closure" means.* It is the export-data reference graph, not
the source import graph: `go/types` documents that `Imports()` of a package
read from export data lists "the packages that provide package-level objects
referenced by pkg", more or fewer than its source imports. Both passes use
the same walk, so they agree on it. An implementor package that a consumer
imports only for its internal use, without exporting any of its types, is
not reached. That is a known limit and is not fixed here.

**Composition with D1.** An imported object is tagged with the **importing**
load (`loadOf[typeName] = dir` of the load that walked it), not with its own
module. The D1 pair test (`loadOf[subtype] == loadOf[iface]`) is unchanged,
so D2 only adds candidates inside a load and never pairs across loads. In
`app`'s load, `impl/s.S` (export data) and `lib/j.W` (export data) and
`app/c.Sink` (source) carry the same tag and are paired exactly
(`types.Implements` within one universe). The `impl` load's own source copy
of `S` is a distinct object with tag `impl`, and it never meets `app`'s
interfaces.

**Where edges are written.** As today, only from a type whose declaring file
is in `structural` (`declarationNode`, 1674). In a whole pass that is every
file. In a per-file pass of a consumer file, imported types have no node, so
they are skipped before the inner loop and cost nothing. A consumer's
per-file pass never writes or retracts rows of another file.

**Dedup across loads.** One pair `(S, W)` produced by several loads (`impl`'s
own load if it imports `lib`, plus every consumer module that imports both)
gives the same edge id: `edgeIDFor(subtypeNode, SUPERTYPE_OF, placeholder)`,
and the placeholder id depends only on the interface's address and scope
(`placeholder`, 1939). `semanticDiff.edge` (1979) keeps the first, and the
`supertypes` record is overwritten by an equal one. For determinism, sort the
candidates by `objectLess` then `loadOf` (1609), so equal-name objects of
different loads are visited in a fixed order. The union is intended: an edge
holds if any build that sees both declarations assigns one to the other. No
extra dedup code is needed. Measured duplicate emissions on otel: 1,600 of
10,711.

**Per-file agreement.** The whole pass and a per-file pass agree on D2 edges at
the level that the in-module GM-590 case agrees on today (a type and an
interface in two packages of one module that do not import each other):
carried in process, re-evaluated exactly whenever the pass holds both
declarations in one load. This needs one fix, and the fix is required by D1
as well:

*Bug (probe-confirmed, also on base).* `carryForward` asks
`resolved.unevaluated(J)` (2048), which is **pass-global**: if *any* load of
the pass folded `J`, the record is not carried, and because no load paired
it, it is retracted. A GM-620 re-check after an interface edit loads `J`'s
module and `S`'s module separately, so every cross-load edge is retracted
even when `S` still satisfies the new `J`:

| fixture (scratch `gm633-s6/go/zz_fixture_test.go`) | per-file `[j.go]` after a compatible edit (`W` gains `String()`, `S` has it) | next whole pass |
|---|---|---|
| D1 kept (`b` requires `a`, `b/u` imports `a/j`) | `b/s.S -> a/j#W` retracted (today, D1, D2 alike) | re-emitted |
| D2 (`app` imports `impl/s` and `lib/j`) | `S -> lib/j#W` and `S -> app/c#Sink` retracted | both re-emitted |

*Fix: per-load evaluation.* `resolutions` keeps, per load dir, the sets
`folded`, `importedInterfaces` and `importedPackages` (`loadView`). The
pass-global sets stay for `staleImplementorFiles` (2080). `carryForward`
treats a prior record `r` of file `F` as follows:

1. **Answered** if some load `L` has `r`'s subtype (package, name, declared
   in `F`) in `namedTypes` tagged `L` **and** `!unevaluatedIn(L, J)`. That
   load gave the exact answer: produced means kept, not produced means
   retracted (as today).
2. **Otherwise carried** when `S`'s current method names include `J`'s
   method names. These are the **current** `J`'s names if any load of this
   pass holds `J`'s declaration (preferring a source fold), else
   `r.ifaceMethods`. A carried record takes the current `J`'s `ifaceSig`,
   `ifaceMethods` and `ifaceScope`. Otherwise `staleImplementorFiles` would
   re-flag `F` on every later pass of `J`'s file.

Rule 2's "current names" is what retracts `S -> W` when `W` gains a method
`S` lacks (break edit). Probe: the whole pass after it keeps only
`S -> app/c#Sink`. Without it, the record's old names would keep the stale
edge.

*Limits that remain (same as the in-module GM-590 case, stated in the README):*

- A change that keeps method names but changes a signature (on `S` or `J`)
  leaves a carried edge until the next whole pass. In-module this is exact,
  because the re-check puts both files in one load. Across loads, matching
  by name is all a per-file pass can do without loading the consumer.
- A **new** pair is edged on the next whole pass. Examples: a consumer adds
  an import, or `S` gains the missing method. A consumer per-file pass sees
  the pair but cannot write `S`'s file, and `S`'s own pass does not load the
  consumer. This is GM-629's class of problem (see below).
- A **fresh plugin process** (crash respawn mid-session) has no records:
  the first per-file pass of `impl/s/s.go` drops its D2 edges until the next
  whole pass (every daemon start runs one). Probe `three-fresh-impl`: no
  rows. Same as in-module today.
- A consumer that drops its import keeps the D2 edge (carried) until the
  next whole pass.
- Version skew: if two consumer loads resolve a third-party type in `S`'s
  signatures to different versions, the whole pass takes the union, while a
  per-file pass answers from whichever load it has. This is theoretical, not
  seen in the probe.

*Rejected for GM-633: exact per-file via "pair loads".* For a record whose
producing load is not in the pass, the pass would add a types-only
`packages.Load` from that consumer's module dir, with roots `{S's package,
J's package}`. The record would carry its producing `loads`. *Benefit*:
signature-exact re-check. *Risk*: rooting `S` artificially answers "yes"
after the consumer dropped its import (needs a `go list -deps` closure check
too). It adds one more load per implementor edit (measured ~1.5-1.7 s each on
otel), and it changes uses/twins folding unless a separate types-only fold
mode is added. For 3 extra edges on otel (below), it does not pay. Left as
owner decision D3.

### Cost (probe on opentelemetry-go, 30 modules, no go.work, 1,297 files)

Scratch copy of `plugins/go` with D1 (`loadOf` tag + same-load `continue`) and
D2 (imported-type fold) behind `PROBE_MODE`. Whole pass, deps in the module
cache, `GOWORK=`/`GOFLAGS=` cleared. The machine was shared (load averages
6-27, recorded per run in `gm633-s6/out*/summary.txt`).

| mode | candidate types (imported) | interfaces | pairs tested | `hasAllMethods` hits | emitted (unique rows) | answer step | whole pass real / user / sys |
|---|---|---|---|---|---|---|---|
| today | 6,746 (0) | 2,511 | 16,939,206 | 280,337 | 182,890 (16,181) | 2.28 s | 56.6 / 37.1 / 24.3 (warm), 58.4 / 39.0 / 27.2 |
| D1 | 6,746 (0) | 2,511 | 728,618 | 13,430 | 9,111 (8,200) | 0.50 / 1.04 / 0.62 s | 56.3 / 36.2 / 26.1; 76.9 / 42.2 / 29.1; 62.3 / 38.8 / 27.5 |
| D1+D2 | 8,628 (1,882) | 2,511 | 927,772 | 15,214 | 10,711 (8,203) | 1.27 / 1.15 / 1.68 / 0.77 s | 78.2 / 38.0 / 26.0; 73.2 / 39.6 / 27.7; 73.7 / 43.1 / 30.7; 47.0 / 33.1 / 22.9 |

- Whole-pass wall time is set by the `go list` + type-check loads, and the
  machine load moved it from 44 s to 78 s, with user+sys equal within noise
  in every mode. D2's own cost is the answer step, +0.15 to +0.7 s over D1.
  D1+D2 still tests 18x fewer pairs than today.
- Per-file pass, D1+D2: `[trace/trace.go]` 1.50 s (27 imported types, 336
  pairs); `[bridge/opentracing/bridge.go]` 1.66 s (55 imported, 1,780 pairs).
  The answer step rounds to 0 ms. D2 adds nothing measurable per edit.
- **No bound is needed.** The D1+D2 product is a sum of per-load products and
  stays below today's single global product. If a repo ever shows otherwise
  (many modules, each importing a huge shared closure), the cheap bound is to
  index interfaces by their first method name and test only types that have
  that method. That bound gives the same answers in less time. Not
  proposed now.
- **What D1 removes on otel:** 7,981 of 16,181 rows (49%). 7,890 of them point
  at two **unexported, test-file** interfaces in `otel/metric`
  (`int64Config` / `float64Config` = `{Description() string; Unit() string}`,
  `syncint64_test.go:43`), from types of other modules (mostly `semconv/*`).
  That is the predeclared-signature accident at scale. The other 91 point at
  `experimentalOption` / `samplable` / `finishOption` copies. None is
  assignable in any build.
- **What D2 adds back on otel:** 3 rows, all real consumer-build facts.
  `metric/noop/noop.go:Meter -> sdk/metric/internal/finish#Factory`, and
  `trace/trace.go:SpanContext`, `TraceFlags -> bridge/opentracing#samplable`
  (an unexported consumer-defined interface). otel's modules mostly import the
  interfaces they implement, a pair D1 keeps. D2 matters for
  "accept interfaces" consumers and three-module layouts.

### Merged D1+D2 edit map (supersedes "Edit map (A)")

| site | file:line (b5c62b6) | change |
|---|---|---|
| `resolutions` | semantic.go:323 (fields 335-343, 375-391) | add `loadOf map[*types.TypeName]string` and `views map[string]*loadView` (`folded`, `importedInterfaces`, `importedPackages` per load dir); init in `newResolutions` (399-418) |
| `loadFor` | semantic.go:1262, call 1283 | pass `dir` (project-relative) to `load` |
| `load` | semantic.go:425, call 441 | take `dir`, hand it to `foldPackage` |
| `foldPackage` | semantic.go:718 | `views[dir].folded` next to 750; `loadOf[typeName] = dir` next to 774 (covers 781); pass `dir` to `foldImportedInterfaces` (751) |
| `foldImportedInterfaces` | semantic.go:786-831 | doc comment: types too, exported only, why. Loop 809-826: D2 type fold before the interface test (816-818) with `loadOf`/`declaredIn`; `loadOf` on interfaces at 819; per-load `importedPackages` (801) / `importedInterfaces` (820); `dir` in the recursion (827). `importsWalked` (797-799, keyed by `*types.Package`) is already per load |
| `answerImplements` | semantic.go:1593; sort 1609; pair loop 1631-1650 | `continue` when `loadOf[subtype.obj] != loadOf[ifaceObj]`, before `hasAllMethods`, with a comment (different loads: `types.Implements` matches only predeclared signatures, no build sees both); sort tie-break by `loadOf` |
| `carryForward` | semantic.go:2026-2074 (~2120 after GM-631) | per-record "answered by some load" (rule 1) replacing `resolved.unevaluated` at 2048; current-interface names and record refresh (rule 2) at 2061-2069; build a `(pkg,name) -> current iface` map once per pass |
| `unevaluated` | semantic.go:2165-2176 | becomes `unevaluatedIn(load, pkg, name)` on a `loadView`; same three-clause body |
| plugin README | plugins/go/README.md:149-152 | `SUPERTYPE_OF` pairs a type with interfaces of builds that see both. That is its own module, or a module that imports its package (exported types with methods). There are no edges between modules that share no build. A per-file pass carries cross-build edges by method name and re-checks them exactly on the next whole pass |
| test 10 | semantic_twins_modules_test.go:433-456 | compare every `SUPERTYPE_OF` from `t.go` with the whole pass; assert there is no `nested-fork` target |
| new tests | plugins/go/semantic_cross_module_test.go (new) | behaviours 5-10 below |

Read, not change: `staleImplementorFiles` (2080, keeps the pass-global
`folded`), `declarationNode` (1674, the emission gate), `semanticDiff.edge`
(1979, the dedup), `loadReferencedTwins` (497) / `listUncoveredCopies` (560)
(list only, no fold). Callers relied on: `unevaluated` has one non-test
caller (2048), and `folded` is read only by `staleImplementorFiles`
(2095, 2101) (grep in `plugins/go`, single file).

### Behaviours tests must pin (D2; 1-4 above stand)

Fixture `three`: `lib/j` (`W{To(io.Writer)}`), `impl/s` (`S` with `To`,
`String`), `app` requiring both via `replace`, `app/main.go` importing both,
`app/c` (`Sink{To(io.Writer)}`, imports neither).

5. **Consumer build pairs imported types (D2 fold).** Whole pass emits
   `impl/s/s.go:S -> lib/j#W`, `impl/s/s.go:S -> app/c#Sink` and
   `app/c/c.go:Sink -> lib/j#W`. Without `app` importing `impl/s`, there are no
   `S` rows. *Control A*: skip the imported-type fold. The `S` rows vanish.
6. **Exported only.** `impl/s` adds `type t struct{}` with `To`, plus
   `func New() t`, and `app` calls it. There is no `t` row in any pass. *Control
   B*: drop the `Exported()` test. A `t` row appears.
7. **Same process per-file equals whole.** Whole, then `[impl/s/s.go]`: the
   same two `S` rows (carried). The consumer per-file pass `[app/main.go]`
   emits and retracts nothing for `impl/s/s.go`. Covered by Control A (the
   whole pass lacks them); no extra control.
8. **Compatible interface edit keeps the cross-load edge (per-load
   evaluation).** Whole; edit `W` to `{To(io.Writer); String() string}`; then
   `[lib/j/j.go]`. `S -> W` and `S -> Sink` are kept, and the next whole pass
   gives the same rows. The same holds on the D1 fixture: `b/s.S -> a/j#W`
   is kept. *Control C*: restore the pass-global `unevaluated` in
   `carryForward`. Both are retracted (probe-confirmed).
9. **Breaking interface edit retracts by current names.** Edit `W` to
   `{To(io.Writer); Close() error}`. `[lib/j/j.go]` retracts `S -> W` and
   keeps `S -> Sink`, as the next whole pass does. *Control D*: carry
   against `r.ifaceMethods` instead of the current interface. `S -> W` is kept.
10. **Carried record is refreshed.** After 8, a second `[lib/j/j.go]` pass
    with no change re-checks nothing (`recheckedImplementors` empty).
    *Control E*: do not refresh `ifaceSig`. `impl/s/s.go` is re-checked
    again.

Controls total: 1/4 (shared), 2, 3 from D1, and A-E: **8**. They all land in
the `plugins/go` test binary, one `go test -run` per control.

### Must confirm (measure slice)

- Re-run the otel probe on the implementation, not the scratch prototype:
  rows 8,203; no row onto `otel/metric#int64Config`/`#float64Config` from
  another module; the 3 D2 rows present; answer step under 2 s. Record
  `uptime` and `time -p`, because wall time here is mostly machine load.
- Whole and per-file agree on otel for a sample of files that hold D2 rows
  (`trace/trace.go`, `metric/noop/noop.go`): whole, then per-file `[f]` in
  process gives the same `SUPERTYPE_OF` rows from `f`.
- No regression on the existing carry tests
  (`semantic_supertype_carry_test.go`, `semantic_iface_edit_test.go`),
  whose in-module answers must be unchanged by per-load evaluation.
- The g-mesh repo itself (`plugins/go` + conformance fixtures): the
  `SUPERTYPE_OF` row count before and after, every removed row explained as
  cross-load.

### Overlap update

- **GM-629** must use the per-load rule both for its candidate scan (same
  `loadOf` test) and for anything it adds to `carryForward`. Its "newly
  satisfied implementor" scan is also where the D2 new-pair limit would be
  closed, if ever (a consumer load that sees a new pair for an out-of-scope
  file).
- **GM-631**: no shared lines. Its `interfaceSignature` feeds the
  `ifaceSig` refresh in rule 2.

### Owner decision

- **D3 - how exact must a per-file pass be about cross-build edges?** Today
  (and after D1+D2 as designed): a per-file pass of `impl/s/s.go` keeps a
  cross-build edge `S -> lib/j#W` by method names and re-checks it exactly
  only on the next whole pass. A signature-only change (`To(io.Writer)` ->
  `To(io.Reader)`) leaves the edge until then. Change (if chosen): add a
  types-only load from the consumer module (`app`) on every such edit, so
  the check is exact. Example cost on otel: about +1.6 s per edit of a file
  holding such an edge, for 3 edges repo-wide. Consequence: exact per-file
  answers, a new load mode to maintain, and a closure check (otherwise a
  consumer that dropped its import still gets an edge). Recommended: no,
  keep name-carry (the same contract as the in-module GM-590 case).
