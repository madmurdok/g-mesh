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
	"sort"
)

func sortStrings(values []string) { sort.Strings(values) }

// cachedFile is the last graph this process emitted for one file, keyed by
// id on both sides so the next extraction of that file can be diffed against
// it without re-deriving anything.
type cachedFile struct {
	nodes map[string]wireNode
	edges map[string]wireEdge
	// The file's open sites (open_sites.go) as of that same extraction.
	// Replaced wholesale, never merged, so they cannot describe code that is
	// no longer there. Read by GM-281's semantic pass; nothing sends them.
	openSites []openSite
}

// pluginState is this process's whole memory: the project root, the
// workspace's module layout, and the last graph this plugin sent for each
// file it has seen since it started, keyed by project-relative path.
//
// A fresh process - every control-plane session, and every one-shot
// --bulk-index run - starts with none of this, the same "cold" starting
// point plugins/typescript/src/incremental.ts's own cache has. That is why
// `fileChanged` on a file this process has never reparsed always answers
// with a full upsert, even when nothing has actually changed since an
// earlier bulk walk: that walk was a *different* process, with its own
// cache.
type pluginState struct {
	projectRoot string
	workspace   *workspace
	files       map[string]cachedFile
	// semantic is this process's go/types tier (semantic.go). Constructing
	// it loads nothing - the engine is inert until the first semanticPass,
	// which is the laziness `capabilities.semantic-engine-lazy` checks.
	semantic *semanticEngine
}

func newPluginState(projectRoot string) *pluginState {
	return &pluginState{
		projectRoot: projectRoot,
		workspace:   loadWorkspace(projectRoot),
		files:       map[string]cachedFile{},
		semantic:    newSemanticEngine(projectRoot),
	}
}

// handleFileChanged re-extracts relPath from what is on disk right now and
// diffs the result, by id, against what this process last sent for that
// path.
//
// # Why the diff compares whole records, not a shortcut
//
// GM-279's scaffold compared File-node *ranges*, which was sufficient when a
// file produced exactly one node whose every other field was a function of
// its path. With real declarations that is no longer true: an edit can
// change a signature, a doc comment, a visibility or a container without
// moving anything, and it can change a range without changing anything else.
// Comparing the whole record is the only version that cannot miss one of
// those, and it is what makes both halves of the contract hold at once:
//
//   - a whitespace-only edit before the file's last newline moves no range
//     and changes no field, so every record compares equal and the diff is
//     empty - `id-stability.whitespace-edit`;
//   - a real edit to a declaration moves that declaration's range and every
//     later one in the file, so each of those records is re-sent with its
//     new range - `id-stability.declaration-edit-applies`, which compares
//     the result against a fresh bulk walk of the edited tree.
//
// The comparison is by value on a struct of comparable fields plus one
// pointer (`Target`), which is why it goes through nodesEqual rather than
// `==`: two placeholders with equal targets must compare equal even though
// their pointers differ.
func (s *pluginState) handleFileChanged(relPath string) fileChangeDiff {
	diff := emptyDiff()

	abs := filepath.Join(s.projectRoot, filepath.FromSlash(relPath))
	content, err := os.ReadFile(abs)
	if err != nil {
		// Gone from disk: delete everything this process said about it.
		// Edges first in the diff's own lists is not required by anything -
		// core applies deletes as a set - but a file that is no longer there
		// leaves neither nodes nor edges behind.
		previous, had := s.files[relPath]
		if !had {
			return diff
		}
		for id := range previous.edges {
			diff.DeleteEdgeIds = append(diff.DeleteEdgeIds, id)
		}
		for id := range previous.nodes {
			diff.DeleteNodeIds = append(diff.DeleteNodeIds, id)
		}
		sortStrings(diff.DeleteEdgeIds)
		sortStrings(diff.DeleteNodeIds)
		delete(s.files, relPath)
		return diff
	}

	graph := extractFile(s.workspace, relPath, content)
	previous := s.files[relPath]

	next := cachedFile{
		nodes:     make(map[string]wireNode, len(graph.nodes)),
		edges:     make(map[string]wireEdge, len(graph.edges)),
		openSites: graph.openSites,
	}
	// Emission order is preserved in the diff (File node first, then
	// declarations, then placeholders), because it costs nothing and makes
	// the stream readable to a human diffing two runs by eye.
	for _, node := range graph.nodes {
		next.nodes[node.ID] = node
		if before, existed := previous.nodes[node.ID]; !existed || !nodesEqual(before, node) {
			diff.UpsertNodes = append(diff.UpsertNodes, node)
		}
	}
	for _, edge := range graph.edges {
		next.edges[edge.ID] = edge
		if before, existed := previous.edges[edge.ID]; !existed || before != edge {
			diff.UpsertEdges = append(diff.UpsertEdges, edge)
		}
	}
	for id := range previous.edges {
		if _, still := next.edges[id]; !still {
			diff.DeleteEdgeIds = append(diff.DeleteEdgeIds, id)
		}
	}
	for id := range previous.nodes {
		if _, still := next.nodes[id]; !still {
			diff.DeleteNodeIds = append(diff.DeleteNodeIds, id)
		}
	}
	// Map iteration order is randomized in Go, and a diff whose delete lists
	// shuffle between two otherwise identical runs is a diff no one can
	// compare by eye - and would make this plugin's own tests depend on the
	// runtime's hash seed.
	sortStrings(diff.DeleteEdgeIds)
	sortStrings(diff.DeleteNodeIds)

	s.files[relPath] = next
	return diff
}

