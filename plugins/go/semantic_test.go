package main

// The semantic tier's own tests. Everything here that needs type
// information runs a real `packages.Load` over a real module on disk -
// there is no useful way to fake `go/types`, and a fake would test the fake.
//
// The one fixture module below carries every shape GM-280 left open, so all
// of them are judged against a single load rather than paying ~1s of `go
// list` per assertion:
//
//	receiver call through a variable        varReceiver      -> Handle.Name
//	receiver call through an embedded field embeddedReceiver -> Handle.Name
//	receiver call through an interface      interfaceReceiver-> Closer.Close
//	a mis-guessed import binding name       misguessedImport -> misnamed.Ping
//	a bare name under a dot import          dotImported      -> dotted.DotFunc
//	conversions written like calls          convert*         -> REFERENCES
//	implicit interface satisfaction         Handle, Wrapper  -> Closer

import (
	"os"
	"os/exec"
	"path/filepath"
	"sort"
	"strings"
	"testing"
)

// probeFiles is the fixture module. Package `actual` under directory
// `misnamed` is deliberate: it is the one thing that makes
// extract.go's packageNameFromPath guess wrong, which turns `actual.Ping()`
// into an open site instead of a qualified placeholder.
var probeFiles = map[string]string{
	"go.mod": "module example.com/probe\n\ngo 1.22\n",

	"api.go": `package probe

type Closer interface {
	Close() error
}

type Handle struct{ name string }

func (h *Handle) Close() error { return nil }

func (h *Handle) Name() string { return h.name }

type Wrapper struct {
	*Handle
}

type Kelvin float64

func newHandle() *Handle { return &Handle{name: "h"} }
`,

	"use.go": `package probe

import (
	"example.com/probe/misnamed"
	"example.com/probe/other"
)

func varReceiver() string {
	h := newHandle()
	return h.Name()
}

func embeddedReceiver(w *Wrapper) string {
	return w.Name()
}

func interfaceReceiver(c Closer) error {
	return c.Close()
}

func convert(v float64) interface{} {
	return Kelvin(v)
}

func convertCelsius(v float64) interface{} {
	return other.Celsius(v)
}

func misguessedImport() string {
	return actual.Ping()
}
`,

	"dotuse.go": `package probe

import . "example.com/probe/dotted"

func dotImported() string {
	return DotFunc()
}
`,

	"misnamed/pkg.go": "package actual\n\nfunc Ping() string { return \"pong\" }\n",
	"other/other.go":  "package other\n\ntype Celsius float64\n",
	"dotted/dotted.go": `package dotted

func DotFunc() string { return "dot" }
`,
}

func requireGoToolchain(t *testing.T) {
	t.Helper()
	if _, err := exec.LookPath("go"); err != nil {
		t.Skip("no `go` on PATH: the semantic tier has nothing to run")
	}
}

func writeProbeProject(t *testing.T) string {
	t.Helper()
	root := t.TempDir()
	for name, contents := range probeFiles {
		path := filepath.Join(root, filepath.FromSlash(name))
		if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
			t.Fatalf("mkdir %s: %v", path, err)
		}
		if err := os.WriteFile(path, []byte(contents), 0o644); err != nil {
			t.Fatalf("write %s: %v", path, err)
		}
	}
	return root
}

