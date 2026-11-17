//! The `nodes` columns that feed a node's embedded text, and the owned row
//! read from them. The rule that turns a row into text is
//! `embedding::text`'s; which columns it reads is SQL knowledge and lives
//! here, so every SELECT that feeds it names the same list.

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, Row};

use crate::model::NodeRecord;

/// The rule that turns a row into embedded text, as storage needs it while
/// planning a per-language swap: the text itself, and the version tag a live
/// vector must carry to be kept. Implemented by `embedding::text::TextRule`;
/// storage only calls it, inside `plan`'s staging transaction.
pub trait NodeText {
    /// The embedding version vectors made by this rule are tagged with.
    fn embedding_version(&self) -> &str;
    /// The text a node with `row`'s columns embeds, or `None` for one that
    /// embeds nothing (noise, generated).
    fn text(&self, row: &EmbedRow) -> Option<String>;
}

/// The columns of `nodes` an [`EmbedRow`] reads, in [`EmbedRow::from_row`]'s
/// order.
pub const EMBED_COLUMNS: [&str; 7] =
    ["kind", "name", "qualifiedName", "filePath", "docComment", "signature", "generated"];

/// [`EMBED_COLUMNS`] as a SELECT list, each prefixed with `alias`
/// (`"n."` gives `"n.kind, n.name, ..."`; `""` leaves them bare).
pub fn embed_columns(alias: &str) -> String {
    EMBED_COLUMNS.iter().map(|column| format!("{alias}{column}")).collect::<Vec<_>>().join(", ")
}

/// One node's embedded-text inputs, read from [`EMBED_COLUMNS`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbedRow {
    pub kind: String,
    pub name: String,
    pub qualified_name: String,
    /// Project-relative, `/`-separated.
    pub file_path: String,
    pub doc: Option<String>,
    pub signature: Option<String>,
    pub generated: bool,
}

impl EmbedRow {
    /// Reads the seven [`EMBED_COLUMNS`] starting at column `at`.
    pub fn from_row(row: &Row, at: usize) -> rusqlite::Result<Self> {
        Ok(Self {
            kind: row.get(at)?,
            name: row.get(at + 1)?,
            qualified_name: row.get(at + 2)?,
            file_path: row.get(at + 3)?,
            doc: row.get(at + 4)?,
            signature: row.get(at + 5)?,
            generated: row.get(at + 6)?,
        })
    }

    /// Like [`Self::from_row`] for the nullable side of a LEFT JOIN: `None`
    /// when `kind` is NULL (no joined row).
    pub fn from_nullable_row(row: &Row, at: usize) -> rusqlite::Result<Option<Self>> {
        let kind: Option<String> = row.get(at)?;
        if kind.is_none() {
            return Ok(None);
        }
        Self::from_row(row, at).map(Some)
    }

    /// A [`NodeRecord`] carrying only what `EmbeddingPipeline::compute`
    /// reads; every other field is a filler (language `""`).
    pub fn into_node_record(self, id: impl Into<String>) -> NodeRecord {
        let mut node =
            NodeRecord::new(id.into(), self.kind, self.name, self.qualified_name, self.file_path, "");
        node.doc_comment = self.doc;
        node.signature = self.signature;
        node.generated = self.generated;
        node
    }
}

/// The embed row of `node_id`, or `None` when the node no longer exists.
pub fn read(conn: &Connection, node_id: &str) -> Result<Option<EmbedRow>> {
    conn.query_row(&format!("SELECT {} FROM nodes WHERE id = ?1", embed_columns("")), [node_id], |row| {
        EmbedRow::from_row(row, 0)
    })
    .optional()
    .context("failed to read node for its embedding row")
}

/// One node owed a vector: its id, the `visibility` its embedding tier reads,
/// and its embedded-text inputs.
pub struct EmbedCandidate {
    pub id: String,
    pub visibility: String,
    pub row: EmbedRow,
}

/// How many nodes are owed a vector under `version`: every node with
/// embeddable text (a doc comment or a signature) that is not generated and
/// has no `vectors` row yet, or one made under another `embeddingVersion`.
/// The same `WHERE` clause as [`fetch_candidate_page`]: this is the unpaged
/// count of the set that query pages through.
pub fn count_candidates(conn: &Connection, version: &str) -> rusqlite::Result<i64> {
    conn.query_row(
        "SELECT COUNT(*) FROM nodes n LEFT JOIN vectors v ON v.nodeId = n.id \
         WHERE (v.nodeId IS NULL OR v.embeddingVersion != ?1) \
         AND (n.docComment IS NOT NULL OR n.signature IS NOT NULL) AND n.generated = 0",
        [version],
        |row| row.get(0),
    )
}

