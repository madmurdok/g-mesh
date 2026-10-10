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
    /// The segments of a [`TargetKey::QualifiedName`] key, joining back to
    /// that key exactly (see [`QualifiedPath`]). Absent for a `name` key, and
    /// for a plugin that sends no paths; core then treats the key as a single
    /// opaque string.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_path: Option<QualifiedPath>,
}

/// One segment of a [`QualifiedPath`]: the separator text that joins it to
/// the segment before it, and its name. `sep` is absent on the first segment
/// and present, non-empty, on every later one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathSegment {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sep: Option<String>,
    pub name: String,
}

/// A qualified name as the plugin's own segments, each carrying the
/// separator the language writes before it. Core only ever concatenates
/// separators and names; it never splits a string into segments.
///
/// Joined (`display`), a valid path equals the display string it accompanies
/// (`qualifiedName`, or a placeholder's `qualifiedName` key). The element
/// rules are [`QualifiedPath::check`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct QualifiedPath(pub Vec<PathSegment>);

/// Characters no segment may contain: storage joins segments with U+001F.
pub const PATH_FORBIDDEN_CHARS: [char; 2] = ['\u{1f}', '\0'];

impl QualifiedPath {
    /// A one-segment path.
    pub fn root(name: impl Into<String>) -> Self {
        Self(vec![PathSegment { sep: None, name: name.into() }])
    }

    /// This path extended by one segment written after `sep`.
    pub fn child(&self, sep: impl Into<String>, name: impl Into<String>) -> Self {
        let mut segments = self.0.clone();
        segments.push(PathSegment { sep: Some(sep.into()), name: name.into() });
        Self(segments)
    }

    pub fn segments(&self) -> &[PathSegment] {
        &self.0
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Every separator and name concatenated in order.
    pub fn display(&self) -> String {
        self.suffix_from(0)
    }

    /// All segments but the last, or `None` for a path of fewer than two.
    pub fn head(&self) -> Option<QualifiedPath> {
        (self.0.len() >= 2).then(|| Self(self.0[..self.0.len() - 1].to_vec()))
    }

    pub fn last(&self) -> Option<&PathSegment> {
        self.0.last()
    }

    /// The display text of the segments from `start` on: `start`'s name,
    /// then each later segment's separator and name. Empty past the end.
    pub fn suffix_from(&self, start: usize) -> String {
        let mut text = String::new();
        for (i, segment) in self.0.iter().enumerate().skip(start) {
            if i > start {
                text.push_str(segment.sep.as_deref().unwrap_or(""));
            }
            text.push_str(&segment.name);
        }
        text
    }

    /// The element rules every path obeys: at least one segment, no empty
    /// name, no separator on the first segment, a non-empty separator on
    /// every later one, and no [`PATH_FORBIDDEN_CHARS`] anywhere.
    pub fn check(&self) -> Result<(), PathError> {
        if self.0.is_empty() {
            return Err(PathError::Empty);
        }
        for (i, segment) in self.0.iter().enumerate() {
            if segment.name.is_empty() {
                return Err(PathError::EmptyName(i));
            }
            match (i, segment.sep.as_deref()) {
                (0, Some(_)) => return Err(PathError::SeparatorOnFirstSegment),
                (0, None) => {}
                (_, None | Some("")) => return Err(PathError::MissingSeparator(i)),
                (_, Some(_)) => {}
            }
            let text = [segment.sep.as_deref().unwrap_or(""), segment.name.as_str()];
            if text.iter().any(|part| part.contains(PATH_FORBIDDEN_CHARS)) {
                return Err(PathError::ForbiddenChar(i));
            }
        }
        Ok(())
    }

    /// [`QualifiedPath::check`], plus: joined, the path equals `display`.
    pub fn check_joins_to(&self, display: &str) -> Result<(), PathError> {
        self.check()?;
        let joined = self.display();
        if joined != display {
            return Err(PathError::DoesNotJoin { joined, expected: display.to_string() });
        }
        Ok(())
    }
}

/// Why a [`QualifiedPath`] breaks a rule. Segment indices are from 0.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathError {
    Empty,
    EmptyName(usize),
    SeparatorOnFirstSegment,
    MissingSeparator(usize),
    ForbiddenChar(usize),
    DoesNotJoin { joined: String, expected: String },
    LastNameIsNotName { last: String, name: String },
    AliasTooShort,
    AliasEqualsPath,
    KeyPathOnNameKey,
    AliasWithoutPath,
}

impl std::fmt::Display for PathError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PathError::Empty => write!(f, "the path has no segments"),
            PathError::EmptyName(i) => write!(f, "segment {i} has an empty name"),
            PathError::SeparatorOnFirstSegment => write!(f, "the first segment carries a separator"),
            PathError::MissingSeparator(i) => write!(f, "segment {i} has no separator"),
            PathError::ForbiddenChar(i) => write!(f, "segment {i} contains U+001F or NUL"),
            PathError::DoesNotJoin { joined, expected } => {
                write!(f, "the path joins to {joined:?}, not {expected:?}")
            }
            PathError::LastNameIsNotName { last, name } => {
                write!(f, "the last segment is {last:?}, not the node's name {name:?}")
            }
            PathError::AliasTooShort => write!(f, "an alias path has fewer than two segments"),
            PathError::AliasEqualsPath => write!(f, "an alias path equals the node's qualifiedPath"),
            PathError::KeyPathOnNameKey => write!(f, "a keyPath accompanies a name key"),
            PathError::AliasWithoutPath => write!(f, "an alias path was sent without a qualifiedPath"),
        }
    }
}

impl std::error::Error for PathError {}

