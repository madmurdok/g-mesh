use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Bumped on any breaking change to this wire contract. A mismatch between
/// core and plugin is a hard load failure - never best-effort compatibility.
pub const CURRENT_PROTOCOL_VERSION: u32 = 1;

pub const JSONRPC_VERSION: &str = "2.0";

/// A point in a source file, as a line and a column.
// Doc comments on this type and its fields are user-facing: `JsonSchema` is
// derived so the MCP tool schemas can describe positions with the very type
// the plugin protocol and storage layer already use, and schemars copies the
// prose straight into the published schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Position {
    pub line: u32,
    pub col: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Range {
    pub start: Position,
    pub end: Position,
}

/// Matches the `nodes` table's `kind` column (see storage::schema).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NodeKind {
    File,
    Module,
    Type,
    Function,
    Variable,
}

/// Matches the `edges` table's `kind` column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum EdgeKind {
    Defines,
    Imports,
    Calls,
    SupertypeOf,
    References,
    Exports,
}

/// Whether an edge was produced by the fast structural pass or confirmed by
/// the plugin's semantic layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EdgeSource {
    TreeSitter,
    TsCompiler,
}

/// One declaration of a symbol written as several - an overload signature
/// beside its implementation, an interface or a namespace merged across
/// statements. Mirrors the plugin's `SymbolDeclaration`
/// (plugins/js-ts/src/extract.ts) exactly, flat line/col fields and all,
/// rather than nesting a [`Range`] the way [`WireNode`] does: this shape
/// crosses the wire as the plugin already builds it in process, and a
/// transformation on the way out would be one more thing for the two sides to
/// disagree about.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WireDeclaration {
    /// Source order from 0 - the ordinal [`WireEdge::to_declaration`] names.
    pub ordinal: u32,
    pub start_line: u32,
    pub start_col: u32,
    pub end_line: u32,
    pub end_col: u32,
    /// Absent for a declaration with no signature of its own - a merged
    /// interface or namespace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    pub has_body: bool,
}

/// Bulk-transfer wire shape for a single graph node (one NDJSON line).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WireNode {
    pub id: String,
    pub kind: NodeKind,
    pub name: String,
    pub qualified_name: String,
    pub file_path: String,
    pub range: Range,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    pub exported: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub doc_comment: Option<String>,
    pub language: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_kind: Option<String>,
    #[serde(default)]
    pub has_syntax_errors: bool,
    /// Every declaration this symbol is written as, in source order - sent
    /// **only** when there is more than one.
    ///
    /// `skip_serializing_if` is load-bearing rather than tidiness: the design
    /// promises an ordinary single-declaration node stays byte-identical on
    /// the wire, and the plugin holds up its half by omitting the key
    /// entirely (`toWireNode` in plugins/js-ts/src/bulkIndex.ts). An empty
    /// list would be a different, and equally wrong, way to say "one
    /// declaration" - hence `Option`, not `Vec`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub declarations: Option<Vec<WireDeclaration>>,
}

/// Bulk-transfer wire shape for a single graph edge (one NDJSON line).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WireEdge {
    pub id: String,
    pub from_id: String,
    pub to_id: String,
    pub kind: EdgeKind,
    pub source: EdgeSource,
    pub resolved: bool,
    /// Which of the target's declarations this edge binds, as an ordinal into
    /// its declaration list. Set only on [`EdgeKind::Calls`], only by the
    /// semantic pass, and only when the target really has more than one call
    /// signature - so it is absent on every edge the structural pass emits,
    /// and omitted rather than sent as `null` for exactly the reason
    /// [`WireNode::declarations`] is.
    ///
    /// It is part of the edge's identity (`edgeIdFor` in
    /// plugins/js-ts/src/extract.ts), which is what lets one caller that calls
    /// two overloads of the same function record both bindings instead of one
    /// overwriting the other.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_declaration: Option<u32>,
}

/// JSON-RPC request id - either form is legal per the JSON-RPC 2.0 spec.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    Number(i64),
    String(String),
}

/// The control-plane payload shapes: reindex request, file-changed
/// notification, status query, semantic-pass request. Which of these is a
/// "request" (expects a response) vs. a "notification" (fire-and-forget) is
/// determined by whether `ControlEnvelope.id` is present, per JSON-RPC 2.0 -
/// not by this enum itself.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "camelCase")]
pub enum ControlMessage {
    // Enum-level rename_all only renames the tag ("method") value, not a
    // struct variant's own fields - each variant needs its own rename_all
    // to get filePath instead of file_path in "params".
    #[serde(rename_all = "camelCase")]
    Reindex { file_path: String },
    #[serde(rename_all = "camelCase")]
    FileChanged { file_path: String },
    Status,
    /// Asks the plugin's semantic layer to re-answer what the structural
    /// (tree-sitter) pass could only guess at, and reply with the edges it
    /// can now upgrade - see `watcher::apply::apply_semantic_pass`.
    ///
    /// Plural `file_paths`, unlike every other variant, because the two
    /// moments core sends this are different in kind: after an incremental
    /// reparse it names the one file that just settled, while after the
    /// cold-start bulk walk there is no single file to name - the whole
    /// project just became resolvable at once. An **empty** list is that
    /// second case, and means "everything indexed so far", not "nothing":
    /// a request with nothing to do would not be worth a round trip.
    #[serde(rename_all = "camelCase")]
    SemanticPass { file_paths: Vec<String> },
}

