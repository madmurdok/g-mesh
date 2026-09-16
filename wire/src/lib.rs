//! The g-mesh plugin wire protocol: every type that crosses the boundary
//! between core and a language plugin, and nothing else.
//!
//! # Why this is a crate and not a module of `g-mesh`
//!
//! It was a module of core (`protocol::types`) for as long as core was the
//! only Rust crate that had to speak this protocol. `plugins/sdk` (GM-284)
//! is the second, and the two ways of giving it these types were both worse
//! than moving them here:
//!
//! - **Depend on `g-mesh`.** The types could not drift, which is the whole
//!   requirement - but every plugin built on the SDK would then link core's
//!   entire dependency subtree: a statically linked ONNX Runtime, a bundled
//!   SQLite, tokio, an HTTP client. None of that is a plugin's business, and
//!   paying for it in compile time and binary size on every language g-mesh
//!   ever supports is not a cost that gets smaller.
//! - **Re-declare them in the SDK.** Free at the boundary, and wrong the
//!   first time either side changes: a wire v3 would be a silent, per-field
//!   mismatch discovered by a plugin emitting lines core refuses, rather than
//!   by a compiler. `CURRENT_PROTOCOL_VERSION` exists precisely because this
//!   protocol is code, not data (`protocol::handshake::verify`), and two
//!   hand-kept copies of code is the arrangement that makes a version number
//!   necessary in the first place.
//!
//! So the types moved into a crate whose entire dependency list is `serde`
//! (plus `schemars`, behind a default-off feature core turns on for its MCP
//! tool schemas). Core re-exports this crate wholesale as `protocol::types`,
//! so nothing inside core changed at its call sites; the SDK depends on this
//! crate directly. Sharing the declaration is what makes drift impossible,
//! and keeping it this small is what makes sharing affordable.
//!
//! The wire format itself is specified in
//! `docs/architecture/multi-language-plugins.md` ("Interfaces > Wire v2").

use serde::{Deserialize, Serialize};

/// Bumped on any breaking change to this wire contract. A mismatch between
/// core and plugin is a hard load failure - never best-effort compatibility.
///
/// `2` as of GM-275: every plugin core spawns, bundled JS/TS included, now
/// speaks wire v2 (`Visibility`, structured `PlaceholderTarget`, and
/// `SourceTier` plus `engine`) - see this file's own git history for the v1
/// shapes and the normalization that used to accept both, during the
/// migration window GM-263 opened and this task closes. A v1 sender is now
/// an ordinary handshake version mismatch, exactly like any other
/// (`protocol::handshake::verify`).
pub const CURRENT_PROTOCOL_VERSION: u32 = 2;

pub const JSONRPC_VERSION: &str = "2.0";

/// A point in a source file, as a line and a column.
// Doc comments on this type and its fields are user-facing: `JsonSchema` is
// derived so the MCP tool schemas can describe positions with the very type
// the plugin protocol and storage layer already use, and schemars copies the
// prose straight into the published schema.
//
// Behind the `json-schema` feature since this crate was split out of core
// (GM-284): core turns it on, and a plugin - which has no MCP surface and no
// schemas to publish - does not, so the SDK's dependency tree stays `serde`
// alone. The derive is the only thing the feature gates; the type, its
// fields and its wire shape are identical either way.
#[cfg_attr(feature = "json-schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
/// a plugin's semantic layer - the closed set queries and code branch on.
///
/// Replaces wire v1's `EdgeSource` (`"tree-sitter" | "ts-compiler"`), which
/// conflated the *tier* with the one pair of engines the JS/TS plugin
/// happens to have. [`WireEdge::engine`] is the free-text label that used to
/// be baked into `EdgeSource`'s two variants; splitting it out is what lets
/// a Go or Rust plugin report its own engine (`go-parser`, `rust-analyzer`,
/// ...) without a schema or protocol change - see the design doc's Data
/// Model > Edge source section.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SourceTier {
    Syntactic,
    Semantic,
}

/// Who may reach a declaration - replaces wire v1's `exported: bool` (Data
/// Model > Visibility). `exported` stays a *derived storage column*
/// (`Public` => `true`, everything else => `false`), so `get_file_outline`'s
/// output does not change; only the wire shape and the in-core type move to
/// this richer enum, which is what lets a container-scoped visibility (Go
/// unexported, Rust private, Java package-private, ...) answers
/// differently from a file-private one in `graph::symbol_links`'s
/// container-aware visibility check (GM-266; its module doc has the rules).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Visibility {
    Public,
    File,
    Container(String),
}