/// One keyset page of [`count_candidates`]' set strictly after `after_id`
/// (or from the start, if `None`), ordered by `id`, so paging never revisits
/// or skips a row however many pages have run.
pub fn fetch_candidate_page(
    conn: &Connection,
    version: &str,
    after_id: Option<&str>,
    limit: i64,
) -> rusqlite::Result<Vec<EmbedCandidate>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT n.id, n.visibility, {} \
         FROM nodes n LEFT JOIN vectors v ON v.nodeId = n.id \
         WHERE (v.nodeId IS NULL OR v.embeddingVersion != ?3) \
         AND (n.docComment IS NOT NULL OR n.signature IS NOT NULL) AND n.generated = 0 \
         AND n.id > ?1 ORDER BY n.id LIMIT ?2",
        embed_columns("n."),
    ))?;
    // Every real node id is a non-empty string, so `id > ''` is true for all
    // of them: one statement serves both "from the very start" (`after_id`
    // is `None`) and "strictly after the last page's id".
    let rows = stmt.query_map(rusqlite::params![after_id.unwrap_or(""), limit, version], |row| {
        Ok(EmbedCandidate { id: row.get(0)?, visibility: row.get(1)?, row: EmbedRow::from_row(row, 2)? })
    })?;
    rows.collect()
}

/// One keyset page of the nodes with a vector strictly after `after_id`,
/// ordered by id, each with its embedded-text inputs.
pub fn embedded_rows_after(conn: &Connection, after_id: &str, limit: i64) -> Result<Vec<(String, EmbedRow)>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT n.id, {} FROM vectors v JOIN nodes n ON n.id = v.nodeId \
             WHERE v.nodeId > ?1 ORDER BY v.nodeId LIMIT ?2",
        embed_columns("n."),
    ))?;
    let rows = stmt.query_map(rusqlite::params![after_id, limit], |row| {
        Ok((row.get(0)?, EmbedRow::from_row(row, 1)?))
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Diff;
    use crate::storage::connection::open_staging;
    use crate::storage::write::apply_diff;

    /// The column list is `from_row`'s order, with the alias in front of
    /// each. Control: swap `docComment` and `signature` in `EMBED_COLUMNS`
    /// (a read row gets the signature as its doc).
    #[test]
    fn embed_columns_lists_the_seven_columns_in_from_rows_order() {
        assert_eq!(
            embed_columns("n."),
            "n.kind, n.name, n.qualifiedName, n.filePath, n.docComment, n.signature, n.generated"
        );
        assert_eq!(embed_columns(""), EMBED_COLUMNS.join(", "));
    }

    fn database() -> (tempfile::TempDir, Connection) {
        let dir = tempfile::tempdir().unwrap();
        let mut conn = open_staging(&dir.path().join("index.db")).unwrap();
        let mut node = NodeRecord::new("n1", "Function", "open", "io::open", "src/io.rs", "rust");
        node.doc_comment = Some("Opens it.".to_string());
        node.signature = Some("fn open()".to_string());
        node.generated = true;
        apply_diff(&mut conn, &Diff { upsert_nodes: vec![node], ..Default::default() }).unwrap();
        (dir, conn)
    }

    /// `read` maps each column to its field, and a missing node to `None`.
    #[test]
    fn read_returns_the_nodes_row_and_none_for_a_missing_node() {
        let (_dir, conn) = database();

        assert_eq!(
            read(&conn, "n1").unwrap(),
            Some(EmbedRow {
                kind: "Function".to_string(),
                name: "open".to_string(),
                qualified_name: "io::open".to_string(),
                file_path: "src/io.rs".to_string(),
                doc: Some("Opens it.".to_string()),
                signature: Some("fn open()".to_string()),
                generated: true,
            })
        );
        assert_eq!(read(&conn, "gone").unwrap(), None);
    }

    /// The nullable side of a LEFT JOIN with no joined row is `None`, and a
    /// joined row reads at its offset.
    #[test]
    fn from_nullable_row_is_none_when_kind_is_null() {
        let (_dir, conn) = database();
        let sql = format!(
            "SELECT o.id, {} FROM (SELECT 'n1' AS id UNION ALL SELECT 'gone') o
             LEFT JOIN nodes n ON n.id = o.id ORDER BY o.id",
            embed_columns("n.")
        );
        let mut statement = conn.prepare(&sql).unwrap();
        let rows: Vec<(String, Option<EmbedRow>)> = statement
            .query_map([], |row| Ok((row.get(0)?, EmbedRow::from_nullable_row(row, 1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();

        assert_eq!(rows[0].0, "gone");
        assert_eq!(rows[0].1, None);
        assert_eq!(rows[1].0, "n1");
        assert_eq!(rows[1].1, read(&conn, "n1").unwrap());
    }

    /// `into_node_record` keeps every embedded-text input and fills the
    /// rest (language `""`).
    #[test]
    fn into_node_record_carries_the_text_inputs() {
        let row = EmbedRow {
            kind: "Method".to_string(),
            name: "read".to_string(),
            qualified_name: "S::read".to_string(),
            file_path: "src/s.rs".to_string(),
            doc: Some("Reads.".to_string()),
            signature: Some("fn read(&self)".to_string()),
            generated: true,
        };

        let node = row.into_node_record("id-1");

        assert_eq!(
            (
                node.id.as_str(),
                node.kind.as_str(),
                node.name.as_str(),
                node.qualified_name.as_str(),
                node.file_path.as_str(),
                node.language.as_str()
            ),
            ("id-1", "Method", "read", "S::read", "src/s.rs", "")
        );
        assert_eq!(node.doc_comment.as_deref(), Some("Reads."));
        assert_eq!(node.signature.as_deref(), Some("fn read(&self)"));
        assert!(node.generated);
    }
}
