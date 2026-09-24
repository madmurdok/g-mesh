package main

import (
	"bufio"
	"bytes"
	"encoding/json"
	"io"
	"os"
	"testing"
)

func TestHandleFileChangedEmitsOnFirstSightAndSuppressesAnUnchangedRepeat(t *testing.T) {
	root := t.TempDir()
	writeFile(t, root, "a.go", "package a\n\nfunc F() {}\n")
	state := newPluginState(root)

	first := state.handleFileChanged("a.go")
	// The File node and `F`, plus DEFINES and EXPORTS between them.
	if len(first.UpsertNodes) != 2 || len(first.UpsertEdges) != 2 {
		t.Fatalf("first fileChanged: %+v, want 2 node and 2 edge upserts (cold cache)", first)
	}

	second := state.handleFileChanged("a.go")
	if len(second.UpsertNodes) != 0 || len(second.DeleteNodeIds) != 0 ||
		len(second.UpsertEdges) != 0 || len(second.DeleteEdgeIds) != 0 {
		t.Fatalf("second fileChanged over unchanged content: %+v, want an empty diff", second)
	}
}

// A real edit to a declaration - the shape core's
// id-stability.declaration-edit-applies check exercises - must re-send every
// node whose range moved, under the *same* ids, and delete nothing.
func TestHandleFileChangedDeclarationEditReupsertsMovedRangesUnderTheSameIDs(t *testing.T) {
	root := t.TempDir()
	abs := writeFile(t, root, "a.go", "package a\n\nfunc F() {\n}\n\nfunc G() {}\n")
	state := newPluginState(root)

	warm := state.handleFileChanged("a.go")
	before := map[string]wireRange{}
	for _, node := range warm.UpsertNodes {
		before[node.ID] = node.Range
	}

	// A line break before `F`'s closing brace: `F` grows by a line and
	// everything after it moves down one.
	if err := os.WriteFile(abs, []byte("package a\n\nfunc F() {\n\n}\n\nfunc G() {}\n"), 0o644); err != nil {
		t.Fatalf("WriteFile: %v", err)
	}
	diff := state.handleFileChanged("a.go")

	if len(diff.DeleteNodeIds) != 0 || len(diff.DeleteEdgeIds) != 0 {
		t.Fatalf("a declaration edit deleted something: %+v", diff)
	}
	// The File node, `F` and `G` all move; the edges between them do not
	// change at all, since an edge carries no position.
	if len(diff.UpsertNodes) != 3 || len(diff.UpsertEdges) != 0 {
		t.Fatalf("diff = %+v, want 3 node upserts and no edge upserts", diff)
	}
	for _, node := range diff.UpsertNodes {
		was, known := before[node.ID]
		if !known {
			t.Fatalf("upserted an id the warm cache never had: %q (%s)", node.ID, node.QualifiedName)
		}
		if was == node.Range {
			t.Fatalf("%s was re-upserted with an unchanged range %+v", node.QualifiedName, node.Range)
		}
	}
}

// The exact scenario core/src/cli/plugin_check/session.rs's whitespace_edit
// exercises: a single space inserted immediately before the file's last
// newline must not move the File node's range, so the answering diff must
// be completely empty - id-stability.whitespace-edit's own requirement.
func TestHandleFileChangedWhitespaceOnlyEditIsAnEmptyDiff(t *testing.T) {
	root := t.TempDir()
	abs := writeFile(t, root, "a.go", "package a\nfunc F() {}\n")
	state := newPluginState(root)

	warm := state.handleFileChanged("a.go")
	if len(warm.UpsertNodes) == 0 {
		t.Fatalf("expected the cold call to upsert, got %+v", warm)
	}

	edited := []byte("package a\nfunc F() {} \n") // a space before the last newline
	if err := os.WriteFile(abs, edited, 0o644); err != nil {
		t.Fatalf("WriteFile: %v", err)
	}
	diff := state.handleFileChanged("a.go")
	if len(diff.UpsertNodes) != 0 || len(diff.DeleteNodeIds) != 0 ||
		len(diff.UpsertEdges) != 0 || len(diff.DeleteEdgeIds) != 0 {
		t.Fatalf("whitespace-only edit produced a non-empty diff: %+v", diff)
	}
}

