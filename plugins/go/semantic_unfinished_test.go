package main

// A semantic pass names the files it did not answer (`unfinishedFiles`) once
// at least one module loaded: the failed modules' files, possibly none. When
// nothing loaded, or the scope was empty, it names none and the field is
// absent, so core keeps retrying the whole scope.

import (
	"bufio"
	"bytes"
	"encoding/json"
	"reflect"
	"testing"
)

// semanticPassFrame drives a pass over `filePaths` through handleEnvelope
// and returns the one response frame.
func semanticPassFrame(t *testing.T, state *pluginState, filePaths []string) map[string]interface{} {
	t.Helper()
	if filePaths == nil {
		filePaths = []string{}
	}
	params, err := json.Marshal(map[string]interface{}{"filePaths": filePaths})
	if err != nil {
		t.Fatalf("marshal params: %v", err)
	}
	var out bytes.Buffer
	handleEnvelope(state, controlEnvelope{
		JSONRPC: jsonrpcVersion,
		ID:      json.RawMessage("1"),
		Method:  "semanticPass",
		Params:  params,
	}, &out)
	frames := readFrames(t, bufio.NewReader(&out))
	if len(frames) != 1 {
		t.Fatalf("got %d response frame(s), want 1: %+v", len(frames), frames)
	}
	return frames[0]
}

// wireUnfinished reads `unfinishedFiles` off a frame: present reports
// whether the key is there at all.
func wireUnfinished(t *testing.T, frame map[string]interface{}) (files []string, present bool) {
	t.Helper()
	raw, present := frame["unfinishedFiles"]
	if !present {
		return nil, false
	}
	list, ok := raw.([]interface{})
	if !ok {
		t.Fatalf("unfinishedFiles is %T, want a list: %+v", raw, frame)
	}
	files = []string{}
	for _, item := range list {
		name, ok := item.(string)
		if !ok {
			t.Fatalf("unfinishedFiles holds %T, want strings: %+v", item, frame)
		}
		files = append(files, name)
	}
	return files, true
}

// G1: one failed module of three on a whole-project pass. The pass is
// incomplete, names exactly the broken module's file, and still answers
// the modules that loaded.
func TestWholeProjectPassNamesTheFailedModulesFilesAsUnfinished(t *testing.T) {
	requireGoToolchain(t)
	root := writeMultiModuleProject(t, brokenGoMod)

	diff, reason, unfinished := newPluginState(root).handleSemanticPass(nil)
	if reason == "" {
		t.Fatalf("one module failed to load, yet the pass answered complete")
	}
	if want := []string{"broken/bad.go"}; !reflect.DeepEqual(unfinished, want) {
		t.Fatalf("unfinished = %#v, want %#v", unfinished, want)
	}
	if got, want := renderEdges(t, root, diff), []string{goodEdge, rootEdge}; !equalStrings(got, want) {
		t.Errorf("edges = %v, want %v", got, want)
	}

	files, present := wireUnfinished(t, semanticPassFrame(t, newPluginState(root), nil))
	if !present || !reflect.DeepEqual(files, []string{"broken/bad.go"}) {
		t.Fatalf("wire unfinishedFiles = %#v (present %v), want [broken/bad.go]", files, present)
	}
}

// G2: a per-file pass naming one file in a loaded module and one in the
// failed module names only the failed one.
func TestPerFilePassNamesOnlyTheFailedModulesFileAsUnfinished(t *testing.T) {
	requireGoToolchain(t)
	root := writeMultiModuleProject(t, brokenGoMod)

	_, reason, unfinished := newPluginState(root).handleSemanticPass([]string{"good/good.go", "broken/bad.go"})
	if reason == "" {
		t.Fatalf("the per-file pass's broken module failed to load, yet it answered complete")
	}
	if want := []string{"broken/bad.go"}; !reflect.DeepEqual(unfinished, want) {
		t.Fatalf("unfinished = %#v, want %#v", unfinished, want)
	}
}

// G3: every module loaded. The list is empty but present, on the wire as
// `"unfinishedFiles":[]`, so core settles every file it sent.
func TestAPassWhereEveryModuleLoadedSendsAnEmptyUnfinishedList(t *testing.T) {
	requireGoToolchain(t)
	root := writeMultiModuleProject(t, healthyBrokenGoMod)

	for _, filePaths := range [][]string{nil, {"good/good.go", "broken/bad.go"}} {
		_, reason, unfinished := newPluginState(root).handleSemanticPass(filePaths)
		if reason != "" {
			t.Fatalf("pass %v: every module loads, yet it answered incomplete: %s", filePaths, reason)
		}
		if unfinished == nil || len(unfinished) != 0 {
			t.Fatalf("pass %v: unfinished = %#v, want a non-nil empty list", filePaths, unfinished)
		}

		frame := semanticPassFrame(t, newPluginState(root), filePaths)
		files, present := wireUnfinished(t, frame)
		if !present || len(files) != 0 {
			t.Fatalf("pass %v: wire unfinishedFiles = %#v (present %v), want []: %+v",
				filePaths, files, present, frame)
		}
	}
}

