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
