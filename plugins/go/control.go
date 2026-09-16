package main

// The control-plane loop's message handling: fileChanged, semanticPass,
// workspaceChanged, reindex, status, and anything else core (or the
// conformance kit) might send. Mirrors plugins/typescript/src/index.ts's
// handleEnvelope/handleFileChanged/handleSemanticPass - see that file for
// the reference behavior this is kept honest against by
// core/src/cli/plugin_check, the same way the TS plugin is.

import (
	"encoding/json"
	"io"
	"os"
	"path/filepath"
)

// pluginState is this process's whole memory: the last File-node range
// this plugin sent for each file it has seen since it started, keyed by
// project-relative path. A fresh process - every control-plane session,
// and every one-shot --bulk-index run - starts with none of this, the same
// "cold" starting point plugins/typescript/src/incremental.ts's own cache
// has. That is why `fileChanged` on a file this process has never
// reparsed always answers with an upsert, even when nothing has actually
// changed since an earlier bulk walk: that walk was a *different*
// process, with its own cache.
type pluginState struct {
	projectRoot string
	lastRange   map[string]wireRange
}

func newPluginState(projectRoot string) *pluginState {
	return &pluginState{projectRoot: projectRoot, lastRange: map[string]wireRange{}}
}

// handleFileChanged recomputes relPath's sole node (the File node - see
// extract.go) from what is on disk right now and diffs it against what
// this process last sent for that path.
//
// Comparing by range alone - not the whole node - is deliberate and
// sufficient, not a shortcut: every other field of a File node (id, kind,
// name, qualifiedName, filePath, visibility, language, hasSyntaxErrors) is
// a pure function of relPath alone and never of file content, so range is
// the only field that can possibly have changed. That is also exactly what
// core/src/cli/plugin_check/checks.rs's id-stability.whitespace-edit
// exercises: a whitespace-only edit before the file's last newline moves
// nothing in it (see extract.go's textEndPosition doc comment), so the
// cached and freshly computed ranges compare equal and this answers with
// an empty diff, which is what that check requires.
func (s *pluginState) handleFileChanged(relPath string) fileChangeDiff {
	diff := emptyDiff()

	abs := filepath.Join(s.projectRoot, filepath.FromSlash(relPath))
	content, err := os.ReadFile(abs)
	if err != nil {
		if _, had := s.lastRange[relPath]; had {
			diff.DeleteNodeIds = append(diff.DeleteNodeIds, nodeIDFor(relPath, "File", relPath, ""))
			delete(s.lastRange, relPath)
		}
		return diff
	}

	node := computeFileNode(relPath, content)
	if previous, ok := s.lastRange[relPath]; ok && previous == node.Range {
		return diff
	}
	diff.UpsertNodes = append(diff.UpsertNodes, node)
	s.lastRange[relPath] = node.Range
	return diff
}

// handleSemanticPass answers every semanticPass request - per-file or
// whole-project alike, core's own filePaths convention (an empty list
// means "everything") - with an empty diff. There is no semantic engine
// yet to ask (go/types lands in GM-281): this scaffold's structural pass
// already produces this plugin's whole honest answer, so an "upgrade"
// pass has nothing to add.
//
// Deliberately does *not* write the conformance kit's semantic-engine
// marker (session.go's MARKER_DIR_ENV contract, mirrored from
// core/src/cli/plugin_check/session.rs) - see this repo's
// docs/architecture/multi-language-plugins.md, Go plugin section,
// "Implementation notes (GM-279)", for why: there being no engine to
// start is not the same claim as "the engine started lazily", and writing
// a marker with nothing behind it would make the
// capabilities.semantic-engine-lazy check either vacuously pass (useless)
// or fail on a technicality unrelated to laziness. Reporting
// "not instrumented" (the kit's own behavior for a plugin that never
// writes the marker) is the honest answer today, and GM-281 is expected to
// add the marker write at the moment it actually spawns/loads a real
// engine.
func (s *pluginState) handleSemanticPass(filePaths []string) fileChangeDiff {
	_ = filePaths // nothing to resolve yet - see the doc comment above
	return emptyDiff()
}