// G4: nothing loaded (the only module is broken), or the scope is empty.
// No list, and no key on the wire.
func TestAPassThatLoadedNothingOrHadNoScopeNamesNoUnfinishedFiles(t *testing.T) {
	requireGoToolchain(t)
	nothingLoads := t.TempDir()
	writeFixtureFile(t, nothingLoads, "go.mod", brokenGoMod)
	writeFixtureFile(t, nothingLoads, "bad.go", moduleSource("broken"))
	noGoFiles := t.TempDir()
	writeFixtureFile(t, noGoFiles, "go.mod", "module example.com/empty\n\ngo 1.22\n")

	cases := []struct {
		name       string
		root       string
		filePaths  []string
		incomplete bool
	}{
		{"every module broken, whole project", nothingLoads, nil, true},
		{"every module broken, per file", nothingLoads, []string{"bad.go"}, true},
		{"empty whole-project scope", noGoFiles, nil, false},
		{"empty per-file scope", nothingLoads, []string{"README.md"}, false},
	}
	for _, tc := range cases {
		_, reason, unfinished := newPluginState(tc.root).handleSemanticPass(tc.filePaths)
		if (reason != "") != tc.incomplete {
			t.Errorf("%s: reason = %q, want incomplete = %v", tc.name, reason, tc.incomplete)
		}
		if unfinished != nil {
			t.Errorf("%s: unfinished = %#v, want nil", tc.name, unfinished)
		}
		frame := semanticPassFrame(t, newPluginState(tc.root), tc.filePaths)
		if _, present := frame["unfinishedFiles"]; present {
			t.Errorf("%s: the frame carries unfinishedFiles: %+v", tc.name, frame)
		}
	}
}

// G4, without a toolchain: no list either.
func TestAPassWithoutAToolchainNamesNoUnfinishedFiles(t *testing.T) {
	root := writeMultiModuleProject(t, brokenGoMod)
	t.Setenv("PATH", "")

	for _, filePaths := range [][]string{nil, {"good/good.go"}} {
		_, reason, unfinished := newPluginState(root).handleSemanticPass(filePaths)
		if reason == "" {
			t.Fatalf("pass %v without a toolchain answered complete", filePaths)
		}
		if unfinished != nil {
			t.Fatalf("pass %v without a toolchain: unfinished = %#v, want nil", filePaths, unfinished)
		}
	}
}

// G5: writeResult omits the key for nil, sends `[]` for an empty list and
// the names for a non-empty one; a fileChanged response never carries it.
func TestWriteResultSendsUnfinishedFilesOnlyWhenNonNil(t *testing.T) {
	cases := []struct {
		name       string
		unfinished []string
		present    bool
		want       []string
	}{
		{"nil", nil, false, nil},
		{"empty", []string{}, true, []string{}},
		{"one file", []string{"a.go"}, true, []string{"a.go"}},
	}
	for _, tc := range cases {
		var out bytes.Buffer
		writeResult(&out, json.RawMessage("1"), emptyDiff(), "", tc.unfinished)
		frames := readFrames(t, bufio.NewReader(&out))
		if len(frames) != 1 {
			t.Fatalf("%s: got %d frame(s), want 1", tc.name, len(frames))
		}
		files, present := wireUnfinished(t, frames[0])
		if present != tc.present || !reflect.DeepEqual(files, tc.want) {
			t.Errorf("%s: unfinishedFiles = %#v (present %v), want %#v (present %v): %+v",
				tc.name, files, present, tc.want, tc.present, frames[0])
		}
	}

	root := writeMultiModuleProject(t, brokenGoMod)
	var out bytes.Buffer
	handleEnvelope(newPluginState(root), controlEnvelope{
		JSONRPC: jsonrpcVersion,
		ID:      json.RawMessage("2"),
		Method:  "fileChanged",
		Params:  json.RawMessage(`{"filePath":"broken/bad.go"}`),
	}, &out)
	frames := readFrames(t, bufio.NewReader(&out))
	if len(frames) != 1 {
		t.Fatalf("fileChanged: got %d frame(s), want 1: %+v", len(frames), frames)
	}
	if _, present := frames[0]["unfinishedFiles"]; present {
		t.Fatalf("a fileChanged response carried unfinishedFiles: %+v", frames[0])
	}
}
