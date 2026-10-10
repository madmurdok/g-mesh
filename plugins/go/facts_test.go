package main

// GM-545: what a go.mod/go.work save changed for resolution (facts.go), and
// how the control plane acts on it (control.go's handleResolutionChanged).
// Every test writes a tree, records its facts as the bulk walk would, edits
// go.mod/go.work, and compares against the reloaded workspace.

import (
	"bufio"
	"bytes"
	"encoding/json"
	"io"
	"os"
	"path/filepath"
	"reflect"
	"strings"
	"testing"
)

const (
	appMod   = "module example.com/app\n\ngo 1.22\n"
	toolsMod = "module example.com/tools\n\ngo 1.22\n"
	appWork  = "go 1.22\n\nuse (\n\t.\n\t./tools\n)\n"
)

// twoModuleTree is a root module example.com/app with a nested
// example.com/tools at tools/, both used by go.work.
func twoModuleTree(t *testing.T) string {
	t.Helper()
	root := t.TempDir()
	writeFile(t, root, "go.mod", appMod)
	writeFile(t, root, "go.work", appWork)
	writeFile(t, root, "tools/go.mod", toolsMod)
	writeFile(t, root, "main.go", "package main\n\nimport _ \"example.com/tools/gen\"\n")
	writeFile(t, root, "tools/gen/gen.go", "package gen\n\nfunc Gen() {}\n")
	return root
}

// factsNow is the blob core would hold for root's current layout.
func factsNow(root string) *string {
	blob := encodeFacts(loadWorkspace(root))
	return &blob
}

// deltaAfter is the answer to a save of root after edit, against the facts
// recorded before it.
func deltaAfter(root string, edit func()) resolutionDelta {
	previous := factsNow(root)
	edit()
	return resolutionDeltaFor(previous, loadWorkspace(root))
}

func selectorPrefixes(delta resolutionDelta) []string {
	var prefixes []string
	for _, selector := range delta.Imports {
		if selector.Importers.Under != "" || selector.Importers.NotUnder != nil {
			panic("an importer scope other than the whole project")
		}
		if selector.By.Specifier.Under.Separator != "/" {
			panic("a specifier separator other than /")
		}
		prefixes = append(prefixes, selector.By.Specifier.Under.Prefix)
	}
	return prefixes
}

// B1: the directives extraction never reads answer `unchanged`, including
// the plugins-check version bump (`go 1.22` -> `go 1.22.0`).
//
// Control: read `require` instead of `replace` in readReplaces (the require
// edits become replace facts, so the delta is `affected`).
func TestRequireGoToolchainExcludeAndRetractEditsAnswerUnchanged(t *testing.T) {
	edits := map[string]string{
		"go version bump": "module example.com/app\n\ngo 1.22.0\n",
		"every directive": "module example.com/app\n\ngo 1.23\n\ntoolchain go1.23.1\n\n" +
			"require example.com/one v1.0.0\n\nrequire (\n\texample.com/two v1.2.0\n\texample.com/three v0.1.0 // indirect\n)\n\n" +
			"exclude example.com/one v0.9.0\n\nretract v0.0.1\n",
	}
	for name, edited := range edits {
		t.Run(name, func(t *testing.T) {
			root := twoModuleTree(t)
			writeFile(t, root, "go.mod", "module example.com/app\n\ngo 1.22\n\nrequire example.com/one v0.9.0\n")
			got := deltaAfter(root, func() {
				writeFile(t, root, "go.mod", edited)
				writeFile(t, root, "tools/go.mod", toolsMod+"\nrequire example.com/four v2.0.0+incompatible\n")
				writeFile(t, root, "go.work", "go 1.23\n\ntoolchain go1.23.1\n\nuse (\n\t.\n\t./tools\n)\n")
			})
			if !reflect.DeepEqual(got, unchangedDelta()) {
				t.Fatalf("delta = %+v, want unchanged", got)
			}
		})
	}
}