/// Which kind of thing a [`PlaceholderTarget`] is anchored to: a single file
/// (TS's own convention, and every language before containers exist), or a
/// logical container - a Go package, Rust module, C# namespace, ... (Data
/// Model > Logical containers). `graph::symbol_links` looks a `Container`
/// target up among that container's members (GM-266), and `graph::imports`
/// links a `Container` import onto the container node itself (GM-267).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TargetScope {
    File(String),
    Container(String),
}

/// How a [`PlaceholderTarget`] names the thing it wants inside its `scope`:
/// a bare `name` (ambiguous if several match - every structural tier's own
/// contract, `*`/`default` included, ties to this) or an exact
/// `qualifiedName` (a semantic tier's answer - matches one declaration with
/// nothing left to disambiguate). See the design doc's Data Model >
/// Structured placeholder targets and Interfaces > Linker contract sections
/// for the full rules each key gets from the linker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TargetKey {
    Name(String),
    QualifiedName(String),
}

/// What a placeholder [`WireNode`] is waiting on - replaces the `<file>#
/// <name>` convention that used to be packed into `qualifiedName` (see
/// `graph::symbol_links`'s and `graph::imports`'s module docs) with a
/// structured row, so a container scope or a semantic tier's exact-match
/// answer no longer has to be smuggled through a string two different call
/// sites each parse by their own convention.
///
/// Required (via [`WireNode::target`]) exactly when `nativeKind` is one of
/// the placeholder kinds - `pending_symbol`, `reexport`, `resolved_module` -
/// which `protocol::conformance`'s shape check enforces.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaceholderTarget {
    pub scope: TargetScope,
    pub key: TargetKey,
    /// The requester's own container, carried onto the placeholder for the
    /// container-scoped [`Visibility`] check a future linker pass runs once
    /// it has resolved a candidate. `None` for a language with no containers
    /// (every language today), same as [`WireNode::container`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_container: Option<String>,
}

/// One declaration of a symbol written as several - an overload signature
/// beside its implementation, an interface or a namespace merged across
/// statements. Mirrors the plugin's `SymbolDeclaration`
/// (plugins/typescript/src/extract.ts) exactly, flat line/col fields and all,
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
///
/// The protocol v2 shape, both received from a plugin and (in tests) sent to
/// one - every plugin core spawns speaks this shape as of GM-275, so there is
/// nothing left to normalize between deserializing and handing a `WireNode`
/// to the rest of core. (Until GM-275, this type's `Deserialize` was
/// hand-written to also accept wire v1's `exported`/derived-`target` shape
/// from the not-yet-migrated JS/TS plugin - see this file's git history if
/// that normalization is ever needed again.)
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
    pub visibility: Visibility,
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
    /// entirely (`toWireNode` in plugins/typescript/src/bulkIndex.ts). An empty
    /// list would be a different, and equally wrong, way to say "one
    /// declaration" - hence `Option`, not `Vec`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub declarations: Option<Vec<WireDeclaration>>,
    /// Logical container this declaration is a member of - a Go import path,
    /// a Rust module path, ... (Data Model > Logical containers). `None` for
    /// a language with no containers (TS/JS today, and every v1 plugin).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    /// `container`'s own parent key, sent alongside every member rather than
    /// looked up separately - core, not the plugin, materializes container
    /// nodes, so this is the only place a parent relationship is ever
    /// stated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container_parent: Option<String>,
    /// What this node is waiting to be linked onto. Required iff
    /// `native_kind` is a placeholder kind (`pending_symbol`, `reexport`,
    /// `resolved_module`) - `protocol::conformance`'s shape check enforces
    /// this on the normalized (i.e. this) form, so a v1 placeholder whose
    /// legacy address is derivable still passes. `None` for an ordinary
    /// declaration, and for a placeholder a v1 sender's legacy address could
    /// not be derived from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<PlaceholderTarget>,
}

