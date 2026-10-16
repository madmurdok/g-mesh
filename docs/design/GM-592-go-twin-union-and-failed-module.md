# GM-592: Go twin union across module views, and twins of a failed module

Design slice GM-592/S1. Base: `release-4.10.0` @ 2d21a37 (includes GM-589).
Line numbers are for that commit; GM-590 (da12869, unmerged) shifts them, so
the implementer locates code by the function names below.

Source: GM-570/S12 verify, findings F1 and F2 (both low).

## Background

A *twinned* name is one a package declares both in a file the host build
compiles and in a file its build constraints exclude (`twinnedNames`). An
answer onto a twinned name is addressed by file (`file:<built file>#Name`)
instead of by container, because the container address is ambiguous
(`resolutions.targetScopeOf`, semantic.go:787).

Twins are computed in two places:

| Where | When | How it merges |
|---|---|---|
| `collectTwins` (semantic.go:644), called from `foldPackage` | every project package a load folds from source | first fold wins: `twinsKnown[path]` short-circuits, then `twinned[path] = names` |
| `loadReferencedTwins` (semantic.go:430), called from `loadFor` (semantic.go:1028) | per-file pass only: packages an answer referenced but no load folded (imports come from export data, without `IgnoredFiles`) | union across listings (one per load configuration) |

Call sites (g-mesh's Go semantic tier was `absent` in this worktree:
`find_callers loadFor` / `find_callers loadReferencedTwins` returned empty
results with `semanticTier: absent`, `find_references resolutions.twinned`
found no symbol; so grep):

- `loadFor`: called only from `run` (semantic.go:906).
- `loadReferencedTwins`: called only from `loadFor` (semantic.go:1029).
- `twinned` written at 467-471 (`loadReferencedTwins`) and 650
  (`collectTwins`); read at 788 (`targetScopeOf`); test fixture writes in
  semantic_twins_test.go:569.
- `builtFiles` written in `recordBuiltFiles` (658-663, always a union); read
  at 789; test fixture writes in semantic_twins_test.go:570, 604.

## F1: the union in `loadReferencedTwins` is unpinned

### Today

`loadReferencedTwins` adds each listing's twinned names to
`into.twinned[path]` (semantic.go:467-472). Its comment says "a name twinned
in any module's view is twinned". The GM-570/S12 verifier replaced the union
with `into.twinned[pkg.PkgPath] = names` (control e) and every test passed:
in the `requiringRoot` fixture, root and nested list `nested/lib` from the
same files, so both views give the same set and overwrite equals union.

To tell them apart, two module views of one package path must give
*different, non-empty* twin sets (an empty set is skipped by `continue`
before the write, so "one view has no twins" does not separate them either).

### Fixture: an in-repo fork through `replace`

Root requires `example.com/root/nested` but replaces it with a fork
directory; nested, loaded as its own module, sees its own files. One import
path, two file sets, two twin sets.

```
go.mod                      module example.com/root
                            require example.com/root/nested v0.0.0
                            replace example.com/root/nested => ./nested-fork
app/app.go                  package app; func Use() string { return lib.X() + lib.Y() }

nested/go.mod               module example.com/root/nested
nested/lib/b.go             //go:build !tagx   func X(); func Y()
nested/lib/b_x.go           //go:build tagx    func Y()          -> nested's view twins {Y}
nested/use/u.go             package use; func U() string { return lib.X() + lib.Y() }

nested-fork/go.mod          module example.com/root/nested
nested-fork/lib/b.go        //go:build !tagx   func X(); func Y()
nested-fork/lib/b_x.go      //go:build tagx    func X()          -> fork's view twins {X}
```

(`X`/`Y` return a string so the call expressions type-check.)

Per-file pass over `[app/app.go, nested/use/u.go]`: root's load folds only
`app`, nested's only `use`; `nested/lib` is referenced by both and folded by
neither, so `loadReferencedTwins` lists it twice: from the root directory
(resolves to `nested-fork/lib`, twins `{X}`) and from `nested/` (resolves to
`nested/lib`, twins `{Y}`). Listing order is `sortedKeys(missingBy)`; the
key starts with `cfg.Dir`, and `<root>\x00...` sorts before
`<root>/nested\x00...`, so root lists first.

With the union, `twinned[nested/lib] = {X, Y}` and `builtFiles` holds both
`b.go`s, so all four answers are file-scoped:

| Edge | Union | Overwrite (last = nested) | Overwrite if the order flipped |
|---|---|---|---|
| `app/app.go:Use CALLS file:nested-fork/lib/b.go#X` | yes | **container** | yes |
| `app/app.go:Use CALLS file:nested-fork/lib/b.go#Y` | yes | yes | **container** |
| `nested/use/u.go:U CALLS file:nested/lib/b.go#Y` | yes | yes | **container** |
| `nested/use/u.go:U CALLS file:nested/lib/b.go#X` | yes | **container** | yes |

