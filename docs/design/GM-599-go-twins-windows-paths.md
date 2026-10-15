# GM-599: a Go twin's `//line` file is file-scoped on Windows

Design note for GM-599 (release-4.8.0 CI, `x86_64-pc-windows-msvc`, step
"Go plugin tests"). No production code changes here.

Sources: [code] read in this worktree (`fix/GM-599-go-twins-windows-paths`,
dfc9cac, Go plugin code identical to release-4.8.0); [go] the Go standard
library's `go/scanner` (`scanner.go` 296-311 in the local toolchain);
[repro] a copy of `plugins/go` in a scratch dir with one fixture line
changed, run on macOS (below). No timing was measured.

## 0. Summary

- **It is not a `\` vs `/` path comparison.** Every path the twin code
  compares goes through `relPathOf` (semantic.go 794-809), which already
  uses `filepath.Rel` + `filepath.ToSlash`. The twin fixture's `//line
  /gm570-outside/outside.go:1` is the cause: on Windows that path has no
  drive letter, so `filepath.IsAbs` is false, and `go/scanner` joins it
  with the declaring file's directory. The declaration then sits in
  `<root>\lib\gm570-outside\outside.go`, inside the project.
- **The test exposed a real plugin gap, on every OS.** `targetScopeOf`
  (semantic.go 762-768) file-scopes a twinned answer whenever the declaring
  file ends in `.go`. Its comment and GM-570 (2.1 step 3, T6) say "a Go file
  of this project". A `//line` naming a `.go` path that the package does not
  build (relative, or rooted without a drive on Windows) gets
  `scope.file` = a file nothing declares, and core drops the edge. A
  relative `//line` reproduces the Windows output on macOS [repro].
- **Recommended fix:** file-scope a twinned answer only when its declaring
  file is one of the files the host build compiles for that package
  (`pkg.GoFiles`, project-relative). Otherwise keep the container scope,
  as T6 intends. Plus a fixture that tests "outside the root" with a path
  that is absolute on every OS.

## 1. What happens today

### 1.1 The failure

```
semantic_twins_test.go:266: semantic pass produced
  app/app.go:UseOutside CALLS file:lib/gm570-outside/outside.go#Outside.Walk
want
  app/app.go:UseOutside CALLS container:example.com/twins/lib#Outside.Walk
```

The other nine edges match. Passes on macOS and Linux.

### 1.2 How the `//line` path becomes a project file [go, code]

The fixture's host-built twin `lib/binding.go` ends with:

```go
//line /gm570-outside/outside.go:1
type Outside interface {
	Walk() string
}
```

`go/scanner.updateLineInfo` (via `go/parser`, which `packages.Load` uses):

```go
filename = filepath.Clean(filename)
if !filepath.IsAbs(filename) {
	filename = filepath.Join(s.dir, filename)   // s.dir = dir of binding.go
}
```

- macOS/Linux: `/gm570-outside/outside.go` is absolute. `fset.Position`
  gives `/gm570-outside/outside.go`; `relPathOf` gets `../../..` and refuses
  it; `recordDeclaration` (semantic.go 619-630) records no `declaredIn`;
  `targetScopeOf` sees `""`, keeps the container. The test passes.
- Windows: `\gm570-outside\outside.go` is rooted but not absolute (no
  volume). The scanner rewrites it to
  `C:\...\lib\gm570-outside\outside.go`. `relPathOf` returns
  `lib/gm570-outside/outside.go`, `declaredIn[Outside.Walk]` gets it, and
  `targetScopeOf`'s only check is `strings.HasSuffix(file, ".go")`. The
  answer is file-scoped to a file that does not exist.

### 1.3 Local reproduction [repro]

Copy of `plugins/go`, fixture line changed to the relative `//line
gm570-outside/outside.go:1` (what Windows sees), then
`go test -run 'TestSemanticPassAddressesBuildTagTwinsByTheHostBuiltFile$'`
on macOS:

```
--- FAIL: TestSemanticPassAddressesBuildTagTwinsByTheHostBuiltFile
  app/app.go:UseOutside CALLS file:lib/gm570-outside/outside.go#Outside.Walk
want
  app/app.go:UseOutside CALLS container:example.com/twins/lib#Outside.Walk
```

Byte-for-byte the Windows result. So the gap needs no Windows: any
generated Go file whose `//line` names a relative `.go` path (yacc, cgo
output, templates rendered to `.go`) hits it on every OS.

### 1.4 Why the `.go` suffix check is not enough [code]

`declaredIn` follows `//line` on purpose (GM-570 2.1 step 1). So it can name:

| declaring file after `//line` | today | intended (T6) |
|---|---|---|
| `lib/binding.go` (no directive) | file | file |
| `lib/legacy.tmpl` | container (suffix) | container |
| absolute, outside the root | container (`relPathOf` refuses) | container |
| `lib/x/phantom.go`, a path inside the root that the package does not build | **file** | container |
| a rooted path without a drive, on Windows | **file** (it becomes the row above) | container |