/// LSP-style JSON-RPC 2.0 envelope for the control plane. Framing
/// (`Content-Length` header + body) is handled by the transport layer, not
/// this type - this is just the JSON body shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ControlEnvelope {
    pub jsonrpc: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<RequestId>,
    #[serde(flatten)]
    pub message: ControlMessage,
}

/// Handshake payload exchanged when core spawns a language plugin.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Handshake {
    pub protocol_version: u32,
    pub language: String,
    pub plugin_version: String,
}

/// The wire-level shape of a plugin's answer to a `FileChanged` request:
/// which nodes/edges to upsert or delete. Mirrors
/// `storage::write::Diff` field-for-field (same `upsert`/`delete`
/// vocabulary, not a separate "added/removed" one) but using the
/// `WireNode`/`WireEdge` bulk-transfer shapes instead of storage records.
///
/// `SemanticPass` answers in this same shape rather than one of its own,
/// and that is not merely a convenience: a semantic upgrade *is* a diff.
/// Every node and edge here already carries its own `filePath`/`id`, so
/// nothing about the type is singular-file-specific, and an upgraded edge
/// re-sent under its existing (content-derived) id is upserted in place by
/// `storage::write::apply_diff`'s `ON CONFLICT(id) DO UPDATE`, flipping
/// exactly its `source`/`resolved` and leaving every other edge alone.
/// A separate-but-identical type would have bought nothing and given the
/// two shapes room to drift apart.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileChangeDiff {
    #[serde(default)]
    pub upsert_nodes: Vec<WireNode>,
    #[serde(default)]
    pub delete_node_ids: Vec<String>,
    #[serde(default)]
    pub upsert_edges: Vec<WireEdge>,
    #[serde(default)]
    pub delete_edge_ids: Vec<String>,
}

