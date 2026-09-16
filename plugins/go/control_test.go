package main

import (
	"bufio"
	"bytes"
	"encoding/json"
	"os"
	"testing"
)

func TestHandleFileChangedEmitsOnFirstSightAndSuppressesAnUnchangedRepeat(t *testing.T) {
	root := t.TempDir()
	abs := writeFile(t, root, "a.go", "package a\n\nfunc F() {}\n")
	_ = abs
	state := newPluginState(root)

	first := state.handleFileChanged("a.go")
	if len(first.UpsertNodes) != 1 {
		t.Fatalf("first fileChanged: %d upserts, want 1 (cold cache)", len(first.UpsertNodes))
	}

	second := state.handleFileChanged("a.go")
	if len(second.UpsertNodes) != 0 || len(second.DeleteNodeIds) != 0 {
		t.Fatalf("second fileChanged over unchanged content: %+v, want an empty diff", second)
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
	if len(warm.UpsertNodes) != 1 {
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

// Emptying the file must be answered with an upsert carrying the *same*
// id (an empty file still gets a File node - id-stability depends on this
// scaffold never deleting the File node itself while the file exists),
// with an updated (0,0)-(0,0) range.
func TestHandleFileChangedEmptyingTheFileReupsertsSameID(t *testing.T) {
	root := t.TempDir()
	abs := writeFile(t, root, "a.go", "package a\nfunc F() {}\n")
	state := newPluginState(root)

	warm := state.handleFileChanged("a.go")
	originalID := warm.UpsertNodes[0].ID

	if err := os.WriteFile(abs, nil, 0o644); err != nil {
		t.Fatalf("WriteFile: %v", err)
	}
	diff := state.handleFileChanged("a.go")
	if len(diff.UpsertNodes) != 1 {
		t.Fatalf("emptying the file: %+v, want exactly one upsert", diff)
	}
	if diff.UpsertNodes[0].ID != originalID {
		t.Fatalf("id changed after emptying the file: %q != %q", diff.UpsertNodes[0].ID, originalID)
	}
	if diff.UpsertNodes[0].Range != (wireRange{}) {
		t.Fatalf("range = %+v, want the zero range for an empty file", diff.UpsertNodes[0].Range)
	}
	if len(diff.DeleteNodeIds) != 0 {
		t.Fatalf("expected no deletes, got %v", diff.DeleteNodeIds)
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

func TestHandleSemanticPassAlwaysAnswersEmpty(t *testing.T) {
	state := newPluginState(t.TempDir())
	diff := state.handleSemanticPass(nil)
	if len(diff.UpsertNodes) != 0 || len(diff.UpsertEdges) != 0 {
		t.Fatalf("handleSemanticPass(nil) = %+v, want an empty diff", diff)
	}
	diff = state.handleSemanticPass([]string{"a.go"})
	if len(diff.UpsertNodes) != 0 || len(diff.UpsertEdges) != 0 {
		t.Fatalf("handleSemanticPass([a.go]) = %+v, want an empty diff", diff)
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
	runControlLoop(root, &in, &out)

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

// A malformed frame (bad Content-Length) must end the loop without a
// panic - readFrame's own error is unrecoverable (a desynchronized
// stream), and runControlLoop must simply stop, not crash the process.
func TestRunControlLoopStopsCleanlyOnAFramingError(t *testing.T) {
	in := bytes.NewReader([]byte("Content-Length: nope\r\n\r\n{}"))
	var out bytes.Buffer
	runControlLoop(t.TempDir(), in, &out)

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
	runControlLoop(root, &in, &out)

	frames := readFrames(t, bufio.NewReader(&out))
	if len(frames) != 2 {
		t.Fatalf("got %d frames, want 2 (handshake, then the fileChanged response): %+v", len(frames), frames)
	}
}
