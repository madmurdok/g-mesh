package main

// Build-tag twins across modules. A per-file pass folds only the edited
// files' packages; loadReferencedTwins lists every other project package an
// answer resolved into, for its twins. An import path resolves only in a
// module that requires it, so the listing runs once per load configuration,
// from the module whose answers referenced the package, after every module
// loaded. A whole-project pass folds every package itself and lists nothing.
//
// Fixture: a root module and a nested module that holds the twins.
//
//	go.mod                 example.com/root (optionally requires nested via replace)
//	app/app.go             root's answers
//	nested/go.mod          example.com/root/nested
//	nested/lib/b.go        //go:build !tagx  the twin the host builds
//	nested/lib/b_x.go      //go:build tagx   the twin it excludes
//	nested/other/o.go      a plain package
//	nested/use/u.go        nested's answers

import (
	"os"
	"os/exec"
	"path/filepath"
	"sort"
	"strings"
	"testing"
)

const (
	nestedLib   = "example.com/root/nested/lib"
	nestedOther = "example.com/root/nested/other"
	// U's answer onto the twinned Binder.Bind, addressed by the built twin.
	nestedUseBind = "nested/use/u.go:U CALLS file:nested/lib/b.go#Binder.Bind"
)

const nestedTwin = `package lib

type Binder interface{ Bind() string }

func New() Binder { return impl{} }

type impl struct{}

func (impl) Bind() string { return "%s" }
`

func nestedTwinFiles() map[string]string {
	return map[string]string{
		"nested/go.mod":     "module example.com/root/nested\n\ngo 1.22\n",
		"nested/lib/b.go":   "//go:build !tagx\n\n" + strings.Replace(nestedTwin, "%s", "a", 1),
		"nested/lib/b_x.go": "//go:build tagx\n\n" + strings.Replace(nestedTwin, "%s", "b", 1),
		"nested/other/o.go": "package other\n\nfunc Name() string { return \"o\" }\n",
		"nested/use/u.go": "package use\n\nimport (\n\t\"example.com/root/nested/lib\"\n\t\"example.com/root/nested/other\"\n)\n\n" +
			"func U() string { return lib.New().Bind() + other.Name() }\n",
	}
}

// requiringRoot: root requires nested through a replace and app/app.go
// calls into both nested packages.
func requiringRoot() map[string]string {
	files := nestedTwinFiles()
	files["go.mod"] = "module example.com/root\n\ngo 1.22\n\nrequire example.com/root/nested v0.0.0\n\n" +
		"replace example.com/root/nested => ./nested\n"
	files["app/app.go"] = "package app\n\nimport (\n\t\"example.com/root/nested/lib\"\n\t\"example.com/root/nested/other\"\n)\n\n" +
		"func Use() string { return lib.New().Bind() + other.Name() }\n"
	return files
}

// unrelatedRoot: root neither requires nor imports nested, so nested's
// packages do not resolve from the root directory, and app/app.go's
// answers reference no project package outside its own.
func unrelatedRoot() map[string]string {
	files := nestedTwinFiles()
	files["go.mod"] = "module example.com/root\n\ngo 1.22\n"
	files["app/app.go"] = "package app\n\nfunc Use() string { return \"x\" }\n"
	return files
}

// twinListingRun is one NeedName|NeedFiles `go list` (go/packages passes
// -find=true for it), with the module directory it ran in relative to the
// fixture root and the patterns it listed.
type twinListingRun struct {
	dir      string
	patterns []string
	line     string
}

// modulesPass runs one semantic pass over files under a recording `go` and
// returns the rendered answers, every twin listing, and the whole log.
func modulesPass(t *testing.T, files map[string]string, scope []string) ([]string, []twinListingRun, []string) {
	t.Helper()
	got, listings, lines, reason := modulesPassAllowingIncomplete(t, files, scope)
	if reason != "" {
		t.Fatalf("pass over %v answered incomplete: %s", scope, reason)
	}
	return got, listings, lines
}