// renderEdges turns a semantic diff into lines a human can read and a test
// can compare exactly:
//
//	"use.go:varReceiver CALLS example.com/probe#Handle.Name"
//
// The left side is resolved through the structural extraction of the same
// tree, which is also the check that the `from` of every semantic edge is a
// node this plugin really emitted - an edge out of an id nothing declares
// would render as a bare hash and fail the comparison loudly.
func renderEdges(t *testing.T, root string, diff fileChangeDiff) []string {
	t.Helper()
	ws := loadWorkspace(root)
	labels := map[string]string{}
	for _, relPath := range walkProjectFiles(root) {
		content, err := os.ReadFile(filepath.Join(canonicalizeProjectRoot(root), filepath.FromSlash(relPath)))
		if err != nil {
			t.Fatalf("read %s: %v", relPath, err)
		}
		for _, node := range extractFile(ws, relPath, content).nodes {
			labels[node.ID] = relPath + ":" + node.QualifiedName
		}
	}
	targets := map[string]string{}
	for _, node := range diff.UpsertNodes {
		if node.Target == nil {
			t.Fatalf("semantic node %s (%s) carries no placeholder target", node.ID, node.QualifiedName)
		}
		if node.NativeKind != pendingSymbolNativeKind {
			t.Fatalf("semantic node %s has nativeKind %q, want %q", node.ID, node.NativeKind, pendingSymbolNativeKind)
		}
		if node.Target.Scope.Container == "" || node.Target.Scope.File != "" {
			t.Fatalf("semantic node %s is not container-scoped: %+v", node.ID, node.Target.Scope)
		}
		if node.Target.Key.QualifiedName == "" || node.Target.Key.Name != "" {
			t.Fatalf("semantic node %s is not qualifiedName-keyed: %+v", node.ID, node.Target.Key)
		}
		targets[node.ID] = node.Target.Scope.Container + "#" + node.Target.Key.QualifiedName
	}

	var out []string
	for _, edge := range diff.UpsertEdges {
		if edge.Source != sourceTierSemantic || edge.Engine != engineNameSemantic {
			t.Fatalf("edge %s is labelled %s/%s, want %s/%s", edge.ID, edge.Source, edge.Engine,
				sourceTierSemantic, engineNameSemantic)
		}
		if edge.Resolved {
			t.Fatalf("edge %s onto a placeholder claims resolved: true", edge.ID)
		}
		from, ok := labels[edge.FromID]
		if !ok {
			from = "<unknown " + edge.FromID + ">"
		}
		to, ok := targets[edge.ToID]
		if !ok {
			to = "<unknown " + edge.ToID + ">"
		}
		out = append(out, from+" "+edge.Kind+" "+to)
	}
	sort.Strings(out)
	return out
}

// The whole point of the tier, in one comparison: every shape the structural
// pass refused, answered exactly, and nothing else answered by accident.
func TestSemanticPassResolvesEveryOpenShape(t *testing.T) {
	requireGoToolchain(t)
	root := writeProbeProject(t)

	state := newPluginState(root)
	diff, incomplete := state.handleSemanticPass(nil)
	if incomplete {
		t.Fatalf("a whole-project pass with a toolchain present answered incomplete=true")
	}

	want := []string{
		// Implicit interface satisfaction - nothing in either type's syntax
		// mentions Closer.
		"api.go:Handle SUPERTYPE_OF example.com/probe#Closer",
		"api.go:Wrapper SUPERTYPE_OF example.com/probe#Closer",
		// A bare name a dot import brought in. The structural tier emits
		// nothing at all in a file carrying one.
		"dotuse.go:dotImported CALLS example.com/probe/dotted#DotFunc",
		// Two conversions written exactly like calls: REFERENCES, not CALLS.
		"use.go:convert REFERENCES example.com/probe#Kelvin",
		"use.go:convertCelsius REFERENCES example.com/probe/other#Celsius",
		// The three receiver shapes.
		"use.go:embeddedReceiver CALLS example.com/probe#Handle.Name",
		"use.go:interfaceReceiver CALLS example.com/probe#Closer.Close",
		// An import whose package clause disagrees with its directory, so
		// the structural tier could not tell `actual.Ping()` from `x.M()`.
		"use.go:misguessedImport CALLS example.com/probe/misnamed#Ping",
		"use.go:varReceiver CALLS example.com/probe#Handle.Name",
	}
	got := renderEdges(t, root, diff)
	if !equalStrings(got, want) {
		t.Fatalf("semantic pass produced\n  %s\nwant\n  %s",
			strings.Join(got, "\n  "), strings.Join(want, "\n  "))
	}
}

