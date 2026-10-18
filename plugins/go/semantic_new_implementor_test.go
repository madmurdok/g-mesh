package main

// A per-file pass of an interface's file that loosens, re-signs or adds an
// interface also loads the files outside its scope whose types have every
// method name of it (GM-629), so types in other packages that newly satisfy
// it gain their SUPERTYPE_OF edge in that pass, at a bounded cost. Unless a
// test says otherwise, each one runs a whole-project pass, rewrites
// lib/lib.go, runs a per-file pass of it alone on the same long-lived
// pluginState, and compares core's ledger with a fresh whole-project pass.

import (
	"slices"
	"strings"
	"testing"
)

const newImplLibStrict = `package lib

type Binder interface {
	Bind() string
	Close()
}

type impl struct{}

func (impl) Bind() string { return "impl" }

func (impl) Close() {}
`

// newImplLibLoose is newImplLibStrict with Close() removed from Binder.
var newImplLibLoose = strings.Replace(newImplLibStrict, "\tBind() string\n\tClose()\n", "\tBind() string\n", 1)

// newImplFiles: other does not import lib, app does; mis.Bad has every
// method name of Binder (strict and loose) with the wrong types.
func newImplFiles(lib string) map[string]string {
	return map[string]string{
		"go.mod":     "module example.com/x\n\ngo 1.22\n",
		"lib/lib.go": lib,
		"other/other.go": "package other\n\ntype Plain struct{}\n\n" +
			"func (Plain) Bind() string { return \"p\" }\n\nfunc (Plain) Open() {}\n",
		"app/app.go": "package app\n\nimport \"example.com/x/lib\"\n\nvar _ lib.Binder\n\ntype AppT struct{}\n\n" +
			"func (AppT) Bind() string { return \"a\" }\n\nfunc (AppT) Open() {}\n",
		"mis/mis.go": "package mis\n\ntype Bad struct{}\n\nfunc (Bad) Bind() int { return 0 }\n\nfunc (Bad) Close() int { return 0 }\n",
	}
}

const (
	newImplBinder = "example.com/x/lib#Binder"
	newImplOpener = "example.com/x/lib#Opener"
)

var newImplImplEdge = supertypeLabel("lib/lib.go:impl", newImplBinder)

// newImplEdit runs a whole pass over files, then rewrites relPath and runs a
// per-file pass of it.
func newImplEdit(t *testing.T, files map[string]string, relPath, contents string) *crossModuleRun {
	t.Helper()
	run := newCrossModuleRun(t, files)
	run.pass()
	run.rewrite(relPath, contents)
	run.pass(relPath)
	return run
}

func requireNewImplementorFiles(t *testing.T, run *crossModuleRun, want ...string) {
	t.Helper()
	if got := run.state.semantic.newImplementorFiles; !slices.Equal(got, want) {
		t.Errorf("newImplementorFiles = %q, want %q", got, want)
	}
}

func withMaxNewImplementorPkgs(t *testing.T, k int) {
	t.Helper()
	old := maxNewImplementorPkgs
	maxNewImplementorPkgs = k
	t.Cleanup(func() { maxNewImplementorPkgs = old })
}

// Behaviours 1 and 2: a method removed from Binder gives the non-importer
// and the importer their edge; mis.Bad matches by name only, so it is
// loaded and gets none.
func TestGoNewImplementorAfterALoosenedInterface(t *testing.T) {
	run := newImplEdit(t, newImplFiles(newImplLibStrict), "lib/lib.go", newImplLibLoose)
	run.requireEdges("loosened",
		newImplImplEdge,
		supertypeLabel("other/other.go:Plain", newImplBinder),
		supertypeLabel("app/app.go:AppT", newImplBinder))
	run.requireAgreesWithWhole("loosened")
	requireNewImplementorFiles(t, run, "app/app.go", "mis/mis.go", "other/other.go")
	if n := run.state.semantic.skippedNewImplementors; n != 0 {
		t.Errorf("skippedNewImplementors = %d, want 0", n)
	}
}