/// One declaration of a symbol written as several - an overload signature
/// beside its implementation, an interface or a namespace merged across
/// statements. Mirrors the plugin's declaration shape
/// (see `plugins/sdk/src/graph.rs`) exactly, flat line/col fields and all,
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
    /// entirely (`plugins/sdk/src/run.rs`'s `write_graph`). An empty
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
    /// `qualifiedName` as segments, for a declaration: it joins back to
    /// `qualified_name` and its last name is `name`. Absent for placeholders,
    /// `File` nodes and any plugin that sends no paths. Never part of the id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub qualified_path: Option<QualifiedPath>,
    /// Other spellings of this same declaration (a Rust trait-impl method
    /// written without its `<X as T>` segment, say). Each obeys
    /// [`QualifiedPath::check`], has at least two segments, ends in `name`
    /// and differs from `qualified_path`, which must be present; none need
    /// join to `qualified_name`. Only feeds partial-path lookup: never stored
    /// on the node, printed or hashed into an id.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub alias_paths: Vec<QualifiedPath>,
    /// Bare names of methods this node calls through a receiver whose type
    /// the structural tier did not know, sorted and deduplicated. Only on a
    /// `File` or `Function` node, and only from a plugin that opts in; an
    /// absent key means "not reported", not "none". Write-side only: feeds
    /// core's `untyped_calls` table, never printed or hashed into an id.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub untyped_calls: Vec<String>,
}

impl WireNode {
    /// The declaration-path rules beyond [`QualifiedPath::check`]: joins to
    /// `qualified_name` and ends in `name`. `Ok` when there is no path.
    pub fn check_qualified_path(&self) -> Result<(), PathError> {
        let Some(path) = &self.qualified_path else { return Ok(()) };
        path.check_joins_to(&self.qualified_name)?;
        self.check_ends_in_name(path)
    }

    /// The rules for one of [`WireNode::alias_paths`].
    pub fn check_alias_path(&self, alias: &QualifiedPath) -> Result<(), PathError> {
        if self.qualified_path.is_none() {
            return Err(PathError::AliasWithoutPath);
        }
        alias.check()?;
        if alias.len() < 2 {
            return Err(PathError::AliasTooShort);
        }
        if self.qualified_path.as_ref() == Some(alias) {
            return Err(PathError::AliasEqualsPath);
        }
        self.check_ends_in_name(alias)
    }

    fn check_ends_in_name(&self, path: &QualifiedPath) -> Result<(), PathError> {
        match path.last() {
            Some(last) if last.name == self.name => Ok(()),
            last => Err(PathError::LastNameIsNotName {
                last: last.map(|segment| segment.name.clone()).unwrap_or_default(),
                name: self.name.clone(),
            }),
        }
    }
}

impl PlaceholderTarget {
    /// `key_path` joins back to a `qualifiedName` key, and is absent for a
    /// `name` key. `Ok` when there is no path.
    pub fn check_key_path(&self) -> Result<(), PathError> {
        let Some(path) = &self.key_path else { return Ok(()) };
        match &self.key {
            TargetKey::QualifiedName(key) => path.check_joins_to(key),
            TargetKey::Name(_) => Err(PathError::KeyPathOnNameKey),
        }
    }
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
    /// It is part of the edge's identity (`edge_id` in
    /// plugins/sdk/src/ids.rs), which is what lets one caller that calls
    /// two overloads of the same function record both bindings instead of one
    /// overwriting the other.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_declaration: Option<u32>,
    /// **`IMPORTS` edges only:** the import's raw text as written in the
    /// importing file (a module specifier, a crate path, a dotted name). Core
    /// stores it beside the edge, and it survives linking, so a
    /// [`ImportMatch::Specifier`] selector can still find the importer after
    /// the edge was repointed onto its target. Absent on every other edge,
    /// and from a plugin that does not declare `resolution_delta`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub specifier: Option<String>,
}

/// JSON-RPC request id - either form is legal per the JSON-RPC 2.0 spec.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    Number(i64),
    String(String),
}

/// One structural edge core's linker moved, as `SemanticPass` carries it:
/// the edge's id and the declaration its `toId` now names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkedEdge {
    pub edge_id: String,
    pub to_id: String,
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
        /// Extract the file even if its text is what the plugin last
        /// extracted: something the extraction reads besides the text (a
        /// resolution config) changed. The plugin keeps its baseline, so the
        /// answer is still a diff against what it last sent. Absent means
        /// `false`.
        #[serde(default, skip_serializing_if = "is_false")]
        reextract: bool,
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
        /// Every structural edge of the pass's scope that core's linker
        /// moved onto a declaration, with the target it moved it to. A
        /// semantic tier compares its answer with these, so an answer that
        /// lands where core already linked the edge is agreement, not a
        /// contradiction - however many re-exports the linker walked to get
        /// there (`docs/adr/0029-core-ships-its-link-result-to-the-semantic-tier.md`).
        /// Absent and empty are the same: nothing linked in scope.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        linked_edges: Vec<LinkedEdge>,
        /// How long core waits for this pass's answer, in milliseconds: the
        /// round-trip timeout core applies to this very request (GM-521) -
        /// `semantic_pass_project_timeout(n)` for a whole-project or residual
        /// pass, the per-file timeout for a per-file one. A plugin that plans
        /// its work against a clock plans inside it, since core kills a
        /// plugin that answers later. Optional, no protocol bump: absent
        /// means unknown (an older core), and a plugin that ignores it keeps
        /// its own budgets.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        budget_ms: Option<u64>,
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
    /// Tells a plugin that a whole-project `semanticPass` is owed and will be
    /// asked for, so it may start its semantic engine now rather than inside
    /// that request - an engine that takes seconds to become ready then does
    /// so while core walks and asks other languages. A notification: nothing
    /// is answered, and readiness is still decided inside the pass itself.
    ///
    /// Sent only to a plugin whose manifest declares both
    /// `capabilities.semantic_pass` and `capabilities.semantic_prepare`, never
    /// to a language whose semantic tier is suspended, and only when that
    /// language's pass really is owed - so it never starts an engine for
    /// structural work, which is the lazy-engine contract's whole point.
    PrepareSemanticPass,
    /// Tells a plugin that every listed file was created in one watcher batch,
    /// before any of them is sent as `fileChanged`, so its project model knows
    /// all of them before it extracts the first. A notification: nothing is
    /// answered, and each file still gets its own `fileChanged`, which must
    /// find the hook idempotent (ADR 0023's `file_presence_changed`).
    ///
    /// Sent only to a plugin whose manifest declares
    /// `capabilities.files_created`, and only for a language that received at
    /// least two created files in the batch.
    #[serde(rename_all = "camelCase")]
    FilesCreated {
        file_paths: Vec<String>,
    },
    /// A request: a watch file (`plugin.toml`'s `workspace.watch_files`)
    /// changed. The plugin reloads its project model and compares the facts
    /// its resolution reads with `previous_facts`, the opaque blob it handed
    /// core with the rows the index holds now. It answers with a
    /// [`ResolutionChangedResult`]: what the edit changed for resolution, and
    /// the new blob.
    ///
    /// Sent only to a plugin whose manifest declares
    /// `capabilities.resolution_delta`; every other plugin gets
    /// [`ControlMessage::WorkspaceChanged`] and a whole-language reindex.
    /// `previous_facts` absent means core holds none, and the plugin answers
    /// [`ResolutionDelta::Unknown`].
    #[serde(rename_all = "camelCase")]
    ResolutionChanged {
        file_path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        previous_facts: Option<String>,
    },
}