Asserting all four makes the test fail under overwrite whichever listing
runs last, so it does not depend on the key order. Rows 1 and 3 pin "each
module's own twin survives the other's listing"; rows 2 and 4 pin the
documented "twinned in any module's view" (Y is not twinned in root's own
view, yet root's answer onto it is file-scoped).

Is "any view" the right semantics, rather than "twinned in the referencing
load's own view"? Yes (verifier agrees): the union only decides file vs
container; the file comes from `declaredIn[obj]`, the referencing load's
own type information, and must be in `builtFiles`. So a file-scoped answer
always names a file that load compiled; core's container address for `Y`
is ambiguous anyway, because the fork and nested both declare `Y` in
container `example.com/root/nested/lib`.

Alternative fixture considered: root vendors `nested` with a hand-edited
`vendor/` copy (`-mod=vendor` via `modFlagFor`). Rejected: `vendor/` is not
indexed (ignore.go:43, workspace.go:108), so root's answers would be
file-scoped onto `vendor/...` files core has no node for, which is a
separate question this task should not pin.

### Test and control

- Test (add to `plugins/go/semantic_twins_modules_test.go`, after test 5,
  line 257): `TestLoadReferencedTwinsUnionsModuleViews`. New fixture
  builder `forkedRoot()`; reuse `modulesPass(t, forkedRoot(),
  []string{"app/app.go", "nested/use/u.go"})`. Sanity first: exactly two
  listings, dirs `.` and `nested`, each listing `example.com/root/nested/lib`
  (proves the two views ran). Then assert the four edges above.
- Implementer dry-run before writing assertions: confirm the fork replace
  resolves from root under `-mod=readonly` with no go.sum (a directory
  replace needs none) and that `workspace` tolerates two go.mod files with
  the same module path (innermost-dir wins in `moduleDirFor`, so it should).
- Control F1-c1: in `loadReferencedTwins`, replace lines 467-472 with
  `into.twinned[pkg.PkgPath] = names`. Must fail (row 1 or 4 container-scoped).

No production change for F1.

## F2: twins of a module whose load failed

### Today

A whole-project pass relies on every module folding its own packages, so
`loadFor` skips `loadReferencedTwins` when `wholeProject`
(semantic.go:1024-1030). When a nested module fails to load and root loads,
root's answers onto the nested module's packages come from export data, no
load folds `nested/lib`, and nothing lists it: root's answers onto its twins
are **container-scoped**. A per-file pass over `app/app.go` lists
`nested/lib` from root and answers **file-scoped**.

Example (the `requiringRoot` fixture with `nested/go.mod` broken, e.g. the
`brokenGoMod` "not a directive" of semantic_modules_test.go:31, which `go`
rejects for the main module but parses laxly as a dependency's go.mod, so
root still loads):

| Pass | `app/app.go:Use CALLS ...Binder.Bind` |
|---|---|
| per-file `app/app.go` | `file:nested/lib/b.go#Binder.Bind` |
| whole project, nested broken | container `example.com/root/nested/lib#Binder.Bind` (ambiguous) |
| whole project, nested healthy | `file:nested/lib/b.go#Binder.Bind` |

So the edge flips between passes while nested is broken. Not worse than
before GM-570 (everything was container-scoped then), and the pass already
answers incomplete, but `app/app.go` is in the loaded module, so its answer
is *settled* (not unfinished), and the ambiguous container address replaces
the exact one the last per-file pass gave.

### Options

**A. Keep as is, document it.**
Benefit: no code change. Risk: the flip above; `app/app.go` gets the
ambiguous address on every whole pass while nested is broken, and core
treats it as settled.

**B. Call `loadReferencedTwins` unconditionally.**
In a whole pass with no failure every referenced project package was
folded, so `missingBy` is empty and no listing runs (test 1,
`TestSemanticWholePassOverModulesListsNoTwins`, still passes). Benefit:
one code path, smallest diff. Risk: the "whole pass lists nothing" property
becomes an emergent consequence of `twinsKnown` rather than stated; a
future referenced-but-unfolded package in a healthy whole pass would
silently add a `go list`.

**C. Run it unless the pass is exhaustive (recommended).**
`exhaustive = wholeProject && len(outcome.failed) == 0`, GM-590's own
definition, and guard the call with `!exhaustive`. Behaviour equals B in
every case today, but the condition says why: a non-exhaustive whole pass
is, for twins, like a per-file pass (some referenced packages were not
folded), exactly as GM-590 treats it for carried-forward SUPERTYPE_OF edges.
Benefit: aligns both passes; one meaning of "exhaustive" across the plugin.
Risk: one extra `go list` (NeedName|NeedFiles, no type check) per
configuration in a pass that already failed a module; if that listing also
fails it is logged and the answers stay container-scoped, i.e. today's
behaviour.

**D. Make a per-file pass also not list a failed module's packages.**
Rejected: makes both passes equally wrong and contradicts GM-570.

### Recommendation: C

Edit map (after GM-590 merges):

- `semanticEngine.loadFor` (semantic.go:985-1037): after the module loop,
  set `into.exhaustive = wholeProject && len(outcome.failed) == 0` and
  replace `if !wholeProject {` (1028) with `if !into.exhaustive {`; update
  the comment above it (1024-1027) to say a whole pass with a failed module
  lists like a per-file pass.
- `semanticEngine.run` (semantic.go:883-933): remove GM-590's
  `resolved.exhaustive = wholeProject && len(outcome.failed) == 0` (it is
  now set by `loadFor`, before `finish`/`carryForward` read it). If GM-590
  has not merged, add the `exhaustive` field per its definition instead and
  leave `run` alone.
- `loadReferencedTwins` doc comment (416-429): "in a per-file pass" becomes
  "in a per-file pass, or a whole pass with a failed module".
- `resolutions.exhaustive` field comment (GM-590): add "...and no
  referenced package's twins had to be listed".

### Test and control

- Test (semantic_twins_modules_test.go):
  `TestSemanticWholePassListsTwinsOfAFailedNestedModule`. Fixture:
  `requiringRoot()` with `nested/go.mod` = a broken go.mod that still
  declares `module example.com/root/nested`. `modulesPass` fatals on a
  non-empty reason, so give it an `allowIncomplete` variant (or split the
  reason check out). Assert: the reason names `nested`; exactly one twin
  listing, from `.`, listing `example.com/root/nested/lib`;
  `app/app.go:Use CALLS file:nested/lib/b.go#Binder.Bind` present.
  Dry-run first: confirm root loads with nested's go.mod broken (lax
  parse); if not, pick a breakage that fails nested as main module only
  (e.g. a `require` of an unavailable module no root package imports).
