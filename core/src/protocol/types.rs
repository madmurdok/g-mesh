use schemars::JsonSchema;
use serde::de::{self, Deserializer};
use serde::{Deserialize, Serialize};

use crate::graph::imports::RESOLVED_MODULE_NATIVE_KIND;
use crate::graph::symbol_links::{PENDING_SYMBOL_NATIVE_KIND, REEXPORT_ALL_NAME, REEXPORT_NATIVE_KIND};

/// Bumped on any breaking change to this wire contract. A mismatch between
/// core and plugin is a hard load failure - never best-effort compatibility.
///
/// Stays `1` through the wire v2 type changes below (GM-263): the bundled
/// JS/TS plugin does not speak v2 yet (that migration is GM-275), so bumping
/// this today would turn every existing plugin process into a hard load
/// failure the moment this lands. Core instead accepts both the v1 and v2
/// wire shapes for every field that changed - see the `LEGACY-V1` sites in
/// this file - and this only becomes `2` once GM-275 retires the v1 shape
/// and those sites are deleted.
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
/// unexported, Rust private, Java package-private, ...) eventually answer
/// differently from a file-private one once `graph::symbol_links`'s linker
/// grows the container-aware visibility check the design doc describes.
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
/// Model > Logical containers). Nothing in core produces `Container` yet -
/// `graph::imports`/`graph::symbol_links` only ever look a candidate up by
/// file - it exists on the wire now so a future container-aware linker pass
/// does not need another protocol change to receive one.
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
/// Always the protocol v2 shape in core's own memory and on anything core
/// serializes (design doc: "core never emits these to plugins except in
/// tests; serialize the v2 shape"). `Deserialize` is hand-written below
/// rather than derived, because the type accepts *both* the v2 shape a
/// migrated plugin sends and the v1 shape the still-unmigrated JS/TS plugin
/// sends (`visibility` vs. legacy `exported`, present `target` vs. one
/// derived from the legacy `qualifiedName` convention) - see
/// `WireNode::deserialize` and the `LEGACY-V1` sites it calls into. Every
/// caller downstream of deserialization only ever sees the v2 shape.
#[derive(Debug, Clone, PartialEq, Serialize)]
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

/// The as-sent-on-the-wire shape [`WireNode`] actually deserializes: every
/// v2 field, plus the one v1 field (`exported`) the in-core type no longer
/// has. A plain `#[derive(Deserialize)]` target, so only the *meaning* of
/// what came back - normalizing `exported`/`visibility` into one
/// `Visibility`, and deriving a legacy `target` when the wire omitted one -
/// needs hand-written code, in [`WireNode`]'s own `Deserialize` impl below.
///
/// LEGACY-V1: remove in GM-275, together with the `exported` field and the
/// normalization it feeds. Once the JS/TS plugin speaks wire v2, `exported`
/// never arrives on the wire and this shadow struct collapses back into a
/// plain `#[derive(Deserialize)]` on `WireNode` itself.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireNodeOnWire {
    id: String,
    kind: NodeKind,
    name: String,
    qualified_name: String,
    file_path: String,
    range: Range,
    #[serde(default)]
    signature: Option<String>,
    #[serde(default)]
    visibility: Option<Visibility>,
    // LEGACY-V1: remove in GM-275 - the entire v1 shape this field occupied.
    #[serde(default)]
    exported: Option<bool>,
    #[serde(default)]
    doc_comment: Option<String>,
    language: String,
    #[serde(default)]
    native_kind: Option<String>,
    #[serde(default)]
    has_syntax_errors: bool,
    #[serde(default)]
    declarations: Option<Vec<WireDeclaration>>,
    #[serde(default)]
    container: Option<String>,
    #[serde(default)]
    container_parent: Option<String>,
    #[serde(default)]
    target: Option<PlaceholderTarget>,
}