/// The JSON-RPC 2.0 response to [`ControlMessage::ResolutionChanged`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResolutionChangedResponse {
    pub jsonrpc: String,
    pub id: RequestId,
    pub result: ResolutionChangedResult,
}

/// A plugin's answer to [`ControlMessage::ResolutionChanged`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolutionChangedResult {
    pub delta: ResolutionDelta,
    /// The resolution facts of the reloaded model, which core stores in place
    /// of the previous ones once it has acted on `delta`. Absent: core keeps
    /// none, and the next edit falls back to a whole-language reindex.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub facts: Option<String>,
}

/// What a watch-file edit changed for resolution.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ResolutionDelta {
    /// Nothing resolution reads changed: no file is re-extracted.
    Unchanged,
    /// The plugin cannot say what changed: core reindexes the whole language.
    Unknown {
        #[serde(default)]
        reason: String,
    },
    /// Re-extract every indexed file of the language inside one of `files`,
    /// and every importer one of `imports` selects.
    Affected {
        #[serde(default)]
        files: Vec<PathScope>,
        #[serde(default)]
        imports: Vec<ImportSelector>,
    },
}

/// A set of project-relative paths: those under `under` and under none of
/// `not_under`. A directory matches on a `/` boundary; `""` is the whole
/// project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PathScope {
    pub under: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub not_under: Vec<String>,
}

impl PathScope {
    /// Whether `path` lies in this scope.
    pub fn contains(&self, path: &str) -> bool {
        path_is_under(path, &self.under) && !self.not_under.iter().any(|dir| path_is_under(path, dir))
    }
}

fn path_is_under(path: &str, dir: &str) -> bool {
    let dir = dir.trim_end_matches('/');
    dir.is_empty() || path == dir || (path.starts_with(dir) && path.as_bytes().get(dir.len()) == Some(&b'/'))
}

/// The importing files whose `IMPORTS` edges `by` matches, restricted to
/// those in `importers`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportSelector {
    pub importers: PathScope,
    pub by: ImportMatch,
}

/// Which part of a stored import a selector matches.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ImportMatch {
    /// The import's raw text ([`WireEdge::specifier`]).
    Specifier(Matcher),
    /// What the import is stored as pointing at: a linked edge's target
    /// file path or container key, or an unlinked placeholder's scope.
    #[serde(rename_all = "camelCase")]
    Target { scope_kind: TargetScopeKind, matcher: Matcher },
}

/// Which kind of stored target an [`ImportMatch::Target`] compares.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TargetScopeKind {
    File,
    Container,
}

/// A test on one string.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Matcher {
    Exact(String),
    /// `s == prefix`, or `s` starts with `prefix` followed by `separator`:
    /// `pkg.sub` does not match `pkg.subtle`.
    #[serde(rename_all = "camelCase")]
    Under {
        prefix: String,
        separator: String,
    },
    StartsWith(String),
    /// A TypeScript-style non-relative specifier: one that starts with none
    /// of `.`, `/` and `#`.
    NonRelative,
}

