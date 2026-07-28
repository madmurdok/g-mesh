use anyhow::{Context, Result};
use base64::prelude::*;
use rusqlite::{params, Connection, Row};
use serde::{de::DeserializeOwned, Deserialize, Serialize};

use crate::storage::write::{EdgeRecord, NodeRecord};

/// Opaque cursor-paginated batch, shared shape for every list-shaped MCP
/// tool response. Cursor instead of offset: background reindexing can
/// shift/duplicate rows mid-pagination if positions are counted by offset.
pub struct Page<T> {
    pub results: Vec<T>,
    pub has_more: bool,
    pub next_cursor: Option<String>,
}

fn encode_cursor<T: Serialize>(value: &T) -> String {
    BASE64_STANDARD.encode(serde_json::to_vec(value).expect("cursor payload is always serializable"))
}

fn decode_cursor<T: DeserializeOwned>(raw: &str) -> Result<T> {
    let bytes = BASE64_STANDARD.decode(raw).context("invalid pagination cursor encoding")?;
    serde_json::from_slice(&bytes).context("invalid pagination cursor payload")
}

#[derive(Serialize, Deserialize)]
struct StructuralCursor {
    resolved: bool,
    locality: i64,
    id: String,
}

/// Which way to follow edges out of an anchor node.
// Doc comments here are user-facing: `JsonSchema` is derived so
// `get_dependencies`' MCP tool schema can name this exact enum rather than
// restating its variants, and schemars publishes the prose verbatim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub enum Direction {
    /// Edges going out of the anchor node (`fromId = anchor`).
    Outgoing,
    /// Edges coming into the anchor node (`toId = anchor`).
    Incoming,
}

/// Paginates the anchor node's incident edges, ordered per the structural
/// ordering rule: `resolved: true` before `resolved: false`, then locality
/// (an edge whose other endpoint shares `anchor_file_path` sorts first),
/// then `id` as a stable tiebreaker. Backs find_references/find_callers/
/// find_callees/find_implementations, which all differ only in `direction`
/// and `edge_kind`.
pub fn paginate_edges(
    conn: &Connection,
    anchor_node_id: &str,
    direction: Direction,
    edge_kind: Option<&str>,
    anchor_file_path: &str,
    page_size: usize,
    cursor: Option<&str>,
) -> Result<Page<EdgeRecord>> {
    let decoded: Option<StructuralCursor> = cursor.map(decode_cursor).transpose()?;

    let (other_endpoint, this_endpoint) = match direction {
        Direction::Outgoing => ("toId", "fromId"),
        Direction::Incoming => ("fromId", "toId"),
    };

    // Fixed placeholder count/order regardless of whether edge_kind/cursor
    // are present - `?3 IS NULL` and `?4 = 0` make the extra predicates
    // vacuously true instead of needing dynamically-numbered SQL text.
    let sql = format!(
        "SELECT e.id AS id, e.fromId AS fromId, e.toId AS toId, e.kind AS kind, e.source AS source, e.resolved AS resolved, \
         CASE WHEN n.filePath = ?1 THEN 0 ELSE 1 END AS locality \
         FROM edges e JOIN nodes n ON n.id = e.{other_endpoint} \
         WHERE e.{this_endpoint} = ?2 \
           AND (?3 IS NULL OR e.kind = ?3) \
           AND ( \
             ?4 = 0 \
             OR e.resolved < ?5 \
             OR (e.resolved = ?5 AND locality > ?6) \
             OR (e.resolved = ?5 AND locality = ?6 AND e.id > ?7) \
           ) \
         ORDER BY e.resolved DESC, locality ASC, e.id ASC \
         LIMIT ?8"
    );

    let (has_cursor, cursor_resolved, cursor_locality, cursor_id): (i64, i64, i64, String) = match &decoded {
        Some(c) => (1, c.resolved as i64, c.locality, c.id.clone()),
        None => (0, 0, 0, String::new()),
    };
    let limit = (page_size + 1) as i64;

    let mut stmt = conn.prepare(&sql)?;
    let mut rows: Vec<(EdgeRecord, i64)> = stmt
        .query_map(
            params![
                anchor_file_path,
                anchor_node_id,
                edge_kind,
                has_cursor,
                cursor_resolved,
                cursor_locality,
                cursor_id,
                limit
            ],
            |row| {
                let locality: i64 = row.get("locality")?;
                Ok((
                    EdgeRecord {
                        id: row.get("id")?,
                        from_id: row.get("fromId")?,
                        to_id: row.get("toId")?,
                        kind: row.get("kind")?,
                        source: row.get("source")?,
                        resolved: row.get("resolved")?,
                    },
                    locality,
                ))
            },
        )?
        .collect::<rusqlite::Result<_>>()
        .context("failed to paginate edges")?;

    let has_more = rows.len() > page_size;
    rows.truncate(page_size);

    let next_cursor = has_more.then(|| {
        let (edge, locality) = rows.last().expect("has_more implies at least one row");
        encode_cursor(&StructuralCursor {
            resolved: edge.resolved,
            locality: *locality,
            id: edge.id.clone(),
        })
    });

    Ok(Page {
        results: rows.into_iter().map(|(edge, _)| edge).collect(),
        has_more,
        next_cursor,
    })
}

