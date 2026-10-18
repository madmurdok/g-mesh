package main

// SUPERTYPE_OF across Go modules: a type pairs only with the interfaces of a
// load (a module's build) that holds it, a consumer module's load pairs the
// exported types it imports with the interfaces it imports, and a per-file
// pass ends on the same edge set as a fresh whole-project pass.

import (
	"slices"
	"sort"
	"strings"
	"testing"
)

// supertypeLedger is core's view of the SUPERTYPE_OF edges: edge id to its
// rendered label, updated by each pass's diff.
type supertypeLedger map[string]string

func (l supertypeLedger) apply(t *testing.T, root string, diff fileChangeDiff) {
	t.Helper()
	for _, id := range diff.DeleteEdgeIds {
		delete(l, id)
	}
	for label, edge := range supertypeEdgesByLabel(t, root, diff) {
		if edge.Kind == edgeKindSupertypeOf {
			l[edge.ID] = label
		}
	}
}

func (l supertypeLedger) labels() []string {
	out := make([]string, 0, len(l))
	for _, label := range l {
		out = append(out, label)
	}
	sort.Strings(out)
	return out
}

// crossModuleRun is one plugin process over a fixture, with the ledger core
// would hold.
type crossModuleRun struct {
	t      *testing.T
	root   string
	state  *pluginState
	ledger supertypeLedger
}

func newCrossModuleRun(t *testing.T, files map[string]string) *crossModuleRun {
	t.Helper()
	requireGoToolchain(t)
	t.Setenv("GOWORK", "")
	t.Setenv("GOFLAGS", "")
	root := writeTree(t, files)
	return &crossModuleRun{t: t, root: root, state: newPluginState(root), ledger: supertypeLedger{}}
}

// pass runs a semantic pass over scope (nil: the whole project), requires it
// complete and applies it to the ledger.
func (r *crossModuleRun) pass(scope ...string) {
	r.t.Helper()
	diff, reason, _ := r.state.handleSemanticPass(scope)
	if reason != "" {
		r.t.Fatalf("pass over %v answered incomplete: %s", scope, reason)
	}
	r.ledger.apply(r.t, r.root, diff)
}

func (r *crossModuleRun) rewrite(relPath, contents string) {
	r.t.Helper()
	carryRewrite(r.t, r.root, relPath, contents)
}

// requireEdges requires the ledger to hold exactly want.
func (r *crossModuleRun) requireEdges(step string, want ...string) {
	r.t.Helper()
	sort.Strings(want)
	if got := r.ledger.labels(); !slices.Equal(got, want) {
		r.t.Fatalf("%s: SUPERTYPE_OF edges\n got  %q\n want %q", step, got, want)
	}
}

// requireAgreesWithWhole requires the ledger to equal what a fresh process's
// whole-project pass over the current tree emits.
func (r *crossModuleRun) requireAgreesWithWhole(step string) {
	r.t.Helper()
	fresh := supertypeLedger{}
	diff, reason, _ := newPluginState(r.root).handleSemanticPass(nil)
	if reason != "" {
		r.t.Fatalf("%s: fresh whole-project pass answered incomplete: %s", step, reason)
	}
	fresh.apply(r.t, r.root, diff)
	r.requireEdges(step+" (against a fresh whole-project pass)", fresh.labels()...)
}

func supertypeLabel(from, iface string) string {
	return from + " SUPERTYPE_OF container:" + iface
}

// Two modules that share no build. Each has a type satisfying, by
// predeclared signatures alone, the other module's interface too.
var unrelatedModules = map[string]string{
	"a/go.mod": "module example.com/a\n\ngo 1.22\n",
	"a/j/j.go": "package j\n\nimport \"io\"\n\ntype J interface{ Error() string }\n\n" +
		"type W interface{ To(io.Writer) }\n\ntype E struct{ error }\n",
	"b/go.mod": "module example.com/b\n\ngo 1.22\n",
	"b/s/s.go": "package s\n\nimport \"io\"\n\ntype Local interface{ Error() string }\n\n" +
		"type S struct{ error }\n\nfunc (S) To(io.Writer) {}\n",
}