// modulesPassAllowingIncomplete is modulesPass that returns the pass's
// incomplete reason instead of failing on it.
func modulesPassAllowingIncomplete(t *testing.T, files map[string]string, scope []string) ([]string, []twinListingRun, []string, string) {
	t.Helper()
	requireGoToolchain(t)
	realGo, err := exec.LookPath("go")
	if err != nil {
		t.Skip("no go on PATH")
	}
	root := writeTree(t, files)
	log := filepath.Join(t.TempDir(), "go.log")
	binDir := t.TempDir()
	buildRecordingGo(t, binDir, realGo, log)
	t.Setenv("PATH", binDir+string(os.PathListSeparator)+os.Getenv("PATH"))
	t.Setenv("GOWORK", "")
	t.Setenv("GOFLAGS", "")

	diff, reason, _ := newPluginState(root).handleSemanticPass(scope)
	got, _ := renderTwinEdges(t, root, diff)

	realRoot, err := filepath.EvalSymlinks(root)
	if err != nil {
		t.Fatal(err)
	}
	lines := readLines(t, log)
	var listings []twinListingRun
	for _, line := range lines {
		fields := strings.Split(line, "|")
		if len(fields) != 5 || !strings.HasPrefix(fields[4], "DIR=") {
			t.Fatalf("sanity: unexpected recording line %q", line)
		}
		args := strings.Fields(fields[3])
		if len(args) == 0 || args[0] != "list" || !containsString(args, "-find=true") {
			continue
		}
		dir := strings.TrimPrefix(fields[4], "DIR=")
		if resolved, err := filepath.EvalSymlinks(dir); err == nil {
			dir = resolved
		}
		rel, err := filepath.Rel(realRoot, dir)
		if err != nil {
			t.Fatalf("sanity: listing dir %s outside the fixture %s", dir, realRoot)
		}
		var patterns []string
		for i, arg := range args {
			if arg == "--" {
				patterns = append(patterns, args[i+1:]...)
				break
			}
		}
		listings = append(listings, twinListingRun{dir: filepath.ToSlash(rel), patterns: patterns, line: line})
	}
	return got, listings, lines, reason
}

func edgesFrom(edges []string, prefix string) []string {
	var out []string
	for _, e := range edges {
		if strings.HasPrefix(e, prefix) {
			out = append(out, e)
		}
	}
	return out
}

func listingDump(listings []twinListingRun) string {
	var out []string
	for _, l := range listings {
		out = append(out, l.dir+": "+strings.Join(l.patterns, " ")+"   ("+l.line+")")
	}
	return strings.Join(out, "\n  ")
}

// 1. A whole-project pass folds every module's packages itself: no twin
// listing runs, and the nested module's twinned answers, from both modules
// and from the twin's own package, are file-scoped.
func TestSemanticWholePassOverModulesListsNoTwins(t *testing.T) {
	got, listings, lines := modulesPass(t, requiringRoot(), nil)

	if len(listings) != 0 {
		t.Errorf("a whole-project pass ran twin listing(s):\n  %s", listingDump(listings))
	}
	if !strings.Contains(strings.Join(lines, "\n"), "./...") {
		t.Fatalf("sanity: no ./... load was recorded; log:\n%s", strings.Join(lines, "\n"))
	}
	for _, want := range []string{
		"app/app.go:Use CALLS file:nested/lib/b.go#Binder.Bind",
		"nested/lib/b.go:impl SUPERTYPE_OF file:nested/lib/b.go#Binder",
		nestedUseBind,
	} {
		if !containsString(got, want) {
			t.Errorf("whole-project pass did not produce %s; got\n  %s", want, strings.Join(got, "\n  "))
		}
	}
}

// 2. A per-file pass on a root file folds only app; the nested twinned
// package it imports through the replace is listed from the root module,
// so its answer stays file-scoped.
func TestSemanticPerFilePassKeepsTwinsOfANestedModuleItRequires(t *testing.T) {
	got, listings, _ := modulesPass(t, requiringRoot(), []string{"app/app.go"})

	want := "app/app.go:Use CALLS file:nested/lib/b.go#Binder.Bind"
	if !containsString(got, want) {
		t.Fatalf("per-file pass of app/app.go did not produce %s; got\n  %s\nlistings:\n  %s",
			want, strings.Join(got, "\n  "), listingDump(listings))
	}
}