// B2: a module path change at tools/ selects the files under tools/ but not
// under a deeper module dir, and the importers of the old and the new path.
//
// Control: compute the dir's key only from newByDir in factsDelta (the old
// path `example.com/tools` is no longer selected).
func TestAModulePathChangeSelectsItsFilesAndTheImportersOfBothPaths(t *testing.T) {
	root := twoModuleTree(t)
	writeFile(t, root, "tools/sub/go.mod", "module example.com/sub\n")
	got := deltaAfter(root, func() {
		writeFile(t, root, "tools/go.mod", "module example.com/tools2\n\ngo 1.22\n")
	})
	want := resolutionDelta{
		Kind:  "affected",
		Files: []pathScope{{Under: "tools", NotUnder: []string{"tools/sub"}}},
		Imports: []importSelector{
			specifierUnder("example.com/tools"),
			specifierUnder("example.com/tools2"),
		},
	}
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("delta = %+v\nwant  %+v", got, want)
	}
}

// B3: a nested module added at tools/sub, or removed from it, re-keys that
// subtree; its key before/after is `example.com/tools/sub` (the enclosing
// module plus the dir) on one side and `example.com/sub` on the other, and
// both are selected.
//
// Control: make importPathIn answer false for a dir below its innermost
// module (`example.com/tools/sub` is no longer selected).
func TestANestedModuleAddedOrRemovedSelectsTheEnclosingAndItsOwnKey(t *testing.T) {
	want := resolutionDelta{
		Kind:  "affected",
		Files: []pathScope{{Under: "tools/sub"}},
		Imports: []importSelector{
			specifierUnder("example.com/sub"),
			specifierUnder("example.com/tools/sub"),
		},
	}
	t.Run("added", func(t *testing.T) {
		root := twoModuleTree(t)
		got := deltaAfter(root, func() { writeFile(t, root, "tools/sub/go.mod", "module example.com/sub\n") })
		if !reflect.DeepEqual(got, want) {
			t.Fatalf("delta = %+v\nwant  %+v", got, want)
		}
	})
	t.Run("removed", func(t *testing.T) {
		root := twoModuleTree(t)
		writeFile(t, root, "tools/sub/go.mod", "module example.com/sub\n")
		got := deltaAfter(root, func() { removeFile(t, root, "tools/sub/go.mod") })
		if !reflect.DeepEqual(got, want) {
			t.Fatalf("delta = %+v\nwant  %+v", got, want)
		}
	})
}

// B4: when every module dir of the project changed, the answer is `unknown`
// (a whole-language reindex), not a selection of everything.
//
// Control: drop the every-module check in factsDelta (the delta is
// `affected`).
func TestEveryModuleChangedAnswersUnknown(t *testing.T) {
	cases := map[string]struct {
		before map[string]string
		edit   func(t *testing.T, root string)
	}{
		"single-module rename": {
			before: map[string]string{"go.mod": appMod},
			edit:   func(t *testing.T, root string) { writeFile(t, root, "go.mod", "module example.com/renamed\n") },
		},
		"first go.mod": {
			before: map[string]string{"a/a.go": "package a\n"},
			edit:   func(t *testing.T, root string) { writeFile(t, root, "go.mod", appMod) },
		},
		"last go.mod": {
			before: map[string]string{"go.mod": appMod},
			edit:   func(t *testing.T, root string) { removeFile(t, root, "go.mod") },
		},
		"both modules renamed": {
			before: map[string]string{"go.mod": appMod, "tools/go.mod": toolsMod},
			edit: func(t *testing.T, root string) {
				writeFile(t, root, "go.mod", "module example.com/app2\n")
				writeFile(t, root, "tools/go.mod", "module example.com/tools2\n")
			},
		},
	}
	for name, c := range cases {
		t.Run(name, func(t *testing.T) {
			root := t.TempDir()
			for path, content := range c.before {
				writeFile(t, root, path, content)
			}
			got := deltaAfter(root, func() { c.edit(t, root) })
			if got.Kind != "unknown" || !strings.Contains(got.Reason, "every module of the project changed") {
				t.Fatalf("delta = %+v, want unknown because every module changed", got)
			}
			if got.Files != nil || got.Imports != nil {
				t.Fatalf("an unknown delta carries no selection: %+v", got)
			}
		})
	}
}

