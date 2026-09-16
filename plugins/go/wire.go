package main

// Wire v2 mirror of core/src/protocol/types.rs's WireNode/WireEdge/
// ControlEnvelope/FileChangeDiff/Handshake - field names and JSON shapes
// must match what serde actually produces there (see that file's own
// `#[serde(rename_all = ...)]` attributes and its test module), not a
// naive reading of the Rust field names. plugins/typescript/src/protocol.ts
// and bulkIndex.ts are the TS plugin's own copy of the same mirror; this
// file is this plugin's independent copy, not a shared one - Go and TS
// have no code-sharing mechanism between them, and the two are kept honest
// against drift by core/src/cli/plugin_check (the conformance kit both
// plugins are run against), not by shared source.

import (
	"encoding/json"
	"fmt"
)

const (
	protocolVersion = 2
	jsonrpcVersion  = "2.0"
	// Mirrors plugin.toml's `plugin_version` - kept in sync by hand today.
	// daemon::manifest has a test enforcing this for the bundled TS
	// plugin's package.json/plugin.toml pair; no equivalent exists here
	// yet because nothing on the core side compares this value against
	// the manifest's copy (unlike protocol_version, which
	// `handshake::verify` does check) - see this task's own report for
	// the gap.
	pluginVersion = "0.1.0"
	languageName  = "go"
)

type wirePosition struct {
	Line int `json:"line"`
	Col  int `json:"col"`
}

type wireRange struct {
	Start wirePosition `json:"start"`
	End   wirePosition `json:"end"`
}

// visibility mirrors core's Visibility enum: `"public"`, `"file"`, or
// `{"container": "<key>"}`. Only fileVisibility is used by this scaffold's
// File-only extractor - a File node is never itself "exported" (see
// extract.ts's own addNode: every node defaults to "file" visibility
// unless something marks it exported, and nothing ever does that for the
// File node itself). publicVisibility/containerVisibility exist ahead of
// their first real use so GM-280 (real Go declarations: capitalized names
// are `public`, unexported ones are `container(<import path>)`) only has
// to call them, not invent the wire shape.
type visibility struct {
	kind      string // "public" | "file" | "container"
	container string
}

func fileVisibility() visibility { return visibility{kind: "file"} }

//lint:ignore U1000 kept for GM-280, which needs it for real declarations
func publicVisibility() visibility { return visibility{kind: "public"} }

//lint:ignore U1000 kept for GM-280, which needs it for real declarations
func containerVisibility(container string) visibility {
	return visibility{kind: "container", container: container}
}

func (v visibility) MarshalJSON() ([]byte, error) {
	switch v.kind {
	case "public":
		return json.Marshal("public")
	case "file":
		return json.Marshal("file")
	case "container":
		return json.Marshal(struct {
			Container string `json:"container"`
		}{v.container})
	default:
		// Unreachable outside a bug in this file - every constructor above
		// sets a valid kind and visibility has no exported fields a
		// caller could set some other way. Falling back to the most
		// conservative visibility rather than emitting invalid JSON.
		return json.Marshal("file")
	}
}

// UnmarshalJSON is not needed by the plugin itself (this process only ever
// sends visibility, never receives one), but wire.go's own tests round-trip
// wireNode through encoding/json to check what this plugin actually wrote,
// and a struct with a custom MarshalJSON but no UnmarshalJSON is a
// asymmetric trap for exactly that - see the "reads back what it wrote"
// tests in bulkindex_test.go, which this exists for.
func (v *visibility) UnmarshalJSON(data []byte) error {
	var asString string
	if err := json.Unmarshal(data, &asString); err == nil {
		switch asString {
		case "public", "file":
			*v = visibility{kind: asString}
			return nil
		}
		return fmt.Errorf("unrecognized visibility string %q", asString)
	}

	var asContainer struct {
		Container string `json:"container"`
	}
	if err := json.Unmarshal(data, &asContainer); err != nil {
		return fmt.Errorf("visibility is neither a recognized string nor {\"container\": ...}: %w", err)
	}
	*v = visibility{kind: "container", container: asContainer.Container}
	return nil
}

// wireNode mirrors core's WireNode for exactly what this scaffold ever
// emits: File nodes with no container, no declarations and no placeholder
// target. The optional Rust fields this struct has no use for yet
// (signature, docComment, nativeKind, declarations, container,
// containerParent, target) are omitted from the struct entirely rather
// than modeled as always-nil pointers: Rust's
// `skip_serializing_if = "Option::is_none"` makes an absent key exactly
// equivalent to an explicit `None` there, and this scaffold never has
// anything to put in any of them. GM-280 adds them back once there is a
// real symbol to carry them.
type wireNode struct {
	ID              string     `json:"id"`
	Kind            string     `json:"kind"`
	Name            string     `json:"name"`
	QualifiedName   string     `json:"qualifiedName"`
	FilePath        string     `json:"filePath"`
	Range           wireRange  `json:"range"`
	Visibility      visibility `json:"visibility"`
	Language        string     `json:"language"`
	HasSyntaxErrors bool       `json:"hasSyntaxErrors"`
}