// Behaviour 1: a signature change gives the types now matching their edge,
// while the old implementor's edge is retracted in the same pass.
func TestGoNewImplementorAfterASignatureChange(t *testing.T) {
	files := newImplFiles("package lib\n\ntype Binder interface{ Bind(int) }\n")
	files["old/old.go"] = "package old\n\ntype Old struct{}\n\nfunc (Old) Bind(int) {}\n"
	run := newCrossModuleRun(t, files)
	run.pass()
	run.requireEdges("before", supertypeLabel("old/old.go:Old", newImplBinder))
	run.rewrite("lib/lib.go", "package lib\n\ntype Binder interface{ Bind() string }\n")
	run.pass("lib/lib.go")
	run.requireEdges("re-signed",
		supertypeLabel("other/other.go:Plain", newImplBinder),
		supertypeLabel("app/app.go:AppT", newImplBinder))
	run.requireAgreesWithWhole("re-signed")
}

// Behaviour 1: a new interface gets its implementors in other packages.
func TestGoNewImplementorOfANewInterface(t *testing.T) {
	run := newImplEdit(t, newImplFiles(newImplLibStrict), "lib/lib.go",
		newImplLibStrict+"\ntype Opener interface{ Open() }\n")
	run.requireEdges("new interface",
		newImplImplEdge,
		supertypeLabel("other/other.go:Plain", newImplOpener),
		supertypeLabel("app/app.go:AppT", newImplOpener))
	run.requireAgreesWithWhole("new interface")
	requireNewImplementorFiles(t, run, "app/app.go", "other/other.go")
}

// Behaviour 3: a body-only edit loads no candidate and needs no second load
// (mis.Bad has every method name of the unchanged Binder).
func TestGoNewImplementorBodyOnlyEditLoadsNothing(t *testing.T) {
	run := newImplEdit(t, newImplFiles(newImplLibStrict), "lib/lib.go",
		strings.Replace(newImplLibStrict, `return "impl"`, `return "changed"`, 1))
	if got := run.state.semantic.newImplementorFiles; got != nil {
		t.Errorf("newImplementorFiles = %q, want nil", got)
	}
	if got := run.state.semantic.recheckedImplementors; got != nil {
		t.Errorf("recheckedImplementors = %q, want nil (no second load)", got)
	}
	run.requireEdges("body edit", newImplImplEdge)
}

// Behaviour 4: three candidate packages (other, app, mis). Over the bound
// nothing is added and the count is reported, until a whole pass restores
// the edges; at exactly the bound all are added.
func TestGoNewImplementorBound(t *testing.T) {
	all := []string{
		newImplImplEdge,
		supertypeLabel("other/other.go:Plain", newImplBinder),
		supertypeLabel("app/app.go:AppT", newImplBinder),
	}
	t.Run("over", func(t *testing.T) {
		withMaxNewImplementorPkgs(t, 2)
		run := newImplEdit(t, newImplFiles(newImplLibStrict), "lib/lib.go", newImplLibLoose)
		run.requireEdges("over the bound", newImplImplEdge)
		requireNewImplementorFiles(t, run)
		if n := run.state.semantic.skippedNewImplementors; n != 3 {
			t.Errorf("skippedNewImplementors = %d, want 3", n)
		}
		run.pass()
		run.requireEdges("whole pass after", all...)
	})
	t.Run("exactly", func(t *testing.T) {
		withMaxNewImplementorPkgs(t, 3)
		run := newImplEdit(t, newImplFiles(newImplLibStrict), "lib/lib.go", newImplLibLoose)
		run.requireEdges("at the bound", all...)
		if n := run.state.semantic.skippedNewImplementors; n != 0 {
			t.Errorf("skippedNewImplementors = %d, want 0", n)
		}
	})
}

// Behaviour 5: before any whole-project pass, an interface package with no
// baseline is not "new": other is indexed (its own per-file pass), yet the
// loosening of lib loads nothing.
func TestGoNewImplementorNeedsAWholePassBaseline(t *testing.T) {
	run := newCrossModuleRun(t, newImplFiles(newImplLibStrict))
	run.pass("other/other.go")
	run.rewrite("lib/lib.go", newImplLibLoose)
	run.pass("lib/lib.go")
	if got := run.state.semantic.newImplementorFiles; got != nil {
		t.Errorf("newImplementorFiles = %q, want nil", got)
	}
}