// Emptying the file keeps the File node - under the same id, with the zero
// range and `hasSyntaxErrors` set, because an empty file is not a legal Go
// file - and deletes everything that file used to declare, along with the
// edges into it. Restoring it must bring every id back unchanged: that round
// trip is what core's id-stability.incremental-matches-bulk check compares
// against the bulk walk.
func TestHandleFileChangedEmptyingAndRestoringKeepsEveryID(t *testing.T) {
	root := t.TempDir()
	source := "package a\n\nfunc F() {}\n"
	abs := writeFile(t, root, "a.go", source)
	state := newPluginState(root)

	warm := state.handleFileChanged("a.go")
	fileNodeID := nodeIDFor("a.go", nodeKindFile, "a.go", "")
	warmNodeIDs := map[string]bool{}
	for _, node := range warm.UpsertNodes {
		warmNodeIDs[node.ID] = true
	}
	warmEdgeIDs := map[string]bool{}
	for _, edge := range warm.UpsertEdges {
		warmEdgeIDs[edge.ID] = true
	}

	if err := os.WriteFile(abs, nil, 0o644); err != nil {
		t.Fatalf("WriteFile: %v", err)
	}
	emptied := state.handleFileChanged("a.go")
	if len(emptied.UpsertNodes) != 1 || emptied.UpsertNodes[0].ID != fileNodeID {
		t.Fatalf("emptying the file: %+v, want exactly the File node re-upserted", emptied)
	}
	if emptied.UpsertNodes[0].Range != (wireRange{}) {
		t.Fatalf("range = %+v, want the zero range for an empty file", emptied.UpsertNodes[0].Range)
	}
	if !emptied.UpsertNodes[0].HasSyntaxErrors {
		t.Fatal("an empty file has no package clause, so hasSyntaxErrors must be set")
	}
	if len(emptied.DeleteNodeIds) != len(warmNodeIDs)-1 || len(emptied.DeleteEdgeIds) != len(warmEdgeIDs) {
		t.Fatalf("emptying deleted %d node(s) and %d edge(s), want %d and %d",
			len(emptied.DeleteNodeIds), len(emptied.DeleteEdgeIds), len(warmNodeIDs)-1, len(warmEdgeIDs))
	}
	for _, id := range emptied.DeleteNodeIds {
		if !warmNodeIDs[id] {
			t.Fatalf("deleted an id that was never emitted: %q", id)
		}
	}

	if err := os.WriteFile(abs, []byte(source), 0o644); err != nil {
		t.Fatalf("WriteFile: %v", err)
	}
	restored := state.handleFileChanged("a.go")
	if len(restored.DeleteNodeIds) != 0 || len(restored.DeleteEdgeIds) != 0 {
		t.Fatalf("restoring the file deleted something: %+v", restored)
	}
	for _, node := range restored.UpsertNodes {
		if !warmNodeIDs[node.ID] {
			t.Fatalf("restoring produced a node id the original never had: %q (%s)", node.ID, node.QualifiedName)
		}
	}
	for _, edge := range restored.UpsertEdges {
		if !warmEdgeIDs[edge.ID] {
			t.Fatalf("restoring produced an edge id the original never had: %q", edge.ID)
		}
	}
}