// handleEnvelope dispatches one parsed control message and, for a request
// (env.ID present), writes exactly one response frame. A notification
// (no id) is still handled - fileChanged updates pluginState's cache
// either way - but never answered, matching core's own request/
// notification distinction (protocol::types::ControlEnvelope's doc
// comment: presence of `id` is what JSON-RPC 2.0 uses to tell the two
// apart, not the method).
func handleEnvelope(state *pluginState, env controlEnvelope, out io.Writer) {
	hasID := len(env.ID) > 0

	switch env.Method {
	case "reindex":
		// A whole-project rebuild is not something this connection can
		// carry (its output is an unbounded stream, not one response
		// frame) - core runs that as a separate --bulk-index process
		// instead (bulkindex.go), matching index.ts's own handling of
		// this method.
		logf("reindex requested")

	case "fileChanged":
		var params filePathParams
		if err := json.Unmarshal(env.Params, &params); err != nil {
			logf("malformed fileChanged params: %v", err)
			return
		}
		logf("file changed: %s", params.FilePath)
		diff := state.handleFileChanged(params.FilePath)
		if hasID {
			writeResult(out, env.ID, diff)
		}
		return

	case "semanticPass":
		var params filePathsParams
		if err := json.Unmarshal(env.Params, &params); err != nil {
			logf("malformed semanticPass params: %v", err)
			return
		}
		if len(params.FilePaths) == 0 {
			logf("semantic pass requested for the whole project")
		} else {
			logf("semantic pass requested for %d file(s)", len(params.FilePaths))
		}
		diff := state.handleSemanticPass(params.FilePaths)
		if hasID {
			writeResult(out, env.ID, diff)
		}
		return

	case "workspaceChanged":
		// A notification, never a request - core always follows it with a
		// per-language reindex of its own (see
		// core/src/protocol/types.rs's ControlMessage::WorkspaceChanged
		// doc comment), so a plugin never has to answer with a diff. This
		// scaffold caches nothing at the workspace level (no module map -
		// that is GM-280/GM-281's concern once one exists to invalidate),
		// so there is nothing to do beyond logging it.
		logf("workspace file changed: %s", workspaceChangedFilePath(env.Params))
		return

	case "status":
		logf("status requested")

	default:
		// Never crash, never guess at a response shape for a method this
		// plugin does not recognize - mirrors
		// plugins/typescript/src/index.ts's handleFrame, which drops an
		// envelope parseControlEnvelope could not recognize the same way
		// (logged, no response, even if an id was present).
		logf("unknown method: %q", env.Method)
		return
	}

	if hasID {
		writeAck(out, env.ID)
	}
}

// workspaceChangedFilePath best-effort extracts the file path from a
// workspaceChanged notification's params, for the log line only - never
// worth failing over.
func workspaceChangedFilePath(params json.RawMessage) string {
	var p filePathParams
	if err := json.Unmarshal(params, &p); err != nil {
		return "?"
	}
	return p.FilePath
}

func writeResult(out io.Writer, id json.RawMessage, diff fileChangeDiff) {
	body, err := json.Marshal(fileChangeResponse{JSONRPC: jsonrpcVersion, ID: id, Result: diff})
	if err != nil {
		logf("failed to encode response: %v", err)
		return
	}
	if err := writeFrame(out, body); err != nil {
		logf("failed to write response: %v", err)
	}
}

func writeAck(out io.Writer, id json.RawMessage) {
	body, err := json.Marshal(ackResponse{JSONRPC: jsonrpcVersion, ID: id, Result: ackResult{Acknowledged: true}})
	if err != nil {
		logf("failed to encode ack: %v", err)
		return
	}
	if err := writeFrame(out, body); err != nil {
		logf("failed to write ack: %v", err)
	}
}