impl<'de> Deserialize<'de> for WireNode {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = WireNodeOnWire::deserialize(deserializer)?;

        // LEGACY-V1: remove in GM-275 - `visibility` is what a v2 sender
        // writes; `exported` is all a v1 sender ever sends, and the mapping
        // is exactly the Data Model's Visibility table collapsed to the two
        // rows a bare bool can distinguish.
        let visibility = match (raw.visibility, raw.exported) {
            (Some(visibility), _) => visibility,
            (None, Some(true)) => Visibility::Public,
            (None, Some(false)) => Visibility::File,
            (None, None) => {
                return Err(de::Error::custom(
                    "WireNode requires either `visibility` (protocol v2) or `exported` (legacy v1) - neither was present",
                ));
            }
        };

        // LEGACY-V1: remove in GM-275 - a v1 sender never had `target` to
        // send at all; a placeholder's address was, and still is, packed
        // into `qualifiedName` by the convention `graph::symbol_links` and
        // `graph::imports` already parse on the read side. Re-derive the
        // same v2 `target` here so every `WireNode` this type ever hands to
        // the rest of core is already the v2 shape - no call site downstream
        // has to know the legacy convention exists.
        let target =
            raw.target.or_else(|| derive_legacy_target(raw.native_kind.as_deref(), &raw.qualified_name));

        Ok(WireNode {
            id: raw.id,
            kind: raw.kind,
            name: raw.name,
            qualified_name: raw.qualified_name,
            file_path: raw.file_path,
            range: raw.range,
            signature: raw.signature,
            visibility,
            doc_comment: raw.doc_comment,
            language: raw.language,
            native_kind: raw.native_kind,
            has_syntax_errors: raw.has_syntax_errors,
            declarations: raw.declarations,
            container: raw.container,
            container_parent: raw.container_parent,
            target,
        })
    }
}

/// LEGACY-V1: remove in GM-275, together with every call site above.
///
/// Undoes the `<file>#<name>` convention `pendingSymbolQualifiedName` packs
/// into `qualifiedName` (`plugins/typescript/src/extract.ts`) for both
/// placeholder kinds that use it - `pending_symbol` and `reexport` - and the
/// plain-file-path convention `resolved_module` uses instead. Returns `None`
/// rather than an error for anything that does not fit: a wire-level parse
/// failure is the wrong layer to raise "this placeholder has no usable
/// target" - `protocol::conformance`'s shape check is, and it needs to see
/// `target: None` to say so with a clear message rather than have
/// deserialization fail with a generic "malformed NDJSON line".
///
/// Splits on the qualifiedName's own *last* `#`, the same invariant
/// `graph::symbol_links::Placeholder::parse` relies on ("a symbol name never
/// contains a #, whatever a file path might") - but computed directly from
/// the string, not by stripping a `#{name}` suffix built from the row's own
/// `name` field the way `Placeholder::parse` does. The two agree for
/// `pending_symbol`: `importedSymbol` in extract.ts always sets a pending
/// symbol's `name` to the *target's* own export name, never a local alias,
/// so it is always exactly the qualifiedName's own suffix. They disagree for
/// a renamed `reexport`: `export { a as b } from "./y"` sets `name: "b"`
/// (what *this* file publishes it as) while `qualifiedName` still ends in
/// `#a` (what `./y` actually declares) - confirmed against the real
/// plugin's own bulk-index output for that syntax (see the `REAL_V1_*`
/// fixtures in this module's tests). Matching against `name` there would
/// call an entirely ordinary renamed re-export "underivable"; splitting the
/// qualifiedName string directly gets the real target address right in both
/// the plain and the renamed case.
fn derive_legacy_target(native_kind: Option<&str>, qualified_name: &str) -> Option<PlaceholderTarget> {
    match native_kind {
        Some(kind) if kind == PENDING_SYMBOL_NATIVE_KIND || kind == REEXPORT_NATIVE_KIND => {
            let hash = qualified_name.rfind('#')?;
            let (file, rest) = qualified_name.split_at(hash);
            let target_name = &rest[1..];
            if file.is_empty() || target_name.is_empty() {
                return None;
            }
            Some(PlaceholderTarget {
                scope: TargetScope::File(file.to_string()),
                key: TargetKey::Name(target_name.to_string()),
                from_container: None,
            })
        }
        Some(kind) if kind == RESOLVED_MODULE_NATIVE_KIND => {
            // A resolved-import placeholder's legacy `qualifiedName` *is*
            // the target file's path (`graph::imports`'s module doc) - it
            // addresses the whole module, not one export of it, so there is
            // no `#<name>` to split off. There is also no legacy notion of
            // "the name of a module" to carry as the key, so this borrows
            // the `*` convention `REEXPORT_ALL_NAME` already uses elsewhere
            // for "everything this scope exports" - a deliberate stand-in
            // for the `scopeKind: "container"` representation the design
            // doc's Data Model says a `resolved_module` import gains once
            // containers exist.
            if qualified_name.is_empty() {
                return None;
            }
            Some(PlaceholderTarget {
                scope: TargetScope::File(qualified_name.to_string()),
                key: TargetKey::Name(REEXPORT_ALL_NAME.to_string()),
                from_container: None,
            })
        }
        _ => None,
    }
}