// B5: a changed set of `replace` directives for module path P, in a go.mod
// or in go.work, selects the importers of P and no files. A reformatted
// directive is the same rule.
//
// Control: drop the replaces and uses loops in factsDelta (both answer
// `unchanged`); shared with B6.
func TestAChangedReplaceSelectsTheImportersOfTheReplacedModule(t *testing.T) {
	ext := resolutionDelta{Kind: "affected", Imports: []importSelector{specifierUnder("example.com/ext")}}
	cases := map[string]struct {
		file, before, after string
		want                resolutionDelta
	}{
		"added in go.mod": {
			file: "go.mod", before: appMod, after: appMod + "\nreplace example.com/ext => ../ext\n", want: ext,
		},
		"changed in a block": {
			file:   "go.mod",
			before: appMod + "\nreplace (\n\texample.com/ext => ../ext\n\texample.com/other => ../other\n)\n",
			after:  appMod + "\nreplace (\n\texample.com/ext v1.2.3 => ../ext\n\texample.com/other => ../other\n)\n",
			want:   ext,
		},
		"added in go.work": {
			file: "go.work", before: appWork, after: appWork + "\nreplace example.com/ext => ./ext\n", want: ext,
		},
		"removed from the nested module": {
			file: "tools/go.mod", before: toolsMod + "\nreplace example.com/ext => ../ext\n", after: toolsMod, want: ext,
		},
		"reformatted": {
			file:   "go.mod",
			before: appMod + "\nreplace example.com/ext => ../ext\n",
			after:  appMod + "\nreplace   example.com/ext   =>   ../ext   // moved\n",
			want:   unchangedDelta(),
		},
	}
	for name, c := range cases {
		t.Run(name, func(t *testing.T) {
			root := twoModuleTree(t)
			writeFile(t, root, c.file, c.before)
			got := deltaAfter(root, func() { writeFile(t, root, c.file, c.after) })
			if !reflect.DeepEqual(got, c.want) {
				t.Fatalf("delta = %+v\nwant  %+v", got, c.want)
			}
		})
	}
}

// B6: a go.work `use` dir added or removed selects the importers of that
// dir's module path and no files (the module itself is still found by the
// walk, so no container key moves).
//
// Control: drop the replaces and uses loops in factsDelta (shared with B5).
func TestAChangedGoWorkUseSelectsTheImportersOfThatModule(t *testing.T) {
	want := resolutionDelta{Kind: "affected", Imports: []importSelector{specifierUnder("example.com/tools")}}
	single := "go 1.22\n\nuse .\n"
	t.Run("removed", func(t *testing.T) {
		root := twoModuleTree(t)
		got := deltaAfter(root, func() { writeFile(t, root, "go.work", single) })
		if !reflect.DeepEqual(got, want) {
			t.Fatalf("delta = %+v\nwant  %+v", got, want)
		}
	})
	t.Run("added", func(t *testing.T) {
		root := twoModuleTree(t)
		writeFile(t, root, "go.work", single)
		got := deltaAfter(root, func() { writeFile(t, root, "go.work", appWork) })
		if !reflect.DeepEqual(got, want) {
			t.Fatalf("delta = %+v\nwant  %+v", got, want)
		}
	})
}

// B7: with no previous facts, or facts this version cannot read, the answer
// is `unknown`.
//
// Control: drop the `facts.Format != factsFormat` check in decodeFacts (the
// other-format blob describing the same layout answers `unchanged`).
func TestAbsentOrUnreadablePreviousFactsAnswerUnknown(t *testing.T) {
	root := twoModuleTree(t)
	ws := loadWorkspace(root)
	if got := resolutionDeltaFor(nil, ws); !reflect.DeepEqual(got, unknownDelta("no previous resolution facts")) {
		t.Fatalf("absent facts: delta = %+v", got)
	}
	otherFormat := strings.Replace(encodeFacts(ws), `"format":1`, `"format":2`, 1)
	if otherFormat == encodeFacts(ws) {
		t.Fatalf("the facts blob no longer spells its format as \"format\":1: %s", otherFormat)
	}
	for _, blob := range []string{"", "not json", `{"modules":[]}`, otherFormat} {
		blob := blob
		got := resolutionDeltaFor(&blob, ws)
		if got.Kind != "unknown" || !strings.Contains(got.Reason, "unreadable") {
			t.Fatalf("facts %q: delta = %+v, want unknown (unreadable)", blob, got)
		}
	}
	// The same layout's own facts are readable and unchanged: the cases
	// above fail on the blob, not on the layout.
	if got := resolutionDeltaFor(factsNow(root), ws); !reflect.DeepEqual(got, unchangedDelta()) {
		t.Fatalf("own facts: delta = %+v, want unchanged", got)
	}
}