impl Matcher {
    /// Whether `s` passes this test.
    pub fn matches(&self, s: &str) -> bool {
        match self {
            Matcher::Exact(exact) => s == exact,
            Matcher::Under { prefix, separator } => {
                s == prefix
                    || (!separator.is_empty()
                        && s.strip_prefix(prefix.as_str())
                            .is_some_and(|rest| rest.starts_with(separator.as_str())))
            }
            Matcher::StartsWith(prefix) => s.starts_with(prefix.as_str()),
            Matcher::NonRelative => !s.is_empty() && !s.starts_with(['.', '/', '#']),
        }
    }
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
    /// **`fileChanged` only:** `upsertNodes`/`upsertEdges` are the whole
    /// file, not a change against what this process last sent. A plugin sets
    /// it when it has no baseline for the file (a cold process, a restart, a
    /// file it never extracted), because it cannot then name what the file
    /// no longer has. Core deletes the file's stored nodes the diff does not
    /// upsert, and their non-`semantic` outgoing edges likewise
    /// (`core::storage::file_rows`).
    ///
    /// Absent means `false`: the diff names its own deletes. Ignored on a
    /// `semanticPass` answer.
    #[serde(default, skip_serializing_if = "is_false")]
    pub complete: bool,
    /// **`fileChanged` only:** this edit changed the plugin's project model
    /// (a Rust `mod` item added, removed or moved), and these other files
    /// may now extract differently. Core selects and re-extracts them as it
    /// does for a watch-file save's [`ResolutionDelta`]; `Unknown` reindexes
    /// the language.
    ///
    /// Absent means the edit changed no other file's extraction. Ignored on
    /// a `semanticPass` answer and on a re-extract round trip's answer.
    /// Design: `docs/architecture/gm-507-rust-module-tree-refresh.md`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub affected: Option<ResolutionDelta>,
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
    /// **`semanticPass` only:** the pass answered with what it managed and
    /// did *not* cover everything it was asked about - a per-request or
    /// per-pass budget ran out, the engine's server died, or it never became
    /// ready (GM-289).
    ///
    /// Core applies the diff either way and, for a whole-project pass, leaves
    /// `language_state.semanticPassAt` unset so the next daemon start asks
    /// again (`daemon::semantic`, `watcher::apply::apply_semantic_pass`). That
    /// is the whole reason this is a field beside `result` rather than a
    /// JSON-RPC `error`: an error answer carries no diff, so reporting "the
    /// last hundred sites are missing" would throw away the nine thousand this
    /// pass did resolve, and the retry would have to redo all of them from
    /// nothing.
    ///
    /// Absent means `false`, so a plugin that predates this field - any
    /// third-party one that has not adopted it - keeps answering exactly as
    /// it did: a pass that answers at all is a pass that finished. That is
    /// also why this is not an enum: the only
    /// thing core branches on is "was this pass complete"; the reason it was
    /// not travels separately, in words, as [`Self::incomplete_reason`].
    #[serde(default, skip_serializing_if = "is_false")]
    pub incomplete: bool,
    /// **`semanticPass` only, and only beside `incomplete: true`:** why the
    /// pass did not cover everything, in words, for core to record per
    /// language and show in `g-mesh status`. Optional on the wire: absent
    /// means the plugin gave no reason, and core records a generic one.
    #[serde(rename = "incompleteReason", default, skip_serializing_if = "Option::is_none")]
    pub incomplete_reason: Option<String>,
    /// **`semanticPass` only:** the files of this pass's scope it did not
    /// finish. Absent = unknown, and core keeps its behaviour from
    /// before this field (a complete per-file pass settles the files it sent,
    /// an incomplete one settles none); present and empty = every file in
    /// scope finished. Read whatever [`Self::incomplete`] says, so a per-file
    /// pass can name its unfinished files without core logging it as
    /// incomplete. Core keeps the named files and puts them into the scope of
    /// the next per-file pass itself, so `sent - unfinishedFiles` is exactly
    /// what a pass settled.
    #[serde(rename = "unfinishedFiles", default, skip_serializing_if = "Option::is_none")]
    pub unfinished_files: Option<Vec<String>>,
}

