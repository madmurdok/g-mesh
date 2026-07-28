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
}

/// JSON-RPC request id - either form is legal per the JSON-RPC 2.0 spec.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    Number(i64),
    String(String),
}

/// The control-plane payload shapes: reindex request, file-changed
/// notification, status query. Which of these is a "request" (expects a
/// response) vs. a "notification" (fire-and-forget) is determined by
/// whether `ControlEnvelope.id` is present, per JSON-RPC 2.0 - not by this
/// enum itself.
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
/// `ControlResponse<T>` since this ticket only needs this one shape.
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
        };

        let json = serde_json::to_string(&edge).unwrap();
        assert!(json.contains("\"SUPERTYPE_OF\""));
        assert!(json.contains("\"ts-compiler\""));
        let round_tripped: WireEdge = serde_json::from_str(&json).unwrap();
        assert_eq!(edge, round_tripped);
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
            }],
            delete_node_ids: vec!["n2".to_string()],
            upsert_edges: vec![WireEdge {
                id: "e1".to_string(),
                from_id: "n1".to_string(),
                to_id: "n3".to_string(),
                kind: EdgeKind::Calls,
                source: EdgeSource::TreeSitter,
                resolved: false,
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
