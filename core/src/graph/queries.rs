use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension, Row};

use crate::graph::containers::CONTAINER_NATIVE_KIND;
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
//
// The name, qualifiedName and position lookups exclude core's container
// nodes (`graph::containers::CONTAINER_NATIVE_KIND`) as well, for a
// different reason with the same effect: a container is a real node, but it
// has no source - `filePath` is `''` and its range is zero - so an answer
// built on one (a source snippet, a staleness check on its file, an anchor
// echo telling the caller where it lives) points at nothing. Its name is also
// its whole key (`github.com/x/app/server`), which no caller asking "where is
// `server` defined" writes. `find_in_file_named` and the file lookups below
// need no such filter: they already refuse `Module` or require `File`.

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
        // `exported` is read straight off the database's own `GENERATED
        // ALWAYS` column (`storage::schema`'s DDL) - it is guaranteed to
        // already agree with `visibility` below, since nothing can write it
        // any other way. See `NodeRecord.exported`'s own doc comment.
        exported: row.get("exported")?,
        visibility: row.get("visibility")?,
        visibility_container: row.get("visibilityContainer")?,
        container: row.get("container")?,
        // Not a `nodes` column - it lives on the container's own
        // `containers.parentKey` - so there is nothing to read it from here.
        // See `NodeRecord.container_parent` for why a record read this way
        // must not be written straight back.
        container_parent: None,
        // Deliberately not joined, same reasoning as `declarations` just
        // below: a read via this function is never handed back to
        // `apply_diff` expecting an existing `placeholder_targets` row to be
        // preserved (see `NodeRecord.target`'s own doc comment for why that
        // would be actively wrong - `apply_diff` reads a `None` here as
        // "delete this node's target").
        target: None,
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
        engine: row.get("engine")?,
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
            "SELECT * FROM nodes WHERE name = ?1 AND nativeKind IS NOT ?2 AND nativeKind IS NOT ?3 AND nativeKind IS NOT ?4 AND filePath = ?5",
        )?,
        None => conn.prepare(
            "SELECT * FROM nodes WHERE name = ?1 AND nativeKind IS NOT ?2 AND nativeKind IS NOT ?3 AND nativeKind IS NOT ?4",
        )?,
    };
    let rows = match file_path {
        Some(fp) => stmt.query_map(
            params![name, PENDING_SYMBOL_NATIVE_KIND, REEXPORT_NATIVE_KIND, CONTAINER_NATIVE_KIND, fp],
            map_node_row,
        )?,
        None => stmt.query_map(
            params![name, PENDING_SYMBOL_NATIVE_KIND, REEXPORT_NATIVE_KIND, CONTAINER_NATIVE_KIND],
            map_node_row,
        )?,
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

/// Builds the boolean `ORDER BY` expression (and the `?` parameter values it
/// binds, in the order they appear in it) that ranks a `filePath` matching
/// any of `entry_points` ahead of one that does not - the shared core behind
/// [`find_files_under`]'s and [`find_files_ending_in_dir`]'s ordering.
///
/// # Where `entry_points` comes from, and why this function does not know
///
/// `entry_points` is the caller's job to assemble, concretely the union of
/// every discovered plugin's `[plugin.workspace] entry_points`
/// (`daemon::manifest::WorkspaceConfig`,
/// `daemon::registry::PluginRegistry::entry_points`), passed in rather than
/// looked up here so this module, which is a thin SQL layer under
/// `storage`/`graph`, never has to depend on `daemon::manifest` to answer a
/// query. A caller with no manifest available at all (this module's own
/// tests below; a hypothetical CLI path that never starts a daemon) passes an
/// empty slice, which this function turns into an always-false expression:
/// nothing ranks first, and both callers fall back to their older,
/// entry-point-blind `LENGTH(filePath)` ordering. The one place this must
/// stay identical to the pre-task behaviour is the bundled setup: the
/// bundled TS plugin's own manifest already declares `entry_points =
/// ["index"]` (`plugins/typescript/plugin.toml`), so a real daemon feeds this
/// function exactly the one-element list that reproduces the old hardcoded
/// `index.*` ordering, byte for byte.
///
/// # Matching semantics: one rule, two shapes
///
/// Each entry is tested one of two ways, chosen by whether it contains a `.`:
///
/// - **No dot** (`"index"`): matches the file's *stem*, any extension -
///   `index.ts`, `index.tsx`, `index.d.ts` all qualify. This is the exact
///   `%/index.%` shape the code being replaced hardcoded.
/// - **Has a dot** (`"mod.rs"`, `"main.rs"`, `"lib.rs"` - Rust's own
///   convention, once a Rust plugin declares it): matches the *exact* file
///   name, with no trailing wildcard after it - an exact entry must not also
///   match `mod.rs.bak` or `mod.rs2`, which a `LIKE 'mod.rs%'` pattern would.
///
/// One rule rather than two independently configurable modes, because a
/// manifest author choosing an entry point only has one real degree of
/// freedom: whether their convention is extension-agnostic (TS's `index`,
/// which must cover `.ts`/`.tsx`/`.d.ts`) or a fixed file name (Rust's
/// `mod.rs`, which must not smear onto neighbouring names).
///
/// # Every entry is escaped before it ever reaches a `LIKE`
///
/// `entry_points` is manifest content, not a literal this module wrote, so it
/// cannot be trusted to contain no LIKE metacharacters - `%` and `_` are both
/// wildcards to SQLite's `LIKE` (`_` matches exactly one arbitrary character),
/// and a manifest is free to declare an entry point that legitimately
/// contains one: Python's own `__init__.py` convention is the motivating
/// case. Left unescaped, `__init__.py` would rank `pkg/abinitcd.py` as if it
/// were the declared entry point - each of its two `_` wildcards consuming
/// one arbitrary character - which is exactly the "language #N+1 pays for
/// core surgery" trap this task exists to close, just moved from a missing
/// parameter to a missed escape. [`escape_like`] backslash-escapes `\`, `%`
/// and `_` in every entry before it is bound, and every `LIKE` clause below
/// carries the matching `ESCAPE '\'`; the equality clause does not, because
/// `=` has no wildcards to escape in the first place.
///
/// # Cost: not an index range scan today, and this does not make it one
///
/// It would be convenient to say this stays "an indexed prefix lookup", but
/// `EXPLAIN QUERY PLAN` on the query both callers run
/// (`WHERE kind = 'File' AND filePath LIKE ?1 || '/%' ORDER BY ...`) says
/// otherwise: it is a full `SCAN nodes`, index or no index, and always has
/// been - `idx_nodes_filePath` (`storage::schema`) never fires here, because
/// SQLite's LIKE-to-range-scan optimization only applies to a *case-sensitive*
/// LIKE (`PRAGMA case_sensitive_like = ON`, or `GLOB`), and this project sets
/// neither; measured directly against a populated `nodes` table with
/// `ANALYZE` run, `filePath LIKE 'pkg1/%'` alone still plans as `SCAN nodes`,
/// while the equivalent `filePath GLOB 'pkg1/*'` plans as
/// `SEARCH nodes USING INDEX idx_nodes_filePath`. Both `find_files_under` and
/// `find_files_ending_in_dir` are miss-path-only (see their own doc comments)
/// and already paid for a full scan before this task. What this function
/// must not do - and does not - is turn one full scan into several, or into
/// one whose per-row cost grows with the project: it stays one query, and the
/// only new per-row cost is evaluating up to `entry_points.len()` extra
/// `LIKE` tests (in practice 1-3, one convention per language actually
/// discovered) instead of the previous single hardcoded one - a constant
/// factor, not a new scan.
fn entry_point_rank_expr(entry_points: &[String]) -> (String, Vec<String>) {
    if entry_points.is_empty() {
        // Not a bare `"0"`: SQLite's `ORDER BY` treats a literal integer as a
        // 1-based reference to a column of the result set ("ORDER BY 1" means
        // "by the first selected column"), so a bare `0` is a "column out of
        // range" error, not a constant `false` - `(1 = 0)`, an expression
        // rather than an integer literal, is what actually means "always
        // false" here.
        return ("(1 = 0)".to_string(), Vec::new());
    }
    let mut clauses: Vec<&'static str> = Vec::with_capacity(entry_points.len());
    let mut params = Vec::with_capacity(entry_points.len() * 2);
    for entry in entry_points {
        let escaped = escape_like(entry);
        if entry.contains('.') {
            // Exact name: the file's own path either ends in "/<entry>" or,
            // for a root-level file, equals <entry> outright. The first
            // branch is a plain `=`, so the raw (unescaped) entry is bound
            // there - only the `LIKE` branch needs the escaped form.
            clauses.push("(filePath = ? OR filePath LIKE '%/' || ? ESCAPE '\\')");
            params.push(entry.clone());
            params.push(escaped);
        } else {
            // Bare stem: the file's own name starts with "<entry>." at the
            // root, or "/<entry>." under some directory - any extension.
            clauses
                .push("(filePath LIKE ? || '.%' ESCAPE '\\' OR filePath LIKE '%/' || ? || '.%' ESCAPE '\\')");
            params.push(escaped.clone());
            params.push(escaped);
        }
    }
    (format!("({})", clauses.join(" OR ")), params)
}

/// Escapes `\`, `%` and `_` in `entry` so it can be interposed into a `LIKE`
/// pattern as a literal string rather than a pattern of its own - paired with
/// `ESCAPE '\'` on every clause that binds the result. See
/// [`entry_point_rank_expr`]'s own doc comment for why an unescaped entry is
/// a real bug and not a theoretical one (`__init__.py`), not just a stylistic
/// nicety.
fn escape_like(entry: &str) -> String {
    let mut escaped = String::with_capacity(entry.len());
    for ch in entry.chars() {
        if matches!(ch, '\\' | '%' | '_') {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    escaped
}

/// Indexed files sitting under `prefix`, entry points first.
///
/// For the caller who asked about a package or a directory rather than a
/// file: `packages/math` is not a node, but `packages/math/src/index.ts` is,
/// and it is what they meant. Ordering puts a declared entry point first -
/// see [`entry_point_rank_expr`] for the matching rule, where `entry_points`
/// comes from, and this query's actual cost - then shortest path, so the head
/// of the list is the entry point rather than whichever file sorted first.
///
/// Miss path only, like `find_in_file_named` above - a `LIKE` anchored on a
/// prefix can use no index here and does not need to (see
/// [`entry_point_rank_expr`]'s cost section for the measurement behind that).
pub fn find_files_under(
    conn: &Connection,
    prefix: &str,
    entry_points: &[String],
    limit: usize,
) -> Result<Vec<NodeRecord>> {
    let trimmed = prefix.trim_end_matches('/');
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    let (rank_expr, rank_params) = entry_point_rank_expr(entry_points);
    let sql = format!(
        "SELECT * FROM nodes \
         WHERE kind = 'File' AND filePath LIKE ? || '/%' \
         ORDER BY {rank_expr} DESC, LENGTH(filePath) ASC \
         LIMIT ?"
    );
    let mut stmt = conn.prepare(&sql)?;
    // A `Vec<rusqlite::types::Value>` bound via `params_from_iter`, rather
    // than `params![...]`/a `Vec<&dyn ToSql>`, because the parameter count
    // varies with `entry_points.len()` - `Value` is rusqlite's own "any bound
    // type, decided at runtime" wrapper, exactly what a variable-length
    // parameter list needs.
    let mut bound: Vec<rusqlite::types::Value> = Vec::with_capacity(2 + rank_params.len());
    bound.push(trimmed.to_string().into());
    bound.extend(rank_params.into_iter().map(Into::into));
    bound.push((limit as i64).into());
    let rows = stmt.query_map(rusqlite::params_from_iter(bound), map_node_row)?;
    rows.collect::<rusqlite::Result<_>>().context("failed to look up files under a prefix")
}

/// Indexed files under any directory named `dir`, entry points first.
///
/// The second half of the package-name case: `@excalidraw/math` is not a path,
/// but a directory called `math` exists and holds the files. Matches a path
/// segment, not a substring - `/math/` - so `mathutils` does not qualify.
/// Entry-point ordering, `entry_points`'s meaning and this query's cost are
/// exactly [`find_files_under`]'s - see [`entry_point_rank_expr`].
pub fn find_files_ending_in_dir(
    conn: &Connection,
    dir: &str,
    entry_points: &[String],
    limit: usize,
) -> Result<Vec<NodeRecord>> {
    if dir.is_empty() || dir.contains('/') {
        return Ok(Vec::new());
    }
    let (rank_expr, rank_params) = entry_point_rank_expr(entry_points);
    let sql = format!(
        "SELECT * FROM nodes \
         WHERE kind = 'File' AND (filePath LIKE '%/' || ? || '/%' OR filePath LIKE ? || '/%') \
         ORDER BY {rank_expr} DESC, LENGTH(filePath) ASC \
         LIMIT ?"
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut bound: Vec<rusqlite::types::Value> = Vec::with_capacity(3 + rank_params.len());
    bound.push(dir.to_string().into());
    bound.push(dir.to_string().into());
    bound.extend(rank_params.into_iter().map(Into::into));
    bound.push((limit as i64).into());
    let rows = stmt.query_map(rusqlite::params_from_iter(bound), map_node_row)?;
    rows.collect::<rusqlite::Result<_>>().context("failed to look up files by directory name")
}

pub fn find_by_qualified_name(
    conn: &Connection,
    qualified_name: &str,
    file_path: Option<&str>,
) -> Result<Vec<NodeRecord>> {
    let mut stmt = match file_path {
        Some(_) => conn.prepare(
            "SELECT * FROM nodes WHERE qualifiedName = ?1 AND nativeKind IS NOT ?2 AND nativeKind IS NOT ?3 AND nativeKind IS NOT ?4 AND filePath = ?5",
        )?,
        None => conn.prepare(
            "SELECT * FROM nodes WHERE qualifiedName = ?1 AND nativeKind IS NOT ?2 AND nativeKind IS NOT ?3 AND nativeKind IS NOT ?4",
        )?,
    };
    let rows = match file_path {
        Some(fp) => stmt.query_map(
            params![
                qualified_name,
                PENDING_SYMBOL_NATIVE_KIND,
                REEXPORT_NATIVE_KIND,
                CONTAINER_NATIVE_KIND,
                fp
            ],
            map_node_row,
        )?,
        None => stmt.query_map(
            params![qualified_name, PENDING_SYMBOL_NATIVE_KIND, REEXPORT_NATIVE_KIND, CONTAINER_NATIVE_KIND],
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

/// Finds the container node(s) whose own key is exactly `key`, across every
/// language a container of that key exists in - the counterpart of
/// [`find_file_node`] for a logical container (a Go import path, a Rust
/// module path, ... - Data Model > Logical containers) rather than a file.
/// Used by `get_dependencies::from_file` (GM-267) to let a caller anchor a
/// walk on a package name directly instead of hunting for one of its files.
///
/// A key is only unique *within* a language (`containers.key`'s own `UNIQUE
/// (language, key)`), so an exact key can legitimately name more than one
/// container project-wide - a Go package and a Rust module happening to
/// share the string. This returns every match rather than picking one; the
/// caller decides what "more than one" means (refuse and name the languages,
/// for `get_dependencies`). One indexed lookup (`containers.key`'s own
/// implicit index, part of `UNIQUE (language, key)`) joined back onto
/// `nodes` by primary key - not a scan of either table.
pub fn find_containers_by_key(conn: &Connection, key: &str) -> Result<Vec<NodeRecord>> {
    let mut stmt = conn
        .prepare("SELECT n.* FROM nodes n JOIN containers c ON c.nodeId = n.id WHERE c.key = ?1 ORDER BY c.language")
        .context("failed to prepare the container lookup")?;
    let rows = stmt.query_map(params![key], map_node_row).context("failed to look up containers by key")?;
    rows.collect::<rusqlite::Result<_>>().context("failed to read containers by key")
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
           AND nativeKind IS NOT ?6 \
           AND (startLine < ?2 OR (startLine = ?2 AND startCol <= ?3)) \
           AND (endLine > ?2 OR (endLine = ?2 AND endCol >= ?3)) \
         ORDER BY (endLine - startLine) ASC, (endCol - startCol) ASC \
         LIMIT 1",
        params![
            file_path,
            line,
            col,
            PENDING_SYMBOL_NATIVE_KIND,
            REEXPORT_NATIVE_KIND,
            CONTAINER_NATIVE_KIND
        ],
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

    /// A container node (`graph::containers`) is a real row with no source:
    /// no name, qualifiedName or position lookup may answer with it, even
    /// when its key is exactly the string asked for. Materialized the way the
    /// daemon does it, through `apply_diff`, rather than inserted by hand.
    #[test]
    fn container_nodes_are_not_answers_to_name_qualified_name_or_position_lookups() {
        let mut conn = setup();
        let mut member = NodeRecord::new("fn1", "Function", "run", "app::run", "app/run.rs", "rust");
        member.container = Some("app".to_string());
        upsert_node(&mut conn, member).unwrap();
        let container = crate::graph::containers::container_id("rust", "app");
        assert!(
            get_node(&conn, &container).unwrap().is_some(),
            "the container must exist for this to test anything"
        );

        assert!(find_by_name(&conn, "app", None).unwrap().is_empty());
        assert!(find_by_name(&conn, "app", Some("")).unwrap().is_empty());
        assert!(find_by_qualified_name(&conn, "app", None).unwrap().is_empty());
        assert!(find_by_qualified_name(&conn, "app", Some("")).unwrap().is_empty());
        assert!(find_by_position(&conn, "", 0, 0).unwrap().is_none());
        assert_eq!(find_by_name(&conn, "run", None).unwrap().len(), 1, "its member is still found");
    }

    #[test]
    fn find_by_position_returns_none_outside_every_node() {
        let mut conn = setup();
        upsert_node(&mut conn, node_with_span("file1", "File", "a/lib.rs", (0, 0), (20, 0))).unwrap();
        upsert_node(&mut conn, node_with_span("fn1", "Function", "a/lib.rs", (5, 0), (10, 1))).unwrap();

        assert!(find_by_position(&conn, "a/lib.rs", 50, 0).unwrap().is_none());
    }

    fn file(path: &str) -> NodeRecord {
        NodeRecord::new(path, "File", path, path, path, "rust")
    }

    fn entry_points(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    /// `NodeRecord` carries no `Debug` impl, so a failing assertion below
    /// prints candidates by path instead - all a debugging message here
    /// needs.
    fn paths(nodes: &[NodeRecord]) -> Vec<&str> {
        nodes.iter().map(|n| n.file_path.as_str()).collect()
    }

    /// TS behaviour, preserved byte for byte: a bare, dot-free entry
    /// (`"index"`, exactly what the bundled TS plugin's manifest declares)
    /// still matches the file's stem under any extension, and still outranks
    /// a shorter path - the same property the old hardcoded `%/index.%`
    /// check gave `find_files_under`.
    #[test]
    fn a_bare_entry_point_matches_the_stem_under_any_extension_and_ranks_first() {
        let mut conn = setup();
        upsert_node(&mut conn, file("pkg/a.ts")).unwrap();
        upsert_node(&mut conn, file("pkg/index.tsx")).unwrap();

        let found = find_files_under(&conn, "pkg", &entry_points(&["index"]), 5).unwrap();

        assert_eq!(
            found.first().map(|n| n.file_path.as_str()),
            Some("pkg/index.tsx"),
            "a stem match must lead even though pkg/a.ts is the shorter path: {:?}",
            paths(&found)
        );
    }

    /// GM-273's acceptance case: a fake manifest declaring `entry_points =
    /// ["mod.rs"]` (Rust's own convention, not yet backed by a real plugin -
    /// see `daemon::manifest::WorkspaceConfig::entry_points`'s doc comment)
    /// must rank `mod.rs` first in a directory lookup, exactly the way
    /// `"index"` already does for TypeScript. `pkg/a.rs` is deliberately the
    /// *shorter* path, so this only passes if the entry-point rank - not
    /// `LENGTH(filePath)` - decided the order.
    #[test]
    fn a_declared_rust_entry_point_ranks_first_over_a_shorter_path() {
        let mut conn = setup();
        upsert_node(&mut conn, file("pkg/a.rs")).unwrap();
        upsert_node(&mut conn, file("pkg/mod.rs")).unwrap();

        let found = find_files_under(&conn, "pkg", &entry_points(&["mod.rs"]), 5).unwrap();

        assert_eq!(
            found.first().map(|n| n.file_path.as_str()),
            Some("pkg/mod.rs"),
            "pkg/mod.rs is 2 bytes longer than pkg/a.rs, so only the declared \
             entry point can be why it leads: {:?}",
            paths(&found)
        );
    }

    /// The exact-name form's whole reason to exist: unlike the stem form, it
    /// must not smear onto a file that merely starts with the same bytes.
    /// `pkg/mod.rs.bak` sorts after `pkg/mod.rs` here regardless (it is
    /// longer), so this asserts the stronger property directly - the rank
    /// expression itself, not just the final order.
    #[test]
    fn an_exact_entry_point_does_not_match_a_file_that_only_shares_its_prefix() {
        let mut conn = setup();
        upsert_node(&mut conn, file("pkg/mod.rs.bak")).unwrap();

        let (rank_expr, rank_params) = entry_point_rank_expr(&entry_points(&["mod.rs"]));
        // `rank_expr`'s own `?` placeholders come first in the SQL text, so
        // its params are bound first too - unnumbered `?` throughout, so the
        // two lists stay in the same left-to-right order the query text has.
        let mut bound: Vec<rusqlite::types::Value> = rank_params.into_iter().map(Into::into).collect();
        bound.push("pkg/mod.rs.bak".to_string().into());
        let matches: bool = conn
            .query_row(
                &format!("SELECT {rank_expr} FROM nodes WHERE filePath = ?"),
                rusqlite::params_from_iter(bound),
                |row| row.get(0),
            )
            .unwrap();
        assert!(!matches, "pkg/mod.rs.bak must not count as the mod.rs entry point");
    }

    /// Code-review fix: `_` and `%` are `LIKE` wildcards, and `entry_points`
    /// is manifest content this module does not control - a future Python
    /// manifest declaring `__init__.py` (two literal underscores) must not
    /// have those underscores reinterpreted as "any one character". Proves
    /// both halves: the real entry point still ranks first, and a file that
    /// only matches because `_` was treated as a wildcard -
    /// `pkg/abinitcd.py`'s file name is `abinitcd.py`, 11 characters, one per
    /// literal/wildcard position in `__init__.py` (`_` `_` `i` `n` `i` `t` `_`
    /// `_` `.` `p` `y`) - does not count as the entry point at all.
    #[test]
    fn an_entry_point_containing_like_wildcard_characters_is_matched_literally() {
        let mut conn = setup();
        upsert_node(&mut conn, file("pkg/other.py")).unwrap();
        upsert_node(&mut conn, file("pkg/__init__.py")).unwrap();

        let found = find_files_under(&conn, "pkg", &entry_points(&["__init__.py"]), 5).unwrap();
        assert_eq!(
            found.first().map(|n| n.file_path.as_str()),
            Some("pkg/__init__.py"),
            "the real entry point must still rank first: {:?}",
            paths(&found)
        );

        // A separate file, present only so the second check below has a real
        // row to query - unescaped, this is exactly the path the old bug
        // would have misidentified as the __init__.py entry point.
        upsert_node(&mut conn, file("pkg/abinitcd.py")).unwrap();

        let (rank_expr, rank_params) = entry_point_rank_expr(&entry_points(&["__init__.py"]));
        let mut bound: Vec<rusqlite::types::Value> = rank_params.into_iter().map(Into::into).collect();
        bound.push("pkg/abinitcd.py".to_string().into());
        let matches: bool = conn
            .query_row(
                &format!("SELECT {rank_expr} FROM nodes WHERE filePath = ?"),
                rusqlite::params_from_iter(bound),
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            !matches,
            "pkg/abinitcd.py must not count as the __init__.py entry point - each `_` is a literal \
             underscore, not a single-character wildcard"
        );
    }

    /// No manifest available at all (this task's documented fallback) must
    /// not error, and must degrade to the pre-task ordering: shortest path
    /// wins, with no entry point ranked ahead of it.
    #[test]
    fn an_empty_entry_point_list_falls_back_to_shortest_path_only() {
        let mut conn = setup();
        upsert_node(&mut conn, file("pkg/index.ts")).unwrap();
        upsert_node(&mut conn, file("pkg/a.ts")).unwrap();

        let found = find_files_under(&conn, "pkg", &[], 5).unwrap();

        assert_eq!(
            found.first().map(|n| n.file_path.as_str()),
            Some("pkg/a.ts"),
            "with no entry points declared, the shortest path must win: {:?}",
            paths(&found)
        );
    }

    /// `find_files_ending_in_dir` shares `entry_point_rank_expr` with
    /// `find_files_under` - one pass over this same Rust-entry-point case is
    /// enough to prove the wiring, not a full re-run of every case above.
    #[test]
    fn find_files_ending_in_dir_also_ranks_a_declared_entry_point_first() {
        let mut conn = setup();
        upsert_node(&mut conn, file("workspace/pkg/a.rs")).unwrap();
        upsert_node(&mut conn, file("workspace/pkg/mod.rs")).unwrap();

        let found = find_files_ending_in_dir(&conn, "pkg", &entry_points(&["mod.rs"]), 5).unwrap();

        assert_eq!(found.first().map(|n| n.file_path.as_str()), Some("workspace/pkg/mod.rs"));
    }
}
