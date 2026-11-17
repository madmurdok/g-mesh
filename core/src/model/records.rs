use crate::protocol::types::QualifiedPath;

/// One declaration of a symbol that has several - a row of the `declarations`
/// table (see `storage::schema`). Mirrors the wire
/// `WireDeclaration` (wire/src/lib.rs) field for field, which is also the wire
/// shape (`protocol::types::WireDeclaration`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclarationRecord {
    /// Position in the owning node's declaration list, source-ordered from 0.
    /// What an edge's `to_declaration` names.
    pub ordinal: i64,
    pub start_line: i64,
    pub start_col: i64,
    pub end_line: i64,
    pub end_col: i64,
    pub signature: Option<String>,
    /// Whether this declaration carries an implementation - the difference
    /// between an overload signature and the implementation TypeScript never
    /// shows a caller.
    pub has_body: bool,
}

/// What a placeholder [`NodeRecord`] is waiting on - the storage-layer mirror
/// of `protocol::types::PlaceholderTarget`, and the write-side counterpart of
/// the `placeholder_targets` table (`storage::schema`'s DDL comment on that
/// table has the full field-by-field rationale). Flat strings rather than the
/// wire's nested `TargetScope`/`TargetKey` enums, matching this module's own
/// convention for every other wire-shaped record here (`DeclarationRecord`,
/// `EdgeRecord.source`): the enum-to-string mapping happens once, at the
/// wire boundary (`watcher::apply::to_node_record`), so nothing downstream of
/// it has to match on a protocol type it does not otherwise depend on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaceholderTargetRecord {
    /// `"file"` | `"container"` - see `placeholder_targets.scopeKind`'s CHECK.
    pub scope_kind: String,
    /// A file path or a container key, per `scope_kind`.
    pub scope: String,
    /// `"name"` | `"qualifiedName"` - see `placeholder_targets.keyKind`'s CHECK.
    pub key_kind: String,
    pub key: String,
    pub from_container: Option<String>,
    /// The segments of a `qualifiedName` key; `None` for a `name` key or a
    /// plugin that sends no paths.
    pub key_path: Option<QualifiedPath>,
    /// Further file paths the placeholder waits at besides `scope`, stored
    /// as `placeholder_also_scopes` rows; empty for most placeholders.
    pub also_scopes: Vec<String>,
}