// The CALLS edges the structural tier emitted onto a *type* are retracted by
// id, because core's kind filter would never land them and an edge nothing
// can ever resolve is a claim about a call that does not happen.
func TestSemanticPassRetractsCallsOntoAType(t *testing.T) {
	requireGoToolchain(t)
	root := writeProbeProject(t)

	ws := loadWorkspace(root)
	content, err := os.ReadFile(filepath.Join(canonicalizeProjectRoot(root), "use.go"))
	if err != nil {
		t.Fatalf("read use.go: %v", err)
	}
	graph := extractFile(ws, "use.go", content)

	// What the structural tier claimed: a CALLS edge per conversion.
	structural := map[string]bool{}
	for _, edge := range graph.edges {
		if edge.Kind == edgeKindCalls {
			structural[edge.ID] = true
		}
	}
	converts := map[string]string{}
	for _, call := range graph.placeholderCalls {
		if call.Name == "Kelvin" || call.Name == "Celsius" {
			converts[call.Name] = call.EdgeID
		}
	}
	if len(converts) != 2 {
		t.Fatalf("expected the two conversions to be recorded as placeholder calls, got %+v", graph.placeholderCalls)
	}
	for name, id := range converts {
		if !structural[id] {
			t.Fatalf("the recorded edge id for %s(...) is not one the structural tier emitted", name)
		}
	}

	state := newPluginState(root)
	diff, _ := state.handleSemanticPass(nil)

	retracted := map[string]bool{}
	for _, id := range diff.DeleteEdgeIds {
		retracted[id] = true
	}
	for name, id := range converts {
		if !retracted[id] {
			t.Fatalf("the CALLS edge for the conversion %s(...) was not retracted; deletes were %v",
				name, diff.DeleteEdgeIds)
		}
	}
	// And nothing that *is* a call was retracted along with them.
	for _, call := range graph.placeholderCalls {
		if call.Name == "newHandle" && retracted[call.EdgeID] {
			t.Fatalf("a genuine call to newHandle() was retracted")
		}
	}
}

// Two runs of the same pass over the same tree must produce byte-identical
// diffs: every map in semantic.go is iterated through sortedKeys precisely
// so that a diff does not depend on the runtime's hash seed.
func TestSemanticPassIsDeterministic(t *testing.T) {
	requireGoToolchain(t)
	root := writeProbeProject(t)

	firstDiff, _ := newPluginState(root).handleSemanticPass(nil)
	secondDiff, _ := newPluginState(root).handleSemanticPass(nil)
	first := renderEdges(t, root, firstDiff)
	second := renderEdges(t, root, secondDiff)
	if !equalStrings(first, second) {
		t.Fatalf("two passes over one tree disagreed:\n  %s\nvs\n  %s",
			strings.Join(first, "\n  "), strings.Join(second, "\n  "))
	}
}

// selfImplementsFiles is a separate fixture from probeFiles, on purpose: an
// internal `_test.go` file in the same package as the interface it declares
// is exactly what makes `packages.Load(..., Tests: true)` type-check that
// package twice - once as `talks` and again as `talks [talks.test]` - so
// `Talker` surfaces as two distinct *types.TypeName objects for the one
// declaration. Folding probeFiles into the shared fixture would pull that
// duplication into every other test's exact edge comparison; this fixture
// exists so only this test pays for it.
var selfImplementsFiles = map[string]string{
	"go.mod": "module example.com/subprobe\n\ngo 1.22\n",

	"talk.go": `package subprobe

type Talker interface {
	Talk() string
}

type Loud struct{}

func (Loud) Talk() string { return "LOUD" }
`,

	// Nothing here even needs to mention Talker: an internal test file's
	// mere presence in the package is what forces the second, separately
	// type-checked package variant.
	"talk_test.go": `package subprobe

import "testing"

func TestLoudTalks(t *testing.T) {
	if (Loud{}).Talk() != "LOUD" {
		t.Fatal("Loud should talk LOUD")
	}
}
`,
}