// A file that vanishes (deleted from disk) must delete the node this
// process previously emitted for it, and only that node.
func TestHandleFileChangedDeletedFileDeletesItsNode(t *testing.T) {
	root := t.TempDir()
	abs := writeFile(t, root, "a.go", "package a\n")
	state := newPluginState(root)

	warm := state.handleFileChanged("a.go")
	originalID := warm.UpsertNodes[0].ID

	if err := os.Remove(abs); err != nil {
		t.Fatalf("Remove: %v", err)
	}
	diff := state.handleFileChanged("a.go")
	// "package a\n" declares nothing, so the File node is the only id there
	// was to delete.
	if len(diff.DeleteNodeIds) != 1 || diff.DeleteNodeIds[0] != originalID {
		t.Fatalf("delete diff = %+v, want exactly [%q]", diff.DeleteNodeIds, originalID)
	}
	if len(diff.UpsertNodes) != 0 {
		t.Fatalf("expected no upserts, got %v", diff.UpsertNodes)
	}

	// A second fileChanged on the still-deleted file must not repeat the
	// delete - nothing is cached for it any more.
	again := state.handleFileChanged("a.go")
	if len(again.DeleteNodeIds) != 0 || len(again.UpsertNodes) != 0 {
		t.Fatalf("expected an empty diff on a second fileChanged over a still-deleted file, got %+v", again)
	}
}

// Open sites are what GM-281 will answer, and they are kept per file in this
// process's memory - never sent to core, and replaced wholesale on every
// re-extraction so they cannot describe code that is no longer there.
func TestOpenSitesAreCachedPerFileAndReplacedWholesale(t *testing.T) {
	root := t.TempDir()
	abs := writeFile(t, root, "a.go", "package a\n\nfunc F(s *Server) {\n\ts.Close()\n\ts.Flush()\n}\n")
	state := newPluginState(root)

	diff := state.handleFileChanged("a.go")
	for _, node := range diff.UpsertNodes {
		if node.Name == "Close" || node.Name == "Flush" {
			t.Fatalf("a receiver call must never reach core as a node: %+v", node)
		}
	}
	if got := openSiteNames(fileGraph{openSites: state.openSitesFor("a.go")}); !equalStrings(got, []string{"Close", "Flush"}) {
		t.Fatalf("open sites = %v, want [Close Flush]", got)
	}

	if err := os.WriteFile(abs, []byte("package a\n\nfunc F(s *Server) {\n\ts.Close()\n}\n"), 0o644); err != nil {
		t.Fatalf("WriteFile: %v", err)
	}
	state.handleFileChanged("a.go")
	if got := openSiteNames(fileGraph{openSites: state.openSitesFor("a.go")}); !equalStrings(got, []string{"Close"}) {
		t.Fatalf("open sites after the edit = %v, want [Close] - the removed call must be gone", got)
	}

	if err := os.Remove(abs); err != nil {
		t.Fatalf("Remove: %v", err)
	}
	state.handleFileChanged("a.go")
	if got := state.openSitesFor("a.go"); len(got) != 0 {
		t.Fatalf("a deleted file must leave no open sites, got %v", got)
	}
}

func TestHandleSemanticPassAlwaysAnswersEmpty(t *testing.T) {
	state := newPluginState(t.TempDir())
	diff, incomplete := state.handleSemanticPass(nil)
	if len(diff.UpsertNodes) != 0 || len(diff.UpsertEdges) != 0 {
		t.Fatalf("handleSemanticPass(nil) = %+v, want an empty diff", diff)
	}
	if incomplete {
		t.Fatalf("handleSemanticPass(nil) on an empty project answered incomplete=true, want false - " +
			"there was nothing to resolve, which is a trivially complete pass")
	}
	diff, incomplete = state.handleSemanticPass([]string{"a.go"})
	if len(diff.UpsertNodes) != 0 || len(diff.UpsertEdges) != 0 {
		t.Fatalf("handleSemanticPass([a.go]) = %+v, want an empty diff", diff)
	}
	if incomplete {
		t.Fatalf("handleSemanticPass([a.go]) for a file this project doesn't have answered incomplete=true, want false")
	}
}

