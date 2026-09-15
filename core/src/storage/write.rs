use anyhow::{Context, Result};
use rusqlite::{params, Connection};

/// One declaration of a symbol that has several - a row of the `declarations`
/// table (see `storage::schema`). Mirrors the plugin's `SymbolDeclaration`
/// (plugins/typescript/src/extract.ts) field for field, which is also the wire
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
    /// (`storage::schema`'s DDL): [`apply_diff`] never writes it, and cannot,
    /// since SQLite refuses an `INSERT`/`UPDATE` that names a generated
    /// column at all. It stays a field here, rather than becoming a method
    /// computed from `visibility`, so every existing reader keeps compiling
    /// unchanged, most importantly `graph::symbol_links::link_diff`, which
    /// filters `diff.upsert_nodes` by `.exported` *before* anything is read
    /// back from storage, so there is no database row yet to derive it from
    /// at that point. Every *constructor* of a `NodeRecord`
    /// ([`NodeRecord::new`], `watcher::apply::to_node_record`, and this
    /// module's/`graph::symbol_links`'s own test helpers) is required to set
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
    /// What this node is waiting to be linked onto, for a placeholder node -
    /// `None` for an ordinary declaration. Write-side only, the same "absent
    /// says nothing about what is stored" convention `declarations` documents
    /// just below: a read via `graph::queries::map_node_row` always leaves
    /// this `None` rather than joining `placeholder_targets`, so a record
    /// that came *out* of the database must not be handed straight back to
    /// [`apply_diff`] expecting an existing target row to be preserved - see
    /// [`apply_diff`]'s own comment on why a `None` here **deletes** any
    /// existing row instead of leaving it alone.
    pub target: Option<PlaceholderTargetRecord>,
    pub doc_comment: Option<String>,
    pub language: String,
    pub native_kind: Option<String>,
    pub has_syntax_errors: bool,
    /// Every declaration this symbol is written as, in source order - empty
    /// for the single-declaration symbols that are nearly all of them, since
    /// the flat fields above already describe those completely.
    ///
    /// **Write-side only.** The read path (`graph::queries::map_node_row`)
    /// leaves this empty rather than joining the child table on every lookup,
    /// so a `NodeRecord` that came *out* of the database says nothing about
    /// declarations - and must not be handed straight back to [`apply_diff`],
    /// which would read that silence as "this symbol has one declaration now"
    /// and drop the rows. Nothing does that today; a future reader that needs
    /// the list should load it explicitly.
    pub declarations: Vec<DeclarationRecord>,
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
            target: None,
            doc_comment: None,
            language: language.into(),
            native_kind: None,
            has_syntax_errors: false,
            declarations: Vec::new(),
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

