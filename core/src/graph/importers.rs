//! Reads of the `IMPORTS` edges: who imports what, through placeholders,
//! linked files and containers.

use anyhow::{Context, Result};
use rusqlite::{params, Connection};

use crate::protocol::types::TargetScopeKind;

/// The importers in `language` of every `IMPORTS` edge whose target of
/// `scope_kind` is exactly `key`, by the same rule as `ingest::select` reads a [`StoredImport`]'s target:
/// an unlinked edge through its placeholder's scope, a linked one through the
/// `File` path or container key of the node it points at; for a `File` key,
/// also an unlinked edge whose placeholder lists `key` among its extra
/// waiting scopes. Each part starts at an index on the key
/// (`idx_targets_scope`, `idx_also_scopes_scope`, `idx_nodes_filePath`, the
/// containers' `(language, key)`), then follows `idx_edges_toId`.
///
/// A linked container is looked up in the importer's language: the linker
/// resolves a container placeholder in the placeholder's language, which is
/// its importer's, since imports never cross a language boundary.
pub(crate) fn importers_by_key(
    conn: &Connection,
    language: &str,
    scope_kind: TargetScopeKind,
    key: &str,
) -> Result<Vec<String>> {
    let sql = match scope_kind {
        TargetScopeKind::File => {
            "SELECT src.filePath FROM placeholder_targets pt
             JOIN nodes t ON t.id = pt.nodeId
             JOIN edges e ON e.toId = t.id
             JOIN nodes src ON src.id = e.fromId
             WHERE pt.scopeKind = 'file' AND pt.scope = ?2 AND e.kind = 'IMPORTS' AND src.language = ?1
             UNION
             SELECT src.filePath FROM placeholder_also_scopes a
             JOIN edges e ON e.toId = a.nodeId
             JOIN nodes src ON src.id = e.fromId
             WHERE a.scope = ?2 AND e.kind = 'IMPORTS' AND src.language = ?1
             UNION
             SELECT src.filePath FROM nodes t
             JOIN edges e ON e.toId = t.id
             JOIN nodes src ON src.id = e.fromId
             WHERE t.filePath = ?2 AND t.kind = 'File'
               AND NOT EXISTS (SELECT 1 FROM placeholder_targets pt WHERE pt.nodeId = t.id)
               AND e.kind = 'IMPORTS' AND src.language = ?1"
        }
        TargetScopeKind::Container => {
            "SELECT src.filePath FROM placeholder_targets pt
             JOIN nodes t ON t.id = pt.nodeId
             JOIN edges e ON e.toId = t.id
             JOIN nodes src ON src.id = e.fromId
             WHERE pt.scopeKind = 'container' AND pt.scope = ?2 AND e.kind = 'IMPORTS' AND src.language = ?1
             UNION
             SELECT src.filePath FROM containers c
             JOIN nodes t ON t.id = c.nodeId
             JOIN edges e ON e.toId = t.id
             JOIN nodes src ON src.id = e.fromId
             WHERE c.language = ?1 AND c.key = ?2
               AND NOT EXISTS (SELECT 1 FROM placeholder_targets pt WHERE pt.nodeId = t.id)
               AND e.kind = 'IMPORTS' AND src.language = ?1"
        }
    };
    conn.prepare_cached(sql)
        .and_then(|mut statement| statement.query_map(params![language, key], |row| row.get(0))?.collect())
        .with_context(|| format!("failed to look up {language}'s importers of {key}"))
}

/// Calls `visit` with every `IMPORTS` edge of `language`, each read once:
/// its importer, specifier, landing node and, while unlinked, its
/// placeholder's scope.
pub(crate) fn scan_import_rows(
    conn: &Connection,
    language: &str,
    mut visit: impl FnMut(StoredImport),
) -> Result<()> {
    let mut statement = conn
        .prepare(
            "SELECT src.filePath, e.specifier, t.kind, t.filePath, c.key, pt.scopeKind, pt.scope
             FROM edges e
             JOIN nodes src ON src.id = e.fromId
             JOIN nodes t ON t.id = e.toId
             LEFT JOIN containers c ON c.nodeId = t.id
             LEFT JOIN placeholder_targets pt ON pt.nodeId = t.id
             WHERE e.kind = 'IMPORTS' AND src.language = ?1",
        )
        .context("failed to prepare the import scan")?;
    let rows = statement
        .query_map(params![language], |row| {
            Ok(StoredImport {
                importer: row.get(0)?,
                specifier: row.get(1)?,
                target_kind: row.get(2)?,
                target_file: row.get(3)?,
                container_key: row.get(4)?,
                placeholder_scope_kind: row.get(5)?,
                placeholder_scope: row.get(6)?,
            })
        })
        .with_context(|| format!("failed to scan {language}'s imports"))?;
    for row in rows {
        visit(row.with_context(|| format!("failed to read one of {language}'s imports"))?);
    }
    Ok(())
}

/// One `IMPORTS` edge as [`scan_import_rows`] reads it.
pub(crate) struct StoredImport {
    pub(crate) importer: String,
    pub(crate) specifier: Option<String>,
    pub(crate) target_kind: String,
    pub(crate) target_file: String,
    pub(crate) container_key: Option<String>,
    pub(crate) placeholder_scope_kind: Option<String>,
    pub(crate) placeholder_scope: Option<String>,
}

/// One `IMPORTS` edge out of a `File` node, as `g-mesh deps` reads it.
pub(crate) struct FileImport {
    pub(crate) from: String,
    pub(crate) language: String,
    pub(crate) target_id: String,
    pub(crate) target_kind: String,
    pub(crate) target_path: String,
    pub(crate) native_kind: Option<String>,
    /// The edge's specifier, or the target's `qualifiedName` when it has none.
    pub(crate) specifier: String,
}

/// Calls `visit` with every `IMPORTS` edge whose `fromId` is a `File` node,
/// in one pass; stops at the first error `visit` returns.
pub(crate) fn for_each_file_import(
    conn: &Connection,
    mut visit: impl FnMut(FileImport) -> Result<()>,
) -> Result<()> {
    let mut stmt = conn
        .prepare(
            "SELECT f.filePath, f.language, t.id, t.kind, t.filePath, t.nativeKind, \
                    COALESCE(e.specifier, t.qualifiedName) \
             FROM edges e \
             JOIN nodes f ON f.id = e.fromId AND f.kind = ?1 \
             JOIN nodes t ON t.id = e.toId \
             WHERE e.kind = ?2",
        )
        .context("failed to prepare the import edge query")?;
    let rows = stmt
        .query_map(params![FILE_KIND, IMPORTS_KIND], |row| {
            Ok(FileImport {
                from: row.get(0)?,
                language: row.get(1)?,
                target_id: row.get(2)?,
                target_kind: row.get(3)?,
                target_path: row.get(4)?,
                native_kind: row.get(5)?,
                specifier: row.get(6)?,
            })
        })
        .context("failed to query the import edges")?;
    for row in rows {
        visit(row.context("failed to read an import edge")?)?;
    }
    Ok(())
}

const FILE_KIND: &str = "File";
const IMPORTS_KIND: &str = "IMPORTS";