#[derive(Serialize, Deserialize)]
struct SourceOrderCursor {
    start_line: i64,
    start_col: i64,
    id: String,
}

/// Paginates the symbols a `File` node's `DEFINES` edges reach, ordered by
/// source position (`startLine` then `startCol` ascending, `id` as a stable
/// tiebreaker for same-position nodes) rather than `paginate_edges`'
/// resolved/locality rule - `get_file_outline`'s whole point is to read back
/// "as the file reads", not ranked by confidence. Joins straight to `nodes`
/// instead of returning `EdgeRecord`s to resolve one at a time, since every
/// caller wants the full node here and there's no ambiguity about which end
/// of the edge that is.
pub fn paginate_defines(
    conn: &Connection,
    file_node_id: &str,
    page_size: usize,
    cursor: Option<&str>,
) -> Result<Page<NodeRecord>> {
    let decoded: Option<SourceOrderCursor> = cursor.map(decode_cursor).transpose()?;

    let sql = "SELECT n.* FROM edges e JOIN nodes n ON n.id = e.toId \
               WHERE e.fromId = ?1 AND e.kind = 'DEFINES' \
                 AND ( \
                   ?2 = 0 \
                   OR n.startLine > ?3 \
                   OR (n.startLine = ?3 AND n.startCol > ?4) \
                   OR (n.startLine = ?3 AND n.startCol = ?4 AND n.id > ?5) \
                 ) \
               ORDER BY n.startLine ASC, n.startCol ASC, n.id ASC \
               LIMIT ?6";

    let (has_cursor, cursor_line, cursor_col, cursor_id): (i64, i64, i64, String) = match &decoded {
        Some(c) => (1, c.start_line, c.start_col, c.id.clone()),
        None => (0, 0, 0, String::new()),
    };
    let limit = (page_size + 1) as i64;

    let mut stmt = conn.prepare(sql)?;
    let mut rows: Vec<NodeRecord> = stmt
        .query_map(params![file_node_id, has_cursor, cursor_line, cursor_col, cursor_id, limit], crate::graph::queries::map_node_row)?
        .collect::<rusqlite::Result<_>>()
        .context("failed to paginate DEFINES edges")?;

    let has_more = rows.len() > page_size;
    rows.truncate(page_size);

    let next_cursor = has_more.then(|| {
        let last = rows.last().expect("has_more implies at least one row");
        encode_cursor(&SourceOrderCursor { start_line: last.start_line, start_col: last.start_col, id: last.id.clone() })
    });

    Ok(Page { results: rows, has_more, next_cursor })
}

#[derive(Serialize, Deserialize)]
struct ScoreCursor {
    score: f64,
    id: String,
}