// 3. Which other files share a per-file pass does not change an answer: in
// [app/app.go, nested/use/u.go], where root does not require nested,
// nested/lib is listed from nested's own directory (from root's it does
// not resolve) and U's answers equal those of nested/use/u.go alone.
func TestSemanticMixedPerFilePassAnswersAsTheNestedFileAlone(t *testing.T) {
	alone, aloneListings, _ := modulesPass(t, unrelatedRoot(), []string{"nested/use/u.go"})
	mixed, mixedListings, _ := modulesPass(t, unrelatedRoot(), []string{"app/app.go", "nested/use/u.go"})

	if !containsString(alone, nestedUseBind) {
		t.Fatalf("sanity: nested/use/u.go alone did not produce %s; got\n  %s\nlistings:\n  %s",
			nestedUseBind, strings.Join(alone, "\n  "), listingDump(aloneListings))
	}
	aloneU, mixedU := edgesFrom(alone, "nested/use/u.go:"), edgesFrom(mixed, "nested/use/u.go:")
	if !equalStrings(mixedU, aloneU) {
		t.Fatalf("the mixed per-file pass answered U with\n  %s\nwant (nested/use/u.go alone)\n  %s\nmixed listings:\n  %s",
			strings.Join(mixedU, "\n  "), strings.Join(aloneU, "\n  "), listingDump(mixedListings))
	}
}

// 4. A package referenced by two modules is listed by each, from its own
// directory: what is missing is decided before any listing runs. Each
// configuration lists all its referenced packages in one `go list`.
func TestLoadReferencedTwinsListsOncePerReferencingModule(t *testing.T) {
	got, listings, _ := modulesPass(t, requiringRoot(), []string{"app/app.go", "nested/use/u.go"})

	byDir := map[string][]twinListingRun{}
	for _, l := range listings {
		byDir[l.dir] = append(byDir[l.dir], l)
	}
	for _, dir := range []string{".", "nested"} {
		runs := byDir[dir]
		if len(runs) != 1 {
			t.Errorf("module %s ran %d twin listing(s), want 1; listings:\n  %s", dir, len(runs), listingDump(listings))
			continue
		}
		patterns := runs[0].patterns
		if !containsString(patterns, nestedLib) || !containsString(patterns, nestedOther) {
			t.Errorf("module %s listed %v, want both %s and %s in one listing", dir, patterns, nestedLib, nestedOther)
		}
	}
	if len(listings) != 2 {
		t.Errorf("ran %d twin listings, want 2 (one per module):\n  %s", len(listings), listingDump(listings))
	}
	for _, want := range []string{"app/app.go:Use CALLS file:nested/lib/b.go#Binder.Bind", nestedUseBind} {
		if !containsString(got, want) {
			t.Errorf("did not produce %s; got\n  %s", want, strings.Join(got, "\n  "))
		}
	}
}

// 5. A module whose answers reference no other project package gets no
// listing: in the mixed pass only nested lists, from its own directory.
func TestLoadReferencedTwinsSkipsAModuleThatReferencesNothing(t *testing.T) {
	_, listings, _ := modulesPass(t, unrelatedRoot(), []string{"app/app.go", "nested/use/u.go"})

	if len(listings) != 1 || listings[0].dir != "nested" {
		t.Fatalf("want exactly one twin listing, from nested; got:\n  %s", listingDump(listings))
	}
}