pub struct NodeRecord {
    pub id: String,
    pub kind: String,
    pub name: String,
    pub qualified_name: String,
    pub file_path: String,
    pub start_line: i64,
    pub start_col: i64,
    pub end_line: i64,
    pub end_col: i64,
    pub signature: Option<String>,
    /// Whether this declaration is visible from anywhere, true only when
    /// `visibility == "public"`. This is a **read-side mirror** of the
    /// database's own `GENERATED ALWAYS` `exported` column
    /// (`storage::schema`'s DDL): `storage::write::apply_diff` never writes it, and cannot,
    /// since SQLite refuses an `INSERT`/`UPDATE` that names a generated
    /// column at all. It stays a field here, rather than becoming a method
    /// computed from `visibility`, so every existing reader keeps compiling
    /// unchanged. (`link::symbol_links::link_diff` used to be the one that
    /// mattered, filtering `diff.upsert_nodes` by `.exported` before anything
    /// was read back; since GM-266 it reads `visibility` itself, because a
    /// `container`-visible node can answer a placeholder too.) Every
    /// *constructor* of a `NodeRecord`
    /// ([`NodeRecord::new`], `watcher::apply::to_node_record`, and this
    /// module's/`link::symbol_links`'s own test helpers) is required to set
    /// this from `visibility` at construction time; that
    /// single-writer-per-construction-path rule is what keeps the two from
    /// disagreeing, since the database itself no longer can.
    pub exported: bool,
    /// `"public"` | `"file"` | `"container"` - the storage mirror of
    /// `protocol::types::Visibility`, minus the payload (see
    /// `visibility_container` below). Defaults to `"file"` in
    /// [`NodeRecord::new`], matching the column's own `DEFAULT 'file'` and
    /// the exact meaning the old `exported: false` default had.
    pub visibility: String,
    /// The container key from `Visibility::Container(key)`. `None` unless
    /// `visibility == "container"`.
    pub visibility_container: Option<String>,
    /// Logical container this declaration is a member of (Data Model >
    /// Logical containers). `None` for a language with no containers.
    pub container: Option<String>,
    /// `container`'s own parent key, as the plugin sent it with this member.
    /// Not a `nodes` column: it describes the container, so it lands on that
    /// container's `containers.parentKey` (`graph::containers`, which also
    /// documents what happens when two members disagree). **Write-side
    /// only**, like `target` and `declarations` below - a record read back
    /// through `graph::queries::map_node_row` carries `None` here, and handing
    /// it straight back to `storage::write::apply_diff` would record its container as a
    /// root, since `None` is a value ("no parent"), not "unknown".
    pub container_parent: Option<String>,
    /// What this node is waiting to be linked onto, for a placeholder node -
    /// `None` for an ordinary declaration. Write-side only, the same "absent
    /// says nothing about what is stored" convention `declarations` documents
    /// just below: a read via `graph::queries::map_node_row` always leaves
    /// this `None` rather than joining `placeholder_targets`, so a record
    /// that came *out* of the database must not be handed straight back to
    /// `storage::write::apply_diff` expecting an existing target row to be preserved - see
    /// `storage::write::apply_diff`'s own comment on why a `None` here **deletes** any
    /// existing row instead of leaving it alone.
    pub target: Option<PlaceholderTargetRecord>,
    pub doc_comment: Option<String>,
    pub language: String,
    pub native_kind: Option<String>,
    pub has_syntax_errors: bool,
    /// The node's file carries a generated-code header (wire `generated`,
    /// GM-646): it keeps every structural answer and gets no vector
    /// (`embedding::text::node_text`). Read and written (`nodes.generated`).
    pub generated: bool,
    /// Every declaration this symbol is written as, in source order - empty
    /// for the single-declaration symbols that are nearly all of them, since
    /// the flat fields above already describe those completely.
    ///
    /// **Write-side only.** The read path (`graph::queries::map_node_row`)
    /// leaves this empty rather than joining the child table on every lookup,
    /// so a `NodeRecord` that came *out* of the database says nothing about
    /// declarations - and must not be handed straight back to `storage::write::apply_diff`,
    /// which would read that silence as "this symbol has one declaration now"
    /// and drop the rows. Nothing does that today; a future reader that needs
    /// the list should load it explicitly.
    pub declarations: Vec<DeclarationRecord>,
    /// `qualified_name` as the plugin's segments, already checked at ingest
    /// to join back to it. Read and written (`nodes.qualifiedPath`).
    pub qualified_path: Option<QualifiedPath>,
    /// Other spellings of this declaration, already checked at ingest.
    /// **Write-side only**, like `declarations`: they exist in storage only
    /// as `qualified_suffixes` rows, a read leaves this empty, and
    /// `storage::write::apply_diff` replaces the node's rows from it on every upsert.
    pub alias_paths: Vec<QualifiedPath>,
    /// Bare names of methods this node calls through a receiver of unknown
    /// type (wire `untypedCalls`, GM-486). **Write-side only**, like
    /// `alias_paths`: they exist in storage only as `untyped_calls` rows, a
    /// read leaves this empty, and `storage::write::apply_diff` replaces the node's rows
    /// from it on every upsert.
    pub untyped_calls: Vec<String>,
}

impl NodeRecord {
    /// Minimal constructor for the common case; zero/None-fill the rest.
    /// `visibility`/`exported` default to `"file"`/`false` - private, not
    /// exported - matching the column's own `DEFAULT 'file'` and the meaning
    /// the pre-GM-264 `exported: false` default had.
    pub fn new(
        id: impl Into<String>,
        kind: impl Into<String>,
        name: impl Into<String>,
        qualified_name: impl Into<String>,
        file_path: impl Into<String>,
        language: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            kind: kind.into(),
            name: name.into(),
            qualified_name: qualified_name.into(),
            file_path: file_path.into(),
            start_line: 0,
            start_col: 0,
            end_line: 0,
            end_col: 0,
            signature: None,
            exported: false,
            visibility: "file".to_string(),
            visibility_container: None,
            container: None,
            container_parent: None,
            target: None,
            doc_comment: None,
            language: language.into(),
            native_kind: None,
            has_syntax_errors: false,
            generated: false,
            declarations: Vec::new(),
            qualified_path: None,
            alias_paths: Vec::new(),
            untyped_calls: Vec::new(),
        }
    }
}

