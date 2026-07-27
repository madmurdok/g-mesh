use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension, Row};

use crate::storage::write::{self, Diff, EdgeRecord, NodeRecord};

pub(crate) fn map_node_row(row: &Row) -> rusqlite::Result<NodeRecord> {
    Ok(NodeRecord {
        id: row.get("id")?,
        kind: row.get("kind")?,
        name: row.get("name")?,
        qualified_name: row.get("qualifiedName")?,
        file_path: row.get("filePath")?,
        start_line: row.get("startLine")?,
        start_col: row.get("startCol")?,
        end_line: row.get("endLine")?,
        end_col: row.get("endCol")?,
        signature: row.get("signature")?,
        exported: row.get("exported")?,
        doc_comment: row.get("docComment")?,
        language: row.get("language")?,
        native_kind: row.get("nativeKind")?,
        has_syntax_errors: row.get("hasSyntaxErrors")?,
    })
}

fn map_edge_row(row: &Row) -> rusqlite::Result<EdgeRecord> {
    Ok(EdgeRecord {
        id: row.get("id")?,
        from_id: row.get("fromId")?,
        to_id: row.get("toId")?,
        kind: row.get("kind")?,
        source: row.get("source")?,
        resolved: row.get("resolved")?,
    })
}

pub fn upsert_node(conn: &mut Connection, node: NodeRecord) -> Result<()> {
    write::apply_diff(
        conn,
        &Diff {
            upsert_nodes: vec![node],
            ..Default::default()
        },
    )
}

pub fn get_node(conn: &Connection, id: &str) -> Result<Option<NodeRecord>> {
    conn.query_row("SELECT * FROM nodes WHERE id = ?1", params![id], map_node_row)
        .optional()
        .context("failed to look up node by id")
}