var unrelatedModulesEdges = []string{
	supertypeLabel("a/j/j.go:E", "example.com/a/j#J"),
	supertypeLabel("b/s/s.go:S", "example.com/b/s#Local"),
}

// Each type is checked against its own load's interfaces only, in a whole
// pass, in a per-file pass after it, and in a fresh per-file pass whose
// scope spans both modules.
func TestGoSupertypePairsNoTypeWithAnotherModulesInterface(t *testing.T) {
	run := newCrossModuleRun(t, unrelatedModules)
	run.pass()
	run.requireEdges("whole", unrelatedModulesEdges...)
	run.pass("b/s/s.go")
	run.requireEdges("per-file b/s/s.go", unrelatedModulesEdges...)
	run.pass("a/j/j.go")
	run.requireEdges("per-file a/j/j.go", unrelatedModulesEdges...)
	run.requireAgreesWithWhole("after per-file passes")

	mixed := newCrossModuleRun(t, unrelatedModules)
	mixed.pass("a/j/j.go", "b/s/s.go")
	mixed.requireEdges("fresh per-file over both modules", unrelatedModulesEdges...)
}

// One module, two packages that do not import each other: the implicit pair
// is in the module's load, and a per-file pass of the implementor keeps it.
func TestGoSupertypeKeepsAnImplicitPairWithinOneModule(t *testing.T) {
	run := newCrossModuleRun(t, map[string]string{
		"go.mod": "module example.com/m\n\ngo 1.22\n",
		"j/j.go": "package j\n\nimport \"io\"\n\ntype W interface{ To(io.Writer) }\n",
		"s/s.go": "package s\n\nimport \"io\"\n\ntype S struct{}\n\nfunc (S) To(io.Writer) {}\n",
	})
	want := supertypeLabel("s/s.go:S", "example.com/m/j#W")
	run.pass()
	run.requireEdges("whole", want)
	run.pass("s/s.go")
	run.requireEdges("per-file s/s.go", want)
	run.requireAgreesWithWhole("after per-file pass")
}

// Module b requires a through a replace and imports a/j: b's load holds
// a/j.W from export data, so S pairs with it.
func TestGoSupertypeKeepsAnEdgeOntoAReplacedModulesInterface(t *testing.T) {
	run := newCrossModuleRun(t, map[string]string{
		"a/go.mod": "module example.com/a\n\ngo 1.22\n",
		"a/j/j.go": crossLibJ,
		"b/go.mod": "module example.com/b\n\ngo 1.22\n\nrequire example.com/a v0.0.0\n\n" +
			"replace example.com/a => ../a\n",
		"b/s/s.go": "package s\n\nimport \"io\"\n\ntype S struct{}\n\nfunc (S) To(io.Writer) {}\n",
		"b/u/u.go": "package u\n\nimport \"example.com/a/j\"\n\nvar _ j.W\n",
	})
	want := supertypeLabel("b/s/s.go:S", "example.com/a/j#W")
	run.pass()
	run.requireEdges("whole", want)
	run.pass("b/s/s.go")
	run.requireEdges("per-file b/s/s.go", want)
	run.pass("b/u/u.go")
	run.requireEdges("per-file b/u/u.go", want)
	run.requireAgreesWithWhole("after per-file passes")
}

const crossLibJ = "package j\n\nimport \"io\"\n\ntype W interface{ To(io.Writer) }\n"

