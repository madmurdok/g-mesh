use anyhow::{Context, Result};
use rusqlite::{params, Connection};

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
        tx.execute("DELETE FROM edges WHERE id = ?1", params![id])
            .context("failed to delete edge")?;
    }
    for id in &diff.delete_node_ids {
        tx.execute("DELETE FROM nodes WHERE id = ?1", params![id])
            .context("failed to delete node")?;
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
    }
    for edge in &diff.upsert_edges {
        tx.execute(
            "INSERT INTO edges (id, fromId, toId, kind, source, resolved)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(id) DO UPDATE SET
                fromId = excluded.fromId,
                toId = excluded.toId,
                kind = excluded.kind,
                source = excluded.source,
                resolved = excluded.resolved",
            params![edge.id, edge.from_id, edge.to_id, edge.kind, edge.source, edge.resolved],
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
        conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| row.get(0))
            .unwrap()
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