/// Bulk-transfer wire shape for a single graph edge (one NDJSON line).
///
/// Like [`WireNode`], always the protocol v2 shape in core's own memory and
/// on anything core serializes; `Deserialize` is hand-written to accept both
/// v1's `source` alone and v2's `source` + `engine` pair - see
/// `WireEdge::deserialize` and `normalize_source` below.
#[derive(Debug, Clone, PartialEq, Serialize)]
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

/// LEGACY-V1: remove in GM-275, together with `WireEdge`'s custom
/// `Deserialize` impl and `normalize_source` below.
///
/// The as-sent-on-the-wire shape: `source` and `engine` are read as raw
/// strings rather than as `SourceTier`/a required field, because a v1
/// `source` ("tree-sitter"/"ts-compiler") is not a valid `SourceTier` at
/// all. Typing this field as `Option<SourceTier>` would turn every legacy
/// edge into a hard parse error instead of a value `normalize_source` gets
/// a chance to read.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireEdgeOnWire {
    id: String,
    from_id: String,
    to_id: String,
    kind: EdgeKind,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    engine: Option<String>,
    resolved: bool,
    #[serde(default)]
    to_declaration: Option<u32>,
}

impl<'de> Deserialize<'de> for WireEdge {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = WireEdgeOnWire::deserialize(deserializer)?;
        let (source, engine) = normalize_source(raw.source, raw.engine).map_err(de::Error::custom)?;

        Ok(WireEdge {
            id: raw.id,
            from_id: raw.from_id,
            to_id: raw.to_id,
            kind: raw.kind,
            source,
            engine,
            resolved: raw.resolved,
            to_declaration: raw.to_declaration,
        })
    }
}