## 2. The design

### 2.1 Plugin: file-scope only a file the host build compiles

1. **Record the built files per twinned package.** Where `twinnedNames` is
   computed (`collectTwins` and `loadReferencedTwins`), also store the
   package's `pkg.GoFiles` mapped through `e.relPathOf` (files outside the
   root are skipped) in a new `resolutions.builtFiles map[string]map[string]bool`
   (package path to project-relative files). Only packages with twinned
   names need it. Across module views it is a union, like `twinned`.
2. **Check it in `targetScopeOf`.** Replace the suffix test with
   `r.builtFiles[container][file]`. A `.tmpl` file, a phantom `.go` path
   and a path outside the root all fail it, and the answer keeps
   `Scope{Container}`. The `.go` suffix test goes: membership in `GoFiles`
   implies it.

No wire, core or SDK change. Ids do not change for any answer that is
file-scoped today correctly (its file is in `GoFiles`).

### 2.2 Fixture: an outside path that is absolute on every OS

`/gm570-outside/outside.go` is absolute only on Unix. Two fixture cases
replace it:

- **Outside the root**: `//line {{OUTSIDE}}:1`, where a new
  `twinFixture(t)` helper replaces `{{OUTSIDE}}` with
  `filepath.ToSlash(filepath.Join(t.TempDir(), "outside-project-root", "outside.go"))`.
  A second `t.TempDir()` call is a sibling of the root, so `relPathOf`
  refuses it on every OS. On Windows the path is `C:/.../outside.go`:
  `go/scanner` cuts `:line` at the last colon, so the drive colon is kept.
- **Inside the root, not built** (new, platform-independent):
  `//line not-in-build/phantom.go:1` before a third twinned interface
  (`Phantom { Probe() string }` in both twins, `UsePhantom` in `app.go`).
  It resolves to `lib/not-in-build/phantom.go` on every OS.

The three `writeTree(t, twinFiles)` sites (semantic_twins_test.go 211, 329,
360) call `twinFixture(t)` instead, so no test writes an unreplaced
placeholder.

## 3. Options considered

| option | what | Windows | phantom `.go` on every OS | risk |
|---|---|---|---|---|
| **A. built-file check (Recommended)** | 2.1 + 2.2 | fixed | fixed | one more map on `resolutions` |
| B. fixture only | make the outside path absolute per OS, leave `targetScopeOf` | fixed | still file-scoped, edge dropped | hides a real gap; the test stops covering it |
| C. `os.Stat` the declaring file | file-scope if it exists under the root | fixed | fixed for missing files | a real `.go` file of another package, or excluded by build tags, still passes; a syscall per answer |
| D. unadjusted position (`PositionFor(pos, false)`) for `declaredIn` | address by the physical file (`lib/binding.go`) | fixed | fixed, and links instead of falling back | changes GM-570 T6 and `declaredIn`'s other users (SUPERTYPE_OF source nodes, placeholder ids); a design change, not a CI fix |

## 4. Owner decisions

### D1. How is a twinned answer's declaring file judged "known"?

- **Today:** a twinned answer is file-scoped when the file `//line` names
  ends in `.go`. On Windows the twin fixture's `//line
  /gm570-outside/outside.go` turns into `lib/gm570-outside/outside.go`, a
  file that does not exist, and CI fails. On any OS a generated file with
  `//line parser.go.y.go:1` style directives naming a non-built `.go` path
  does the same in a real project.
- **Change:** file-scope only a file the host build compiles for that
  package (`pkg.GoFiles`).
- **Example:** `UseOutside` calls `Outside.Walk`, declared after `//line
  /gm570-outside/outside.go:1`. Today on Windows: `file:lib/gm570-outside/outside.go`,
  core finds nothing there, `find_callers Outside.Walk` is empty. After:
  `container:example.com/twins/lib`, ambiguous as before GM-570, as T6 says.
- **Consequence:** a twinned answer is file-scoped only when core can
  resolve it there; everything else keeps the pre-GM-570 address.
- **Options:**
  - **A: built-file check + per-OS absolute fixture (Recommended).**
    Benefit: fixes CI and the same gap on macOS/Linux; matches GM-570's
    wording. Risk: `builtFiles` is filled from whichever package variant
    `collectTwins` sees first (`twinsKnown`); a twinned name declared in a
    loaded `_test.go` can fall back to the container if the non-test
    variant came first. That is the pre-GM-570 behaviour, not a wrong edge.
  - **B: fixture only.** Benefit: smallest diff. Risk: the plugin still
    emits file scopes to files that do not exist on every OS.
  - **D: address by the physical file.** Benefit: such twins link instead
    of falling back. Risk: reverses GM-570 T6 and touches `declaredIn`'s
    other users; better as its own task if wanted.

### D2. Fixture directory name (AC4)

- **Today:** `gm570-outside` names the task, not the behaviour, and on
  Windows it is not even outside.