// wireEdge mirrors core's WireEdge. Unused by this scaffold's extractor (no
// edges are emitted yet - GM-280), but kept as a real type rather than
// `interface{}` so fileChangeDiff.UpsertEdges has something honest to be a
// slice of, and so GM-280 has the shape ready to fill in.
type wireEdge struct {
	ID       string `json:"id"`
	FromID   string `json:"fromId"`
	ToID     string `json:"toId"`
	Kind     string `json:"kind"`
	Source   string `json:"source"`
	Engine   string `json:"engine"`
	Resolved bool   `json:"resolved"`
}

// fileChangeDiff mirrors core's FileChangeDiff: what a `fileChanged` or
// `semanticPass` response upserts/deletes.
type fileChangeDiff struct {
	UpsertNodes   []wireNode `json:"upsertNodes"`
	DeleteNodeIds []string   `json:"deleteNodeIds"`
	UpsertEdges   []wireEdge `json:"upsertEdges"`
	DeleteEdgeIds []string   `json:"deleteEdgeIds"`
}

// emptyDiff returns a FileChangeDiff whose four lists are all present but
// empty - never nil. encoding/json marshals a nil slice as `null`, but
// core's `Vec<...>` fields always serialize as `[]` (there is no
// `skip_serializing_if` on any of FileChangeDiff's four fields in
// core/src/protocol/types.rs), and plugins/typescript/src/index.ts's own
// EMPTY_WIRE_DIFF constant makes the same choice for the same reason.
func emptyDiff() fileChangeDiff {
	return fileChangeDiff{
		UpsertNodes:   []wireNode{},
		DeleteNodeIds: []string{},
		UpsertEdges:   []wireEdge{},
		DeleteEdgeIds: []string{},
	}
}

// handshakeMessage mirrors core's Handshake, sent unframed-JSON-but-framed
// (i.e. through writeFrame) the moment this process starts, before any
// control message is read - protocol::handshake::perform reads exactly
// this shape off the plugin's stdout.
type handshakeMessage struct {
	ProtocolVersion int    `json:"protocolVersion"`
	Language        string `json:"language"`
	PluginVersion   string `json:"pluginVersion"`
}

// controlEnvelope mirrors core's ControlEnvelope, read generically (Params
// stays raw JSON) because which shape `params` has depends on `method` -
// core's own `#[serde(tag = "method", content = "params")]` encoding,
// mirrored by hand since Go's encoding/json has no adjacently-tagged-enum
// support.
type controlEnvelope struct {
	JSONRPC string          `json:"jsonrpc"`
	ID      json.RawMessage `json:"id,omitempty"`
	Method  string          `json:"method"`
	Params  json.RawMessage `json:"params,omitempty"`
}

// filePathParams mirrors ControlMessage::FileChanged/Reindex/WorkspaceChanged's
// singular `params: { filePath }`.
type filePathParams struct {
	FilePath string `json:"filePath"`
}

// filePathsParams mirrors ControlMessage::SemanticPass's plural
// `params: { filePaths }` - see that variant's own doc comment in
// core/src/protocol/types.rs for why an empty list means "the whole
// project", not "nothing".
type filePathsParams struct {
	FilePaths []string `json:"filePaths"`
}

// fileChangeResponse mirrors core's FileChangeResponse - the one response
// shape both `fileChanged` and `semanticPass` answer with (see that type's
// own doc comment for why there is only one).
type fileChangeResponse struct {
	JSONRPC string          `json:"jsonrpc"`
	ID      json.RawMessage `json:"id"`
	Result  fileChangeDiff  `json:"result"`
}

// ackResponse is the `{ acknowledged: true }` shape this plugin answers a
// `reindex`/`status` request with, mirroring
// plugins/typescript/src/index.ts's handleEnvelope default case. Core has
// no named Rust type for this (those two methods are effectively no-ops on
// the daemon side today - see ControlMessage::Reindex/Status's own doc
// comments), so there is nothing in core/src/protocol/types.rs for this to
// mirror field-for-field; the shape only has to be valid, parseable JSON
// that is not mistaken for a FileChangeResponse.
type ackResponse struct {
	JSONRPC string          `json:"jsonrpc"`
	ID      json.RawMessage `json:"id"`
	Result  ackResult       `json:"result"`
}

type ackResult struct {
	Acknowledged bool `json:"acknowledged"`
}