func writeSelfImplementsProject(t *testing.T) string {
	t.Helper()
	root := t.TempDir()
	for name, contents := range selfImplementsFiles {
		path := filepath.Join(root, filepath.FromSlash(name))
		if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
			t.Fatalf("mkdir %s: %v", path, err)
		}
		if err := os.WriteFile(path, []byte(contents), 0o644); err != nil {
			t.Fatalf("write %s: %v", path, err)
		}
	}
	return root
}

// GM-362: an interface must never come back as its own implementor. Before
// the fix, `answerImplements`'s guard against a self edge compared
// `*types.TypeName` pointer identity, which only holds within one
// type-checking pass - it does not hold between a package's production
// variant and its `[pkg.test]` variant, which `Tests: true` type-checks
// separately. `Talker` (declared once, in talk.go) surfaces as two distinct
// TypeName objects because talk_test.go exists in the same package, both
// trivially satisfying `types.Implements` against each other, and both
// resolving back to the one real `Talker` node - producing
// "talk.go:Talker SUPERTYPE_OF example.com/subprobe#Talker" alongside
// the real "talk.go:Loud SUPERTYPE_OF ...". Disable the fix (revert the
// guard to `ifaceObj == subtype.obj`) and this test fails on exactly that
// extra row; the TypeScript equivalent (no test-variant duplication in that
// plugin) is covered by the control in GM-362's task description, not here.
func TestSemanticPassExcludesAnInterfaceFromItsOwnImplementors(t *testing.T) {
	requireGoToolchain(t)
	root := writeSelfImplementsProject(t)

	state := newPluginState(root)
	diff, _ := state.handleSemanticPass(nil)
	got := renderEdges(t, root, diff)
	want := []string{
		"talk.go:Loud SUPERTYPE_OF example.com/subprobe#Talker",
		// The test file's own call through a composite literal receiver -
		// present because talk_test.go is real, compiling Go, not a stub
		// that exists only to trigger the `[pkg.test]` variant.
		"talk_test.go:TestLoudTalks CALLS example.com/subprobe#Loud.Talk",
	}
	if !equalStrings(got, want) {
		t.Fatalf("semantic pass over a package with an internal _test.go file produced\n  %s\nwant\n  %s",
			strings.Join(got, "\n  "), strings.Join(want, "\n  "))
	}
}

// A per-file pass re-checks that file's package and nothing else - the
// design doc's "re-check that file's package, reusing loaded dependencies".
func TestSemanticPassPerFileAnswersOnlyThatFile(t *testing.T) {
	requireGoToolchain(t)
	root := writeProbeProject(t)

	state := newPluginState(root)
	diff, _ := state.handleSemanticPass([]string{"dotuse.go"})
	got := renderEdges(t, root, diff)
	want := []string{"dotuse.go:dotImported CALLS example.com/probe/dotted#DotFunc"}
	if !equalStrings(got, want) {
		t.Fatalf("per-file pass produced\n  %s\nwant\n  %s",
			strings.Join(got, "\n  "), strings.Join(want, "\n  "))
	}
}