// GM-384: the discriminating case. `semantic.go`'s doc comment used to claim
// that with no `go` on PATH, "language_state.semanticPassAt is never set for
// Go" - false, because nothing here ever told core the pass was incomplete:
// an empty diff and a *complete* diff are the same wire shape unless
// `incomplete` says otherwise (wire/src/lib.rs's `FileChangeResponse::
// incomplete`), and core's `apply_semantic_pass`
// (core/src/watcher/apply.rs) only withholds the timestamp when that field
// is `true`. So a Go-only index with no toolchain looked exactly like one
// whose semantic pass had just finished - `semanticPassAt` got set, and
// both `mcp::provenance` and the receiver-call gap in `mcp::instructions`
// believed it.
//
// Driven through `handleEnvelope` end to end, because the thing core's
// `apply_semantic_pass` actually reads is the marshaled JSON response, not
// this process's internal return value - a passing check on the Go value
// alone would not prove the wire contract is honored.
func TestSemanticPassWithoutAToolchainReportsIncompleteOnTheWire(t *testing.T) {
	root := writeProbeProject(t)

	// Emptied rather than narrowed, matching
	// TestSemanticPassWithoutAToolchainAnswersAnEmptyDiff: exec.LookPath
	// consults PATH only, so this is exactly "no toolchain installed."
	t.Setenv("PATH", "")

	state := newPluginState(root)

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
	incomplete, _ := frames[0]["incomplete"].(bool)
	if !incomplete {
		t.Fatalf(
			"whole-project semanticPass without a toolchain answered %+v, want \"incomplete\":true on the wire - "+
				"without it, core's apply_semantic_pass records the pass as done and both mcp::provenance and "+
				"the receiver-call gap in mcp::instructions believe a semantic tier ran that never did",
			frames[0],
		)
	}
}

// --- handleEnvelope / runControlLoop, end to end over in-memory frames ---

func frameOf(t *testing.T, v interface{}) []byte {
	t.Helper()
	body, err := json.Marshal(v)
	if err != nil {
		t.Fatalf("marshal: %v", err)
	}
	var buf bytes.Buffer
	if err := writeFrame(&buf, body); err != nil {
		t.Fatalf("writeFrame: %v", err)
	}
	return buf.Bytes()
}

func readFrames(t *testing.T, r *bufio.Reader) []map[string]interface{} {
	t.Helper()
	var frames []map[string]interface{}
	for {
		body, err := readFrame(r)
		if err != nil {
			break
		}
		var m map[string]interface{}
		if err := json.Unmarshal(body, &m); err != nil {
			t.Fatalf("frame did not parse as JSON: %v", err)
		}
		frames = append(frames, m)
	}
	return frames
}

// The full control loop: handshake first, then a fileChanged request
// answered with an upsert, an unknown method that must not crash and must
// not be answered, and a clean shutdown on stdin EOF.
func TestRunControlLoopEndToEnd(t *testing.T) {
	root := t.TempDir()
	writeFile(t, root, "a.go", "package a\n")

	var in bytes.Buffer
	in.Write(frameOf(t, map[string]interface{}{
		"jsonrpc": "2.0", "id": 1, "method": "fileChanged",
		"params": map[string]string{"filePath": "a.go"},
	}))
	in.Write(frameOf(t, map[string]interface{}{
		"jsonrpc": "2.0", "id": 2, "method": "somethingCoreDoesNotSendYet",
		"params": map[string]string{},
	}))
	in.Write(frameOf(t, map[string]interface{}{
		"jsonrpc": "2.0", "method": "workspaceChanged",
		"params": map[string]string{"filePath": "go.mod"},
	}))

	var out bytes.Buffer
	runControlLoop(root, &in, &out, nil)

	frames := readFrames(t, bufio.NewReader(&out))
	if len(frames) != 2 {
		t.Fatalf("got %d frames, want 2 (handshake, then the fileChanged response - no response for the \n\t\t\tunknown method or the workspaceChanged notification): %+v", len(frames), frames)
	}

	handshake := frames[0]
	if handshake["protocolVersion"] != float64(2) || handshake["language"] != "go" {
		t.Fatalf("handshake = %+v", handshake)
	}

	response := frames[1]
	if response["id"] != float64(1) {
		t.Fatalf("response id = %v, want 1", response["id"])
	}
	result, ok := response["result"].(map[string]interface{})
	if !ok {
		t.Fatalf("response has no result object: %+v", response)
	}
	upserts, ok := result["upsertNodes"].([]interface{})
	if !ok || len(upserts) != 1 {
		t.Fatalf("upsertNodes = %+v, want exactly one node", result["upsertNodes"])
	}
}