// consumerModules: lib declares W, impl declares S (exported), t (unexported)
// and G (generic), all with To; app imports lib/j and impl/s and declares
// its own Sink. Neither lib nor impl imports the other.
var consumerModules = map[string]string{
	"lib/go.mod":  "module example.com/lib\n\ngo 1.22\n",
	"lib/j/j.go":  crossLibJ,
	"impl/go.mod": "module example.com/impl\n\ngo 1.22\n",
	"impl/s/s.go": "package s\n\nimport \"io\"\n\n" +
		"type S struct{}\n\nfunc (S) To(io.Writer) {}\n\nfunc (S) String() string { return \"\" }\n\n" +
		"type t struct{}\n\nfunc (t) To(io.Writer) {}\n\nfunc New() t { return t{} }\n\n" +
		"type G[T any] struct{}\n\nfunc (G[T]) To(io.Writer) {}\n\nvar _ G[int]\n",
	"app/go.mod": "module example.com/app\n\ngo 1.22\n\nrequire (\n\texample.com/impl v0.0.0\n\texample.com/lib v0.0.0\n)\n\n" +
		"replace example.com/impl => ../impl\n\nreplace example.com/lib => ../lib\n",
	"app/main.go": "package main\n\nimport (\n\t\"example.com/impl/s\"\n\t\"example.com/lib/j\"\n)\n\n" +
		"var _ j.W = s.S{}\n\nvar _ = s.New()\n\nvar _ s.G[int]\n\nfunc main() {}\n",
	"app/c/c.go": "package c\n\nimport \"io\"\n\ntype Sink interface{ To(io.Writer) }\n",
}

var (
	consumerSToW    = supertypeLabel("impl/s/s.go:S", "example.com/lib/j#W")
	consumerSToSink = supertypeLabel("impl/s/s.go:S", "example.com/app/c#Sink")
	consumerSinkToW = supertypeLabel("app/c/c.go:Sink", "example.com/lib/j#W")
	consumerWToSink = supertypeLabel("lib/j/j.go:W", "example.com/app/c#Sink")
)

var consumerModulesEdges = []string{consumerSToW, consumerSToSink, consumerSinkToW, consumerWToSink}

const (
	crossLibJCompat = "package j\n\nimport \"io\"\n\ntype W interface {\n\tTo(io.Writer)\n\tString() string\n}\n"
	crossLibJBreak  = "package j\n\nimport \"io\"\n\ntype W interface {\n\tTo(io.Writer)\n\tClose() error\n}\n"
)

// app's load pairs the exported, non-generic types it imports with the
// interfaces it imports; t and G get no row.
func TestGoSupertypeConsumerModulePairsImportedExportedTypes(t *testing.T) {
	run := newCrossModuleRun(t, consumerModules)
	run.pass()
	run.requireEdges("whole", consumerModulesEdges...)
	for _, file := range []string{"impl/s/s.go", "app/main.go", "app/c/c.go"} {
		run.pass(file)
		run.requireEdges("per-file "+file, consumerModulesEdges...)
	}
	run.requireAgreesWithWhole("after per-file passes")
}

// An interface edit S still satisfies keeps S's edge, which only app's load
// pairs; Sink, which lacks the new method, loses its edge.
func TestGoSupertypeCompatibleInterfaceEditKeepsACrossLoadEdge(t *testing.T) {
	run := newCrossModuleRun(t, consumerModules)
	run.pass()
	run.rewrite("lib/j/j.go", crossLibJCompat)
	run.pass("lib/j/j.go")
	run.requireEdges("per-file after the compatible edit", consumerSToW, consumerSToSink, consumerWToSink)
	run.requireAgreesWithWhole("after the compatible edit")
}

// A carried edge takes the edited interface's signature, so the next pass of
// the interface's file re-checks no implementor.
func TestGoSupertypeCarriedEdgeIsNotRecheckedTwice(t *testing.T) {
	run := newCrossModuleRun(t, consumerModules)
	run.pass()
	run.rewrite("lib/j/j.go", crossLibJCompat)
	run.pass("lib/j/j.go")
	if !slices.Contains(run.state.semantic.recheckedImplementors, "impl/s/s.go") {
		t.Fatalf("the compatible edit did not re-check impl/s/s.go; rechecked: %v",
			run.state.semantic.recheckedImplementors)
	}
	run.pass("lib/j/j.go")
	if len(run.state.semantic.recheckedImplementors) != 0 {
		t.Errorf("second pass of the unchanged interface re-checked %v", run.state.semantic.recheckedImplementors)
	}
	run.requireEdges("second per-file pass", consumerSToW, consumerSToSink, consumerWToSink)
}