// B8: handleResolutionChanged always adopts the new layout and returns its
// facts; only `unknown` drops the cached graphs.
//
// Control: drop the `delta.Kind == "unknown"` condition in
// handleResolutionChanged so every answer clears the cache (the cache is
// empty after the `affected` and `unchanged` answers).
func TestOnlyAnUnknownResolutionAnswerDropsTheCacheAndTheLayoutIsAlwaysAdopted(t *testing.T) {
	root := twoModuleTree(t)
	state := newPluginState(root)
	state.handleFileChanged("main.go")
	state.handleFileChanged("tools/gen/gen.go")
	warm := []string{"main.go", "tools/gen/gen.go"}

	// unchanged
	previous := factsNow(root)
	writeFile(t, root, "go.mod", appMod+"\nrequire example.com/one v1.0.0\n")
	result := state.handleResolutionChanged(previous)
	if result.Delta.Kind != "unchanged" || !reflect.DeepEqual(cachedPaths(state), warm) {
		t.Fatalf("unchanged: delta = %+v, cache = %v, want the cache kept", result.Delta, cachedPaths(state))
	}
	if result.Facts != *factsNow(root) {
		t.Fatalf("facts = %s, want the reloaded layout's %s", result.Facts, *factsNow(root))
	}

	// affected: the cache is kept, and the next fileChanged re-keys the
	// moved file under the adopted layout.
	previous = factsNow(root)
	writeFile(t, root, "tools/go.mod", "module example.com/tools2\n")
	result = state.handleResolutionChanged(previous)
	if result.Delta.Kind != "affected" || !reflect.DeepEqual(cachedPaths(state), warm) {
		t.Fatalf("affected: delta = %+v, cache = %v, want the cache kept", result.Delta, cachedPaths(state))
	}
	if result.Facts != *factsNow(root) || !strings.Contains(result.Facts, "example.com/tools2") {
		t.Fatalf("facts = %s, want the reloaded layout's", result.Facts)
	}
	moved := state.handleFileChanged("tools/gen/gen.go")
	if got := containerOf(t, moved, "Gen"); got != "example.com/tools2/gen" {
		t.Fatalf("container after the rename = %q, want example.com/tools2/gen", got)
	}

	// unknown
	result = state.handleResolutionChanged(nil)
	if result.Delta.Kind != "unknown" || len(cachedPaths(state)) != 0 {
		t.Fatalf("unknown: delta = %+v, cache = %v, want it dropped", result.Delta, cachedPaths(state))
	}
	if result.Facts != *factsNow(root) {
		t.Fatalf("an unknown answer still carries the reloaded facts: %s", result.Facts)
	}
}

// B8 on the wire: a resolutionChanged request is answered with its delta and
// facts under the request's id; B11: a fileChanged with `reextract` is
// accepted and answered like one without it.
//
// Control: shared with B8.
func TestResolutionChangedIsAnsweredOnTheWire(t *testing.T) {
	root := twoModuleTree(t)
	state := newPluginState(root)
	previous := factsNow(root)
	writeFile(t, root, "go.mod", appMod+"\nreplace example.com/ext => ../ext\n")
	params, err := json.Marshal(resolutionChangedParams{FilePath: "go.mod", PreviousFacts: previous})
	if err != nil {
		t.Fatal(err)
	}
	var out bytes.Buffer
	handleEnvelope(state, controlEnvelope{
		JSONRPC: jsonrpcVersion, ID: json.RawMessage(`7`), Method: "resolutionChanged", Params: params,
	}, &out)
	handleEnvelope(state, controlEnvelope{
		JSONRPC: jsonrpcVersion, ID: json.RawMessage(`8`), Method: "fileChanged",
		Params: json.RawMessage(`{"filePath":"main.go","reextract":true}`),
	}, &out)

	frames := readFramesFrom(t, &out)
	if len(frames) != 2 {
		t.Fatalf("frames = %v, want two answers", frames)
	}
	var answer struct {
		ID     int                     `json:"id"`
		Result resolutionChangedResult `json:"result"`
	}
	if err := json.Unmarshal(frames[0], &answer); err != nil {
		t.Fatalf("resolutionChanged answer: %v\n%s", err, frames[0])
	}
	want := resolutionDelta{Kind: "affected", Imports: []importSelector{specifierUnder("example.com/ext")}}
	if answer.ID != 7 || !reflect.DeepEqual(answer.Result.Delta, want) || answer.Result.Facts != *factsNow(root) {
		t.Fatalf("answer = %s", frames[0])
	}
	var changed struct {
		ID     int            `json:"id"`
		Result fileChangeDiff `json:"result"`
	}
	if err := json.Unmarshal(frames[1], &changed); err != nil || changed.ID != 8 {
		t.Fatalf("fileChanged answer: %v\n%s", err, frames[1])
	}
	if !reflect.DeepEqual(upsertedPaths(changed.Result), []string{"main.go"}) {
		t.Fatalf("a reextract fileChanged answers like any other: %s", frames[1])
	}
}