/// Deletes a node and every edge incident to it (fromId or toId), atomically.
pub fn delete_node(conn: &mut Connection, id: &str) -> Result<()> {
    let mut stmt = conn.prepare("SELECT id FROM edges WHERE fromId = ?1 OR toId = ?1")?;
    let incident_edge_ids: Vec<String> = stmt
        .query_map(params![id], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()
        .context("failed to look up incident edges")?;
    drop(stmt);

    write::apply_diff(
        conn,
        &Diff {
            delete_edge_ids: incident_edge_ids,
            delete_node_ids: vec![id.to_string()],
            ..Default::default()
        },
    )
}

pub fn find_by_name(conn: &Connection, name: &str, file_path: Option<&str>) -> Result<Vec<NodeRecord>> {
    let mut stmt = match file_path {
        Some(_) => conn.prepare("SELECT * FROM nodes WHERE name = ?1 AND filePath = ?2")?,
        None => conn.prepare("SELECT * FROM nodes WHERE name = ?1")?,
    };
    let rows = match file_path {
        Some(fp) => stmt.query_map(params![name, fp], map_node_row)?,
        None => stmt.query_map(params![name], map_node_row)?,
    };
    rows.collect::<rusqlite::Result<_>>().context("failed to look up nodes by name")
}

pub fn find_by_qualified_name(
    conn: &Connection,
    qualified_name: &str,
    file_path: Option<&str>,
) -> Result<Vec<NodeRecord>> {
    let mut stmt = match file_path {
        Some(_) => conn.prepare("SELECT * FROM nodes WHERE qualifiedName = ?1 AND filePath = ?2")?,
        None => conn.prepare("SELECT * FROM nodes WHERE qualifiedName = ?1")?,
    };
    let rows = match file_path {
        Some(fp) => stmt.query_map(params![qualified_name, fp], map_node_row)?,
        None => stmt.query_map(params![qualified_name], map_node_row)?,
    };
    rows.collect::<rusqlite::Result<_>>().context("failed to look up nodes by qualifiedName")
}

pub fn upsert_edge(conn: &mut Connection, edge: EdgeRecord) -> Result<()> {
    write::apply_diff(
        conn,
        &Diff {
            upsert_edges: vec![edge],
            ..Default::default()
        },
    )
}

pub fn get_edge(conn: &Connection, id: &str) -> Result<Option<EdgeRecord>> {
    conn.query_row("SELECT * FROM edges WHERE id = ?1", params![id], map_edge_row)
        .optional()
        .context("failed to look up edge by id")
}

pub fn delete_edge(conn: &mut Connection, id: &str) -> Result<()> {
    write::apply_diff(
        conn,
        &Diff {
            delete_edge_ids: vec![id.to_string()],
            ..Default::default()
        },
    )
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

    #[test]
    fn insert_then_lookup_by_id() {
        let mut conn = setup();
        upsert_node(&mut conn, NodeRecord::new("n1", "Function", "foo", "m::foo", "src/lib.rs", "rust")).unwrap();

        let found = get_node(&conn, "n1").unwrap().unwrap();
        assert_eq!(found.name, "foo");
        assert_eq!(found.qualified_name, "m::foo");

        assert!(get_node(&conn, "missing").unwrap().is_none());
    }

    #[test]
    fn upsert_overwrites_existing_node() {
        let mut conn = setup();
        upsert_node(&mut conn, NodeRecord::new("n1", "Function", "foo", "m::foo", "src/lib.rs", "rust")).unwrap();
        upsert_node(&mut conn, NodeRecord::new("n1", "Function", "renamed", "m::renamed", "src/lib.rs", "rust"))
            .unwrap();

        let found = get_node(&conn, "n1").unwrap().unwrap();
        assert_eq!(found.name, "renamed");
        assert_eq!(found.qualified_name, "m::renamed");

        let count: i64 = conn.query_row("SELECT COUNT(*) FROM nodes", [], |row| row.get(0)).unwrap();
        assert_eq!(count, 1, "upsert must not create a duplicate row");
    }

    #[test]
    fn delete_removes_node_and_incident_edges() {
        let mut conn = setup();
        upsert_node(&mut conn, NodeRecord::new("n1", "Function", "foo", "m::foo", "src/lib.rs", "rust")).unwrap();
        upsert_node(&mut conn, NodeRecord::new("n2", "Function", "bar", "m::bar", "src/lib.rs", "rust")).unwrap();
        upsert_node(&mut conn, NodeRecord::new("n3", "Function", "baz", "m::baz", "src/lib.rs", "rust")).unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e1", "n1", "n2", "CALLS", "tree-sitter", false)).unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e2", "n3", "n1", "CALLS", "tree-sitter", false)).unwrap();

        delete_node(&mut conn, "n1").unwrap();

        assert!(get_node(&conn, "n1").unwrap().is_none());
        assert!(get_edge(&conn, "e1").unwrap().is_none(), "outgoing edge from deleted node must be gone");
        assert!(get_edge(&conn, "e2").unwrap().is_none(), "incoming edge to deleted node must be gone");
        assert!(get_node(&conn, "n2").unwrap().is_some(), "unrelated node must survive");
        assert!(get_node(&conn, "n3").unwrap().is_some(), "unrelated node must survive");
    }

    #[test]
    fn delete_edge_removes_only_that_edge() {
        let mut conn = setup();
        upsert_node(&mut conn, NodeRecord::new("n1", "Function", "foo", "m::foo", "src/lib.rs", "rust")).unwrap();
        upsert_node(&mut conn, NodeRecord::new("n2", "Function", "bar", "m::bar", "src/lib.rs", "rust")).unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e1", "n1", "n2", "CALLS", "tree-sitter", false)).unwrap();

        delete_edge(&mut conn, "e1").unwrap();

        assert!(get_edge(&conn, "e1").unwrap().is_none());
        assert!(get_node(&conn, "n1").unwrap().is_some(), "deleting an edge must not delete its nodes");
        assert!(get_node(&conn, "n2").unwrap().is_some());
    }

    #[test]
    fn name_and_qualified_name_lookup_returns_all_ambiguous_matches() {
        let mut conn = setup();
        upsert_node(&mut conn, NodeRecord::new("n1", "Function", "run", "pkg_a::run", "a/lib.rs", "rust")).unwrap();
        upsert_node(&mut conn, NodeRecord::new("n2", "Function", "run", "pkg_b::run", "b/lib.rs", "rust")).unwrap();
        upsert_node(&mut conn, NodeRecord::new("n3", "Function", "other", "pkg_a::other", "a/lib.rs", "rust"))
            .unwrap();

        let by_name = find_by_name(&conn, "run", None).unwrap();
        assert_eq!(by_name.len(), 2, "ambiguous name must return every matching node");

        let scoped = find_by_name(&conn, "run", Some("a/lib.rs")).unwrap();
        assert_eq!(scoped.len(), 1);
        assert_eq!(scoped[0].id, "n1");

        let by_qualified = find_by_qualified_name(&conn, "pkg_a::run", None).unwrap();
        assert_eq!(by_qualified.len(), 1);
        assert_eq!(by_qualified[0].id, "n1");
    }
}