- Existing test 1 (`TestSemanticWholePassOverModulesListsNoTwins`) stays
  the guard that a healthy whole pass lists nothing.
- Control F2-c1: restore `if !wholeProject {` in `loadFor`. Must fail
  (edge container-scoped, no listing).
- Control F2-c2: guard with `if true {` (option B). Expected to **pass**
  every test; this is not a control, it documents that B and C are
  behaviourally equal. Do not run it as a control.

## Related observation (decision needed): whole-pass folds are first-wins

`collectTwins` takes the first fold of a package path and ignores later
ones (`twinsKnown`, semantic.go:645-650). In a whole pass over the F1
fork fixture, modules load in order `""`, `nested`, `nested-fork`; nested
folds `example.com/root/nested/lib` first (twins `{Y}`), the fork's fold is
skipped, so root's answer onto `X` is container-scoped in a whole pass but
file-scoped in a per-file pass: the F2 flip again, from a different cause.
(By reading; not run. It needs two modules with the same module path, which
is rare.)

Options: (a) out of scope, file a follow-up; benefit: GM-592 stays small;
risk: the flip stays for forked modules. (b) make `collectTwins` union too,
keying the re-parse guard by (path, package directory) instead of path;
benefit: one merge rule; risk: touches the hot fold path and `twinsKnown`'s
second role as `loadReferencedTwins`'s "already folded" set, which must stay
keyed by path. Recommended: (a); the F1 test is per-file and does not
depend on it.

## Behaviours to pin, with controls

| # | Behaviour | Test | Control (revert in code) |
|---|---|---|---|
| 1 | per-file listings union module views | `TestLoadReferencedTwinsUnionsModuleViews` | F1-c1: overwrite instead of union |
| 2 | a whole pass with a failed module lists unfolded referenced packages | `TestSemanticWholePassListsTwinsOfAFailedNestedModule` | F2-c1: restore `!wholeProject` guard |

Two controls; both in the same test binary (`plugins/go`), one build each.

## GM-590 interaction

- GM-590 defines `resolutions.exhaustive = wholeProject && no failed
  module` and sets it in `run` after `loadFor`; `carryForward` skips when
  exhaustive. F2 reuses the same definition for the twin listing, so a
  non-exhaustive whole pass behaves like a per-file pass in both features.
- Only change to GM-590's code: the assignment moves from `run` into
  `loadFor`, before `loadReferencedTwins`; `finish`/`carryForward` run after
  `loadFor`, so they see the same value. No other overlap: GM-590 touches
  `foldPackage` (imported interfaces), `answerImplements`, `finish`,
  `carryForward`, not `collectTwins`/`loadReferencedTwins`.
- Implement on the release branch after GM-590 merges.