// forkedRoot: root requires example.com/root/nested but replaces it with an
// in-repo fork, so one import path has two file sets. nested's own view
// twins only Y; the fork's view twins only X.
//
//	nested/lib/b.go        //go:build !tagx  X, Y
//	nested/lib/b_x.go      //go:build tagx   Y        -> twins {Y}
//	nested-fork/lib/b.go   //go:build !tagx  X, Y
//	nested-fork/lib/b_x.go //go:build tagx   X        -> twins {X}
func forkedRoot() map[string]string {
	const both = "package lib\n\nfunc X() string { return \"x\" }\n\nfunc Y() string { return \"y\" }\n"
	const use = "\"example.com/root/nested/lib\"\n\n"
	return map[string]string{
		"go.mod": "module example.com/root\n\ngo 1.22\n\nrequire example.com/root/nested v0.0.0\n\n" +
			"replace example.com/root/nested => ./nested-fork\n",
		"app/app.go":             "package app\n\nimport " + use + "func Use() string { return lib.X() + lib.Y() }\n",
		"nested/go.mod":          "module example.com/root/nested\n\ngo 1.22\n",
		"nested/lib/b.go":        "//go:build !tagx\n\n" + both,
		"nested/lib/b_x.go":      "//go:build tagx\n\npackage lib\n\nfunc Y() string { return \"yx\" }\n",
		"nested/use/u.go":        "package use\n\nimport " + use + "func U() string { return lib.X() + lib.Y() }\n",
		"nested-fork/go.mod":     "module example.com/root/nested\n\ngo 1.22\n",
		"nested-fork/lib/b.go":   "//go:build !tagx\n\n" + both,
		"nested-fork/lib/b_x.go": "//go:build tagx\n\npackage lib\n\nfunc X() string { return \"xx\" }\n",
	}
}

// 6. Per-file listings of one package path union the module views: root
// lists nested/lib through the fork (twins {X}), nested lists its own copy
// (twins {Y}); a name twinned in any view is twinned, so all four answers
// onto X and Y are file-scoped whichever listing ran last.
func TestLoadReferencedTwinsUnionsModuleViews(t *testing.T) {
	got, listings, _ := modulesPass(t, forkedRoot(), []string{"app/app.go", "nested/use/u.go"})

	if len(listings) != 2 {
		t.Fatalf("sanity: ran %d twin listings, want 2 (root and nested):\n  %s", len(listings), listingDump(listings))
	}
	for _, l := range listings {
		if (l.dir != "." && l.dir != "nested") || !containsString(l.patterns, nestedLib) {
			t.Fatalf("sanity: want one listing of %s from . and one from nested; got:\n  %s", nestedLib, listingDump(listings))
		}
	}
	if listings[0].dir == listings[1].dir {
		t.Fatalf("sanity: both listings ran from %s:\n  %s", listings[0].dir, listingDump(listings))
	}
	for _, want := range []string{
		"app/app.go:Use CALLS file:nested-fork/lib/b.go#X",
		"app/app.go:Use CALLS file:nested-fork/lib/b.go#Y",
		"nested/use/u.go:U CALLS file:nested/lib/b.go#Y",
		"nested/use/u.go:U CALLS file:nested/lib/b.go#X",
	} {
		if !containsString(got, want) {
			t.Errorf("did not produce %s; got\n  %s\nlistings:\n  %s", want, strings.Join(got, "\n  "), listingDump(listings))
		}
	}
}

// brokenNestedGoMod still declares nested's module path, so root's replace
// resolves it (a dependency's go.mod is parsed laxly), but `go` refuses it
// as the main module: nested's own load fails.
const brokenNestedGoMod = "module example.com/root/nested\n\ngo 1.22\n\nnot a directive\n"

// 7. A whole-project pass in which a nested module fails to load folds none
// of its packages, so it lists the referenced ones like a per-file pass:
// root's answer onto the nested twin stays file-scoped.
func TestSemanticWholePassListsTwinsOfAFailedNestedModule(t *testing.T) {
	files := requiringRoot()
	files["nested/go.mod"] = brokenNestedGoMod
	got, listings, _, reason := modulesPassAllowingIncomplete(t, files, nil)

	if !strings.Contains(reason, "nested") {
		t.Fatalf("sanity: want the pass incomplete naming nested; reason %q", reason)
	}
	if len(edgesFrom(got, "app/app.go:")) == 0 {
		t.Fatalf("sanity: root did not load (no app/app.go answers); reason %q", reason)
	}
	if len(listings) != 1 || listings[0].dir != "." || !containsString(listings[0].patterns, nestedLib) {
		t.Errorf("want exactly one twin listing of %s, from .; got:\n  %s", nestedLib, listingDump(listings))
	}
	want := "app/app.go:Use CALLS file:nested/lib/b.go#Binder.Bind"
	if !containsString(got, want) {
		t.Errorf("did not produce %s; got\n  %s", want, strings.Join(got, "\n  "))
	}
}

