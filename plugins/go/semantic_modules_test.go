package main

// GM-442: a whole-project pass loads each module root on its own, and the
// pass used to call itself incomplete only when *every* module failed. One
// failed module among several answered complete, so core's post-pass sweep
// then deleted that module's semantic edges as stale.

import (
	"bufio"
	"bytes"
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

// moduleSource is one module's only file: a receiver call, which only the
// semantic tier resolves, so each module that loads contributes exactly one
// edge.
func moduleSource(pkg string) string {
	return "package " + pkg + "\n\n" +
		"type T struct{}\n\n" +
		"func (T) M() string { return \"\" }\n\n" +
		"func use() string {\n\tvar t T\n\treturn t.M()\n}\n"
}

// brokenGoMod is a go.mod `go list` refuses outright ("unknown directive"),
// while this plugin's own go.mod reader still finds its module line - so the
// module is in the pass's scope and its load is what fails.
const brokenGoMod = "module example.com/broken\n\ngo 1.22\n\nnot a directive\n"

const healthyBrokenGoMod = "module example.com/broken\n\ngo 1.22\n"

// writeMultiModuleProject lays out three modules - the root, `good/` and
// `broken/` - with `broken/go.mod` as given.
func writeMultiModuleProject(t *testing.T, brokenMod string) string {
	t.Helper()
	root := t.TempDir()
	files := map[string]string{
		"go.mod":        "module example.com/root\n\ngo 1.22\n",
		"root.go":       moduleSource("root"),
		"good/go.mod":   "module example.com/good\n\ngo 1.22\n",
		"good/good.go":  moduleSource("good"),
		"broken/go.mod": brokenMod,
		"broken/bad.go": moduleSource("broken"),
	}
	for name, contents := range files {
		writeFixtureFile(t, root, name, contents)
	}
	return root
}

func writeFixtureFile(t *testing.T, root, name, contents string) {
	t.Helper()
	path := filepath.Join(root, filepath.FromSlash(name))
	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		t.Fatalf("mkdir %s: %v", path, err)
	}
	if err := os.WriteFile(path, []byte(contents), 0o644); err != nil {
		t.Fatalf("write %s: %v", path, err)
	}
}

// semanticPassOnTheWire drives a whole-project pass through handleEnvelope
// and returns the one response frame - what core actually reads.
func semanticPassOnTheWire(t *testing.T, state *pluginState) map[string]interface{} {
	t.Helper()
	var out bytes.Buffer
	handleEnvelope(state, controlEnvelope{
		JSONRPC: jsonrpcVersion,
		ID:      json.RawMessage("1"),
		Method:  "semanticPass",
		Params:  json.RawMessage(`{"filePaths":[]}`),
	}, &out)
	frames := readFrames(t, bufio.NewReader(&out))
	if len(frames) != 1 {
		t.Fatalf("got %d response frame(s), want 1: %+v", len(frames), frames)
	}
	return frames[0]
}

const (
	rootEdge   = "root.go:use CALLS example.com/root#T.M"
	goodEdge   = "good/good.go:use CALLS example.com/good#T.M"
	brokenEdge = "broken/bad.go:use CALLS example.com/broken#T.M"
)

func TestSemanticPassWithOneFailedModuleIsIncomplete(t *testing.T) {
	requireGoToolchain(t)
	root := writeMultiModuleProject(t, brokenGoMod)
	state := newPluginState(root)

	diff, reason := state.handleSemanticPass(nil)
	if reason == "" {
		t.Fatalf("a whole-project pass with one module failing to load answered complete - " +
			"core would then sweep that module's semantic edges away as stale")
	}
	if !strings.Contains(reason, "broken") {
		t.Errorf("reason %q does not name the module that failed (broken)", reason)
	}
	if strings.Contains(reason, "good") {
		t.Errorf("reason %q names a module that loaded (good)", reason)
	}
	// The modules that did load are still answered.
	got := renderEdges(t, root, diff)
	if want := []string{goodEdge, rootEdge}; !equalStrings(got, want) {
		t.Errorf("edges = %v, want %v", got, want)
	}

	// And the wire says so, which is what core branches on.
	frame := semanticPassOnTheWire(t, newPluginState(root))
	if incomplete, _ := frame["incomplete"].(bool); !incomplete {
		t.Fatalf("response %+v lacks \"incomplete\":true", frame)
	}
	if wireReason, _ := frame["incompleteReason"].(string); !strings.Contains(wireReason, "broken") {
		t.Fatalf("response %+v: incompleteReason does not name the failed module", frame)
	}
}

func TestSemanticPassWithEveryModuleLoadedIsComplete(t *testing.T) {
	requireGoToolchain(t)
	root := writeMultiModuleProject(t, healthyBrokenGoMod)
	state := newPluginState(root)

	diff, reason := state.handleSemanticPass(nil)
	if reason != "" {
		t.Fatalf("a whole-project pass where every module loads answered incomplete: %s", reason)
	}
	got := renderEdges(t, root, diff)
	if want := []string{brokenEdge, goodEdge, rootEdge}; !equalStrings(got, want) {
		t.Errorf("edges = %v, want %v", got, want)
	}

	frame := semanticPassOnTheWire(t, newPluginState(root))
	if _, present := frame["incomplete"]; present {
		t.Errorf("a complete pass carried \"incomplete\" on the wire: %+v", frame)
	}
	if _, present := frame["incompleteReason"]; present {
		t.Errorf("a complete pass carried \"incompleteReason\" on the wire: %+v", frame)
	}
}

// A module that stops loading between two passes keeps what the earlier pass
// gave it: the later pass has no type information for its files, so it must
// not retract their edges.
func TestSemanticPassDoesNotRetractAFailedModulesEdges(t *testing.T) {
	requireGoToolchain(t)
	root := writeMultiModuleProject(t, healthyBrokenGoMod)
	state := newPluginState(root)

	before, reason := state.handleSemanticPass(nil)
	if reason != "" {
		t.Fatalf("first pass answered incomplete: %s", reason)
	}
	var brokenEdgeID string
	for _, edge := range before.UpsertEdges {
		single := fileChangeDiff{UpsertNodes: before.UpsertNodes, UpsertEdges: []wireEdge{edge}}
		if rendered := renderEdges(t, root, single); len(rendered) == 1 && rendered[0] == brokenEdge {
			brokenEdgeID = edge.ID
		}
	}
	if brokenEdgeID == "" {
		t.Fatalf("first pass did not produce %s", brokenEdge)
	}

	writeFixtureFile(t, root, "broken/go.mod", brokenGoMod)
	after, reason := state.handleSemanticPass(nil)
	if !strings.Contains(reason, "broken") {
		t.Fatalf("second pass reason = %q, want it to name the broken module", reason)
	}
	for _, id := range after.DeleteEdgeIds {
		if id == brokenEdgeID {
			t.Fatalf("the pass retracted %s although its module failed to load", brokenEdge)
		}
	}
}