/// Minimal JSON-RPC 2.0 response envelope carrying a `FileChangeDiff` -
/// the counterpart to `ControlEnvelope` (which is the request/notification
/// side only). Kept as one concrete response type rather than a generic
/// `ControlResponse<T>`: both methods that answer with a diff (`FileChanged`
/// and `SemanticPass`) answer with *this* diff, so there is still only one
/// shape to be generic over.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileChangeResponse {
    pub jsonrpc: String,
    pub id: RequestId,
    pub result: FileChangeDiff,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_node_round_trips() {
        let node = WireNode {
            id: "n1".to_string(),
            kind: NodeKind::Function,
            name: "foo".to_string(),
            qualified_name: "mod::foo".to_string(),
            file_path: "src/lib.rs".to_string(),
            range: Range {
                start: Position { line: 1, col: 0 },
                end: Position { line: 3, col: 1 },
            },
            signature: Some("fn foo()".to_string()),
            exported: true,
            doc_comment: None,
            language: "rust".to_string(),
            native_kind: None,
            has_syntax_errors: false,
            declarations: None,
        };

        let json = serde_json::to_string(&node).unwrap();
        let round_tripped: WireNode = serde_json::from_str(&json).unwrap();
        assert_eq!(node, round_tripped);
        assert!(json.contains("\"qualifiedName\""));
    }

    #[test]
    fn wire_edge_round_trips() {
        let edge = WireEdge {
            id: "e1".to_string(),
            from_id: "n1".to_string(),
            to_id: "n2".to_string(),
            kind: EdgeKind::SupertypeOf,
            source: EdgeSource::TsCompiler,
            resolved: true,
            to_declaration: None,
        };

        let json = serde_json::to_string(&edge).unwrap();
        assert!(json.contains("\"SUPERTYPE_OF\""));
        assert!(json.contains("\"ts-compiler\""));
        let round_tripped: WireEdge = serde_json::from_str(&json).unwrap();
        assert_eq!(edge, round_tripped);
    }

    /// Exactly what `toWireNode` (plugins/js-ts/src/bulkIndex.ts) emits for an
    /// overloaded `parse` - copied from that plugin's own output rather than
    /// hand-written, so this asserts against the real wire bytes and not
    /// against what serde would have produced from the Rust struct.
    const OVERLOADED_NODE_LINE: &str = r#"{"id":"5ff9a3373000bb2f00e38ba616f6cd46","kind":"Function","name":"parse","qualifiedName":"parse","filePath":"src/overloads.ts","range":{"start":{"line":3,"col":7},"end":{"line":5,"col":1}},"signature":"parse(input: string): string[]","exported":true,"docComment":"Parses a value.","language":"typescript","nativeKind":"function","hasSyntaxErrors":false,"declarations":[{"ordinal":0,"startLine":1,"startCol":7,"endLine":1,"endCol":47,"hasBody":false,"signature":"parse(input: string): string[]"},{"ordinal":1,"startLine":2,"startCol":7,"endLine":2,"endCol":61,"hasBody":false,"signature":"parse(input: number, radix?: number): number"},{"ordinal":2,"startLine":3,"startCol":7,"endLine":5,"endCol":1,"hasBody":true,"signature":"parse(input: string | number, radix?: number): any"}]}"#;

    #[test]
    fn a_declaration_list_deserializes_from_what_the_plugin_actually_sends() {
        let node: WireNode = serde_json::from_str(OVERLOADED_NODE_LINE).unwrap();

        let declarations = node.declarations.as_ref().expect("an overloaded symbol carries its list");
        assert_eq!(declarations.len(), 3);
        assert_eq!(declarations[0].ordinal, 0);
        assert_eq!(declarations[0].start_line, 1);
        assert_eq!(declarations[0].end_col, 47);
        assert_eq!(declarations[0].signature.as_deref(), Some("parse(input: string): string[]"));
        assert!(!declarations[0].has_body);
        assert!(declarations[2].has_body, "the implementation is the one with a body");

        // Re-serializing has to produce the same list back, since this is the
        // shape core hands to storage.
        let round_tripped: WireNode = serde_json::from_str(&serde_json::to_string(&node).unwrap()).unwrap();
        assert_eq!(node, round_tripped);
    }

    /// The design's central promise: a node with one declaration is
    /// byte-identical to what it was before declarations existed. Serde's half
    /// of it - the plugin's half is asserted in its own suite.
    #[test]
    fn an_ordinary_node_carries_no_declarations_key_at_all() {
        let node = WireNode {
            id: "n1".to_string(),
            kind: NodeKind::Function,
            name: "foo".to_string(),
            qualified_name: "foo".to_string(),
            file_path: "src/lib.ts".to_string(),
            range: Range { start: Position { line: 1, col: 0 }, end: Position { line: 3, col: 1 } },
            signature: None,
            exported: true,
            doc_comment: None,
            language: "typescript".to_string(),
            native_kind: None,
            has_syntax_errors: false,
            declarations: None,
        };

        let json = serde_json::to_string(&node).unwrap();
        assert!(!json.contains("declarations"), "{json}");

        // And a line from a plugin that never heard of the field is still a
        // valid node, rather than a parse failure.
        let without: WireNode = serde_json::from_str(
            r#"{"id":"n1","kind":"Function","name":"foo","qualifiedName":"foo","filePath":"src/lib.ts","range":{"start":{"line":1,"col":0},"end":{"line":3,"col":1}},"exported":true,"language":"typescript"}"#,
        )
        .unwrap();
        assert_eq!(without.declarations, None);
    }

    #[test]
    fn an_edge_binding_an_overload_round_trips_and_is_omitted_when_absent() {
        let unbound = WireEdge {
            id: "e1".to_string(),
            from_id: "n1".to_string(),
            to_id: "n2".to_string(),
            kind: EdgeKind::Calls,
            source: EdgeSource::TreeSitter,
            resolved: false,
            to_declaration: None,
        };
        assert!(!serde_json::to_string(&unbound).unwrap().contains("toDeclaration"));

        let bound = WireEdge { to_declaration: Some(2), ..unbound.clone() };
        let json = serde_json::to_string(&bound).unwrap();
        assert!(json.contains("\"toDeclaration\":2"), "{json}");
        assert_eq!(serde_json::from_str::<WireEdge>(&json).unwrap(), bound);

        // Ordinal 0 is a binding like any other, and must survive the trip as
        // itself rather than collapsing into "none".
        let first = WireEdge { to_declaration: Some(0), ..unbound };
        let json = serde_json::to_string(&first).unwrap();
        assert!(json.contains("\"toDeclaration\":0"), "{json}");
        assert_eq!(serde_json::from_str::<WireEdge>(&json).unwrap().to_declaration, Some(0));
    }

    #[test]
    fn control_request_round_trips_with_id() {
        let envelope = ControlEnvelope {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id: Some(RequestId::Number(1)),
            message: ControlMessage::Reindex {
                file_path: "src/lib.rs".to_string(),
            },
        };

        let json = serde_json::to_string(&envelope).unwrap();
        let round_tripped: ControlEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(envelope, round_tripped);
    }

    #[test]
    fn control_notification_round_trips_without_id() {
        let envelope = ControlEnvelope {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id: None,
            message: ControlMessage::FileChanged {
                file_path: "src/main.rs".to_string(),
            },
        };

        let json = serde_json::to_string(&envelope).unwrap();
        assert!(!json.contains("\"id\""), "notifications must omit id per JSON-RPC 2.0");
        let round_tripped: ControlEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(envelope, round_tripped);
    }

    #[test]
    fn semantic_pass_request_round_trips_with_camel_case_params() {
        let envelope = ControlEnvelope {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id: Some(RequestId::Number(7)),
            message: ControlMessage::SemanticPass {
                file_paths: vec!["src/a.ts".to_string(), "src/b.ts".to_string()],
            },
        };

        let json = serde_json::to_string(&envelope).unwrap();
        assert!(json.contains("\"semanticPass\""), "{json}");
        assert!(json.contains("\"filePaths\""), "{json}");
        let round_tripped: ControlEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(envelope, round_tripped);
    }

    /// The post-bulk-index shape: no single file to name, so the list is
    /// empty and means "everything". It still has to be a present, valid
    /// `filePaths` array on the wire - a plugin validating strictly (as the
    /// JS/TS one does) rejects a missing one.
    #[test]
    fn a_whole_project_semantic_pass_still_carries_an_explicit_empty_list() {
        let envelope = ControlEnvelope {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id: Some(RequestId::Number(1)),
            message: ControlMessage::SemanticPass { file_paths: Vec::new() },
        };

        let json = serde_json::to_string(&envelope).unwrap();
        assert!(json.contains("\"filePaths\":[]"), "{json}");
        let round_tripped: ControlEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(envelope, round_tripped);
    }

    #[test]
    fn file_change_diff_round_trips_with_camel_case_keys() {
        let diff = FileChangeDiff {
            upsert_nodes: vec![WireNode {
                id: "n1".to_string(),
                kind: NodeKind::Function,
                name: "foo".to_string(),
                qualified_name: "mod::foo".to_string(),
                file_path: "src/lib.rs".to_string(),
                range: Range {
                    start: Position { line: 1, col: 0 },
                    end: Position { line: 3, col: 1 },
                },
                signature: None,
                exported: true,
                doc_comment: None,
                language: "rust".to_string(),
                native_kind: None,
                has_syntax_errors: false,
                declarations: None,
            }],
            delete_node_ids: vec!["n2".to_string()],
            upsert_edges: vec![WireEdge {
                id: "e1".to_string(),
                from_id: "n1".to_string(),
                to_id: "n3".to_string(),
                kind: EdgeKind::Calls,
                source: EdgeSource::TreeSitter,
                resolved: false,
                to_declaration: None,
            }],
            delete_edge_ids: vec!["e2".to_string()],
        };

        let json = serde_json::to_string(&diff).unwrap();
        assert!(json.contains("\"upsertNodes\""));
        assert!(json.contains("\"deleteNodeIds\""));
        assert!(json.contains("\"upsertEdges\""));
        assert!(json.contains("\"deleteEdgeIds\""));

        let round_tripped: FileChangeDiff = serde_json::from_str(&json).unwrap();
        assert_eq!(diff, round_tripped);
    }

    #[test]
    fn file_change_diff_round_trips_when_empty() {
        let diff = FileChangeDiff::default();
        let json = serde_json::to_string(&diff).unwrap();
        let round_tripped: FileChangeDiff = serde_json::from_str(&json).unwrap();
        assert_eq!(diff, round_tripped);
    }

    #[test]
    fn file_change_response_round_trips_with_matching_id() {
        let response = FileChangeResponse {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id: RequestId::Number(42),
            result: FileChangeDiff::default(),
        };

        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("\"result\""));
        assert!(!json.contains("\"method\""), "a response has no method field, unlike ControlEnvelope");

        let round_tripped: FileChangeResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(response, round_tripped);
    }

    #[test]
    fn handshake_payload_round_trips() {
        let example = r#"{
            "protocolVersion": 1,
            "language": "typescript",
            "pluginVersion": "0.1.0"
        }"#;

        let handshake: Handshake = serde_json::from_str(example).unwrap();
        assert_eq!(handshake.protocol_version, CURRENT_PROTOCOL_VERSION);
        assert_eq!(handshake.language, "typescript");

        let serialized = serde_json::to_string(&handshake).unwrap();
        let round_tripped: Handshake = serde_json::from_str(&serialized).unwrap();
        assert_eq!(handshake, round_tripped);
    }
}
