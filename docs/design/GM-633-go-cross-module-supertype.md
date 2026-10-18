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