/// Generic keyset pagination for `search_code`-shaped results, ordered by
/// similarity score (descending) then `id` as a tiebreaker. `base_sql` must
/// project `score` (REAL) and `id` (unique) columns; `map_row` reads
/// whatever columns the caller needs, plus `score`/`id` for cursor state.
/// Not yet called by any tool (search_code lands with the Embeddings epic)
/// but exercised directly in tests so the ordering rule is proven now.
pub fn paginate_by_score<T>(
    conn: &Connection,
    base_sql: &str,
    params: &[&dyn rusqlite::ToSql],
    page_size: usize,
    cursor: Option<&str>,
    map_row: impl Fn(&Row) -> rusqlite::Result<(T, f64, String)>,
) -> Result<Page<T>> {
    let decoded: Option<ScoreCursor> = cursor.map(decode_cursor).transpose()?;

    let n = params.len();
    let (has_cursor_idx, score_idx, id_idx, limit_idx) = (n + 1, n + 2, n + 3, n + 4);
    let sql = format!(
        "SELECT * FROM ({base_sql}) AS page \
         WHERE ?{has_cursor_idx} = 0 \
            OR score < ?{score_idx} \
            OR (score = ?{score_idx} AND id > ?{id_idx}) \
         ORDER BY score DESC, id ASC \
         LIMIT ?{limit_idx}"
    );

    let (has_cursor, cursor_score, cursor_id): (i64, f64, String) = match &decoded {
        Some(c) => (1, c.score, c.id.clone()),
        None => (0, 0.0, String::new()),
    };
    let limit = (page_size + 1) as i64;

    let mut all_params: Vec<&dyn rusqlite::ToSql> = params.to_vec();
    all_params.push(&has_cursor);
    all_params.push(&cursor_score);
    all_params.push(&cursor_id);
    all_params.push(&limit);

    let mut stmt = conn.prepare(&sql)?;
    let mut rows: Vec<(T, f64, String)> = stmt
        .query_map(all_params.as_slice(), &map_row)?
        .collect::<rusqlite::Result<_>>()
        .context("failed to paginate by score")?;

    let has_more = rows.len() > page_size;
    rows.truncate(page_size);

    let next_cursor = has_more.then(|| {
        let (_, score, id) = rows.last().expect("has_more implies at least one row");
        encode_cursor(&ScoreCursor { score: *score, id: id.clone() })
    });

    Ok(Page {
        results: rows.into_iter().map(|(item, _, _)| item).collect(),
        has_more,
        next_cursor,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::schema;

    fn setup() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        schema::apply(&conn).unwrap();
        conn
    }

    fn make_node(conn: &Connection, id: &str, file_path: &str) {
        conn.execute(
            "INSERT INTO nodes (id, kind, name, qualifiedName, filePath, startLine, startCol, endLine, endCol, language)
             VALUES (?1, 'Function', ?1, ?1, ?2, 0, 0, 0, 0, 'rust')",
            params![id, file_path],
        )
        .unwrap();
    }

    fn make_edge(conn: &Connection, id: &str, from: &str, to: &str, resolved: bool) {
        conn.execute(
            "INSERT INTO edges (id, fromId, toId, kind, source, resolved) VALUES (?1, ?2, ?3, 'CALLS', 'tree-sitter', ?4)",
            params![id, from, to, resolved],
        )
        .unwrap();
    }

    #[test]
    fn resolved_true_sorts_before_resolved_false_at_equal_locality() {
        let conn = setup();
        make_node(&conn, "root", "a.rs");
        make_node(&conn, "n1", "a.rs");
        make_node(&conn, "n2", "a.rs");
        make_edge(&conn, "e_unresolved", "root", "n1", false);
        make_edge(&conn, "e_resolved", "root", "n2", true);

        let page = paginate_edges(&conn, "root", Direction::Outgoing, None, "a.rs", 10, None).unwrap();
        assert_eq!(page.results.len(), 2);
        assert!(page.results[0].resolved, "resolved edge must sort first at equal locality");
        assert!(!page.results[1].resolved);
        assert!(!page.has_more);
    }

    #[test]
    fn locality_breaks_ties_after_resolved() {
        let conn = setup();
        make_node(&conn, "root", "a.rs");
        make_node(&conn, "far", "b.rs");
        make_node(&conn, "near", "a.rs");
        make_edge(&conn, "e_far", "root", "far", true);
        make_edge(&conn, "e_near", "root", "near", true);

        let page = paginate_edges(&conn, "root", Direction::Outgoing, None, "a.rs", 10, None).unwrap();
        assert_eq!(page.results[0].id, "e_near", "same-file target must sort before a distant one");
        assert_eq!(page.results[1].id, "e_far");
    }

    #[test]
    fn incoming_direction_paginates_edges_pointing_at_the_anchor() {
        let conn = setup();
        make_node(&conn, "root", "a.rs");
        make_node(&conn, "caller1", "a.rs");
        make_node(&conn, "caller2", "b.rs");
        make_edge(&conn, "e1", "caller1", "root", true);
        make_edge(&conn, "e2", "caller2", "root", true);

        let page = paginate_edges(&conn, "root", Direction::Incoming, None, "a.rs", 10, None).unwrap();
        let ids: Vec<&str> = page.results.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, vec!["e1", "e2"], "same-file caller must sort before the distant one");
    }

    #[test]
    fn pagination_returns_every_row_once_even_with_inserts_between_calls() {
        let conn = setup();
        make_node(&conn, "root", "a.rs");
        for i in 0..5 {
            let id = format!("n{i}");
            make_node(&conn, &id, "a.rs");
            make_edge(&conn, &format!("e{i}"), "root", &id, i % 2 == 0);
        }

        let mut seen = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let page = paginate_edges(&conn, "root", Direction::Outgoing, None, "a.rs", 2, cursor.as_deref()).unwrap();
            seen.extend(page.results.iter().map(|e| e.id.clone()));

            // Simulate a background reindex inserting a new low-priority edge
            // (unresolved, distant file) after the first page - it must not
            // disturb the pages already served or duplicate/skip the
            // original five.
            if seen.len() == 2 {
                make_node(&conn, "intruder", "z.rs");
                make_edge(&conn, "e_intruder", "root", "intruder", false);
            }

            if !page.has_more {
                break;
            }
            cursor = page.next_cursor;
        }

        let original: Vec<String> = (0..5).map(|i| format!("e{i}")).collect();
        for id in &original {
            assert_eq!(seen.iter().filter(|s| *s == id).count(), 1, "row {id} must appear exactly once");
        }
        assert!(seen.contains(&"e_intruder".to_string()), "the new lowest-priority row lands on the final page");
        assert_eq!(seen.len(), 6);
    }

    fn make_node_at(conn: &Connection, id: &str, file_path: &str, start_line: i64, start_col: i64) {
        conn.execute(
            "INSERT INTO nodes (id, kind, name, qualifiedName, filePath, startLine, startCol, endLine, endCol, language)
             VALUES (?1, 'Function', ?1, ?1, ?2, ?3, ?4, ?3, ?4, 'rust')",
            params![id, file_path, start_line, start_col],
        )
        .unwrap();
    }

    fn make_defines_edge(conn: &Connection, id: &str, file_id: &str, symbol_id: &str) {
        conn.execute(
            "INSERT INTO edges (id, fromId, toId, kind, source, resolved) VALUES (?1, ?2, ?3, 'DEFINES', 'tree-sitter', false)",
            params![id, file_id, symbol_id],
        )
        .unwrap();
    }

    #[test]
    fn paginate_defines_orders_by_source_position_not_insertion_order() {
        let conn = setup();
        make_node(&conn, "file", "a.rs");
        make_node_at(&conn, "third", "a.rs", 30, 0);
        make_node_at(&conn, "first", "a.rs", 5, 0);
        make_node_at(&conn, "second", "a.rs", 5, 4);
        make_defines_edge(&conn, "e_third", "file", "third");
        make_defines_edge(&conn, "e_first", "file", "first");
        make_defines_edge(&conn, "e_second", "file", "second");

        let page = paginate_defines(&conn, "file", 10, None).unwrap();
        let ids: Vec<&str> = page.results.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(ids, vec!["first", "second", "third"], "must come back in source order, not insertion order");
        assert!(!page.has_more);
    }

    #[test]
    fn paginate_defines_paginates_across_cursor_continuation() {
        let conn = setup();
        make_node(&conn, "file", "a.rs");
        for i in 0..5 {
            let id = format!("n{i}");
            make_node_at(&conn, &id, "a.rs", i, 0);
            make_defines_edge(&conn, &format!("e{i}"), "file", &id);
        }

        let mut seen = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let page = paginate_defines(&conn, "file", 2, cursor.as_deref()).unwrap();
            seen.extend(page.results.into_iter().map(|n| n.id));
            if !page.has_more {
                break;
            }
            cursor = page.next_cursor;
        }

        assert_eq!(seen, vec!["n0", "n1", "n2", "n3", "n4"], "must return every symbol exactly once, in source order");
    }

    #[test]
    fn score_pagination_orders_by_score_descending() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE scored (id TEXT PRIMARY KEY, score REAL, label TEXT)").unwrap();
        conn.execute("INSERT INTO scored VALUES ('a', 0.5, 'low'), ('b', 0.9, 'high'), ('c', 0.7, 'mid')", [])
            .unwrap();

        let page = paginate_by_score::<String>(
            &conn,
            "SELECT id, score, label FROM scored",
            &[],
            10,
            None,
            |row| Ok((row.get::<_, String>("label")?, row.get("score")?, row.get("id")?)),
        )
        .unwrap();

        assert_eq!(page.results, vec!["high", "mid", "low"]);
        assert!(!page.has_more);
    }
}