/// Applies `diff` in a single SQLite transaction: edge deletes, node
/// deletes, node upserts, edge upserts, in that order so edge FKs are
/// always valid mid-transaction. Any failure rolls back the whole diff -
/// nothing partial is ever committed.
pub fn apply_diff(conn: &mut Connection, diff: &Diff) -> Result<()> {
    if diff.is_empty() {
        return Ok(());
    }

    let tx = conn.transaction().context("failed to start transaction")?;

    for id in &diff.delete_edge_ids {
        tx.execute("DELETE FROM edges WHERE id = ?1", params![id]).context("failed to delete edge")?;
    }
    for id in &diff.delete_node_ids {
        // Explicitly, rather than leaning on the child tables' ON DELETE
        // CASCADE: `foreign_keys` is off on the connection the daemon actually
        // runs on (`storage::connection::open` sets WAL and nothing else), so
        // the cascade only fires in tests that switch it on. Left orphaned,
        // these rows would be handed to whoever next takes this node's id -
        // `placeholder_targets` joins `declarations` here for exactly that
        // reason: a placeholder node deleted (its import/usage removed, or
        // the file it lived in reparsed without it) must not leave a stale
        // target row for some *other* node to inherit if it is ever given
        // this same content-derived id.
        tx.execute("DELETE FROM declarations WHERE nodeId = ?1", params![id])
            .context("failed to delete a node's declarations")?;
        tx.execute("DELETE FROM placeholder_targets WHERE nodeId = ?1", params![id])
            .context("failed to delete a node's placeholder target")?;
        tx.execute("DELETE FROM nodes WHERE id = ?1", params![id]).context("failed to delete node")?;
    }
    for node in &diff.upsert_nodes {
        tx.execute(
            "INSERT INTO nodes (id, kind, name, qualifiedName, filePath, startLine, startCol, endLine, endCol, signature, visibility, visibilityContainer, docComment, language, nativeKind, hasSyntaxErrors, container)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)
             ON CONFLICT(id) DO UPDATE SET
                kind = excluded.kind,
                name = excluded.name,
                qualifiedName = excluded.qualifiedName,
                filePath = excluded.filePath,
                startLine = excluded.startLine,
                startCol = excluded.startCol,
                endLine = excluded.endLine,
                endCol = excluded.endCol,
                signature = excluded.signature,
                visibility = excluded.visibility,
                visibilityContainer = excluded.visibilityContainer,
                docComment = excluded.docComment,
                language = excluded.language,
                nativeKind = excluded.nativeKind,
                hasSyntaxErrors = excluded.hasSyntaxErrors,
                container = excluded.container",
            // `exported` is deliberately absent from both the column list and
            // the `SET` clause: it is a `GENERATED ALWAYS` column
            // (`storage::schema`'s DDL) and SQLite refuses to `INSERT`/
            // `UPDATE` one directly - the database derives it from
            // `visibility` on every write instead, which is what makes it
            // impossible for the two to disagree. See `NodeRecord.exported`'s
            // own doc comment for the write-side field this replaces.
            params![
                node.id,
                node.kind,
                node.name,
                node.qualified_name,
                node.file_path,
                node.start_line,
                node.start_col,
                node.end_line,
                node.end_col,
                node.signature,
                node.visibility,
                node.visibility_container,
                node.doc_comment,
                node.language,
                node.native_kind,
                node.has_syntax_errors,
                node.container,
            ],
        )
        .context("failed to upsert node")?;

        // A declaration list is replaced wholesale, never merged into: it
        // describes how the symbol is written *now*, so an overload deleted
        // between two reparses has to leave with the edit that deleted it.
        // Issued for every node, including the overwhelming majority that
        // carry no list at all - the alternative is not knowing whether this
        // node used to have one, and a probe of a (nodeId, ordinal) primary
        // key that matches nothing is the cheapest possible way to find out.
        // `prepare_cached` keeps that to one prepared statement per
        // transaction rather than one per node.
        tx.prepare_cached("DELETE FROM declarations WHERE nodeId = ?1")
            .context("failed to prepare the declaration replacement")?
            .execute(params![node.id])
            .context("failed to clear a node's declarations")?;
        for declaration in &node.declarations {
            tx.prepare_cached(
                "INSERT INTO declarations
                    (nodeId, ordinal, startLine, startCol, endLine, endCol, signature, hasBody)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            )
            .context("failed to prepare the declaration insert")?
            .execute(params![
                node.id,
                declaration.ordinal,
                declaration.start_line,
                declaration.start_col,
                declaration.end_line,
                declaration.end_col,
                declaration.signature,
                declaration.has_body,
            ])
            .context("failed to insert a declaration")?;
        }

        // `placeholder_targets` is replaced wholesale too, and for the same
        // "describes how this node is written *now*" reason: a re-upserted
        // placeholder whose target changed (a reparse that resolves the
        // import to a different file, say) must not leave its old row
        // sitting alongside the new one - `nodeId` is this table's own
        // primary key, so the delete-then-maybe-insert pair below is what
        // "replace" means for a 1:0-or-1 child, the same shape `vectors`
        // (`storage::schema`) already uses for its own re-embed. Cleared
        // unconditionally, then re-inserted only when `node.target` is
        // `Some`, so a node that *used* to be a placeholder and no longer is
        // (should never happen in practice - `nativeKind` does not change out
        // from under one id - but nothing here assumes it cannot) leaves no
        // orphaned target behind either.
        tx.prepare_cached("DELETE FROM placeholder_targets WHERE nodeId = ?1")
            .context("failed to prepare the placeholder target replacement")?
            .execute(params![node.id])
            .context("failed to clear a node's placeholder target")?;
        if let Some(target) = &node.target {
            tx.prepare_cached(
                "INSERT INTO placeholder_targets
                    (nodeId, scopeKind, scope, keyKind, key, fromContainer, fromFile)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )
            .context("failed to prepare the placeholder target insert")?
            .execute(params![
                node.id,
                target.scope_kind,
                target.scope,
                target.key_kind,
                target.key,
                target.from_container,
                // Not a field of `PlaceholderTargetRecord`: the requester's
                // own file is already `node.file_path` by the existing
                // "a placeholder's filePath is the importing file"
                // convention (`graph::symbol_links`' module doc) - see
                // `placeholder_targets.fromFile`'s own DDL comment.
                node.file_path,
            ])
            .context("failed to insert a placeholder target")?;
        }
    }
    for edge in &diff.upsert_edges {
        tx.execute(
            "INSERT INTO edges (id, fromId, toId, kind, source, engine, resolved, toDeclaration)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(id) DO UPDATE SET
                fromId = excluded.fromId,
                toId = excluded.toId,
                kind = excluded.kind,
                source = excluded.source,
                engine = excluded.engine,
                resolved = excluded.resolved,
                toDeclaration = excluded.toDeclaration",
            params![
                edge.id,
                edge.from_id,
                edge.to_id,
                edge.kind,
                edge.source,
                edge.engine,
                edge.resolved,
                edge.to_declaration
            ],
        )
        .context("failed to upsert edge")?;
    }

    tx.commit().context("failed to commit diff transaction")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::schema;

    fn setup() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        schema::apply(&conn).unwrap();
        conn
    }

    fn count(conn: &Connection, table: &str) -> i64 {
        conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| row.get(0)).unwrap()
    }

    #[test]
    fn empty_diff_is_a_safe_no_op() {
        let mut conn = setup();
        apply_diff(&mut conn, &Diff::default()).unwrap();
        assert_eq!(count(&conn, "nodes"), 0);
        assert_eq!(count(&conn, "edges"), 0);
    }

    #[test]
    fn mixed_upserts_and_deletes_commit_atomically() {
        let mut conn = setup();

        // Seed a node/edge pair that the diff below will delete.
        apply_diff(
            &mut conn,
            &Diff {
                upsert_nodes: vec![
                    NodeRecord::new("n1", "Function", "foo", "m::foo", "src/lib.rs", "rust"),
                    NodeRecord::new("n2", "Function", "bar", "m::bar", "src/lib.rs", "rust"),
                ],
                upsert_edges: vec![EdgeRecord::new("e1", "n1", "n2", "CALLS", "tree-sitter", false)],
                ..Default::default()
            },
        )
        .unwrap();

        apply_diff(
            &mut conn,
            &Diff {
                upsert_nodes: vec![NodeRecord::new("n3", "Function", "baz", "m::baz", "src/lib.rs", "rust")],
                delete_node_ids: vec!["n2".to_string()],
                upsert_edges: vec![EdgeRecord::new("e2", "n1", "n3", "CALLS", "tree-sitter", false)],
                delete_edge_ids: vec!["e1".to_string()],
            },
        )
        .unwrap();

        assert_eq!(count(&conn, "nodes"), 2); // n1, n3 (n2 deleted)
        assert_eq!(count(&conn, "edges"), 1); // e2 (e1 deleted)
    }

    fn declaration(ordinal: i64, signature: &str, has_body: bool) -> DeclarationRecord {
        DeclarationRecord {
            ordinal,
            start_line: ordinal,
            start_col: 7,
            end_line: ordinal,
            end_col: 47,
            signature: Some(signature.to_string()),
            has_body,
        }
    }

    fn overloaded(declarations: Vec<DeclarationRecord>) -> NodeRecord {
        let mut node = NodeRecord::new("n1", "Function", "parse", "parse", "src/parse.ts", "typescript");
        node.declarations = declarations;
        node
    }

    /// Every declaration row a node has, ordered, as (ordinal, signature,
    /// hasBody) - the fields the acceptance criteria names.
    fn declarations_of(conn: &Connection, node_id: &str) -> Vec<(i64, Option<String>, bool)> {
        let mut stmt = conn
            .prepare(
                "SELECT ordinal, signature, hasBody FROM declarations WHERE nodeId = ?1 ORDER BY ordinal",
            )
            .unwrap();
        stmt.query_map(params![node_id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    #[test]
    fn a_nodes_declarations_are_persisted_with_it() {
        let mut conn = setup();

        apply_diff(
            &mut conn,
            &Diff {
                upsert_nodes: vec![overloaded(vec![
                    declaration(0, "parse(input: string): string[]", false),
                    declaration(1, "parse(input: number): number", false),
                    declaration(2, "parse(input: string | number): any", true),
                ])],
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(
            declarations_of(&conn, "n1"),
            vec![
                (0, Some("parse(input: string): string[]".to_string()), false),
                (1, Some("parse(input: number): number".to_string()), false),
                (2, Some("parse(input: string | number): any".to_string()), true),
            ]
        );
    }

    /// The 99% case, and the reason the child table exists rather than more
    /// columns on `nodes`: an ordinary symbol costs no rows at all.
    #[test]
    fn a_single_declaration_node_writes_no_declaration_rows() {
        let mut conn = setup();

        apply_diff(
            &mut conn,
            &Diff {
                upsert_nodes: vec![NodeRecord::new(
                    "n1",
                    "Function",
                    "once",
                    "once",
                    "src/lib.ts",
                    "typescript",
                )],
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(count(&conn, "declarations"), 0);
    }

    #[test]
    fn re_upserting_a_node_replaces_its_declarations_instead_of_appending() {
        let mut conn = setup();
        apply_diff(
            &mut conn,
            &Diff {
                upsert_nodes: vec![overloaded(vec![
                    declaration(0, "parse(input: string): string[]", false),
                    declaration(1, "parse(input: number): number", false),
                    declaration(2, "parse(input: string | number): any", true),
                ])],
                ..Default::default()
            },
        )
        .unwrap();

        // The second overload is deleted from the source: the list is shorter
        // and renumbered, and the row describing the deleted one has to go
        // with it rather than linger at ordinal 2.
        apply_diff(
            &mut conn,
            &Diff {
                upsert_nodes: vec![overloaded(vec![
                    declaration(0, "parse(input: string): string[]", false),
                    declaration(1, "parse(input: string | number): any", true),
                ])],
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(
            declarations_of(&conn, "n1"),
            vec![
                (0, Some("parse(input: string): string[]".to_string()), false),
                (1, Some("parse(input: string | number): any".to_string()), true),
            ]
        );
    }

    /// The other half of "replace, don't accumulate": a symbol that stops
    /// being overloaded arrives with no list at all, and must not keep the one
    /// it had.
    #[test]
    fn a_node_that_lost_its_overloads_keeps_no_declaration_rows() {
        let mut conn = setup();
        apply_diff(
            &mut conn,
            &Diff {
                upsert_nodes: vec![overloaded(vec![
                    declaration(0, "parse(input: string): string[]", false),
                    declaration(1, "parse(input: string): string[] {}", true),
                ])],
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(count(&conn, "declarations"), 2);

        apply_diff(&mut conn, &Diff { upsert_nodes: vec![overloaded(Vec::new())], ..Default::default() })
            .unwrap();

        assert_eq!(count(&conn, "declarations"), 0);
    }

    /// `foreign_keys` is off on the connection the daemon actually runs on, so
    /// the child table's ON DELETE CASCADE never fires there - the delete has
    /// to be explicit, or a deleted node's declarations would be inherited by
    /// whatever next claims its id.
    #[test]
    fn deleting_a_node_takes_its_declarations_with_it_without_foreign_keys() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "OFF").unwrap();
        schema::apply(&conn).unwrap();

        apply_diff(
            &mut conn,
            &Diff {
                upsert_nodes: vec![overloaded(vec![
                    declaration(0, "parse(input: string): string[]", false),
                    declaration(1, "parse(input: string): string[] {}", true),
                ])],
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(count(&conn, "declarations"), 2);

        apply_diff(&mut conn, &Diff { delete_node_ids: vec!["n1".to_string()], ..Default::default() })
            .unwrap();

        assert_eq!(count(&conn, "nodes"), 0);
        assert_eq!(
            count(&conn, "declarations"),
            0,
            "orphaned rows would be handed to the next node with this id"
        );
    }

    fn file_scoped_target(scope: &str, key: &str) -> PlaceholderTargetRecord {
        PlaceholderTargetRecord {
            scope_kind: "file".to_string(),
            scope: scope.to_string(),
            key_kind: "name".to_string(),
            key: key.to_string(),
            from_container: None,
        }
    }

    fn placeholder(id: &str, importer_file: &str, target: PlaceholderTargetRecord) -> NodeRecord {
        let mut node =
            NodeRecord::new(id, "Module", "change", "target.ts#change", importer_file, "typescript");
        node.native_kind = Some("pending_symbol".to_string());
        node.target = Some(target);
        node
    }

    fn placeholder_target_row(conn: &Connection, node_id: &str) -> (String, String, String, String, String) {
        conn.query_row(
            "SELECT scopeKind, scope, keyKind, key, fromFile FROM placeholder_targets WHERE nodeId = ?1",
            params![node_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
        )
        .unwrap()
    }

    /// The acceptance criterion: `apply_diff` writes a `placeholder_targets`
    /// row for a node carrying `NodeRecord.target`, with `fromFile` filled in
    /// from the node's own `filePath` (never a field of the target record
    /// itself - see `PlaceholderTargetRecord`'s and this table's own DDL
    /// comment for why).
    #[test]
    fn apply_diff_writes_a_placeholder_target_row() {
        let mut conn = setup();
        apply_diff(
            &mut conn,
            &Diff {
                upsert_nodes: vec![placeholder("p1", "caller.ts", file_scoped_target("target.ts", "change"))],
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(count(&conn, "placeholder_targets"), 1);
        assert_eq!(
            placeholder_target_row(&conn, "p1"),
            (
                "file".to_string(),
                "target.ts".to_string(),
                "name".to_string(),
                "change".to_string(),
                "caller.ts".to_string(),
            )
        );
    }

    /// A node with no target writes no `placeholder_targets` row at all - the
    /// overwhelming majority of nodes, mirroring `declarations`' own "no rows
    /// for the ordinary case" shape.
    #[test]
    fn a_node_with_no_target_writes_no_placeholder_target_row() {
        let mut conn = setup();
        apply_diff(
            &mut conn,
            &Diff {
                upsert_nodes: vec![NodeRecord::new(
                    "n1",
                    "Function",
                    "foo",
                    "foo",
                    "src/lib.ts",
                    "typescript",
                )],
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(count(&conn, "placeholder_targets"), 0);
    }

    /// Re-upserting a placeholder whose target changed (a reparse that
    /// resolves the import to a different file) replaces the row rather than
    /// accumulating a second one - the same "replace, don't merge" contract
    /// `declarations` already has. Discriminates: drop the unconditional
    /// `DELETE FROM placeholder_targets` this test relies on and the second
    /// assertion below fails with two rows, or the stale `target.ts` scope.
    #[test]
    fn re_upserting_a_placeholder_replaces_its_target_instead_of_accumulating_one() {
        let mut conn = setup();
        apply_diff(
            &mut conn,
            &Diff {
                upsert_nodes: vec![placeholder("p1", "caller.ts", file_scoped_target("target.ts", "change"))],
                ..Default::default()
            },
        )
        .unwrap();

        apply_diff(
            &mut conn,
            &Diff {
                upsert_nodes: vec![placeholder("p1", "caller.ts", file_scoped_target("other.ts", "change"))],
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(count(&conn, "placeholder_targets"), 1, "must not accumulate a second row");
        assert_eq!(
            placeholder_target_row(&conn, "p1").1,
            "other.ts",
            "the stale scope must not survive the re-upsert"
        );
    }

    /// The acceptance criterion at its sharpest: `PRAGMA foreign_keys` is OFF
    /// on the connection the daemon actually runs on
    /// (`storage::connection::open`), so `placeholder_targets`' own `ON
    /// DELETE CASCADE` never fires there - `apply_diff` itself has to delete
    /// the row explicitly, exactly like it already does for `declarations`.
    /// Tested in that same pragma state, not the `foreign_keys = ON` state
    /// most of this module's tests use, because that is the state that would
    /// actually hide a regression here: with `foreign_keys` ON, SQLite's own
    /// cascade would silently paper over a missing explicit delete and this
    /// test would pass for the wrong reason. Discriminates: comment out the
    /// `DELETE FROM placeholder_targets WHERE nodeId = ?1` line in the
    /// `delete_node_ids` loop and the final assertion fails, with the target
    /// row still present under the deleted node's id.
    #[test]
    fn deleting_a_placeholder_node_takes_its_target_with_it_without_foreign_keys() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "OFF").unwrap();
        schema::apply(&conn).unwrap();

        apply_diff(
            &mut conn,
            &Diff {
                upsert_nodes: vec![placeholder("p1", "caller.ts", file_scoped_target("target.ts", "change"))],
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(count(&conn, "placeholder_targets"), 1);

        apply_diff(&mut conn, &Diff { delete_node_ids: vec!["p1".to_string()], ..Default::default() })
            .unwrap();

        assert_eq!(count(&conn, "nodes"), 0);
        assert_eq!(
            count(&conn, "placeholder_targets"),
            0,
            "orphaned rows would be handed to the next node given this id"
        );
    }

    /// `visibility`/`visibilityContainer`/`container` round-trip through
    /// `apply_diff`, and `exported` - the `GENERATED ALWAYS` column - tracks
    /// `visibility` with no write of its own. Discriminates against a
    /// regression that reintroduces writing `exported` directly (which would
    /// fail to compile against a generated column) as much as it does against
    /// one that stops writing `visibility` at all (which would leave every
    /// node `'file'`/not-exported regardless of what was asked for).
    #[test]
    fn visibility_and_container_round_trip_and_exported_is_derived() {
        let mut conn = setup();
        let mut node = NodeRecord::new("n1", "Function", "Close", "Server.Close", "server.go", "go");
        node.visibility = "container".to_string();
        node.visibility_container = Some("github.com/x/app/server".to_string());
        node.container = Some("github.com/x/app/server".to_string());
        apply_diff(&mut conn, &Diff { upsert_nodes: vec![node], ..Default::default() }).unwrap();

        let (visibility, visibility_container, container, exported): (
            String,
            Option<String>,
            Option<String>,
            bool,
        ) = conn
            .query_row(
                "SELECT visibility, visibilityContainer, container, exported FROM nodes WHERE id = 'n1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(visibility, "container");
        assert_eq!(visibility_container.as_deref(), Some("github.com/x/app/server"));
        assert_eq!(container.as_deref(), Some("github.com/x/app/server"));
        assert!(!exported, "container visibility is not public");

        let mut public_node = NodeRecord::new("n2", "Function", "Run", "Run", "server.go", "go");
        public_node.visibility = "public".to_string();
        apply_diff(&mut conn, &Diff { upsert_nodes: vec![public_node], ..Default::default() }).unwrap();
        let exported: bool =
            conn.query_row("SELECT exported FROM nodes WHERE id = 'n2'", [], |row| row.get(0)).unwrap();
        assert!(exported, "public visibility must derive exported = true");
    }

    #[test]
    fn an_edges_declaration_binding_round_trips_and_is_upgradable_in_place() {
        let mut conn = setup();
        apply_diff(
            &mut conn,
            &Diff {
                upsert_nodes: vec![
                    NodeRecord::new("n1", "Function", "caller", "caller", "src/lib.ts", "typescript"),
                    NodeRecord::new("n2", "Function", "parse", "parse", "src/lib.ts", "typescript"),
                ],
                upsert_edges: vec![EdgeRecord::new("e1", "n1", "n2", "CALLS", "tree-sitter", false)],
                ..Default::default()
            },
        )
        .unwrap();

        let unbound: Option<i64> =
            conn.query_row("SELECT toDeclaration FROM edges WHERE id = 'e1'", [], |row| row.get(0)).unwrap();
        assert_eq!(unbound, None, "the structural pass binds no declaration");

        // The semantic pass' shape: the same edge re-sent under its own id,
        // now carrying what the checker resolved. Note this only ever happens
        // for an edge whose id already accounts for the binding - see
        // `edgeIdFor` - but the write path must carry the column either way.
        let mut upgraded = EdgeRecord::new("e1", "n1", "n2", "CALLS", "ts-compiler", true);
        upgraded.to_declaration = Some(1);
        apply_diff(&mut conn, &Diff { upsert_edges: vec![upgraded], ..Default::default() }).unwrap();

        let (source, engine, bound): (String, String, Option<i64>) = conn
            .query_row("SELECT source, engine, toDeclaration FROM edges WHERE id = 'e1'", [], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .unwrap();
        assert_eq!(source, "semantic", "the legacy 'ts-compiler' string maps onto the v2 tier");
        assert_eq!(engine, "ts-compiler", "and the legacy engine name is preserved in the new column");
        assert_eq!(bound, Some(1));
        assert_eq!(count(&conn, "edges"), 1, "an upgrade updates the row in place");
    }

    #[test]
    fn failed_write_mid_batch_leaves_nothing_committed() {
        let mut conn = setup();

        // n1 upserts fine, but the edge references a node ("missing") that
        // never exists - with foreign_keys=ON this INSERT fails, and the
        // whole transaction (including the n1 upsert before it) must roll
        // back rather than leaving n1 committed on its own.
        let diff = Diff {
            upsert_nodes: vec![NodeRecord::new("n1", "Function", "foo", "m::foo", "src/lib.rs", "rust")],
            upsert_edges: vec![EdgeRecord::new("e1", "n1", "missing", "CALLS", "tree-sitter", false)],
            ..Default::default()
        };

        let result = apply_diff(&mut conn, &diff);
        assert!(result.is_err());
        assert_eq!(count(&conn, "nodes"), 0, "node upsert must not survive a rolled-back transaction");
        assert_eq!(count(&conn, "edges"), 0);
    }
}