pub struct EdgeRecord {
    pub id: String,
    pub from_id: String,
    pub to_id: String,
    pub kind: String,
    /// `"syntactic"` | `"semantic"` - the closed tier `edges.source`'s CHECK
    /// enforces (`storage::schema`'s DDL comment on `edges`). See
    /// [`EdgeRecord::new`] for how this stays populated with a valid tier
    /// even for the ~100 call sites across this codebase's test suites that
    /// still pass a v1 legacy string here.
    pub source: String,
    /// Free-text engine label (`"tree-sitter"`, `"ts-compiler"`,
    /// `"go-types"`, `"rust-analyzer"`, ...) - diagnostic only, never matched
    /// against in a `WHERE` clause the way `source` is. See
    /// `storage::schema`'s DDL comment on `edges.engine`.
    pub engine: String,
    pub resolved: bool,
    /// Which of the target's declarations this edge binds, as an ordinal into
    /// its declaration list. `None` for everything that binds no particular
    /// one - every structural-pass edge, and every edge whose target has a
    /// single declaration. See `edges.toDeclaration` in `storage::schema`.
    pub to_declaration: Option<i64>,
    /// An `IMPORTS` edge's raw import text, `None` on every other edge. Only
    /// the write side carries it: readers leave it `None`.
    pub specifier: Option<String>,
}

impl EdgeRecord {
    /// `source` accepts either shape found across this codebase's callers:
    /// a v1 legacy engine string (`"tree-sitter"` | `"ts-compiler"` - what
    /// every test helper written before GM-264 still passes here) or a v2
    /// tier (`"syntactic"` | `"semantic"`). [`normalize_legacy_source`] below
    /// derives the right `(source, engine)` pair either way, so this
    /// constructor's signature - and therefore every one of its ~100 call
    /// sites - did not have to change for GM-264's real `source`/`engine`
    /// split. A caller that already has a distinct engine value (the wire
    /// boundary, `watcher::apply::to_edge_record`) overwrites `.engine` on
    /// the result afterwards, the same way `.to_declaration` is already set
    /// post-construction below.
    pub fn new(
        id: impl Into<String>,
        from_id: impl Into<String>,
        to_id: impl Into<String>,
        kind: impl Into<String>,
        source: impl Into<String>,
        resolved: bool,
    ) -> Self {
        let (source, engine) = normalize_legacy_source(&source.into());
        Self {
            id: id.into(),
            from_id: from_id.into(),
            to_id: to_id.into(),
            kind: kind.into(),
            source,
            engine,
            resolved,
            to_declaration: None,
            specifier: None,
        }
    }
}

/// Maps a v1 legacy engine string to its `(tier, engine)` pair
/// (`"tree-sitter"` -> `("syntactic", "tree-sitter")`, `"ts-compiler"` ->
/// `("semantic", "ts-compiler")` - the same migration
/// `storage::schema`'s DDL comment on `edges` documents), and passes any
/// other string through unchanged as both tier and engine - which is exactly
/// right for a v2 tier string (`"syntactic"`/`"semantic"` map to themselves,
/// with a caller expected to set a real `.engine` afterwards if it has one -
/// see [`EdgeRecord::new`]'s own doc comment) and merely inert for anything
/// else, rather than a hard error: this is a storage-layer convenience, not
/// the wire's own validation (`protocol::types::normalize_source` is that,
/// and already rejects an unrecognized wire value before a `WireEdge` ever
/// reaches here).
fn normalize_legacy_source(source: &str) -> (String, String) {
    match source {
        "tree-sitter" => ("syntactic".to_string(), "tree-sitter".to_string()),
        "ts-compiler" => ("semantic".to_string(), "ts-compiler".to_string()),
        other => (other.to_string(), other.to_string()),
    }
}

/// A set of node/edge changes to apply atomically - the single write path
/// used by both initial bulk-index ingestion and incremental per-file/burst
/// updates.
#[derive(Default)]
pub struct Diff {
    pub upsert_nodes: Vec<NodeRecord>,
    pub delete_node_ids: Vec<String>,
    pub upsert_edges: Vec<EdgeRecord>,
    pub delete_edge_ids: Vec<String>,
}

impl Diff {
    pub fn is_empty(&self) -> bool {
        self.upsert_nodes.is_empty()
            && self.delete_node_ids.is_empty()
            && self.upsert_edges.is_empty()
            && self.delete_edge_ids.is_empty()
    }
}
