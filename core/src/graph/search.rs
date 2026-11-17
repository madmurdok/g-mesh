//! Vector-similarity reads: embedded nodes ranked against a query vector.

use std::collections::HashMap;

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension};

use crate::graph::pagination;

/// One embedded node scored against a query vector.
pub(crate) struct VectorHit {
    pub(crate) id: String,
    pub(crate) qualified_name: String,
    pub(crate) kind: String,
    pub(crate) file_path: String,
    pub(crate) start_line: i64,
    pub(crate) start_col: i64,
    pub(crate) language: String,
    /// Cosine similarity to the query: `1 - cosine_distance`.
    pub(crate) score: f64,
}

/// Ranks every embedded node against `query` (packed as `vectors.embedding`
/// stores it), best first, one keyset page of `page_size` at a time. A node
/// with no `vectors` row never appears.
pub(crate) fn by_vector(
    conn: &Connection,
    query: &[u8],
    page_size: usize,
    cursor: Option<&str>,
) -> Result<pagination::Page<VectorHit>> {
    // `v.nodeId AS id` and the distance-to-similarity flip are the only
    // things this base query owes `paginate_by_score`'s contract (a `score`
    // and an `id` column); every other column rides along for the caller.
    // The inner join keeps unembedded nodes out without a separate filter.
    let base_sql = "SELECT v.nodeId AS id, n.qualifiedName, n.kind, n.filePath, n.startLine, n.startCol, \
                     n.language, \
                     (1.0 - vec_distance_cosine(v.embedding, ?1)) AS score \
                     FROM vectors v JOIN nodes n ON n.id = v.nodeId";
    let params: Vec<&dyn rusqlite::ToSql> = vec![&query];

    pagination::paginate_by_score(conn, base_sql, &params, page_size, cursor, |row| {
        let score: f64 = row.get("score")?;
        let id: String = row.get("id")?;
        Ok((
            VectorHit {
                id: id.clone(),
                qualified_name: row.get("qualifiedName")?,
                kind: row.get("kind")?,
                file_path: row.get("filePath")?,
                start_line: row.get("startLine")?,
                start_col: row.get("startCol")?,
                score,
                language: row.get("language")?,
            },
            score,
            id,
        ))
    })
}

/// The hits for `ids` that still have a vector, in `ids`' order, scored as
/// [`by_vector`] scores them.
pub(crate) fn hits_by_id(conn: &Connection, query: &[u8], ids: &[String]) -> Result<Vec<VectorHit>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let sql =
        "SELECT v.nodeId AS id, n.qualifiedName, n.kind, n.filePath, n.startLine, n.startCol, n.language, \
               (1.0 - vec_distance_cosine(v.embedding, ?1)) AS score \
               FROM vectors v JOIN nodes n ON n.id = v.nodeId \
               WHERE v.nodeId IN (SELECT value FROM json_each(?2))";
    let ids_json = serde_json::to_string(ids)?;
    let mut statement = conn.prepare(sql)?;
    let mut found: HashMap<String, VectorHit> = statement
        .query_map(rusqlite::params![query, ids_json], |row| {
            let id: String = row.get("id")?;
            Ok((
                id.clone(),
                VectorHit {
                    id,
                    qualified_name: row.get("qualifiedName")?,
                    kind: row.get("kind")?,
                    file_path: row.get("filePath")?,
                    start_line: row.get("startLine")?,
                    start_col: row.get("startCol")?,
                    score: row.get("score")?,
                    language: row.get("language")?,
                },
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(ids.iter().filter_map(|id| found.remove(id)).collect())
}

/// The `nodes` columns a node's cross-encoder text reads.
pub(crate) struct NodeTextColumns {
    pub(crate) doc_comment: Option<String>,
    pub(crate) signature: Option<String>,
    pub(crate) kind: Option<String>,
    pub(crate) native_kind: Option<String>,
    pub(crate) name: Option<String>,
    pub(crate) qualified_name: Option<String>,
    pub(crate) file_path: Option<String>,
    pub(crate) start_line: i64,
    pub(crate) end_line: i64,
    pub(crate) language: Option<String>,
}

impl NodeTextColumns {
    fn read(r: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            doc_comment: r.get(0)?,
            signature: r.get(1)?,
            kind: r.get(2)?,
            native_kind: r.get(3)?,
            name: r.get(4)?,
            qualified_name: r.get(5)?,
            file_path: r.get(6)?,
            start_line: r.get(7)?,
            end_line: r.get(8)?,
            language: r.get(9)?,
        })
    }
}

/// Each of `ids`' [`NodeTextColumns`], in order; `None` for a node that is
/// gone.
pub(crate) fn node_text_columns<'a>(
    conn: &Connection,
    ids: impl Iterator<Item = &'a str>,
) -> Result<Vec<Option<NodeTextColumns>>> {
    let mut statement = conn.prepare_cached(
        "SELECT docComment, signature, kind, nativeKind, name, qualifiedName, filePath, startLine, endLine, \
         language FROM nodes WHERE id = ?1",
    )?;
    ids.map(|id| Ok(statement.query_row([id], NodeTextColumns::read).optional()?)).collect()
}