// An edge this process emitted for a file, and did not produce again when
// that file was re-checked, is retracted by id - otherwise a long-lived
// daemon accumulates semantic edges out of declarations that have moved.
// Nothing else deletes them: the structural `fileChanged` diff only ever
// deletes edges it emitted itself, and it never emitted these.
func TestSemanticPassRetractsWhatAReCheckNoLongerProduces(t *testing.T) {
	requireGoToolchain(t)
	root := writeProbeProject(t)

	state := newPluginState(root)
	before, _ := state.handleSemanticPass(nil)

	var dotCall string
	for _, edge := range before.UpsertEdges {
		for _, node := range before.UpsertNodes {
			if node.ID == edge.ToID && node.Target.Scope.Container == "example.com/probe/dotted" {
				dotCall = edge.ID
			}
		}
	}
	if dotCall == "" {
		t.Fatal("the first pass did not emit the dot-imported call this test retracts")
	}

	// The dot import and the call it made possible are gone; the package
	// still compiles, so this is a re-check with a real answer, not a load
	// failure that would have answered empty for a different reason.
	rewritten := "package probe\n\nfunc dotImported() string {\n\treturn \"dot\"\n}\n"
	if err := os.WriteFile(filepath.Join(canonicalizeProjectRoot(root), "dotuse.go"), []byte(rewritten), 0o644); err != nil {
		t.Fatalf("rewrite dotuse.go: %v", err)
	}

	after, _ := state.handleSemanticPass([]string{"dotuse.go"})
	retracted := false
	for _, id := range after.DeleteEdgeIds {
		if id == dotCall {
			retracted = true
		}
	}
	if !retracted {
		t.Fatalf("the re-check did not retract the edge it no longer produces; deletes were %v",
			after.DeleteEdgeIds)
	}
	if len(after.UpsertEdges) != 0 {
		t.Fatalf("the re-checked file should produce no semantic edge at all now, got %+v", after.UpsertEdges)
	}
}

// The design doc's "Semantic engine missing" failure mode: no `go` binary,
// one log line, an empty diff answered `incomplete: true` (GM-384), and a
// structural graph that is completely untouched.
func TestSemanticPassWithoutAToolchainAnswersAnEmptyDiff(t *testing.T) {
	root := writeProbeProject(t)

	// Emptied rather than narrowed: `exec.LookPath` consults PATH only, so
	// this is exactly the "no toolchain installed" case and not an
	// approximation of it.
	t.Setenv("PATH", "")

	state := newPluginState(root)

	// The structural answer is unaffected - the point of degrading rather
	// than failing.
	structural := state.handleFileChanged("use.go")
	if len(structural.UpsertNodes) == 0 || len(structural.UpsertEdges) == 0 {
		t.Fatalf("the structural tier answered nothing without a toolchain: %+v", structural)
	}

	for _, filePaths := range [][]string{nil, {"use.go"}} {
		diff, incomplete := state.handleSemanticPass(filePaths)
		if len(diff.UpsertNodes) != 0 || len(diff.UpsertEdges) != 0 ||
			len(diff.DeleteNodeIds) != 0 || len(diff.DeleteEdgeIds) != 0 {
			t.Fatalf("semanticPass(%v) without a toolchain answered %+v, want an empty diff", filePaths, diff)
		}
		if !incomplete {
			t.Fatalf("semanticPass(%v) without a toolchain answered incomplete=false, want true - "+
				"otherwise core records the pass as done and never asks again", filePaths)
		}
	}
}

// The laziness the conformance kit's `capabilities.semantic-engine-lazy`
// check reads off the marker file: nothing that is not a `semanticPass`
// starts the engine.
func TestSemanticEngineMarkerIsWrittenOnlyByASemanticPass(t *testing.T) {
	requireGoToolchain(t)
	root := writeProbeProject(t)
	markers := t.TempDir()
	t.Setenv(markerDirEnv, markers)
	marker := filepath.Join(markers, semanticEngineMarker)

	state := newPluginState(root)
	if _, err := runBulkIndex(root, discardWriter{}); err != nil {
		t.Fatalf("bulk index: %v", err)
	}
	state.handleFileChanged("use.go")
	state.reloadWorkspace()
	if _, err := os.Stat(marker); !os.IsNotExist(err) {
		t.Fatalf("the semantic-engine marker exists after structural work alone (stat error: %v)", err)
	}

	state.handleSemanticPass(nil)
	if _, err := os.Stat(marker); err != nil {
		t.Fatalf("the semantic-engine marker was not written by a semanticPass: %v", err)
	}
}

type discardWriter struct{}

func (discardWriter) Write(p []byte) (int, error) { return len(p), nil }
