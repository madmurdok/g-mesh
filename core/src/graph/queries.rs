use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension, Row};

use crate::graph::symbol_links::{PENDING_SYMBOL_NATIVE_KIND, REEXPORT_NATIVE_KIND};
use crate::storage::write::{self, Diff, EdgeRecord, NodeRecord};

// Every "which symbol is this?" lookup below excludes both of
// `graph::symbol_links`' placeholder `nativeKind`s. A pending symbol
// placeholder carries an imported symbol's *name*, and a re-export
// placeholder the name a barrel republishes, while both stand for a
// definition that lives in another file entirely - so answering a name,
// qualifiedName or position query with one would point the caller at a
// pass-through instead of at the definition it asked for. `IS NOT` rather
// than `<>` or `NOT IN`, so an ordinary node's NULL `nativeKind` still
// passes.

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
        // Deliberately not joined: almost no node has declaration rows, and
        // every reader of this function today asks about the symbol as a
        // whole, which the flat fields above already answer. See the field's
        // own doc comment for why a record read this way must not be written
        // straight back.
        declarations: Vec::new(),
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
        to_declaration: row.get("toDeclaration")?,
    })
}

pub fn upsert_node(conn: &mut Connection, node: NodeRecord) -> Result<()> {
    write::apply_diff(conn, &Diff { upsert_nodes: vec![node], ..Default::default() })
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
        Some(_) => conn.prepare(
            "SELECT * FROM nodes WHERE name = ?1 AND nativeKind IS NOT ?2 AND nativeKind IS NOT ?3 AND filePath = ?4",
        )?,
        None => conn.prepare(
            "SELECT * FROM nodes WHERE name = ?1 AND nativeKind IS NOT ?2 AND nativeKind IS NOT ?3",
        )?,
    };
    let rows = match file_path {
        Some(fp) => {
            stmt.query_map(params![name, PENDING_SYMBOL_NATIVE_KIND, REEXPORT_NATIVE_KIND, fp], map_node_row)?
        }
        None => {
            stmt.query_map(params![name, PENDING_SYMBOL_NATIVE_KIND, REEXPORT_NATIVE_KIND], map_node_row)?
        }
    };
    rows.collect::<rusqlite::Result<_>>().context("failed to look up nodes by name")
}