// Behaviour 6: a per-file pass of other gives Plain its Bind; the index
// follows it, so the later loosening of lib finds Plain.
func TestGoNewImplementorIndexFollowsPerFilePasses(t *testing.T) {
	files := newImplFiles(newImplLibStrict)
	files["other/other.go"] = "package other\n\ntype Plain struct{}\n\nfunc (Plain) Open() {}\n"
	run := newCrossModuleRun(t, files)
	run.pass()
	run.rewrite("other/other.go", newImplFiles("")["other/other.go"])
	run.pass("other/other.go")
	run.rewrite("lib/lib.go", newImplLibLoose)
	run.pass("lib/lib.go")
	if !slices.Contains(run.ledger.labels(), supertypeLabel("other/other.go:Plain", newImplBinder)) {
		t.Errorf("Plain did not gain its edge; edges %q", run.ledger.labels())
	}
	run.requireAgreesWithWhole("after both edits")
}

// newImplWorkspace: m2 is a second module; m2/use imports lib and is the
// only package that brings lib.Binder into m2's load, m2/nouse.N matches.
func newImplWorkspace(use string) map[string]string {
	return map[string]string{
		"go.work":       "go 1.22\n\nuse (\n\t.\n\t./m2\n)\n",
		"go.mod":        "module example.com/x\n\ngo 1.22\n",
		"lib/lib.go":    "package lib\n\ntype Binder interface {\n\tBind() string\n\tClose()\n}\n",
		"m2/go.mod":     "module example.com/m2\n\ngo 1.22\n\nrequire example.com/x v0.0.0\n\nreplace example.com/x => ../\n",
		"m2/use/u.go":   use,
		"m2/nouse/n.go": "package nouse\n\ntype N struct{}\n\nfunc (N) Bind() string { return \"\" }\n",
	}
}

const newImplLooseOnly = "package lib\n\ntype Binder interface{ Bind() string }\n"

// Behaviour 7: N is in another module's load whose importer of lib is not a
// candidate itself; the pass also loads that importer, with or without
// named types of its own, so N gains its edge.
func TestGoNewImplementorInAForeignLoad(t *testing.T) {
	for name, use := range map[string]string{
		"importer with a type":  "package use\n\nimport \"example.com/x/lib\"\n\nvar _ lib.Binder\n\ntype U struct{}\n",
		"importer with no type": "package use\n\nimport \"example.com/x/lib\"\n\nvar _ lib.Binder\n",
	} {
		t.Run(name, func(t *testing.T) {
			run := newImplEdit(t, newImplWorkspace(use), "lib/lib.go", newImplLooseOnly)
			run.requireEdges("loosened", supertypeLabel("m2/nouse/n.go:N", newImplBinder))
			run.requireAgreesWithWhole("loosened")
			requireNewImplementorFiles(t, run, "m2/nouse/n.go", "m2/use/u.go")
		})
	}
}

const newImplOpenerDecl = "\ntype Opener interface {\n\tOpen()\n\tBind() string\n}\n"

