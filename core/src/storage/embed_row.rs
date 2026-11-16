//! The `nodes` columns that feed a node's embedded text, and the owned row
//! read from them. The rule that turns a row into text is
//! `embedding::text`'s; which columns it reads is SQL knowledge and lives
//! here, so every SELECT that feeds it names the same list.

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, Row};

use crate::storage::write::NodeRecord;

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