/// Nodes declared in a file whose stem is `name` - `DropdownMenuGroup` finds
/// `.../DropdownMenuGroup.tsx`.
///
/// Exists for one failure that looks like a bug to whoever hits it: a default
/// import binds the exporting file's declaration under whatever local name the
/// importer chose (`import DropdownMenuGroup from "./DropdownMenuGroup"`), and
/// that local name is never indexed - see `graph::symbol_links`' module doc,
/// "the local name never reaches this index at all". So the name a caller is
/// reading at every use site resolves to nothing, while the declaration it
/// binds sits in the index under a different name. The file's own name is the
/// one link between them that the index does hold.
///
/// Only ever called on the miss path, which is why a `LIKE` with a leading
/// wildcard is acceptable here and would not be on a hot one. Case-insensitive
/// by SQLite's default ASCII `LIKE`, deliberately: `import Foo from "./foo"` is
/// the same situation.
pub fn find_in_file_named(conn: &Connection, name: &str, limit: usize) -> Result<Vec<NodeRecord>> {
    // A name carrying a separator or an extension is not a module stem, and
    // would turn the patterns below into something that matches far too much.
    if name.is_empty() || name.contains(['/', '\\', '.']) {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare(
        "SELECT * FROM nodes \
         WHERE (filePath LIKE '%/' || ?1 || '.%' OR filePath LIKE ?1 || '.%') \
           AND nativeKind IS NOT ?2 AND nativeKind IS NOT ?3 \
           AND kind IS NOT 'File' AND kind IS NOT 'Module' \
         ORDER BY exported DESC, startLine ASC \
         LIMIT ?4",
    )?;
    let rows = stmt.query_map(
        params![name, PENDING_SYMBOL_NATIVE_KIND, REEXPORT_NATIVE_KIND, limit as i64],
        map_node_row,
    )?;
    rows.collect::<rusqlite::Result<_>>().context("failed to look up nodes by file name")
}

/// Indexed files sitting under `prefix`, entry points first.
///
/// For the caller who asked about a package or a directory rather than a
/// file: `packages/math` is not a node, but `packages/math/src/index.ts` is,
/// and it is what they meant. Ordering puts an `index.*` first because that is
/// what a package specifier resolves to, then shortest path, so the head of
/// the list is the entry point rather than whichever file sorted first.
///
/// Miss path only, like `find_in_file_named` above - a `LIKE` anchored on a
/// prefix can use no index here and does not need to.
pub fn find_files_under(conn: &Connection, prefix: &str, limit: usize) -> Result<Vec<NodeRecord>> {
    let trimmed = prefix.trim_end_matches('/');
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare(
        "SELECT * FROM nodes \
         WHERE kind = 'File' AND filePath LIKE ?1 || '/%' \
         ORDER BY (filePath LIKE '%/index.%') DESC, LENGTH(filePath) ASC \
         LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![trimmed, limit as i64], map_node_row)?;
    rows.collect::<rusqlite::Result<_>>().context("failed to look up files under a prefix")
}

/// Indexed files under any directory named `dir`, entry points first.
///
/// The second half of the package-name case: `@excalidraw/math` is not a path,
/// but a directory called `math` exists and holds the files. Matches a path
/// segment, not a substring - `/math/` - so `mathutils` does not qualify.
pub fn find_files_ending_in_dir(conn: &Connection, dir: &str, limit: usize) -> Result<Vec<NodeRecord>> {
    if dir.is_empty() || dir.contains('/') {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare(
        "SELECT * FROM nodes \
         WHERE kind = 'File' AND (filePath LIKE '%/' || ?1 || '/%' OR filePath LIKE ?1 || '/%') \
         ORDER BY (filePath LIKE '%/index.%') DESC, LENGTH(filePath) ASC \
         LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![dir, limit as i64], map_node_row)?;
    rows.collect::<rusqlite::Result<_>>().context("failed to look up files by directory name")
}

pub fn find_by_qualified_name(
    conn: &Connection,
    qualified_name: &str,
    file_path: Option<&str>,
) -> Result<Vec<NodeRecord>> {
    let mut stmt = match file_path {
        Some(_) => conn.prepare(
            "SELECT * FROM nodes WHERE qualifiedName = ?1 AND nativeKind IS NOT ?2 AND nativeKind IS NOT ?3 AND filePath = ?4",
        )?,
        None => conn.prepare(
            "SELECT * FROM nodes WHERE qualifiedName = ?1 AND nativeKind IS NOT ?2 AND nativeKind IS NOT ?3",
        )?,
    };
    let rows = match file_path {
        Some(fp) => stmt.query_map(
            params![qualified_name, PENDING_SYMBOL_NATIVE_KIND, REEXPORT_NATIVE_KIND, fp],
            map_node_row,
        )?,
        None => stmt.query_map(
            params![qualified_name, PENDING_SYMBOL_NATIVE_KIND, REEXPORT_NATIVE_KIND],
            map_node_row,
        )?,
    };
    rows.collect::<rusqlite::Result<_>>().context("failed to look up nodes by qualifiedName")
}

/// Finds the `File` node for a project-relative path, e.g. resolving
/// `get_file_outline`'s anchor. `File` nodes' own `filePath` is the path
/// itself (see the js-ts plugin's extractor), so this is a plain lookup, not
/// a join.
pub fn find_file_node(conn: &Connection, file_path: &str) -> Result<Option<NodeRecord>> {
    conn.query_row(
        "SELECT * FROM nodes WHERE kind = 'File' AND filePath = ?1",
        params![file_path],
        map_node_row,
    )
    .optional()
    .context("failed to look up file node")
}

/// Finds the innermost node enclosing a cursor position, e.g. resolving
/// `find_definition`'s file+position input. Multiple nodes can contain a
/// position (a `File` spans the whole file, a `Function` inside it spans
/// just itself) - ordering by span size ascending picks the smallest one
/// first, which is always the most specific.
pub fn find_by_position(
    conn: &Connection,
    file_path: &str,
    line: u32,
    col: u32,
) -> Result<Option<NodeRecord>> {
    let (line, col) = (line as i64, col as i64);
    conn.query_row(
        "SELECT * FROM nodes \
         WHERE filePath = ?1 \
           AND nativeKind IS NOT ?4 \
           AND nativeKind IS NOT ?5 \
           AND (startLine < ?2 OR (startLine = ?2 AND startCol <= ?3)) \
           AND (endLine > ?2 OR (endLine = ?2 AND endCol >= ?3)) \
         ORDER BY (endLine - startLine) ASC, (endCol - startCol) ASC \
         LIMIT 1",
        params![file_path, line, col, PENDING_SYMBOL_NATIVE_KIND, REEXPORT_NATIVE_KIND],
        map_node_row,
    )
    .optional()
    .context("failed to look up node by position")
}

pub fn upsert_edge(conn: &mut Connection, edge: EdgeRecord) -> Result<()> {
    write::apply_diff(conn, &Diff { upsert_edges: vec![edge], ..Default::default() })
}

pub fn get_edge(conn: &Connection, id: &str) -> Result<Option<EdgeRecord>> {
    conn.query_row("SELECT * FROM edges WHERE id = ?1", params![id], map_edge_row)
        .optional()
        .context("failed to look up edge by id")
}

pub fn delete_edge(conn: &mut Connection, id: &str) -> Result<()> {
    write::apply_diff(conn, &Diff { delete_edge_ids: vec![id.to_string()], ..Default::default() })
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
        upsert_node(&mut conn, NodeRecord::new("n1", "Function", "foo", "m::foo", "src/lib.rs", "rust"))
            .unwrap();

        let found = get_node(&conn, "n1").unwrap().unwrap();
        assert_eq!(found.name, "foo");
        assert_eq!(found.qualified_name, "m::foo");

        assert!(get_node(&conn, "missing").unwrap().is_none());
    }

    #[test]
    fn upsert_overwrites_existing_node() {
        let mut conn = setup();
        upsert_node(&mut conn, NodeRecord::new("n1", "Function", "foo", "m::foo", "src/lib.rs", "rust"))
            .unwrap();
        upsert_node(
            &mut conn,
            NodeRecord::new("n1", "Function", "renamed", "m::renamed", "src/lib.rs", "rust"),
        )
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
        upsert_node(&mut conn, NodeRecord::new("n1", "Function", "foo", "m::foo", "src/lib.rs", "rust"))
            .unwrap();
        upsert_node(&mut conn, NodeRecord::new("n2", "Function", "bar", "m::bar", "src/lib.rs", "rust"))
            .unwrap();
        upsert_node(&mut conn, NodeRecord::new("n3", "Function", "baz", "m::baz", "src/lib.rs", "rust"))
            .unwrap();
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
    fn find_file_node_looks_up_by_file_path_not_name() {
        let mut conn = setup();
        upsert_node(
            &mut conn,
            NodeRecord::new("file1", "File", "lib.rs", "src/lib.rs", "src/lib.rs", "rust"),
        )
        .unwrap();
        upsert_node(&mut conn, NodeRecord::new("fn1", "Function", "run", "pkg::run", "src/lib.rs", "rust"))
            .unwrap();

        let found = find_file_node(&conn, "src/lib.rs").unwrap().unwrap();
        assert_eq!(
            found.id, "file1",
            "must return the File node, not the unrelated symbol sharing its filePath"
        );

        assert!(find_file_node(&conn, "missing.rs").unwrap().is_none());
    }

    #[test]
    fn delete_edge_removes_only_that_edge() {
        let mut conn = setup();
        upsert_node(&mut conn, NodeRecord::new("n1", "Function", "foo", "m::foo", "src/lib.rs", "rust"))
            .unwrap();
        upsert_node(&mut conn, NodeRecord::new("n2", "Function", "bar", "m::bar", "src/lib.rs", "rust"))
            .unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e1", "n1", "n2", "CALLS", "tree-sitter", false)).unwrap();

        delete_edge(&mut conn, "e1").unwrap();

        assert!(get_edge(&conn, "e1").unwrap().is_none());
        assert!(get_node(&conn, "n1").unwrap().is_some(), "deleting an edge must not delete its nodes");
        assert!(get_node(&conn, "n2").unwrap().is_some());
    }

    #[test]
    fn name_and_qualified_name_lookup_returns_all_ambiguous_matches() {
        let mut conn = setup();
        upsert_node(&mut conn, NodeRecord::new("n1", "Function", "run", "pkg_a::run", "a/lib.rs", "rust"))
            .unwrap();
        upsert_node(&mut conn, NodeRecord::new("n2", "Function", "run", "pkg_b::run", "b/lib.rs", "rust"))
            .unwrap();
        upsert_node(
            &mut conn,
            NodeRecord::new("n3", "Function", "other", "pkg_a::other", "a/lib.rs", "rust"),
        )
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

    fn node_with_span(
        id: &str,
        kind: &str,
        file_path: &str,
        start: (i64, i64),
        end: (i64, i64),
    ) -> NodeRecord {
        let mut node = NodeRecord::new(id, kind, id, id, file_path, "rust");
        node.start_line = start.0;
        node.start_col = start.1;
        node.end_line = end.0;
        node.end_col = end.1;
        node
    }

    #[test]
    fn find_by_position_picks_the_innermost_enclosing_node() {
        let mut conn = setup();
        upsert_node(&mut conn, node_with_span("file1", "File", "a/lib.rs", (0, 0), (20, 0))).unwrap();
        upsert_node(&mut conn, node_with_span("fn1", "Function", "a/lib.rs", (5, 0), (10, 1))).unwrap();

        let found = find_by_position(&conn, "a/lib.rs", 7, 2).unwrap().unwrap();
        assert_eq!(found.id, "fn1", "the nested function must win over the enclosing file");
    }

    #[test]
    fn find_by_position_returns_none_outside_every_node() {
        let mut conn = setup();
        upsert_node(&mut conn, node_with_span("file1", "File", "a/lib.rs", (0, 0), (20, 0))).unwrap();
        upsert_node(&mut conn, node_with_span("fn1", "Function", "a/lib.rs", (5, 0), (10, 1))).unwrap();

        assert!(find_by_position(&conn, "a/lib.rs", 50, 0).unwrap().is_none());
    }
}