// `workspaceChanged` is the notification core sends when go.mod or go.work
// moves. Container keys are computed from those files, so the module layout
// is re-read and every cached file is dropped - after which the next
// `fileChanged` is cold again and re-sends the file under the *new* keys.
func TestWorkspaceChangedRereadsTheModuleLayoutAndDropsTheCache(t *testing.T) {
	root := t.TempDir()
	writeFile(t, root, "go.mod", "module github.com/example/before\n\ngo 1.22\n")
	writeFile(t, root, "pkg/a.go", "package pkg\n\nfunc F() {}\n")
	state := newPluginState(root)

	warm := state.handleFileChanged("pkg/a.go")
	if got := containerOf(t, warm, "F"); got != "github.com/example/before/pkg" {
		t.Fatalf("container = %q, want github.com/example/before/pkg", got)
	}
	if empty := state.handleFileChanged("pkg/a.go"); len(empty.UpsertNodes) != 0 {
		t.Fatalf("expected a warm cache to answer an unchanged file with nothing, got %+v", empty)
	}

	writeFile(t, root, "go.mod", "module github.com/example/after\n\ngo 1.22\n")
	handleEnvelope(state, controlEnvelope{
		JSONRPC: jsonrpcVersion,
		Method:  "workspaceChanged",
		Params:  json.RawMessage(`{"filePath":"go.mod"}`),
	}, io.Discard)

	cold := state.handleFileChanged("pkg/a.go")
	if got := containerOf(t, cold, "F"); got != "github.com/example/after/pkg" {
		t.Fatalf("container after the rename = %q, want github.com/example/after/pkg", got)
	}
}

func containerOf(t *testing.T, diff fileChangeDiff, qualifiedName string) string {
	t.Helper()
	for _, node := range diff.UpsertNodes {
		if node.QualifiedName == qualifiedName {
			return node.Container
		}
	}
	t.Fatalf("no node named %q in %+v", qualifiedName, diff.UpsertNodes)
	return ""
}

// A malformed frame (bad Content-Length) must end the loop without a
// panic - readFrame's own error is unrecoverable (a desynchronized
// stream), and runControlLoop must simply stop, not crash the process.
func TestRunControlLoopStopsCleanlyOnAFramingError(t *testing.T) {
	in := bytes.NewReader([]byte("Content-Length: nope\r\n\r\n{}"))
	var out bytes.Buffer
	runControlLoop(t.TempDir(), in, &out, nil)

	frames := readFrames(t, bufio.NewReader(&out))
	if len(frames) != 1 {
		t.Fatalf("got %d frames, want exactly the handshake: %+v", len(frames), frames)
	}
}

// A malformed JSON body inside an otherwise well-framed message must be
// logged and skipped, not crash the loop or desynchronize the stream -
// the next well-formed frame must still be handled.
func TestRunControlLoopSkipsAMalformedJSONBodyAndContinues(t *testing.T) {
	root := t.TempDir()
	writeFile(t, root, "a.go", "package a\n")

	var in bytes.Buffer
	if err := writeFrame(&in, []byte("not json")); err != nil {
		t.Fatalf("writeFrame: %v", err)
	}
	in.Write(frameOf(t, map[string]interface{}{
		"jsonrpc": "2.0", "id": 1, "method": "fileChanged",
		"params": map[string]string{"filePath": "a.go"},
	}))

	var out bytes.Buffer
	runControlLoop(root, &in, &out, nil)

	frames := readFrames(t, bufio.NewReader(&out))
	if len(frames) != 2 {
		t.Fatalf("got %d frames, want 2 (handshake, then the fileChanged response): %+v", len(frames), frames)
	}
}