/// Bulk-transfer wire shape for a single graph edge (one NDJSON line).
///
/// The protocol v2 shape both ways, like [`WireNode`] - a plain
/// `#[derive(Deserialize)]`, since GM-275 retired the v1 `source`-alone shape
/// this type's `Deserialize` used to also accept (see this file's git history
/// for `WireEdgeOnWire`/`normalize_source` if that shape is ever relevant
/// again).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WireEdge {
    pub id: String,
    pub from_id: String,
    pub to_id: String,
    pub kind: EdgeKind,
    pub source: SourceTier,
    pub engine: String,
    pub resolved: bool,
    /// Which of the target's declarations this edge binds, as an ordinal into
    /// its declaration list. Set only on [`EdgeKind::Calls`], only by the
    /// semantic pass, and only when the target really has more than one call
    /// signature - so it is absent on every edge the structural pass emits,
    /// and omitted rather than sent as `null` for exactly the reason
    /// [`WireNode::declarations`] is.
    ///
    /// It is part of the edge's identity (`edgeIdFor` in
    /// plugins/typescript/src/extract.ts), which is what lets one caller that calls
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
/// notification, status query, semantic-pass request, workspace-changed
/// notification. Which of these is a "request" (expects a response) vs. a
/// "notification" (fire-and-forget) is determined by whether
/// `ControlEnvelope.id` is present, per JSON-RPC 2.0 - not by this enum
/// itself.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "camelCase")]
pub enum ControlMessage {
    // Enum-level rename_all only renames the tag ("method") value, not a
    // struct variant's own fields - each variant needs its own rename_all
    // to get filePath instead of file_path in "params".
    #[serde(rename_all = "camelCase")]
    Reindex {
        file_path: String,
    },
    #[serde(rename_all = "camelCase")]
    FileChanged {
        file_path: String,
    },
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
    SemanticPass {
        file_paths: Vec<String>,
    },
    /// Tells a plugin its cached module/crate map is stale - a workspace
    /// file changed (`plugin.toml`'s `workspace.watch_files`, e.g. `go.mod`,
    /// `Cargo.toml`), so whatever it memoized about the project's module
    /// layout no longer applies. A notification, not a request: core always
    /// follows it with the per-language reindex that actually repopulates
    /// the graph, so the plugin never has to answer with a diff of its own
    /// (design doc's Interfaces > Wire v2 section).
    #[serde(rename_all = "camelCase")]
    WorkspaceChanged {
        file_path: String,
    },
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
            range: Range { start: Position { line: 1, col: 0 }, end: Position { line: 3, col: 1 } },
            signature: Some("fn foo()".to_string()),
            visibility: Visibility::Public,
            doc_comment: None,
            language: "rust".to_string(),
            native_kind: None,
            has_syntax_errors: false,
            declarations: None,
            container: None,
            container_parent: None,
            target: None,
        };

        let json = serde_json::to_string(&node).unwrap();
        let round_tripped: WireNode = serde_json::from_str(&json).unwrap();
        assert_eq!(node, round_tripped);
        assert!(json.contains("\"qualifiedName\""));
        assert!(json.contains("\"visibility\":\"public\""), "{json}");
        assert!(!json.contains("\"exported\""), "v2 serialization never emits the legacy field: {json}");
    }

    #[test]
    fn wire_edge_round_trips() {
        let edge = WireEdge {
            id: "e1".to_string(),
            from_id: "n1".to_string(),
            to_id: "n2".to_string(),
            kind: EdgeKind::SupertypeOf,
            source: SourceTier::Semantic,
            engine: "ts-compiler".to_string(),
            resolved: true,
            to_declaration: None,
        };

        let json = serde_json::to_string(&edge).unwrap();
        assert!(json.contains("\"SUPERTYPE_OF\""));
        assert!(json.contains("\"source\":\"semantic\""), "{json}");
        assert!(json.contains("\"engine\":\"ts-compiler\""), "{json}");
        let round_tripped: WireEdge = serde_json::from_str(&json).unwrap();
        assert_eq!(edge, round_tripped);
    }

    #[test]
    fn visibility_round_trips_every_variant() {
        for (visibility, json) in [
            (Visibility::Public, "\"public\""),
            (Visibility::File, "\"file\""),
            (Visibility::Container("github.com/x/pkg".to_string()), "{\"container\":\"github.com/x/pkg\"}"),
        ] {
            assert_eq!(serde_json::to_string(&visibility).unwrap(), json);
            assert_eq!(serde_json::from_str::<Visibility>(json).unwrap(), visibility);
        }
    }

    #[test]
    fn target_scope_round_trips_both_variants() {
        for (scope, json) in [
            (TargetScope::File("src/a.ts".to_string()), r#"{"file":"src/a.ts"}"#),
            (TargetScope::Container("github.com/x/pkg".to_string()), r#"{"container":"github.com/x/pkg"}"#),
        ] {
            assert_eq!(serde_json::to_string(&scope).unwrap(), json);
            assert_eq!(serde_json::from_str::<TargetScope>(json).unwrap(), scope);
        }
    }

    #[test]
    fn target_key_round_trips_both_variants() {
        for (key, json) in [
            (TargetKey::Name("foo".to_string()), r#"{"name":"foo"}"#),
            (TargetKey::QualifiedName("Server.Close".to_string()), r#"{"qualifiedName":"Server.Close"}"#),
        ] {
            assert_eq!(serde_json::to_string(&key).unwrap(), json);
            assert_eq!(serde_json::from_str::<TargetKey>(json).unwrap(), key);
        }
    }

    #[test]
    fn source_tier_round_trips_both_variants() {
        for (tier, json) in [(SourceTier::Syntactic, "\"syntactic\""), (SourceTier::Semantic, "\"semantic\"")]
        {
            assert_eq!(serde_json::to_string(&tier).unwrap(), json);
            assert_eq!(serde_json::from_str::<SourceTier>(json).unwrap(), tier);
        }
    }

    /// A full v2-native node: container membership plus a container-scoped,
    /// qualifiedName-keyed target - the shape only a semantic tier over a
    /// containered language (Go, Rust, ...) will ever actually send, but
    /// which the wire format has to carry correctly today regardless.
    #[test]
    fn wire_node_v2_shape_round_trips_container_and_target() {
        let node = WireNode {
            id: "n1".to_string(),
            kind: NodeKind::Function,
            name: "Close".to_string(),
            qualified_name: "Server.Close".to_string(),
            file_path: "server.go".to_string(),
            range: Range { start: Position { line: 4, col: 0 }, end: Position { line: 6, col: 1 } },
            signature: None,
            visibility: Visibility::Container("github.com/x/app/server".to_string()),
            doc_comment: None,
            language: "go".to_string(),
            native_kind: Some("pending_symbol".to_string()),
            has_syntax_errors: false,
            declarations: None,
            container: Some("github.com/x/app/server".to_string()),
            container_parent: None,
            target: Some(PlaceholderTarget {
                scope: TargetScope::Container("github.com/x/app/server".to_string()),
                key: TargetKey::QualifiedName("Server.Close".to_string()),
                from_container: Some("github.com/x/app/client".to_string()),
            }),
        };

        let json = serde_json::to_string(&node).unwrap();
        assert!(json.contains("\"container\":\"github.com/x/app/server\""), "{json}");
        assert!(json.contains("\"qualifiedName\":\"Server.Close\""), "{json}");
        let round_tripped: WireNode = serde_json::from_str(&json).unwrap();
        assert_eq!(node, round_tripped);
    }

    /// Exactly what `toWireNode` (plugins/typescript/src/bulkIndex.ts) emits
    /// for an overloaded `parse` - copied from that plugin's own output
    /// rather than hand-written, so this asserts against the real wire bytes
    /// and not against what serde would have produced from the Rust struct.
    const OVERLOADED_NODE_LINE: &str = r#"{"id":"5ff9a3373000bb2f00e38ba616f6cd46","kind":"Function","name":"parse","qualifiedName":"parse","filePath":"src/overloads.ts","range":{"start":{"line":3,"col":7},"end":{"line":5,"col":1}},"signature":"parse(input: string): string[]","visibility":"public","docComment":"Parses a value.","language":"typescript","nativeKind":"function","hasSyntaxErrors":false,"declarations":[{"ordinal":0,"startLine":1,"startCol":7,"endLine":1,"endCol":47,"hasBody":false,"signature":"parse(input: string): string[]"},{"ordinal":1,"startLine":2,"startCol":7,"endLine":2,"endCol":61,"hasBody":false,"signature":"parse(input: number, radix?: number): number"},{"ordinal":2,"startLine":3,"startCol":7,"endLine":5,"endCol":1,"hasBody":true,"signature":"parse(input: string | number, radix?: number): any"}]}"#;

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
        assert_eq!(node.visibility, Visibility::Public);

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
            visibility: Visibility::Public,
            doc_comment: None,
            language: "typescript".to_string(),
            native_kind: None,
            has_syntax_errors: false,
            declarations: None,
            container: None,
            container_parent: None,
            target: None,
        };

        let json = serde_json::to_string(&node).unwrap();
        assert!(!json.contains("declarations"), "{json}");

        // And a minimal line that never heard of `declarations` at all is
        // still a valid node, rather than a parse failure.
        let without: WireNode = serde_json::from_str(
            r#"{"id":"n1","kind":"Function","name":"foo","qualifiedName":"foo","filePath":"src/lib.ts","range":{"start":{"line":1,"col":0},"end":{"line":3,"col":1}},"visibility":"public","language":"typescript"}"#,
        )
        .unwrap();
        assert_eq!(without.declarations, None);
        assert_eq!(without.visibility, Visibility::Public);
    }

    #[test]
    fn an_edge_binding_an_overload_round_trips_and_is_omitted_when_absent() {
        let unbound = WireEdge {
            id: "e1".to_string(),
            from_id: "n1".to_string(),
            to_id: "n2".to_string(),
            kind: EdgeKind::Calls,
            source: SourceTier::Syntactic,
            engine: "tree-sitter".to_string(),
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

    // --- Wire v1 is rejected, not normalized (GM-275) -----------------------
    //
    // Before GM-275, `WireNode`/`WireEdge` accepted both shapes below and
    // normalized a v1 sender's `exported`/bare `source` into the v2 fields -
    // see this file's git history for the mapping tests that exercised that
    // (real `--bulk-index` output from the not-yet-migrated JS/TS plugin).
    // Every plugin core spawns speaks v2 now, so a v1-shaped line is simply a
    // parse failure like any other malformed message - the same failure mode
    // `protocol::conformance`'s `shape` check and `cli::plugin_check` surface
    // it through, and the same reasoning `handshake::verify` applies one
    // level up (a protocol is code, not data - there is nothing left to
    // reconcile once a version no longer matches).

    #[test]
    fn a_v1_shaped_node_exported_instead_of_visibility_is_rejected_with_a_clear_error() {
        let v1_line = r#"{"id":"n1","kind":"Function","name":"run","qualifiedName":"run","filePath":"caller.ts","range":{"start":{"line":3,"col":7},"end":{"line":5,"col":1}},"signature":"run(): void","exported":true,"docComment":null,"language":"typescript","nativeKind":"function","hasSyntaxErrors":false}"#;
        let err = serde_json::from_str::<WireNode>(v1_line).unwrap_err();
        assert!(err.to_string().contains("visibility"), "{err}");
    }

    #[test]
    fn a_v1_shaped_edge_bare_tree_sitter_source_instead_of_source_plus_engine_is_rejected_with_a_clear_error()
    {
        // `"tree-sitter"` was a valid v1 `source` value; it is not a
        // `SourceTier` at all in v2 ("syntactic"/"semantic" plus a separate
        // `engine"), so this now fails to parse `source` itself rather than
        // being accepted and missing `engine`.
        let v1_line =
            r#"{"id":"e1","fromId":"n1","toId":"n2","kind":"CALLS","source":"tree-sitter","resolved":false}"#;
        let err = serde_json::from_str::<WireEdge>(v1_line).unwrap_err();
        assert!(err.to_string().contains("tree-sitter"), "{err}");
        assert!(err.to_string().contains("syntactic") || err.to_string().contains("semantic"), "{err}");
    }

    #[test]
    fn a_node_missing_visibility_is_rejected() {
        let json = r#"{"id":"n1","kind":"Function","name":"foo","qualifiedName":"foo","filePath":"a.ts","range":{"start":{"line":0,"col":0},"end":{"line":0,"col":1}},"language":"typescript"}"#;
        let err = serde_json::from_str::<WireNode>(json).unwrap_err();
        assert!(err.to_string().contains("visibility"), "{err}");
    }

    #[test]
    fn an_edge_missing_source_is_rejected() {
        let json = r#"{"id":"e1","fromId":"n1","toId":"n2","kind":"CALLS","resolved":false}"#;
        let err = serde_json::from_str::<WireEdge>(json).unwrap_err();
        assert!(err.to_string().contains("source"), "{err}");
    }

    #[test]
    fn control_request_round_trips_with_id() {
        let envelope = ControlEnvelope {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id: Some(RequestId::Number(1)),
            message: ControlMessage::Reindex { file_path: "src/lib.rs".to_string() },
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
            message: ControlMessage::FileChanged { file_path: "src/main.rs".to_string() },
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
    fn workspace_changed_round_trips_as_a_notification() {
        let envelope = ControlEnvelope {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id: None,
            message: ControlMessage::WorkspaceChanged { file_path: "go.mod".to_string() },
        };

        let json = serde_json::to_string(&envelope).unwrap();
        assert!(json.contains("\"workspaceChanged\""), "{json}");
        assert!(json.contains("\"filePath\":\"go.mod\""), "{json}");
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
                range: Range { start: Position { line: 1, col: 0 }, end: Position { line: 3, col: 1 } },
                signature: None,
                visibility: Visibility::Public,
                doc_comment: None,
                language: "rust".to_string(),
                native_kind: None,
                has_syntax_errors: false,
                declarations: None,
                container: None,
                container_parent: None,
                target: None,
            }],
            delete_node_ids: vec!["n2".to_string()],
            upsert_edges: vec![WireEdge {
                id: "e1".to_string(),
                from_id: "n1".to_string(),
                to_id: "n3".to_string(),
                kind: EdgeKind::Calls,
                source: SourceTier::Syntactic,
                engine: "tree-sitter".to_string(),
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
            "protocolVersion": 2,
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