// B9: the bulk walk's last line is the facts of the layout it walked.
//
// Control: drop the bulkFactsLine write in runBulkIndex (the last line is a
// node or an edge).
func TestRunBulkIndexEndsWithTheWalkedLayoutsFacts(t *testing.T) {
	root := twoModuleTree(t)
	writeFile(t, root, "go.mod", appMod+"\nreplace example.com/ext => ../ext\n")
	var out bytes.Buffer
	if _, err := runBulkIndex(root, &out); err != nil {
		t.Fatalf("runBulkIndex: %v", err)
	}
	lines := strings.Split(strings.TrimRight(out.String(), "\n"), "\n")
	var last struct {
		ResolutionFacts *string `json:"resolutionFacts"`
	}
	if err := json.Unmarshal([]byte(lines[len(lines)-1]), &last); err != nil || last.ResolutionFacts == nil {
		t.Fatalf("last line %q is not the facts trailer (%v)", lines[len(lines)-1], err)
	}
	if *last.ResolutionFacts != *factsNow(root) {
		t.Fatalf("trailer facts = %s, want %s", *last.ResolutionFacts, *factsNow(root))
	}
	facts, ok := decodeFacts(*last.ResolutionFacts)
	want := []factModule{{Dir: "", Path: "example.com/app"}, {Dir: "tools", Path: "example.com/tools"}}
	if !ok || !reflect.DeepEqual(facts.Modules, want) || !reflect.DeepEqual(facts.Uses, []string{"", "tools"}) ||
		len(facts.Replaces) != 1 || facts.Replaces[0].From != "example.com/ext" {
		t.Fatalf("trailer facts = %+v", facts)
	}
	for _, line := range lines[:len(lines)-1] {
		if strings.Contains(line, `"resolutionFacts"`) {
			t.Fatalf("a facts line before the end: %s", line)
		}
	}
}

// B10: every IMPORTS edge carries its import path as written; no other edge
// carries one, and the edge id does not depend on it.
//
// Control: pass "" as the specifier in addImportEdge.
func TestImportEdgesCarryTheirImportPath(t *testing.T) {
	graph := extract(t, "cmd/main.go",
		"package main\n\nimport (\n\t\"fmt\"\n\tsrv \""+serverPkg+"\"\n\t_ \"example.com/ext/x\"\n)\n\n"+
			"func main() { fmt.Println(); srv.Run() }\n")
	var specifiers []string
	for _, edge := range graph.edges {
		if edge.ID != edgeIDFor(edge.FromID, edge.Kind, edge.ToID, nil) {
			t.Fatalf("edge %+v: id is not edgeIDFor(from, kind, to)", edge)
		}
		if edge.Kind != edgeKindImports {
			if edge.Specifier != "" {
				t.Fatalf("a %s edge carries a specifier: %+v", edge.Kind, edge)
			}
			continue
		}
		specifiers = append(specifiers, edge.Specifier)
	}
	sortStrings(specifiers)
	want := []string{"example.com/ext/x", "fmt", serverPkg}
	if !reflect.DeepEqual(specifiers, want) {
		t.Fatalf("IMPORTS specifiers = %v, want %v", specifiers, want)
	}
}

func removeFile(t *testing.T, root, rel string) {
	t.Helper()
	if err := os.Remove(filepath.Join(root, filepath.FromSlash(rel))); err != nil {
		t.Fatalf("remove %s: %v", rel, err)
	}
}

// readFramesFrom returns every frame's raw body in buf.
func readFramesFrom(t *testing.T, buf io.Reader) [][]byte {
	t.Helper()
	var bodies [][]byte
	reader := bufio.NewReader(buf)
	for {
		body, err := readFrame(reader)
		if err != nil {
			return bodies
		}
		bodies = append(bodies, body)
	}
}