// An edit adding a method S lacks, and deleting the interface, retract the
// cross-load edges onto it.
func TestGoSupertypeBreakingInterfaceEditRetractsCrossLoadEdges(t *testing.T) {
	run := newCrossModuleRun(t, consumerModules)
	run.pass()
	run.rewrite("lib/j/j.go", crossLibJBreak)
	run.pass("lib/j/j.go")
	run.requireEdges("per-file after the breaking edit", consumerSToSink, consumerWToSink)
	run.requireAgreesWithWhole("after the breaking edit")

	gone := newCrossModuleRun(t, consumerModules)
	gone.pass()
	gone.rewrite("lib/j/j.go", "package j\n")
	gone.pass("lib/j/j.go")
	gone.requireEdges("per-file after deleting the interface", consumerSToSink)
	gone.requireAgreesWithWhole("after deleting the interface")
}

// k's export data names j only for j.Other, so a load importing k alone
// holds a partial j: its missing W is not gone, and S's edge onto it is
// carried, across modules and within one.
func TestGoSupertypeCarriesAnEdgeOntoAPartiallyImportedPackage(t *testing.T) {
	const (
		libJ = "package j\n\nimport \"io\"\n\ntype W interface{ To(io.Writer) }\n\ntype Other struct{}\n"
		sSrc = "package s\n\nimport (\n\t\"io\"\n\n\t\"example.com/%s/k\"\n)\n\nvar _ = k.F\n\n" +
			"type S struct{}\n\nfunc (S) To(io.Writer) {}\n"
	)
	cross := newCrossModuleRun(t, map[string]string{
		"lib/go.mod":  "module example.com/lib\n\ngo 1.22\n",
		"lib/j/j.go":  libJ,
		"lib/k/k.go":  "package k\n\nimport \"example.com/lib/j\"\n\nfunc F() j.Other { return j.Other{} }\n",
		"impl/go.mod": "module example.com/impl\n\ngo 1.22\n\nrequire example.com/lib v0.0.0\n\nreplace example.com/lib => ../lib\n",
		"impl/s/s.go": strings.Replace(sSrc, "%s", "lib", 1),
		"app/go.mod": "module example.com/app\n\ngo 1.22\n\nrequire (\n\texample.com/impl v0.0.0\n\texample.com/lib v0.0.0\n)\n\n" +
			"replace example.com/impl => ../impl\n\nreplace example.com/lib => ../lib\n",
		"app/main.go": "package main\n\nimport (\n\t\"example.com/impl/s\"\n\t\"example.com/lib/j\"\n)\n\n" +
			"var _ j.W = s.S{}\n\nfunc main() {}\n",
	})
	crossWant := supertypeLabel("impl/s/s.go:S", "example.com/lib/j#W")
	cross.pass()
	cross.requireEdges("cross-module whole", crossWant)
	cross.pass("impl/s/s.go")
	cross.requireEdges("cross-module per-file impl/s/s.go", crossWant)
	cross.requireAgreesWithWhole("cross-module after per-file pass")

	in := newCrossModuleRun(t, map[string]string{
		"go.mod": "module example.com/m\n\ngo 1.22\n",
		"j/j.go": libJ,
		"k/k.go": "package k\n\nimport \"example.com/m/j\"\n\nfunc F() j.Other { return j.Other{} }\n",
		"s/s.go": strings.Replace(sSrc, "%s", "m", 1),
	})
	inWant := supertypeLabel("s/s.go:S", "example.com/m/j#W")
	in.pass()
	in.requireEdges("in-module whole", inWant)
	in.pass("s/s.go")
	in.requireEdges("in-module per-file s/s.go", inWant)
	in.requireAgreesWithWhole("in-module after per-file pass")
}