// reloadWorkspace re-reads the project's module layout and forgets every
// cached file - see the `workspaceChanged` case in handleEnvelope.
func (s *pluginState) reloadWorkspace() {
	s.workspace = loadWorkspace(s.projectRoot)
	s.files = map[string]cachedFile{}
}

// openSitesFor returns the unresolved selections this process currently
// holds for a file - GM-281's input, and nothing core ever sees.
func (s *pluginState) openSitesFor(relPath string) []openSite {
	return s.files[relPath].openSites
}

// nodesEqual compares two wire nodes by value, including the placeholder
// target behind the one pointer field.
func nodesEqual(a, b wireNode) bool {
	if a.Target == nil || b.Target == nil {
		if a.Target != b.Target {
			return false
		}
	} else if *a.Target != *b.Target {
		return false
	}
	a.Target, b.Target = nil, nil
	return a == b
}

// handleSemanticPass answers a semanticPass request - per-file or
// whole-project alike, core's own filePaths convention (an empty list means
// "everything") - by asking the go/types engine (semantic.go).
//
// This is the *only* path into that engine. Nothing in bulk indexing,
// `fileChanged` or `workspaceChanged` touches it, which is what makes the
// conformance kit's `capabilities.semantic-engine-lazy` check pass rather
// than merely go uninstrumented: the engine's own first `packages.Load`
// writes the kit's marker, and that call can only be reached from here.
//
// With no Go toolchain on PATH, or a `packages.Load` that fails outright,
// the engine logs once and returns an empty diff with the second return
// value `true` - GM-384's `incomplete`, without which the structural graph
// stayed put but Go's `language_state.semanticPassAt` got set anyway, and
// the receiver-call gap dropped out of the MCP instructions despite no
// semantic tier having actually run.
func (s *pluginState) handleSemanticPass(filePaths []string) (fileChangeDiff, bool) {
	return s.semantic.run(s.workspace, filePaths)
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
			// A structural reparse has nothing to be incomplete about
			// (core/src/watcher/apply.rs's own comment on this same
			// distinction) - always `false`.
			writeResult(out, env.ID, diff, false)
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
		diff, incomplete := state.handleSemanticPass(params.FilePaths)
		if hasID {
			writeResult(out, env.ID, diff, incomplete)
		}
		return

	case "workspaceChanged":
		// A notification, never a request - core always follows it with a
		// per-language reindex of its own (see core/src/protocol/types.rs's
		// ControlMessage::WorkspaceChanged doc comment), so a plugin never
		// has to answer with a diff.
		//
		// There *is* now something to invalidate, which there was not in
		// GM-279: a go.mod/go.work edit can rename a module or add one, and
		// every container key in the affected subtree is computed from that
		// (workspace.go). So the module layout is reloaded from disk and the
		// per-file cache is dropped wholesale - a cached graph carries the
		// old container keys inside its nodes, so keeping it would make the
		// next `fileChanged` diff against a graph that no longer describes
		// how this project is laid out. Dropping it costs one full re-upsert
		// per file core then asks about, which is exactly what the reindex
		// core follows this with does anyway.
		logf("workspace file changed: %s", workspaceChangedFilePath(env.Params))
		state.reloadWorkspace()
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

func writeResult(out io.Writer, id json.RawMessage, diff fileChangeDiff, incomplete bool) {
	body, err := json.Marshal(fileChangeResponse{JSONRPC: jsonrpcVersion, ID: id, Result: diff, Incomplete: incomplete})
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