// Behaviour 8: T gets Open only by promotion from a type of emb. emb's own
// per-file pass adds it; the later new interface in lib finds T.
func TestGoNewImplementorThroughAPromotedMethod(t *testing.T) {
	cases := []struct {
		name           string
		files          map[string]string
		embPath, embTo string
		want           string
	}{
		{
			name: "struct field",
			files: map[string]string{
				"emb/emb.go": "package emb\n\ntype E struct{}\n",
				"other/other.go": "package other\n\nimport \"example.com/x/emb\"\n\ntype T struct{ emb.E }\n\n" +
					"func (T) Bind() string { return \"\" }\n",
			},
			embPath: "emb/emb.go", embTo: "package emb\n\ntype E struct{}\n\nfunc (E) Open() {}\n",
			want: "other/other.go:T",
		},
		{
			name: "pointer field",
			files: map[string]string{
				"emb/emb.go": "package emb\n\ntype E struct{}\n",
				"other/other.go": "package other\n\nimport \"example.com/x/emb\"\n\ntype T struct{ *emb.E }\n\n" +
					"func (T) Bind() string { return \"\" }\n",
			},
			embPath: "emb/emb.go", embTo: "package emb\n\ntype E struct{}\n\nfunc (*E) Open() {}\n",
			want: "other/other.go:T",
		},
		{
			name: "interface embed",
			files: map[string]string{
				"emb/emb.go": "package emb\n\ntype EI interface{ Ping() }\n",
				"other/other.go": "package other\n\nimport \"example.com/x/emb\"\n\ntype TI interface {\n\temb.EI\n" +
					"\tBind() string\n}\n",
			},
			embPath: "emb/emb.go", embTo: "package emb\n\ntype EI interface {\n\tPing()\n\tOpen()\n}\n",
			want: "other/other.go:TI",
		},
		{
			name: "two levels",
			files: map[string]string{
				"emb/emb.go": "package emb\n\ntype E struct{}\n",
				"mid/mid.go": "package mid\n\nimport \"example.com/x/emb\"\n\ntype M struct{ emb.E }\n",
				"other/other.go": "package other\n\nimport \"example.com/x/mid\"\n\ntype T struct{ mid.M }\n\n" +
					"func (T) Bind() string { return \"\" }\n",
			},
			embPath: "emb/emb.go", embTo: "package emb\n\ntype E struct{}\n\nfunc (E) Open() {}\n",
			want: "other/other.go:T",
		},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			c.files["go.mod"] = "module example.com/x\n\ngo 1.22\n"
			c.files["lib/lib.go"] = newImplLibStrict
			run := newImplEdit(t, c.files, c.embPath, c.embTo)
			run.rewrite("lib/lib.go", newImplLibStrict+newImplOpenerDecl)
			run.pass("lib/lib.go")
			if !slices.Contains(run.ledger.labels(), supertypeLabel(c.want, newImplOpener)) {
				t.Errorf("%s did not gain its edge; edges %q", c.want, run.ledger.labels())
			}
			run.requireAgreesWithWhole("after both edits")
		})
	}
}

// Behaviour 9: K = 1. two/both.go holds Both's edge onto Binder, so GM-620
// re-checks it; Half, in another file of the same package, is a candidate
// that the re-check loads anyway, so only other counts toward the bound.
func TestGoNewImplementorBoundSkipsPackagesLoadedAnyway(t *testing.T) {
	withMaxNewImplementorPkgs(t, 1)
	files := newImplFiles(newImplLibStrict)
	delete(files, "app/app.go")
	delete(files, "mis/mis.go")
	files["two/both.go"] = "package two\n\ntype Both struct{}\n\n" +
		"func (Both) Bind() string { return \"\" }\n\nfunc (Both) Close() {}\n"
	files["two/half.go"] = "package two\n\ntype Half struct{}\n\nfunc (Half) Bind() string { return \"\" }\n"
	run := newImplEdit(t, files, "lib/lib.go", newImplLibLoose)
	if n := run.state.semantic.skippedNewImplementors; n != 0 {
		t.Errorf("skippedNewImplementors = %d, want 0", n)
	}
	run.requireEdges("loosened",
		newImplImplEdge,
		supertypeLabel("two/both.go:Both", newImplBinder),
		supertypeLabel("two/half.go:Half", newImplBinder),
		supertypeLabel("other/other.go:Plain", newImplBinder))
	run.requireAgreesWithWhole("loosened")
}

// Behaviour 9: K = 1 in the foreign-load case: the importer m2/use joins
// the second load without counting toward the bound.
func TestGoNewImplementorBoundSkipsImporters(t *testing.T) {
	withMaxNewImplementorPkgs(t, 1)
	run := newImplEdit(t, newImplWorkspace("package use\n\nimport \"example.com/x/lib\"\n\nvar _ lib.Binder\n"),
		"lib/lib.go", newImplLooseOnly)
	if n := run.state.semantic.skippedNewImplementors; n != 0 {
		t.Errorf("skippedNewImplementors = %d, want 0", n)
	}
	run.requireEdges("loosened", supertypeLabel("m2/nouse/n.go:N", newImplBinder))
}