// forkedRootWithLibCaller is forkedRoot plus an untagged file inside
// nested's copy of the twinned package, so a file inside a copy can be a
// per-file pass's scope.
func forkedRootWithLibCaller() map[string]string {
	files := forkedRoot()
	files["nested/lib/c.go"] = "package lib\n\nfunc C() string { return X() + Y() }\n"
	return files
}

// forkedRootWithLibImplementor is forkedRoot plus an interface I that only
// the fork twins and a type T in nested's copy that implements it through
// the embedded universe error. Nothing in nested's lib names an object of a
// package (a method receiver would reference lib, and so would an import's
// package name, whose object belongs to the importing package), so a
// per-file pass over t.go references no package: lib is only folded, and
// only the implements answer addresses it.
//
//	nested/lib/i.go          (untagged)       I
//	nested/lib/t.go          (untagged)       T implements I
//	nested-fork/lib/i.go     //go:build !tagx I
//	nested-fork/lib/i_x.go   //go:build tagx  I     -> fork twins {X, I, I.Error}
func forkedRootWithLibImplementor() map[string]string {
	const iface = "package lib\n\ntype I interface{ Error() string }\n"
	files := forkedRoot()
	files["nested/lib/i.go"] = iface
	files["nested/lib/t.go"] = "package lib\n\ntype T struct{ error }\n"
	files["nested-fork/lib/i.go"] = "//go:build !tagx\n\n" + iface
	files["nested-fork/lib/i_x.go"] = "//go:build tagx\n\n" + iface
	return files
}

// 8. A whole-project pass unions the twins of every copy of a package path:
// nested's copy twins only Y and the fork only X, yet all four answers onto
// X and Y are file-scoped, including root's onto Y, which only nested's
// copy twins (so the fork's built files, folded second, must be recorded
// too). An exhaustive pass lists nothing.
func TestSemanticWholePassUnionsForkedModuleCopies(t *testing.T) {
	got, listings, _ := modulesPass(t, forkedRoot(), nil)

	if len(listings) != 0 {
		t.Errorf("a whole-project pass ran twin listing(s):\n  %s", listingDump(listings))
	}
	for _, want := range []string{
		"app/app.go:Use CALLS file:nested-fork/lib/b.go#X",
		"app/app.go:Use CALLS file:nested-fork/lib/b.go#Y",
		"nested/use/u.go:U CALLS file:nested/lib/b.go#X",
		"nested/use/u.go:U CALLS file:nested/lib/b.go#Y",
	} {
		if !containsString(got, want) {
			t.Errorf("whole-project pass did not produce %s; got\n  %s", want, strings.Join(got, "\n  "))
		}
	}
}

// 9. A per-file pass answers each file as the whole-project pass does, also
// when the file lies inside one copy of a forked package path: the other
// copies are listed, so the twins are the same union. In [nested/lib/c.go]
// lib is folded, not referenced from another package, and its fork copy is
// still listed.
func TestSemanticSingleFilePassesAnswerAsTheWholePass(t *testing.T) {
	files := forkedRootWithLibCaller()
	whole, _, _ := modulesPass(t, files, nil)

	for _, file := range []string{"app/app.go", "nested/use/u.go", "nested/lib/c.go"} {
		got, listings, _ := modulesPass(t, files, []string{file})
		want := edgesFrom(whole, file+":")
		if len(want) == 0 {
			t.Fatalf("sanity: the whole-project pass produced no answers from %s", file)
		}
		if gotFrom := edgesFrom(got, file+":"); !equalStrings(gotFrom, want) {
			t.Errorf("per-file pass [%s] answered\n  %s\nwant (whole-project pass)\n  %s\nlistings:\n  %s",
				file, strings.Join(gotFrom, "\n  "), strings.Join(want, "\n  "), listingDump(listings))
		}
		if file == "nested/lib/c.go" {
			const x = "nested/lib/c.go:C CALLS file:nested/lib/b.go#X"
			if !containsString(got, x) {
				t.Errorf("per-file pass [%s] did not produce %s; got\n  %s", file, x, strings.Join(got, "\n  "))
			}
		}
	}
}

