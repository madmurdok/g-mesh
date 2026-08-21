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
    pub exported: bool,
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
    pub source: String,
    pub resolved: bool,
    /// Which of the target's declarations this edge binds, as an ordinal into
    /// its declaration list. `None` for everything that binds no particular
    /// one - every structural-pass edge, and every edge whose target has a
    /// single declaration. See `edges.toDeclaration` in `storage::schema`.
    pub to_declaration: Option<i64>,
}

impl EdgeRecord {
    pub fn new(
        id: impl Into<String>,
        from_id: impl Into<String>,
        to_id: impl Into<String>,
        kind: impl Into<String>,
        source: impl Into<String>,
        resolved: bool,
    ) -> Self {
        Self {
            id: id.into(),
            from_id: from_id.into(),
            to_id: to_id.into(),
            kind: kind.into(),
            source: source.into(),
            resolved,
            to_declaration: None,
        }
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
        // Explicitly, rather than leaning on the child table's ON DELETE
        // CASCADE: `foreign_keys` is off on the connection the daemon actually
        // runs on (`storage::connection::open` sets WAL and nothing else), so
        // the cascade only fires in tests that switch it on. Left orphaned,
        // these rows would be handed to whoever next takes this node's id.
        tx.execute("DELETE FROM declarations WHERE nodeId = ?1", params![id])
            .context("failed to delete a node's declarations")?;
        tx.execute("DELETE FROM nodes WHERE id = ?1", params![id]).context("failed to delete node")?;
    }
    for node in &diff.upsert_nodes {
        tx.execute(
            "INSERT INTO nodes (id, kind, name, qualifiedName, filePath, startLine, startCol, endLine, endCol, signature, exported, docComment, language, nativeKind, hasSyntaxErrors)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
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
                exported = excluded.exported,
                docComment = excluded.docComment,
                language = excluded.language,
                nativeKind = excluded.nativeKind,
                hasSyntaxErrors = excluded.hasSyntaxErrors",
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
                node.exported,
                node.doc_comment,
                node.language,
                node.native_kind,
                node.has_syntax_errors,
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
    }
    for edge in &diff.upsert_edges {
        tx.execute(
            "INSERT INTO edges (id, fromId, toId, kind, source, resolved, toDeclaration)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(id) DO UPDATE SET
                fromId = excluded.fromId,
                toId = excluded.toId,
                kind = excluded.kind,
                source = excluded.source,
                resolved = excluded.resolved,
                toDeclaration = excluded.toDeclaration",
            params![
                edge.id,
                edge.from_id,
                edge.to_id,
                edge.kind,
                edge.source,
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

        let (source, bound): (String, Option<i64>) = conn
            .query_row("SELECT source, toDeclaration FROM edges WHERE id = 'e1'", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .unwrap();
        assert_eq!(source, "ts-compiler");
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