/// LEGACY-V1: remove in GM-275.
///
/// A v2 sender writes `source` as one of the closed `SourceTier` values
/// ("syntactic"/"semantic") plus a free `engine` label; a v1 sender writes
/// `source` as the tier and the engine conflated into one of exactly two
/// strings, and never sends `engine` at all. `engine`'s presence is what
/// tells the two apart - a v2 message always carries one alongside `source`,
/// a v1 message never does - matching the migration this module's
/// `SourceTier` doc comment and the design doc's Data Model > Edge source
/// section both describe: `tree-sitter` -> (`syntactic`, `tree-sitter`),
/// `ts-compiler` -> (`semantic`, `ts-compiler`).
fn normalize_source(source: Option<String>, engine: Option<String>) -> Result<(SourceTier, String), String> {
    match engine {
        Some(engine) => match source.as_deref() {
            Some("syntactic") => Ok((SourceTier::Syntactic, engine)),
            Some("semantic") => Ok((SourceTier::Semantic, engine)),
            Some(other) => Err(format!(
                "unknown protocol v2 edge source tier {other:?} - expected \"syntactic\" or \"semantic\""
            )),
            None => Err("WireEdge carries `engine` but no `source` - protocol v2 requires both".to_string()),
        },
        // LEGACY-V1: remove in GM-275 - no `engine` at all means the v1
        // shape, where `source` alone named tree-sitter or the ts-compiler
        // and there was no separate engine label to send.
        None => match source.as_deref() {
            Some("tree-sitter") => Ok((SourceTier::Syntactic, "tree-sitter".to_string())),
            Some("ts-compiler") => Ok((SourceTier::Semantic, "ts-compiler".to_string())),
            Some(other) => Err(format!(
                "unknown legacy edge source {other:?} - expected \"tree-sitter\" or \"ts-compiler\""
            )),
            None => Err("WireEdge requires `source` (plus `engine` for protocol v2, or alone for legacy v1)"
                .to_string()),
        },
    }
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

    /// Exactly what `toWireNode` (plugins/typescript/src/bulkIndex.ts) emits for an
    /// overloaded `parse` - copied from that plugin's own output rather than
    /// hand-written, so this asserts against the real wire bytes and not
    /// against what serde would have produced from the Rust struct. Legacy
    /// v1 shape (`exported`, no `visibility`), unaffected by GM-263 -
    /// exercises the legacy path alongside `declarations`.
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
        assert_eq!(node.visibility, Visibility::Public, "legacy exported:true maps to Visibility::Public");

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

        // And a legacy line from a plugin that never heard of `visibility`
        // (or `declarations`) is still a valid node, rather than a parse
        // failure.
        let without: WireNode = serde_json::from_str(
            r#"{"id":"n1","kind":"Function","name":"foo","qualifiedName":"foo","filePath":"src/lib.ts","range":{"start":{"line":1,"col":0},"end":{"line":3,"col":1}},"exported":true,"language":"typescript"}"#,
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

    // --- Legacy v1 -> v2 mapping ------------------------------------------
    //
    // The lines below are real `--bulk-index` output from the compiled JS/TS
    // plugin (protocol v1 - it has not migrated to v2, GM-275), captured
    // with:
    //
    //   node plugins/typescript/dist/src/index.js --bulk-index <fixture-dir>
    //
    // against a three-file fixture: `target.ts` (`export function change()`,
    // `export class Server`), `barrel.ts` (`export * from "./target"` and
    // `export { change as renamed } from "./target"`), and `caller.ts`
    // (`import { change } from "./target"; import "./barrel"; export
    // function run() { change(); }`). Used verbatim rather than hand-written
    // so the mapping tests below assert against the wire bytes a real v1
    // plugin actually sends, matching this file's existing
    // `OVERLOADED_NODE_LINE` convention.

    const REAL_V1_EXPORTED_FUNCTION_LINE: &str = r#"{"id":"54d20bd331b40e14a32ecd7c76a2706b","kind":"Function","name":"run","qualifiedName":"run","filePath":"caller.ts","range":{"start":{"line":3,"col":7},"end":{"line":5,"col":1}},"signature":"run(): void","exported":true,"docComment":null,"language":"typescript","nativeKind":"function","hasSyntaxErrors":false}"#;

    const REAL_V1_PENDING_SYMBOL_LINE: &str = r#"{"id":"b57d05fcf99ef4676464faa63d5a2348","kind":"Module","name":"change","qualifiedName":"target.ts#change","filePath":"caller.ts","range":{"start":{"line":0,"col":9},"end":{"line":0,"col":15}},"signature":null,"exported":false,"docComment":null,"language":"typescript","nativeKind":"pending_symbol","hasSyntaxErrors":false}"#;

    const REAL_V1_REEXPORT_ALL_LINE: &str = r#"{"id":"8f6f2468aeb9cef3718c9ce7804fd9c2","kind":"Module","name":"*","qualifiedName":"target.ts#*","filePath":"barrel.ts","range":{"start":{"line":0,"col":0},"end":{"line":0,"col":25}},"signature":null,"exported":false,"docComment":null,"language":"typescript","nativeKind":"reexport","hasSyntaxErrors":false}"#;

    const REAL_V1_REEXPORT_RENAMED_LINE: &str = r#"{"id":"4c1d69a2ca5c561dc3257421fbc9e147","kind":"Module","name":"renamed","qualifiedName":"target.ts#change","filePath":"barrel.ts","range":{"start":{"line":1,"col":19},"end":{"line":1,"col":26}},"signature":null,"exported":false,"docComment":null,"language":"typescript","nativeKind":"reexport","hasSyntaxErrors":false}"#;

    const REAL_V1_RESOLVED_MODULE_LINE: &str = r#"{"id":"14aa35bc4730001fc3ef95a1d740d3d1","kind":"Module","name":"./target","qualifiedName":"target.ts","filePath":"barrel.ts","range":{"start":{"line":0,"col":14},"end":{"line":0,"col":24}},"signature":null,"exported":false,"docComment":null,"language":"typescript","nativeKind":"resolved_module","hasSyntaxErrors":false}"#;

    const REAL_V1_CALLS_EDGE_LINE: &str = r#"{"id":"6142d896873f4357ea36b1b683bc0bd9","fromId":"54d20bd331b40e14a32ecd7c76a2706b","toId":"b57d05fcf99ef4676464faa63d5a2348","kind":"CALLS","source":"tree-sitter","resolved":false}"#;

    #[test]
    fn legacy_exported_true_maps_to_visibility_public() {
        let node: WireNode = serde_json::from_str(REAL_V1_EXPORTED_FUNCTION_LINE).unwrap();
        assert_eq!(node.visibility, Visibility::Public);
        assert_eq!(node.target, None, "an ordinary function is not a placeholder");
    }

    #[test]
    fn legacy_exported_false_maps_to_visibility_file() {
        let node: WireNode = serde_json::from_str(REAL_V1_PENDING_SYMBOL_LINE).unwrap();
        assert_eq!(node.visibility, Visibility::File);
    }

    #[test]
    fn legacy_pending_symbol_placeholder_derives_a_file_scoped_name_target() {
        let node: WireNode = serde_json::from_str(REAL_V1_PENDING_SYMBOL_LINE).unwrap();
        assert_eq!(
            node.target,
            Some(PlaceholderTarget {
                scope: TargetScope::File("target.ts".to_string()),
                key: TargetKey::Name("change".to_string()),
                from_container: None,
            })
        );
    }

    #[test]
    fn legacy_whole_module_reexport_derives_the_reexport_all_name() {
        let node: WireNode = serde_json::from_str(REAL_V1_REEXPORT_ALL_LINE).unwrap();
        assert_eq!(
            node.target,
            Some(PlaceholderTarget {
                scope: TargetScope::File("target.ts".to_string()),
                key: TargetKey::Name(REEXPORT_ALL_NAME.to_string()),
                from_container: None,
            })
        );
    }

    /// The case a row-name-driven split gets wrong: `export { change as
    /// renamed } from "./target"` publishes under `name: "renamed"` while
    /// `qualifiedName` still ends in the *target's* real name, `#change`.
    /// The derived target must name the real declaration (`change`), not
    /// the published alias - see `derive_legacy_target`'s doc comment.
    #[test]
    fn legacy_renamed_reexport_derives_the_real_target_name_not_the_published_alias() {
        let node: WireNode = serde_json::from_str(REAL_V1_REEXPORT_RENAMED_LINE).unwrap();
        assert_eq!(node.name, "renamed", "the row still reports what this file publishes it as");
        assert_eq!(
            node.target,
            Some(PlaceholderTarget {
                scope: TargetScope::File("target.ts".to_string()),
                key: TargetKey::Name("change".to_string()),
                from_container: None,
            }),
            "the target must be the real declaration the re-export forwards to, not the published alias"
        );
    }

    #[test]
    fn legacy_resolved_module_derives_a_whole_module_target() {
        let node: WireNode = serde_json::from_str(REAL_V1_RESOLVED_MODULE_LINE).unwrap();
        assert_eq!(
            node.target,
            Some(PlaceholderTarget {
                scope: TargetScope::File("target.ts".to_string()),
                key: TargetKey::Name(REEXPORT_ALL_NAME.to_string()),
                from_container: None,
            })
        );
    }

    #[test]
    fn legacy_edge_source_tree_sitter_maps_to_syntactic_tier_and_engine() {
        let edge: WireEdge = serde_json::from_str(REAL_V1_CALLS_EDGE_LINE).unwrap();
        assert_eq!(edge.source, SourceTier::Syntactic);
        assert_eq!(edge.engine, "tree-sitter");
    }

    #[test]
    fn legacy_edge_source_ts_compiler_maps_to_semantic_tier_and_engine() {
        let json =
            r#"{"id":"e1","fromId":"n1","toId":"n2","kind":"CALLS","source":"ts-compiler","resolved":true}"#;
        let edge: WireEdge = serde_json::from_str(json).unwrap();
        assert_eq!(edge.source, SourceTier::Semantic);
        assert_eq!(edge.engine, "ts-compiler");
    }

    #[test]
    fn v2_source_and_engine_are_kept_apart_from_legacy_strings() {
        let json = r#"{"id":"e1","fromId":"n1","toId":"n2","kind":"CALLS","source":"syntactic","engine":"go-parser","resolved":false}"#;
        let edge: WireEdge = serde_json::from_str(json).unwrap();
        assert_eq!(edge.source, SourceTier::Syntactic);
        assert_eq!(edge.engine, "go-parser");
    }

    #[test]
    fn a_node_with_neither_visibility_nor_legacy_exported_is_rejected() {
        let json = r#"{"id":"n1","kind":"Function","name":"foo","qualifiedName":"foo","filePath":"a.ts","range":{"start":{"line":0,"col":0},"end":{"line":0,"col":1}},"language":"typescript"}"#;
        let err = serde_json::from_str::<WireNode>(json).unwrap_err();
        assert!(err.to_string().contains("visibility"), "{err}");
    }

    #[test]
    fn an_edge_with_neither_v2_nor_legacy_source_is_rejected() {
        let json = r#"{"id":"e1","fromId":"n1","toId":"n2","kind":"CALLS","resolved":false}"#;
        let err = serde_json::from_str::<WireEdge>(json).unwrap_err();
        assert!(err.to_string().contains("source"), "{err}");
    }

    /// A pending_symbol whose qualifiedName carries no `#<name>` suffix at
    /// all - nothing in the wire tells us what it was waiting on.
    /// Deserialization must not fabricate a guess; `protocol::conformance`'s
    /// shape check is what turns a still-`None` target here into a reported
    /// violation with a clear message (see `conformance.rs`'s
    /// `placeholder_with_no_derivable_legacy_target_is_a_violation`).
    #[test]
    fn an_underivable_legacy_placeholder_gets_no_target_rather_than_a_guess() {
        let json = r#"{"id":"n1","kind":"Module","name":"foo","qualifiedName":"not-the-convention","filePath":"a.ts","range":{"start":{"line":0,"col":0},"end":{"line":0,"col":1}},"exported":false,"language":"typescript","nativeKind":"pending_symbol"}"#;
        let node: WireNode = serde_json::from_str(json).unwrap();
        assert_eq!(node.target, None);
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