/// `skip_serializing_if` for a `bool` that is absent-means-false on the wire.
/// A free function because `bool` has no inherent method with the right
/// signature, and spelling it `std::ops::Not::not` would read as cleverness
/// rather than as "do not send the default".
fn is_false(value: &bool) -> bool {
    !*value
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
            alias_paths: Vec::new(),
            untyped_calls: Vec::new(),
            qualified_path: None,
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
            specifier: None,
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
                key_path: None,
            }),
            alias_paths: Vec::new(),
            untyped_calls: Vec::new(),
            qualified_path: None,
        };

        let json = serde_json::to_string(&node).unwrap();
        assert!(json.contains("\"container\":\"github.com/x/app/server\""), "{json}");
        assert!(json.contains("\"qualifiedName\":\"Server.Close\""), "{json}");
        let round_tripped: WireNode = serde_json::from_str(&json).unwrap();
        assert_eq!(node, round_tripped);
    }

    /// Exactly what the plugin's bulk-index stream emits
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
            alias_paths: Vec::new(),
            untyped_calls: Vec::new(),
            qualified_path: None,
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
            specifier: None,
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
            message: ControlMessage::FileChanged { file_path: "src/main.rs".to_string(), reextract: false },
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
                linked_edges: Vec::new(),
                budget_ms: None,
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
            message: ControlMessage::SemanticPass {
                file_paths: Vec::new(),
                linked_edges: Vec::new(),
                budget_ms: None,
            },
        };

        let json = serde_json::to_string(&envelope).unwrap();
        assert!(json.contains("\"filePaths\":[]"), "{json}");
        let round_tripped: ControlEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(envelope, round_tripped);
    }

    /// `linkedEdges` is optional on the wire both ways: a pass with nothing
    /// linked omits the key (so a plugin that predates the field sees the
    /// request it always did), and a request without the key reads as empty.
    #[test]
    fn semantic_pass_linked_edges_are_omitted_when_empty_and_read_as_empty_when_absent() {
        let empty = ControlEnvelope {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id: Some(RequestId::Number(3)),
            message: ControlMessage::SemanticPass {
                file_paths: vec!["src/a.rs".to_string()],
                linked_edges: Vec::new(),
                budget_ms: None,
            },
        };
        let json = serde_json::to_string(&empty).unwrap();
        assert!(!json.contains("linkedEdges"), "an empty list must not be serialized: {json}");
        let absent: ControlEnvelope = serde_json::from_str(&json).unwrap();
        match absent.message {
            ControlMessage::SemanticPass { linked_edges, .. } => {
                assert!(linked_edges.is_empty(), "an absent field must read as empty: {linked_edges:?}")
            }
            other => panic!("expected SemanticPass, got {other:?}"),
        }

        let linked = ControlEnvelope {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id: Some(RequestId::Number(4)),
            message: ControlMessage::SemanticPass {
                file_paths: Vec::new(),
                linked_edges: vec![LinkedEdge { edge_id: "x".to_string(), to_id: "d".to_string() }],
                budget_ms: None,
            },
        };
        let json = serde_json::to_string(&linked).unwrap();
        assert!(json.contains(r#""linkedEdges":[{"edgeId":"x","toId":"d"}]"#), "{json}");
        let round_tripped: ControlEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(linked, round_tripped);
    }

    /// `budgetMs` is optional both ways: `None` omits the key, so a
    /// plugin that predates it sees the request it always did; a frame
    /// without it reads as `None`; a value round-trips under its camelCase name.
    #[test]
    fn semantic_pass_budget_ms_round_trips_and_is_omitted_when_none() {
        let none = ControlEnvelope {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id: Some(RequestId::Number(5)),
            message: ControlMessage::SemanticPass {
                file_paths: vec!["src/a.rs".to_string()],
                linked_edges: Vec::new(),
                budget_ms: None,
            },
        };
        let json = serde_json::to_string(&none).unwrap();
        assert!(!json.contains("budgetMs"), "None must not be serialized: {json}");
        let absent: ControlEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(none, absent, "an absent budgetMs must read as None");

        let some = ControlEnvelope {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id: Some(RequestId::Number(6)),
            message: ControlMessage::SemanticPass {
                file_paths: Vec::new(),
                linked_edges: Vec::new(),
                budget_ms: Some(120_000),
            },
        };
        let json = serde_json::to_string(&some).unwrap();
        assert!(json.contains(r#""budgetMs":120000"#), "{json}");
        let round_tripped: ControlEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(some, round_tripped);
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

    /// The SDK matches on this exact method string, with no params.
    #[test]
    fn prepare_semantic_pass_round_trips_as_a_notification() {
        let envelope = ControlEnvelope {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id: None,
            message: ControlMessage::PrepareSemanticPass,
        };

        let json = serde_json::to_string(&envelope).unwrap();
        assert_eq!(json, r#"{"jsonrpc":"2.0","method":"prepareSemanticPass"}"#);
        let round_tripped: ControlEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(envelope, round_tripped);
    }

    /// The SDK reads `params.filePaths`; a notification, so no `id`.
    #[test]
    fn files_created_round_trips_as_a_notification() {
        let envelope = ControlEnvelope {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id: None,
            message: ControlMessage::FilesCreated {
                file_paths: vec!["src/a.ts".to_string(), "src/b.ts".to_string()],
            },
        };

        let json = serde_json::to_string(&envelope).unwrap();
        assert_eq!(
            json,
            r#"{"jsonrpc":"2.0","method":"filesCreated","params":{"filePaths":["src/a.ts","src/b.ts"]}}"#
        );
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
                alias_paths: Vec::new(),
                untyped_calls: Vec::new(),
                qualified_path: None,
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
                specifier: None,
            }],
            delete_edge_ids: vec!["e2".to_string()],
            complete: true,
            affected: None,
        };

        let json = serde_json::to_string(&diff).unwrap();
        assert!(json.contains("\"complete\":true"));
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
    fn file_change_diff_without_complete_reads_as_partial_and_omits_it() {
        let diff: FileChangeDiff = serde_json::from_str(
            r#"{"upsertNodes":[],"deleteNodeIds":[],"upsertEdges":[],"deleteEdgeIds":[]}"#,
        )
        .unwrap();
        assert!(!diff.complete);
        assert!(!serde_json::to_string(&diff).unwrap().contains("complete"));
    }

    #[test]
    fn file_change_response_round_trips_with_matching_id() {
        let response = FileChangeResponse {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id: RequestId::Number(42),
            result: FileChangeDiff::default(),
            incomplete: false,
            incomplete_reason: None,
            unfinished_files: None,
        };

        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("\"result\""));
        assert!(!json.contains("\"method\""), "a response has no method field, unlike ControlEnvelope");
        assert!(!json.contains("\"incomplete\""), "a complete pass says nothing: {json}");

        let round_tripped: FileChangeResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(response, round_tripped);
    }

    /// The field a semantic tier reports a partial pass with, and the
    /// backwards compatibility that lets every plugin written before it keep
    /// answering unchanged - see [`FileChangeResponse::incomplete`].
    #[test]
    fn a_response_without_the_incomplete_field_reads_as_a_complete_pass() {
        let without = r#"{"jsonrpc":"2.0","id":7,"result":{}}"#;
        let parsed: FileChangeResponse = serde_json::from_str(without).unwrap();
        assert!(!parsed.incomplete);

        let with = r#"{"jsonrpc":"2.0","id":7,"result":{},"incomplete":true}"#;
        let parsed: FileChangeResponse = serde_json::from_str(with).unwrap();
        assert!(parsed.incomplete);
        assert!(serde_json::to_string(&parsed).unwrap().contains("\"incomplete\":true"));
        assert_eq!(parsed.incomplete_reason, None);

        let reasoned =
            r#"{"jsonrpc":"2.0","id":7,"result":{},"incomplete":true,"incompleteReason":"server exited"}"#;
        let parsed: FileChangeResponse = serde_json::from_str(reasoned).unwrap();
        assert_eq!(parsed.incomplete_reason.as_deref(), Some("server exited"));
        assert!(serde_json::to_string(&parsed).unwrap().contains("\"incompleteReason\":\"server exited\""));
    }

    /// `unfinishedFiles` is optional both ways. Absent reads as
    /// `None` (a plugin written before the field), `None` is not written, and
    /// an empty and a non-empty list each survive a round trip as written -
    /// an empty list ("every file finished") must not collapse into `None`
    /// ("the plugin did not say").
    #[test]
    fn unfinished_files_is_omitted_when_absent_and_round_trips_when_present() {
        let without = r#"{"jsonrpc":"2.0","id":7,"result":{}}"#;
        let parsed: FileChangeResponse = serde_json::from_str(without).unwrap();
        assert_eq!(parsed.unfinished_files, None);
        let json = serde_json::to_string(&parsed).unwrap();
        assert!(!json.contains("unfinishedFiles"), "None is not written: {json}");

        for files in [Vec::new(), vec!["a.rs".to_string()]] {
            let response = FileChangeResponse { unfinished_files: Some(files.clone()), ..parsed.clone() };
            let json = serde_json::to_string(&response).unwrap();
            let value: serde_json::Value = serde_json::from_str(&json).unwrap();
            assert_eq!(value["unfinishedFiles"], serde_json::json!(files), "{json}");
            let round_tripped: FileChangeResponse = serde_json::from_str(&json).unwrap();
            assert_eq!(round_tripped.unfinished_files, Some(files));
        }
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

    const OLD_SHAPE_NODE: &str = r#"{"id":"n1","kind":"Function","name":"read","qualifiedName":"m::S::read","filePath":"src/m.rs","range":{"start":{"line":1,"col":0},"end":{"line":3,"col":1}},"visibility":"public","language":"rust","hasSyntaxErrors":false}"#;

    fn path(first: &str, rest: &[(&str, &str)]) -> QualifiedPath {
        rest.iter().fold(QualifiedPath::root(first), |path, (sep, name)| path.child(*sep, *name))
    }

    /// A node without path keys still parses, has no paths, and serializes
    /// back to the very same bytes. Control: drop `skip_serializing_if` on
    /// `alias_paths` (an `"aliasPaths":[]` key appears).
    #[test]
    fn an_old_shape_node_parses_and_round_trips_byte_identically() {
        let node: WireNode = serde_json::from_str(OLD_SHAPE_NODE).unwrap();
        assert_eq!((node.qualified_path.as_ref(), node.alias_paths.len()), (None, 0));
        assert_eq!(serde_json::to_string(&node).unwrap(), OLD_SHAPE_NODE);
        assert_eq!(node.check_qualified_path(), Ok(()));
    }

    /// The wire spelling: segments are `{sep, name}` objects with `sep`
    /// omitted on the first; `keyPath` sits in the target.
    #[test]
    fn paths_use_the_documented_wire_shape() {
        let line = OLD_SHAPE_NODE.replace(
            r#""hasSyntaxErrors":false}"#,
            r#""hasSyntaxErrors":false,"qualifiedPath":[{"name":"m"},{"sep":"::","name":"S"},{"sep":"::","name":"read"}],"aliasPaths":[[{"name":"T"},{"sep":"::","name":"read"}]]}"#,
        );
        let node: WireNode = serde_json::from_str(&line).unwrap();
        assert_eq!(node.qualified_path, Some(path("m", &[("::", "S"), ("::", "read")])));
        assert_eq!(node.alias_paths, vec![path("T", &[("::", "read")])]);
        assert_eq!(serde_json::to_string(&node).unwrap(), line);

        let target: PlaceholderTarget = serde_json::from_str(
            r#"{"scope":{"file":"b.rs"},"key":{"qualifiedName":"a::T.f"},"keyPath":[{"name":"a"},{"sep":"::","name":"T"},{"sep":".","name":"f"}]}"#,
        )
        .unwrap();
        assert_eq!(target.key_path, Some(path("a", &[("::", "T"), (".", "f")])));
        assert_eq!(target.check_key_path(), Ok(()));
    }

    #[test]
    fn a_path_joins_its_own_separators_and_reports_each_broken_rule() {
        let p = path("a", &[("::", "T"), (".", "f")]);
        assert_eq!(p.display(), "a::T.f");
        assert_eq!(p.suffix_from(1), "T.f");
        assert_eq!(p.head(), Some(path("a", &[("::", "T")])));
        assert_eq!(p.last().map(|s| s.name.as_str()), Some("f"));
        assert_eq!(p.check_joins_to("a::T.f"), Ok(()));
        assert!(matches!(p.check_joins_to("a::T::f"), Err(PathError::DoesNotJoin { .. })));

        let segment = |sep: Option<&str>, name: &str| PathSegment {
            sep: sep.map(str::to_string),
            name: name.to_string(),
        };
        assert_eq!(QualifiedPath(vec![]).check(), Err(PathError::Empty));
        assert_eq!(
            QualifiedPath(vec![segment(Some("::"), "a")]).check(),
            Err(PathError::SeparatorOnFirstSegment)
        );
        assert_eq!(
            QualifiedPath(vec![segment(None, "a"), segment(None, "b")]).check(),
            Err(PathError::MissingSeparator(1))
        );
        assert_eq!(
            QualifiedPath(vec![segment(None, "a"), segment(Some(""), "b")]).check(),
            Err(PathError::MissingSeparator(1))
        );
        assert_eq!(QualifiedPath(vec![segment(None, "")]).check(), Err(PathError::EmptyName(0)));
        assert_eq!(path("a", &[("::", "b\u{1f}")]).check(), Err(PathError::ForbiddenChar(1)));
    }

    #[test]
    fn a_nodes_path_and_aliases_end_in_its_name() {
        let mut node: WireNode = serde_json::from_str(OLD_SHAPE_NODE).unwrap();
        node.alias_paths = vec![path("T", &[("::", "read")])];
        assert_eq!(node.check_alias_path(&node.alias_paths[0]), Err(PathError::AliasWithoutPath));

        node.qualified_path = Some(path("m", &[("::", "S"), ("::", "read")]));
        assert_eq!(node.check_qualified_path(), Ok(()));
        assert_eq!(node.check_alias_path(&path("T", &[("::", "read")])), Ok(()));
        assert_eq!(node.check_alias_path(&QualifiedPath::root("read")), Err(PathError::AliasTooShort));
        assert_eq!(
            node.check_alias_path(&path("m", &[("::", "S"), ("::", "read")])),
            Err(PathError::AliasEqualsPath)
        );
        assert!(matches!(
            node.check_alias_path(&path("T", &[("::", "write")])),
            Err(PathError::LastNameIsNotName { .. })
        ));

        node.name = "other".to_string();
        assert!(matches!(node.check_qualified_path(), Err(PathError::LastNameIsNotName { .. })));
    }

    /// `untyped_calls` travels as `untypedCalls`, after every other
    /// key, and an empty list sends no key at all, so a node that reports
    /// none is byte-identical to the old shape. Controls: rename the field's
    /// serde key (the line no longer parses into the list); drop its
    /// `skip_serializing_if` (an `"untypedCalls":[]` key appears).
    #[test]
    fn untyped_calls_use_the_documented_key_and_are_omitted_when_empty() {
        let line = OLD_SHAPE_NODE.replace(
            r#""hasSyntaxErrors":false}"#,
            r#""hasSyntaxErrors":false,"untypedCalls":["frob","m"]}"#,
        );
        let node: WireNode = serde_json::from_str(&line).unwrap();
        assert_eq!(node.untyped_calls, vec!["frob".to_string(), "m".to_string()]);
        assert_eq!(serde_json::to_string(&node).unwrap(), line);

        let empty = WireNode { untyped_calls: Vec::new(), ..node };
        let json = serde_json::to_string(&empty).unwrap();
        assert!(!json.contains("untypedCalls"), "{json}");
        assert_eq!(json, OLD_SHAPE_NODE);
    }

    // ---------------------------------------------------------------
    // resolutionChanged, the facts trailer's types, reextract and
    // the edge specifier.
    // ---------------------------------------------------------------

    fn envelope_json(envelope: &ControlEnvelope) -> serde_json::Value {
        serde_json::to_value(envelope).unwrap()
    }

    /// `resolutionChanged` is a request named in camelCase, with
    /// `previousFacts` only when core holds some.
    #[test]
    fn a_resolution_changed_request_round_trips_and_omits_absent_facts() {
        let with_facts = ControlEnvelope {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id: Some(RequestId::Number(3)),
            message: ControlMessage::ResolutionChanged {
                file_path: "package.json".to_string(),
                previous_facts: Some("{\"v\":1}".to_string()),
            },
        };
        let value = envelope_json(&with_facts);
        assert_eq!(value["method"], "resolutionChanged");
        assert_eq!(value["params"]["filePath"], "package.json");
        assert_eq!(value["params"]["previousFacts"], "{\"v\":1}");
        let back: ControlEnvelope = serde_json::from_value(value).unwrap();
        assert_eq!(back, with_facts);

        let without = ControlEnvelope {
            message: ControlMessage::ResolutionChanged {
                file_path: "package.json".to_string(),
                previous_facts: None,
            },
            ..with_facts
        };
        let json = serde_json::to_string(&without).unwrap();
        assert!(!json.contains("previousFacts"), "{json}");
        let back: ControlEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(back, without);
    }

    /// A `fileChanged` without `reextract` reads as
    /// `false`, and `false` is not written, so the frame is byte-identical
    /// to the old one; `true` is written as `"reextract":true`.
    #[test]
    fn file_changed_reextract_defaults_to_false_and_is_written_only_when_set() {
        let old = r#"{"jsonrpc":"2.0","id":1,"method":"fileChanged","params":{"filePath":"src/a.ts"}}"#;
        let envelope: ControlEnvelope = serde_json::from_str(old).unwrap();
        assert_eq!(
            envelope.message,
            ControlMessage::FileChanged { file_path: "src/a.ts".to_string(), reextract: false }
        );
        assert_eq!(serde_json::to_string(&envelope).unwrap(), old);

        let reextract = ControlEnvelope {
            message: ControlMessage::FileChanged { file_path: "src/a.ts".to_string(), reextract: true },
            ..envelope
        };
        let value = envelope_json(&reextract);
        assert_eq!(value["params"]["reextract"], true);
        let back: ControlEnvelope = serde_json::from_value(value).unwrap();
        assert_eq!(back, reextract);
    }

    /// An edge without a `specifier` reads as `None`;
    /// `None` is not written; a set one round trips.
    #[test]
    fn an_edge_specifier_is_optional_and_round_trips() {
        let old = r#"{"id":"e","fromId":"a","toId":"b","kind":"IMPORTS","source":"syntactic","engine":"tree-sitter","resolved":false}"#;
        let edge: WireEdge = serde_json::from_str(old).unwrap();
        assert_eq!(edge.specifier, None);
        let json = serde_json::to_string(&edge).unwrap();
        assert!(!json.contains("specifier"), "{json}");

        let with = WireEdge { specifier: Some("@app/math".to_string()), ..edge };
        let value = serde_json::to_value(&with).unwrap();
        assert_eq!(value["specifier"], "@app/math");
        let back: WireEdge = serde_json::from_value(value).unwrap();
        assert_eq!(back, with);
    }

    /// Every delta shape the protocol documents, read from its JSON and
    /// written back to the same JSON.
    #[test]
    fn every_resolution_delta_shape_round_trips_through_its_documented_json() {
        let cases: Vec<(&str, ResolutionChangedResult)> = vec![
            (
                r#"{"delta":{"kind":"unchanged"},"facts":"f2"}"#,
                ResolutionChangedResult { delta: ResolutionDelta::Unchanged, facts: Some("f2".to_string()) },
            ),
            (
                r#"{"delta":{"kind":"unknown","reason":"no model"}}"#,
                ResolutionChangedResult {
                    delta: ResolutionDelta::Unknown { reason: "no model".to_string() },
                    facts: None,
                },
            ),
            (
                r#"{"delta":{"kind":"affected","files":[{"under":"a","notUnder":["a/b"]}],"imports":[{"importers":{"under":""},"by":{"specifier":{"exact":"x"}}},{"importers":{"under":"pkg"},"by":{"target":{"scopeKind":"container","matcher":{"under":{"prefix":"app","separator":"."}}}}},{"importers":{"under":""},"by":{"target":{"scopeKind":"file","matcher":{"startsWith":"lib/"}}}},{"importers":{"under":""},"by":{"specifier":"nonRelative"}}]},"facts":"f3"}"#,
                ResolutionChangedResult {
                    delta: ResolutionDelta::Affected {
                        files: vec![PathScope { under: "a".to_string(), not_under: vec!["a/b".to_string()] }],
                        imports: vec![
                            ImportSelector {
                                importers: PathScope { under: String::new(), not_under: vec![] },
                                by: ImportMatch::Specifier(Matcher::Exact("x".to_string())),
                            },
                            ImportSelector {
                                importers: PathScope { under: "pkg".to_string(), not_under: vec![] },
                                by: ImportMatch::Target {
                                    scope_kind: TargetScopeKind::Container,
                                    matcher: Matcher::Under {
                                        prefix: "app".to_string(),
                                        separator: ".".to_string(),
                                    },
                                },
                            },
                            ImportSelector {
                                importers: PathScope { under: String::new(), not_under: vec![] },
                                by: ImportMatch::Target {
                                    scope_kind: TargetScopeKind::File,
                                    matcher: Matcher::StartsWith("lib/".to_string()),
                                },
                            },
                            ImportSelector {
                                importers: PathScope { under: String::new(), not_under: vec![] },
                                by: ImportMatch::Specifier(Matcher::NonRelative),
                            },
                        ],
                    },
                    facts: Some("f3".to_string()),
                },
            ),
        ];
        for (json, expected) in cases {
            let read: ResolutionChangedResult = serde_json::from_str(json).unwrap();
            assert_eq!(read, expected, "{json}");
            assert_eq!(serde_json::to_string(&read).unwrap(), json);
        }

        // `reason`, `files` and `imports` may be left out.
        let terse: ResolutionDelta = serde_json::from_str(r#"{"kind":"unknown"}"#).unwrap();
        assert_eq!(terse, ResolutionDelta::Unknown { reason: String::new() });
        let terse: ResolutionDelta = serde_json::from_str(r#"{"kind":"affected"}"#).unwrap();
        assert_eq!(terse, ResolutionDelta::Affected { files: vec![], imports: vec![] });
    }

    /// The full response frame reads with its id.
    #[test]
    fn a_resolution_changed_response_reads_with_its_id() {
        let json = r#"{"jsonrpc":"2.0","id":9,"result":{"delta":{"kind":"unchanged"}}}"#;
        let response: ResolutionChangedResponse = serde_json::from_str(json).unwrap();
        assert_eq!(response.id, RequestId::Number(9));
        assert_eq!(
            response.result,
            ResolutionChangedResult { delta: ResolutionDelta::Unchanged, facts: None }
        );
    }

    /// `Under` matches the prefix itself or the prefix followed by the
    /// separator, never a longer name sharing the prefix.
    ///
    /// Control: drop the separator check from `Matcher::Under`
    /// (`pkg.subtle` matches).
    #[test]
    fn under_matches_on_the_separator_boundary_only() {
        let under = Matcher::Under { prefix: "pkg.sub".to_string(), separator: ".".to_string() };
        assert!(under.matches("pkg.sub"));
        assert!(under.matches("pkg.sub.leaf"));
        assert!(!under.matches("pkg.subtle"));
        assert!(!under.matches("pkg"));
        assert!(!under.matches("other.pkg.sub"));

        let rust = Matcher::Under { prefix: "crate::a".to_string(), separator: "::".to_string() };
        assert!(rust.matches("crate::a::b"));
        assert!(!rust.matches("crate::ab"));
        assert!(!rust.matches("crate::a:b"));
    }

    #[test]
    fn exact_starts_with_and_non_relative_match_as_documented() {
        assert!(Matcher::Exact("x".to_string()).matches("x"));
        assert!(!Matcher::Exact("x".to_string()).matches("xy"));

        let starts = Matcher::StartsWith("@app/".to_string());
        assert!(starts.matches("@app/math"));
        assert!(!starts.matches("@apple/math"));

        for specifier in ["react", "@app/math", "lodash/fp"] {
            assert!(Matcher::NonRelative.matches(specifier), "{specifier} is non-relative");
        }
        for specifier in ["./a", "../b", "/abs", "#internal", ""] {
            assert!(!Matcher::NonRelative.matches(specifier), "{specifier:?} is not non-relative");
        }
    }

    /// A scope is a directory on a `/` boundary minus its `notUnder`
    /// directories; `""` is the whole project.
    ///
    /// Control: ignore `not_under` in `PathScope::contains` (`a/b/x.ts`
    /// is in).
    #[test]
    fn a_path_scope_is_its_directory_minus_the_excluded_ones() {
        let scope = PathScope { under: "a".to_string(), not_under: vec!["a/b".to_string()] };
        assert!(scope.contains("a/x.ts"));
        assert!(scope.contains("a/c/x.ts"));
        assert!(!scope.contains("a/b/x.ts"));
        assert!(!scope.contains("a/b"));
        assert!(!scope.contains("ab/x.ts"), "a directory matches on a / boundary");
        assert!(scope.contains("a/bc/x.ts"), "a/b excludes a/b/, not a/bc/");

        let whole = PathScope { under: String::new(), not_under: vec![] };
        assert!(whole.contains("x.ts"));
        assert!(whole.contains("deep/down/x.ts"));

        let trailing = PathScope { under: "a/".to_string(), not_under: vec![] };
        assert!(trailing.contains("a/x.ts"));
        assert!(!trailing.contains("ab/x.ts"));
    }
}