- **Change:** `outside-project-root` for the absolute sibling-temp path,
  `not-in-build` for the new in-root phantom (`lib/not-in-build/phantom.go`).
- **Consequence:** the rendered edge in a failure message says which
  behaviour broke.

## 5. Edit map (D1 A, D2 as proposed)

| file | lines | change |
|---|---|---|
| plugins/go/semantic.go | 310-345 `resolutions` | add `builtFiles map[string]map[string]bool` with a comment (package path to project-relative `GoFiles`, union across module views) |
| plugins/go/semantic.go | 347-358 `newResolutions` | initialise `builtFiles` |
| plugins/go/semantic.go | 421-470 `loadReferencedTwins` | where `names` is non-empty (454-463), also add `relPathOf(f)` of each `pkg.GoFiles` to `builtFiles[pkg.PkgPath]` |
| plugins/go/semantic.go | 634-642 `collectTwins` | same, when `names` is non-empty |
| plugins/go/semantic.go | 752-768 `targetScopeOf` | `r.builtFiles[container][file]` replaces `strings.HasSuffix(file, ".go")`; comment: "a file the host build compiles for that package" |
| plugins/go/semantic_twins_test.go | 51-77 `twinFiles` | `{{OUTSIDE}}` placeholder; `//line not-in-build/phantom.go:1` + `Phantom` in both twins; header comment 11-17 lists both |
| plugins/go/semantic_twins_test.go | 113-147 `app/app.go` | `func UsePhantom(p lib.Phantom) string { return p.Probe() }` |
| plugins/go/semantic_twins_test.go | 208-218, 329, 360 | new `twinFixture(t) string` (replaces the placeholder, calls `writeTree`); the three sites call it |
| plugins/go/semantic_twins_test.go | 239-275 | want `UsePhantom CALLS container:...lib#Phantom.Probe`; T6 comment names both directories |
| plugins/go/semantic_twins_test.go | new | `TestTargetScopeOfKeepsTheContainerForAFileTheHostDoesNotBuild` (2.3) |

Callers of `targetScopeOf` (all keep their signature): semantic.go 1089 and
1122 (`answerFile`), 1320 (`answerImplements`) [code; `grep -n` on the one
known symbol in the one file that declares and uses it].

### 2.3 The unit test

No `go list`, no processes:

```go
func TestTargetScopeOfKeepsTheContainerForAFileTheHostDoesNotBuild(t *testing.T) {
	r := newResolutions()
	const pkg = "example.com/twins/lib"
	r.twinned[pkg] = map[string]bool{"Outside": true}
	r.builtFiles[pkg] = map[string]bool{"lib/binding.go": true}
	obj := types.NewTypeName(token.NoPos, nil, "Outside", nil)
	key := []pathSegment{ /* "Outside" */ }
	for file, want := range map[string]targetScope{
		"lib/binding.go":                {File: "lib/binding.go"},
		"lib/not-in-build/phantom.go":   {Container: pkg},
		"lib/legacy.tmpl":               {Container: pkg},
	} {
		r.declaredIn[obj] = file
		if got := r.targetScopeOf(obj, pkg, key); got != want { ... }
	}
}
```

## 6. Behaviours the tests must pin, with candidate controls

| # | behaviour | test | control (revert code, the test must fail) |
|---|---|---|---|
| W1 | a twinned answer whose `//line` names an in-root `.go` path the package does not build keeps the container | `...ByTheHostBuiltFile` (`UsePhantom`) and the unit test | restore `strings.HasSuffix(file, ".go")` in `targetScopeOf`: fails on macOS, Linux and Windows |
| W2 | a twinned answer declared outside the root keeps the container on every OS | `...ByTheHostBuiltFile` (`UseOutside`) | same revert, but fails only on Windows CI (on Unix `relPathOf` refuses the path before `targetScopeOf`); W1's control covers it locally |
| W3 | a host-built twin is still file-scoped (no over-correction) | `...ByTheHostBuiltFile` (`UseBinder`, `Kelvin`, `Binder`) and the unit test | do not fill `builtFiles` in `collectTwins`: twins fall back to the container |
| W4 | a per-file pass, whose twinned package comes from `loadReferencedTwins`, is still file-scoped | `TestSemanticPassPerFileKeepsTwinsOfAnImportedPackage` | do not fill `builtFiles` in `loadReferencedTwins` |

Three controls (W1/W2 share one), all in the Go plugin's test binary (one
`go test` build). Repeats: the fixture tests shell out to `go list`, so
they run 5x per the standing rule; the unit test has no process and needs
none. The Windows half of W2 is observed only in CI: the verify slice
dispatches the CI workflow on the branch (`gh workflow run`) and reads the
"Go plugin tests" step on `x86_64-pc-windows-msvc`.

## 7. Measure slice: not needed

No index-wide effect beyond generated files with relative `//line` .go
names, which none of the bench corpora (gin) has: GM-570's 114 answers all
declare in `binding/binding.go`, a built file, so their scope and ids are
unchanged.