// 10. A folded package counts as one the pass depends on even when no call
// answer references it: [nested/lib/t.go] folds lib and references nothing,
// yet the implements answer onto I, which only the fork's copy twins, is
// file-scoped, because the fork's copy is listed. (Only this edge is
// compared with the whole-project pass: there root's load also reaches the
// fork's own I, an answer a pass that loads only nested cannot give.)
func TestSemanticPerFilePassListsCopiesOfAFoldedPackage(t *testing.T) {
	files := forkedRootWithLibImplementor()
	const file = "nested/lib/t.go"
	whole, _, _ := modulesPass(t, files, nil)
	got, listings, _ := modulesPass(t, files, []string{file})

	const want = "nested/lib/t.go:T SUPERTYPE_OF file:nested/lib/i.go#I"
	if !containsString(whole, want) {
		t.Fatalf("sanity: the whole-project pass did not produce %s; got\n  %s", want, strings.Join(whole, "\n  "))
	}
	if !containsString(got, want) {
		t.Errorf("per-file pass [%s] did not produce %s; got\n  %s\nlistings:\n  %s",
			file, want, strings.Join(got, "\n  "), listingDump(listings))
	}
	if len(listings) != 1 || listings[0].dir != "nested-fork" || !containsString(listings[0].patterns, nestedLib) {
		t.Errorf("want exactly one twin listing of %s, from nested-fork; got:\n  %s", nestedLib, listingDump(listings))
	}
}

// 11. A copy a pass already folded or listed is not listed again: [app/app.go]
// lists nested/lib from . (the fork) and nested's copy from nested;
// [nested/use/u.go] lists the fork's copy from nested-fork only (nested's
// own copy it resolved through the referenced listing).
func TestSemanticPerFilePassListsEachForkCopyOnce(t *testing.T) {
	for _, tc := range []struct {
		file string
		dirs []string
	}{
		{"app/app.go", []string{".", "nested"}},
		{"nested/use/u.go", []string{"nested", "nested-fork"}},
	} {
		_, listings, _ := modulesPass(t, forkedRoot(), []string{tc.file})
		var dirs []string
		for _, l := range listings {
			dirs = append(dirs, l.dir)
		}
		sort.Strings(dirs)
		if !equalStrings(dirs, tc.dirs) {
			t.Errorf("per-file pass [%s] listed from %v, want once from each of %v:\n  %s",
				tc.file, dirs, tc.dirs, listingDump(listings))
		}
	}
}

// 12. A copy of a module that failed to load in this pass is not listed (its
// listing would fail too): in forkedRoot with nested's go.mod broken, root
// folds the fork's copy itself and nested's copy is skipped.
func TestSemanticWholePassSkipsAFailedModulesCopy(t *testing.T) {
	files := forkedRoot()
	files["nested/go.mod"] = brokenNestedGoMod
	got, listings, _, reason := modulesPassAllowingIncomplete(t, files, nil)

	if !strings.Contains(reason, "nested") {
		t.Fatalf("sanity: want the pass incomplete naming nested; reason %q", reason)
	}
	if len(edgesFrom(got, "app/app.go:")) == 0 {
		t.Fatalf("sanity: root did not load (no app/app.go answers); reason %q", reason)
	}
	if len(listings) != 0 {
		t.Errorf("ran twin listing(s) in a pass whose nested module failed:\n  %s", listingDump(listings))
	}
}
