# GM-621: Go twins across module copies, in every pass kind

Design slice GM-621/S5. Base: `release-4.11.0` (worktree `g-mesh-wt-gm621`,
branch `fix/GM-621-go-collect-twins-union`). Line numbers are for that base;
where GM-620 (`fix/GM-620-go-interface-edit-implementors` @ 3b9a0dc, not yet
merged) shifts them, the GM-620-merged number is given as `[620: n]`.

Inputs: the S1 diagnosis (reproduced by a probe, not by reading) and
`docs/design/GM-592-go-twin-union-and-failed-module.md` (its "related
observation", option (b), is part 1 here).

Owner decision: fix both parts now. (1) The whole-project pass unions twins
across module copies. (2) A per-file pass over any scope, a single file
included, marks the same twins as the whole pass.

## Background, in one paragraph

A *twinned* name is declared in a package both in a file the host build
compiles and in a file its build tags exclude (`twinnedNames`). An answer
onto a twinned name is sent with a file address (`file:nested/lib/b.go#X`)
instead of the package address (`example.com/root/nested/lib#X`), because the
package address cannot tell the two declarations apart
(`resolutions.targetScopeOf`, semantic.go:870). Two inputs decide it:
`twinned[path]` (is the name twinned?) and `builtFiles[path]` (is the
declaring file one the host builds?). Both are filled per pass, from
`collectTwins` (packages a load type-checks from source) and
`loadReferencedTwins` (packages an answer reached that no load type-checked;
listed with `go list`, names and files only).

## Today (F1), with the fixture

`forkedRoot()` (semantic_twins_modules_test.go:275): one import path,
`example.com/root/nested/lib`, exists twice in the project.

```
go.mod                  module example.com/root; replace example.com/root/nested => ./nested-fork
app/app.go              Use() calls lib.X(), lib.Y()        (resolves lib to nested-fork/lib)
nested/go.mod           module example.com/root/nested
nested/lib/b.go         !tagx: X, Y     nested/lib/b_x.go       tagx: Y   -> nested's copy twins {Y}
nested/use/u.go         U() calls lib.X(), lib.Y()          (resolves lib to nested/lib)
nested-fork/go.mod      module example.com/root/nested
nested-fork/lib/b.go    !tagx: X, Y     nested-fork/lib/b_x.go  tagx: X   -> fork's copy twins {X}
```

Measured by S1 (file-scoped answers; everything else is package-scoped):

| Pass | app→X | app→Y | U→X | U→Y |
|---|---|---|---|---|
| whole project | pkg | pkg | pkg | **file** |
| per-file `[app.go, u.go]` | file | file | file | file |
| per-file `[app.go]` | file | pkg | — | — |
| per-file `[u.go]` | — | — | pkg | file |

Two separate causes:

1. **Whole pass: first copy wins.** Modules load in order `""`, `nested`,
   `nested-fork`. `nested`'s load folds `nested/lib` first, so
   `collectTwins` records `{Y}` and nested's built files. When `nested-fork`
   folds the same path, `collectTwins` returns at `twinsKnown[path]`
   (semantic.go:728): `{X}` and the fork's built files are dropped.
2. **Per-file pass: only the copies the scope's own modules reach.** A
   per-file pass lists a package from each module that referenced it. `u.go`
   alone lives in `nested`, which reaches only nested's copy (`{Y}`); `app.go`
   alone lives in root, which reaches only the fork (`{X}`). The other copy
   is never looked at. A third, smaller hole of the same kind: a per-file
   pass over a file *inside* `nested/lib` folds nested's copy itself, which
   sets `twinsKnown`, so nothing lists the fork's copy either (by reading).

Consequence: the same edge flips between file and package address depending
on which pass ran last, and a file-scoped answer replaced by an ambiguous
package-scoped one is a regression core treats as settled.

## Target semantics

**The twins of a package path are the union, over every copy of that path in
the project, of the names that copy twins under the host build; the built
files are the union of every copy's built files.** A "copy" is a directory
`m.dir/<suffix>` of a module `m` whose module path is a prefix of the package
path and which is the innermost module of that directory (the same rule as
`moduleDirFor`). In the fixture, `example.com/root/nested/lib` has two copies:
`nested/lib` and `nested-fork/lib` (root's candidate `nested/lib` belongs to
module `nested`, so it is not a third).

This is exactly what an exhaustive whole pass computes once part 1 is in: it
folds every module's `./...`, i.e. every copy, from source. Part 2 makes a
per-file pass reach every copy of each path its answers depend on. After both
parts every row of the table above is "file" for all four answers that a
pass's scope contains.

Why the union and not "the copy the referencing load sees": GM-592 settled
this. The union only decides file vs package address; the file itself comes
from `declaredIn[obj]` (the referencing load's own type information) and must
be in `builtFiles`, so a file address always names a file that load compiled.
The package address is ambiguous anyway once two copies declare the name.

## Part 1: the whole pass unions copies (no option to choose)

S1's edit map, unchanged in substance.

- **Today:** `collectTwins` remembers "done" per package path.
- **Change:** remember "done" per *copy* (package path + its directory); for a
  new copy, add its twinned names to `twinned[path]` and its built files to
  `builtFiles[path]` instead of overwriting.
- **Example:** whole pass over `forkedRoot`: nested folds `{Y}`, the fork
  then adds `{X}` and `nested-fork/lib/b.go`: all four answers file-scoped.
- **Consequence:** test variants `p` and `p [p.test]` share path and
  directory, so a package is still parsed once per copy. `twinsKnown` stays
  keyed by path, because `loadReferencedTwins` reads it as "this path was
  already folded or listed" and must not list a path the pass folded.

## Part 2: how a per-file pass learns the other copies' twins

### Options

**A. Reuse what the last whole pass recorded (engine state, like GM-590's
`supertypes`).** Keep, per copy, the twins and built files the last whole pass
folded; a per-file pass unions them in.
- Cost per pass: none (map lookups).
- Correctness: stale by design. Adding `func Y()` to `nested-fork/lib/b_x.go`
  changes the fork's twins; a per-file pass over `nested/use/u.go` afterwards
  keeps using the old set until the next whole pass. After a plugin restart
  there is no record until a whole pass runs. Keeping it fresh would need
  every per-file pass that touches any copy's file (including a build-excluded
  file) to refresh the record: new bookkeeping with its own failure modes.
- Verdict: violates "same twins as the whole pass" whenever a copy changed
  since the last whole pass.

**B. List the missing copies in every per-file pass (recommended).** After
today's listings, for each package path the pass depends on, find its copies
from the workspace layout and `go list` (names and files only, no type check)
every copy no fold or listing of this pass has covered yet, from that copy's
own module directory.
- Cost per pass: in a project where no package path has two copies (the
  normal case) zero extra processes; the copy check is a loop over
  `ws.modules` per path, in memory. In a fork project: one `go list` per
  module holding an uncovered copy (all its uncovered paths in one call),
  plus one `go env` (`modFlag`) for a module this pass did not load. Measured
  on this machine: one such `go list` = 0.27 s real, user 0.01 s, sys 0.01 s,
  at load average 17.4 (i.e. mostly waiting on the go command's startup, not
  CPU). The fixture's `[u.go]` pass gains one listing (`nested-fork`), the
  `[app.go]` pass one (`nested`).
- Correctness: reads the copy from disk every pass, the same files and the
  same build flags the whole pass folds them with; no state across passes,
  so nothing goes stale.
- Risk: a copy whose module fails to list (broken go.mod) leaves its names
  out, i.e. package-scoped answers, same as a failed listing today; mitigated
  by skipping modules that already failed in this pass. GM-620's gated second
  `loadFor` repeats the listing (its `resolutions` is fresh), so a pass that
  triggers GM-620's re-check pays it twice: a rare pass in a rare project.

**C. A twins table refreshed on go.mod/replace edits.** Which copies a path
has changes only on go.mod edits, and that table already exists: `workspace`
(rebuilt on `workspaceChanged`/`resolutionChanged`, workspace.go:55). The
twin *names* change on ordinary `.go` edits, so a table refreshed only on
go.mod edits is stale exactly like A; refreshing it on `.go` edits means
re-listing copies, i.e. B with extra state.
- Verdict: C reduces to "B, reading copies from `workspace`", which is what B
  does.

**D. Type-check the other copies (full `loadMode` load).** Same answer as B,
several times the cost (type checking); rejected.

### Chosen: B

- **Today:** a per-file pass lists a package only from the modules that
  referenced it, so it sees only the copies those modules resolve to.
- **Change:** after those listings, list every other copy of each path the
  pass depends on, from the copy's own module, unless this pass already
  folded or listed that copy.
- **Example:** per-file pass over `nested/use/u.go`: nested lists its own
  copy (`{Y}`); the new step finds the copy `nested-fork/lib` uncovered and
  lists it from `nested-fork/` (`{X}`); U→X and U→Y are file-scoped, as in
  the whole pass. Per-file `[app.go]`: root lists the fork (`{X}`), the new
  step lists `nested/lib` (`{Y}`) and skips `nested-fork/lib` (root's listing
  already covered that directory): app→X and app→Y file-scoped.
- **Consequence:** in a project without duplicate module paths behaviour and
  process count are unchanged (every existing listing-count test keeps its
  number); in a fork project a per-file pass costs at most one extra
  `go list` per uncovered module copy.

Which paths the pass "depends on": every project path an answer resolved into
(today's `referenced`, accumulated in `listings`) **and** every path the pass
folded (`into.folded`): `answerImplements` addresses a folded package's
interface through `targetScopeOf` even if no call answer referenced it (for
example a per-file pass over `nested/lib/b.go` producing
`impl SUPERTYPE_OF ...#Binder`). This also closes the third hole above (a file
inside `nested/lib`).

Exclusions, each with its reason:
- An external test package path (`…/lib_test`) is not listed: `go list`
  cannot list it by import path, and an answer only reaches it from its own
  files, which the pass folded. Residual: the other copy's `lib_test` twins
  are not added in a per-file pass (a whole pass folds them). Accepted: a
  twin between two forks' external tests is not a case worth a load.
- A copy directory with no `.go` file is skipped (one `os.ReadDir`, only for
  copies not yet covered), so a module path prefix with no such package
  costs nothing.
- A module that failed to load in this pass is skipped (its listing would
  fail too).
- An exhaustive whole pass runs none of this (it is inside
  `loadReferencedTwins`, guarded by `!into.exhaustive`): it folded every copy.
  A whole pass with a failed module runs it, but every loaded module folded
  its copies and every failed one is skipped, so it adds no listing either.

## Edit map

All in `plugins/go`. No change to `run`, `foldPackage`, `targetScopeOf`,
`answerFile`, `answerImplements`.

1. **`resolutions` struct** (semantic.go:317-384 [620: 323-390]) and
   **`newResolutions`** (386-402 [620: 392-408]): add
   `twinCopies map[string]bool`, keyed `path + "\x00" + project-relative dir`
   (a dir outside the project: the absolute `pkg.Dir`), "this copy's twins are
   already unioned in". Reword the `twinned`/`builtFiles` comments to "unioned
   across the copies of a path"; keep `twinsKnown`'s meaning ("path folded or
   listed") and say so.
2. **New helper `twinCopyKey(into, pkg) string`** (or a method on the
   engine, next to `collectTwins`): `pkg.Dir`, falling back to the directory
   of the first `GoFiles`/`IgnoredFiles` entry; `e.relPathOf` +
   `normalizeDir` so `/var` and `/private/var` spell one key.
3. **New helper `unionTwins(into, pkg)`**: `twinnedNames` → merge into
   `twinned[pkg.PkgPath]` → `recordBuiltFiles`; marks `twinCopies[key]`. Body
   is today's loop at semantic.go:499-510 [620: 505-516].
4. **`collectTwins`** (725-737 [620: 731-743]): guard on
   `twinCopies[key]` instead of `twinsKnown[path]`; always set
   `twinsKnown[path] = true`; call `unionTwins`. Doc: "once per copy, unioned".
   **(part 1)**
5. **`loadReferencedTwins`** (451-519 [620: 457-525]): signature gains what
   step 7 needs (`ws`/`e.ws` is already on the engine; add
   `failed map[string]error` and `folded`, or read `into.folded`). Today's
   per-listing loop calls `unionTwins` (so listed copies are marked). Then the
   new step: collect the dependent paths (union of `listings[*].paths` and
   `into.folded`, minus `_test` paths); for each, `e.ws.packageCopies(path)`;
   drop copies in `twinCopies`, copies whose module is in `failed`, and
   copies with no `.go` file; group the rest by module dir; for each module
   (sorted) run one `packages.Load` with `NeedName|NeedFiles` and that
   module's configuration (step 6) over its paths; `unionTwins` each result
   whose `pkg.Dir` is the expected copy (a path that resolves elsewhere from
   that module is ignored). Failure: log like today, continue. Update the doc
   comment ("from every copy of the path, not only the referencing modules'").
   **(part 2)**
6. **`load`** (405-432 [620: 411-438]): factor the `packages.Config`
   construction (`Dir`, `loadEnv()`, `modFlag`) into `loadConfig(dir, mode)`
   so step 5 lists a copy with exactly the flags that module's whole-pass
   fold uses. Reuse a `cfg` from `listings` when that module loaded this pass
   (saves its `go env`).
7. **`loadFor`** (1068-1121 [620: 1098-1151]): pass `outcome.failed` to
   `loadReferencedTwins` (line 1115 [620: 1145]); comment above it (1107-1112)
   gains one sentence on copies.
8. **workspace.go, new method `(*workspace).packageCopies(importPath string)
   []moduleCopy`** (`moduleCopy{module, dir string}`), next to
   `isProjectImportPath` (256): for each module whose path is the import path
   or a `/`-prefix of it, `dir = join(module.dir, suffix)`, kept when
   `moduleDirFor(w, dir) == module.dir`. Pure; no filesystem access.

### Overlap with GM-620

No shared hunk. GM-620 edits `semanticEngine` (adds `recheckedImplementors`
after `supertypes`, +6 lines), `run` (gated second `loadFor`),
`answerImplements` (`ifaceSig`), `supertypeRecord`, and adds functions after
`carryForward`. This design edits `resolutions`/`newResolutions`, `load`,
`loadReferencedTwins`, `collectTwins`, `loadFor` and workspace.go. Merging
GM-620 first only shifts lines (+6 from the engine struct; +30 for `loadFor`),
as the `[620: n]` numbers show. Behavioural interplay: GM-620's second
`loadFor` builds a fresh `resolutions`, so it runs both parts again and gets
the same twins; its `staleImplementorFiles` reads `folded`/`interfaces`, which
this design does not change. Implement after GM-620 merges or rebase on it;
either order merges cleanly.

## Behaviours to pin, with controls

New tests in `semantic_twins_modules_test.go` (one test binary, `plugins/go`).
New fixture `forkedRootWithLibCaller()` = `forkedRoot()` plus
`nested/lib/c.go` (`package lib; func C() string { return X() + Y() }`, no
build tag), so a file inside a copy can be the scope; existing tests keep
`forkedRoot()`.

| # | Behaviour | Test | Control (revert in code; must fail) |
|---|---|---|---|
| 1 | Whole pass unions the copies' twins: app→X, app→Y, U→X, U→Y all file-scoped, 0 listings | `TestSemanticWholePassUnionsForkedModuleCopies` (`forkedRoot`, scope nil) | C1: `collectTwins` guard back to `twinsKnown[path]` (only U→Y remains, as S1 measured) |
| 2 | A later copy's built files are recorded | same test, edge `app/app.go:Use CALLS file:nested-fork/lib/b.go#Y` | C2: in `collectTwins`, union names but skip `recordBuiltFiles` when `twinned[path]` already existed |
| 3 | A single-file pass answers as the whole pass, for each file: for every scope `[app/app.go]`, `[nested/use/u.go]`, `[nested/lib/c.go]`, its edges equal the whole pass's edges from that file | `TestSemanticSingleFilePassesAnswerAsTheWholePass` (`forkedRootWithLibCaller`) | C3: delete the new copy-listing step in `loadReferencedTwins` (u.go→X and app→Y back to package scope) |
| 4 | Folded paths count as dependent: `nested/lib/c.go:C CALLS file:nested/lib/b.go#X` in the `[nested/lib/c.go]` pass | same test, that scope | C4: build the dependent set from `listings` only, not `into.folded` |
| 5 | A copy already folded or listed is not listed again: `[app/app.go]` runs exactly 2 listings (`.` and `nested`), `[nested/use/u.go]` exactly 2 (`nested`, `nested-fork`) | `TestSemanticPerFilePassListsEachForkCopyOnce` (`forkedRoot`) | C5: drop the `twinCopies` check in the new step (3 listings; existing test 4 also fails) |
| 6 | A copy of a module that failed this pass is not listed: whole pass over `forkedRoot` with `nested/go.mod` = `brokenNestedGoMod` runs 0 listings | `TestSemanticWholePassSkipsAFailedModulesCopy` | C6: drop the `failed` check (one failing listing from `nested`) |

Six controls, one test binary. Dry-runs for the implementer: confirm
`pkg.Dir` is populated under `NeedName|NeedFiles` (x/tools v0.50.0 sets it in
golist.go:540) and that root still loads with `nested`'s go.mod broken in
`forkedRoot` (the replace points at the fork, so it should); if not, give
behaviour 6 a fixture where the broken module is not on root's path.

Must stay green, unchanged: tests 1-7 of `semantic_twins_modules_test.go`
(`TestSemanticWholePassOverModulesListsNoTwins` is the guard that an
exhaustive whole pass lists nothing; test 4's "2 listings" and test 5's "1
listing" are the guard that a project without forks pays no extra listing).
Optional: a pure test of `packageCopies` (root excluded for
`example.com/root/nested/lib`; both copies found); no control needed, it is
covered by C3.

## Open points (no decision needed unless the owner disagrees)

- External test packages (`_test` paths) are not completed in a per-file pass
  (reason above).
- A copy outside the project (module cache, `vendor/`, which is not indexed)
  is never listed by the new step; a referencing load that resolves there
  keeps today's behaviour.
- Found by verify (GM-621/S11): when a project vendors a module that is also
  one of its own modules, a referencing listing resolves into `vendor/` and
  leaves the module's own copy uncovered, so a per-file pass lists that copy
  too (1 listing becomes 2 on the vendored `requiringRoot` probe; edges
  unchanged). Kept: the whole pass folds that copy as well, so the union now
  matches it, and the cost (~0.27 s per such pass) arises only in this layout.
  The "no forks, zero extra listings" claim above holds for projects that do
  not vendor one of their own modules.
